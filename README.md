# desktop_v1

Lean rewrite of the desktop daemon, built around the hub's `/supervisor`
WebSocket as the single command channel. Step 1 is handshake-only; later
steps add session create/kill/restart, vt100 terminal mirroring, and
managed-vs-unmanaged session bookkeeping. See the chat history for the
full design.

## Status

- [x] Step 1 — handshake-only client (this crate)
- [x] Step 1.5 — OAuth login (browser-based SPA + PKCE)
- [ ] Step 2 — push events (`session.state`, `desktop.health`)
- [ ] Step 3 — folder allowlist + tray UX for adding folders
- [ ] Step 4 — read-only commands (`session.list`, `daemon.version`)
- [ ] Step 5 — state-changing commands (`session.create/kill/restart`) with audit log
- [ ] Step 6 — vt100 + `session.screenshot` + live frame streaming

## Run

```sh
# Optional — defaults to https://next.clawborrator.com
export CLAWBORRATOR_HUB_URL=https://next.clawborrator.com

cargo run -p clawborrator-supervisor
```

First run opens your browser to authenticate with GitHub, then writes
the minted `cw_app_…` token to `~/.clawborrator/desktop_v1.json`
alongside the daemon's stable `machine_id`. Subsequent runs reuse the
cached token. The file is `0600` on Unix; on Windows the per-user
profile dir is the only protection.

To override the cached token (e.g., for ad-hoc testing with a token
copied from another tool), set `CLAWBORRATOR_PAT`:

```sh
export CLAWBORRATOR_PAT=cw_app_…   # or cw_sess_…
cargo run -p clawborrator-supervisor
```

To wipe and re-register as a fresh machine (new `machine_id`, fresh
OAuth flow), delete `~/.clawborrator/desktop_v1.json`.

## Logging

`RUST_LOG` honored. Defaults to `info`.

```sh
RUST_LOG=info,clawborrator_supervisor=debug cargo run -p clawborrator-supervisor
```

## Why a separate workspace from `desktop/`

The existing `desktop/` workspace solves a different problem (local
process supervision via Unix-socket IPC, with HTTPS polling for hub state).
desktop_v1 inverts that: the hub *commands* the daemon over a long-lived
WS, and the daemon's only job is to be a passive executor of those
commands. Different protocol, different lifecycle, easier to keep them
separate while the new shape settles.

The two are not currently expected to merge — desktop_v1 will eventually
subsume the user-facing pieces of `desktop/` once Steps 2-6 land.
