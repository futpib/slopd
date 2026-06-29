use clap::{Parser, Subcommand};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{Mutex, Notify};
use tracing::{debug, error, info, trace, warn};

mod opencode;

#[derive(Parser)]
#[command(name = "slopd", about = "Claude session manager daemon", version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT"), ")"))]
struct Cli {
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
    /// Override the executable used to spawn Claude sessions (default: from config or "claude").
    /// Specify the program and optional arguments, e.g. --executable claude --foo --bar
    #[arg(long, num_args = 1.., allow_hyphen_values = true)]
    executable: Option<Vec<String>>,
    /// Read configuration from this file instead of the default
    /// `$XDG_CONFIG_HOME/slopd/config.toml`. Supports `~` and `$VAR` expansion.
    /// Lets a second slopd instance run from its own config (give it a distinct
    /// `[tmux] socket`/`session`, and `--socket` for the control socket).
    #[arg(long, value_name = "PATH")]
    config: Option<std::path::PathBuf>,
    /// Listen on this control socket instead of the default
    /// `$XDG_RUNTIME_DIR/slopd/slopd.sock`. Supports `~` and `$VAR` expansion.
    /// `slopctl` must be given the same `--socket` to reach this instance;
    /// injected hook commands carry it automatically so spawned panes report
    /// back here. This is the clean way to isolate a second instance's control
    /// socket without juggling `$XDG_RUNTIME_DIR`.
    #[arg(long, value_name = "PATH")]
    socket: Option<std::path::PathBuf>,
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Remove slopctl hook entries from Claude's settings.json.
    UninjectHooks,
}

fn tmux(config: &libslop::SlopdConfig) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("tmux");
    if let Some(socket) = &config.tmux.socket {
        let socket = libslop::expand_path(socket);
        cmd.args(["-S", socket.to_str().unwrap()]);
    }
    cmd
}

async fn tmux_set_pane_option(config: &libslop::SlopdConfig, pane_id: &str, option: &str, value: &str) -> std::io::Result<std::process::ExitStatus> {
    tmux(config)
        .args(["set-option", "-t", pane_id, "-p", option, value])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
}

async fn tmux_unset_pane_option(config: &libslop::SlopdConfig, pane_id: &str, option: &str) -> std::io::Result<std::process::ExitStatus> {
    tmux(config)
        .args(["set-option", "-t", pane_id, "-p", "-u", option])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
}

async fn tmux_send_keys(config: &libslop::SlopdConfig, pane_id: &str, keys: &str) -> std::io::Result<std::process::ExitStatus> {
    tmux(config)
        .args(["send-keys", "-t", pane_id, keys])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
}

/// Events that can cause a pane state transition.
enum PaneStateEvent<'a> {
    /// slopd startup recovery or new pane creation.
    Init,
    /// A hook fired by Claude (received via the hook socket).
    Hook { event: &'a str, notification_type: Option<&'a str> },
    /// A transcript record was observed.
    TranscriptRecord { record_type: &'a str, record: &'a serde_json::Value },
}

/// Pure reducer: given the current state and an event, returns the new state
/// (or None if the event doesn't cause a transition).
fn reduce_pane_state(
    current: &libslop::PaneDetailedState,
    event: &PaneStateEvent,
) -> Option<libslop::PaneDetailedState> {
    match event {
        PaneStateEvent::Init => Some(libslop::PaneDetailedState::BootingUp),

        PaneStateEvent::Hook { event, notification_type } => reduce_hook_event(event, *notification_type),

        PaneStateEvent::TranscriptRecord { record_type, record } => {
            match *record_type {
                // `progress` records with `data.type: "hook_progress"` carry the
                // hook event name in `data.hookEvent`. Replay them like hooks.
                "progress" => {
                    let hook_event = record
                        .get("data")
                        .and_then(|d| {
                            if d.get("type").and_then(|t| t.as_str()) == Some("hook_progress") {
                                d.get("hookEvent").and_then(|e| e.as_str())
                            } else {
                                None
                            }
                        });
                    hook_event.and_then(|e| reduce_hook_event(e, None))
                }

                // `system` with `subtype: "turn_duration"` marks the end of a turn —
                // Claude is idle and ready for input.
                "system" if record.get("subtype").and_then(|s| s.as_str()) == Some("turn_duration") => {
                    Some(libslop::PaneDetailedState::Ready)
                }

                // When Claude is interrupted while awaiting permission or elicitation
                // input, it writes transcript `user` events (tool rejection + interrupt
                // message) but does NOT fire any hooks.
                "user" if matches!(current,
                    libslop::PaneDetailedState::AwaitingInputPermission
                    | libslop::PaneDetailedState::AwaitingInputElicitation
                ) => Some(libslop::PaneDetailedState::Ready),

                _ => None,
            }
        }
    }
}

/// Map a hook event name to the resulting detailed state.
fn reduce_hook_event(event: &str, notification_type: Option<&str>) -> Option<libslop::PaneDetailedState> {
    match event {
        "SessionStart" => Some(libslop::PaneDetailedState::Ready),
        "UserPromptSubmit" => Some(libslop::PaneDetailedState::BusyProcessing),
        "Stop" | "StopFailure" => Some(libslop::PaneDetailedState::Ready),
        "PreToolUse" => Some(libslop::PaneDetailedState::BusyToolUse),
        "PostToolUse" | "PostToolUseFailure" => Some(libslop::PaneDetailedState::BusyProcessing),
        "PermissionRequest" => Some(libslop::PaneDetailedState::AwaitingInputPermission),
        "SubagentStart" => Some(libslop::PaneDetailedState::BusySubagent),
        "SubagentStop" | "ElicitationResult" => Some(libslop::PaneDetailedState::BusyProcessing),
        "PreCompact" => Some(libslop::PaneDetailedState::BusyCompacting),
        "PostCompact" => Some(libslop::PaneDetailedState::BusyProcessing),
        "Elicitation" => Some(libslop::PaneDetailedState::AwaitingInputElicitation),
        // Claude fires Notification with notification_type "idle_prompt"
        // ("Claude is waiting for your input") when it returns to the prompt.
        // This is the authoritative idle signal and the only recovery for
        // turns that end without a clean Stop (e.g. SubagentStop after a
        // /clear-over-busy race) — without it the pane stays stuck busy.
        // Other Notification types (permission, etc.) must not clear state.
        "Notification" if notification_type == Some("idle_prompt") => {
            Some(libslop::PaneDetailedState::Ready)
        }
        _ => None,
    }
}

async fn set_pane_detailed_state(
    config: &libslop::SlopdConfig,
    pane_id: &str,
    detailed: &libslop::PaneDetailedState,
    previous: Option<&libslop::PaneDetailedState>,
    event_tx: &EventTx,
    panes: &PaneMap,
) {
    *panes.get_or_insert(pane_id).detailed_state.lock().unwrap() = detailed.clone();
    let simple = detailed.to_simple();
    for (opt, val) in [
        (libslop::TmuxOption::SlopdState, simple.as_str()),
        (libslop::TmuxOption::SlopdDetailedState, detailed.as_str()),
    ] {
        if let Err(e) = tmux_set_pane_option(config, pane_id, opt.as_str(), val).await {
            warn!("failed to set {} on pane {}: {}", opt.as_str(), pane_id, e);
        }
    }

    let previous_simple = previous.map(|p| p.to_simple());
    if previous_simple.as_ref() != Some(&simple) {
        let _ = event_tx.send(libslop::Record {
            source: "slopd".to_string(),
            event_type: "StateChange".to_string(),
            pane_id: Some(pane_id.to_string()),
            payload: serde_json::json!({
                "state": simple.as_str(),
                "previous_state": previous_simple.as_ref().map(|s| s.as_str()),
            }),
            cursor: None,
        });
    }

    let _ = event_tx.send(libslop::Record {
        source: "slopd".to_string(),
        event_type: "DetailedStateChange".to_string(),
        pane_id: Some(pane_id.to_string()),
        payload: serde_json::json!({
            "detailed_state": detailed.as_str(),
            "previous_detailed_state": previous.map(|p| p.as_str()),
        }),
        cursor: None,
    });
}

/// Exponential-backoff policy for auto-continue retries. A plain copy of the
/// three relevant `[run]` config knobs, so the retry decision logic stays pure
/// and unit-testable without constructing a whole `SlopdConfig`.
#[derive(Clone, Copy, Debug)]
struct BackoffPolicy {
    max_attempts: u32,
    initial_backoff_ms: u64,
    /// Optional ceiling on the per-retry delay. `None` lets the delay keep
    /// doubling uncapped; `Some(ms)` flattens the schedule into steady polling
    /// once the delay reaches `ms`.
    max_backoff_ms: Option<u64>,
}

impl BackoffPolicy {
    fn from_config(cfg: &libslop::SlopdRunConfig) -> Self {
        Self {
            max_attempts: cfg.max_retry_attempts,
            initial_backoff_ms: cfg.initial_backoff_ms,
            max_backoff_ms: cfg.max_backoff_ms,
        }
    }

    /// Delay before the `attempt`-th retry (1-based): `initial * 2^(attempt-1)`,
    /// optionally capped at `max_backoff_ms`. The exponent is clamped and the
    /// multiply saturates, so a long streak can't overflow — uncapped, the delay
    /// just grows until it saturates `u64`.
    fn delay_ms(&self, attempt: u32) -> u64 {
        let delay = self.initial_backoff_ms
            .saturating_mul(2_u64.pow(attempt.saturating_sub(1).min(63)));
        match self.max_backoff_ms {
            Some(cap) => delay.min(cap),
            None => delay,
        }
    }
}

/// Retry state for auto-continue on StopFailure.
#[derive(Clone, Debug)]
struct RetryState {
    attempt_count: u32,
    next_send_at: tokio::time::Instant,
}

impl RetryState {
    /// Given the previous retry state (if any) and the backoff policy, compute
    /// the next retry to schedule — or `None` once the attempt cap is exceeded.
    /// Pure aside from the injected `now`, so the whole backoff-and-give-up
    /// policy is unit-testable without a clock or any I/O.
    fn next(
        prev: Option<&RetryState>,
        policy: &BackoffPolicy,
        now: tokio::time::Instant,
    ) -> Option<RetryState> {
        let attempt = prev.map_or(1, |s| s.attempt_count + 1);
        if attempt > policy.max_attempts {
            return None;
        }
        Some(RetryState {
            attempt_count: attempt,
            next_send_at: now + tokio::time::Duration::from_millis(policy.delay_ms(attempt)),
        })
    }

    /// Whether this state still matches a retry that was scheduled for
    /// (`attempt`, `at`). A manual prompt or a clean Stop replaces/clears the
    /// per-pane retry state, so a delayed sender uses this to detect that its
    /// scheduled retry was superseded and bail out.
    fn matches(&self, attempt: u32, at: tokio::time::Instant) -> bool {
        self.attempt_count == attempt && self.next_send_at == at
    }
}

/// Per-pane opencode runtime: the HTTP client driving the pane's embedded
/// server + the session id to target. `None` for Claude panes.
#[derive(Clone)]
struct OpencodeState {
    client: opencode::OpencodeClient,
    session_id: String,
}

/// Per-pane state shared across connection handlers.
struct PaneState {
    /// Serialises the type-then-enter sequence so two concurrent sends don't interleave.
    type_mutex: Mutex<()>,
    /// Notified whenever UserPromptSubmit fires for this pane.
    prompt_submitted: Notify,
    /// Cached detailed state, kept in sync by set_pane_detailed_state.
    detailed_state: std::sync::Mutex<libslop::PaneDetailedState>,
    /// Cancels the transcript tail task when the pane is killed or the tailer is restarted.
    transcript_cancel: std::sync::Mutex<tokio_util::sync::CancellationToken>,
    /// The transcript path currently being tailed (if any).
    transcript_path: std::sync::Mutex<Option<String>>,
    /// Auto-continue retry state (when a turn fails with StopFailure).
    retry_state: std::sync::Mutex<Option<RetryState>>,
    /// Set just before slopd injects its own "continue" prompt, so the
    /// UserPromptSubmit that prompt triggers is not mistaken for the user
    /// manually taking over (which would reset the retry counter and let a
    /// persistently-failing turn retry forever, defeating max_retry_attempts).
    expecting_auto_continue: std::sync::atomic::AtomicBool,
    /// For opencode panes: HTTP client + session id (set on spawn). None for Claude.
    opencode: std::sync::Mutex<Option<OpencodeState>>,
    /// Cancels the opencode status-polling driver when the pane is killed.
    opencode_cancel: std::sync::Mutex<tokio_util::sync::CancellationToken>,
}

impl PaneState {
    fn new() -> Self {
        Self {
            type_mutex: Mutex::new(()),
            prompt_submitted: Notify::new(),
            detailed_state: std::sync::Mutex::new(libslop::PaneDetailedState::BootingUp),
            transcript_cancel: std::sync::Mutex::new(tokio_util::sync::CancellationToken::new()),
            transcript_path: std::sync::Mutex::new(None),
            retry_state: std::sync::Mutex::new(None),
            expecting_auto_continue: std::sync::atomic::AtomicBool::new(false),
            opencode: std::sync::Mutex::new(None),
            opencode_cancel: std::sync::Mutex::new(tokio_util::sync::CancellationToken::new()),
        }
    }

    /// Whether this pane is an opencode pane (has an opencode runtime attached).
    fn is_opencode(&self) -> bool {
        self.opencode.lock().unwrap().is_some()
    }
}

/// OpenCode pane driver: poll the pane's embedded server for session status and
/// translate it into slopd detailed-state transitions via the shared
/// [`set_pane_detailed_state`] path (the same one Claude's hook handler uses).
/// This is the opencode analogue of Claude's `slopctl hook` socket + jsonl
/// tailer, collapsed into one HTTP poll loop.
async fn run_opencode_driver(
    client: opencode::OpencodeClient,
    session_id: String,
    pane_id: String,
    config: Arc<libslop::SlopdConfig>,
    panes: PaneMap,
    event_tx: EventTx,
    cancel: tokio_util::sync::CancellationToken,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(700));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = interval.tick() => {}
        }
        match client.session_status(&session_id).await {
            Ok(Some(status)) => {
                if let Some(new) = opencode::status_to_detailed(&status) {
                    let current = panes.get_or_insert(&pane_id).detailed_state.lock().unwrap().clone();
                    if new != current {
                        set_pane_detailed_state(&config, &pane_id, &new, Some(&current), &event_tx, &panes).await;
                    }
                }
            }
            Ok(None) => {
                // Session not listed yet (pane still booting); leave state as-is.
            }
            Err(e) => debug!("opencode status poll failed for {}: {}", pane_id, e),
        }
    }
}

/// Tail a transcript .jsonl file, broadcasting each new JSON record as an event.
/// Reads from `offset` (the byte position after any content that existed before
/// we started watching) and polls for new data until cancelled.
async fn tail_transcript(
    path: std::path::PathBuf,
    pane_id: String,
    pane_state: Arc<PaneState>,
    config: Arc<libslop::SlopdConfig>,
    panes: PaneMap,
    event_tx: EventTx,
    cancel: tokio_util::sync::CancellationToken,
) {
    use tokio::io::AsyncBufReadExt;

    // Open the file; if it doesn't exist yet, wait until it appears.
    let file = loop {
        match tokio::fs::File::open(&path).await {
            Ok(f) => break f,
            Err(_) => {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
                }
            }
        }
    };

    let mut reader = tokio::io::BufReader::new(file);
    let mut line = String::new();
    let mut byte_pos: u64 = 0;

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                // EOF — wait for more data or cancellation.
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
                }
            }
            Ok(n) => {
                let line_start = byte_pos;
                byte_pos += n as u64;

                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<serde_json::Value>(trimmed) {
                    Ok(record) => {
                        let record_type = record
                            .get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string();

                        // A queue-operation enqueue means a prompt was accepted
                        // while Claude was busy. Notify pending senders so
                        // slopctl send unblocks immediately.
                        if record_type == "queue-operation"
                            && record.get("operation").and_then(|v| v.as_str()) == Some("enqueue") {
                                debug!("transcript enqueue: notifying pending senders for pane {}", pane_id);
                                pane_state.prompt_submitted.notify_waiters();
                            }

                        // Client-local slash commands (/model, /effort, /compact,
                        // /clear, /rename, ...) fire NO UserPromptSubmit hook.
                        // Their command record appears in one of two shapes:
                        //   - type=user with message.content starting with
                        //     `<command-name>/` (e.g. /clear)
                        //   - type=system, subtype=local_command, with top-level
                        //     content starting with `<command-name>/` (e.g. /rename)
                        // Either is a prompt-accepted signal — notify pending
                        // senders so `slopctl send` confirms without timing out.
                        let is_slash_command_record = match record_type.as_str() {
                            "user" => record
                                .get("message")
                                .and_then(|m| m.get("content"))
                                .and_then(|c| c.as_str())
                                .is_some_and(|content| content.starts_with("<command-name>/")),
                            "system" => record
                                .get("subtype")
                                .and_then(|v| v.as_str())
                                == Some("local_command")
                                && record
                                    .get("content")
                                    .and_then(|c| c.as_str())
                                    .is_some_and(|content| content.starts_with("<command-name>/")),
                            _ => false,
                        };
                        if is_slash_command_record {
                            debug!("transcript slash-command: notifying pending senders for pane {}", pane_id);
                            pane_state.prompt_submitted.notify_waiters();
                        }

                        // Check if this transcript record triggers a state transition.
                        {
                            let current = pane_state.detailed_state.lock().unwrap().clone();
                            let event = PaneStateEvent::TranscriptRecord { record_type: &record_type, record: &record };
                            if let Some(new_state) = reduce_pane_state(&current, &event) {
                                debug!("transcript {} event while pane {} in {:?} — transitioning to {:?}", record_type, pane_id, current, new_state);
                                set_pane_detailed_state(
                                    &config, &pane_id, &new_state,
                                    Some(&current), &event_tx, &panes,
                                ).await;
                            }
                        }

                        let _ = event_tx.send(libslop::Record {
                            source: "transcript".to_string(),
                            event_type: record_type,
                            pane_id: Some(pane_id.clone()),
                            payload: record,
                            cursor: Some(line_start),
                        });
                    }
                    Err(e) => {
                        debug!("failed to parse transcript line: {}", e);
                    }
                }
            }
            Err(e) => {
                warn!("error reading transcript {}: {}", path.display(), e);
                return;
            }
        }
    }
}

