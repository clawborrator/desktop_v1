// Spawn flow for `session.create`. Steps, in order:
//
//   1. Validate the folder exists + is a directory.
//   2. Mint a channel token via the hub REST API. The token + hub
//      URL go into a temporary .mcp.json that we write under
//      ~/.clawborrator/sessions/<scratch>/ — NOT into the user's
//      project folder, so we don't pollute their repo with a
//      file containing a live secret.
//   3. Spawn `claude --dangerously-load-development-channels=
//      server:clawborrator --mcp-config <our-config>` under a PTY,
//      with cwd set to the target folder.
//   4. Kick a background reader task that drains the PTY master
//      into the vt100 parser (so screenshots work) and a dumb
//      auto-answer task that hammers Enter for 10 seconds (to
//      acknowledge CC's startup prompts). Categorized
//      startupAnswers will replace the dumb hammer in a follow-on.
//   5. Poll <folder>/.claude/clawborrator.session.json for up to
//      30 seconds for the resolved hub session id (CC's MCP writes
//      it on register). Return that id once present.
//
// On failure at any step we revoke the channel token and clean up
// the spawn-config dir so we don't accumulate orphans.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use portable_pty::{native_pty_system, CommandBuilder};
use serde::{Deserialize, Serialize};
use tokio::time::{sleep, Instant};
use tracing::{info, warn};
use vt100::Parser;

use crate::sessions::{fresh_pty_size, ManagedSession, SessionManager, PTY_COLS, PTY_ROWS};

const SIDECAR_POLL_TIMEOUT_MS: u64 = 30_000;
const AUTO_ENTER_DURATION_MS:  u64 = 5_000;
const AUTO_ENTER_INTERVAL_MS:  u64 = 1_000;

#[derive(Serialize)]
struct MintTokenBody<'a> { name: &'a str }

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
struct MintTokenResponse {
    id:    i64,
    token: String,
    name:  String,
}

#[derive(Serialize)]
struct PreflightBody<'a> {
    folder:       &'a str,
    #[serde(rename = "machineId")]
    machine_id:   &'a str,
    #[serde(rename = "routingName", skip_serializing_if = "Option::is_none")]
    routing_name: Option<&'a str>,
}

