# slopd

[![Coverage Status](https://coveralls.io/repos/github/futpib/slopd/badge.svg?branch=master)](https://coveralls.io/github/futpib/slopd?branch=master)

**slopd** is an agent session manager daemon for Claude Code, OpenCode, and Codex. It runs interactive sessions inside [tmux](https://github.com/tmux/tmux) panes, exposes a Unix socket RPC API for controlling them, and streams normalized lifecycle and transcript events to subscribers.

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
- [Multi-backend support](#multi-backend-support-opencode-and-codex)
- [Claude hook integration](#claude-hook-integration)
- [Event system](#event-system)
- [ACP adapter](#acp-adapter)
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
│  │  claude …   │  │ opencode …  │  │   codex …   │   │
│  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘   │
│         │ hooks / transcript / local API │            │
└─────────┼────────────────┼────────────────┼────────────┘
          │                │                │
          └────────────────▼────────────────┘
                     slopd (daemon)
                  $XDG_RUNTIME_DIR/slopd/slopd.sock
                           │
                    slopctl (CLI client)
```

- **slopd** listens on a Unix domain socket and accepts JSON-RPC requests.
- Each agent process runs in its own pane inside a dedicated `slopd` tmux session.
- Claude Code and Codex report lifecycle events through injected hooks; OpenCode
  is observed and controlled through the local API embedded in each TUI process.
- Clients see one normalized state, lifecycle, and transcript event stream across
  all three backends.

---

## Requirements

- **Rust** (2024 edition) — to build from source
- **tmux** — must be in `PATH`
- At least one supported agent CLI — `claude`, `opencode`, or `codex` — on
  slopd's `PATH`, unless configured with an explicit executable path

The control socket normally lives under `$XDG_RUNTIME_DIR`. If that variable is
unset, all local slopd tools consistently fall back to an existing
`/run/user/<uid>` or a private `0700` directory under the system temp directory.

---

## Installation

On Arch Linux, install the [`slopd-git`](https://aur.archlinux.org/packages/slopd-git)
package from the AUR:

```bash
yay -S slopd-git
```

The package installs `slopd`, `slopctl`, `iroh-slopd`, `iroh-slopctl`, and the
systemd user service. Install from source if you also need `slopd-acp`.

To install from source:

```bash
cargo install --path slopd
cargo install --path slopctl
# Optional: expose managed panes to ACP clients
cargo install --path slopd-acp
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
2. Start listening on the configured `[control] socket`, the path passed with
   `--socket`, or the default control socket
   (`$XDG_RUNTIME_DIR/slopd/slopd.sock`, subject to the fallback described
   above). The CLI flag has highest precedence.
3. Inject `slopctl hook <event>` entries for configured Claude Code and Codex
   accounts as panes are launched. OpenCode panes use their embedded local API
   instead of hook files.

Verbosity can be increased with `-v` / `-vv` / `-vvv` (INFO / DEBUG / TRACE)
or top-level `verbose` in the config; `RUST_LOG` is also honored. A one-off
`--executable PROGRAM [ARGS]...` overrides `[run] executable`.

slopd removes its injected hook entries on clean shutdown. To remove them
without starting the daemon, while preserving hooks owned by other tools:

```bash
slopd uninject-hooks
```

### Config file location

By default slopd reads `$XDG_CONFIG_HOME/slopd/config.toml`. Point it at any file with `--config`:

```bash
slopd --config /path/to/other.toml
```

The path supports `~` and `$VAR` expansion. `slopctl --config <path>` reads the
same file for the slopd settings it needs (the control socket and the `[tmux]`
socket/session used by `run --interactive`), so a single file can configure
both. SIGHUP reloads from the `--config` path too. `slopd`, `slopctl`,
`iroh-slopd`, and `iroh-slopctl` all accept a `--config` override for their
respective config.

#### Running a second instance

Give each instance a distinct tmux socket/session and control socket in its
config:

```toml
# ~/.config/slopd/b.toml
[control]
socket = "/run/user/1000/slopd-b.sock"

[tmux]
session = "slopd-b"
socket = "/run/user/1000/slopd-b-tmux.sock"
start_server = true
```

```bash
slopd --config ~/.config/slopd/b.toml
slopctl --config ~/.config/slopd/b.toml ps
```

`--socket PATH` remains available on slopd and its local clients as a
higher-precedence one-off override. The daemon automatically includes the
effective control socket in injected hook commands. `iroh-slopd` can likewise
read `[control] socket` from its own config or override it with `--socket`.

### Reloading config

`SIGHUP` re-reads `config.toml` without restarting:

```bash
kill -HUP $(pgrep -x slopd)
# or, when running under systemd:
systemctl --user reload slopd
```

The reload affects subsequent operations only — already-running panes keep the
backend, executable, environment, and config directory they were spawned with.
Logging, `[control] socket`, and the startup-resolved `[backup]` toggles and
interval require a restart. A malformed config keeps the
previous generation; check the daemon log for the parse error. `slopctl status`
exposes a `config_generation` counter that increments after every successful
reload.

If a reload changes an account's hook-file location, entries in the old location
may remain. Clean them with `slopd --config <old-config> uninject-hooks`.

---

## Configuration

### slopd config

File: `~/.config/slopd/config.toml`

All fields are optional. With an empty config, slopd uses the `claude` backend,
the agent's standard config location, the default tmux server, and a tmux
session named `slopd`.

```toml
# Log verbosity: 0 = WARN (default), 1 = INFO, 2 = DEBUG, 3 = TRACE.
# CLI -v flags also control verbosity; RUST_LOG is honored.
# verbose = 0

# Backend and config dir for the reserved "default" account.
# backend = "claude" # "claude", "opencode", or "codex"
# config_dir = "~/.claude"
#
# config_dir is exported using the selected backend's variable:
# CLAUDE_CONFIG_DIR, OPENCODE_CONFIG_DIR, or CODEX_HOME. When omitted, the
# backend uses its own standard location.
# Supports ~ and $VAR / ${VAR} expansion (as do all account config dirs).

# Pointer to the named account used by `slopctl run` when no --account is given
# and none is inherited from the current pane. Switching the default then only
# requires changing this one value. Omit it to use the reserved "default"
# account above.
# default_account = "work"

# Local RPC endpoint. `--socket PATH` overrides this for both slopd and
# slopctl. Changes require a daemon restart.
# [control]
# socket = "/run/user/1000/slopd.sock"

# Named accounts. A bare string is shorthand for a config directory. The table
# form can set config_dir, backend, and an account-specific executable.
# [accounts]
# work = "~/.config/claude-work"            # shorthand: just the dir
# [accounts.personal]
# config_dir = "~/.config/claude-personal"
# backend = "claude"
# executable = ["claude", "--model", "sonnet"]
#
# [accounts.codex]
# backend = "codex"                         # config_dir may be omitted
# executable = "codex"                     # overrides a global Claude executable

# [tmux]
# Path to a custom tmux socket. When omitted slopd uses its default server.
# Supports ~ and $VAR / ${VAR} expansion.
# socket = "/run/user/1000/tmux-slopd.sock"
# Name of the tmux session slopd manages (default: "slopd"). Mainly useful for
# running more than one slopd instance against the same tmux server.
# session = "slopd"
# Run `tmux start-server` during startup. Defaults to true for the default tmux
# server and false when a custom socket is configured.
# start_server = true

# [run]
# Default agent command. A canonical executable name also selects its backend;
# an account's backend/executable can override it. Can be a string or an array.
# executable = "claude"
# executable = ["claude", "--dangerously-skip-permissions", "--model", "sonnet", "--effort", "max", "--thinking-display", "summarized"]

# Path to the slopctl binary injected into Claude Code and Codex hooks.
# slopctl = "slopctl"

# Default working directory for every new agent pane.
# Supports ~ and $VAR / ${VAR} expansion.
# Overridden per-session by `slopctl run --start-directory`.
# start_directory = "~/code/my-project"

# Extra environment variables for every new agent pane.
# Values support $VAR / ${VAR} expansion against slopd's environment.
# [run.env]
# FOO = "bar"
# TOKEN = "${MY_TOKEN}"

# Paths to dotenv-style files loaded for every new agent pane.
# Paths support ~ and $VAR expansion. Files loaded in order; later entries win.
# CLI `--env` / `--env-file` override these.
# env_files = ["~/.config/slopd/pane.env"]

# Retry a failed Claude Code or OpenCode turn after exponential backoff so an
# unattended pane does not stall (default: true). Claude receives "continue";
# OpenCode re-submits the failed prompt. Codex does not currently expose the
# failure event needed for this policy.
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
# Automatically checkpoint the live pane set on a timer and shutdown (default: true).
# auto_backup = true
# Automatically re-spawn the recorded panes after a reboot (default: false, so a
# reboot does not resurrect panes unless you ask).
# auto_restore = false
# How often (seconds) to auto-back-up while running (default: 30). A backup is
# also taken on clean shutdown regardless of this interval.
# interval_secs = 30
```

#### Multiple accounts

Run different panes under different agent accounts and backends. There is always
a reserved account named `default`, backed by the top-level `backend`,
`config_dir`, and `[run] executable`. Define additional entries under
`[accounts]`; the table form accepts `backend`, `config_dir`, and `executable`.

`default_account` is a pointer to one of those named accounts. Keeping every
backend in a named account makes changing the default a one-line edit:

```toml
default_account = "codex" # change only this line to "claude" or "opencode"

[accounts.claude]

[accounts.codex]
backend = "codex"

[accounts.opencode]
backend = "opencode"
```

The target account name—not the literal name `default`—is recorded on newly
created panes, so their children continue to inherit the account they were
launched with even if `default_account` changes later.

Launch a pane under a specific account with `slopctl run --account <name>`:
when `config_dir` is set, slopd exports it through the backend's config
environment variable (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, or
`OPENCODE_CONFIG_DIR`). When omitted, the variable is left unset and the backend
uses its standard location. Every managed pane carries its account, shown in the
`ACCOUNT` column of `slopctl ps`.

slopd records the account on the pane itself, so a pane that spawns more panes
with `slopctl run` passes its own account down by default — the child inherits
it from the parent pane, no need to repeat `--account` (and no extra environment
variable to manage). Resolution order for each `run`:

1. an explicit `--account <name>` flag;
2. otherwise the account inherited from the current pane;
3. otherwise slopd's `default_account`;
4. otherwise the `default` account (`config_dir`, or Claude's `~/.claude`).

An unknown account name fails the `run` with an error listing the configured
accounts, before any pane is spawned. Account config dirs support `~` and
`$VAR` / `${VAR}` expansion.

Backend and executable resolve together:

- an explicit `backend` defaults to that backend's canonical executable;
- a canonical executable (`claude`, `opencode`, or `codex`) selects its matching
  backend when `backend` is omitted;
- contradictory canonical values are rejected;
- a custom executable path does not imply a backend, so pair it with an explicit
  `backend`.

Named accounts do not inherit the top-level `backend` or `config_dir`, but they
do fall back to `[run] executable` when they do not define their own. At run
time, `slopctl run --backend KIND` can override the resolved account backend;
the pane still records and inherits the selected account.

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

slopd keeps each pane's identity — backend session ID, account, tags, ancestry,
working directory, and transcript path — in tmux pane options, and rebuilds its
in-memory state from them whenever the daemon restarts. That makes a daemon
restart transparent: the agent processes keep running in tmux and slopd
re-adopts them.

A **reboot** destroys the tmux server, taking those pane options and the agent
processes with it. The backends keep their conversations on disk and can resume
them by session ID, but slopd's record of *which* sessions were running is gone
with tmux. Backup/restore closes that gap through the durable lifecycle journal.

Backup and restore each have an **automatic** toggle plus an always-available **manual** command, and the two automatic toggles are independent — all four combinations are valid:

| | `auto_restore = false` (default) | `auto_restore = true` |
|---|---|---|
| **`auto_backup = true`** (default) | back up automatically; restore only on demand | full reboot survival |
| **`auto_backup = false`** | drive both by hand | restore on reboot from checkpoints you write by hand |

**Backup.** With `auto_backup` on (the default), slopd writes the managed-pane
set as a checkpoint every `[backup] interval_secs` seconds and once more on
clean shutdown; `slopctl backup` checkpoints it on demand. Only panes with a
recorded backend session ID are included. Pane bodies are appended only when
their recovery metadata changes, and an unchanged checkpoint only syncs the
existing file, so a quiet daemon does not grow storage every interval.

The journal defaults below `$XDG_STATE_HOME/slopd/tmux-targets/`. It is first
namespaced by the configured tmux socket and session name, so separate slopd
instances under one Unix user cannot consume each other's restore points. Each
target is then split into generations identified by a tmux-server UUID plus
tmux's `#{session_id}`. The server UUID distinguishes pane IDs reused after a
tmux restart; the session ID distinguishes a managed session killed and
recreated inside the same server. Generation files are append-only JSONL and
are retained without a time or count limit. The old single
`$XDG_STATE_HOME/slopd/panes.json` is imported once for the default target.

**Restore.** With `auto_restore` on, slopd restores when it has to create its
tmux session from scratch — the signature of a fresh server after a reboot. It
uses each recorded backend's native resume path (`claude --resume`, `opencode
-s`, or `codex resume`) and restores the account, working directory, tags, and
parent/child ancestry (remapped to the new pane IDs). On an ordinary daemon
restart the tmux session still exists, so slopd recovers live panes from tmux
and does not duplicate them. `slopctl restore` performs the same operation on
demand and skips sessions that are already running.

**Pending restore (with `auto_restore` off).** When slopd starts into a fresh
generation and `auto_restore` is off, it does *not* resurrect the panes. The
older unresolved checkpoint becomes a **pending restore**: its count appears in
`slopctl status`, and auto-backup is suspended so post-reboot activity cannot
replace the recovery choice. Resolution is itself a journal record, so pending
state survives daemon crashes and restarts without a shared marker file.
`slopctl restore` resolves it by bringing the panes back; `slopctl backup`
resolves it by explicitly choosing the current live set.

Restore never starts two agents on one backend session: it skips a session ID
that is already running or that it has already restored in the same pass. It is
otherwise best-effort — a pane whose session can no longer be resumed simply
fails without preventing the other panes from being restored.

Because restore continues the same backend session, a restored pane keeps its
conversation identity and its checkpoints stay useful across repeated reboots.
The default (`auto_backup = true`, `auto_restore = false`) keeps a current backup
but does not resurrect panes until you run `slopctl restore` or opt into
automatic restore.

---

## slopctl commands

All commands communicate with the running daemon over its Unix socket. Global
`--config PATH` selects the control socket and local client settings from a
specific slopd config; `--socket PATH` overrides its socket for one invocation.

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
PANE  CREATED        LAST_ACTIVE    SESSION         PARENT  BACKEND  ACCOUNT  TAGS      STATE  DETAILED_STATE  WORKING_DIR     TITLE
%1    2 minutes ago  2 minutes ago  session-abc123  -       claude   work     -         ready  ready           ~/code/project  add login
%2    5 seconds ago  5 seconds ago  ses_xyz          %1      opencode default  web,prod  busy   busy_tool_use   ~/code/project  fix tests
```

Filter by `tag`, `backend`, or `account`. Repeated filters use AND semantics:

```bash
slopctl ps --filter tag=prod
slopctl ps --filter backend=codex --filter account=work
```

Output as a JSON array (one object per pane) instead of the default table:

```bash
slopctl ps --json
```

### `slopctl run [--backend KIND] [--no-wait] [-i] [--ready-timeout SECS] [-a NAME] [-c DIR] [-e KEY=VALUE]... [--env-file PATH]... [-- EXTRA_ARGS...]`

Open a new agent pane in the slopd tmux session. Prints the new pane's ID on
stdout. The window is created in the **background** (`tmux new-window -d`), so
spawning never moves clients already watching the session — use `--interactive`
when you do want to land on the new pane.

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

Use `--backend KIND` for a one-off backend override without declaring another
account. The account still supplies its config directory and custom executable;
if that executable is a different canonical backend binary, slopd swaps it to
the selected backend's canonical binary:

```bash
PANE=$(slopctl run --backend opencode)
```

Use `-c` / `--start-directory` to set the working directory for this session, overriding the global `[run] start_directory` from config:

```bash
PANE=$(slopctl run -c ~/code/other-project)
PANE=$(slopctl run --start-directory ~/code/other-project)
```

For local `slopctl`, relative paths are resolved against the client's current
directory. A remote `iroh-slopctl` requires an absolute path because its local
cwd has no meaning on the daemon host.

Use `-e` / `--env KEY=VALUE` (repeatable) to add environment variables to the new pane. Values support `$VAR` / `${VAR}` expansion against slopctl's environment; a missing variable is an error:

```bash
PANE=$(slopctl run --env FOO=bar --env 'TOKEN=${MY_TOKEN}')
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

### `slopctl fork <PANE_ID> [--no-wait] [-i] [--ready-timeout SECS] [-c DIR] [-e KEY=VALUE]... [--env-file PATH]... [-- EXTRA_ARGS...]`

Open a new pane whose agent session **starts as a copy** of `PANE_ID`'s conversation and then diverges independently. The source pane keeps running, untouched — the two share history up to the fork point and nothing after it. The fork inherits the source's account, backend, and (by default) working directory. Prints the new pane id, and — like `run` — waits for it to become ready unless `--no-wait` is given.

```bash
FORK=$(slopctl fork %1)          # branch %1's conversation into a fresh pane
```

How the copy is made is backend-specific, using the agent's native fork rather
than re-sending the conversation:

- **Claude Code** — spawns `claude --resume <src> --fork-session` with a freshly minted `--session-id`, so the new pane is a distinct session whose transcript is copied from the source's.
- **opencode** — calls the source pane's server `POST /session/:id/fork`, which creates a new top-level session copying the source's messages, then binds the new pane to it.
- **Codex** — launches `codex fork <SESSION_ID>`; Codex creates the new rollout
  lazily when its first prompt is submitted, and the resulting `SessionStart`
  hook binds the new session ID.

The fork records the source as its `PARENT` (visible in `ps`), so provenance is
clear. Because Claude resolves a resumed transcript by working directory,
`--start-directory` defaults to the source pane's cwd; overriding it to a
different directory will usually make a Claude fork fail to find the history.
OpenCode and Codex do not have that cwd-based transcript lookup constraint.
`-i` / `--no-wait` / `-e` / `--env-file` behave as they do for `run`.

### `slopctl kill <PANE_ID>`

Terminate a managed agent pane.

```bash
slopctl kill %2
```

### `slopctl send <PANE_ID> <PROMPT> [--interrupt] [--timeout SECS]`
### `slopctl send <KEY=VALUE> <PROMPT> [--filter KEY=VALUE]... [--select one|any|all] [--interrupt] [--timeout SECS]`

Submit `PROMPT` to a pane or to panes matching filters. Claude Code and Codex
use their visible tmux composers and wait for prompt-acceptance confirmation;
Codex input uses bracketed paste to avoid treating the following Enter as a
newline. OpenCode replaces the visible TUI composer through its local API and
then sends a physical Enter, so slash commands retain normal TUI semantics.
The default timeout is 60 seconds.

When the first positional argument contains `=`, it is treated as a filter instead of a pane ID.

```bash
# Send to a specific pane
slopctl send %1 "Summarize this file: README.md"
slopctl send %1 "Run the tests" --timeout 10

# Interrupt a busy pane first, then send a new prompt
slopctl send %1 "Cancel that — do this instead" --interrupt

# Send to all panes tagged "worker"
slopctl send tag=worker "Report your status" --select all

# Combine tag/backend/account filters with AND semantics
slopctl send tag=worker "Run the tests" \
  --filter backend=codex --filter account=work --select all

# Send to any one idle worker
slopctl send tag=idle "Start task X" --select any

# Compact the context on backends that support this slash command
slopctl send tag=worker "/compact" --select all

# Reset a pane's conversation where the backend supports this slash command
slopctl send %1 "/clear"
```

`--select` values (only used with filter target):

| Value | Behaviour |
|-------|-----------|
| `one` (default) | Exactly one matching pane must exist; error otherwise |
| `any` | Send to one arbitrarily chosen matching pane |
| `all` | Send to all matching panes |

Supported filter keys are `tag`, `backend`, and `account`; repeated filters use
AND semantics. `--interrupt` / `-i` preempts the active turn before submitting
the new prompt.

### `slopctl interrupt <PANE_ID>`

Interrupt a running agent using its backend-native path: OpenCode's abort API,
Codex's Escape cancel key, or Claude Code's Ctrl+C / Ctrl+D / Escape sequence.

```bash
slopctl interrupt %1
```

### `slopctl backup`

Write a lifecycle-journal checkpoint now, regardless of the `[backup]
auto_backup` setting. Prints how many panes were recorded. See [Backup and
restore](#backup-and-restore).

```bash
slopctl backup
# backed up 3 pane(s)
```

### `slopctl restore`

Re-spawn panes from the pending or latest checkpoint, regardless of `[backup]
auto_restore`. Sessions already running are skipped, so this is safe against a
live daemon. Prints how many panes were re-spawned.

```bash
slopctl restore
# restored 2 pane(s)
```

### `slopctl graveyard [--boot N] [--limit N] [--json]`

List durable pane-death records, newest first. With no `--boot`, every retained
tmux generation is searched. `--boot 0` selects the current generation,
`--boot -1` the previous generation, and so on. Each row has a stable grave ID;
the complete JSON form also includes the tmux server boot UUID and tmux session
ID, so an old `%N` remains unambiguous after tmux reuses pane IDs. Grave IDs
are timestamp-ordered UUID v7 values generated by the stock `uuid` crate. The
human table shows relative `CREATED`, `DESTROYED`, and `REVIVED` times like
`slopctl ps`; `--json` retains exact Unix timestamps.

```bash
slopctl graveyard
slopctl graveyard --boot -1 --json
```

### `slopctl revive [GRAVE_ID|PANE_ID] [--boot N]`

Resume the backend session captured by a graveyard record and print its new tmux
pane ID. A unique grave-ID prefix is accepted; with no target, the newest
unrevived record is used. An old pane ID such as `%21` is also accepted when it
matches one generation. If it was reused across generations, add `--boot` or use
the stable grave ID. A grave can be revived once; killing the revived pane
creates a new record.

```bash
slopctl revive 019c1234
slopctl revive %21 --boot -2
```

### `slopctl hook <EVENT>`

Forward a Claude Code or Codex lifecycle hook event to slopd. Reads the JSON
payload from stdin. This is normally called by injected hooks.

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

# Transcript records only
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

# Wait for the next UserPromptSubmit on a pane (60s default timeout)
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

## Multi-backend support (OpenCode and Codex)

slopd can drive [Claude Code](https://claude.com/claude-code),
[OpenCode](https://opencode.ai), or OpenAI Codex panes. Each pane's backend is
selected by its account (default `claude`) or a one-off `slopctl run --backend`
override. `slopctl ps` records both `BACKEND` and `ACCOUNT`, and either can be
used as a filter.

OpenCode runs its TUI as a client of an **embedded HTTP server**, so slopd
spawns each pane with a pinned port on `127.0.0.1` and subscribes to `GET /event`
SSE, with `/session` and `/session/status` polling as a backstop. User input is
placed in the visible composer through `/tui/clear-prompt` and
`/tui/append-prompt`, then submitted with a physical Enter. Interrupt uses
`POST /session/:id/abort`, while transcripts come from `GET
/session/:id/message`. These signals are normalized onto the same state, hook,
and transcript interfaces used by the other backends.

> **No server password.** slopd deliberately does **not** set `OPENCODE_SERVER_PASSWORD`: the opencode TUI is itself a client of its embedded server, and its internal client does not authenticate — setting a password makes the TUI `401` against its own server and crash on startup (verified against real opencode 1.17.x). The embedded server is therefore open on `127.0.0.1`, which matches the local-only threat model slopd already assumes. (Headless `opencode serve` does support a password; slopd uses TUI mode.)

### Configuring an OpenCode account

```toml
[accounts.oc]
backend = "opencode"               # selects the opencode backend
config_dir = "~/.config/opencode"  # agent config dir (exported as OPENCODE_CONFIG_DIR)
```

```bash
slopctl run --account oc
```

The `backend` and the executable resolve bidirectionally ("each implies the other"):

- `backend = "opencode"` alone → spawns `opencode` (its canonical binary).
- `executable = "opencode"` (no `backend`) → infers the opencode backend.
- `backend = "claude"` + `executable = "opencode"` → **error** (contradiction).
- `executable = "/path/to/my-opencode-fork"` (unrecognized name) → treated as an executable override under the configured `backend` (default `claude`); set `backend = "opencode"` explicitly to drive a fork.

Named accounts do **not** inherit the top-level `backend` (mirroring `config_dir`); set it on each `[accounts.<name>]`.

### What works identically

`run`, `fork`, `send`, `interrupt`, `listen`, `wait`, `transcript`, tagging,
backup/restore, `ps`, and the iroh remote path are backend-aware behind the same
CLI. A state such as `ready`, `busy_tool_use`, or `awaiting_input_permission`
has the same meaning regardless of backend.

### Current limitations

- **No server password (TUI mode)**: the opencode TUI is itself a client of its embedded server and can't authenticate to it, so slopd spawns the pane **without** `OPENCODE_SERVER_PASSWORD` (verified against real opencode 1.17.x). The server is therefore open on `127.0.0.1` — acceptable for the single-user local model slopd assumes, but it can't be locked down the way headless `opencode serve` can.
- **Subagent transcript not surfaced**: opencode runs subagents as child sessions; slopd tracks the subagent *state* (`busy_subagent`) and emits `SubagentStart`/`SubagentStop` hooks, but the subagent's own messages are **not** folded into the pane's transcript (`slopctl transcript` / `listen --transcript` show the main session only).
- **Rare events mapped per-doc**: the `permission.asked` and `session.compacted` mappings follow the opencode plugin docs but weren't individually triggered in a real-opencode smoke (auto-allowed bash; no compaction in short turns). The common path (idle/busy/tool/subagent/elicitation) is verified against real opencode 1.17.x.

### Configuring a Codex account

Each Codex pane is a standalone `codex` process. slopd does not start a shared
Codex app-server, use `--remote`, or require systemd. It injects `slopctl hook`
commands into `$CODEX_HOME/hooks.json`; `SessionStart` supplies the session ID
and rollout transcript path, and slopd tails that per-session JSONL file. The
interactive Codex CLI creates fresh, forked, and resumed rollouts lazily: the
pane is ready for input first, then its initial prompt fires `SessionStart` and
binds the durable session ID. Consequently, `ps` can briefly show
`session_id: null`, and backup skips that pane until it has received a prompt.

This keeps failure domains independent: a signal, crash, or disconnect affecting
one Codex process does not take the other Codex panes down with a shared service.
The panes may run on different hosts when paired with the normal slopd/iroh
remote path; no Codex-specific shared server is required.

```toml
[accounts.codex]
backend = "codex"
config_dir = "~/.codex"      # exported as CODEX_HOME
executable = "codex"         # needed only if global [run] executable is another backend
```

```bash
slopctl run --account codex
```

To run every pane for an account without approval prompts or sandboxing, put
the Codex flag in that account's executable:

```toml
[accounts.codex-yolo]
backend = "codex"
config_dir = "~/.codex"
executable = ["codex", "--dangerously-bypass-approvals-and-sandbox"]
```

slopd passes configured executable arguments to every standalone launch,
including `codex fork` and backup restore via `codex resume`.

`send` always types into the visible Codex TUI in tmux, so both new prompts and
in-flight steering follow the same UI path as a person at the terminal.
`interrupt` sends Codex's native `Escape` cancel key; it never sends `Ctrl-D`,
which would exit the standalone CLI. `fork` launches Codex's native
`codex fork <SESSION_ID>` and learns the new ID from its first `SessionStart`
hook. On a slopd restart, the existing pane remains untouched; slopd recovers
its identity and state from tmux metadata plus the rollout transcript. Backup
restore launches `codex resume <SESSION_ID>` in the recorded working directory
and makes the restored composer immediately sendable.

Codex approval requests are published as normalized `PermissionRequest` hook
events for observation. They must be answered in the visible TUI; slopd
deliberately does not provide a headless Codex interaction path:

```bash
slopctl listen --pane-id %42 --hook PermissionRequest
```

Codex compatibility notes:

- slopd launches Codex with `--dangerously-bypass-hook-trust` because it
  programmatically installs and vets the hook commands. This flag bypasses the
  hook-source trust prompt; it does not change Codex approval or sandbox policy.
- Codex's separate workspace-trust screen precedes session creation and hooks.
  For unattended launches, trust the working directory in Codex first; slopd
  does not silently trust project-local configuration on the user's behalf.
- Codex's rollout JSONL format is not a stable public interface. Hook events are
  the lifecycle authority; transcript parsing is a recovery and transcript
  adapter that should be checked when upgrading Codex.
- `hooks.json` receives only event names supported by Codex. `slopd
  uninject-hooks` and clean shutdown remove only slopd's entries and preserve
  unrelated hooks.
- If a recorded session was deleted or belongs to a different `CODEX_HOME`,
  `codex resume` fails normally rather than silently creating a replacement.

---

## Claude hook integration

When slopd starts a Claude Code pane it injects `slopctl hook <event>` entries
into that account's `settings.json` (`$CLAUDE_CONFIG_DIR/settings.json`, or the
standard `~/.claude/settings.json`) for all supported lifecycle events:

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

Hook injection is **idempotent** and **concurrency-safe**: an exclusive advisory
lock prevents duplicate entries even if multiple slopd processes target the
same config directory.

Hooks are shared by every Claude process using that same config directory,
including sessions slopd did not spawn. A hook from a newly-created pane can
arrive before slopd has registered it, so the daemon briefly waits for the pane
to enter its managed set before treating it as external. `SessionEnd` is handled
immediately because it is terminal and Claude gives that hook a short deadline.

### Auto-continue on failure

When Claude Code emits `StopFailure`, slopd sends `continue` after an
exponential backoff. OpenCode's corresponding `session.error` path re-submits
the last accepted non-command prompt. Both retry until the turn succeeds or the
attempt cap is reached. Codex does not currently expose an equivalent failure
event to this policy.

Retry is **edge-triggered** by the backend's failure event, not a periodic
timer, so a long-running retry does not provoke another submission while it is
still active. Retrying stops as soon as any of these happens:

- the turn succeeds (`Stop` / `session.idle`) — the counter resets;
- `max_retry_attempts` consecutive failures are reached — slopd gives up and leaves the pane idle;
- you submit a prompt yourself — taking over cancels any pending retry.

All of this is configurable (or disabled) under `[run]` — see `auto_continue_on_failure`, `max_retry_attempts`, `initial_backoff_ms`, and `max_backoff_ms` in [slopd config](#slopd-config). With the defaults (8 attempts, 1s backoff doubling uncapped) a persistently-failing turn is retried over ~4m15s before slopd stops.

---

## Event system

Clients can subscribe to the live event stream with `slopctl listen`. Events are delivered as newline-delimited JSON objects. There are three event sources:

### `source:hook` — normalized agent lifecycle events

```json
{
  "source": "hook",
  "event_type": "UserPromptSubmit",
  "pane_id": "%1",
  "payload": { ... }
}
```

`event_type` uses the Claude-style lifecycle vocabulary (for example,
`SessionStart`, `Stop`, or `PreToolUse`) across all backends. Claude Code and
Codex payloads come from their hooks. OpenCode has no hook file; slopd maps its
SSE events onto the same names and includes the original OpenCode event data
under `properties`.

### `source:slopd` — daemon state events

Emitted by slopd itself for pane lifecycle and state changes.

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

`StateChange` fires when the coarse `state` transitions (`booting_up` → `ready`
→ `busy` → `awaiting_input`). `DetailedStateChange` fires whenever a hook,
transcript record, or backend driver changes the fine-grained state.

Detailed state values: `booting_up`, `ready`, `busy_processing`, `busy_tool_use`, `busy_subagent`, `busy_compacting`, `awaiting_input_permission`, `awaiting_input_elicitation`.

#### `PaneCreated` — a pane was launched

Emitted after a new pane has been registered:

```json
{
  "source": "slopd",
  "event_type": "PaneCreated",
  "pane_id": "%42",
  "payload": {
    "pane_id": "%42",
    "parent_pane_id": "%7"
  }
}
```

#### `PaneDestroyed` — a managed pane died

Emitted once whenever a managed pane is torn down, from every death path, with a classified cause and the pane's full identity — so "what happened to pane %N?" is answerable after the fact, without any surviving tmux state.

```json
{
  "source": "slopd",
  "event_type": "PaneDestroyed",
  "pane_id": "%119",
  "payload": {
    "cause": "vanished",
    "detected_by": "reconcile_vanished",
    "backend": "opencode",
    "session_id": "ses_0e54cad77ffeghO0xI5t1A27d2",
    "parent_pane_id": "%60",
    "working_dir": "/home/claude",
    "title": "tg-dm-responder takeover",
    "last_state": "ready",
    "spawned_at": 1720000000,
    "lived_secs": 12345,
    "exit_status": 37,
    "preceding_hook": "after-kill-pane",
    "output": "…final screen tail (self_exit only)…"
  }
}
```

`cause` is one of:

| cause | meaning |
| --- | --- |
| `deliberate_kill` | an explicit `slopctl kill` (`detected_by: kill_rpc`) |
| `self_exit` | the agent process exited on its own; `exit_status` and `output` (its final screen) are captured |
| `vanished` | the pane was removed from tmux by something outside slopd (an external `tmux kill-pane`/`kill-window`) |
| `server_gone` | the whole tmux server/session was gone — every managed pane died at once |

For a `vanished` pane, `preceding_hook` correlates the tmux lifecycle hook that fired just before it: `after-kill-pane` (a deliberate external kill) versus `window-unlinked` (a closed window).

The broadcast is ephemeral, so slopd also appends the pane's full recovery
snapshot and cause to the lifecycle graveyard and writes a structured log line
to stderr. The graveyard is the durable recovery interface (`slopctl graveyard`
and `slopctl revive`); the systemd journal remains useful for log correlation.
Abnormal deaths (`vanished`, `server_gone`, or a nonzero `self_exit`) log at
`WARN`; a clean `slopctl kill` or zero-exit process logs at `INFO`.

```bash
journalctl --user -u slopd | grep 'pane %119 died'
# pane %119 died: cause=vanished detected_by=reconcile_vanished backend=opencode \
#   session=ses_0e54… parent=%60 state=ready exit=- lived=12345s title="…" \
#   cwd=/home/claude preceding_hook=after-kill-pane
```

### `source:transcript` — normalized agent transcript

Claude Code and Codex records are read from their JSONL transcript/rollout
files. OpenCode records are mapped from its SSE stream and message API. The
payload shape and granularity remain backend-specific, but all are published
through the same source:

```json
{
  "source": "transcript",
  "event_type": "assistant",
  "pane_id": "%1",
  "payload": { ... }
}
```

Typical `event_type` values include `user`, `assistant`, `progress`, and
backend-specific part/tool types.

### Filtering

Subscriptions can be filtered by source/event type, pane ID, session ID, and
payload predicates. At the protocol level, multiple filter objects are OR-ed
and fields within one filter are AND-ed; the `listen` and `wait` options build
these filters for you.

---

## ACP adapter

`slopd-acp` is a stdio [Agent Client Protocol (ACP)](https://agentclientprotocol.com/)
adapter. An ACP host such as [Buzz](https://github.com/block/buzz) launches it
as an agent process; each `session/new` creates a dedicated slopd pane, and
later ACP prompts are sent to that pane.

For a local daemon, configure the ACP host's custom agent executable as:

```bash
slopd-acp --account work --backend codex
```

The socket normally defaults to `$XDG_RUNTIME_DIR/slopd/slopd.sock`, with the
runtime-directory fallback described under [Requirements](#requirements). Use
`--socket PATH` for another local instance.

Useful adapter-wide launch options include:

- `--account NAME` and `--backend claude|opencode|codex`;
- repeatable `--env KEY=VALUE` and `--agent-arg ARG`;
- `--working-directory PATH` to override every ACP-provided cwd;
- `--ready-timeout`, `--send-timeout`, and `--turn-timeout`;
- `--forward-buzz-env` to explicitly forward the adapter's allowlisted
  [Buzz](https://github.com/block/buzz) credentials into panes (off by default,
  especially important over iroh).

The adapter maps ACP onto slopd as follows:

| ACP | slopd operation |
|-----|-----------------|
| `session/new` | `run` with ACP's cwd, selected account/backend, env, and agent args |
| `session/resume` | reuse the resident pane, revive its graveyard entry, or resume its backend-native session |
| `session/list` | list the durable sessions reconstructed from live panes and graveyard entries |
| `session/close` | interrupt an active turn and move the pane to the graveyard while retaining the logical session |
| `session/delete` | persist a deletion tombstone and remove the logical session |
| `session/prompt` | subscribe to pane state and transcript, then `send` |
| `session/update` message/tool chunks | normalized transcript records |
| prompt completion | the pane's busy-to-ready state transition |
| `session/cancel` | `interrupt` |

For ACP hosts such as [Buzz](https://github.com/block/buzz), the adapter
advertises and implements the vendor-neutral `_session/steering` extension. It
routes steering text into the existing pane while the original ACP turn remains
in flight, instead of opening a second session.

Every adapter-created pane receives durable `acp` session and cwd tags. ACP
session IDs have the form `slopd:<uuid>` and do not depend on tmux pane IDs. At
startup, the adapter reconstructs its session catalog from tagged live panes
and the slopd graveyard, then reattaches to resumable panes. This works after
both orderly stdin shutdown and abrupt adapter termination; orderly shutdown
interrupts active turns but deliberately leaves their panes available to a
replacement adapter. A pane that never reached its first accepted prompt is
not considered resumable because its pending ACP system prompt existed only in
the original adapter process.

The adapter advertises ACP `resume`, `list`, `close`, and `delete` session
capabilities. It continues to advertise `loadSession: false`: resuming preserves
the underlying agent context but does not replay historical ACP updates to the
client.

`--max-sessions N` limits **live panes**, not logical ACP sessions (default 4).
At the limit, the adapter moves the least-recently-used inactive pane to the
graveyard while retaining its durable logical session. If that session is used
again, the adapter first attempts an exact graveyard revival and otherwise
resumes the captured backend-native session ID when available. Startup applies
the same bound to panes left by an older adapter. Active turns are never
selected for eviction; if every live pane is active, the new session or prompt
fails instead of disrupting one.

### ACP limitations

The slopd control protocol does not provide a backend-neutral system-role
prompt. By default, `slopd-acp` preserves ACP's `systemPrompt` text by clearly
framing it above the first user prompt, but the underlying CLI receives the
combined text as a user message. Choose the failure policy explicitly when that
is not acceptable:

```bash
slopd-acp --system-prompt-mode reject
slopd-acp --system-prompt-mode ignore
```

The adapter also advertises no MCP or non-text prompt support. A non-empty
`mcpServers` list and image/audio/resource content are rejected rather than
silently discarded.

Permission and elicitation states are observable but not answerable through
slopctl. When an underlying CLI stops at one of those dialogs, the ACP stream
explains that input is required in the terminal pane; it does not issue an ACP
permission request or auto-approve anything. After the terminal interaction,
the same ACP turn continues until the pane returns to ready.

Transcript granularity is backend-dependent. OpenCode generally yields
incremental text parts; Claude and Codex may yield complete message blocks.

### ACP over iroh

Iroh changes only the transport below the adapter. `slopd-acp` shares the ALPN,
client config, persisted secret key, endpoint aliases, address-file format, and
authorization identity used by `iroh-slopctl`. If `iroh-slopctl info` is already
authorized on the server, `slopd-acp` is already authorized too.

```bash
# Use the default endpoint in ~/.config/iroh-slopctl/config.toml
slopd-acp --iroh --account work --backend codex

# Or select an alias/raw EndpointId/full EndpointAddr file
slopd-acp --endpoint my-server
slopd-acp --addr-file /path/to/iroh-addr.json
```

ACP's cwd names a path on the machine running the ACP host. When the remote
slopd host has a different filesystem layout, override it with a path meaningful
to that host:

```bash
slopd-acp --iroh --working-directory /srv/agents/project
```

Use `--iroh-config PATH` to select a different client identity/config instead of
the shared `iroh-slopctl` default.

---

## Remote access (iroh)

`iroh-slopd` and `iroh-slopctl` provide remote access to a running slopd instance by exposing the Unix socket over the [iroh](https://github.com/n0-computer/iroh) peer-to-peer network. This lets you control slopd from another machine via an encrypted P2P connection with EndpointId allowlist authentication.

```
 [remote machine]               [local machine]
 iroh-slopctl ──── iroh ────► iroh-slopd ──► slopd.sock ──► slopd
```

`iroh-slopctl` exposes the same common command set as `slopctl`, plus `info`.
Remote `run`/`fork` require an absolute `--start-directory`, `--interactive` is
not available because the tmux server is remote, and `tags` requires an
explicit pane ID.

### iroh-slopd

`iroh-slopd` is a proxy that listens for iroh connections and forwards them to the local slopd Unix socket. Only clients whose EndpointId has been explicitly authorized are allowed to connect.

**Config file:** `~/.config/iroh-slopd/config.toml`

```toml
# Auto-generated on first run; do not edit manually.
secret_key = "..."

# List of authorized client EndpointIds (z-base-32 public keys).
authorized_clients = []

# Optional local slopd instance to expose. `--socket PATH` overrides it.
# [control]
# socket = "/run/user/1000/slopd-b.sock"
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

Set `[control] socket` in iroh-slopd's config to bridge a non-default local
slopd instance, or use `--socket PATH` as an override. `--config PATH` selects
another iroh server identity/allowlist file. Verbosity can be increased with
`-v` / `-vv` / `-vvv`.

### iroh-slopctl

`iroh-slopctl` is a remote slopctl that connects to `iroh-slopd` instead of a
local Unix socket. It exposes the common daemon commands described under
[slopctl commands](#slopctl-commands), but not the local-only `hook` and
`tmux-hook` commands, and adds an `info` subcommand.

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
| `libslopiroh` | Shared iroh client transport — ALPN, identity/config, endpoint resolution |
| `iroh-slopd` | iroh proxy binary — exposes slopd over iroh with EndpointId allowlist auth |
| `iroh-slopctl` | iroh remote CLI binary — connects to iroh-slopd instead of a Unix socket |
| `slopd-acp` | ACP stdio adapter — exposes slopd-managed panes to hosts such as [Buzz](https://github.com/block/buzz) |
| `libsloptest` | Test helpers — isolated tmux environments for integration tests |
