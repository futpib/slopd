# Architecture

## Overview

**slopd** is a tmux-integrated daemon for managing Claude AI sessions programmatically. Three crates:

- **libslop**: Shared types, config, IPC protocol, hook injection utilities
- **slopd**: Daemon — manages tmux, handles requests, coordinates hooks
- **slopctl**: CLI client — speaks to slopd over a Unix socket

## Data Flow

```
Claude (in tmux pane)
  └─ fires hook → slopctl hook <Event> (via settings.json hook commands)
                      │
slopctl ──────────────┤──── run / send / kill / status
                      │
                 Unix socket ($XDG_RUNTIME_DIR/slopd/slopd.sock)
                      │
slopd ────────────────┘
  ├─ manages tmux sessions/panes
  ├─ per-pane state: type_mutex + Notify
  └─ modifies ~/.claude/settings.json (hook injection)
```

## Protocol

Line-delimited JSON over a Unix domain socket. Tagged enums:

- **Requests**: `Ping`, `Status`, `Run`, `Kill{pane_id}`, `Hook{event, payload, pane_id}`, `Send{pane_id, prompt}`
- **Responses**: `Pong`, `Status{uptime}`, `Run{pane_id}`, `Kill{pane_id}`, `Hooked`, `Sent{pane_id}`, `Error{message}`

## Notable Design Decisions

**Send delivery confirmation** — `Send` subscribes to a `Notify` *before* sending keystrokes, then awaits `UserPromptSubmit` hook. The `type_mutex` serialises concurrent senders per pane so keystrokes don't interleave. Correct, but:
- No timeout — if Claude never fires `UserPromptSubmit`, `Send` hangs forever
- `notified()` is created before `send-keys`, so a fast delivery can't be missed

**Hook injection** — idempotent, checks before inserting. But no file locking — racy if Claude simultaneously writes `settings.json`.

**PaneMap never shrinks** — `DashMap<String, Arc<PaneState>>` grows with every pane ever spawned and is never pruned. Low risk today but worth cleaning up on `Kill`.

**Config failures are silent** — invalid TOML falls back to defaults with just a `warn!`. A user misconfiguring the socket path would see confusing behaviour.

**Request IDs echoed but not validated** — works fine for slopctl's sequential request pattern, but would break multiplexed clients.

## What's Well Done

- Isolated tmux socket per test — no host tmux interference
- SIGTERM → graceful tokio drain → LLVM coverage flush
- Hook injection idempotency tested
- `DashMap` + per-pane `Mutex`/`Notify` is the right concurrency primitive choice
- XDG path conventions throughout

## Suggestions

1. **Add a timeout to `Send`** — `tokio::time::timeout(Duration, notified)` to avoid hanging if a pane dies mid-send
2. **Prune PaneState on Kill** — `panes.remove(&pane_id)` in the Kill handler
3. **File-lock `settings.json` writes** — atomic write via temp file + rename
4. **Validate pane exists before Send** — `tmux has-pane -t <pane_id>` before acquiring the mutex
