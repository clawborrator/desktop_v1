// SPA OAuth + PKCE flow for clawborrator-supervisor.
//
// First run opens the user's browser to `<hub>/api/v1/auth/spa/start`
// with a `127.0.0.1:<random_port>/callback` redirect_uri. We hold a
// tiny TCP listener on that port; GitHub authenticates the user, the
// hub redirects through to our callback with a one-shot code, and we
// POST it to `/api/v1/auth/spa/exchange` to redeem a `cw_app_…`
// Bearer token that we persist to the desktop config.
//
// PKCE-S256 means the verifier never leaves this process — even if
// the redirect URL is logged somewhere, the code can't be redeemed
// without the verifier. The `state` parameter prevents callback
// requests for OTHER OAuth flows from binding to ours.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;
use tracing::{info, warn};
use url::Url;

const APP_NAME: &str = "clawborrator-supervisor";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Deserialize, Debug)]
struct ExchangeResponse {
    token:      String,
    #[serde(default)]
    #[allow(dead_code)]
    token_name: Option<String>,
}

#[derive(Serialize, Debug)]
struct ExchangeBody<'a> {
    code:          &'a str,
    code_verifier: &'a str,
}

/// PKCE pair. The verifier stays in memory; the challenge goes in
/// the authorize URL.
fn make_pkce() -> (String, String) {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let digest   = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(digest);
    (verifier, challenge)
}

fn make_state() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Read enough of the inbound HTTP request to extract the GET line's
/// path + query, send a small "you can close this tab" response, and
/// drop the connection. Doesn't try to be a full HTTP/1.1 server —
/// just enough to capture one redirect.
async fn handle_callback(stream: &mut tokio::net::TcpStream) -> Result<String> {
    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).await.context("reading callback request")?;
    let head = String::from_utf8_lossy(&buf[..n]);
    // First line: "GET /callback?code=...&state=... HTTP/1.1"
    let first = head.lines().next().ok_or_else(|| anyhow!("empty request"))?;
    let mut parts = first.split_whitespace();
    let _method = parts.next();
    let path    = parts.next().unwrap_or("").to_string();

    let body = "<!doctype html><html><body style='font-family:sans-serif;padding:48px;'>\
                <h2>clawborrator-supervisor</h2>\
                <p>Login successful. You can close this tab.</p>\
                </body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
    Ok(path)
}

fn parse_callback(path: &str, expected_state: &str) -> Result<String> {
    // The path is `/callback?code=...&state=...`. Build a fake URL
    // so url crate parses the query for us.
    let fake = format!("http://127.0.0.1{}", path);
    let u = Url::parse(&fake).with_context(|| format!("invalid callback path: {path}"))?;
    let mut code: Option<String>  = None;
    let mut state: Option<String> = None;
    for (k, v) in u.query_pairs() {
        match k.as_ref() {
            "code"  => code  = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            _ => (),
        }
    }
    let code  = code.ok_or_else(|| anyhow!("callback missing `code`"))?;
    let state = state.ok_or_else(|| anyhow!("callback missing `state`"))?;
    if state != expected_state {
        bail!("state mismatch — possible CSRF; expected {expected_state}, got {state}");
    }
    Ok(code)
}

async fn wait_for_callback(listener: TcpListener, expected_state: &str) -> Result<String> {
    // Loop until we see a request to /callback that parses cleanly.
    // Random scanners (favicon.ico, /, etc.) are dropped silently.
    loop {
        let (mut stream, _) = listener.accept().await.context("accepting callback")?;
        let path = match handle_callback(&mut stream).await {
            Ok(p)  => p,
            Err(e) => { warn!(?e, "callback read failed; waiting for next request"); continue; }
        };
        if !path.starts_with("/callback") {
            continue; // unrelated request
        }
        return parse_callback(&path, expected_state);
    }
}

/// Run the OAuth flow. Returns the minted `cw_app_…` token.
pub async fn run_oauth_flow(hub_url: &str) -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await
        .context("binding loopback for OAuth callback")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let (verifier, challenge) = make_pkce();
    let state = make_state();

    let mut authorize = Url::parse(hub_url)?.join("/api/v1/auth/spa/start")?;
    authorize.query_pairs_mut()
        .append_pair("redirect_uri",          &redirect_uri)
        .append_pair("state",                 &state)
        .append_pair("code_challenge",        &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("app_name",              APP_NAME);

    info!(url = %authorize, "opening browser for GitHub OAuth");
    eprintln!("\nOpening your browser to authenticate with GitHub.");
    eprintln!("If it doesn't open automatically, paste this URL:\n  {authorize}\n");
    if let Err(e) = webbrowser::open(authorize.as_str()) {
        warn!(?e, "failed to auto-open browser; you'll need to visit the URL manually");
    }

    let code = timeout(CALLBACK_TIMEOUT, wait_for_callback(listener, &state))
        .await
        .map_err(|_| anyhow!("OAuth callback didn't arrive within {:?} — aborting", CALLBACK_TIMEOUT))?
        .context("waiting for OAuth callback")?;

    info!("received OAuth code — exchanging for token");
    let exchange_url = Url::parse(hub_url)?.join("/api/v1/auth/spa/exchange")?;
    let client = reqwest::Client::builder()
        .user_agent(format!("{APP_NAME}/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let resp = client.post(exchange_url)
        .json(&ExchangeBody { code: &code, code_verifier: &verifier })
        .send()
        .await
        .context("POST /spa/exchange")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("token exchange failed: {status} {body}");
    }
    let parsed: ExchangeResponse = resp.json().await.context("parsing exchange response")?;
    Ok(parsed.token)
}