#[derive(Deserialize, Debug)]
struct PreflightResponse {
    #[serde(rename = "sessionId")]
    session_id:       String,
    #[serde(rename = "channelToken")]
    channel_token:    String,
    #[serde(rename = "channelTokenId")]
    channel_token_id: i64,
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
struct SidecarPayload {
    #[serde(rename = "sessionId")]
    session_id: String,
}

/// Preflight a managed session — hub allocates the sessionId AND
/// mints the channel token in one atomic call. Used in place of the
/// older mint+spawn+sidecar-poll dance: the daemon writes the
/// returned sessionId directly into the folder's sidecar BEFORE
/// spawning CC, so CC's clawborrator-mcp registers with that exact
/// id and the hub's UPSERT path rebinds to the preflight row.
async fn preflight(
    hub_url:      &str,
    pat:          &str,
    folder:       &str,
    machine_id:   &str,
    routing_name: Option<&str>,
) -> Result<PreflightResponse> {
    let url = format!("{}/api/v1/sessions/preflight", hub_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .user_agent(format!("clawborrator-supervisor/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let resp = client.post(url)
        .bearer_auth(pat)
        .json(&PreflightBody { folder, machine_id, routing_name })
        .send()
        .await
        .context("POST /sessions/preflight")?;
    if !resp.status().is_success() {
        let s = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("preflight failed: {s} {body}");
    }
    Ok(resp.json::<PreflightResponse>().await?)
}

/// Mint a channel token via REST. Returned token goes into the
/// .mcp.json so CC's MCP can register against the hub.
#[allow(dead_code)]
async fn mint_channel_token(hub_url: &str, pat: &str, name: &str) -> Result<MintTokenResponse> {
    let url = format!("{}/api/v1/tokens/channel", hub_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .user_agent(format!("clawborrator-supervisor/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let resp = client.post(url)
        .bearer_auth(pat)
        .json(&MintTokenBody { name })
        .send()
        .await
        .context("POST /tokens/channel")?;
    if !resp.status().is_success() {
        let s = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("token mint failed: {s} {body}");
    }
    Ok(resp.json::<MintTokenResponse>().await?)
}

async fn revoke_channel_token(hub_url: &str, pat: &str, id: i64) {
    let url = format!("{}/api/v1/tokens/{}", hub_url.trim_end_matches('/'), id);
    let client = match reqwest::Client::builder().build() {
        Ok(c) => c, Err(_) => return,
    };
    let _ = client.delete(url).bearer_auth(pat).send().await;
}

#[allow(dead_code)]
async fn patch_session_managed_by(hub_url: &str, pat: &str, session_id: &str, machine_id: &str) -> Result<()> {
    let url = format!("{}/api/v1/sessions/{}", hub_url.trim_end_matches('/'), session_id);
    let client = reqwest::Client::builder().build()?;
    let resp = client.patch(url)
        .bearer_auth(pat)
        .json(&serde_json::json!({ "managedByMachineId": machine_id }))
        .send()
        .await
        .context("PATCH /sessions/:id")?;
    if !resp.status().is_success() {
        let s = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("session managedBy patch failed: {s} {body}");
    }
    Ok(())
}

fn write_mcp_json(scratch_dir: &PathBuf, hub_ws_url: &str, token: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(scratch_dir).with_context(|| format!("creating {scratch_dir:?}"))?;
    let mcp_path = scratch_dir.join(".mcp.json");
    let json = serde_json::json!({
        "mcpServers": {
            "clawborrator": {
                "command": "npx",
                "args":    ["-y", "clawborrator-mcp"],
                "env": {
                    "CLAWBORRATOR_HUB_URL": hub_ws_url,
                    "CLAWBORRATOR_TOKEN":   token,
                }
            }
        }
    });
    std::fs::write(&mcp_path, serde_json::to_string_pretty(&json)? + "\n")
        .with_context(|| format!("writing {mcp_path:?}"))?;
    Ok(mcp_path)
}

fn hub_ws_url(hub_url: &str) -> String {
    let trimmed = hub_url.trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("https://") {
        return format!("wss://{rest}");
    }
    if let Some(rest) = trimmed.strip_prefix("http://") {
        return format!("ws://{rest}");
    }
    trimmed.to_string()
}

/// Spawn CC under a PTY in `folder`, configured to load the MCP
/// at `mcp_path`. Returns the PTY master + child handle so the
/// caller can wire reader/writer tasks.
fn spawn_cc(folder: &PathBuf, mcp_path: &PathBuf, extra_flags: &[String]) -> Result<(Box<dyn portable_pty::MasterPty + Send>, Box<dyn portable_pty::Child + Send + Sync>)> {
    let pty = native_pty_system().openpty(fresh_pty_size())
        .map_err(|e| anyhow!("openpty: {e}"))?;
    let mut cmd = CommandBuilder::new("claude");
    cmd.cwd(folder);
    cmd.arg("--dangerously-load-development-channels=server:clawborrator");
    cmd.arg("--mcp-config");
    cmd.arg(mcp_path.as_os_str());
    // Operator-supplied extra flags. Appended last so they can
    // override our defaults if needed (CC's CLI takes the last
    // value when a flag repeats). One argv slot per entry —
    // operators pass `--model opus` as two entries `["--model","opus"]`
    // or as a single `["--model=opus"]`.
    for flag in extra_flags {
        if !flag.is_empty() { cmd.arg(flag); }
    }
    // Explicit terminal type. portable-pty doesn't set this on
    // Windows ConPTY by default, and CC's TUI banner falls back
    // to a degraded layout (boxes collapse onto single rows) when
    // it doesn't see a known terminfo. xterm-256color is the
    // standard truecolor terminal everywhere — vt100 emulator on
    // our side speaks the same dialect. COLORTERM=truecolor lets
    // CC's startup banner emit 24-bit color escape sequences.
    cmd.env("TERM",      "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    let child = pty.slave.spawn_command(cmd)
        .map_err(|e| anyhow!("spawn claude: {e}"))?;
    // We can drop the slave handle now — the spawned child holds
    // the only reference that matters.
    drop(pty.slave);
    Ok((pty.master, child))
}

/// Drive a reader thread that pumps PTY bytes into a vt100 parser
/// shared with the session manager. Runs until the reader returns
/// 0 bytes (PTY closed), at which point the task exits.
fn spawn_pty_reader(mut reader: Box<dyn std::io::Read + Send>, parser: Arc<Mutex<Parser>>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0)  => break,
                Ok(n)  => {
                    if let Ok(mut p) = parser.lock() {
                        p.process(&buf[..n]);
                    }
                }
                Err(e) => {
                    warn!(?e, "pty reader: read failed");
                    break;
                }
            }
        }
    });
}

/// Hammer Enter into the PTY for AUTO_ENTER_DURATION_MS. Stopgap
/// for CC's startup prompts until the categorized prompt-detector
/// lands. Takes a SHARED writer (Arc<Mutex<...>>) — owning it
/// would close CC's stdin when the task ends, which exits CC.
/// See ManagedSession for the lifecycle commentary.
#[allow(dead_code)]
fn spawn_auto_enter(writer: Arc<Mutex<Box<dyn std::io::Write + Send>>>) {
    tokio::spawn(async move {
        let deadline = Instant::now() + Duration::from_millis(AUTO_ENTER_DURATION_MS);
        while Instant::now() < deadline {
            {
                let mut w = match writer.lock() {
                    Ok(g) => g,
                    Err(_) => break,
                };
                if w.write_all(b"\r").is_err() { break; }
                let _ = w.flush();
            }
            sleep(Duration::from_millis(AUTO_ENTER_INTERVAL_MS)).await;
        }
    });
}

#[allow(dead_code)]
async fn poll_sidecar(folder: &PathBuf) -> Result<String> {
    let sidecar = folder.join(".claude").join("clawborrator.session.json");
    let deadline = Instant::now() + Duration::from_millis(SIDECAR_POLL_TIMEOUT_MS);
    while Instant::now() < deadline {
        if sidecar.exists() {
            if let Ok(text) = std::fs::read_to_string(&sidecar) {
                if let Ok(parsed) = serde_json::from_str::<SidecarPayload>(&text) {
                    return Ok(parsed.session_id);
                }
            }
        }
        sleep(Duration::from_millis(500)).await;
    }
    bail!("sidecar at {} did not appear within {SIDECAR_POLL_TIMEOUT_MS}ms", sidecar.display())
}

pub struct CreateArgs<'a> {
    pub hub_url:      &'a str,
    pub pat:          &'a str,
    pub machine_id:   &'a str,
    pub folder:       PathBuf,
    pub routing_name: Option<&'a str>,
    /// Extra CLI flags to append after the daemon-required ones
    /// (`--dangerously-load-development-channels=server:clawborrator`
    /// + `--mcp-config <path>`). Each entry is a single argv slot;
    /// e.g. `["--model", "opus"]` or `["--add-dir=/repos/foo"]`.
    /// Operator-supplied via the SPA's create-session modal; the
    /// daemon does NOT validate them — CC owns the contract.
    pub extra_flags:  &'a [String],
    /// When true, the daemon types Enter once per second for
    /// AUTO_ENTER_DURATION_MS after spawn — dismisses CC's
    /// default-yes startup prompts (Trust folder, Load MCP,
    /// etc.). Operator can disable from the SPA when they need
    /// to answer prompts themselves.
    pub auto_enter:   bool,
}

pub async fn create_session(mgr: &SessionManager, args: CreateArgs<'_>) -> Result<String> {
    if !args.folder.is_dir() {
        bail!("folder does not exist or is not a directory: {}", args.folder.display());
    }
    // Refuse a concurrent create in a folder that already has an
    // active managed session. Two CC instances in the same folder
    // would race on .claude/clawborrator.session.json (the sidecar
    // file the daemon polls for the resolved hub session id) and
    // produce undefined behavior — at best the second create's
    // RPC times out; at worst it binds to the first session.
    if let Some(existing) = mgr.find_by_folder(&args.folder) {
        bail!(
            "folder {} already has an active managed session ({}). Kill or destroy it first, or pick a different folder.",
            args.folder.display(), existing,
        );
    }
    // Clear any stale persisted-session file BEFORE spawning. The
    // MCP (clawborrator-mcp) reads <cwd>/.claude/clawborrator/
    // session.json on boot to rebind the same hub session id
    // across `claude --resume`. We're about to overwrite it with
    // the preflight id, but if a stale one exists from a prior
    // session in this folder its sessionId would otherwise survive
    // the file-rewrite branch in the MCP — defensive nuke.
    //
    // (We also nuke the legacy <cwd>/.claude/clawborrator.session.
    // json path I wrote to in earlier daemon builds, on the off
    // chance an operator's project still has one lying around.)
    let persisted_path = args.folder.join(".claude").join("clawborrator").join("session.json");
    if persisted_path.exists() {
        let _ = std::fs::remove_file(&persisted_path);
    }
    let legacy_sidecar = args.folder.join(".claude").join("clawborrator.session.json");
    if legacy_sidecar.exists() {
        let _ = std::fs::remove_file(&legacy_sidecar);
    }

    // Stage 1 — preflight: hub pre-allocates the sessionId AND
    // mints the channel token in one call. We get back both
    // pieces and don't have to wait for CC's MCP to register.
    let pre = preflight(args.hub_url, args.pat, &args.folder.to_string_lossy(), args.machine_id, args.routing_name)
        .await
        .context("preflight")?;
    info!(session_id = %pre.session_id, "preflight allocated session");

    // Stage 2 — write the persisted-session file with the preflight
    // sessionId. clawborrator-mcp reads `<cwd>/.claude/clawborrator/
    // session.json` on boot via loadPersistedSessionId() and uses
    // that id when registering against /channel WS — so by writing
    // it here BEFORE spawning CC, the MCP's register frame carries
    // the exact preflight sessionId and the hub's upsert binds to
    // the existing preflight row instead of minting a sibling.
    let persisted_dir = args.folder.join(".claude").join("clawborrator");
    if let Err(e) = std::fs::create_dir_all(&persisted_dir) {
        bail!("creating {} failed: {e}", persisted_dir.display());
    }
    // Drop a sibling .gitignore on first creation so the dir as a
    // whole stays out of source control — same pattern the MCP
    // uses, kept consistent so manual cleanup is symmetric.
    let gitignore = persisted_dir.join(".gitignore");
    if !gitignore.exists() {
        let _ = std::fs::write(&gitignore, "*\n");
    }
    let sidecar_path = persisted_dir.join("session.json");
    let sidecar_json = serde_json::json!({
        "sessionId":  pre.session_id,
        "routingName": args.routing_name,
        "hubUrl":      hub_ws_url(args.hub_url),
        "writtenAt":   chrono::Utc::now().to_rfc3339(),
    });
    if let Err(e) = std::fs::write(&sidecar_path, serde_json::to_string_pretty(&sidecar_json)? + "\n") {
        bail!("writing persisted-session {} failed: {e}", sidecar_path.display());
    }
    info!(path = %sidecar_path.display(), "wrote persisted-session with preflight sessionId");

    // Stage 3 — write a daemon-managed .mcp.json with the
    // preflight token. Per-session scratch dir under
    // ~/.clawborrator/sessions/ so concurrent sessions don't
    // collide.
    let scratch_dir = match dirs::home_dir() {
        Some(h) => h.join(".clawborrator").join("sessions").join(uuid::Uuid::new_v4().to_string()),
        None    => bail!("could not resolve home dir"),
    };
    let ws_url = hub_ws_url(args.hub_url);
    let mcp_path = write_mcp_json(&scratch_dir, &ws_url, &pre.channel_token)
        .context("writing .mcp.json")?;
    info!(path = %mcp_path.display(), "wrote .mcp.json");

    // Stage 4 — spawn CC under a PTY.
    let (master, child) = spawn_cc(&args.folder, &mcp_path, args.extra_flags)
        .context("spawning claude")?;

    // Stage 5 — wire reader + (optionally) auto-enter. Writer is
    // shared between the session (owns it) and the auto-enter
    // task (borrows via Arc<Mutex<...>>). Dropping the writer
    // would close CC's stdin and exit CC, so the session retains
    // the strong ref; when AUTO START is enabled, an additional
    // task holds a clone for AUTO_ENTER_DURATION_MS.
    //
    // AUTO START: daemon presses Enter ~5x at 1s intervals to
    //   dismiss CC's default-yes startup prompts (Trust folder,
    //   Load MCP, Load dev channels). Fast path; assumes operator
    //   wants the defaults.
    // MANUAL START: no auto-enter. Operator drives the prompts
    //   via the screenshot PIP keystroke capture or `claw session
    //   input`. Required when a non-default answer is needed
    //   (e.g. declining --dangerously-skip-permissions).
    let parser = Arc::new(Mutex::new(Parser::new(PTY_ROWS, PTY_COLS, 0)));
    let reader = master.try_clone_reader().map_err(|e| anyhow!("try_clone_reader: {e}"))?;
    spawn_pty_reader(reader, parser.clone());
    let writer = Arc::new(Mutex::new(master.take_writer().map_err(|e| anyhow!("take_writer: {e}"))?));
    if args.auto_enter {
        info!("AUTO START — pressing Enter for {AUTO_ENTER_DURATION_MS}ms to dismiss startup prompts");
        spawn_auto_enter(writer.clone());
    } else {
        info!("MANUAL START — operator will drive startup prompts via screenshot PIP / claw session input");
    }

    // Stage 6 — register the live state in the manager. The
    // sessionId is already known (from preflight), so we just
    // insert and return immediately. SPA's `/sessions` poll will
    // see the new row right away (it was inserted hub-side at
    // preflight time); the operator can open the screenshot PIP,
    // focus it, and answer CC's startup prompts manually.
    mgr.insert(pre.session_id.clone(), ManagedSession {
        _session_id:      pre.session_id.clone(),
        folder:           args.folder.clone(),
        routing_name:     args.routing_name.map(|s| s.to_string()),
        _master:          master,
        _writer:          writer,
        child,
        parser,
        scratch_dir:      scratch_dir.clone(),
        channel_token_id: pre.channel_token_id,
    });
    Ok(pre.session_id)
}

pub fn kill_session(mgr: &SessionManager, session_id: &str) -> Result<()> {
    let entry = mgr.remove(session_id)
        .ok_or_else(|| anyhow!("no managed session with id {session_id}"))?;
    let mut s = entry.lock().unwrap();
    s.child.kill().map_err(|e| anyhow!("kill: {e}"))?;
    info!(session_id, "killed CC process");
    Ok(())
}

/// Restart a managed session. Snapshots the original create-args
/// (folder + routing_name), tears the existing session down via
/// `destroy_session` so the SPA's old row gets purged AND the
/// daemon-side scratch dir + channel token are cleaned up, then
/// spawns a fresh session in the same folder. The new session
/// gets a new hub session id; the response carries it back.
///
/// We DON'T mint into the same hub session row because the row
/// is keyed by channel-token registration — there's no "rebind"
/// path. Documented behavior: restart = new row in the same
/// folder; the old row is hard-deleted (along with its short
/// pre-kill history). Operators wanting history-preserving
/// restart should use `kill` + manually re-create.
pub async fn restart_session(
    mgr: &SessionManager,
    hub_url: &str,
    pat: &str,
    machine_id: &str,
    session_id: &str,
) -> Result<String> {
    // Snapshot the args BEFORE destroy_session pulls the entry out
    // of the manager.
    let entry = mgr.get(session_id)?;
    let (folder, routing_name) = {
        let s = entry.lock().unwrap();
        (s.folder.clone(), s.routing_name.clone())
    };
    drop(entry);

    // destroy_session runs the full daemon-side cleanup (kill +
    // scratch + revoke). We DO NOT also DELETE the hub row here —
    // that's the orchestrator's job (the hub-side restart handler
    // could call DELETE before forwarding restart, but for now
    // we leave the old row as a short-lived stub. Operator can
    // archive/purge it.)
    destroy_session(mgr, hub_url, pat, session_id).await
        .with_context(|| "destroy phase of restart")?;

    // Now spawn fresh in the same folder. The new sessionId is
    // returned to the hub, which forwards it to the SPA so the
    // operator can be auto-selected onto the new row. Restart
    // does NOT carry over extra_flags from the original create —
    // a deliberate gap; if you need the same flags after a
    // restart, recreate from the SPA. (We could persist them on
    // the session row later if it becomes a pain point.)
    create_session(mgr, CreateArgs {
        hub_url,
        pat,
        machine_id,
        folder,
        routing_name: routing_name.as_deref(),
        extra_flags:  &[],
        // Restart always runs AUTO START — this is a re-spawn of
        // a session the operator already greenlit. Manual prompts
        // would force them to re-answer the trust/MCP gates every
        // restart, which is friction without value.
        auto_enter:   true,
    }).await
}

/// Tear a managed session down completely. Steps:
///   1. Kill the CC process (best-effort — ignore "already dead").
///   2. Remove the daemon-managed scratch dir (the one holding
///      .mcp.json with the soon-to-be-revoked token).
///   3. Revoke the channel token via REST so the hub doesn't
///      accumulate orphaned cw_live_… rows.
/// Hub's DELETE /api/v1/sessions/:id wraps this — it calls
/// `session.destroy` first, then does its own DB cascade. So the
/// session row itself is NOT touched here; the daemon's job is
/// purely to clean up daemon-side state + the channel token.
pub async fn destroy_session(mgr: &SessionManager, hub_url: &str, pat: &str, session_id: &str) -> Result<()> {
    // Removing from the manager first transfers ownership of the
    // scratch_dir + channel_token_id out of the map, which lets us
    // hold them across the async revoke without keeping the lock.
    let entry = mgr.remove(session_id)
        .ok_or_else(|| anyhow!("no managed session with id {session_id}"))?;
    let (scratch_dir, channel_token_id, folder) = {
        let mut s = entry.lock().unwrap();
        // best-effort kill; child may already be dead from a prior
        // session.kill or natural exit. Don't propagate the error.
        let _ = s.child.kill();
        (s.scratch_dir.clone(), s.channel_token_id, s.folder.clone())
    };
    if scratch_dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&scratch_dir) {
            warn!(?e, dir = %scratch_dir.display(), "failed to remove scratch dir");
        } else {
            info!(dir = %scratch_dir.display(), "removed scratch dir");
        }
    }
    // Wipe the per-folder persisted-session file so a future create
    // in this same folder doesn't pick this dead session's id back
    // up from MCP's loadPersistedSessionId() path. Also nuke the
    // legacy flat-file location in case an old daemon build left
    // one.
    let persisted = folder.join(".claude").join("clawborrator").join("session.json");
    if persisted.exists() { let _ = std::fs::remove_file(&persisted); }
    let legacy_sidecar = folder.join(".claude").join("clawborrator.session.json");
    if legacy_sidecar.exists() { let _ = std::fs::remove_file(&legacy_sidecar); }
    revoke_channel_token(hub_url, pat, channel_token_id).await;
    info!(session_id, channel_token_id, "session destroyed");
    Ok(())
}

