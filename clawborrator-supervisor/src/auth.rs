// Operator-facing authentication. The browser-based OAuth+PKCE flow
// itself lives in `oauth.rs`; this module wraps it with the
// surrounding lifecycle:
//
//   - login: walk the cache → identity-verify → run-oauth → cache
//     decision tree, return a structured outcome the subcommand
//     handler can render as user-facing text
//   - logout: server-revoke the token then clear the local cache
//   - fetch_user_login: GET /api/v1/me to confirm a token works
//     and surface the resolved github login
//
// Splitting these out keeps `main.rs::cmd_login` / `cmd_logout` to
// pure presentation logic — load config, dispatch, render outcome,
// save config — without inlining the network primitives.

use anyhow::{anyhow, bail, Context, Result};
use url::Url;

use crate::oauth;
use crate::Config;

/// Result of `login`. Distinguished so the caller knows whether to
/// save_config (only after a fresh mint, not the cached-token path)
/// and what to print to the operator.
pub enum LoginOutcome {
    /// A cached token already authenticates against this hub URL.
    /// `cfg` was not mutated; no save needed.
    AlreadyLoggedIn { login: String },
    /// Walked the OAuth flow and minted a fresh token. `cfg` has
    /// been mutated; caller should save_config. `login` is the
    /// /me response, or None if /me failed (token still valid for
    /// WS register, just couldn't confirm identity post-mint).
    Authenticated   { login: Option<String> },
}

/// Resolve the cached-vs-fresh-OAuth path and update `cfg` in place.
/// Caller is responsible for `save_config(&cfg)` after the
/// `Authenticated` branch (and choosing not to after `AlreadyLoggedIn`,
/// since nothing changed).
pub async fn login(cfg: &mut Config, hub_url: &str, force: bool) -> Result<LoginOutcome> {
    if !force && token_matches_hub(cfg, hub_url) {
        let cached = cfg.token.as_deref().expect("token_matches_hub guarantees Some");
        if let Ok(login) = fetch_user_login(hub_url, cached).await {
            return Ok(LoginOutcome::AlreadyLoggedIn { login });
        }
        // Cached token rejected by the hub — fall through to re-auth.
    }
    let token = oauth::run_oauth_flow(hub_url, &cfg.machine_id).await
        .with_context(|| "OAuth login failed")?;
    cfg.token   = Some(token.clone());
    cfg.hub_url = Some(hub_url.to_string());
    let login = fetch_user_login(hub_url, &token).await.ok();
    Ok(LoginOutcome::Authenticated { login })
}

/// Result of `logout`. The local cache is cleared regardless of the
/// server-side revoke outcome — the operator wants out, and a hung
/// hub shouldn't leave a stale token on disk pretending to be valid.
pub enum LogoutOutcome {
    Revoked,
    NoCachedToken,
    RevokeFailed(anyhow::Error),
}

/// POST `/api/v1/auth/logout` to revoke the cached token (best-effort)
/// and clear `cfg.token` + `cfg.hub_url`. `cfg.machine_id` is preserved
/// so a subsequent `login` reuses the same install identity.
pub async fn logout(cfg: &mut Config, hub_url: &str) -> LogoutOutcome {
    let outcome = match cfg.token.clone() {
        None => LogoutOutcome::NoCachedToken,
        Some(token) => match revoke_token(hub_url, &token).await {
            Ok(())  => LogoutOutcome::Revoked,
            Err(e)  => LogoutOutcome::RevokeFailed(e),
        },
    };
    cfg.token   = None;
    cfg.hub_url = None;
    outcome
}

/// Cached token matches the hub it was minted against — both
/// conditions must hold for the cache to count as "still valid for
/// this hub URL".
fn token_matches_hub(cfg: &Config, hub_url: &str) -> bool {
    cfg.token.is_some() && cfg.hub_url.as_deref() == Some(hub_url)
}

/// GET `/api/v1/me` — returns `githubLogin`. Used after login to
/// confirm the token works AND to surface the resolved user to the
/// operator.
pub async fn fetch_user_login(hub_url: &str, token: &str) -> Result<String> {
    let url = Url::parse(hub_url)?.join("/api/v1/me")?;
    let resp = reqwest::Client::new()
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .context("GET /api/v1/me")?;
    if !resp.status().is_success() {
        bail!("hub returned {}", resp.status());
    }
    let body: serde_json::Value = resp.json().await.context("parsing /api/v1/me response")?;
    body.get("githubLogin")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing githubLogin in /me response"))
}

/// POST `/api/v1/auth/logout` — revokes the bearer token server-side.
/// Idempotent on the hub; this just translates HTTP errors to anyhow.
async fn revoke_token(hub_url: &str, token: &str) -> Result<()> {
    let url = Url::parse(hub_url)?.join("/api/v1/auth/logout")?;
    let resp = reqwest::Client::new()
        .post(url)
        .bearer_auth(token)
        .send()
        .await
        .context("POST /api/v1/auth/logout")?;
    if !resp.status().is_success() {
        bail!("hub returned {}", resp.status());
    }
    Ok(())
}
