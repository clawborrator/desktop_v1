// clawborrator-supervisor — Step 1 (handshake-only) + OAuth login.
//
// Connects to the hub's /supervisor WebSocket, sends a `hello` frame
// identifying this machine, then keeps the connection alive with
// 30s pings. No command handling yet — that's Step 2+. Reconnects
// with exponential backoff on disconnect so the daemon survives
// transient network blips and hub restarts.
//
// Auth: first run walks the SPA OAuth + PKCE flow (browser-based)
// to mint a `cw_app_…` Bearer token, then persists it to the local
// config. Subsequent runs reuse the cached token until you nuke
// `~/.clawborrator/desktop_v1.json`. CLAWBORRATOR_PAT env var
// overrides the cache for ad-hoc testing.
//
// Identity: the daemon assigns itself a stable machine_id stored
// in the same config file. Hostname alone isn't unique (people
// rename machines, dual-boot, VMs); the install nonce makes it
// durable across renames while staying stable across daemon
// restarts.

// Release Windows builds: link as the GUI subsystem so Task Scheduler
// (and double-clicks) don't pop a console window. Debug builds keep
// the console subsystem so `cargo run` still works ergonomically.
// Subcommands re-attach to the parent shell's console at runtime via
// AttachConsole — see `attach_parent_console_if_any`.
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod auth;
mod autostart;
mod logging;
mod oauth;
mod sessions;
mod spawn;
#[cfg(target_os = "windows")] mod tray;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::time::{sleep, Instant};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};
use url::Url;

use crate::sessions::SessionManager;
use crate::spawn::{create_session, destroy_session, input_session, kill_session, restart_session, screenshot_session, CreateArgs};

const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");
// Note: DAEMON_VERSION is sourced from Cargo.toml, so a version bump
// in Cargo.toml flows automatically to the hello frame + --version.
const DEFAULT_HUB_URL: &str = "https://next.clawborrator.com";
const PING_INTERVAL: Duration = Duration::from_secs(30);
const RECONNECT_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(60);

#[derive(Parser, Debug)]
#[command(author, version, about = "clawborrator desktop supervisor daemon")]
pub(crate) struct Cli {
    /// Hub base URL. Defaults to https://next.clawborrator.com or
    /// the CLAWBORRATOR_HUB_URL env var.
    #[arg(long, env = "CLAWBORRATOR_HUB_URL", default_value = DEFAULT_HUB_URL)]
    hub_url: String,

    /// Bearer token (`cw_pat_*` or `cw_app_*`). Read from
    /// CLAWBORRATOR_PAT env var if not provided. OAuth-driven mint
    /// flow is a follow-on.
    #[arg(long, env = "CLAWBORRATOR_PAT")]
    pat: Option<String>,

    /// Override the machine_id (otherwise read/generated from the
    /// config file at `~/.clawborrator/desktop_v1.json`).
    #[arg(long, env = "CLAWBORRATOR_MACHINE_ID")]
    machine_id: Option<String>,

    /// No subcommand = run the daemon (default). Subcommands manage
    /// the platform's autostart entry so the daemon launches at
    /// user logon.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the OAuth flow against `--hub-url`, mint an app token, and
    /// cache it locally. Required before the daemon can start. If a
    /// token is already cached for the same hub, this is a no-op
    /// unless `--force` is passed.
    Login {
        /// Re-authenticate even if a valid token is already cached.
        #[arg(long)]
        force: bool,
    },
    /// Revoke the cached app token server-side (best-effort) and
    /// clear it from the local config. The machine_id is preserved
    /// so a subsequent `login` re-uses this machine's identity.
    Logout,
    /// Register an autostart entry that launches this binary at
    /// user logon. On Windows that's a per-user Task Scheduler
    /// entry — no admin elevation needed. Requires a cached token
    /// (run `login` first).
    InstallTask,
    /// Remove the autostart entry. Idempotent.
    UninstallTask,
    /// Show whether the autostart entry is currently registered.
    TaskStatus,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct Config {
    /// Stable per-install identifier. Generated once on first run;
    /// persists across daemon restarts. NOT keyed off hostname so
    /// that a hostname change doesn't orphan the registration.
    pub(crate) machine_id: String,
    /// `cw_app_…` Bearer token minted via the OAuth flow on first
    /// run. Optional only because the file is created BEFORE the
    /// flow runs (so we have a stable machine_id during the
    /// browser round-trip). Once the OAuth flow completes the
    /// token is written back via `save_config`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) token: Option<String>,
    /// Hub URL the token was minted against. Lets us refuse to
    /// reuse a token when the operator changes hub URLs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) hub_url: Option<String>,
}

fn config_path() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .ok_or_else(|| anyhow!("could not resolve home dir"))?
        .join(".clawborrator");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {dir:?}"))?;
    Ok(dir.join("desktop_v1.json"))
}