/// Type raw bytes into a managed session's PTY. The bytes are
/// written through the same Arc<Mutex<Writer>> the auto-enter
/// task uses — operator's keystrokes from the SPA's PIP land
/// here. UTF-8 string in, raw bytes out; the SPA is responsible
/// for translating special keys (Enter → "\r", Esc → "\x1b",
/// arrows → "\x1b[A" etc.) before posting.
pub fn input_session(mgr: &SessionManager, session_id: &str, bytes: &[u8]) -> Result<()> {
    let entry = mgr.get(session_id)?;
    let s = entry.lock().unwrap();
    let mut w = s._writer.lock().map_err(|_| anyhow!("writer mutex poisoned"))?;
    w.write_all(bytes).map_err(|e| anyhow!("write: {e}"))?;
    w.flush().map_err(|e| anyhow!("flush: {e}"))?;
    Ok(())
}

pub fn screenshot_session(mgr: &SessionManager, session_id: &str) -> Result<serde_json::Value> {
    let entry = mgr.get(session_id)?;
    let s = entry.lock().unwrap();
    let parser = s.parser.lock().unwrap();
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    let (cur_row, cur_col) = screen.cursor_position();
    // Explicit cell-by-cell render. screen.contents() trims runs
    // of trailing-empty cells from each row, which collapses
    // box-drawing layouts (CC's startup banner uses two boxes
    // side-by-side at columns 0 and ~80; trimming the gap
    // squishes the right box's columns onto the leftmost
    // column's row). Padding every row to `cols` characters
    // preserves the layout exactly as vt100 sees it.
    let mut text = String::with_capacity((rows as usize + 1) * (cols as usize + 2));
    for r in 0..rows {
        for c in 0..cols {
            match screen.cell(r, c) {
                Some(cell) => {
                    let s = cell.contents();
                    if s.is_empty() { text.push(' '); }
                    else { text.push_str(&s); }
                }
                None => text.push(' '),
            }
        }
        text.push('\n');
    }
    Ok(serde_json::json!({
        "rows":   rows,
        "cols":   cols,
        "text":   text,
        "cursor": { "row": cur_row, "col": cur_col },
    }))
}
