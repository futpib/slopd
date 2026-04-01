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
- [Remote access (iroh)](#remote-access-iroh)
  - [iroh-slopd](#iroh-slopd)
  - [iroh-slopctl](#iroh-slopctl)
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
# Optional: remote access via iroh
cargo install --path iroh-slopd
cargo install --path iroh-slopctl
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

# Default working directory for every new Claude pane.
# Supports ~ and $VAR / ${VAR} expansion.
# Overridden per-session by `slopctl run --start-directory`.
# start_directory = "~/code/my-project"
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

### `slopctl ps [--filter KEY=VALUE] [--json]`

List all panes managed by slopd.

```
PANE  CREATED        LAST_ACTIVE    SESSION         PARENT  TAGS      STATE  DETAILED_STATE
%1    2 minutes ago  2 minutes ago  session-abc123  -       -         ready  ready
%2    5 seconds ago  5 seconds ago  -               %1      web,prod  busy   busy_tool_use
```

Filter by tag:

```bash
slopctl ps --filter tag=prod
```

Output as a JSON array (one object per pane) instead of the default table:

```bash
slopctl ps --json
```

### `slopctl run [-c DIR] [-- EXTRA_ARGS...]`

Open a new Claude pane in the slopd tmux session. Prints the new pane's ID on stdout.

```bash
PANE=$(slopctl run)
```

If called from within a tmux pane (i.e. `$TMUX_PANE` is set), the new pane automatically records that pane as its parent.

Use `-c` / `--start-directory` to set the working directory for this session, overriding the global `[run] start_directory` from config:

```bash
PANE=$(slopctl run -c ~/code/other-project)
PANE=$(slopctl run --start-directory ~/code/other-project)
```

### `slopctl kill <PANE_ID>`

Terminate a Claude pane.

```bash
slopctl kill %2
```

### `slopctl send <PANE_ID> <PROMPT> [--interrupt] [--timeout SECS]`
### `slopctl send <KEY=VALUE> <PROMPT> [--select one|any|all] [--interrupt] [--timeout SECS]`

Type `PROMPT` into a pane (or panes matching a filter) and wait until Claude acknowledges it via the `UserPromptSubmit` hook. Defaults to a 60-second timeout.

When the first positional argument contains `=`, it is treated as a filter instead of a pane ID.

```bash
# Send to a specific pane
slopctl send %1 "Summarize this file: README.md"
slopctl send %1 "Run the tests" --timeout 10

# Interrupt a busy pane first, then send a new prompt
slopctl send %1 "Cancel that — do this instead" --interrupt

# Send to all panes tagged "worker"
slopctl send tag=worker "Report your status" --select all

# Send to any one pane tagged "idle"
slopctl send tag=idle "Start task X" --select one
```

`--select` values (only used with filter target):

| Value | Behaviour |
|-------|-----------|
| `one` (default) | Exactly one matching pane must exist; error otherwise |
| `any` | Send to one arbitrarily chosen matching pane |
| `all` | Send to all matching panes |

`--interrupt` / `-i`: Send Ctrl+C, Ctrl+D, and Escape to the pane(s) before typing the prompt. Equivalent to running `slopctl interrupt` first.

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

### `slopctl listen [--hook EVENT] [--event EVENT] [--transcript TYPE] [--pane-id ID] [--session-id ID]`

Subscribe to the event stream and print events as JSON lines.

```bash
# All events
slopctl listen

# Only Stop hook events on a specific pane
slopctl listen --hook Stop --pane-id %1

# slopd state-change events only
slopctl listen --event StateChange

# Transcript records only (Claude conversation content)
slopctl listen --transcript user --transcript assistant

# Mix sources: hook Stop events and state changes for a pane
slopctl listen --hook Stop --event DetailedStateChange --pane-id %1
```

Flag summary:

| Flag | Source matched | Example values |
|------|---------------|----------------|
| `--hook EVENT` | `source:hook` | `Stop`, `UserPromptSubmit`, … |
| `--event EVENT` | `source:slopd` | `StateChange`, `DetailedStateChange` |
| `--transcript TYPE` | `source:transcript` | `user`, `assistant`, `progress` |

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

Clients can subscribe to the live event stream with `slopctl listen`. Events are delivered as newline-delimited JSON objects. There are three event sources:

### `source:hook` — Claude lifecycle hook events

```json
{
  "source": "hook",
  "event_type": "UserPromptSubmit",
  "pane_id": "%1",
  "payload": { ... }
}
```

`event_type` is the Claude hook name (e.g. `SessionStart`, `Stop`, `PreToolUse`). `payload` is the raw JSON object Claude passed to the hook.

### `source:slopd` — daemon state events

Emitted by slopd itself whenever a pane's state changes.

```json
{
  "source": "slopd",
  "event_type": "StateChange",
  "pane_id": "%1",
  "payload": {
    "state": "busy",
    "previous_state": "ready"
  }
}
```

```json
{
  "source": "slopd",
  "event_type": "DetailedStateChange",
  "pane_id": "%1",
  "payload": {
    "detailed_state": "busy_tool_use",
    "previous_detailed_state": "ready"
  }
}
```

`StateChange` fires when the coarse `state` transitions (`booting_up` → `ready` → `busy` → `awaiting_input`). `DetailedStateChange` fires on every hook event that updates the fine-grained state.

Detailed state values: `booting_up`, `ready`, `busy_processing`, `busy_tool_use`, `busy_subagent`, `busy_compacting`, `awaiting_input_permission`, `awaiting_input_elicitation`.

### `source:transcript` — Claude conversation transcript

slopd tails each pane's Claude transcript `.jsonl` file and re-broadcasts every record as an event. This lets subscribers read Claude's conversation in real time without polling the file system.

```json
{
  "source": "transcript",
  "event_type": "assistant",
  "pane_id": "%1",
  "payload": { ... }
}
```

`event_type` is the `type` field from the transcript record (e.g. `user`, `assistant`, `progress`, `queue-operation`).

### Filtering

Subscriptions can be filtered by any combination of `event_type`, `pane_id`, and `session_id`. Multiple filter objects are OR-ed; fields within a single filter object are AND-ed.

---

## Remote access (iroh)

`iroh-slopd` and `iroh-slopctl` provide remote access to a running slopd instance by exposing the Unix socket over the [iroh](https://github.com/n0-computer/iroh) peer-to-peer network. This lets you control slopd from another machine via an encrypted P2P connection with EndpointId allowlist authentication.

```
 [remote machine]               [local machine]
 iroh-slopctl ──── iroh ────► iroh-slopd ──► slopd.sock ──► slopd
```

`iroh-slopctl` supports all the same subcommands as `slopctl` (plus `info`). It is a drop-in remote replacement.

### iroh-slopd

`iroh-slopd` is a proxy that listens for iroh connections and forwards them to the local slopd Unix socket. Only clients whose EndpointId has been explicitly authorized are allowed to connect.

**Config file:** `~/.config/iroh-slopd/config.toml`

```toml
# Auto-generated on first run; do not edit manually.
secret_key = "..."

# List of authorized client EndpointIds (z-base-32 public keys).
authorized_clients = []
```

**Subcommands:**

| Command | Description |
|---------|-------------|
| `iroh-slopd` | Run the proxy server (default mode) |
| `iroh-slopd info` | Print this server's EndpointId |
| `iroh-slopd authorize <endpoint-id>` | Add a client EndpointId to the allowlist |
| `iroh-slopd revoke <endpoint-id>` | Remove a client EndpointId from the allowlist |

**Setup walkthrough:**

```bash
# 1. On the server — get the server's EndpointId
iroh-slopd info
# example output: abc123...

# 2. On the client — get the client's EndpointId
iroh-slopctl info
# example output: xyz789...

# 3. On the server — authorize the client
iroh-slopd authorize xyz789...

# 4. On the server — start the proxy
iroh-slopd
# iroh-slopd endpoint: abc123...
# iroh-slopd addr: {"node_id":"...","info":{...}}
```

Use `--addr-file PATH` to write the full `EndpointAddr` JSON to a file on startup. This is useful in scripts or tests that need to pass the address to a client without relying on discovery:

```bash
iroh-slopd --addr-file /tmp/iroh-addr.json
```

Verbosity can be increased with `-v` / `-vv` / `-vvv`.

### iroh-slopctl

`iroh-slopctl` is a remote slopctl that connects to `iroh-slopd` instead of a local Unix socket. It supports all the same commands as `slopctl` (see [slopctl commands](#slopctl-commands)) plus an `info` subcommand.

**Config file:** `~/.config/iroh-slopctl/config.toml`

```toml
# Auto-generated on first run; do not edit manually.
secret_key = "..."

# Default named endpoint to connect to when --endpoint is not given.
default = "my-server"

[endpoints.my-server]
endpoint_id = "abc123..."
```

**Connecting:**

```bash
# Connect by EndpointId (raw key)
iroh-slopctl --endpoint abc123... ps

# Connect by name defined in config
iroh-slopctl --endpoint my-server ps

# Connect using a full EndpointAddr JSON file (no discovery needed)
iroh-slopctl --addr-file /tmp/iroh-addr.json ps --json

# Use the default endpoint from config
iroh-slopctl ps
```

**Additional subcommand:**

| Command | Description |
|---------|-------------|
| `iroh-slopctl info` | Print this client's EndpointId (share with server for authorization) |

---

## Workspace layout

| Crate | Description |
|-------|-------------|
| `slopd` | Daemon binary — tmux management, RPC server, event broadcasting |
| `slopctl` | CLI client binary — all user-facing subcommands |
| `libslop` | Shared library — protocol types, config, hook injection, path helpers |
| `libslopctl` | Transport-agnostic client library — JSON-RPC protocol, typed methods, streaming |
| `iroh-slopd` | iroh proxy binary — exposes slopd over iroh with EndpointId allowlist auth |
| `iroh-slopctl` | iroh remote CLI binary — connects to iroh-slopd instead of a Unix socket |
| `libsloptest` | Test helpers — isolated tmux environments for integration tests |
