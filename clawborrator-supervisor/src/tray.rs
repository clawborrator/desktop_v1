// System-tray integration (Windows). Gives the operator a passive
// "yes, the supervisor is running" signal in their notification area
// when the daemon was launched by Task Scheduler with no console.
//
// Architecture — three threads, one runtime, one shutdown signal:
//
//   main thread (this fn):
//     builds the tray + menu, then runs the Win32 message loop.
//     `tray-icon` requires the icon to be created on the same thread
//     that pumps messages; that thread is the only one Windows
//     dispatches WM_* events on.
//
//   menu-event drainer (side thread):
//     loops on `MenuEvent::receiver()`, dispatches per item:
//       Open dashboard   → opens hub URL in default browser
//       Open log folder  → opens %LOCALAPPDATA%/clawborrator/ in
//                          Explorer (the daily-rolled files inside
//                          are named supervisor.log.YYYY-MM-DD;
//                          opening a single file directly would
//                          either name the wrong day or 404 on day
//                          rollover, so we let the operator pick)
//       Quit             → flips the shutdown watch + PostQuitMessage
//
//   tokio worker thread:
//     owns the `run_daemon` future. `tokio::select!`s the daemon
//     against the shutdown watch so a tray Quit cleanly unwinds the
//     reconnect loop / WS instead of being process-killed.
//
// Why not `winit` for the loop: a single while-GetMessageW pump is
// ~10 lines of windows-sys; pulling in winit would be heavier and
// would conflict with `tray-icon`'s own internal HWND.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::thread;

use anyhow::{anyhow, Context, Result};
use tracing::{info, warn};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
    Icon, TrayIcon, TrayIconBuilder,
};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MsgWaitForMultipleObjects, PeekMessageW, PostQuitMessage,
    PostThreadMessageW, TranslateMessage, MSG, PM_REMOVE, QS_ALLINPUT, WM_QUIT,
};

use crate::Cli;
use crate::status::{TrayStatus, TrayStatusUpdater};

const TRAY_PNG: &[u8] = include_bytes!("../assets/tray.png");
const TOOLTIP:  &str  = "clawborrator-supervisor";

struct MenuIds {
    dashboard: MenuId,
    log:       MenuId,
    quit:      MenuId,
}

/// Refs the status-watcher thread mutates when the daemon publishes
/// a `TrayStatus` change. Cloned out of `build_menu`/`run_with_tray`
/// so the watcher thread owns its own handles.
struct StatusUiHandles {
    /// The disabled header menu item; gets `set_text` to reflect
    /// "connecting…" / "connected" / "AUTH FAILED — run `login`".
    header: MenuItem,
    /// The tray icon itself; gets `set_tooltip` so the same status
    /// is visible on hover even when the menu isn't open.
    tray:   TrayIcon,
}