/// Read the last `n` JSON records from a transcript JSONL file.
/// Returns `(records, file_len)` where each record is `(byte_offset, parsed_json)`,
/// ordered oldest-first, and `file_len` is the file size at read time.
async fn read_transcript_tail(
    path: &std::path::Path,
    n: u64,
) -> std::io::Result<(Vec<(u64, serde_json::Value)>, u64)> {
    use tokio::io::AsyncBufReadExt;

    let file = tokio::fs::File::open(path).await?;
    let file_len = file.metadata().await?.len();
    let mut reader = tokio::io::BufReader::new(file);
    let mut line = String::new();
    let mut byte_pos: u64 = 0;
    let n = n as usize;

    // Sliding window: keep only the last N valid records.
    let mut window = std::collections::VecDeque::with_capacity(n + 1);

    loop {
        line.clear();
        match reader.read_line(&mut line).await? {
            0 => break,
            bytes_read => {
                let line_start = byte_pos;
                byte_pos += bytes_read as u64;

                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(record) = serde_json::from_str::<serde_json::Value>(trimmed) {
                    if window.len() == n {
                        window.pop_front();
                    }
                    window.push_back((line_start, record));
                }
            }
        }
    }

    Ok((window.into(), file_len))
}

/// Read up to `limit` JSON records from a transcript JSONL file that start
/// strictly before `before_offset` bytes. Returns `(records, at_beginning)`
/// where records are ordered oldest-first.
async fn read_transcript_before(
    path: &std::path::Path,
    before_offset: u64,
    limit: u64,
) -> std::io::Result<(Vec<(u64, serde_json::Value)>, bool)> {
    use tokio::io::AsyncBufReadExt;

    let file = tokio::fs::File::open(path).await?;
    let mut reader = tokio::io::BufReader::new(file);
    let mut line = String::new();
    let mut byte_pos: u64 = 0;
    let limit = limit as usize;

    let mut window = std::collections::VecDeque::with_capacity(limit + 1);

    loop {
        line.clear();
        match reader.read_line(&mut line).await? {
            0 => break,
            bytes_read => {
                let line_start = byte_pos;
                byte_pos += bytes_read as u64;

                if line_start >= before_offset {
                    break;
                }

                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(record) = serde_json::from_str::<serde_json::Value>(trimmed) {
                    if window.len() == limit {
                        window.pop_front();
                    }
                    window.push_back((line_start, record));
                }
            }
        }
    }

    let at_beginning = window.front().is_none_or(|(offset, _)| *offset == 0);
    Ok((window.into(), at_beginning))
}

/// Map of tmux pane ID → per-pane shared state.
///
/// ## Why this is a newtype, not a plain `DashMap`
///
/// An earlier version exposed `dashmap::DashMap` directly and deadlocked in
/// production (commit 2bac67b / r163): a `for entry in panes.iter()` loop held
/// a shard read guard across a `tmux(...).output().await`, and a concurrent
/// `panes.remove(...)` parked on the same shard's writer-preferring
/// `parking_lot::RwLock`.
///
/// The newtype fixes this by construction.  Only owned-return APIs are exposed
/// (`Arc<PaneState>`, `Option<Arc<PaneState>>`, `Vec<String>`) — none of which
/// borrow from the underlying map.  There is no way for a caller to obtain a
/// shard guard, so there is no way to hold one across an `.await`.
///
/// **Review rule:** do not add methods that return `dashmap::mapref::one::Ref`,
/// `RefMut`, `Entry`, or `Iter` (or anything that borrows from the map).  Every
/// public method must return an owned value.
#[derive(Clone)]
struct PaneMap {
    inner: Arc<dashmap::DashMap<String, Arc<PaneState>>>,
}

impl PaneMap {
    fn new() -> Self {
        Self { inner: Arc::new(dashmap::DashMap::new()) }
    }

    /// Return the `Arc<PaneState>` for `pane_id`, creating a fresh one if
    /// absent.  The shard guard is released before this returns.
    fn get_or_insert(&self, pane_id: &str) -> Arc<PaneState> {
        self.inner
            .entry(pane_id.to_string())
            .or_insert_with(|| Arc::new(PaneState::new()))
            .clone()
    }

    /// Return the existing `Arc<PaneState>` for `pane_id` if any.  The shard
    /// guard is released before this returns.
    fn get(&self, pane_id: &str) -> Option<Arc<PaneState>> {
        self.inner.get(pane_id).map(|r| r.clone())
    }

    /// Remove and return the `Arc<PaneState>` for `pane_id` if any.  The
    /// shard guard is released before this returns.
    fn remove(&self, pane_id: &str) -> Option<Arc<PaneState>> {
        self.inner.remove(pane_id).map(|(_, v)| v)
    }
}

/// Set of pane IDs in the `slopd` tmux session.
/// Populated from tmux on startup (so it survives slopd restarts) and kept
/// in sync as panes are created/killed.
///
/// See `PaneMap` doc-comment for the reason this is a newtype — same deadlock
/// hazard applies to `DashSet::iter()`.  Only owned-return APIs are exposed.
#[derive(Clone)]
struct ManagedPanes {
    inner: Arc<dashmap::DashSet<String>>,
}

impl ManagedPanes {
    fn new() -> Self {
        Self { inner: Arc::new(dashmap::DashSet::new()) }
    }

    fn insert(&self, pane_id: String) -> bool {
        self.inner.insert(pane_id)
    }

    fn remove(&self, pane_id: &str) -> bool {
        self.inner.remove(pane_id).is_some()
    }

    fn contains(&self, pane_id: &str) -> bool {
        self.inner.contains(pane_id)
    }

    /// Return a snapshot of the current pane IDs as an owned `Vec<String>`.
    ///
    /// This is the only way to iterate.  The shard guards used internally are
    /// released before the `Vec` is returned, so the caller is free to
    /// `.await` while walking the snapshot.  Writers that arrive after the
    /// snapshot is taken are not reflected — this is intentional; any change
    /// concurrent with a reconcile/reparent pass will be picked up on the
    /// next pass.
    fn snapshot(&self) -> Vec<String> {
        self.inner.iter().map(|r| r.key().clone()).collect()
    }
}

/// Populate the managed-pane set from the `slopd` tmux session.
/// Read the account a pane was launched under from its `@slopd_account` option.
/// Returns `None` when the option is unset/empty or the pane can't be queried
/// (e.g. the parent isn't a slopd-managed pane) — the caller then falls back to
/// `default_account` / the default account.
async fn read_pane_account(config: &libslop::SlopdConfig, pane_id: &str) -> Option<String> {
    let out = tmux(config)
        .args(["show-options", "-t", pane_id, "-p", "-v",
               libslop::TmuxOption::SlopdAccount.as_str()])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let val = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if val.is_empty() { None } else { Some(val) }
}

/// Scan the slopd session for managed panes and return the distinct
/// `settings.json` paths whose hooks must be re-injected — one per account a
/// recovered pane belongs to (the unnamed default included).
///
/// Only panes that have the `@slopd_managed` pane option set are considered managed
/// (i.e. were registered via `slopctl run`). For each recovered pane, replays the
/// last N transcript records through the state reducer to recover the real state
/// instead of leaving it stuck at BootingUp.
async fn load_managed_panes(config: &Arc<libslop::SlopdConfig>, managed: &ManagedPanes, event_tx: &EventTx, panes: &PaneMap) -> std::collections::HashSet<std::path::PathBuf> {
    let mut settings_paths = std::collections::HashSet::new();
    let session = config.tmux.session();
    // Enumerate the session's pane ids, then read each pane's options with
    // `show-options -p` (pane scope, *no* inheritance).
    //
    // We must NOT detect managed panes via a `#{@slopd_managed}` format on
    // `list-panes`: a format resolves user options hierarchically, so the
    // session's idle shell — which has no pane-level value — would inherit the
    // session-level @slopd_managed marker (set in main) and be wrongly adopted.
    // The per-pane `-p` read returns only options actually set on the pane, so
    // it sees @slopd_managed only on real managed panes (cf. pane_is_still_alive).
    let output = tmux(config)
        .args(["list-panes", "-s", "-t", &session, "-F", "#{pane_id}"])
        .output()
        .await;
    let Ok(out) = output else { return settings_paths };
    if !out.status.success() {
        return settings_paths;
    }
    for pane_id in String::from_utf8_lossy(&out.stdout).lines().map(str::trim).filter(|s| !s.is_empty()) {
        let opts = match tmux(config).args(["show-options", "-t", pane_id, "-p"]).output().await {
            Ok(o) if o.status.success() => parse_pane_options(&String::from_utf8_lossy(&o.stdout)),
            _ => continue,
        };
        if !opts.slopd_managed {
            continue;
        }
        managed.insert(pane_id.to_string());

        // Record where this pane's hooks live so we can re-inject them.
        // An unresolvable account (removed from config, or a misconfigured
        // default_account) falls back to the reserved default account, which
        // always resolves — recovery must never crash startup.
        let resolved = config
            .resolve_account(opts.account.as_deref())
            .or_else(|_| config.resolve_account(Some(libslop::DEFAULT_ACCOUNT)))
            .expect("the reserved default account always resolves");
        // Hooks are a Claude-only mechanism; a non-Claude backend has no
        // settings.json to inject into (and nothing to re-inject on recovery).
        if resolved.backend.uses_injected_hooks() {
            settings_paths.insert(config.resolved_settings_path(&resolved));
        }

        // Replay the last N transcript records to recover the real state.
        let transcript_path = opts.transcript_path;
        let recovered_state = match transcript_path.as_deref() {
            Some(path) => recover_state_from_transcript(path).await,
            None => None,
        };
        let initial_state = recovered_state.unwrap_or(libslop::PaneDetailedState::BootingUp);
        set_pane_detailed_state(config, pane_id, &initial_state, None, event_tx, panes).await;

        // Start the transcript tailer if we have a path.
        if let Some(transcript_path) = transcript_path {
            let state = panes.get_or_insert(pane_id);
            let new_cancel = tokio_util::sync::CancellationToken::new();
            *state.transcript_cancel.lock().unwrap() = new_cancel.clone();
            *state.transcript_path.lock().unwrap() = Some(transcript_path.clone());
            tokio::spawn(tail_transcript(
                std::path::PathBuf::from(transcript_path),
                pane_id.to_string(),
                state.clone(),
                config.clone(),
                panes.clone(),
                event_tx.clone(),
                new_cancel,
            ));
        }

        // OpenCode pane recovery: the embedded HTTP server is still running in
        // the recovered tmux pane (a daemon restart doesn't kill it). Reattach
        // the HTTP runtime from the stored port/token/session and resume the
        // status-poll driver, which will advance BootingUp → the real state.
        if opts.backend == Some(libslop::Backend::Opencode) {
            if let (Some(port), Some(session_id)) = (opts.opencode_port, opts.session_id.as_deref()) {
                if !session_id.is_empty() {
                    let client = opencode::OpencodeClient::new(port, opts.opencode_token.clone());
                    let driver_cancel = tokio_util::sync::CancellationToken::new();
                    let state = panes.get_or_insert(pane_id);
                    *state.opencode.lock().unwrap() = Some(OpencodeState {
                        client: client.clone(),
                        session_id: session_id.to_string(),
                    });
                    *state.opencode_cancel.lock().unwrap() = driver_cancel.clone();
                    tokio::spawn(run_opencode_driver(
                        client,
                        session_id.to_string(),
                        pane_id.to_string(),
                        config.clone(),
                        panes.clone(),
                        event_tx.clone(),
                        driver_cancel,
                    ));
                }
            }
        }
    }
    settings_paths
}

/// Replay the last N records from a transcript file through the state reducer
/// to recover the pane's actual state after a slopd restart.
async fn recover_state_from_transcript(
    transcript_path: &str,
) -> Option<libslop::PaneDetailedState> {
    let path = std::path::Path::new(transcript_path);
    let (records, _) = read_transcript_tail(path, 100).await.ok()?;
    if records.is_empty() {
        return None;
    }

    // Replay records through the reducer starting from BootingUp.
    let mut state = libslop::PaneDetailedState::BootingUp;
    for (_offset, record) in &records {
        let record_type = record.get("type").and_then(|v| v.as_str()).unwrap_or("unknown");
        let event = PaneStateEvent::TranscriptRecord { record_type, record };
        if let Some(new_state) = reduce_pane_state(&state, &event) {
            state = new_state;
        }
    }
    Some(state)
}

type EventTx = Arc<tokio::sync::broadcast::Sender<libslop::Record>>;
type PaneRegistered = Arc<tokio::sync::Notify>;
/// After a reboot with `auto_restore` off, holds `Some(n)` while `n` panes from
/// the on-disk manifest await a `slopctl restore`; `None` when nothing is
/// pending. While `Some`, auto-backup is suspended so the manifest (the restore
/// point) is preserved through any post-reboot activity until the user resolves
/// it via `slopctl restore` (consume) or `slopctl backup` (replace).
type PendingRestore = Arc<std::sync::Mutex<Option<usize>>>;

/// How long to wait for a pane to be registered before concluding that a hook
/// came from a genuinely unmanaged (external) pane.  The race window is
/// typically sub-millisecond; 2 s is generous headroom for a loaded system.
const PANE_REGISTRATION_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

fn filters_match(filters: &[libslop::EventFilter], ev: &libslop::Record) -> bool {
    if filters.is_empty() {
        return true;
    }
    filters.iter().any(|f| {
        if let Some(ref src) = f.source
            && src != &ev.source {
                return false;
            }
        if let Some(ref et) = f.event_type
            && et != &ev.event_type {
                return false;
            }
        if let Some(ref pane_id) = f.pane_id
            && ev.pane_id.as_deref() != Some(pane_id.as_str()) {
                return false;
            }
        if let Some(ref session_id) = f.session_id
            && ev.payload.get("session_id").and_then(|v| v.as_str()) != Some(session_id.as_str()) {
                return false;
            }
        for (k, v) in &f.payload_match {
            if ev.payload.get(k) != Some(v) {
                return false;
            }
        }
        if !libslop::predicates_match(&ev.payload, &f.payload_path_match) {
            return false;
        }
        true
    })
}

/// Tmux session-scope hooks that slopd subscribes to on the slopd session.
/// Each entry is (hook_name, include_pane_id).
/// Note: pane-exited and pane-died are pane/window-scoped in tmux and cannot
/// be set at session level, so we rely on a background polling reconciler for
/// detecting process exit.
const TMUX_HOOKS: &[(&str, bool)] = &[
    ("after-kill-pane", false),
    ("window-linked", false),
    ("window-unlinked", false),
];

/// Build the `run-shell` command string for a tmux hook.
/// Includes XDG_RUNTIME_DIR so slopctl can find the slopd socket even when
/// the hook fires in the tmux server's environment (not a pane).
fn tmux_hook_command(slopctl: &str, hook_name: &str, include_pane_id: bool) -> String {
    let runtime_dir = libslop::runtime_dir();
    let runtime_str = runtime_dir.to_str().unwrap();
    if include_pane_id {
        format!("run-shell \"XDG_RUNTIME_DIR={} {} tmux-hook {} #{{hook_pane}} || true\"", runtime_str, slopctl, hook_name)
    } else {
        format!("run-shell \"XDG_RUNTIME_DIR={} {} tmux-hook {} || true\"", runtime_str, slopctl, hook_name)
    }
}

