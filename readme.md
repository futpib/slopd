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
- [Backup and restore](#backup-and-restore)
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

### Config file location

By default slopd reads `$XDG_CONFIG_HOME/slopd/config.toml`. Point it at any file with `--config`:

```bash
slopd --config /path/to/other.toml
```

The path supports `~` and `$VAR` expansion. `slopctl --config <path>` reads the same file for the slopd settings it needs (the `[tmux]` socket/session used by `run --interactive`), so a single file can configure both. SIGHUP reloads from the `--config` path too. All four binaries (`slopd`, `slopctl`, `iroh-slopd`, `iroh-slopctl`) accept `--config`.

#### Running a second instance

`--config` is what lets a second slopd run alongside the first without touching it. Give the second instance its own **tmux** socket/session (via `[tmux] socket` / `session`) and its own **control** socket. The control socket lives at `$XDG_RUNTIME_DIR/slopd/slopd.sock` — it is *not* read from the config file — so isolate it by pointing `XDG_RUNTIME_DIR` somewhere else for both the daemon and the `slopctl` commands that talk to it:

```bash
# second daemon (own config + own control socket)
XDG_RUNTIME_DIR=/run/user/1000/slopd-b slopd --config ~/.config/slopd/b.toml

# talk to it
XDG_RUNTIME_DIR=/run/user/1000/slopd-b slopctl ps
```

With a custom `[tmux] socket`, set `[tmux] start_server = true` if no tmux server is already listening on it.

### Reloading config

`SIGHUP` re-reads `config.toml` without restarting:

```bash
kill -HUP $(pgrep -x slopd)
# or, when running under systemd:
systemctl --user reload slopd
```

The reload affects subsequent operations only — already-running Claude panes keep the executable, env, and `claude_config_dir` they were spawned with. Verbosity / log level cannot be changed at reload (re-start to apply). A malformed `config.toml` keeps the previous config; check the daemon log for the parse error. `slopctl status` exposes a `config_generation` counter that bumps on each successful reload.

---

## Configuration

### slopd config

File: `~/.config/slopd/config.toml`

All defaults are fine for most setups. The only key you are likely to want to set is `claude_config_dir` if Claude's config lives somewhere other than `~/.claude`.

```toml
# Claude config dir (mirrors CLAUDE_CONFIG_DIR; default: ~/.claude). This sets
# the dir for the reserved "default" account — the one used when no account is
# selected. Equivalent to writing [accounts.default].
# Supports ~ and $VAR / ${VAR} expansion (as do all account config dirs).
# claude_config_dir = "~/.claude"

# Account used by `slopctl run` when no --account is given and none is inherited
# from the current pane. Omit to fall back to the "default" account above.
# default_account = "work"

# Named accounts: each maps an account name to its own config (at minimum a
# Claude config dir). Pick one per pane with `slopctl run --account <name>`.
# Both forms below are accepted; the table form leaves room for future
# per-account options.
# [accounts]
# work = "~/.config/claude-work"            # shorthand: just the dir
# [accounts.personal]
# claude_config_dir = "~/.config/claude-personal"

# [tmux]
# Path to a custom tmux socket. When omitted slopd uses its default server.
# Supports ~ and $VAR / ${VAR} expansion.
# socket = "/run/user/1000/tmux-slopd.sock"
# Name of the tmux session slopd manages (default: "slopd"). Mainly useful for
# running more than one slopd instance against the same tmux server.
# session = "slopd"

# [run]
# Command used to launch Claude. Can be a string or an array.
# executable = "claude"
# executable = ["claude", "--dangerously-skip-permissions", "--model", "sonnet", "--effort", "max", "--thinking-display", "summarized"]

# Path to the slopctl binary injected into Claude hooks.
# slopctl = "slopctl"

# Default working directory for every new Claude pane.
# Supports ~ and $VAR / ${VAR} expansion.
# Overridden per-session by `slopctl run --start-directory`.
# start_directory = "~/code/my-project"

# Extra environment variables for every new Claude pane.
# Values support $VAR / ${VAR} expansion against slopd's environment.
# [run.env]
# FOO = "bar"
# TOKEN = "${MY_TOKEN}"

# Paths to dotenv-style files loaded for every new Claude pane.
# Paths support ~ and $VAR expansion. Files loaded in order; later entries win.
# CLI `--env` / `--env-file` override these.
# env_files = ["~/.config/slopd/pane.env"]

# Auto-continue a turn that ends with StopFailure (e.g. an API 500): slopd
# injects a "continue" prompt after an exponential backoff so an unattended pane
# recovers on its own instead of stalling until you nudge it (default: true).
# auto_continue_on_failure = true
# Give up after this many consecutive failed retries, then leave the pane idle
# (default: 8 — with the defaults below, ~4m15s of retrying).
# max_retry_attempts = 8
# Delay before the first retry, in milliseconds; doubles each subsequent retry
# (default: 1000).
# initial_backoff_ms = 1000
# Optional ceiling (milliseconds) on the backoff delay. Unset means the delay
# keeps doubling uncapped (1s, 2s, 4s, …); set it to flatten the tail into steady
# polling once the delay reaches this value (default: unset).
# max_backoff_ms = 30000

# [backup]
# Back up the managed-pane set to disk and restore it after a reboot (see
# "Backup and restore"). The two automatic behaviours are independent; manual
# `slopctl backup` / `slopctl restore` work regardless of them.
# Automatically write the manifest on a timer and on clean shutdown (default: true).
# auto_backup = true
# Automatically re-spawn the recorded panes after a reboot (default: false, so a
# reboot does not resurrect panes unless you ask).
# auto_restore = false
# Manifest path (default: $XDG_STATE_HOME/slopd/panes.json). Supports ~ / $VAR.
# path = "~/.local/state/slopd/panes.json"
# How often (seconds) to auto-back-up while running (default: 30). A backup is
# also taken on clean shutdown regardless of this interval.
# interval_secs = 30
```

#### Multiple accounts

Run different panes under different Claude config dirs. There is always a
reserved account named `default`, backed by the top-level `claude_config_dir`
(or `~/.claude`); define additional ones under `[accounts]`, each mapping a name
to its own config (at minimum a config dir). The table form
(`[accounts.<name>]`) is extensible — future per-account options live there.

Launch a pane under a specific account with `slopctl run --account <name>`:
slopd points the pane at the account's config dir (exporting `CLAUDE_CONFIG_DIR`)
and injects its hooks there. Every managed pane carries its account, shown in
the `ACCOUNT` column of `slopctl ps`.

slopd records the account on the pane itself, so a pane that spawns more panes
with `slopctl run` passes its own account down by default — the child inherits
it from the parent pane, no need to repeat `--account` (and no extra environment
variable to manage). Resolution order for each `run`:

1. an explicit `--account <name>` flag;
2. otherwise the account inherited from the current pane;
3. otherwise slopd's `default_account`;
4. otherwise the `default` account (`claude_config_dir`, or Claude's `~/.claude`).

An unknown account name fails the `run` with an error listing the configured
accounts, before any pane is spawned. Account config dirs support `~` and
`$VAR` / `${VAR}` expansion.

### slopctl config

File: `~/.config/slopctl/config.toml`

Only used by `slopctl run --interactive` (see below):

```toml
# [run]
# Command run by `slopctl run --interactive` once the new pane exists. These
# placeholders are substituted in each argument:
#   {{pane_id}}  the new pane id
#   {{socket}}   slopd's [tmux] socket (empty when the default socket is used)
#   {{session}}  slopd's tmux session name ("slopd")
# When unset, the default attaches an *isolated grouped view* of slopd's tmux
# session and focuses the new pane, so other clients watching the session aren't
# moved (honoring slopd's [tmux] socket):
#   tmux [-S <socket>] new-session -t <session> ';' set destroy-unattached on ';' select-window -t {{pane_id}}
# interactive_command = ["tmux", "attach", "-t", "{{session}}"]   # simpler: shared view
#
# How to run it (a subset of systemd's Type=):
#   "exec"    (default) replace the slopctl process with the command
#   "forking" run it detached in the background; slopctl prints the pane id and exits
# interactive_type = "exec"
```

The default command picks up slopd's `[tmux] socket` and `[tmux] session` automatically. It uses a *grouped session* — which shares the slopd session's windows but keeps its own current window — so focusing the new pane doesn't pull other clients off what they're viewing; `destroy-unattached on` makes that throwaway view clean itself up on detach. `{{session}}` lets custom commands stay symbolic rather than hardcoding the session name.

---

## Backup and restore

slopd keeps each pane's identity — Claude session id, account, tags, ancestry, working directory — in tmux pane options, and rebuilds its in-memory state from them whenever the daemon restarts. That makes a daemon restart transparent: the Claude processes keep running in tmux and slopd re-adopts them.

A **reboot** is the one case that breaks: it destroys the whole tmux server, taking those pane options *and* the Claude processes with it. The conversations themselves are safe — Claude writes each session to a transcript on disk and `claude --resume <id>` continues it — but slopd's record of *which* sessions were running, and how, is gone with tmux. Backup/restore closes that gap by keeping a copy on durable storage.

Backup and restore each have an **automatic** toggle plus an always-available **manual** command, and the two automatic toggles are independent — all four combinations are valid:

| | `auto_restore = false` (default) | `auto_restore = true` |
|---|---|---|
| **`auto_backup = true`** (default) | back up automatically; restore only on demand | full reboot survival |
| **`auto_backup = false`** | drive both by hand | restore on reboot from manifests you write by hand |

**Backup.** With `auto_backup` on (the default), slopd writes the managed-pane set to a JSON manifest (default `$XDG_STATE_HOME/slopd/panes.json`) every `[backup] interval_secs` seconds and once more on clean shutdown; `slopctl backup` writes it on demand at any time. Writes are atomic (temp file + rename), so a crash mid-write never corrupts the manifest, and only panes that have recorded a Claude session id — the ones that can actually be resumed — are written. The manifest lives in the XDG **state** dir, which survives reboot, unlike the runtime dir that holds the control socket.

**Restore.** With `auto_restore` on, slopd restores when it finds it had to create its tmux session from scratch — the signature of a fresh server after a reboot: it re-spawns each recorded pane with `claude --resume <session_id>` in its original working directory and account, restoring tags and parent/child ancestry (remapped to the new tmux pane ids). On an ordinary daemon restart the tmux session still exists, so slopd recovers panes from tmux as usual and does not touch the manifest — panes are never duplicated. `slopctl restore` does the same on demand; it is safe on a live daemon because it skips any session that is already running.

**Pending restore (with `auto_restore` off).** When slopd starts into a fresh session and `auto_restore` is off, it does *not* resurrect the panes — but it does mark the manifest as a **pending restore**: the count shows in `slopctl status`, and **auto-backup is suspended** so the restore point is preserved. This matters because otherwise auto-backup would immediately overwrite the manifest with the empty (or, once you start working, diverged) post-reboot pane set, destroying it before you could use it. With the pending hold, you can reboot, start working, and run `slopctl restore` whenever you remember — the pre-reboot set is still there. The pending state is recorded by a `<manifest>.pending` marker file, so it survives a *daemon* restart too (a crash, `systemctl restart`, or package update in the pending window won't resume auto-backup and clobber the restore point). It is resolved by `slopctl restore` (bring the panes back) or `slopctl backup` (replace the restore point with the current state); either one removes the marker and resumes normal auto-backup.

Restore never starts two Claude processes on one session: it skips a session id that is already running *or* that it has already restored in this pass (a manifest can legitimately contain the same session id twice). It is otherwise best-effort — a pane whose session can no longer be resumed (e.g. its transcript was deleted) simply fails to come up and is reconciled away, without affecting the others.

Because plain `--resume` continues the *same* session id and appends to the *same* transcript, a restored pane keeps its identity and the manifest stays valid across repeated reboots. The default (`auto_backup = true`, `auto_restore = false`) keeps a current backup but does not resurrect panes on reboot unless you opt in or run `slopctl restore`.

---

## slopctl commands

All commands communicate with the running `slopd` daemon over its Unix socket.

### `slopctl status`

Print daemon uptime and state.

```
uptime: 5025s
subscribers: 3
config_generation: 1
```

After a reboot with `auto_restore` off, a `pending_restore` line appears while panes from the previous session await a `slopctl restore` (see [Backup and restore](#backup-and-restore)):

```
uptime: 12s
subscribers: 0
config_generation: 0
pending_restore: 7 pane(s) — run `slopctl restore`
```

### `slopctl ps [--filter KEY=VALUE] [--json]`

List all panes managed by slopd.

```
PANE  CREATED        LAST_ACTIVE    SESSION         PARENT  TAGS      STATE  DETAILED_STATE  WORKING_DIR
%1    2 minutes ago  2 minutes ago  session-abc123  -       -         ready  ready           ~/code/project
%2    5 seconds ago  5 seconds ago  -               %1      web,prod  busy   busy_tool_use   -
```

Filter by tag:

```bash
slopctl ps --filter tag=prod
```

Output as a JSON array (one object per pane) instead of the default table:

```bash
slopctl ps --json
```

### `slopctl run [--no-wait] [-i] [--ready-timeout SECS] [-a NAME] [-c DIR] [-e KEY=VALUE]... [--env-file PATH]... [-- EXTRA_ARGS...]`

Open a new Claude pane in the slopd tmux session. Prints the new pane's ID on stdout. The pane's window is created in the **background** (`tmux new-window -d`), so spawning a pane never yanks clients already watching the session to it — use `--interactive` (below) when you do want to land on the new pane.

```bash
PANE=$(slopctl run)
```

By default `run` waits for the new pane to become ready before returning, so a pane that dies during startup is reported as a failure instead of a dangling pane ID:

- The pane becomes ready and stays alive → exit 0 and print the pane ID (as above).
- The pane dies before becoming ready (e.g. `claude --resume <bad-id>` exits right after launch) → non-zero exit and an error on stderr, including the session-end reason when available. No pane ID is printed.
- The pane doesn't become ready within `--ready-timeout` seconds (default 30) → non-zero exit and a timeout message, but the pane ID is still printed so you can investigate.

Pass `--no-wait` to restore the historical fire-and-forget behaviour (return as soon as the pane is created):

```bash
PANE=$(slopctl run --no-wait)
```

If called from within a tmux pane (i.e. `$TMUX_PANE` is set), the new pane automatically records that pane as its parent.

Use `-a` / `--account NAME` to launch the pane under a named account from `[accounts]` (or the reserved `default`; see [Multiple accounts](#multiple-accounts)). Without the flag, the account is inherited from the current pane, then slopd's `default_account`:

```bash
PANE=$(slopctl run --account work)
```

Use `-c` / `--start-directory` to set the working directory for this session, overriding the global `[run] start_directory` from config:

```bash
PANE=$(slopctl run -c ~/code/other-project)
PANE=$(slopctl run --start-directory ~/code/other-project)
```

Use `-e` / `--env KEY=VALUE` (repeatable) to add environment variables to the new pane. Values support `$VAR` / `${VAR}` expansion against slopctl's environment; a missing variable is an error:

```bash
PANE=$(slopctl run --env FOO=bar --env TOKEN=${MY_TOKEN})
```

Use `--env-file PATH` (repeatable) to load environment variables from a dotenv-style file (`KEY=VALUE` per line, `#` comments, blank lines ignored). Files are loaded in the order given; later files and `--env` flags override earlier ones, and CLI flags override `[run.env]` / `[run.env_files]` from config:

```bash
PANE=$(slopctl run --env-file ~/.config/slopd/pane.env --env DEBUG=1)
```

Use `-i` / `--interactive` to drop straight into the new pane instead of waiting for it to become ready. As soon as the pane exists, slopctl runs the command from `[run] interactive_command` in [slopctl config](#slopctl-config) — by default it attaches an *isolated grouped view* of the session focused on the new pane (so other clients aren't moved) — with the `{{pane_id}}`, `{{socket}}`, and `{{session}}` placeholders substituted:

```bash
slopctl run --interactive        # tmux attach into the slopd session, on the new pane
```

By default this `exec`s — slopctl is replaced by the command, so e.g. `tmux attach` takes over the terminal. Set `interactive_type = "forking"` to instead launch the command detached in the background (slopctl prints the pane id and returns), e.g. to pop the pane open in a new terminal window:

```toml
[run]
interactive_command = ["kitty", "tmux", "attach", "-t", "{{session}}"]
interactive_type = "forking"
```

`--interactive` is a local-slopctl feature (it attaches to slopd's tmux); `iroh-slopctl run --interactive` errors.

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

# Compact the context of all running agents to reclaim context window
slopctl send tag=worker "/compact" --select all

# Reset a specific agent's conversation history entirely
slopctl send %1 "/clear"
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

### `slopctl backup`

Write the backup manifest now, regardless of the `[backup] auto_backup` setting. Prints how many panes were recorded. See [Backup and restore](#backup-and-restore).

```bash
slopctl backup
# backed up 3 pane(s)
```

### `slopctl restore`

Re-spawn panes from the backup manifest now, regardless of `[backup] auto_restore`. Sessions that are already running are skipped, so this is safe to run against a live daemon (e.g. to pull back panes that died, or to restore after a reboot when `auto_restore` is off). Prints how many panes were re-spawned.

```bash
slopctl restore
# restored 2 pane(s)
```

### `slopctl hook <EVENT>`

Forward a Claude lifecycle hook event to slopd. Reads the JSON payload from stdin. Normally called automatically from Claude's settings hooks — you do not need to invoke this manually.

```bash
echo '{"session_id":"abc"}' | slopctl hook SessionStart
```

### `slopctl tmux-hook <EVENT> [PANE_ID]`

Forward a tmux hook event to slopd. Normally called automatically from tmux hooks registered by the daemon — you do not need to invoke this manually.

```bash
slopctl tmux-hook after-kill-pane
```

### `slopctl listen [--hook EVENT] [--event EVENT] [--transcript TYPE] [--pane-id ID] [--session-id ID] [--where KEY=VALUE] [--replay N]`

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

# Server-side payload filter: only assistant messages with a text block
slopctl listen --transcript assistant --where 'message.content[].type=text'

# Replay the last 20 transcript records then stream live events
slopctl listen --transcript user --transcript assistant --pane-id %1 --replay 20
```

Flag summary:

| Flag | Source matched | Example values |
|------|---------------|----------------|
| `--hook EVENT` | `source:hook` | `Stop`, `UserPromptSubmit`, … |
| `--event EVENT` | `source:slopd` | `StateChange`, `DetailedStateChange` |
| `--transcript TYPE` | `source:transcript` | `user`, `assistant`, `progress` |

`--replay N`: Replay the last N transcript records from the pane's history before switching to live events. Requires `--pane-id`.

`--where KEY=VALUE` (repeatable, AND): server-side payload predicate. KEY is a [jq-style path](#payload-paths) into the event's `payload`; non-matching events are not delivered. Incompatible with `--replay`.

### `slopctl wait [--hook EVENT] [--event EVENT] [--transcript TYPE] [--pane-id ID] [--session-id ID] [--where KEY=VALUE] [--until KEY=VALUE] [--timeout SECS] [--no-snapshot]`

One-shot version of `listen`: same filter surface and same output (the `{"subscribed":true}` confirmation followed by each record as a JSON line). Exits 0 after printing the first matching event, or non-zero on timeout.

```bash
# Wait until pane reaches the ready state
slopctl wait --event DetailedStateChange --pane-id %1 --until detailed_state=ready

# Wait for the next UserPromptSubmit on a tagged pane (60s default timeout)
slopctl wait --hook UserPromptSubmit --pane-id %1

# Wait for an assistant message that contains a text block
slopctl wait --pane-id %1 --transcript assistant --until 'message.content[].type=text'

# Wait for the next transition only (skip pre-wait snapshot of current state)
slopctl wait --event DetailedStateChange --pane-id %1 --until detailed_state=ready --no-snapshot
```

`--until KEY=VALUE` (repeatable, AND): client-side stop predicate. KEY is a [jq-style path](#payload-paths). Without `--until`, any event matching the filters wins.

`--where KEY=VALUE` (repeatable, AND): server-side payload predicate, same syntax as `--until` (see `listen`). Use `--where` when the listener is expensive or the predicate is selective; use `--until` when you want to see every event but stop on a specific one.

`--timeout SECS`: default 60. Pass `0` to wait indefinitely.

`--no-snapshot`: Skip the pre-wait pane-state snapshot. By default `wait` checks the pane's current state and exits immediately if it already satisfies the predicates (emitting a synthetic `CurrentState` record). Use `--no-snapshot` when you want to wait for the next transition specifically, ignoring whatever state the pane is in right now.

#### Payload paths

`--where` and `--until` accept a jq-style path on the left side of `KEY=VALUE`. Supported syntax:

| Form | Meaning |
|------|---------|
| `foo` or `.foo` | Object key (leading `.` is optional) |
| `foo.bar` | Nested object access |
| `foo[]` | Any element of an array (succeeds if any element matches the rest of the path) |
| `foo[3]` | Specific array index |
| `messages[].content[].type` | Combined: any message, any content block, type field |

Comparison is string-equal against the reachable scalar (`null`, `true`, numbers compared as their JSON form). Arrays and objects never match a scalar value. A missing path is not a match.

### `slopctl transcript <PANE_ID> [--limit N] [--before CURSOR]`

Read historical transcript records from a pane. Returns records as a JSON object with a `records` array.

```bash
# Last 50 records (default)
slopctl transcript %1

# Last 10 records
slopctl transcript %1 --limit 10

# Records before a specific byte-offset cursor (for pagination)
slopctl transcript %1 --before 4096
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

### `slopctl tags [PANE_ID]`

List all tags on a pane. `PANE_ID` defaults to `$TMUX_PANE` if omitted.

```bash
slopctl tags %1
# prod
# web
```

---

## Multi-backend support (OpenCode)

slopd can drive either [Claude Code](https://claude.com/claude-code) or [OpenCode](https://opencode.ai) panes. Each pane's backend is selected by its account (default `claude`).

OpenCode runs its TUI as a client of an **embedded HTTP server**, so slopd drives an opencode pane over that API — no Claude-style hooks, no transcript file tailing. slopd spawns the pane with a pinned `--port` on `127.0.0.1` (plus a per-pane auth token), then polls `GET /session/status` to track state, `POST /prompt_async` to send, `POST /abort` to interrupt, and `GET /message` for transcripts. opencode's signals normalize onto slopd's existing state machine, so the daemon core stays agent-agnostic.

### Configuring an OpenCode account

```toml
[accounts.oc]
backend = "opencode"                      # selects the opencode backend
claude_config_dir = "~/.config/opencode"  # agent config dir (exported as OPENCODE_CONFIG_DIR)
```

```bash
slopctl run --account oc
```

The `backend` and the executable resolve bidirectionally ("each implies the other"):

- `backend = "opencode"` alone → spawns `opencode` (its canonical binary).
- `executable = "opencode"` (no `backend`) → infers the opencode backend.
- `backend = "claude"` + `executable = "opencode"` → **error** (contradiction).
- `executable = "/path/to/my-opencode-fork"` (unrecognized name) → treated as an executable override under the configured `backend` (default `claude`); set `backend = "opencode"` explicitly to drive a fork.

Named accounts do **not** inherit the top-level `backend` (mirroring `claude_config_dir`); set it on each `[accounts.<name>]`.

### What works identically

`send`, `interrupt`, `listen`, `wait`, `transcript`, `tag`, `kill`, `ps`, and the iroh remote path are all agent-agnostic. An opencode pane's state (`ready` / `busy_processing` / …) means the same as a Claude pane's. `send` is in fact more reliable for opencode (HTTP POST, not keystroke-the-TUI-and-hope).

### Current limitations

- **State fidelity**: opencode's `/session/status` is mapped onto slopd's states best-effort; some granular Claude states (e.g. `busy_subagent`, `awaiting_input_elicitation`) may collapse to the nearest equivalent until finer opencode signals are confirmed against a real server.
- **Live transcript streaming**: `slopctl transcript` (pull) works; live `listen --transcript` streaming for opencode is pending (SSE `GET /event`).

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

### Auto-continue on failure

When a turn ends with `StopFailure` (e.g. Claude hit an API 500), it would otherwise just stop and wait for a human to resume it — bad for an unattended pane. slopd instead recovers on its own: it sends a `continue` prompt after an exponential backoff, retrying until the turn completes or the attempt cap is reached. The pane stays in the `ready` state throughout; the retry counter lives in per-pane metadata.

The resend is **edge-triggered** by `StopFailure` (the end of a turn), not a periodic timer — so a `continue` that kicks off a long-running turn never provokes a second one while that turn is still going. Retrying stops as soon as any of these happens:

- the turn finally succeeds (a clean `Stop`) — the counter resets, ready for a future failure;
- `max_retry_attempts` consecutive failures are reached — slopd gives up and leaves the pane idle;
- you submit a prompt yourself — taking over cancels any pending retry.

All of this is configurable (or disabled) under `[run]` — see `auto_continue_on_failure`, `max_retry_attempts`, `initial_backoff_ms`, and `max_backoff_ms` in [slopd config](#slopd-config). With the defaults (8 attempts, 1s backoff doubling uncapped) a persistently-failing turn is retried over ~4m15s before slopd stops.

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