/// Load or generate the per-install config. First run mints a fresh
/// uuid for `machine_id` and writes the file with no token (the
/// OAuth flow fills that in later). Subsequent runs reuse both.
fn load_or_init_config() -> Result<Config> {
    let path = config_path()?;
    if path.exists() {
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {path:?}"))?;
        let cfg: Config = serde_json::from_str(&text).with_context(|| "parsing config")?;
        return Ok(cfg);
    }
    let cfg = Config {
        machine_id: uuid::Uuid::new_v4().to_string(),
        token:      None,
        hub_url:    None,
    };
    save_config(&cfg)?;
    info!(path = %path.display(), "wrote fresh config with new machine_id");
    Ok(cfg)
}

fn save_config(cfg: &Config) -> Result<()> {
    let path = config_path()?;
    let json = serde_json::to_string_pretty(cfg)?;
    std::fs::write(&path, json).with_context(|| format!("writing {path:?}"))?;
    // Best-effort tighten perms on Unix; no-op on Windows.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Translate `https://host/...` → `wss://host/supervisor`. The hub
/// is HTTPS in production; localhost dev uses ws://. Either way we
/// just swap the scheme rather than asking the operator to type
/// two URLs.
fn ws_url_for_supervisor(hub_url: &str) -> Result<Url> {
    let mut u = Url::parse(hub_url).with_context(|| format!("invalid hub url: {hub_url}"))?;
    let scheme = match u.scheme() {
        "https" => "wss",
        "http"  => "ws",
        other => return Err(anyhow!("unsupported hub scheme: {other}")),
    };
    u.set_scheme(scheme).map_err(|_| anyhow!("set_scheme failed"))?;
    u.set_path("/supervisor");
    Ok(u)
}

#[derive(Serialize, Debug)]
#[serde(tag = "t", rename_all = "snake_case")]
enum OutFrame<'a> {
    /// First frame after WS handshake. Hub uses it to register +
    /// validate the daemon. `current_sessions` lets the hub
    /// reconcile its managed_by_machine_id state — on a fresh
    /// daemon start this is empty, so the hub clears managed_by
    /// for all of this machine's previously-managed sessions
    /// (their CCs are dead since the daemon was their parent).
    /// `version` lets the hub gracefully downgrade for older
    /// daemons.
    Hello {
        machine_id:       &'a str,
        daemon_version:   &'a str,
        hostname:         &'a str,
        capabilities:     &'a [&'a str],
        current_sessions: &'a [String],
    },
    Ping,
    Pong,
    /// RPC success — replies to a hub `cmd` frame by id.
    Ok {
        id:   &'a str,
        data: serde_json::Value,
    },
    /// RPC failure — replies to a hub `cmd` frame by id.
    Err {
        id:      &'a str,
        code:    &'a str,
        message: &'a str,
    },
}