/// Idempotently register slopd's tmux hooks on the slopd session.
/// Appends our hook commands if not already present; removes stale entries
/// from a previous slopctl path.
async fn register_tmux_hooks(config: &libslop::SlopdConfig) {
    let slopctl = config.hook_slopctl();
    let session = config.tmux.session();

    // Read existing hooks.
    let existing = match tmux(config)
        .args(["show-hooks", "-t", &session])
        .output()
        .await
    {
        Ok(out) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).to_string()
        }
        _ => String::new(),
    };

    for &(hook_name, include_pane_id) in TMUX_HOOKS {
        let our_command = tmux_hook_command(&slopctl, hook_name, include_pane_id);

        // Check if our exact command is already present.
        let already_present = existing.lines().any(|line| {
            line.starts_with(&format!("{}[", hook_name)) && line.contains(&our_command)
        });
        if already_present {
            continue;
        }

        // Remove stale entries: lines whose command contains "slopctl tmux-hook <hook>"
        // (or an absolute path ending in /slopctl) but is not our current command.
        let stale_marker = format!("slopctl tmux-hook {}", hook_name);
        let mut stale_indices: Vec<i32> = Vec::new();
        for line in existing.lines() {
            let prefix_bracket = format!("{}[", hook_name);
            if !line.starts_with(&prefix_bracket) {
                continue;
            }
            // Check if this is a slopctl tmux-hook command (but not ours).
            let is_slopctl_hook = line.contains(&stale_marker)
                && !line.contains(&our_command);
            if !is_slopctl_hook {
                continue;
            }
            // Extract the array index from "hook-name[N] ...".
            if let Some(idx_str) = line.strip_prefix(&prefix_bracket)
                .and_then(|s| s.split(']').next())
                && let Ok(idx) = idx_str.parse::<i32>() {
                    stale_indices.push(idx);
                }
        }

        // Remove stale entries in reverse order so indices stay valid.
        stale_indices.sort_unstable();
        for &idx in stale_indices.iter().rev() {
            let indexed_name = format!("{}[{}]", hook_name, idx);
            let _ = tmux(config)
                .args(["set-hook", "-u", "-t", &session, &indexed_name])
                .output()
                .await;
        }

        // Append our hook.
        if let Err(e) = tmux(config)
            .args(["set-hook", "-a", "-t", &session, hook_name, &our_command])
            .status()
            .await
        {
            warn!("failed to set tmux hook {}: {}", hook_name, e);
        }
    }
}

type SessionLock = Arc<Mutex<()>>;

/// Recreate the slopd tmux session (server + session + hooks) under the lock.
async fn recreate_slopd_session(config: &libslop::SlopdConfig, session_lock: &SessionLock) {
    let _guard = session_lock.lock().await;

    // Start the server if needed (it may have exited entirely).
    if config.tmux.should_start_server() {
        let _ = tmux(config)
            .arg("start-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }

    // Check again under the lock — another task may have already recreated it.
    let session = config.tmux.session();
    let has_session = tmux(config)
        .args(["has-session", "-t", &session])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
    if matches!(has_session, Ok(s) if s.success()) {
        return;
    }

    info!("slopd tmux session is gone, recreating");
    let _ = tmux(config)
        .args(["new-session", "-d", "-s", &session])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
    let _ = tmux(config)
        .args(["set-option", "-t", &session, libslop::TmuxOption::SlopdManaged.as_str(), "true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
    register_tmux_hooks(config).await;
}

/// Check whether a failed tmux output indicates the server or session is gone.
fn is_tmux_session_gone(output: &std::process::Output) -> bool {
    if output.status.success() {
        return false;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr.contains("no server running on")
        || stderr.contains("can't find session:")
        || stderr.contains("can't find window:")
}

/// Run a tmux command that targets the slopd session.  If it fails because
/// the server or session is gone, recreate under the lock and retry once.
async fn tmux_session_output(
    config: &libslop::SlopdConfig,
    session_lock: &SessionLock,
    build_cmd: impl Fn(&libslop::SlopdConfig) -> tokio::process::Command,
) -> std::io::Result<std::process::Output> {
    let output = build_cmd(config).output().await?;
    if !is_tmux_session_gone(&output) {
        return Ok(output);
    }
    recreate_slopd_session(config, session_lock).await;
    build_cmd(config).output().await
}

/// Reconcile managed_panes against live tmux panes, emitting PaneDestroyed
/// for any managed pane that no longer exists.
async fn reconcile_panes(
    config: &libslop::SlopdConfig,
    panes: &PaneMap,
    managed_panes: &ManagedPanes,
    event_tx: &EventTx,
) {
    let session = config.tmux.session();
    // Pull pane_dead/pane_dead_status alongside the id: a pane we set
    // remain-on-exit on does NOT vanish when its process exits — it lingers as a
    // DEAD pane (still listed here) with its final screen frozen. We must tell
    // that apart from a pane tmux no longer lists at all.
    let output = tmux(config)
        .args(["list-panes", "-s", "-t", &session, "-F", "#{pane_id} #{pane_dead} #{pane_dead_status}"])
        .output()
        .await;
    let (present_ids, dead_panes): (std::collections::HashSet<String>, std::collections::HashMap<String, Option<i64>>) = match output {
        Ok(out) if out.status.success() => {
            parse_list_panes(&String::from_utf8_lossy(&out.stdout))
        }
        Ok(out) if {
            let stderr = String::from_utf8_lossy(&out.stderr);
            stderr.contains("no server running on")
                || stderr.contains("can't find session:")
        } => {
            // Server or session is gone — all managed panes are dead.
            (std::collections::HashSet::new(), std::collections::HashMap::new())
        }
        _ => return,
    };

    // Test hook: simulate the production failure mode where `tmux list-panes`
    // transiently returned without our managed panes.  Used by the reconcile
    // false-positive regression test.
    let (present_ids, dead_panes) = if std::env::var("SLOPD_TEST_RECONCILE_FORCE_EMPTY").is_ok() {
        (std::collections::HashSet::new(), std::collections::HashMap::new())
    } else {
        (present_ids, dead_panes)
    };

    let managed = managed_panes.snapshot();

    // Path 1: managed panes that exited and are lingering as DEAD panes (thanks
    // to the remain-on-exit we set at spawn). Capture their frozen final screen
    // and exit status to explain the death, emit an enriched PaneDestroyed, then
    // kill-pane to clear the husk.
    for pane_id in &managed {
        if let Some(exit_status) = dead_panes.get(pane_id).copied() {
            handle_dead_pane(config, panes, managed_panes, event_tx, pane_id, exit_status).await;
        }
    }

    // Path 2: managed panes tmux no longer lists at all. This is the original
    // vanished-pane path — a pane that disappeared without lingering (force-killed,
    // remain-on-exit somehow unset, or the whole server/session gone). Dead panes
    // are in `present_ids`, so they are excluded here and handled in Path 1 above.
    let candidates: Vec<String> = managed
        .into_iter()
        .filter(|id| !present_ids.contains(id))
        .collect();

    for pane_id in candidates {
        // The session-scoped list-panes call above can transiently fail to
        // include a still-alive pane: the slopd session may be briefly missing
        // (recreated between ticks), tmux may return "can't find session:"
        // during a concurrent operation, or the result may be otherwise
        // incomplete.  Once we wrongly call `managed_panes.remove(...)`, the
        // pane is permanently disowned for the rest of this slopd's lifetime
        // — Send/Interrupt/Tag all reject it, and hooks from it are dropped.
        // Verify per-pane via show-options before declaring death.  Pane IDs
        // are global to the tmux server, so this works regardless of which
        // session the pane currently lives in.
        if pane_is_still_alive(config, &pane_id).await {
            continue;
        }

        info!("pane {} no longer exists, emitting PaneDestroyed", pane_id);
        reparent_children_of(config, managed_panes, &pane_id).await;
        if let Some(state) = panes.remove(&pane_id) {
            state.transcript_cancel.lock().unwrap().cancel();
        }
        managed_panes.remove(&pane_id);
        let _ = event_tx.send(libslop::Record {
            source: "slopd".to_string(),
            event_type: "PaneDestroyed".to_string(),
            pane_id: Some(pane_id.clone()),
            payload: serde_json::json!({
                "pane_id": pane_id,
            }),
            cursor: None,
        });
    }
}

/// Parse `list-panes -F '#{pane_id} #{pane_dead} #{pane_dead_status}'` output
/// into (every listed pane id, map of dead pane id -> exit status). A pane id
/// never contains whitespace and the two flags are integers, so splitting on
/// whitespace is unambiguous. `pane_dead_status` is empty for a live pane and is
/// only read when `pane_dead` is 1.
fn parse_list_panes(
    stdout: &str,
) -> (std::collections::HashSet<String>, std::collections::HashMap<String, Option<i64>>) {
    let mut present = std::collections::HashSet::new();
    let mut dead = std::collections::HashMap::new();
    for line in stdout.lines() {
        let mut parts = line.split_whitespace();
        let Some(id) = parts.next() else { continue };
        present.insert(id.to_string());
        if parts.next() == Some("1") {
            let status = parts.next().and_then(|s| s.parse::<i64>().ok());
            dead.insert(id.to_string(), status);
        }
    }
    (present, dead)
}

/// Handle a managed pane that exited and is lingering as a DEAD pane because we
/// set remain-on-exit on it at spawn. Capture its frozen final screen and exit
/// status — the whole point of remain-on-exit — emit an enriched PaneDestroyed
/// carrying them so `slopctl run` can explain WHY the pane died, then kill-pane
/// to clear the husk. Mirrors the vanished-pane cleanup (reparent children,
/// cancel the transcript tail, drop from both maps) so internal state stays
/// consistent regardless of which death path fired.
async fn handle_dead_pane(
    config: &libslop::SlopdConfig,
    panes: &PaneMap,
    managed_panes: &ManagedPanes,
    event_tx: &EventTx,
    pane_id: &str,
    exit_status: Option<i64>,
) {
    // Capture the final screen BEFORE killing the pane. remain-on-exit froze it
    // at the instant the process exited, so this is exactly what the user would
    // have seen — typically the startup error claude printed before bailing.
    //
    // Crucially we include scrollback (`-S -200`): when a pane dies, tmux renders
    // its own "Pane is dead" line on the visible screen and the process's actual
    // final output ends up just above it in history. Capturing only the visible
    // screen loses the very lines we want; the scrollback window recovers them.
    // `dead_pane_output_tail` then trims the padding/footer down to the tail.
    let captured = tmux(config)
        .args(["capture-pane", "-t", pane_id, "-p", "-S", "-200"])
        .output()
        .await;
    let output_tail = match captured {
        Ok(out) if out.status.success() => {
            dead_pane_output_tail(&String::from_utf8_lossy(&out.stdout))
        }
        _ => String::new(),
    };

    info!(
        "managed pane {} exited (status {:?}); emitting PaneDestroyed with {} bytes of captured output",
        pane_id, exit_status, output_tail.len(),
    );
    reparent_children_of(config, managed_panes, pane_id).await;
    if let Some(state) = panes.remove(pane_id) {
        state.transcript_cancel.lock().unwrap().cancel();
    }
    managed_panes.remove(pane_id);

    let mut payload = serde_json::json!({ "pane_id": pane_id });
    if let Some(code) = exit_status {
        payload["exit_status"] = serde_json::json!(code);
    }
    if !output_tail.is_empty() {
        payload["output"] = serde_json::json!(output_tail);
    }
    let _ = event_tx.send(libslop::Record {
        source: "slopd".to_string(),
        event_type: "PaneDestroyed".to_string(),
        pane_id: Some(pane_id.to_string()),
        payload,
        cursor: None,
    });

    // Clear the husk now that we've captured everything we need from it.
    let _ = tmux(config)
        .args(["kill-pane", "-t", pane_id])
        .output()
        .await;
}

/// Reduce tmux's `capture-pane -p` dump of a dead pane to a small, meaningful
/// tail for the PaneDestroyed payload. capture-pane returns the whole visible
/// grid (mostly blank padding) plus tmux's own "Pane is dead (status N, <date>)"
/// footer; drop that footer (the exit status is reported separately) and the
/// blank padding, then bound the result to `MAX_LINES`/`MAX_BYTES` so the
/// broadcast Record stays small.
fn dead_pane_output_tail(captured: &str) -> String {
    const MAX_LINES: usize = 40;
    const MAX_BYTES: usize = 4096;
    let lines: Vec<&str> = captured
        .lines()
        .map(|l| l.trim_end())
        .filter(|l| !l.trim_start().starts_with("Pane is dead (status "))
        .collect();
    let (Some(first), Some(last)) = (
        lines.iter().position(|l| !l.is_empty()),
        lines.iter().rposition(|l| !l.is_empty()),
    ) else {
        return String::new();
    };
    let meaningful = &lines[first..=last];
    let tail = if meaningful.len() > MAX_LINES {
        &meaningful[meaningful.len() - MAX_LINES..]
    } else {
        meaningful
    };
    let mut out = tail.join("\n");
    if out.len() > MAX_BYTES {
        // Keep the most recent output (the tail end). Advance to a char boundary
        // so we never slice through a multi-byte sequence.
        let cut = out.len() - MAX_BYTES;
        let cut = (cut..=out.len()).find(|&i| out.is_char_boundary(i)).unwrap_or(out.len());
        out = out[cut..].to_string();
    }
    out
}

/// Confirm that `pane_id` is still alive in tmux and still flagged as
/// slopd-managed.  Returns `true` if show-options succeeds and reports
/// `@slopd_managed=true`.  Returns `false` when tmux confirms the pane is
/// gone (stderr signalling "no such pane:" / "can't find pane:") or when
/// `@slopd_managed` has been cleared.  On ambiguous errors (e.g. tmux
/// unavailable, unknown stderr) we return `true` to err on the side of
/// keeping the pane managed — the next reconcile tick will retry, which is
/// far cheaper than the alternative of permanently disowning a live pane.
async fn pane_is_still_alive(config: &libslop::SlopdConfig, pane_id: &str) -> bool {
    let out = tmux(config)
        .args(["show-options", "-t", pane_id, "-p"])
        .output()
        .await;
    match out {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            parse_pane_options(&stdout).slopd_managed
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // tmux phrasing varies by version: "no such pane:", "can't find pane:".
            // Anything else is treated as a transient/ambiguous error and the
            // pane is kept (caller will retry next tick).
            !(stderr.contains("no such pane:") || stderr.contains("can't find pane:"))
        }
        Err(_) => true,
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Resolve the config file path once (CLI override or default) so the initial
    // load and every SIGHUP reload read the same file.
    let config_path = cli
        .config
        .as_deref()
        .map(libslop::expand_path)
        .unwrap_or_else(libslop::SlopdConfig::config_path);
    let mut config = libslop::SlopdConfig::load_from(&config_path);

    let verbosity = cli.verbose.max(config.verbose);
    let level = libslop::verbosity_to_level(verbosity);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level.as_str())),
        )
        .with_writer(std::io::stderr)
        .init();
    // Apply CLI overrides and the slopctl path resolution to the initial
    // config; capture as a closure so SIGHUP can re-apply the same massaging
    // to a freshly-loaded config.
    let executable_override = cli.executable.clone();
    let socket_override = cli.socket.as_deref().map(libslop::expand_path);
    let apply_overrides = move |cfg: &mut libslop::SlopdConfig| {
        if let Some(executable) = executable_override.clone() {
            cfg.run.executable = Some(if executable.len() == 1 {
                libslop::Executable::String(executable.into_iter().next().unwrap())
            } else {
                libslop::Executable::Array(executable)
            });
        }
        cfg.run.slopctl = libslop::resolve_slopctl(&cfg.run.slopctl);
        cfg.control_socket = socket_override.clone();
    };
    apply_overrides(&mut config);

    if let Some(CliCommand::UninjectHooks) = cli.command {
        // Clean every dir slopd might have injected into: the default plus all
        // configured accounts.
        let mut failed = false;
        for settings_path in config.all_settings_paths() {
            if let Err(e) = libslop::remove_hooks_from_file(&settings_path) {
                error!("failed to remove hooks from {}: {}", settings_path.display(), e);
                failed = true;
            } else {
                info!("removed slopctl hooks from {}", settings_path.display());
            }
        }
        if failed {
            std::process::exit(1);
        }
        return;
    }

    let initial_config = Arc::new(config);
    // Watch channel lets SIGHUP swap the live config atomically. Every code
    // path that needs the current config snapshots `config_rx.borrow().clone()`
    // at the moment it dispatches work; in-flight operations keep their
    // existing Arc snapshot for consistency.
    let (config_tx, config_rx) = tokio::sync::watch::channel::<Arc<libslop::SlopdConfig>>(initial_config.clone());
    // Counter bumped on every successful reload so callers can wait deterministically
    // for SIGHUP to take effect (exposed via Status.config_generation).
    let config_generation: Arc<std::sync::atomic::AtomicU64> = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let config = initial_config;

    if config.tmux.should_start_server() {
        tmux(&config)
            .arg("start-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .expect("failed to run tmux start-server");
    } else {
        let status = tmux(&config)
            .arg("list-sessions")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .expect("failed to run tmux");
        if !status.success() {
            error!("tmux is not running");
            std::process::exit(1);
        }
    }

    // Create the slopd session if it doesn't exist
    let session = config.tmux.session();
    let has_session = tmux(&config)
        .args(["has-session", "-t", &session])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .expect("failed to run tmux has-session");
    // Whether slopd's tmux session already existed. False means we are starting
    // into a fresh tmux server (the common case after a reboot, which wipes the
    // server) — the trigger for restoring panes from the on-disk manifest. True
    // means a daemon restart against a surviving server, where load_managed_panes
    // already recovers panes from tmux and restoring from disk would duplicate them.
    let session_existed = has_session.success();
    if !has_session.success() {
        tmux(&config)
            .args(["new-session", "-d", "-s", &session])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .expect("failed to create slopd tmux session");
    }

    // Mark the session with a user option so it can be identified
    tmux(&config)
        .args(["set-option", "-t", &session, libslop::TmuxOption::SlopdManaged.as_str(), "true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .expect("failed to set @slopd_managed option on tmux session");

    register_tmux_hooks(&config).await;

    let socket_path = config.control_socket_path();
    let socket_dir = socket_path.parent().unwrap();

    tokio::fs::create_dir_all(&socket_dir).await.unwrap();

    let lock_path = socket_path.with_extension("lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        // Advisory lock file: flock'd, never written, so never truncated.
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .unwrap_or_else(|e| panic!("failed to open lock file {}: {}", lock_path.display(), e));
    let lock_result = unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&lock_file), libc::LOCK_EX | libc::LOCK_NB) };
    if lock_result != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            error!("slopd is already running (lock file held: {})", lock_path.display());
            std::process::exit(1);
        }
        panic!("flock failed: {}", err);
    }

    let start_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let panes = PaneMap::new();
    let managed_panes = ManagedPanes::new();
    let pane_registered: PaneRegistered = Arc::new(tokio::sync::Notify::new());

    let (event_tx, _) = tokio::sync::broadcast::channel::<libslop::Record>(256);
    let event_tx: EventTx = Arc::new(event_tx);

    // Serializes tmux session-mutating operations (new-window, restore spawns).
    let session_lock: SessionLock = Arc::new(Mutex::new(()));

    // Backup/restore configuration, resolved once from the initial config. The
    // two automatic behaviours are independent; manual backup/restore (via the
    // RPC) ignore them.
    let auto_backup = config.backup.auto_backup;
    let auto_restore = config.backup.auto_restore;
    let manifest_path = config.backup.manifest_path();
    let pending_restore: PendingRestore = Arc::new(std::sync::Mutex::new(None));

    // Recover managed pane IDs from the tmux session so panes that existed
    // before a slopd restart are still recognized. This must happen before
    // binding the socket so that clients cannot create panes in the slopd
    // session while the scan is in progress.
    let recovered_settings_paths = load_managed_panes(&config, &managed_panes, &event_tx, &panes).await;

    // Re-inject hooks if there are recovered panes — the previous slopd instance
    // removed them on exit, but the Claude sessions are still running in tmux.
    // Each recovered pane reports its account, so we re-inject only the dirs that
    // are actually in use rather than every configured account.
    for settings_path in &recovered_settings_paths {
        if let Err(e) = libslop::inject_hooks_into_file(settings_path, &config.hook_slopctl()) {
            warn!("failed to re-inject hooks into {}: {}", settings_path.display(), e);
        }
    }

    // Decide what to do with the on-disk manifest on this start.
    //
    // `!session_existed` (we had to create the tmux session) is the post-reboot
    // case, where the manifest is the only surviving record: auto_restore
    // re-spawns the panes, otherwise we hold a *pending restore*. A surviving
    // session is a mere daemon restart — load_managed_panes already recovered
    // the live panes, so we don't restore — EXCEPT when a pending restore was
    // left unresolved before this restart: the `.pending` marker tells us to
    // re-enter the pending state so the preserved manifest isn't clobbered by
    // auto-backup resuming. (Without the marker, the in-memory pending flag would
    // be lost on a daemon restart.)
    let marker_path = config.backup.pending_marker_path();
    let marker_exists = tokio::fs::metadata(&marker_path).await.is_ok();
    let manifest = read_pane_manifest(&manifest_path).await;
    let count = manifest.len();
    let enter_pending = if count == 0 {
        false
    } else if !session_existed {
        if !auto_restore {
            true
        } else if !restore_executable_available(&config) {
            // Don't spawn panes that will die instantly and let the empty live
            // set clobber the manifest. Preserve the restore point and tell the
            // user how to fix it. (The post-reboot PATH failure mode.)
            error!(
                "backup: cannot auto-restore {} pane(s) — configured executable {:?} not found on slopd's PATH. \
                 systemd user services start with a minimal PATH (no ~/.local/bin); add a PATH drop-in for slopd.service. \
                 Manifest preserved and auto-backup paused — fix PATH, then run `slopctl restore`.",
                count,
                config.run.executable.as_ref().map(|e| e.program()).unwrap_or("claude"),
            );
            true
        } else {
            info!("backup: fresh tmux session; restoring {} pane(s) from {}", count, manifest_path.display());
            restore_panes(&config, &managed_panes, &panes, &event_tx, &pane_registered, &session_lock, manifest).await;
            false
        }
    } else {
        // Daemon restart: only pending if a previous pending was unresolved.
        marker_exists
    };

    if enter_pending {
        *pending_restore.lock().unwrap() = Some(count);
        if let Err(e) = tokio::fs::write(&marker_path, count.to_string()).await {
            warn!("backup: failed to persist pending-restore marker {}: {}", marker_path.display(), e);
        }
        info!("backup: {} pane(s) from a previous session can be restored — run `slopctl restore` (auto-backup paused until then; see `slopctl status`)", count);
    } else {
        // Not pending — clear any stale marker (restored, empty, or resolved).
        let _ = tokio::fs::remove_file(&marker_path).await;
    }

    let _ = tokio::fs::remove_file(&socket_path).await;

    let listener = UnixListener::bind(&socket_path).unwrap();
    info!("listening on {}", socket_path.display());

    let mut sigterm = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::terminate(),
    ).expect("failed to install SIGTERM handler");
    let mut sigint = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::interrupt(),
    ).expect("failed to install SIGINT handler");
    let mut sighup = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::hangup(),
    ).expect("failed to install SIGHUP handler");

    // Background task: periodically reconcile managed_panes against live tmux
    // panes to detect panes that exited without going through slopctl kill.
    // This catches cases that tmux session-scope hooks cannot (e.g. process
    // exit, which only fires pane-scope hooks).
    let reconcile_config_rx = config_rx.clone();
    let reconcile_panes_map = panes.clone();
    let reconcile_managed = managed_panes.clone();
    let reconcile_tx = event_tx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            interval.tick().await;
            // Snapshot the current config for this reconcile pass.
            let config_snapshot = reconcile_config_rx.borrow().clone();
            reconcile_panes(&config_snapshot, &reconcile_panes_map, &reconcile_managed, &reconcile_tx).await;
        }
    });

    // Periodic auto-backup. Driven from the main select loop (not a spawned task)
    // so it can never run concurrently with the shutdown backup, keeping the
    // temp-file write in backup_panes race-free.
    let mut backup_interval = tokio::time::interval(
        std::time::Duration::from_secs(config.backup.interval_secs.max(1)),
    );

    loop {
        tokio::select! {
            _ = backup_interval.tick(), if auto_backup => {
                // Skip while a restore is pending so the preserved manifest (the
                // restore point) isn't clobbered by the empty/diverged live set.
                let pending = pending_restore.lock().unwrap().is_some();
                if !pending {
                    let config_snapshot = config_rx.borrow().clone();
                    backup_panes(&config_snapshot, &managed_panes, &manifest_path).await;
                }
            }
            result = listener.accept() => {
                let (stream, _addr) = result.unwrap();
                debug!("accepted connection");
                let config_snapshot = config_rx.borrow().clone();
                tokio::spawn(handle_connection(stream, start_time, config_snapshot, panes.clone(), managed_panes.clone(), event_tx.clone(), pane_registered.clone(), session_lock.clone(), config_generation.clone(), pending_restore.clone()));
            }
            _ = sigterm.recv() => {
                info!("received SIGTERM, shutting down");
                break;
            }
            _ = sigint.recv() => {
                info!("received SIGINT, shutting down");
                break;
            }
            _ = sighup.recv() => {
                let path = config_path.clone();
                match libslop::SlopdConfig::try_load_from(&path) {
                    Ok(mut new_config) => {
                        apply_overrides(&mut new_config);
                        let _ = config_tx.send(Arc::new(new_config));
                        let new_gen = config_generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        info!("reloaded config from {} (generation {})", path.display(), new_gen);
                    }
                    Err(e) => {
                        warn!("SIGHUP: failed to reload config from {} (keeping previous config): {}", path.display(), e);
                    }
                }
            }
        }
    }

    // Final auto-backup on clean shutdown, so the manifest reflects the very
    // latest pane set rather than being up to interval_secs stale. Runs only
    // after the select loop has exited, so it never races the periodic backup.
    // Skipped while a restore is pending, so an unresolved restore point survives
    // another shutdown (e.g. a second reboot before the user restored).
    let restore_pending = pending_restore.lock().unwrap().is_some();
    if auto_backup && !restore_pending {
        let config_snapshot = config_rx.borrow().clone();
        backup_panes(&config_snapshot, &managed_panes, &manifest_path).await;
    }

    // Use the latest config for the shutdown hook cleanup. If a config dir
    // changed at reload time hooks may linger in the previous path — that's a
    // documented limitation of mid-run config reloads.
    let shutdown_config = config_rx.borrow().clone();
    for settings_path in shutdown_config.all_settings_paths() {
        if let Err(e) = libslop::remove_hooks_from_file(&settings_path) {
            warn!("failed to remove hooks from {} on shutdown: {}", settings_path.display(), e);
        } else {
            info!("removed slopctl hooks from {}", settings_path.display());
        }
    }
}

