# slopd

[![Coverage Status](https://coveralls.io/repos/github/futpib/slopd/badge.svg?branch=master)](https://coveralls.io/github/futpib/slopd?branch=master)

A tmux-integrated daemon for managing [Claude](https://www.anthropic.com/claude) AI sessions programmatically.

`slopd` runs Claude inside a dedicated tmux session, exposes a Unix socket for control, and automatically injects lifecycle hooks into Claude's `settings.json` so that every prompt submit, tool use, stop event, and more is forwarded to the daemon in real time.

## Overview

```
Claude (in tmux pane)
  └─ fires hook → slopctl hook <Event>   (injected into ~/.claude/settings.json)
                       │
slopctl ───────────────┤──── run / send / kill / status / listen / …
                       │
              Unix socket ($XDG_RUNTIME_DIR/slopd/slopd.sock)
                       │
slopd ─────────────────┘
  ├─ manages tmux sessions / panes
  ├─ per-pane state and event fan-out
  └─ writes ~/.claude/settings.json (hook injection, atomic + locked)
```

## Installation

Prerequisites: **Rust** (stable) and **tmux**.

```sh
cargo install --git https://github.com/futpib/slopd slopd slopctl
```

Or build from source:

```sh
git clone https://github.com/futpib/slopd
cd slopd
cargo build --release
# binaries are at target/release/slopd and target/release/slopctl
```

### Systemd user service (optional)

A ready-made unit file is provided at `slopd.service`. Install it:

```sh
mkdir -p ~/.config/systemd/user
cp slopd.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now slopd
```

## Configuration

`slopd` reads `$XDG_CONFIG_HOME/slopd/config.toml` (usually `~/.config/slopd/config.toml`).  
All fields are optional; an empty or missing file uses sensible defaults.

```toml
[tmux]
# Path to a custom tmux server socket.
# Default: use the system tmux socket.
# socket = "/run/user/1000/slopd/tmux.sock"

# Whether to run `tmux start-server` on startup.
# Default: true when socket is not set, false otherwise.
# start_server = true

[run]
# Command used to launch Claude.
# Can be a string or an array for extra arguments.
# executable = "claude"
# executable = ["/usr/local/bin/claude", "--some-flag"]

# Path to the slopctl binary used when injecting hooks into settings.json.
# Default: "slopctl"  (looked up on $PATH)
# slopctl = "/home/user/.cargo/bin/slopctl"

# Override Claude's config directory (mirrors CLAUDE_CONFIG_DIR).
# Default: ~/.claude
# claude_config_dir = "/home/user/.claude"
```

`slopctl` reads `$XDG_CONFIG_HOME/slopctl/config.toml`. Currently no fields are defined.

## Usage

### Start the daemon

```sh
slopd
# or with increased verbosity:
slopd -v        # INFO
slopd -vv       # DEBUG
slopd -vvv      # TRACE
```

### slopctl commands

#### Show daemon status

```sh
slopctl status
```

#### List managed panes

```sh
slopctl ps
# filter by tag:
slopctl ps --filter tag=mytag
```

#### Open a new Claude pane

```sh
PANE_ID=$(slopctl run)
echo "Spawned pane: $PANE_ID"
```

#### Send a prompt and wait for completion

```sh
slopctl send %42 "Summarize this file: README.md"
# optionally override the confirmation timeout (default 60 s):
slopctl send %42 "Hello" --timeout 120
```

#### Send a prompt to all panes matching a filter

```sh
# one (default) — error if not exactly one match
slopctl send-filtered "Do the thing" --filter tag=worker

# all — send concurrently to every matching pane
slopctl send-filtered "Do the thing" --filter tag=worker --select all
```

#### Interrupt a running agent

Sends Ctrl+C, Ctrl+D, and Escape to the pane.

```sh
slopctl interrupt %42
```

#### Kill a pane

```sh
slopctl kill %42
```

#### Tags

```sh
slopctl tag   %42 worker   # add tag
slopctl untag %42 worker   # remove tag
slopctl tags  %42          # list tags
```

#### Subscribe to events

Streams newline-delimited JSON to stdout.

```sh
# all events:
slopctl listen

# filter by hook event name:
slopctl listen --hook UserPromptSubmit --hook Stop

# filter by pane:
slopctl listen --pane-id %42

# filter by Claude session ID:
slopctl listen --session-id <uuid>
```

#### Forward a hook event (called by Claude, not by humans)

```sh
slopctl hook UserPromptSubmit
```

This command is injected automatically into `~/.claude/settings.json` by `slopd run`.

## Hook injection

When `slopd run` spawns a new Claude pane it calls `inject_hooks_into_file` to add a hook entry for every Claude lifecycle event into `~/.claude/settings.json`.  The operation is idempotent (duplicate entries are never written), protected by an exclusive advisory lock, and written atomically via a temp-file rename.

Supported hook events: `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`, `PostToolUse`, `PostToolUseFailure`, `Notification`, `SubagentStart`, `SubagentStop`, `Stop`, `StopFailure`, `TeammateIdle`, `TaskCompleted`, `InstructionsLoaded`, `ConfigChange`, `WorktreeCreate`, `WorktreeRemove`, `PreCompact`, `PostCompact`, `Elicitation`, `ElicitationResult`, `SessionEnd`.

## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for a detailed description of the internal design, data flow, protocol, and known trade-offs.