#[derive(Deserialize, Debug)]
#[serde(tag = "t", rename_all = "snake_case")]
enum InFrame {
    /// Hub acknowledges the registration. Carries server-assigned
    /// fields the daemon may want to log (e.g. the user this PAT
    /// resolved to). Optional fields are flexible while the
    /// protocol is in flux.
    HelloAck {
        #[serde(default)] user_login: Option<String>,
        #[serde(default)] message:    Option<String>,
    },
    /// Hub-initiated ping; daemon responds with Pong.
    Ping,
    /// Reply to a daemon-initiated Ping.
    Pong,
    /// Hub-initiated RPC. Daemon executes `op` against `args` and
    /// responds with an `ok` (carrying `data`) or `err` frame
    /// keyed by the same `id`.
    Cmd {
        id:   String,
        op:   String,
        #[serde(default)] args: serde_json::Value,
    },
    /// Catch-all so unknown frames don't kill the connection.
    #[serde(other)]
    Unknown,
}

/// Per-connection context threaded through every command handler.
/// Owns the session-manager Arc + the auth bits the daemon needs
/// to mint channel tokens / stamp managedBy on session.create.
struct DaemonCtx {
    pub hub_url:    String,
    pub pat:        String,
    pub machine_id: String,
    pub mgr:        Arc<SessionManager>,
}

async fn run_session(ctx: &DaemonCtx, ws_url: &Url, cfg: &Config) -> Result<()> {
    let host = hostname::get()
        .ok()
        .and_then(|s| s.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string());

    info!(url = %ws_url, "connecting to /supervisor");

    let mut request: Request = ws_url.as_str().into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", ctx.pat).parse().context("invalid PAT for header")?,
    );

    let (mut ws, response) = tokio_tungstenite::connect_async(request)
        .await
        .with_context(|| "ws handshake failed")?;
    info!(status = %response.status(), "ws connected");

    // Send the hello frame immediately; hub registers us against
    // (userId-from-token, machine_id) and responds with HelloAck.
    let current_sessions = ctx.mgr.list_session_ids();
    let hello = OutFrame::Hello {
        machine_id:       &cfg.machine_id,
        daemon_version:   DAEMON_VERSION,
        hostname:         &host,
        capabilities:     &["session.create", "session.kill", "session.destroy", "session.restart", "session.screenshot", "session.input"],
        current_sessions: &current_sessions,
    };
    ws.send(Message::Text(serde_json::to_string(&hello)?)).await?;

    let mut next_ping = Instant::now() + PING_INTERVAL;

    loop {
        let now = Instant::now();
        let until_ping = if next_ping > now { next_ping - now } else { Duration::ZERO };
        tokio::select! {
            biased;
            _ = sleep(until_ping) => {
                ws.send(Message::Text(serde_json::to_string(&OutFrame::Ping)?)).await?;
                next_ping = Instant::now() + PING_INTERVAL;
            }
            msg = ws.next() => {
                let Some(msg) = msg else { return Err(anyhow!("ws stream ended")); };
                if !handle_ws_message(ctx, &mut ws, msg?).await? {
                    return Ok(());
                }
            }
        }
    }
}

/// WS-frame demux. Pulled out of `run_session`'s `tokio::select!` arm
/// so the loop body stays flat and the per-variant handling lives in
/// one place. Returns `Ok(true)` to keep looping; `Ok(false)` for a
/// clean exit (hub-initiated Close); `Err` for protocol failures the
/// caller should propagate.
async fn handle_ws_message<S>(
    ctx: &DaemonCtx,
    ws:  &mut tokio_tungstenite::WebSocketStream<S>,
    msg: Message,
) -> Result<bool>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    match msg {
        Message::Text(text)   => { handle_text(ctx, ws, &text).await?; Ok(true) }
        Message::Binary(_)    => { warn!("ignoring unexpected binary frame"); Ok(true) }
        Message::Ping(p)      => { ws.send(Message::Pong(p)).await?; Ok(true) }
        Message::Pong(_)      => Ok(true),
        Message::Close(frame) => { info!(?frame, "hub closed the connection"); Ok(false) }
        Message::Frame(_)     => Ok(true),
    }
}

