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
use std::thread;

use anyhow::{anyhow, Context, Result};
use tracing::{info, warn};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
    Icon, TrayIconBuilder,
};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PostQuitMessage, PostThreadMessageW, TranslateMessage,
    MSG, WM_QUIT,
};

use crate::Cli;

const TRAY_PNG: &[u8] = include_bytes!("../assets/tray.png");
const TOOLTIP:  &str  = "clawborrator-supervisor";

struct MenuIds {
    dashboard: MenuId,
    log:       MenuId,
    quit:      MenuId,
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
    let hub_url = cli.hub_url.clone();

    // Capture the main thread id BEFORE spawning workers so the
    // daemon thread can post WM_QUIT here when it exits (e.g. no
    // cached token, hub-permanent-401, panic) — without it the
    // message loop would keep the tray icon alive past the daemon's
    // death, leaving a zombie UI.
    let main_thread_id = unsafe { GetCurrentThreadId() };

    // Tokio worker — owns the daemon future. `select!` lets a tray
    // Quit unwind through the future's drop chain instead of going
    // through std::process::exit.
    let daemon_handle = thread::spawn(move || -> Result<()> {
        let res = runtime.block_on(async move {
            let mut shutdown_rx = shutdown_rx;
            tokio::select! {
                res = crate::run_daemon(cli) => res,
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
    let (menu, ids) = build_menu()?;
    let icon = decode_icon().context("decoding embedded tray icon")?;
    let _tray = TrayIconBuilder::new()
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

    // Main thread: Win32 message pump. Returns when WM_QUIT arrives
    // (PostQuitMessage from the menu-event drainer or from Windows
    // shutdown).
    run_message_loop();

    // Tray exited; tell the daemon if it hasn't already noticed and
    // collect its result. join().unwrap() because the daemon thread
    // panicking is fatal regardless.
    match daemon_handle.join() {
        Ok(res) => res,
        Err(_)  => Err(anyhow!("daemon thread panicked")),
    }
}

fn build_menu() -> Result<(Menu, MenuIds)> {
    let menu = Menu::new();

    // Header item — disabled, just labels what's running. Cheap
    // affordance: makes the menu feel intentional rather than just
    // "Quit" floating in space.
    let header = MenuItem::new("clawborrator-supervisor", false, None);
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
    }))
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

fn run_message_loop() {
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        loop {
            let r = GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
            // GetMessageW returns 0 on WM_QUIT, -1 on error, >0 otherwise.
            // We treat both 0 and -1 as "stop pumping".
            if r <= 0 { return; }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
