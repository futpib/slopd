# slopd

[![Coverage Status](https://coveralls.io/repos/github/futpib/slopd/badge.svg?branch=master)](https://coveralls.io/github/futpib/slopd?branch=master)

**slopd** is a Claude agent session manager daemon. It runs Claude CLI sessions inside [tmux](https://github.com/tmux/tmux) panes, exposes a Unix socket RPC API for controlling them, and streams lifecycle events (hook events from Claude) to subscribers.

`slopctl` is the companion CLI for talking to the daemon.

---

## Table of Contents

- [Overview](#overview)
- [Requirements](#requirements)
- [Installation](#installation)
- [Running the daemon](#running-the-daemon)
- [Configuration](#configuration)
  - [slopd](#slopd-config)
  - [slopctl](#slopctl-config)
- [slopctl commands](#slopctl-commands)
- [Claude hook integration](#claude-hook-integration)
- [Event system](#event-system)
- [Workspace layout](#workspace-layout)

---

## Overview

```
┌────────────────────────────────────────────────────────┐
│                        tmux                            │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐   │
│  │  pane %1    │  │  pane %2    │  │  pane %3    │   │
│  │  claude …   │  │  claude …   │  │  claude …   │   │
│  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘   │
│         │  hook events   │                │            │
└─────────┼────────────────┼────────────────┼────────────┘
          │                │                │
          └────────────────▼────────────────┘
                     slopd (daemon)
                  $XDG_RUNTIME_DIR/slopd/slopd.sock
                           │
                    slopctl (CLI client)
```

- **slopd** listens on a Unix domain socket and accepts JSON-RPC requests.
- Each Claude process runs as a pane inside a dedicated `slopd` tmux session.
- Claude's [lifecycle hooks](https://docs.anthropic.com/en/docs/claude-code/hooks) are forwarded to `slopd` by `slopctl hook`, giving the daemon real-time knowledge of what every agent is doing.
- Clients can subscribe to the event stream to react to hook events as they happen.

---

## Requirements

- **Rust** (2024 edition) — to build from source
- **tmux** — must be in `PATH`; slopd manages all Claude panes inside a tmux session
- **XDG runtime directory** (`$XDG_RUNTIME_DIR`) — socket is placed there

---

## Installation

```bash
cargo install --path slopd
cargo install --path slopctl
```

Or build without installing:

```bash
cargo build --workspace --release
```

To enable the provided systemd user service, copy `slopd.service` to `~/.config/systemd/user/` and enable it:

```bash
cp slopd.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now slopd
```

---

## Running the daemon

```bash
slopd
```

`slopd` will:

1. Create (or attach to) a tmux session named `slopd`.
2. Start listening on `$XDG_RUNTIME_DIR/slopd/slopd.sock`.
3. Inject `slopctl hook <event>` entries into Claude's `~/.claude/settings.json` so that every Claude pane started by the daemon reports its lifecycle events back.

Verbosity can be increased with `-v` / `-vv` / `-vvv` (maps to INFO / DEBUG / TRACE).

---

## Configuration

### slopd config

File: `~/.config/slopd/config.toml`

All defaults are fine for most setups. The only key you are likely to want to set is `claude_config_dir` if Claude's config lives somewhere other than `~/.claude`.

```toml
# Override the directory Claude uses for its config (mirrors CLAUDE_CONFIG_DIR).
# Uncomment if Claude is configured somewhere other than ~/.claude.
# claude_config_dir = "~/.claude"

# [tmux]
# Path to a custom tmux socket. When omitted slopd uses its default server.
# socket = "/run/user/1000/tmux-slopd.sock"

# [run]
# Command used to launch Claude. Can be a string or an array.
# executable = "claude"
# executable = ["claude", "--dangerously-skip-permissions"]

# Path to the slopctl binary injected into Claude hooks.
# slopctl = "slopctl"
```

### slopctl config

File: `~/.config/slopctl/config.toml`

Currently unused; reserved for future options.

---

## slopctl commands

All commands communicate with the running `slopd` daemon over its Unix socket.

### `slopctl status`

Print daemon uptime.

```
uptime: 1h 23m 45s
```

### `slopctl ps [--filter KEY=VALUE]`

List all panes managed by slopd.

```
PANE_ID  CREATED           SESSION_ID        PARENT  TAGS
%1       2 minutes ago     session-abc123    -       []
%2       5 seconds ago     -                 %1      [web, prod]
```

Filter by tag:

```bash
slopctl ps --filter tag=prod
```

### `slopctl run`

Open a new Claude pane in the slopd tmux session. Prints the new pane's ID on stdout.

```bash
PANE=$(slopctl run)
```

If called from within a tmux pane (i.e. `$TMUX_PANE` is set), the new pane automatically records that pane as its parent.

### `slopctl kill <PANE_ID>`

Terminate a Claude pane.

```bash
slopctl kill %2
```

### `slopctl send <PANE_ID> <PROMPT> [--timeout SECS]`

Type `PROMPT` into a pane and wait until Claude acknowledges it via the `UserPromptSubmit` hook. Defaults to a 60-second timeout.

```bash
slopctl send %1 "Summarize this file: README.md"
slopctl send %1 "Run the tests" --timeout 10
```

### `slopctl interrupt <PANE_ID>`

Send Ctrl+C, Ctrl+D, and Escape to a pane to interrupt a running agent.

```bash
slopctl interrupt %1
```

### `slopctl hook <EVENT>`

Forward a Claude lifecycle hook event to slopd. Reads the JSON payload from stdin. Normally called automatically from Claude's settings hooks — you do not need to invoke this manually.

```bash
echo '{"session_id":"abc"}' | slopctl hook SessionStart
```

### `slopctl listen [--hook EVENT] [--pane-id ID] [--session-id ID]`

Subscribe to the event stream and print events as JSON lines.

```bash
# All events
slopctl listen

# Only Stop events on a specific pane
slopctl listen --hook Stop --pane-id %1
```

### `slopctl tag <PANE_ID> <TAG>`

Add a tag to a pane. Tag names must match `[A-Za-z0-9_-]+`.

```bash
slopctl tag %1 prod
slopctl tag %1 web
```

### `slopctl untag <PANE_ID> <TAG>`

Remove a tag from a pane.

```bash
slopctl untag %1 prod
```

### `slopctl tags <PANE_ID>`

List all tags on a pane.

```bash
slopctl tags %1
# prod
# web
```

### `slopctl send-filtered <PROMPT> --filter KEY=VALUE [--select one|any|all] [--timeout SECS]`

Send a prompt to every pane that matches the given filter.

```bash
# Send to all panes tagged "worker"
slopctl send-filtered "Report your status" --filter tag=worker --select all

# Send to any one pane tagged "idle"
slopctl send-filtered "Start task X" --filter tag=idle --select one
```

`--select` values:

| Value | Behaviour |
|-------|-----------|
| `one` (default) | Exactly one matching pane must exist; error otherwise |
| `any` | Send to one arbitrarily chosen matching pane |
| `all` | Send to all matching panes |

---

## Claude hook integration

When slopd starts a Claude pane it automatically injects `slopctl hook <event>` entries into `~/.claude/settings.json` for **all** supported lifecycle events:

| Category | Events |
|----------|--------|
| Session | `SessionStart`, `SessionEnd` |
| Prompt | `UserPromptSubmit` |
| Tools | `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionRequest` |
| Sub-agents | `SubagentStart`, `SubagentStop` |
| Flow | `Stop`, `StopFailure`, `TeammateIdle`, `TaskCompleted` |
| Config/worktree | `InstructionsLoaded`, `ConfigChange`, `WorktreeCreate`, `WorktreeRemove` |
| Compaction | `PreCompact`, `PostCompact` |
| Elicitation | `Elicitation`, `ElicitationResult` |
| Misc | `Notification` |

Hook injection is **idempotent** and **concurrency-safe**: an exclusive advisory lock prevents duplicate entries even if multiple slopd processes run simultaneously.

---

## Event system

Clients can subscribe to the live event stream with `slopctl listen`. Events are delivered as newline-delimited JSON objects, each with the shape:

```json
{
  "source": "hook",
  "event_type": "UserPromptSubmit",
  "pane_id": "%1",
  "session_id": "session-abc123",
  "payload": { ... }
}
```

Subscriptions can be filtered by any combination of `event_type`, `pane_id`, and `session_id`. Multiple filter objects are OR-ed; fields within a single filter object are AND-ed.

---

## Workspace layout

| Crate | Description |
|-------|-------------|
| `slopd` | Daemon binary — tmux management, RPC server, event broadcasting |
| `slopctl` | CLI client binary — all user-facing subcommands |
| `libslop` | Shared library — protocol types, config, hook injection, path helpers |
| `libsloptest` | Test helpers — isolated tmux environments for integration tests |