async fn handle_text<S>(ctx: &DaemonCtx, ws: &mut tokio_tungstenite::WebSocketStream<S>, text: &str) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let parsed: InFrame = match serde_json::from_str(text) {
        Ok(f) => f,
        Err(e) => {
            warn!(?e, raw = text, "failed to parse frame");
            return Ok(());
        }
    };
    match parsed {
        InFrame::HelloAck { user_login, message } => {
            info!(?user_login, ?message, "registered with hub");
        }
        InFrame::Ping => {
            ws.send(Message::Text(serde_json::to_string(&OutFrame::Pong)?)).await?;
        }
        InFrame::Pong => { /* application-level pong; nothing to do */ }
        InFrame::Cmd { id, op, args } => {
            handle_cmd(ctx, ws, &id, &op, args).await?;
        }
        InFrame::Unknown => {
            warn!(raw = text, "received unknown frame");
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct SessionCreateArgs {
    folder:        String,
    #[serde(default, rename = "routingName")]
    routing_name:  Option<String>,
    #[serde(default, rename = "extraFlags")]
    extra_flags:   Vec<String>,
    /// Hub forwards the operator's AUTO START / MANUAL START
    /// choice as a boolean. Default true (auto) — matches the
    /// SPA modal's default and lets older callers continue
    /// working without specifying.
    #[serde(default = "default_true", rename = "autoEnter")]
    auto_enter:    bool,
}

fn default_true() -> bool { true }

#[derive(Deserialize)]
struct SessionRefArgs {
    #[serde(rename = "sessionId")]
    session_id: String,
}

/// Restart-only args: session id + the operator's persisted
/// AUTO/MANUAL choice + the operator's persisted CLI flags. The hub
/// reads `auto_enter` and `extra_flags` off the sessions row and
/// forwards them here so a Restart preserves both the prompt-handling
/// mode AND the operator's --model / --add-dir / etc. choices, instead
/// of silently dropping them. Defaults (auto_enter=true, extra_flags
/// empty) keep older hubs / direct-CLI testing on the prior implicit
/// behavior.
#[derive(Deserialize)]
struct SessionRestartArgs {
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(default = "default_true", rename = "autoEnter")]
    auto_enter: bool,
    #[serde(default, rename = "extraFlags")]
    extra_flags: Vec<String>,
}

/// Dispatch helpers detect "no managed session" errors (the
/// SessionManager's miss path) and map them to a specific
/// `session_not_found` code. Hub-side orphan-restart relies on
/// this code to decide whether to fall back to recreate.
fn classify_dispatch_err(default_code: &str, e: anyhow::Error) -> (String, String) {
    let msg = e.to_string();
    if msg.contains("no managed session") {
        ("session_not_found".into(), msg)
    } else {
        (default_code.into(), msg)
    }
}

async fn dispatch_session_create(ctx: &DaemonCtx, args: serde_json::Value) -> std::result::Result<serde_json::Value, (String, String)> {
    let parsed: SessionCreateArgs = serde_json::from_value(args).map_err(|e| ("bad_args".into(), e.to_string()))?;
    let folder = PathBuf::from(parsed.folder);
    let routing_name_owned = parsed.routing_name;
    let extra_flags = parsed.extra_flags;
    let auto_enter = parsed.auto_enter;
    let create_args = CreateArgs {
        hub_url:      &ctx.hub_url,
        pat:          &ctx.pat,
        machine_id:   &ctx.machine_id,
        folder,
        routing_name: routing_name_owned.as_deref(),
        extra_flags:  &extra_flags,
        auto_enter,
    };
    match create_session(&ctx.mgr, create_args).await {
        Ok(session_id) => Ok(serde_json::json!({ "sessionId": session_id })),
        Err(e)         => Err(("create_failed".into(), e.to_string())),
    }
}

fn dispatch_session_kill(ctx: &DaemonCtx, args: serde_json::Value) -> std::result::Result<serde_json::Value, (String, String)> {
    let parsed: SessionRefArgs = serde_json::from_value(args).map_err(|e| ("bad_args".into(), e.to_string()))?;
    match kill_session(&ctx.mgr, &parsed.session_id) {
        Ok(())  => Ok(serde_json::json!({ "ok": true })),
        Err(e)  => Err(classify_dispatch_err("kill_failed", e)),
    }
}

async fn dispatch_session_destroy(ctx: &DaemonCtx, args: serde_json::Value) -> std::result::Result<serde_json::Value, (String, String)> {
    let parsed: SessionRefArgs = serde_json::from_value(args).map_err(|e| ("bad_args".into(), e.to_string()))?;
    match destroy_session(&ctx.mgr, &ctx.hub_url, &ctx.pat, &parsed.session_id).await {
        Ok(())  => Ok(serde_json::json!({ "ok": true })),
        Err(e)  => Err(classify_dispatch_err("destroy_failed", e)),
    }
}

async fn dispatch_session_restart(ctx: &DaemonCtx, args: serde_json::Value) -> std::result::Result<serde_json::Value, (String, String)> {
    let parsed: SessionRestartArgs = serde_json::from_value(args).map_err(|e| ("bad_args".into(), e.to_string()))?;
    match restart_session(&ctx.mgr, &ctx.hub_url, &ctx.pat, &ctx.machine_id, &parsed.session_id, parsed.auto_enter, &parsed.extra_flags).await {
        Ok(new_id) => Ok(serde_json::json!({ "sessionId": new_id })),
        Err(e)     => Err(classify_dispatch_err("restart_failed", e)),
    }
}

fn dispatch_session_screenshot(ctx: &DaemonCtx, args: serde_json::Value) -> std::result::Result<serde_json::Value, (String, String)> {
    let parsed: SessionRefArgs = serde_json::from_value(args).map_err(|e| ("bad_args".into(), e.to_string()))?;
    match screenshot_session(&ctx.mgr, &parsed.session_id) {
        Ok(v)  => Ok(v),
        Err(e) => Err(classify_dispatch_err("screenshot_failed", e)),
    }
}

#[derive(Deserialize)]
struct SessionInputArgs {
    #[serde(rename = "sessionId")]
    session_id: String,
    bytes:      String,
}

fn dispatch_session_input(ctx: &DaemonCtx, args: serde_json::Value) -> std::result::Result<serde_json::Value, (String, String)> {
    let parsed: SessionInputArgs = serde_json::from_value(args).map_err(|e| ("bad_args".into(), e.to_string()))?;
    match input_session(&ctx.mgr, &parsed.session_id, parsed.bytes.as_bytes()) {
        Ok(())  => Ok(serde_json::json!({ "ok": true, "wrote": parsed.bytes.len() })),
        Err(e)  => Err(classify_dispatch_err("input_failed", e)),
    }
}

// Dispatch a hub-initiated RPC. Each verb returns either Ok(data) →
// daemon emits `ok` frame, or Err((code, message)) → daemon emits
// `err`. session.restart is intentionally still unimplemented —
// requires retaining the create-args for the original session, which
// is its own slice. session.create / kill / screenshot are real.
async fn handle_cmd<S>(
    ctx: &DaemonCtx,
    ws:  &mut tokio_tungstenite::WebSocketStream<S>,
    id:  &str,
    op:  &str,
    args: serde_json::Value,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let result: std::result::Result<serde_json::Value, (String, String)> = match op {
        "session.create"     => dispatch_session_create(ctx, args).await,
        "session.kill"       => dispatch_session_kill(ctx, args),
        "session.destroy"    => dispatch_session_destroy(ctx, args).await,
        "session.restart"    => dispatch_session_restart(ctx, args).await,
        "session.screenshot" => dispatch_session_screenshot(ctx, args),
        "session.input"      => dispatch_session_input(ctx, args),
        other                => Err(("unknown_op".into(), format!("unknown op: {other}"))),
    };
    match result {
        Ok(data) => ws.send(Message::Text(serde_json::to_string(&OutFrame::Ok { id, data })?)).await?,
        Err((code, msg)) => ws.send(Message::Text(serde_json::to_string(&OutFrame::Err {
            id, code: &code, message: &msg,
        })?)).await?,
    }
    Ok(())
}

async fn run_with_reconnect(ctx: DaemonCtx, ws_url: Url, cfg: Config) -> Result<()> {
    let mut backoff = RECONNECT_BACKOFF_INITIAL;
    loop {
        match run_session(&ctx, &ws_url, &cfg).await {
            Ok(()) => {
                info!("session ended cleanly; reconnecting in {:?}", RECONNECT_BACKOFF_INITIAL);
                backoff = RECONNECT_BACKOFF_INITIAL;
            }
            Err(e) => {
                error!(?e, "session error; reconnecting in {:?}", backoff);
            }
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
    }
}

/// Resolve the Bearer token to use. Priority:
///   1. `--pat` flag / CLAWBORRATOR_PAT env (always wins; ad-hoc)
///   2. cached token in `~/.clawborrator/desktop_v1.json` (verified
///      to match `--hub-url`)
///
/// No implicit OAuth fallback — the daemon can be launched from a
/// non-interactive context (Task Scheduler at logon, with no console
/// or browser available), where opening a browser would silently fail.
/// Use the `login` subcommand to mint a token interactively.
fn resolve_token(cli: &Cli, cfg: &Config) -> Result<String> {
    if let Some(t) = &cli.pat {
        return Ok(t.clone());
    }
    let cached = cfg.token.as_deref().ok_or_else(|| anyhow!(
        "no cached app token — run `clawborrator-supervisor login` first to authenticate"
    ))?;
    if cfg.hub_url.as_deref() != Some(cli.hub_url.as_str()) {
        return Err(anyhow!(
            "cached token was minted against {:?}, not {} — run `clawborrator-supervisor login` against the new hub",
            cfg.hub_url, cli.hub_url,
        ));
    }
    Ok(cached.to_string())
}

/// Re-attach to the parent shell's console on Windows release builds
/// so `eprintln!` / subcommand output reach the user. The release
/// binary is `windows_subsystem = "windows"` (no console allocated by
/// default) which is great for Task Scheduler launches but breaks
/// `clawborrator-supervisor.exe install-task` invoked from PowerShell.
/// AttachConsole(ATTACH_PARENT_PROCESS) silently no-ops if the parent
/// has no console (e.g. Task Scheduler), so it's safe to always call.
#[cfg(all(target_os = "windows", not(debug_assertions)))]
fn attach_parent_console_if_any() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS); }
}

