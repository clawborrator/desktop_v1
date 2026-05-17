# desktop_v1

`clawborrator-supervisor` — desktop daemon that connects a workstation
to a clawborrator hub and lets the hub spawn / control Claude Code
sessions on it. Pre-built Windows binary on
[GitHub Releases](https://github.com/clawborrator/desktop_v1/releases).

## Status

| | |
|---|---|
| Hub WS handshake + reconnect | shipped |
| OAuth login (SPA + PKCE) | shipped |
| Session create / kill / restart / destroy | shipped |
| Screenshot + input forwarding | shipped |
| Windows: per-user Task Scheduler autostart | shipped (v0.1.0) |
| Windows: system-tray icon + graceful Quit | shipped (v0.2.0) |
| Linux: systemd user-service autostart (headless) | shipped |
| macOS autostart | stubbed; install fails with NotYetImplemented |
| Push events (`desktop.health` etc) | open |

## Quick start (Windows)

1. Download the latest
   [`clawborrator-supervisor-windows-x64.exe`](https://github.com/clawborrator/desktop_v1/releases/latest)
   from GitHub Releases.

2. Open PowerShell, sign in:
   ```pwsh
   .\clawborrator-supervisor-windows-x64.exe login
   ```
   This opens a browser, authenticates against the hub via GitHub
   OAuth + PKCE, and caches a `cw_app_…` Bearer token at
   `%USERPROFILE%\.clawborrator\desktop_v1.json`.

3. Register the daemon as a per-user Task Scheduler entry — no admin
   elevation required:
   ```pwsh
   .\clawborrator-supervisor-windows-x64.exe install-task
   ```

4. Log out and back in. The daemon launches silently and a system-tray
   icon appears (probably in the overflow chevron — drag it to pin).
   Logs land at `%LOCALAPPDATA%\clawborrator\supervisor.log`.

5. Open https://next.clawborrator.com/orchard/, click `+ Session`,
   pick a folder, and the daemon spawns a managed Claude Code session
   you and any collaborators you share with can drive in real time.

## Subcommands

| Command | Description |
|---|---|
| `login [--force]` | OAuth flow + cache a token. `--force` re-auths over an existing token. |
| `logout` | Server-side revoke + clear local cache. `machine_id` preserved. |
| `install-task` | Register the autostart entry (Windows only). Requires a cached token. |
| `uninstall-task` | Remove the autostart entry (idempotent). |
| `task-status` | Report whether the autostart entry is registered. |
| (no subcommand) | Run the daemon. |

`--hub-url` / `CLAWBORRATOR_HUB_URL` overrides the default
`https://next.clawborrator.com`. `--pat` / `CLAWBORRATOR_PAT` overrides
the cached token (useful for ad-hoc testing with a token from another
machine).

## Tray icon (Windows)

When the daemon is running it shows a tray icon with a right-click
menu:

- **Open dashboard** — opens the configured hub URL in your default browser
- **View log** — opens `supervisor.log` in your default app
- **Quit** — graceful shutdown (the daemon path through Task Manager
  is hard-kill; prefer Quit so the WS connection closes cleanly)

The release Windows build links as the GUI subsystem, so Task Scheduler
launches it without a console flash. Subcommands invoked from
PowerShell still print to the parent shell via
`AttachConsole(ATTACH_PARENT_PROCESS)`.

## Quick start (Linux)

Linux runs the daemon **headless** as a **systemd user service**. No
tray icon, no GUI. Tested against systemd distros (Debian / Ubuntu /
Fedora / Arch / openSUSE); non-systemd distros (Alpine / Void /
Gentoo musl) work via `cargo run` but the `install-task` subcommand
will fail.

### Prerequisites

The supervisor binary itself is self-contained, but it SPAWNS Claude
Code under a PTY for each session, and that Claude Code instance
launches the `clawborrator-mcp` server via `npx`. So the Linux host
needs:

```sh
# Claude Code CLI (official installer; drops binary at ~/.local/bin/claude)
curl -fsSL https://claude.ai/install.sh | bash
claude setup-token        # one-time auth

# Node.js + npm + npx (for the clawborrator-mcp server CC launches)
sudo apt install -y nodejs npm     # Debian / Ubuntu
# OR
sudo dnf install -y nodejs npm     # Fedora / RHEL
```

Verify both are on PATH:
```sh
which claude npx
```
Both should print paths. Without them, session-create will fail with
`502: spawning claude` (claude missing) or the MCP server will fail to
start mid-session (npm/npx missing).

1. Download the latest
   [`clawborrator-supervisor-linux-x64`](https://github.com/clawborrator/desktop_v1/releases/latest)
   from GitHub Releases, or `cargo build --release -p
   clawborrator-supervisor` from source.

2. **(Server installs only — once per machine)** Enable linger so
   your user systemd manager runs before login. Skipping this on a
   fresh SSH-only box means `install-task` will fail at
   `daemon-reload` with "No medium found" because the user manager
   isn't running yet:
   ```sh
   sudo loginctl enable-linger "$USER"
   ```
   Then re-login over SSH (or `export
   XDG_RUNTIME_DIR=/run/user/$(id -u)` for the current shell only).
   Desktop installs can usually skip this since a graphical login
   has already started the user manager.

3. Sign in. Default uses **OAuth device flow** — works on any host,
   no browser needed locally, you approve from any phone/laptop:
   ```sh
   ./clawborrator-supervisor login
   ```
   Prints a verification URL + short code. Open on any device,
   enter the code, approve on GitHub. The daemon's poller picks up
   the token + caches it at `$HOME/.clawborrator/desktop_v1.json`.

   Desktop users with a local browser can opt into the legacy
   browser-callback flow with `login --browser` instead.

4. Register the systemd user service. No root needed:
   ```sh
   ./clawborrator-supervisor install-task
   ```
   Writes `~/.config/systemd/user/clawborrator-supervisor.service`,
   runs `systemctl --user daemon-reload`, enables the unit.

5. Start it now without waiting for boot:
   ```sh
   systemctl --user start clawborrator-supervisor
   ```

6. Watch the logs live:
   ```sh
   journalctl --user -u clawborrator-supervisor -f
   ```

7. Open https://next.clawborrator.com/orchard/, click `+ Session`,
   pick a folder, and the daemon spawns a managed Claude Code
   session that you and any collaborators you share with can drive
   in real time.

To uninstall:
```sh
./clawborrator-supervisor uninstall-task
```
Disables the unit, stops it, removes the unit file, runs
`daemon-reload`. Linger is left untouched. Revert linger with
`sudo loginctl disable-linger "$USER"` if you also want that.

## macOS

Stubbed. `install-task` returns `NotYetImplemented`. The daemon
itself runs from source:

```sh
cargo run -p clawborrator-supervisor -- login
cargo run -p clawborrator-supervisor
```

launchd LaunchAgent integration lands when a macOS user needs it.

## Architecture (Windows daemon path)

Three threads, one tokio runtime, one shutdown signal:

| Thread | Owns |
|---|---|
| Main | tray icon + Win32 `GetMessageW` message pump (thread-affine) |
| Tokio worker | `run_daemon`: WS to `/supervisor`, reconnect loop, command dispatch |
| Menu drainer | `MenuEvent::receiver()` loop, dispatches Open / View log / Quit |

Tray Quit flips a `tokio::sync::watch` so the daemon unwinds the WS
cleanly. Daemon-thread exit (success, error, panic) posts `WM_QUIT` to
the main thread so the tray icon dies with the process — no zombie UI.

## Configuration

| Path | Contents |
|---|---|
| `%USERPROFILE%\.clawborrator\desktop_v1.json` (Windows) | `machine_id`, cached token, hub URL the token was minted against |
| `~/.clawborrator/desktop_v1.json` (macOS / Linux) | same |
| `%LOCALAPPDATA%\clawborrator\supervisor.log` (Windows) | daily-rolling daemon log |
| `~/Library/Application Support/clawborrator/supervisor.log` (macOS) | same |
| `~/.local/share/clawborrator/supervisor.log` (Linux) | same |

## Build from source

```sh
cargo build --release -p clawborrator-supervisor
```

Outputs `target/release/clawborrator-supervisor.exe` (Windows) or
`clawborrator-supervisor` elsewhere. Requires Rust 1.75+. Logo asset
at `clawborrator-supervisor/assets/tray.png` is embedded via
`include_bytes!` at compile time.

## Logging

`RUST_LOG` honored. Defaults to `info`.

```sh
RUST_LOG=info,clawborrator_supervisor=debug cargo run -p clawborrator-supervisor
```

The release binary logs to file (no stdout — windowless GUI build).
Debug builds keep the console subsystem so `cargo run` shows logs
inline.

## License

MIT OR Apache-2.0.