/// Run the daemon with a system-tray UI on Windows. Blocks the main
/// thread until the operator picks Quit (or the daemon crashes /
/// completes on its own). Returns whatever the daemon's last result
/// was — a tray Quit yields Ok(()).
pub fn run_with_tray(cli: Cli, log_path: PathBuf) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let (status_updater, status_rx) = TrayStatusUpdater::channel();
    // Resolve the effective hub URL for the "Open dashboard" tray
    // entry. Mirrors the same precedence used at daemon-start so the
    // menu link tracks whatever hub this daemon is actually connected
    // to (flag/env > cfg cache > built-in default).
    let cfg_for_dash = crate::load_or_init_config().context("loading config for tray dashboard URL")?;
    let hub_url = crate::effective_hub_url(&cli, &cfg_for_dash);

    // Capture the main thread id BEFORE spawning workers so the
    // daemon thread can post WM_QUIT here when it exits (e.g. no
    // cached token, hub-permanent-401, panic) — without it the
    // message loop would keep the tray icon alive past the daemon's
    // death, leaving a zombie UI.
    let main_thread_id = unsafe { GetCurrentThreadId() };

    // Tokio worker — owns the daemon future. `select!` lets a tray
    // Quit unwind through the future's drop chain instead of going
    // through std::process::exit. The status updater lets the
    // daemon's WS reader push connect-state transitions to the
    // tray's watcher thread (below).
    let daemon_status = status_updater.clone();
    let daemon_handle = thread::spawn(move || -> Result<()> {
        let res = runtime.block_on(async move {
            let mut shutdown_rx = shutdown_rx;
            tokio::select! {
                res = crate::run_daemon(cli, daemon_status) => res,
                _   = shutdown_rx.changed() => {
                    info!("tray Quit received; shutting down daemon");
                    Ok(())
                }
            }
        });
        // Daemon exited (success, error, or shutdown signal) — wake
        // the main thread's message loop. PostThreadMessageW posts
        // to a specific thread's queue; PostQuitMessage would post
        // to *this* thread's queue and never reach main.
        unsafe { PostThreadMessageW(main_thread_id, WM_QUIT, 0, 0); }
        res
    });

    // Build tray + menu on the main thread.
    let (menu, ids, header) = build_menu()?;
    let icon = decode_icon().context("decoding embedded tray icon")?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(TOOLTIP)
        .with_icon(icon)
        .build()
        .map_err(|e| anyhow!("creating tray icon: {e}"))?;

    // Menu-event drainer — fire-and-forget. Holds shutdown_tx and
    // posts WM_QUIT to break the message loop on Quit.
    thread::spawn({
        let log_path = log_path.clone();
        move || drain_menu_events(ids, hub_url, log_path, shutdown_tx)
    });

    // tray-icon's TrayIcon and MenuItem are Rc<RefCell<...>> — they
    // can ONLY be mutated from the thread that built them (this one,
    // main). So instead of spawning a watcher thread, the message
    // loop itself drains pending TrayStatus events between message
    // dispatches. Status latency is bounded by the loop's wakeup
    // interval (250ms) — fine for human perception.
    let handles = StatusUiHandles { header, tray };
    run_message_loop_with_status(handles, status_rx);

    // Tray exited; tell the daemon if it hasn't already noticed and
    // collect its result. join().unwrap() because the daemon thread
    // panicking is fatal regardless.
    match daemon_handle.join() {
        Ok(res) => res,
        Err(_)  => Err(anyhow!("daemon thread panicked")),
    }
}

fn build_menu() -> Result<(Menu, MenuIds, MenuItem)> {
    let menu = Menu::new();

    // Header item — disabled. Initial label is the connect placeholder;
    // the status-watcher thread mutates this text on each
    // TrayStatus transition (connecting / connected / AUTH FAILED).
    // Returned to the caller so the watcher can hold its own clone.
    let header = MenuItem::new("clawborrator-supervisor — starting…", false, None);
    menu.append(&header).map_err(menu_err)?;
    menu.append(&PredefinedMenuItem::separator()).map_err(menu_err)?;

    let dashboard = MenuItem::with_id("dashboard", "Open dashboard",  true, None);
    let log       = MenuItem::with_id("log",       "Open log folder", true, None);
    let quit      = MenuItem::with_id("quit",      "Quit",             true, None);

    menu.append(&dashboard).map_err(menu_err)?;
    menu.append(&log).map_err(menu_err)?;
    menu.append(&PredefinedMenuItem::separator()).map_err(menu_err)?;
    menu.append(&quit).map_err(menu_err)?;

    Ok((menu, MenuIds {
        dashboard: dashboard.id().clone(),
        log:       log.id().clone(),
        quit:      quit.id().clone(),
    }, header))
}

/// Apply each pending `TrayStatus` to the menu header text + tray
/// tooltip. Best-effort — `set_tooltip` errors are swallowed (the
/// tray-icon crate occasionally returns transient ones during menu
/// rebuilds; not worth crashing the loop over). Drained in chunks
/// inside `run_message_loop_with_status` so all UI mutation stays
/// on the main thread.
fn apply_pending_status(handles: &StatusUiHandles, rx: &Receiver<TrayStatus>) {
    while let Ok(status) = rx.try_recv() {
        let header_text  = format!("clawborrator-supervisor — {}", status.label());
        let tooltip_text = status.tooltip();
        handles.header.set_text(&header_text);
        if let Err(e) = handles.tray.set_tooltip(Some(&tooltip_text)) {
            warn!(?e, "tray set_tooltip failed");
        }
    }
}