#[cfg(not(all(target_os = "windows", not(debug_assertions))))]
fn attach_parent_console_if_any() {}

async fn run_subcommand(cli: &Cli, cmd: Command) -> Result<()> {
    let provider = autostart::current();
    match cmd {
        Command::Login { force } => cmd_login(cli, force).await,
        Command::Logout          => cmd_logout(cli).await,
        Command::InstallTask     => install_task(provider),
        Command::UninstallTask   => uninstall_task(provider),
        Command::TaskStatus      => task_status(provider),
    }
}

async fn cmd_login(cli: &Cli, force: bool) -> Result<()> {
    let mut cfg = load_or_init_config()?;
    match auth::login(&mut cfg, &cli.hub_url, force).await? {
        auth::LoginOutcome::AlreadyLoggedIn { login } => {
            eprintln!("Already logged in as {} at {}.", login, cli.hub_url);
            eprintln!("Pass --force to re-authenticate.");
        }
        auth::LoginOutcome::Authenticated { login } => {
            save_config(&cfg)?;
            match login {
                Some(l) => eprintln!("Logged in as {} at {}.", l, cli.hub_url),
                None    => eprintln!("Logged in (couldn't confirm identity via /api/v1/me)."),
            }
            eprintln!("Run `install-task` next if you want the daemon to launch at logon.");
        }
    }
    Ok(())
}

