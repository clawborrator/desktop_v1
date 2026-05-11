// ParserWatcher — per-session background task that polls the vt100
// parser's screen snapshot, runs the plugin registry against it, and
// dispatches matched actions. Replaces the AUTO_ENTER blind pump.
//
// Lifecycle:
//   - Spawned once per session create / respawn / soft-restart.
//   - Owns a clone of the SharedWriter (so it can write to CC's
//     stdin) and the Arc<Mutex<Parser>> (so it can read the screen).
//   - For RestartWithoutFlag actions, emits over an mpsc channel
//     handed in by the spawn site — the receiver lives wherever the
//     respawn logic is wired (see spawn.rs).
//   - Exits when the parser is dropped (Arc count → 1) OR the
//     watcher_cancel one-shot fires (e.g. session destroy). Either
//     terminates the loop cleanly without orphan tasks.
//
// Fire-once: each plugin's `name()` is added to the fired set on
// first match, so a plugin can't keep poking the same prompt. A
// soft-restart spawns a fresh watcher with a clean fired set.
//
// Polling cadence: 250ms. Tight enough to dismiss a typical CC
// prompt within ~half a second of it appearing; light enough to
// not show up in CPU profiling.

use std::collections::HashSet;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::time::sleep;
use tracing::{debug, info, warn};
use vt100::Parser;

use super::{Action, PluginRegistry, ScreenView};
use crate::sessions::SharedWriter;

const POLL_INTERVAL_MS: u64 = 250;

#[derive(Debug, Clone)]
pub struct RestartRequest {
    pub session_id:    String,
    pub flag_to_strip: String,
}

pub struct WatcherHandle {
    pub cancel_tx: Option<oneshot::Sender<()>>,
}

impl WatcherHandle {
    pub fn cancel(&mut self) {
        if let Some(tx) = self.cancel_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Spawn the watcher task. The returned handle can be used to
/// cancel it (e.g. on session destroy). The `restart_tx` channel
/// is signalled when a plugin returns a RestartWithoutFlag action;
/// the receiver side handles tearing down + respawning the session.
pub fn spawn_watcher(
    session_id: String,
    parser:     Arc<Mutex<Parser>>,
    writer:     SharedWriter,
    registry:   Arc<PluginRegistry>,
    restart_tx: Option<mpsc::UnboundedSender<RestartRequest>>,
) -> WatcherHandle {
    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    let sid_for_task = session_id.clone();

    tokio::spawn(async move {
        let mut fired: HashSet<&'static str> = HashSet::new();
        let mut last_text = String::new();
        info!(session_id = %sid_for_task, plugins = registry.plugins().len(),
              "parser watcher started");

        loop {
            tokio::select! {
                _ = sleep(Duration::from_millis(POLL_INTERVAL_MS)) => {}
                _ = &mut cancel_rx => {
                    info!(session_id = %sid_for_task, "parser watcher canceled");
                    return;
                }
            }

            let snapshot = match snapshot_screen(&parser) {
                Some(s) => s,
                None => continue,
            };

            // Skip plugin dispatch when the screen is unchanged since
            // the last tick — cheap optimization for an idle CC.
            if snapshot.text == last_text { continue; }
            last_text = snapshot.text.clone();

            for plugin in registry.plugins() {
                let name = plugin.name();
                if fired.contains(name) { continue; }
                let Some(action) = plugin.inspect(&snapshot) else { continue; };

                match &action {
                    Action::WriteBytes(bytes) => {
                        info!(session_id = %sid_for_task, plugin = name,
                              bytes_len = bytes.len(), "plugin matched → writing bytes");
                        match writer.lock() {
                            Ok(mut w) => {
                                if let Err(e) = w.write_all(bytes) {
                                    warn!(session_id = %sid_for_task, plugin = name, ?e,
                                          "plugin write failed");
                                    continue; // don't mark fired — retry next tick
                                }
                                let _ = w.flush();
                            }
                            Err(_) => {
                                warn!(session_id = %sid_for_task, plugin = name,
                                      "writer mutex poisoned; skipping plugin");
                                continue;
                            }
                        }
                    }
                    Action::RestartWithoutFlag(flag) => {
                        info!(session_id = %sid_for_task, plugin = name, flag = %flag,
                              "plugin matched → requesting restart-without-flag");
                        match &restart_tx {
                            Some(tx) => {
                                let req = RestartRequest {
                                    session_id:    sid_for_task.clone(),
                                    flag_to_strip: flag.clone(),
                                };
                                if let Err(e) = tx.send(req) {
                                    warn!(session_id = %sid_for_task, plugin = name, ?e,
                                          "restart channel closed; skipping");
                                }
                            }
                            None => {
                                warn!(session_id = %sid_for_task, plugin = name,
                                      "no restart channel wired; ignoring action");
                            }
                        }
                    }
                }
                fired.insert(name);
                debug!(session_id = %sid_for_task, plugin = name, "plugin marked fired");
            }
        }
    });

    WatcherHandle { cancel_tx: Some(cancel_tx) }
}

fn snapshot_screen(parser: &Arc<Mutex<Parser>>) -> Option<ScreenView> {
    let guard = parser.lock().ok()?;
    let screen = guard.screen();
    let text = screen.contents();
    let cursor = screen.cursor_position();
    Some(ScreenView::from_text(text, cursor))
}