fn menu_err(e: tray_icon::menu::Error) -> anyhow::Error {
    anyhow!("menu error: {e}")
}

fn decode_icon() -> Result<Icon> {
    let img = image::load_from_memory_with_format(TRAY_PNG, image::ImageFormat::Png)
        .context("decoding tray.png")?
        .into_rgba8();
    let (w, h) = img.dimensions();
    Icon::from_rgba(img.into_raw(), w, h)
        .map_err(|e| anyhow!("Icon::from_rgba: {e}"))
}

/// Three-way classification of a `MenuEvent` against our known IDs.
/// `Unknown` covers any future menu item whose handler hasn't been
/// wired (or, in practice, debounce events the crate sometimes emits).
enum MenuAction {
    OpenDashboard,
    OpenLog,
    Quit,
    Unknown,
}

fn classify(ev: &tray_icon::menu::MenuEvent, ids: &MenuIds) -> MenuAction {
    if      ev.id == ids.dashboard { MenuAction::OpenDashboard }
    else if ev.id == ids.log       { MenuAction::OpenLog }
    else if ev.id == ids.quit      { MenuAction::Quit }
    else                           { MenuAction::Unknown }
}

fn drain_menu_events(
    ids:         MenuIds,
    hub_url:     String,
    log_path:    PathBuf,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
) {
    for ev in MenuEvent::receiver() {
        match classify(&ev, &ids) {
            MenuAction::OpenDashboard => {
                if let Err(e) = webbrowser::open(&hub_url) {
                    warn!(?e, hub_url = %hub_url, "failed to open dashboard");
                }
            }
            // The log file is daily-rolled by tracing_appender — actual
            // names are `supervisor.log.YYYY-MM-DD`, so the bare
            // `supervisor.log` path the rest of the daemon carries
            // around as a label doesn't exist on disk. Open the
            // containing folder instead so the operator can pick the
            // current day's file (or scroll back through the history).
            MenuAction::OpenLog => open_path(log_path.parent().unwrap_or(&log_path)),
            MenuAction::Quit => {
                info!("tray Quit clicked");
                let _ = shutdown_tx.send(true);
                unsafe { PostQuitMessage(0); }
                return;
            }
            MenuAction::Unknown => {}
        }
    }
}

/// Open a path in the user's default app. On Windows that's the
/// shell's "open" verb, dispatched via `cmd /c start`. Best-effort —
/// failures only get a warn line, not a user-facing error.
fn open_path(path: &std::path::Path) {
    let path_str = match path.to_str() {
        Some(s) => s,
        None => { warn!(path = %path.display(), "log path is not valid UTF-8"); return; }
    };
    // `start` needs a window-title arg before the path because it
    // treats a quoted first arg as the title. Empty title works.
    let r = std::process::Command::new("cmd")
        .args(["/c", "start", "", path_str])
        .spawn();
    if let Err(e) = r {
        warn!(?e, path = %path.display(), "failed to open path");
    }
}

/// Win32 message pump that ALSO drains pending TrayStatus updates
/// from the daemon. We can't spawn a separate watcher thread because
/// tray-icon's TrayIcon + MenuItem are Rc<RefCell<…>> (not Send) —
/// every UI mutation has to happen on the thread that built them.
///
/// Loop shape: `MsgWaitForMultipleObjects` blocks for up to 250ms
/// waiting for either a Win32 message OR the timer. On wake-up we
/// drain queued status updates first, then process all pending Win32
/// messages with `PeekMessageW(PM_REMOVE)`. Loop exits on WM_QUIT.
fn run_message_loop_with_status(
    handles:   StatusUiHandles,
    status_rx: Receiver<TrayStatus>,
) {
    const STATUS_POLL_MS: u32 = 250;
    unsafe {
        loop {
            // Wake when EITHER a Win32 message arrives OR ~250ms
            // has passed (so we pick up channel updates with bounded
            // latency even when no menu activity is happening).
            let _ = MsgWaitForMultipleObjects(0, std::ptr::null(), 0, STATUS_POLL_MS, QS_ALLINPUT);

            apply_pending_status(&handles, &status_rx);

            // Drain everything Win32 has queued for us. PeekMessageW
            // returns 0 when the queue is empty.
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                if msg.message == WM_QUIT { return; }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}