async fn cmd_logout(cli: &Cli) -> Result<()> {
    let mut cfg = load_or_init_config()?;
    match auth::logout(&mut cfg, &cli.hub_url).await {
        auth::LogoutOutcome::Revoked         => eprintln!("Hub revoked the token."),
        auth::LogoutOutcome::NoCachedToken   => eprintln!("No cached token; nothing to revoke server-side."),
        auth::LogoutOutcome::RevokeFailed(e) => warn!(?e, "hub /auth/logout failed; clearing local cache anyway"),
    }
    save_config(&cfg)?;
    eprintln!("Local config cleared. machine_id preserved.");

    let provider = autostart::current();
    if let Ok(autostart::AutostartStatus::Installed { .. }) = provider.status() {
        eprintln!("Note: autostart is still installed — without a token the daemon will fail at next logon.");
        eprintln!("Run `uninstall-task` to remove it, or `login` to mint a fresh token.");
    }
    Ok(())
}

fn install_task(provider: &dyn autostart::AutostartProvider) -> Result<()> {
    let cfg = load_or_init_config()?;
    if cfg.token.is_none() {
        anyhow::bail!("no cached token — run `clawborrator-supervisor login` first; otherwise the autostart entry will fire with no credentials");
    }
    let exe = std::env::current_exe().context("resolving current exe path")?;
    eprintln!("Installing autostart via {} for: {}", provider.facility_name(), exe.display());
    eprintln!("Token cached for hub: {}", cfg.hub_url.as_deref().unwrap_or("<none>"));
    provider.install(&exe)?;
    eprintln!("Done. The supervisor will launch at your next logon.");
    Ok(())
}

