// Session manager — owns the per-managed-session state on the
// daemon side. Each running CC has:
//
//   - a PTY master (write end for typing into CC, read end for the
//     vt100 reader to consume)
//   - a child process handle (so we can kill it on `session.kill`)
//   - a vt100 parser fed by every byte that comes off the PTY,
//     queryable for `session.screenshot`
//
// The map is keyed by the hub's session UUID (resolved from the
// sidecar file CC's MCP writes). Until the sidecar lands, sessions
// live under a temporary "pending key" so kill/screenshot can still
// reach them by routingName if needed. Phase-1 simplification: we
// only key by sessionId post-sidecar; pre-sidecar kills happen by
// returning the pending PTY child from session.create itself.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use portable_pty::{Child, MasterPty, PtySize};
use vt100::Parser;

pub const PTY_ROWS: u16 = 40;
pub const PTY_COLS: u16 = 120;

// Session-scoped state.
//
// Lifecycle gotcha: `take_writer()` returns an OWNED writer that
// wraps an OS handle. When that writer drops, the handle closes,
// which on Windows ConPTY closes CC's stdin → CC reads EOF → CC
// exits. We can't let the auto-enter task own the writer because
// the task ends after 10s and would take the writer with it. So
// we wrap it in Arc<Mutex<...>> here; the auto-enter task gets a
// clone, the session keeps the original alive, and CC's stdin
// stays open for the full session lifetime.
//
// `_master` is retained for resize() in a future slice. `folder`
// + `routing_name` are retained for `session.restart`. `scratch_dir`
// + `channel_token_id` are retained for `session.destroy`.
pub struct ManagedSession {
    pub _session_id:      String,
    pub folder:           PathBuf,
    pub routing_name:     Option<String>,
    pub _master:          Box<dyn MasterPty + Send>,
    pub _writer:          Arc<Mutex<Box<dyn std::io::Write + Send>>>,
    pub child:            Box<dyn Child + Send + Sync>,
    pub parser:           Arc<Mutex<Parser>>,
    pub scratch_dir:      PathBuf,
    pub channel_token_id: i64,
}

#[derive(Default)]
pub struct SessionManager {
    inner: Mutex<HashMap<String, Arc<Mutex<ManagedSession>>>>,
}

impl SessionManager {
    pub fn new() -> Self { Self::default() }

    pub fn insert(&self, sid: String, sess: ManagedSession) {
        self.inner.lock().unwrap().insert(sid, Arc::new(Mutex::new(sess)));
    }

    pub fn get(&self, sid: &str) -> Result<Arc<Mutex<ManagedSession>>> {
        self.inner.lock().unwrap().get(sid)
            .cloned()
            .ok_or_else(|| anyhow!("no managed session with id {sid}"))
    }

    pub fn remove(&self, sid: &str) -> Option<Arc<Mutex<ManagedSession>>> {
        self.inner.lock().unwrap().remove(sid)
    }

    /// Return the session id of any currently-managed session
    /// whose folder matches `folder`. Used to refuse a second
    /// concurrent create in the same folder — two CC instances
    /// sharing a sidecar file at .claude/clawborrator.session.json
    /// race-write each other and produce undefined behavior.
    pub fn find_by_folder(&self, folder: &std::path::Path) -> Option<String> {
        let map = self.inner.lock().unwrap();
        for (sid, entry) in map.iter() {
            if let Ok(s) = entry.lock() {
                if s.folder == folder { return Some(sid.clone()); }
            }
        }
        None
    }

    /// Snapshot of all currently-managed session ids. Sent in the
    /// supervisor `hello` frame so the hub can reconcile its
    /// managed_by_machine_id state against the daemon's actual
    /// in-memory truth (after a daemon restart, this is `[]` and
    /// the hub clears managed_by for everything that was
    /// previously managed by this machine).
    pub fn list_session_ids(&self) -> Vec<String> {
        self.inner.lock().unwrap().keys().cloned().collect()
    }
}

pub fn fresh_pty_size() -> PtySize {
    PtySize { rows: PTY_ROWS, cols: PTY_COLS, pixel_width: 0, pixel_height: 0 }
}