async fn write_response(
    writer: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    id: u64,
    body: libslop::ResponseBody,
) -> std::io::Result<()> {
    let response = libslop::Response { id, body };
    let mut json = serde_json::to_string(&response).unwrap();
    trace!("sending: {}", json);
    json.push('\n');
    writer.lock().await.write_all(json.as_bytes()).await
}

/// Deduplication state for SubscribeTranscript: skip transcript records for the
/// given pane whose byte offset is below the file-end position at replay time.
struct Dedup {
    pane_id: String,
    file_end_offset: u64,
}

/// Stream broadcast records to a subscriber, applying filters and optional dedup.
async fn stream_events(
    rx: &mut tokio::sync::broadcast::Receiver<libslop::Record>,
    writer: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    id: u64,
    filters: &[libslop::EventFilter],
    dedup: Option<&Dedup>,
) -> std::io::Result<()> {
    loop {
        match rx.recv().await {
            Ok(record) => {
                // Skip transcript records that were already replayed from disk.
                if let Some(dedup) = dedup
                    && record.source == "transcript"
                        && record.pane_id.as_deref() == Some(&dedup.pane_id)
                        && record.cursor.is_some_and(|o| o < dedup.file_end_offset)
                    {
                        continue;
                    }
                if filters_match(filters, &record) {
                    write_response(writer, id, libslop::ResponseBody::Record(record)).await?;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                warn!("subscriber lagged, dropped {} events", n);
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                return Ok(());
            }
        }
    }
}

/// Owned version of stream_events for spawning as a task.
/// Takes owned filters and dedup so it can be 'static.
/// Respects the cancellation token for clean shutdown.
async fn stream_events_owned(
    mut rx: tokio::sync::broadcast::Receiver<libslop::Record>,
    writer: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    id: u64,
    filters: Vec<libslop::EventFilter>,
    dedup: Option<Dedup>,
    cancel: tokio_util::sync::CancellationToken,
) {
    tokio::select! {
        _ = cancel.cancelled() => {}
        result = stream_events(&mut rx, &writer, id, &filters, dedup.as_ref()) => {
            let _ = result;
        }
    }
}

#[allow(clippy::too_many_arguments)] // wiring fn threading shared daemon state
async fn handle_connection(
    stream: tokio::net::UnixStream,
    start_time: u64,
    config: Arc<libslop::SlopdConfig>,
    panes: PaneMap,
    managed_panes: ManagedPanes,
    event_tx: EventTx,
    pane_registered: PaneRegistered,
    session_lock: SessionLock,
    config_generation: Arc<std::sync::atomic::AtomicU64>,
    pending_restore: PendingRestore,
) {
    let (reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(writer));
    let mut lines = BufReader::new(reader).lines();
    // Track active subscriptions so they can be cancelled via Unsubscribe
    // and so that they are reaped when the connection closes (otherwise the
    // background `stream_events_owned` task leaks until an event causes its
    // next write to fail).
    //
    // Held inside a cancel-on-drop guard so any subscriptions still alive when
    // `handle_connection` returns (clean EOF, broken pipe, parse error, slopctl
    // crash, etc.) get their background tasks cancelled.
    struct SubscriptionGuard {
        subscriptions: std::collections::HashMap<u64, tokio_util::sync::CancellationToken>,
    }
    impl Drop for SubscriptionGuard {
        fn drop(&mut self) {
            for (_, cancel) in self.subscriptions.drain() {
                cancel.cancel();
            }
        }
    }
    let mut guard = SubscriptionGuard { subscriptions: std::collections::HashMap::new() };
    let subscriptions = &mut guard.subscriptions;

    while let Ok(Some(line)) = lines.next_line().await {
        trace!("received: {}", line);
        let req = match serde_json::from_str::<libslop::Request>(&line) {
            Ok(req) => req,
            Err(e) => {
                warn!("failed to parse request: {}", e);
                let _ = write_response(&writer, 0, libslop::ResponseBody::Error { message: e.to_string() }).await;
                continue;
            }
        };

        match req.body {
            libslop::RequestBody::Subscribe { filters } => {
                let rx = event_tx.subscribe();
                if write_response(&writer, req.id, libslop::ResponseBody::Subscribed).await.is_err() {
                    return;
                }
                // Spawn event streaming as a background task so the read
                // loop can continue processing further requests.
                let cancel = tokio_util::sync::CancellationToken::new();
                subscriptions.insert(req.id, cancel.clone());
                tokio::spawn(stream_events_owned(rx, Arc::clone(&writer), req.id, filters, None, cancel));
            }

            libslop::RequestBody::SubscribeTranscript { pane_id, last_n } => {
                // Step 1: Subscribe to broadcast FIRST to avoid gaps.
                let rx = event_tx.subscribe();

                // Step 2: Read last N records from the transcript file on disk.
                let transcript_path = panes
                    .get(&pane_id)
                    .and_then(|state| state.transcript_path.lock().unwrap().clone());

                let (records, file_end_offset) = match transcript_path {
                    Some(ref path) => {
                        let path = std::path::PathBuf::from(path);
                        match read_transcript_tail(&path, last_n).await {
                            Ok((records, file_len)) => (records, file_len),
                            Err(e) => {
                                warn!("failed to read transcript for replay: {}", e);
                                (vec![], 0)
                            }
                        }
                    }
                    None => (vec![], 0),
                };

                // Step 3: Send Subscribed confirmation.
                if write_response(&writer, req.id, libslop::ResponseBody::Subscribed).await.is_err() {
                    return;
                }

                // Step 4: Send replayed records.
                for (cursor, payload) in &records {
                    let record_type = payload
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let record = libslop::Record {
                        cursor: Some(*cursor),
                        source: "transcript".to_string(),
                        event_type: record_type,
                        pane_id: Some(pane_id.clone()),
                        payload: payload.clone(),
                    };
                    if write_response(&writer, req.id, libslop::ResponseBody::Record(record)).await.is_err() {
                        return;
                    }
                }

                // Step 5: Send ReplayEnd as a Record.
                let replay_end = libslop::Record {
                    cursor: None,
                    source: "slopd".to_string(),
                    event_type: "ReplayEnd".to_string(),
                    pane_id: Some(pane_id.clone()),
                    payload: serde_json::Value::Null,
                };
                if write_response(&writer, req.id, libslop::ResponseBody::Record(replay_end)).await.is_err() {
                    return;
                }

                // Step 6: Spawn live event streaming as a background task,
                // skipping transcript records already replayed.
                let transcript_filter = vec![libslop::EventFilter {
                    source: Some("transcript".to_string()),
                    pane_id: Some(pane_id.clone()),
                    ..Default::default()
                }];
                let dedup = Some(Dedup { pane_id, file_end_offset });
                let cancel = tokio_util::sync::CancellationToken::new();
                subscriptions.insert(req.id, cancel.clone());
                tokio::spawn(stream_events_owned(rx, Arc::clone(&writer), req.id, transcript_filter, dedup, cancel));
            }

            libslop::RequestBody::Unsubscribe { subscription_id } => {
                if let Some(cancel) = subscriptions.remove(&subscription_id) {
                    cancel.cancel();
                    let _ = write_response(&writer, req.id, libslop::ResponseBody::Unsubscribed { subscription_id }).await;
                } else {
                    let _ = write_response(&writer, req.id, libslop::ResponseBody::Error {
                        message: format!("no active subscription with id {}", subscription_id),
                    }).await;
                }
            }

            body => {
                let body = handle_request(body, start_time, &config, &panes, &managed_panes, &event_tx, &pane_registered, &session_lock, &config_generation, &pending_restore).await;
                if write_response(&writer, req.id, body).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Parsed pane options from tmux `show-options -p` output.
struct ParsedPaneOptions {
    slopd_managed: bool,
    session_id: Option<String>,
    /// Full ancestor chain (immediate parent first). Stored as @slopd_ancestor_panes.
    ancestor_panes: Vec<String>,
    tags: Vec<String>,
    detailed_state: Option<libslop::PaneDetailedState>,
    created_at: Option<u64>,
    /// Account the pane was launched under (@slopd_account); None when unset.
    account: Option<String>,
    /// Path to the pane's Claude transcript (@slopd_transcript_path); None unset.
    transcript_path: Option<String>,
    /// Pane backend (@slopd_backend); None/unset = Claude (the default).
    backend: Option<libslop::Backend>,
    /// For opencode panes: the embedded server port (@slopd_opencode_port).
    opencode_port: Option<u16>,
    /// For opencode panes: the per-pane auth token (@slopd_opencode_token).
    opencode_token: Option<String>,
}

impl ParsedPaneOptions {
    /// Derive parent_pane_id from the first ancestor.
    fn parent_pane_id(&self) -> Option<String> {
        self.ancestor_panes.first().cloned()
    }
}

fn parse_pane_options(stdout: &str) -> ParsedPaneOptions {
    let mut slopd_managed = false;
    let mut session_id = None;
    let mut ancestor_panes = Vec::new();
    let mut tags = Vec::new();
    let mut detailed_state = None;
    let mut created_at = None;
    let mut account = None;
    let mut transcript_path = None;
    let mut backend = None;
    let mut opencode_port = None;
    let mut opencode_token = None;
    for opt_line in stdout.lines() {
        let mut words = opt_line.splitn(2, ' ');
        let key = words.next().unwrap_or("").trim();
        let val = words.next().unwrap_or("").trim().trim_matches('"');
        if key == libslop::TmuxOption::SlopdManaged.as_str() {
            slopd_managed = val == "true";
        } else if key == libslop::TmuxOption::SlopdClaudeSessionId.as_str() {
            session_id = Some(val.to_string());
        } else if key == libslop::TmuxOption::SlopdAncestorPanes.as_str() {
            ancestor_panes = val.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        } else if key == libslop::TmuxOption::SlopdDetailedState.as_str() {
            detailed_state = libslop::PaneDetailedState::from_str(val);
        } else if key == libslop::TmuxOption::SlopdCreatedAt.as_str() {
            created_at = val.parse::<u64>().ok();
        } else if key == libslop::TmuxOption::SlopdAccount.as_str() {
            account = if val.is_empty() { None } else { Some(val.to_string()) };
        } else if key == libslop::TmuxOption::SlopdTranscriptPath.as_str() {
            transcript_path = if val.is_empty() { None } else { Some(val.to_string()) };
        } else if key == libslop::TmuxOption::SlopdBackend.as_str() {
            backend = match val {
                "opencode" => Some(libslop::Backend::Opencode),
                "claude" => Some(libslop::Backend::Claude),
                _ => None,
            };
        } else if key == libslop::TmuxOption::SlopdOpencodePort.as_str() {
            opencode_port = val.parse::<u16>().ok();
        } else if key == libslop::TmuxOption::SlopdOpencodeToken.as_str() {
            opencode_token = if val.is_empty() { None } else { Some(val.to_string()) };
        } else if let Some(tag) = key.strip_prefix(libslop::TAG_OPTION_PREFIX) {
            tags.push(tag.to_string());
        }
    }
    ParsedPaneOptions { slopd_managed, session_id, ancestor_panes, tags, detailed_state, created_at, account, transcript_path, backend, opencode_port, opencode_token }
}

/// Encode an ancestor list as a comma-separated string for tmux storage.
fn encode_ancestors(ancestors: &[String]) -> String {
    ancestors.join(",")
}

/// Remove `dead_pane_id` from the ancestor chain of every managed pane that
/// references it.  Called from the Kill handler before the pane is destroyed,
/// and also usable for batch cleanup.
async fn reparent_children_of(
    config: &libslop::SlopdConfig,
    managed_panes: &ManagedPanes,
    dead_pane_id: &str,
) {
    for child_id in managed_panes.snapshot() {
        if child_id == dead_pane_id {
            continue;
        }
        // Read this pane's current ancestor chain.
        let Ok(out) = tmux(config)
            .args(["show-options", "-t", &child_id, "-p", "-v",
                   libslop::TmuxOption::SlopdAncestorPanes.as_str()])
            .output()
            .await
        else {
            continue;
        };
        let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let ancestors: Vec<String> = raw.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !ancestors.contains(&dead_pane_id.to_string()) {
            continue;
        }
        // Remove the dead pane from the ancestor chain.
        let new_ancestors: Vec<String> = ancestors.into_iter()
            .filter(|a| a != dead_pane_id)
            .collect();
        if new_ancestors.is_empty() {
            let _ = tmux(config)
                .args(["set-option", "-t", &child_id, "-p", "-u",
                       libslop::TmuxOption::SlopdAncestorPanes.as_str()])
                .output()
                .await;
        } else {
            let encoded = encode_ancestors(&new_ancestors);
            let _ = tmux_set_pane_option(config, &child_id,
                libslop::TmuxOption::SlopdAncestorPanes.as_str(), &encoded).await;
        }
    }
}

async fn list_panes(config: &libslop::SlopdConfig, managed_panes: &ManagedPanes) -> Result<Vec<libslop::PaneInfo>, String> {
    // Iterate slopd's authoritative in-memory managed_panes set, not
    // `tmux list-panes`.  The two are not always equivalent:
    //   - A pane can have @slopd_managed=true set in tmux yet not be in
    //     managed_panes (stale option, manual `tmux new-window`, or a pane
    //     that was reconciled away while still alive in tmux).  Showing such
    //     a pane in `ps` confuses callers because Send/Interrupt/Tag all
    //     reject it.
    //   - managed_panes is what Send/Interrupt/Tag/Kill check, so iterating
    //     it makes `ps` consistent with the operations a caller can perform.
    // Per-pane metadata (activity, cwd, slopd options) still comes from tmux;
    // panes that have died in tmux but are still in managed_panes are skipped
    // here — the next reconcile tick will clean them up.

    struct RawPane {
        pane_id: String,
        last_active: u64,
        working_dir: Option<String>,
        opts: ParsedPaneOptions,
    }
    let mut raw_panes = Vec::new();
    for pane_id in managed_panes.snapshot() {
        let dm_out = tmux(config)
            .args(["display-message", "-p", "-t", &pane_id, "-F",
                   "#{window_activity} #{pane_current_path}"])
            .output()
            .await;
        let (last_active, working_dir) = match dm_out {
            Ok(out) if out.status.success() => {
                let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let mut parts = line.splitn(2, ' ');
                let activity: u64 = parts.next().unwrap_or("0").trim().parse().unwrap_or(0);
                let cwd = parts.next().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
                (activity, cwd)
            }
            _ => continue,
        };

        let opts_out = tmux(config)
            .args(["show-options", "-t", &pane_id, "-p"])
            .output()
            .await;
        let opts = match opts_out {
            Ok(out) if out.status.success() => parse_pane_options(&String::from_utf8_lossy(&out.stdout)),
            _ => continue,
        };

        raw_panes.push(RawPane { pane_id, last_active, working_dir, opts });
    }

    // Build set of live managed pane IDs.
    let live_ids: std::collections::HashSet<String> = raw_panes.iter().map(|p| p.pane_id.clone()).collect();

    // Second pass: reparent any pane whose parent is dead by walking the ancestor chain.
    let mut panes = Vec::new();
    for mut raw in raw_panes {
        let parent_pane_id = raw.opts.parent_pane_id();
        let needs_reparent = parent_pane_id.as_ref().is_some_and(|p| !live_ids.contains(p.as_str()));

        if needs_reparent {
            // Walk ancestors to find the first one that is still alive.
            let new_ancestors: Vec<String> = raw.opts.ancestor_panes.iter()
                .skip_while(|a| !live_ids.contains(a.as_str()))
                .cloned()
                .collect();
            raw.opts.ancestor_panes = new_ancestors;
            // Persist the updated ancestor chain to tmux so it survives slopd restarts.
            let encoded = encode_ancestors(&raw.opts.ancestor_panes);
            if raw.opts.ancestor_panes.is_empty() {
                let _ = tmux(config)
                    .args(["set-option", "-t", &raw.pane_id, "-p", "-u", libslop::TmuxOption::SlopdAncestorPanes.as_str()])
                    .output()
                    .await;
            } else {
                let _ = tmux_set_pane_option(config, &raw.pane_id, libslop::TmuxOption::SlopdAncestorPanes.as_str(), &encoded).await;
            }
        }

        let parent_pane_id = raw.opts.parent_pane_id();
        let detailed_state = raw.opts.detailed_state.unwrap_or(libslop::PaneDetailedState::BootingUp);
        let state = detailed_state.to_simple();
        let created_at = raw.opts.created_at.unwrap_or(raw.last_active);
        // A pane with no recorded account is on the default account (e.g. panes
        // from before this option existed, or the session's idle pane).
        let account = raw.opts.account.unwrap_or_else(|| libslop::DEFAULT_ACCOUNT.to_string());
        let backend = raw.opts.backend.unwrap_or(libslop::Backend::Claude);
        panes.push(libslop::PaneInfo {
            pane_id: raw.pane_id,
            created_at,
            last_active: raw.last_active,
            session_id: raw.opts.session_id,
            parent_pane_id,
            tags: raw.opts.tags,
            state,
            detailed_state,
            working_dir: raw.working_dir,
            transcript_path: raw.opts.transcript_path,
            account,
            backend,
        });
    }
    Ok(panes)
}

/// Write the current managed-pane set to the backup manifest on disk, returning
/// the number of panes recorded.
///
/// Writes to a temp file and atomically renames it into place, so a crash
/// mid-write can never leave a torn manifest. A transient failure to enumerate
/// panes is logged and skipped rather than clobbering a good manifest. Callers
/// must serialize their calls (the daemon does, by auto-backing-up only from the
/// main select loop and once on shutdown) so the shared temp path is safe.
async fn backup_panes(
    config: &libslop::SlopdConfig,
    managed_panes: &ManagedPanes,
    manifest_path: &std::path::Path,
) -> usize {
    let panes = match list_panes(config, managed_panes).await {
        Ok(panes) => panes,
        Err(e) => {
            warn!("backup: failed to enumerate panes, skipping backup: {}", e);
            return 0;
        }
    };
    // Only panes with a recorded Claude session id are restorable; a pane still
    // booting before its first SessionStart has none and would just be skipped on
    // restore. Keep the manifest to resumable panes.
    let panes: Vec<libslop::PaneInfo> =
        panes.into_iter().filter(|p| p.session_id.is_some()).collect();
    let json = match serde_json::to_string_pretty(&panes) {
        Ok(j) => j,
        Err(e) => {
            warn!("backup: failed to serialize pane manifest: {}", e);
            return 0;
        }
    };
    if let Some(parent) = manifest_path.parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await {
            warn!("backup: failed to create manifest dir {}: {}", parent.display(), e);
            return 0;
        }
    // Temp file beside the manifest so the rename stays on one filesystem (atomic).
    let tmp_path = manifest_path.with_extension("json.tmp");
    if let Err(e) = tokio::fs::write(&tmp_path, json.as_bytes()).await {
        warn!("backup: failed to write {}: {}", tmp_path.display(), e);
        return 0;
    }
    if let Err(e) = tokio::fs::rename(&tmp_path, manifest_path).await {
        warn!("backup: failed to rename into {}: {}", manifest_path.display(), e);
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return 0;
    }
    debug!("backup: wrote {} pane(s) to {}", panes.len(), manifest_path.display());
    panes.len()
}

/// Read the backup manifest from disk, returning the panes recorded there.
/// A missing file yields an empty list (nothing to restore); a present-but-
/// unreadable or malformed file is logged and treated as empty.
async fn read_pane_manifest(manifest_path: &std::path::Path) -> Vec<libslop::PaneInfo> {
    let bytes = match tokio::fs::read(manifest_path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            warn!("backup: failed to read manifest {}: {}", manifest_path.display(), e);
            return Vec::new();
        }
    };
    match serde_json::from_slice::<Vec<libslop::PaneInfo>>(&bytes) {
        Ok(panes) => panes,
        Err(e) => {
            warn!("backup: manifest {} is malformed ({}); ignoring", manifest_path.display(), e);
            Vec::new()
        }
    }
}

/// Everything that differs between the two ways slopd launches a Claude pane
/// (`run` and restore). Everything they *share* — resolving the executable to
/// an absolute path and building the `tmux new-window` command — lives in
/// [`spawn_claude_pane`], the single chokepoint both go through.
#[derive(Default)]
struct SpawnSpec {
    /// `-c` working directory for the new pane (also the cwd a relative
    /// executable resolves against). `None` → tmux default.
    working_dir: Option<String>,
    /// Agent config dir for the resolved account, if any. Exported under the
    /// backend's env var (`CLAUDE_CONFIG_DIR` / `OPENCODE_CONFIG_DIR`).
    config_dir: Option<std::path::PathBuf>,
    /// Agent backend in effect (drives the config-dir env var; spawn dispatch
    /// by backend is added in a later phase — for now all panes spawn via the
    /// same tmux path, which is correct for Claude and a no-op stub for opencode
    /// until the OpencodeBackend lands).
    backend: libslop::Backend,
    /// Resolved executable to spawn (program + its own args), from
    /// `ResolvedAccount`. Trailing args below are appended after these.
    executable: libslop::Executable,
    /// Extra `-e KEY=VALUE` for the pane (run only; a PATH entry here also
    /// drives executable resolution). Empty for restore.
    extra_env: Vec<(String, String)>,
    /// Args appended after the executable's own args (run: the user's extra
    /// args; restore: `--resume <session_id>`).
    trailing_args: Vec<String>,
}

/// The single place slopd launches a Claude pane in its tmux session. Resolves
/// the configured executable to an ABSOLUTE path and spawns *that*, so the new
/// pane never depends on its own inherited PATH to find `claude`. That
/// dependency is exactly what made restore silently fail after a reboot
/// (systemd user services start with a minimal PATH that omits `~/.local/bin`,
/// so every restored pane's bare `claude` was not found and it died instantly).
/// Routing both `run` and restore through here means the resolution can't be
/// present on one spawn path and forgotten on the other.
///
/// Returns the new pane id, or an error string if the executable can't be
/// resolved (so the caller can surface it / preserve the manifest) or tmux
/// fails.
async fn spawn_claude_pane(
    config: &Arc<libslop::SlopdConfig>,
    session_lock: &SessionLock,
    spec: &SpawnSpec,
) -> Result<String, String> {
    // Resolve against the pane's effective PATH (a spec PATH override wins, else
    // slopd's) and working dir, matching what the spawned pane would see.
    let lookup_path = spec
        .extra_env
        .iter()
        .rev()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| std::ffi::OsString::from(v))
        .or_else(|| std::env::var_os("PATH"))
        .unwrap_or_default();
    let lookup_cwd = spec
        .working_dir
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let program = spec.executable.program();
    let resolved = libslop::resolve_executable(program, &lookup_path, &lookup_cwd).ok_or_else(|| {
        format!(
            "configured executable {:?} not found — check `[run] executable` / account `executable` (or --executable) and slopd's PATH \
             (systemd user services start with a minimal PATH that omits ~/.local/bin, where `claude` usually lives)",
            program
        )
    })?;

    let xdg_runtime_dir = libslop::runtime_dir();
    let profile_file = std::env::var("LLVM_PROFILE_FILE").ok();

    let output = tmux_session_output(config, session_lock, |c| {
        let mut cmd = tmux(c);
        let session = c.tmux.session();
        // `-d`: create the window in the background so spawning a pane doesn't
        // yank clients already watching the session to it.
        cmd.args(["new-window", "-d", "-t", &session, "-P", "-F", "#{pane_id}"])
            .args(["-e", &format!("XDG_RUNTIME_DIR={}", xdg_runtime_dir.display())])
            .args(["-e", &format!("SLOPCTL={}", c.run.slopctl)]);
        if let Some(ref dir) = spec.working_dir {
            cmd.args(["-c", dir]);
        }
        if let Some(ref dir) = spec.config_dir {
            cmd.args(["-e", &format!("{}={}", spec.backend.config_dir_env_var(), dir.display())]);
        }
        // Forward LLVM_PROFILE_FILE so instrumented child binaries (e.g.
        // mock_claude) write coverage data even when launched in a tmux window.
        if let Some(ref pf) = profile_file {
            cmd.args(["-e", &format!("LLVM_PROFILE_FILE={}", pf)]);
        }
        for (k, v) in &spec.extra_env {
            cmd.args(["-e", &format!("{}={}", k, v)]);
        }
        // Spawn the resolved absolute path, not the bare program name, so the
        // launch never depends on the pane's own PATH.
        cmd.arg(&resolved)
            .args(spec.executable.args())
            .args(&spec.trailing_args);
        cmd
    })
    .await;

    match output {
        Ok(out) if out.status.success() => {
            let pane_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
            // Keep the pane alive as a DEAD pane after its process exits, scoped to
            // THIS pane only (`-p`) so it never leaks onto the user's other windows
            // on the shared default tmux server. A claude that crashes at startup
            // then lingers with its final screen intact for reconcile_panes to
            // capture — that capture is what lets `slopctl run` explain *why* the
            // pane died instead of reporting a contentless death.
            let _ = tmux_set_pane_option(config, &pane_id, "remain-on-exit", "on").await;
            Ok(pane_id)
        }
        Ok(out) => Err(format!(
            "tmux new-window exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => Err(format!("tmux new-window failed: {}", e)),
    }
}

/// Whether the configured Claude executable resolves on slopd's PATH. Used as a
/// pre-flight for the startup restore decision: if it can't be resolved we keep
/// the manifest as a pending restore (rather than spawn panes that fail) until
/// the user fixes their PATH. The actual spawn in [`spawn_claude_pane`] resolves
/// it again to an absolute path, so this only gates *whether* to attempt a
/// restore, never how the executable is located.
fn restore_executable_available(config: &libslop::SlopdConfig) -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let cwd = std::env::current_dir().unwrap_or_default();
    // Restore currently targets Claude panes only (the manifest gains a `backend`
    // field in a later phase); use the global executable or the Claude default.
    let program = config
        .run
        .executable
        .as_ref()
        .map(|e| e.program())
        .unwrap_or("claude");
    libslop::executable_exists(program, &path, &cwd)
}

/// The launch cwd recorded in a Claude transcript: the first record's `cwd`,
/// i.e. the directory Claude was *started* in, which determines the project dir
/// the session is stored under (`~/.claude/projects/<encoded-cwd>/<id>.jsonl`).
/// Restore must `claude --resume` from this dir, not the pane's drifted
/// `pane_current_path`, or claude searches the wrong project and can't find the
/// session (it then starts a fresh one and the pane dies). `None` if the file
/// is unreadable or has no cwd in its first records.
fn transcript_launch_cwd(transcript_path: &str) -> Option<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(transcript_path).ok()?;
    // The cwd appears in the earliest records; scan a bounded prefix so a huge
    // transcript is never read end-to-end.
    for line in std::io::BufReader::new(file).lines().take(50) {
        let Ok(line) = line else { break };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        if let Some(cwd) = value.get("cwd").and_then(|c| c.as_str())
            && !cwd.is_empty()
        {
            return Some(cwd.to_string());
        }
    }
    None
}

/// Re-spawn the panes recorded in `manifest` after a reboot, each via
/// `claude --resume <session_id>` in its original working dir and account.
///
/// Panes are restored parents-first so that ancestry can be remapped from the
/// old (pre-reboot) tmux pane ids to the freshly-assigned ones. Panes with no
/// recorded session id are skipped (nothing to resume). Each spawn is
/// best-effort: a pane whose session can no longer be resumed (e.g. its
/// transcript was deleted) just dies and is cleaned up by the reconciler; it
/// does not abort the rest of the batch.
async fn restore_panes(
    config: &Arc<libslop::SlopdConfig>,
    managed_panes: &ManagedPanes,
    panes: &PaneMap,
    event_tx: &EventTx,
    pane_registered: &PaneRegistered,
    session_lock: &SessionLock,
    manifest: Vec<libslop::PaneInfo>,
) -> usize {
    let total = manifest.len();
    // Only panes with a Claude session id can be resumed.
    let (resumable, skipped): (Vec<libslop::PaneInfo>, Vec<libslop::PaneInfo>) =
        manifest.into_iter().partition(|p| p.session_id.is_some());
    for p in &skipped {
        info!("backup: skipping pane {} (no recorded session id, nothing to resume)", p.pane_id);
    }

    // Pre-compute each pane's depth in the ancestor tree so we can restore
    // parents before children (owned keys so the sort below doesn't conflict
    // with borrows into the vec).
    let id_set: std::collections::HashSet<String> =
        resumable.iter().map(|p| p.pane_id.clone()).collect();
    let parent_of: std::collections::HashMap<String, String> = resumable.iter()
        .filter_map(|p| p.parent_pane_id.clone().map(|par| (p.pane_id.clone(), par)))
        .collect();
    let depth_of: std::collections::HashMap<String, usize> = resumable.iter().map(|p| {
        let mut depth = 0usize;
        let mut cur = p.pane_id.as_str();
        let mut seen = std::collections::HashSet::new();
        while let Some(par) = parent_of.get(cur) {
            // Stop at an ancestor that isn't itself being restored, or a cycle.
            if !id_set.contains(par) || !seen.insert(par.clone()) {
                break;
            }
            depth += 1;
            cur = par.as_str();
        }
        (p.pane_id.clone(), depth)
    }).collect();
    let mut ordered = resumable;
    ordered.sort_by_key(|p| depth_of.get(&p.pane_id).copied().unwrap_or(0));

    // Maps from the manifest's (old) pane ids to the new ids we spawn, and each
    // new pane's remapped ancestor chain (so children can prepend their parent).
    let mut old_to_new: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut ancestors_of_new: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    // Session ids we must not (re-)spawn. Seeded with the sessions of panes
    // already running, so a manual `slopctl restore` on a live daemon never puts
    // a second Claude on an already-open session (for auto-restore into a fresh
    // tmux session there are none). Two manifest entries can also share a session
    // id — e.g. an in-pane `claude --resume` overwrites a pane's
    // @slopd_claude_session_id — so we also add each as we restore it, resuming
    // every session at most once.
    let mut seen_sessions: std::collections::HashSet<String> = list_panes(config, managed_panes)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| p.session_id)
        .collect();
    let mut restored = 0usize;

    for p in ordered {
        let old_id = p.pane_id;
        let account = p.account;
        let created_at = p.created_at;
        let tags = p.tags;
        let parent = p.parent_pane_id;
        let session_id = p.session_id.expect("resumable panes have a session id");
        let working_dir = p.working_dir;
        let transcript_path = p.transcript_path;

        if !seen_sessions.insert(session_id.clone()) {
            info!("backup: skipping pane {} — session {} already running or restored", old_id, session_id);
            continue;
        }

        // Resolve the account (falling back to default if it was removed from
        // config) and inject its hooks before spawning, like the run handler.
        let resolved = config
            .resolve_account(Some(account.as_str()))
            .or_else(|_| config.resolve_account(Some(libslop::DEFAULT_ACCOUNT)))
            .expect("the reserved default account always resolves");
        // OpenCode reboot-restore is not yet wired (it needs a fresh port/token
        // allocation + `-s <id>` spawn + driver re-attach, not `--resume`). Skip
        // rather than mis-spawn; the session survives in the opencode DB and can
        // be resumed manually with `opencode -s <id>`.
        if resolved.backend == libslop::Backend::Opencode {
            warn!("backup: skipping opencode pane {} (session {}) — reboot-restore for opencode is not yet supported", old_id, session_id);
            continue;
        }
        let settings_path = config.resolved_settings_path(&resolved);
        if resolved.backend.uses_injected_hooks() {
            if let Err(e) = libslop::inject_hooks_into_file(&settings_path, &config.hook_slopctl()) {
                warn!("backup: failed to inject hooks into {} for restored pane: {}", settings_path.display(), e);
            }
        }

        // `claude --resume` finds the session via the project dir of its launch
        // cwd (the dir Claude was started in). `working_dir` is pane_current_path,
        // which drifts when the agent `cd`s, so prefer the transcript's recorded
        // launch cwd; fall back to working_dir when there's no transcript.
        let launch_dir = transcript_path
            .as_deref()
            .and_then(transcript_launch_cwd)
            .or_else(|| working_dir.clone());
        let new_id = match spawn_claude_pane(config, session_lock, &SpawnSpec {
            working_dir: launch_dir,
            config_dir: resolved.config_dir.clone(),
            backend: resolved.backend,
            executable: resolved.executable.clone(),
            extra_env: Vec::new(),
            trailing_args: vec!["--resume".to_string(), session_id.clone()],
        }).await {
            Ok(id) => id,
            Err(e) => {
                warn!("backup: failed to restore pane {} (session {}): {}", old_id, session_id, e);
                continue;
            }
        };

        managed_panes.insert(new_id.clone());
        // Wake any hook handler that arrived before the insert (the resumed pane
        // fires SessionStart as soon as the window opens).
        pane_registered.notify_waiters();

        let _ = tmux_set_pane_option(config, &new_id, libslop::TmuxOption::SlopdManaged.as_str(), "true").await;
        // Preserve the original creation time so pane age/ordering survives reboot.
        let _ = tmux_set_pane_option(config, &new_id, libslop::TmuxOption::SlopdCreatedAt.as_str(), &created_at.to_string()).await;
        let _ = tmux_set_pane_option(config, &new_id, libslop::TmuxOption::SlopdAccount.as_str(), &resolved.name).await;
        // Set the session id directly so `ps` is correct immediately; the
        // SessionStart hook will re-set the same id once the session resumes
        // (plain --resume continues the session rather than forking it).
        let _ = tmux_set_pane_option(config, &new_id, libslop::TmuxOption::SlopdClaudeSessionId.as_str(), &session_id).await;

        // Remap ancestry: the parent's new id, prepended to the parent's own
        // (already-remapped) chain. Truncates at any ancestor that wasn't
        // restored, matching reconcile-time reparenting.
        let new_ancestors: Vec<String> = match parent.as_deref().and_then(|par| old_to_new.get(par)) {
            Some(new_parent) => {
                let mut chain = vec![new_parent.clone()];
                if let Some(rest) = ancestors_of_new.get(new_parent) {
                    chain.extend(rest.iter().cloned());
                }
                chain
            }
            None => Vec::new(),
        };
        if !new_ancestors.is_empty() {
            let encoded = encode_ancestors(&new_ancestors);
            let _ = tmux_set_pane_option(config, &new_id, libslop::TmuxOption::SlopdAncestorPanes.as_str(), &encoded).await;
        }
        ancestors_of_new.insert(new_id.clone(), new_ancestors);

        // Re-apply tags.
        for tag in &tags {
            let opt = format!("{}{}", libslop::TAG_OPTION_PREFIX, tag);
            let _ = tmux_set_pane_option(config, &new_id, &opt, "1").await;
        }

        // Initial state; the resumed session's hooks advance it from here.
        set_pane_detailed_state(config, &new_id, &libslop::PaneDetailedState::BootingUp, None, event_tx, panes).await;

        old_to_new.insert(old_id.clone(), new_id.clone());
        restored += 1;
        info!("backup: restored pane {} -> {} (session {})", old_id, new_id, session_id);
    }

    info!("backup: restored {}/{} recorded pane(s)", restored, total);
    restored
}

async fn send_interrupt_keys(config: &libslop::SlopdConfig, pane_id: &str) -> Result<(), libslop::ResponseBody> {
    for key in &["C-c", "C-d", "Escape"] {
        if let Err(e) = tmux_send_keys(config, pane_id, key).await {
            return Err(libslop::ResponseBody::Error { message: e.to_string() });
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // wiring fn threading shared daemon state
async fn handle_request(
    body: libslop::RequestBody,
    start_time: u64,
    config: &Arc<libslop::SlopdConfig>,
    panes: &PaneMap,
    managed_panes: &ManagedPanes,
    event_tx: &EventTx,
    pane_registered: &PaneRegistered,
    session_lock: &SessionLock,
    config_generation: &Arc<std::sync::atomic::AtomicU64>,
    pending_restore: &PendingRestore,
) -> libslop::ResponseBody {
    match body {

        libslop::RequestBody::Status => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            libslop::ResponseBody::Status {
                state: libslop::DaemonState {
                    uptime_secs: now.saturating_sub(start_time),
                    subscriber_count: event_tx.receiver_count() as u64,
                    config_generation: config_generation.load(std::sync::atomic::Ordering::Relaxed),
                    pending_restore: *pending_restore.lock().unwrap(),
                },
            }
        }

        libslop::RequestBody::Kill { pane_id } => {
            if !managed_panes.contains(&pane_id) {
                return libslop::ResponseBody::Error {
                    message: format!("pane {} is not managed by slopd", pane_id),
                };
            }
            // Reparent children before killing: for every managed pane whose ancestor
            // list contains the dying pane, remove it from their ancestor chain.
            reparent_children_of(config, managed_panes, &pane_id).await;
            let output = tmux(config)
                .args(["kill-pane", "-t", &pane_id])
                .output()
                .await;
            // Clean up internal state regardless of whether tmux kill-pane
            // succeeded (the pane may already be dead from process exit).
            match &output {
                Err(e) => {
                    return libslop::ResponseBody::Error { message: e.to_string() };
                }
                Ok(out) if !out.status.success() => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    warn!("tmux kill-pane failed for pane {} (already dead?): {}", pane_id, stderr.trim());
                }
                _ => {}
            }
            if let Some(state) = panes.remove(&pane_id) {
                state.transcript_cancel.lock().unwrap().cancel();
            }
            managed_panes.remove(&pane_id);
            let _ = event_tx.send(libslop::Record {
                source: "slopd".to_string(),
                event_type: "PaneDestroyed".to_string(),
                pane_id: Some(pane_id.clone()),
                payload: serde_json::json!({
                    "pane_id": pane_id,
                }),
                cursor: None,
            });
            libslop::ResponseBody::Kill { pane_id }
        }

        libslop::RequestBody::TmuxHook { event, pane_id } => {
            debug!("tmux-hook: {} pane={:?}", event, pane_id);
            reconcile_panes(config, panes, managed_panes, event_tx).await;
            libslop::ResponseBody::TmuxHooked
        }

        libslop::RequestBody::Hook { event, payload, pane_id } => {
            let Some(pane) = pane_id.as_deref() else {
                debug!("hook: {} ignored (no pane_id)", event);
                return libslop::ResponseBody::Hooked;
            };
            debug!("hook: {} pane={} payload={}", event, pane, payload);

            // Ignore hooks from panes that were not spawned by slopd. This can happen
            // when an external Claude instance shares the same settings.json with
            // injected hooks.
            {
                if !managed_panes.contains(pane) {
                    // The hook might have arrived before the Run handler's
                    // managed_panes.insert() ran (race between tmux creating the pane
                    // and the async task resuming).  Wait briefly for registration.
                    let _ = tokio::time::timeout(PANE_REGISTRATION_WAIT, async {
                        loop {
                            // Create the notified future before re-checking so we don't
                            // miss a notification that fires between the check and the await.
                            let notified = pane_registered.notified();
                            if managed_panes.contains(pane) {
                                return;
                            }
                            notified.await;
                        }
                    })
                    .await;
                    if !managed_panes.contains(pane) {
                        debug!("ignoring hook from unmanaged pane {}", pane);
                        return libslop::ResponseBody::Hooked;
                    }
                }
            }

            // Start (or re-start) tailing the transcript file whenever a hook
            // includes a transcript_path we haven't seen yet for this pane.
            // This covers both SessionStart and any hook fired after a slopd
            // restart where the tailer is no longer running.
            if let Some(transcript_path) = payload.get("transcript_path").and_then(|v| v.as_str()) {
                let state = panes.get_or_insert(pane);
                let already_tailing = state.transcript_path.lock().unwrap().as_deref() == Some(transcript_path);
                if !already_tailing {
                    debug!("hook {}: starting transcript tail for pane {} path={}", event, pane, transcript_path);
                    if let Err(e) = tmux_set_pane_option(config, pane, libslop::TmuxOption::SlopdTranscriptPath.as_str(), transcript_path).await {
                        warn!("failed to set @slopd_transcript_path on pane {}: {}", pane, e);
                    }
                    // Cancel any previous tailer and swap in a fresh token.
                    let new_cancel = tokio_util::sync::CancellationToken::new();
                    {
                        let mut cancel_guard = state.transcript_cancel.lock().unwrap();
                        cancel_guard.cancel();
                        *cancel_guard = new_cancel.clone();
                    }
                    *state.transcript_path.lock().unwrap() = Some(transcript_path.to_string());
                    tokio::spawn(tail_transcript(
                        std::path::PathBuf::from(transcript_path),
                        pane.to_string(),
                        state.clone(),
                        config.clone(),
                        panes.clone(),
                        event_tx.clone(),
                        new_cancel,
                    ));
                }
            }

            // Side effects for specific hooks (not state-related).
            if event == "SessionStart"
                && let Some(session_id) = payload.get("session_id").and_then(|v| v.as_str()) {
                    debug!("SessionStart: pane={} session_id={}", pane, session_id);
                    if let Err(e) = tmux_set_pane_option(config, pane, libslop::TmuxOption::SlopdClaudeSessionId.as_str(), session_id).await {
                        warn!("failed to set @slopd_claude_session_id on pane {}: {}", pane, e);
                    }
                }
            if event == "UserPromptSubmit" {
                debug!("UserPromptSubmit: notifying pending senders for pane {}", pane);
                let pane_state = panes.get_or_insert(pane);
                pane_state.prompt_submitted.notify_waiters();
                // A manual prompt means the user has taken over — reset the retry
                // counter so the next failure starts a fresh backoff sequence. But
                // slopd's OWN injected "continue" also fires UserPromptSubmit; that
                // one must NOT reset the counter, or max_retry_attempts could never
                // be reached and a persistently-failing turn would retry forever.
                if pane_state.expecting_auto_continue.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    debug!("UserPromptSubmit: pane {} is slopd's auto-continue, preserving retry counter", pane);
                } else {
                    *pane_state.retry_state.lock().unwrap() = None;
                }
            }

            // Unified state transition via reducer.
            {
                let current = panes.get_or_insert(pane).detailed_state.lock().unwrap().clone();
                if let Some(new_state) = reduce_pane_state(&current, &PaneStateEvent::Hook {
                    event: &event,
                    notification_type: payload.get("notification_type").and_then(|v| v.as_str()),
                }) {
                    set_pane_detailed_state(config, pane, &new_state, Some(&current), event_tx, panes).await;
                }
            }

            // Handle retry state: reset on clean Stop, schedule retry on StopFailure.
            if event == "Stop" {
                // Turn completed successfully — reset retry state.
                *panes.get_or_insert(pane).retry_state.lock().unwrap() = None;
            } else if event == "StopFailure" && config.run.auto_continue_on_failure {
                // Turn failed — decide whether to auto-retry and when.
                let pane_state = panes.get_or_insert(pane);
                let mut retry_guard = pane_state.retry_state.lock().unwrap();

                let policy = BackoffPolicy::from_config(&config.run);
                let next = RetryState::next(retry_guard.as_ref(), &policy, tokio::time::Instant::now());

                if let Some(next_state) = next {
                    // Schedule auto-continue.
                    let attempt = next_state.attempt_count;
                    let next_send_instant = next_state.next_send_at;
                    *retry_guard = Some(next_state);

                    // Spawn a task to send "continue" after the backoff.
                    let pane_id = pane.to_string();
                    let config_clone = config.clone();
                    let panes_clone = panes.clone();

                    tokio::spawn(async move {
                        let delay = next_send_instant.saturating_duration_since(tokio::time::Instant::now());
                        if !delay.is_zero() {
                            debug!("StopFailure: pane {} will auto-continue in {:?}", pane_id, delay);
                            tokio::time::sleep(delay).await;
                        }

                        // Check if retry state is still valid (may have been reset by manual prompt or Stop).
                        let should_send = panes_clone.get(&pane_id)
                            .map(|state| {
                                let guard = state.retry_state.lock().unwrap();
                                guard.as_ref().is_some_and(|s| s.matches(attempt, next_send_instant))
                            })
                            .unwrap_or(false);

                        if !should_send {
                            debug!("StopFailure: pane {} retry state changed, cancelling auto-continue", pane_id);
                            return;
                        }

                        debug!("StopFailure: sending auto-continue to pane {}", pane_id);

                        // Mark the upcoming UserPromptSubmit as ours so its handler
                        // doesn't reset the retry counter (which would defeat
                        // max_retry_attempts for a persistently-failing turn).
                        if let Some(pane_obj) = panes_clone.get(&pane_id) {
                            pane_obj.expecting_auto_continue.store(true, std::sync::atomic::Ordering::SeqCst);
                        }

                        // Type "continue" into the pane.
                        let _ = tmux(&config_clone)
                            .args(["send-keys", "-t", &pane_id, "continue"])
                            .status()
                            .await;

                        // Small delay before Enter to ensure the text lands.
                        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

                        // Send Enter and wait for UserPromptSubmit.
                        if let Some(pane_obj) = panes_clone.get(&pane_id) {
                            let _ = tmux(&config_clone)
                                .args(["send-keys", "-t", &pane_id, "Enter"])
                                .status()
                                .await;
                            let notified = pane_obj.prompt_submitted.notified();
                            let _ = tokio::time::timeout(tokio::time::Duration::from_secs(10), notified).await;
                            debug!("StopFailure: auto-continue submitted to pane {}", pane_id);
                        } else {
                            warn!("StopFailure: failed to send auto-continue to pane {} (pane disappeared)", pane_id);
                        }
                    });
                } else {
                    // Attempt cap exceeded — give up and clear retry state so a
                    // later failure starts a fresh backoff sequence.
                    *retry_guard = None;
                    debug!("StopFailure: pane {} exceeded max attempts ({}), giving up", pane, config.run.max_retry_attempts);
                }
            }

            let _ = event_tx.send(libslop::Record {
                source: "hook".to_string(),
                event_type: event,
                pane_id,
                payload,
                cursor: None,
            });

            libslop::ResponseBody::Hooked
        }

        libslop::RequestBody::Run { parent_pane_id, extra_args, start_directory, env, account, backend } => {
            // Pick the account: an explicit --account wins; otherwise inherit the
            // parent pane's account (its @slopd_account option) so a pane spawned
            // from another pane stays on the same account by default.
            let requested_account = match account {
                Some(name) => Some(name),
                None => match parent_pane_id.as_deref() {
                    Some(parent) => read_pane_account(config, parent).await,
                    None => None,
                },
            };
            // Resolve to the account's Claude config dir before doing anything
            // else, so an unknown account fails fast without spawning.
            let resolved = match config.resolve_account(requested_account.as_deref()) {
                Ok(mut resolved) => {
                    // `--backend` override is authoritative: flip the backend and
                    // recompute the executable — keep it if it already matches or is
                    // a custom path, else swap a conflicting recognized name to the
                    // backend's canonical binary.
                    if let Some(backend) = backend {
                        resolved.executable = match libslop::Backend::infer_from_program(resolved.executable.program()) {
                            Some(inferred) if inferred == backend => resolved.executable.clone(),
                            Some(_) => libslop::Executable::String(backend.canonical_executable().to_string()),
                            None => resolved.executable.clone(),
                        };
                        resolved.backend = backend;
                    }
                    resolved
                }
                Err(message) => return libslop::ResponseBody::Error { message },
            };
            // Inject hooks into the account's settings.json (the dir the new pane
            // will actually read), not necessarily the default one. Claude only —
            // opencode and other non-Claude backends are driven without hooks.
            if resolved.backend.uses_injected_hooks() {
                let settings_path = config.resolved_settings_path(&resolved);
                if let Err(e) = libslop::inject_hooks_into_file(&settings_path, &config.hook_slopctl()) {
                    warn!("failed to inject hooks into {}: {}", settings_path.display(), e);
                }
            }
            // Resolve start directory: per-session flag takes precedence over config default.
            // Both are `~` / `$VAR`-expanded here (against slopd's environment), so a
            // quoted `~` works and a remote `~` resolves to the remote home.
            let effective_start_dir = start_directory
                .as_deref()
                .map(libslop::expand_path)
                .or_else(|| config.run.start_directory.as_ref().map(|p| libslop::expand_path(p)));
            // Merge env: config env_files (in order) → config env → request env.
            // Later entries override earlier ones (tmux applies -e left-to-right).
            let mut merged_env: Vec<(String, String)> = Vec::new();
            for raw_path in &config.run.env_files {
                let path = libslop::expand_path(raw_path);
                match libslop::load_env_file(&path) {
                    Ok(pairs) => merged_env.extend(pairs),
                    Err(e) => {
                        return libslop::ResponseBody::Error { message: e };
                    }
                }
            }
            for (k, v) in &config.run.env {
                match libslop::expand_env_value(v) {
                    Ok(expanded) => merged_env.push((k.clone(), expanded)),
                    Err(e) => {
                        return libslop::ResponseBody::Error {
                            message: format!("invalid [run.env] {}: {}", k, e),
                        };
                    }
                }
            }
            merged_env.extend(env.iter().cloned());

            // For opencode panes: allocate a port + per-pane auth token for the
            // embedded HTTP server, and pass them as spawn args/env. Claude
            // panes take the unmodified path.
            let opencode_port = if resolved.backend == libslop::Backend::Opencode {
                match opencode::alloc_port() {
                    Ok(p) => Some(p),
                    Err(e) => return libslop::ResponseBody::Error {
                        message: format!("failed to allocate opencode port: {}", e),
                    },
                }
            } else {
                None
            };
            let opencode_token = opencode_port.map(|_| opencode::random_token());

            let mut trailing = extra_args;
            let mut spawn_env = merged_env;
            if let (Some(port), Some(token)) = (opencode_port, opencode_token.as_ref()) {
                let mut v = trailing.clone();
                v.extend([
                    "--port".to_string(),
                    port.to_string(),
                    "--hostname".to_string(),
                    "127.0.0.1".to_string(),
                ]);
                trailing = v;
                spawn_env.push(("OPENCODE_SERVER_PASSWORD".to_string(), token.clone()));
                spawn_env.push(("OPENCODE_SERVER_USERNAME".to_string(), "opencode".to_string()));
            }

            // Spawn through the shared chokepoint, which resolves the executable
            // to an absolute path (so the pane can't fail to find it on its PATH)
            // and surfaces a clear error if it's missing.
            let output = spawn_claude_pane(config, session_lock, &SpawnSpec {
                working_dir: effective_start_dir.as_ref().and_then(|d| d.to_str().map(str::to_string)),
                config_dir: resolved.config_dir.clone(),
                backend: resolved.backend,
                executable: resolved.executable.clone(),
                extra_env: spawn_env,
                trailing_args: trailing,
            }).await;
            match output {
                Ok(pane_id) => {
                    debug!("spawned {:?} ({}) in pane {}", resolved.executable, resolved.backend.canonical_executable(), pane_id);
                    managed_panes.insert(pane_id.clone());
                    // Wake any hook handlers that arrived before managed_panes.insert()
                    // (race between tmux creating the pane and this task resuming).
                    pane_registered.notify_waiters();
                    let _ = tmux_set_pane_option(config, &pane_id, libslop::TmuxOption::SlopdManaged.as_str(), "true").await;
                    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
                    let _ = tmux_set_pane_option(config, &pane_id, libslop::TmuxOption::SlopdCreatedAt.as_str(), &now.to_string()).await;
                    // Record the account so `ps` can show it, child panes can
                    // inherit it, and a slopd restart re-injects the right hooks
                    // for this pane (see load_managed_panes).
                    let _ = tmux_set_pane_option(config, &pane_id, libslop::TmuxOption::SlopdAccount.as_str(), &resolved.name).await;

                    // OpenCode panes: record backend/port/token, discover the session
                    // id, attach the HTTP runtime, and start the status-poll driver
                    // (which advances BootingUp → Ready/idle). Claude panes skip this
                    // and rely on the SessionStart hook + jsonl tailer instead.
                    if resolved.backend == libslop::Backend::Opencode {
                        let port = opencode_port.expect("opencode port allocated above");
                        let token = opencode_token.clone().expect("opencode token allocated above");
                        let _ = tmux_set_pane_option(config, &pane_id, libslop::TmuxOption::SlopdBackend.as_str(), "opencode").await;
                        let _ = tmux_set_pane_option(config, &pane_id, libslop::TmuxOption::SlopdOpencodePort.as_str(), &port.to_string()).await;
                        let _ = tmux_set_pane_option(config, &pane_id, libslop::TmuxOption::SlopdOpencodeToken.as_str(), &token).await;

                        let client = opencode::OpencodeClient::new(port, Some(token.clone()));
                        // opencode creates the session on startup; poll /session until
                        // one appears (bounded, so a pane that never starts a session
                        // doesn't hang slopd's run handler indefinitely).
                        let session_id = match tokio::time::timeout(std::time::Duration::from_secs(20), async {
                            loop {
                                if let Ok(Some(id)) = client.latest_session().await {
                                    return id;
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                            }
                        }).await {
                            Ok(id) => id,
                            Err(_) => {
                                warn!("opencode pane {}: timed out discovering session id; state tracking will be limited", pane_id);
                                String::new()
                            }
                        };
                        if !session_id.is_empty() {
                            let _ = tmux_set_pane_option(config, &pane_id, libslop::TmuxOption::SlopdClaudeSessionId.as_str(), &session_id).await;
                        }
                        let driver_cancel = tokio_util::sync::CancellationToken::new();
                        let pane_state = panes.get_or_insert(&pane_id);
                        *pane_state.opencode.lock().unwrap() = Some(OpencodeState {
                            client: client.clone(),
                            session_id: session_id.clone(),
                        });
                        *pane_state.opencode_cancel.lock().unwrap() = driver_cancel.clone();
                        tokio::spawn(run_opencode_driver(
                            client,
                            session_id,
                            pane_id.clone(),
                            config.clone(),
                            panes.clone(),
                            event_tx.clone(),
                            driver_cancel,
                        ));
                    }

                    // Test hook: SLOPD_TEST_RUN_YIELD_MS adds an extra async sleep here so
                    // that concurrent hook tasks (e.g. SessionStart fired by mock_claude as
                    // soon as the tmux window opens) are guaranteed to be processed before we
                    // reach the guard below. This makes the race condition deterministic in
                    // the run_handler_does_not_reset_pane_state_on_concurrent_hook test.
                    // Only compiled when the "testing" feature is enabled — never in production.
                    #[cfg(feature = "testing")]
                    if let Some(ms) = std::env::var("SLOPD_TEST_RUN_YIELD_MS")
                        .ok()
                        .and_then(|s| s.parse::<u64>().ok())
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                    }
                    // Guard: only set BootingUp state if no concurrent hook has already
                    // advanced it. PaneState::new() already initialises detailed_state to
                    // BootingUp; a fast-starting process (e.g. mock_claude under coverage)
                    // can fire its SessionStart hook during the await points above, setting
                    // the pane to Ready before we reach this point. Without this guard we
                    // would reset a Ready pane back to BootingUp, causing slopctl send to
                    // wait indefinitely for the pane to become ready again.
                    let current_state = panes.get_or_insert(&pane_id).detailed_state.lock().unwrap().clone();
                    if current_state == libslop::PaneDetailedState::BootingUp {
                        let new_state = reduce_pane_state(&current_state, &PaneStateEvent::Init).unwrap();
                        set_pane_detailed_state(config, &pane_id, &new_state, None, event_tx, panes).await;
                    }
                    if let Some(ref parent) = parent_pane_id {
                        // Build the ancestor chain: [parent, parent's ancestors...].
                        let mut ancestors = vec![parent.clone()];
                        // Read the parent's ancestor chain from tmux.
                        if let Ok(out) = tmux(config)
                            .args(["show-options", "-t", parent, "-p", "-v",
                                   libslop::TmuxOption::SlopdAncestorPanes.as_str()])
                            .output()
                            .await
                        {
                            let parent_ancestors = String::from_utf8_lossy(&out.stdout).trim().to_string();
                            for a in parent_ancestors.split(',') {
                                let a = a.trim();
                                if !a.is_empty() {
                                    ancestors.push(a.to_string());
                                }
                            }
                        }
                        let encoded = encode_ancestors(&ancestors);
                        if let Err(e) = tmux_set_pane_option(config, &pane_id, libslop::TmuxOption::SlopdAncestorPanes.as_str(), &encoded).await {
                            warn!("failed to set @slopd_ancestor_panes on pane {}: {}", pane_id, e);
                        }
                    }
                    let _ = event_tx.send(libslop::Record {
                        source: "slopd".to_string(),
                        event_type: "PaneCreated".to_string(),
                        pane_id: Some(pane_id.clone()),
                        payload: serde_json::json!({
                            "pane_id": pane_id,
                            "parent_pane_id": parent_pane_id,
                        }),
                        cursor: None,
                    });
                    libslop::ResponseBody::Run { pane_id }
                }
                Err(message) => libslop::ResponseBody::Error { message },
            }
        }

        libslop::RequestBody::Send { pane_id, prompt, timeout_secs, interrupt } => {
            if !managed_panes.contains(&pane_id) {
                return libslop::ResponseBody::Error {
                    message: format!("pane {} is not managed by slopd", pane_id),
                };
            }
            let state = panes.get_or_insert(&pane_id);

            // OpenCode panes are HTTP-driven: handle the whole send here and
            // return. Claude panes fall through to the tmux keystroke path below.
            if state.is_opencode() {
                let oc = state.opencode.lock().unwrap().clone();
                let Some(oc) = oc else {
                    return libslop::ResponseBody::Error {
                        message: format!("opencode pane {} has no HTTP runtime", pane_id),
                    };
                };
                if interrupt {
                    if let Err(e) = oc.client.abort(&oc.session_id).await {
                        warn!("opencode abort failed for pane {}: {}", pane_id, e);
                    }
                }
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
                loop {
                    let current_state = state.detailed_state.lock().unwrap().clone();
                    match current_state {
                        libslop::PaneDetailedState::BootingUp => {
                            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                            if remaining.is_zero() {
                                return libslop::ResponseBody::Error {
                                    message: format!("timed out after {}s waiting for opencode pane {} to become ready", timeout_secs, pane_id),
                                };
                            }
                            tokio::time::sleep(remaining.min(std::time::Duration::from_millis(300))).await;
                        }
                        libslop::PaneDetailedState::AwaitingInputPermission
                        | libslop::PaneDetailedState::AwaitingInputElicitation => {
                            return libslop::ResponseBody::Error {
                                message: format!("pane {} cannot accept a prompt (state: {}); use --interrupt to preempt", pane_id, current_state.as_str()),
                            };
                        }
                        _ => break,
                    }
                }
                let is_command = prompt.starts_with('/');
                let res = if is_command {
                    oc.client.send_command(&oc.session_id, &prompt).await
                } else {
                    oc.client.send_message(&oc.session_id, &prompt).await
                };
                return match res {
                    Ok(()) => {
                        state.prompt_submitted.notify_waiters();
                        libslop::ResponseBody::Sent { pane_id }
                    }
                    Err(e) => libslop::ResponseBody::Error { message: e },
                };
            }

            let deadline = tokio::time::Instant::now()
                + std::time::Duration::from_secs(timeout_secs);

            // If --interrupt was requested, send C-c/C-d/Escape first to preempt
            // whatever Claude is currently doing.
            if interrupt {
                let _guard = state.type_mutex.lock().await;
                if let Err(e) = send_interrupt_keys(config, &pane_id).await {
                    return e;
                }
            }

            // Subscribe to DetailedStateChange events before reading current state
            // to avoid a race between the check and the subscription.
            let mut state_rx = event_tx.subscribe();

            // Wait for the pane to reach a sendable state if it isn't already.
            // BootingUp: Claude hasn't drawn its UI yet — wait for Ready.
            // AwaitingInput*: pane is at a dialog — reject immediately (interrupt
            //   should be used first if the caller wants to preempt).
            loop {
                let current_state = state.detailed_state.lock().unwrap().clone();
                match current_state {
                    libslop::PaneDetailedState::BootingUp => {
                        // Wait for DetailedStateChange → ready for this pane.
                        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                        if remaining.is_zero() {
                            return libslop::ResponseBody::Error {
                                message: format!(
                                    "timed out after {}s waiting for pane {} to become ready (still booting_up)",
                                    timeout_secs, pane_id
                                ),
                            };
                        }
                        match tokio::time::timeout(remaining, async {
                            loop {
                                match state_rx.recv().await {
                                    Ok(ev) if ev.event_type == "DetailedStateChange" && ev.pane_id.as_deref() == Some(&pane_id) => {
                                        if ev.payload.get("detailed_state").and_then(|v| v.as_str()) == Some("ready") {
                                            return;
                                        }
                                    }
                                    Ok(_) => continue,
                                    Err(_) => return,
                                }
                            }
                        }).await {
                            Ok(()) => continue,
                            Err(_) => {
                                return libslop::ResponseBody::Error {
                                    message: format!(
                                        "timed out after {}s waiting for pane {} to become ready (still booting_up)",
                                        timeout_secs, pane_id
                                    ),
                                };
                            }
                        }
                    }
                    libslop::PaneDetailedState::AwaitingInputPermission
                    | libslop::PaneDetailedState::AwaitingInputElicitation => {
                        return libslop::ResponseBody::Error {
                            message: format!(
                                "pane {} cannot accept a prompt (state: {}); use --interrupt to preempt",
                                pane_id, current_state.as_str()
                            ),
                        };
                    }
                    _ => break,
                }
            }

            // Acquire the type-mutex so concurrent sends don't interleave keystrokes.
            let _guard = state.type_mutex.lock().await;

            // Type the prompt text (without Enter) first.
            let result = tmux_send_keys(config, &pane_id, &prompt).await;

            // Release the type-mutex before awaiting delivery so other senders can type.
            drop(_guard);

            match result {
                Err(e) => libslop::ResponseBody::Error { message: e.to_string() },
                Ok(out) if !out.success() => {
                    let msg = format!("tmux send-keys failed for pane {}", pane_id);
                    libslop::ResponseBody::Error { message: msg }
                }
                Ok(_) => {
                    // Send Enter repeatedly with exponential backoff until
                    // UserPromptSubmit fires, confirming the prompt was submitted.
                    // Real Claude may treat some newlines as literal (Ctrl+J) rather
                    // than submit, so we retry.
                    let deadline = tokio::time::Instant::now()
                        + std::time::Duration::from_secs(timeout_secs);
                    let mut backoff = std::time::Duration::from_millis(100);
                    let max_backoff = std::time::Duration::from_secs(2);

                    loop {
                        let notified = state.prompt_submitted.notified();

                        let enter_result = tmux_send_keys(config, &pane_id, "Enter").await;

                        if let Err(e) = enter_result {
                            break libslop::ResponseBody::Error { message: e.to_string() };
                        }

                        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                        if remaining.is_zero() {
                            break libslop::ResponseBody::Error {
                                message: format!("timed out after {}s waiting for UserPromptSubmit on pane {}", timeout_secs, pane_id),
                            };
                        }

                        let wait = backoff.min(remaining);
                        match tokio::time::timeout(wait, notified).await {
                            Ok(()) => break libslop::ResponseBody::Sent { pane_id },
                            Err(_) => {
                                backoff = (backoff * 2).min(max_backoff);
                            }
                        }
                    }
                }
            }
        }

        libslop::RequestBody::Interrupt { pane_id } => {
            if !managed_panes.contains(&pane_id) {
                return libslop::ResponseBody::Error {
                    message: format!("pane {} is not managed by slopd", pane_id),
                };
            }
            let state = panes.get_or_insert(&pane_id);

            // OpenCode: interrupt via HTTP abort. Claude: tmux C-c/C-d/Esc below.
            if state.is_opencode() {
                let oc = state.opencode.lock().unwrap().clone();
                let Some(oc) = oc else {
                    return libslop::ResponseBody::Error {
                        message: format!("opencode pane {} has no HTTP runtime", pane_id),
                    };
                };
                return match oc.client.abort(&oc.session_id).await {
                    Ok(()) => libslop::ResponseBody::Interrupted { pane_id },
                    Err(e) => libslop::ResponseBody::Error { message: e },
                };
            }

            // Acquire the type-mutex so we don't interleave with concurrent sends.
            let _guard = state.type_mutex.lock().await;

            if let Err(e) = send_interrupt_keys(config, &pane_id).await {
                return e;
            }

            libslop::ResponseBody::Interrupted { pane_id }
        }

        libslop::RequestBody::Tag { pane_id, tag, remove } => {
            if !managed_panes.contains(&pane_id) {
                return libslop::ResponseBody::Error {
                    message: format!("pane {} is not managed by slopd", pane_id),
                };
            }
            let option_name = match libslop::tag_option_name(&tag) {
                Ok(name) => name,
                Err(e) => return libslop::ResponseBody::Error { message: e },
            };
            if remove {
                match tmux_unset_pane_option(config, &pane_id, &option_name).await {
                    Ok(s) if s.success() => libslop::ResponseBody::Untagged { pane_id, tag },
                    Ok(s) => libslop::ResponseBody::Error { message: format!("tmux exited with {}", s) },
                    Err(e) => libslop::ResponseBody::Error { message: e.to_string() },
                }
            } else {
                match tmux_set_pane_option(config, &pane_id, &option_name, "1").await {
                    Ok(s) if s.success() => libslop::ResponseBody::Tagged { pane_id, tag },
                    Ok(s) => libslop::ResponseBody::Error { message: format!("tmux exited with {}", s) },
                    Err(e) => libslop::ResponseBody::Error { message: e.to_string() },
                }
            }
        }

        libslop::RequestBody::Tags { pane_id } => {
            let output = tmux(config)
                .args(["show-options", "-t", &pane_id, "-p"])
                .output()
                .await;
            match output {
                Err(e) => libslop::ResponseBody::Error { message: e.to_string() },
                Ok(out) if !out.status.success() => {
                    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    libslop::ResponseBody::Error { message: stderr }
                }
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let tags: Vec<String> = stdout.lines()
                        .filter_map(|line| {
                            let opt = line.split_whitespace().next()?;
                            opt.strip_prefix(libslop::TAG_OPTION_PREFIX).map(|t| t.to_string())
                        })
                        .collect();
                    libslop::ResponseBody::Tags { pane_id, tags }
                }
            }
        }

        libslop::RequestBody::Ps => {
            match list_panes(config, managed_panes).await {
                Ok(panes) => libslop::ResponseBody::Ps { panes },
                Err(e) => libslop::ResponseBody::Error { message: e },
            }
        }

        libslop::RequestBody::Backup => {
            // Manual backup: write the manifest now, regardless of auto_backup.
            // This explicitly replaces the restore point with the current state,
            // so it resolves any pending restore and lets auto-backup resume.
            let manifest_path = config.backup.manifest_path();
            let count = backup_panes(config, managed_panes, &manifest_path).await;
            *pending_restore.lock().unwrap() = None;
            let _ = tokio::fs::remove_file(config.backup.pending_marker_path()).await;
            libslop::ResponseBody::BackedUp { count }
        }

        libslop::RequestBody::Restore => {
            // Manual restore: re-spawn from the manifest now, regardless of
            // auto_restore. restore_panes seeds its dedup set with the sessions
            // of currently-running panes, so this won't double a live session.
            let manifest_path = config.backup.manifest_path();
            let manifest = read_pane_manifest(&manifest_path).await;
            let restored = restore_panes(
                config, managed_panes, panes, event_tx, pane_registered, session_lock, manifest,
            ).await;
            // The pending restore (if any) has now been consumed; resume auto-backup.
            *pending_restore.lock().unwrap() = None;
            let _ = tokio::fs::remove_file(config.backup.pending_marker_path()).await;
            libslop::ResponseBody::Restored { restored }
        }

        libslop::RequestBody::ReadTranscript { pane_id, before_cursor, limit } => {
            // OpenCode: conversation lives in the opencode DB, served over HTTP —
            // there is no jsonl file. Pull and map it. (Cursor pagination is
            // byte-offset based for Claude; opencode uses index cursors for now.)
            if let Some(state) = panes.get(&pane_id) {
                if state.is_opencode() {
                    let oc = state.opencode.lock().unwrap().clone();
                    let Some(oc) = oc else {
                        return libslop::ResponseBody::TranscriptPage { records: vec![] };
                    };
                    let _ = before_cursor;
                    match oc.client.messages(&oc.session_id).await {
                        Ok(msgs) => {
                            let mut records: Vec<libslop::Record> = opencode::messages_to_records(&msgs)
                                .into_iter()
                                .enumerate()
                                .map(|(i, (event_type, payload))| libslop::Record {
                                    cursor: Some(i as u64),
                                    source: "transcript".to_string(),
                                    event_type,
                                    pane_id: Some(pane_id.clone()),
                                    payload,
                                })
                                .collect();
                            if limit > 0 {
                                let limit = limit as usize;
                                if records.len() > limit {
                                    records = records.split_off(records.len() - limit);
                                }
                            }
                            return libslop::ResponseBody::TranscriptPage { records };
                        }
                        Err(e) => return libslop::ResponseBody::Error { message: e },
                    }
                }
            }

            let transcript_path = panes
                .get(&pane_id)
                .and_then(|state| state.transcript_path.lock().unwrap().clone());

            match transcript_path {
                None => libslop::ResponseBody::TranscriptPage {
                    records: vec![],
                },
                Some(path) => {
                    let path = std::path::PathBuf::from(&path);
                    let effective_before = match before_cursor {
                        Some(c) => c,
                        None => tokio::fs::metadata(&path).await
                            .map(|m| m.len()).unwrap_or(0),
                    };
                    match read_transcript_before(&path, effective_before, limit).await {
                        Ok((records, _at_beginning)) => {
                            let records = records.into_iter().map(|(cursor, payload)| {
                                let event_type = payload
                                    .get("type")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown")
                                    .to_string();
                                libslop::Record {
                                    cursor: Some(cursor),
                                    source: "transcript".to_string(),
                                    event_type,
                                    pane_id: Some(pane_id.clone()),
                                    payload,
                                }
                            }).collect();
                            libslop::ResponseBody::TranscriptPage { records }
                        }
                        Err(e) => libslop::ResponseBody::Error { message: e.to_string() },
                    }
                }
            }
        }

        libslop::RequestBody::Subscribe { .. }
        | libslop::RequestBody::SubscribeTranscript { .. }
        | libslop::RequestBody::Unsubscribe { .. } => {
            // Handled in handle_connection before reaching here.
            unreachable!("Subscribe/SubscribeTranscript/Unsubscribe should be handled before handle_request")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(max_attempts: u32, initial_backoff_ms: u64, max_backoff_ms: u64) -> BackoffPolicy {
        BackoffPolicy { max_attempts, initial_backoff_ms, max_backoff_ms: Some(max_backoff_ms) }
    }

    fn uncapped_policy(max_attempts: u32, initial_backoff_ms: u64) -> BackoffPolicy {
        BackoffPolicy { max_attempts, initial_backoff_ms, max_backoff_ms: None }
    }

    #[test]
    fn backoff_delay_doubles_then_caps() {
        let p = policy(10, 100, 1000);
        assert_eq!(p.delay_ms(1), 100);  // 100 * 2^0
        assert_eq!(p.delay_ms(2), 200);  // 100 * 2^1
        assert_eq!(p.delay_ms(3), 400);  // 100 * 2^2
        assert_eq!(p.delay_ms(4), 800);  // 100 * 2^3
        assert_eq!(p.delay_ms(5), 1000); // 1600 capped at 1000
        assert_eq!(p.delay_ms(6), 1000); // stays capped
    }

    #[test]
    fn backoff_delay_uncapped_keeps_doubling() {
        // With no cap (the default), the delay doubles every attempt forever.
        let p = uncapped_policy(20, 1000);
        assert_eq!(p.delay_ms(1), 1000);    // 1s
        assert_eq!(p.delay_ms(2), 2000);    // 2s
        assert_eq!(p.delay_ms(5), 16_000);  // 16s
        assert_eq!(p.delay_ms(11), 1_024_000); // ~17m, no ceiling
    }

    #[test]
    fn backoff_delay_does_not_overflow_on_huge_attempt() {
        // A pathological streak must not panic via shift/mul overflow.
        // Capped: saturates to the ceiling.
        let p = policy(u32::MAX, 1000, 30_000);
        assert_eq!(p.delay_ms(u32::MAX), 30_000);
        assert_eq!(p.delay_ms(1_000_000), 30_000);
        // Uncapped: saturates u64 rather than panicking.
        let p = uncapped_policy(u32::MAX, 1000);
        assert_eq!(p.delay_ms(u32::MAX), u64::MAX);
    }

    #[test]
    fn retry_next_increments_attempt_until_cap_then_stops() {
        let p = policy(2, 100, 1000);
        let now = tokio::time::Instant::now();

        // First failure → attempt 1.
        let s1 = RetryState::next(None, &p, now).expect("attempt 1 should schedule");
        assert_eq!(s1.attempt_count, 1);
        assert_eq!(s1.next_send_at, now + tokio::time::Duration::from_millis(100));

        // Second failure → attempt 2 (still within cap of 2).
        let s2 = RetryState::next(Some(&s1), &p, now).expect("attempt 2 should schedule");
        assert_eq!(s2.attempt_count, 2);
        assert_eq!(s2.next_send_at, now + tokio::time::Duration::from_millis(200));

        // Third failure → attempt 3 exceeds cap → give up.
        assert!(RetryState::next(Some(&s2), &p, now).is_none(),
            "attempt 3 must exceed max_attempts=2 and return None");
    }

    #[test]
    fn retry_next_with_zero_max_attempts_never_schedules() {
        let p = policy(0, 100, 1000);
        assert!(RetryState::next(None, &p, tokio::time::Instant::now()).is_none());
    }

    #[test]
    fn retry_matches_only_its_own_scheduled_attempt() {
        let now = tokio::time::Instant::now();
        let at = now + tokio::time::Duration::from_millis(100);
        let s = RetryState { attempt_count: 1, next_send_at: at };
        assert!(s.matches(1, at), "must match its own (attempt, time)");
        assert!(!s.matches(2, at), "different attempt must not match");
        assert!(!s.matches(1, now), "different scheduled time must not match");
    }

    #[test]
    fn parse_list_panes_separates_live_and_dead() {
        // Live panes report pane_dead=0 (status field empty); a dead pane reports
        // pane_dead=1 and its exit code in pane_dead_status.
        let out = "%26 0 \n%27 1 1\n%28 1 37\n";
        let (present, dead) = parse_list_panes(out);
        assert!(present.contains("%26") && present.contains("%27") && present.contains("%28"));
        // Dead panes are still listed (present), but also recorded with status.
        assert_eq!(dead.len(), 2);
        assert_eq!(dead.get("%27"), Some(&Some(1)));
        assert_eq!(dead.get("%28"), Some(&Some(37)));
        assert!(!dead.contains_key("%26"), "a live pane must not be marked dead");
    }

    #[test]
    fn parse_list_panes_tolerates_blank_and_missing_status() {
        // Blank lines are skipped; a dead pane with no parsable status maps to None.
        let (present, dead) = parse_list_panes("\n%1 1\n  \n");
        assert_eq!(present.len(), 1);
        assert_eq!(dead.get("%1"), Some(&None));
    }

    #[test]
    fn dead_pane_output_tail_strips_padding_and_footer() {
        // capture-pane returns the error, blank padding, then tmux's own footer.
        let captured = "claude: cannot start\nfatal: bad config\n\n\n\nPane is dead (status 1, Fri Jun 19 18:54:16 2026)";
        let tail = dead_pane_output_tail(captured);
        assert_eq!(tail, "claude: cannot start\nfatal: bad config");
    }

    #[test]
    fn dead_pane_output_tail_empty_when_only_blanks_and_footer() {
        let captured = "\n\n\nPane is dead (status 0, Fri Jun 19 18:54:16 2026)\n\n";
        assert_eq!(dead_pane_output_tail(captured), "");
    }

    #[test]
    fn dead_pane_output_tail_keeps_last_lines_when_long() {
        // More than MAX_LINES (40) lines: only the most recent are kept.
        let captured: String = (0..100).map(|i| format!("line{}\n", i)).collect();
        let tail = dead_pane_output_tail(&captured);
        assert!(tail.ends_with("line99"));
        assert!(!tail.contains("line0\n"), "oldest lines should be dropped");
        assert!(tail.contains("line99") && tail.contains("line60"));
    }
}