fn uninstall_task(provider: &dyn autostart::AutostartProvider) -> Result<()> {
    eprintln!("Removing autostart from {}…", provider.facility_name());
    provider.uninstall()?;
    eprintln!("Done.");
    Ok(())
}

fn task_status(provider: &dyn autostart::AutostartProvider) -> Result<()> {
    match provider.status()? {
        autostart::AutostartStatus::Installed { details } => {
            eprintln!("Installed ({}). {}", provider.facility_name(), details);
        }
        autostart::AutostartStatus::NotInstalled => {
            eprintln!("Not installed ({} — run `install-task` to register).", provider.facility_name());
        }
    }
    Ok(())
}

pub(crate) async fn run_daemon(cli: Cli) -> Result<()> {
    let mut cfg = load_or_init_config()?;
    if let Some(forced) = cli.machine_id.clone() { cfg.machine_id = forced; }
    info!(machine_id = %cfg.machine_id, daemon = DAEMON_VERSION, "starting");

    let token = resolve_token(&cli, &cfg)?;
    let ws_url = ws_url_for_supervisor(&cli.hub_url)?;

    let ctx = DaemonCtx {
        hub_url:    cli.hub_url.clone(),
        pat:        token,
        machine_id: cfg.machine_id.clone(),
        mgr:        Arc::new(SessionManager::new()),
    };

    tokio::select! {
        res = run_with_reconnect(ctx, ws_url, cfg) => res,
        _ = tokio::signal::ctrl_c() => {
            info!("ctrl-c received; shutting down");
            Ok(())
        }
    }
}

fn main() -> Result<()> {
    attach_parent_console_if_any();

    let mut cli = Cli::parse();

    // Subcommand path — short-lived, console-driven. tokio on the
    // main thread is fine because there's no tray to run there.
    if let Some(cmd) = cli.command.take() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("building tokio runtime")?;
        return runtime.block_on(run_subcommand(&cli, cmd));
    }

    // Daemon path — long-running, file-based logging always on.
    let log = logging::init().context("initializing logging")?;
    info!(log_path = %log.log_path.display(), "logs will be written here");

    // Windows: tray owns the main thread (Win32 message loop is
    // thread-affine). The daemon future runs on a tokio worker
    // started inside `tray::run_with_tray`.
    #[cfg(target_os = "windows")]
    {
        tray::run_with_tray(cli, log.log_path.clone())
    }

    // Non-Windows: no tray yet — tokio on main, run the daemon
    // directly. macOS/Linux tray support lands when their autostart
    // facilities do (PR4+).
    #[cfg(not(target_os = "windows"))]
    {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("building tokio runtime")?;
        runtime.block_on(run_daemon(cli))
    }
}
