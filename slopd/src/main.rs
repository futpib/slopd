use clap::{Parser, Subcommand};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{Mutex, Notify};
use tracing::{debug, error, info, trace, warn};

#[derive(Parser)]
#[command(name = "slopd", about = "Claude session manager daemon", version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT"), ")"))]
struct Cli {
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
    /// Override the executable used to spawn Claude sessions (default: from config or "claude").
    /// Specify the program and optional arguments, e.g. --executable claude --foo --bar
    #[arg(long, num_args = 1.., allow_hyphen_values = true)]
    executable: Option<Vec<String>>,
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
    Hook { event: &'a str },
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

        PaneStateEvent::Hook { event } => reduce_hook_event(event),

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
                    hook_event.and_then(reduce_hook_event)
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
fn reduce_hook_event(event: &str) -> Option<libslop::PaneDetailedState> {
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
}

impl PaneState {
    fn new() -> Self {
        Self {
            type_mutex: Mutex::new(()),
            prompt_submitted: Notify::new(),
            detailed_state: std::sync::Mutex::new(libslop::PaneDetailedState::BootingUp),
            transcript_cancel: std::sync::Mutex::new(tokio_util::sync::CancellationToken::new()),
            transcript_path: std::sync::Mutex::new(None),
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
                        if record_type == "queue-operation" {
                            if record.get("operation").and_then(|v| v.as_str()) == Some("enqueue") {
                                debug!("transcript enqueue: notifying pending senders for pane {}", pane_id);
                                pane_state.prompt_submitted.notify_waiters();
                            }
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

    let at_beginning = window.front().map_or(true, |(offset, _)| *offset == 0);
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

    fn is_empty(&self) -> bool {
        self.inner.is_empty()
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
/// Only panes that have the `@slopd_managed` pane option set are considered managed
/// (i.e. were registered via `slopctl run`). For each recovered pane, replays the
/// last N transcript records through the state reducer to recover the real state
/// instead of leaving it stuck at BootingUp.
async fn load_managed_panes(config: &Arc<libslop::SlopdConfig>, managed: &ManagedPanes, event_tx: &EventTx, panes: &PaneMap) {
    let format_str = format!(
        "#{{pane_id}} #{{{}}} #{{{}}}",
        libslop::TmuxOption::SlopdManaged.as_str(),
        libslop::TmuxOption::SlopdTranscriptPath.as_str(),
    );
    let output = tmux(config)
        .args(["list-panes", "-s", "-t", "slopd", "-F", &format_str])
        .output()
        .await;
    if let Ok(out) = output {
        if out.status.success() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let mut parts = line.splitn(3, ' ');
                let pane_id = parts.next().unwrap_or("").trim();
                let slopd_managed = parts.next().unwrap_or("").trim();
                let transcript_path = parts.next().unwrap_or("").trim();
                if pane_id.is_empty() || slopd_managed != "true" {
                    continue;
                }
                managed.insert(pane_id.to_string());

                // Replay the last N transcript records to recover the real state.
                let recovered_state = if !transcript_path.is_empty() {
                    recover_state_from_transcript(transcript_path).await
                } else {
                    None
                };

                let initial_state = recovered_state.unwrap_or(libslop::PaneDetailedState::BootingUp);
                set_pane_detailed_state(config, pane_id, &initial_state, None, event_tx, panes).await;

                // Start the transcript tailer if we have a path.
                if !transcript_path.is_empty() {
                    let state = panes.get_or_insert(pane_id);
                    let new_cancel = tokio_util::sync::CancellationToken::new();
                    *state.transcript_cancel.lock().unwrap() = new_cancel.clone();
                    *state.transcript_path.lock().unwrap() = Some(transcript_path.to_string());
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
            }
        }
    }
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

/// How long to wait for a pane to be registered before concluding that a hook
/// came from a genuinely unmanaged (external) pane.  The race window is
/// typically sub-millisecond; 2 s is generous headroom for a loaded system.
const PANE_REGISTRATION_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

fn filters_match(filters: &[libslop::EventFilter], ev: &libslop::Record) -> bool {
    if filters.is_empty() {
        return true;
    }
    filters.iter().any(|f| {
        if let Some(ref src) = f.source {
            if src != &ev.source {
                return false;
            }
        }
        if let Some(ref et) = f.event_type {
            if et != &ev.event_type {
                return false;
            }
        }
        if let Some(ref pane_id) = f.pane_id {
            if ev.pane_id.as_deref() != Some(pane_id.as_str()) {
                return false;
            }
        }
        if let Some(ref session_id) = f.session_id {
            if ev.payload.get("session_id").and_then(|v| v.as_str()) != Some(session_id.as_str()) {
                return false;
            }
        }
        for (k, v) in &f.payload_match {
            if ev.payload.get(k) != Some(v) {
                return false;
            }
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
    let slopctl = &config.run.slopctl;

    // Read existing hooks.
    let existing = match tmux(config)
        .args(["show-hooks", "-t", "slopd"])
        .output()
        .await
    {
        Ok(out) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).to_string()
        }
        _ => String::new(),
    };

    for &(hook_name, include_pane_id) in TMUX_HOOKS {
        let our_command = tmux_hook_command(slopctl, hook_name, include_pane_id);

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
            {
                if let Ok(idx) = idx_str.parse::<i32>() {
                    stale_indices.push(idx);
                }
            }
        }

        // Remove stale entries in reverse order so indices stay valid.
        stale_indices.sort_unstable();
        for &idx in stale_indices.iter().rev() {
            let indexed_name = format!("{}[{}]", hook_name, idx);
            let _ = tmux(config)
                .args(["set-hook", "-u", "-t", "slopd", &indexed_name])
                .output()
                .await;
        }

        // Append our hook.
        if let Err(e) = tmux(config)
            .args(["set-hook", "-a", "-t", "slopd", hook_name, &our_command])
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
    let has_session = tmux(config)
        .args(["has-session", "-t", "slopd"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
    if matches!(has_session, Ok(s) if s.success()) {
        return;
    }

    info!("slopd tmux session is gone, recreating");
    let _ = tmux(config)
        .args(["new-session", "-d", "-s", "slopd"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
    let _ = tmux(config)
        .args(["set-option", "-t", "slopd", libslop::TmuxOption::SlopdManaged.as_str(), "true"])
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
    let output = tmux(config)
        .args(["list-panes", "-s", "-t", "slopd", "-F", "#{pane_id}"])
        .output()
        .await;
    let live_ids: std::collections::HashSet<String> = match output {
        Ok(out) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        }
        Ok(out) if {
            let stderr = String::from_utf8_lossy(&out.stderr);
            stderr.contains("no server running on")
                || stderr.contains("can't find session:")
        } => {
            // Server or session is gone — all managed panes are dead.
            std::collections::HashSet::new()
        }
        _ => return,
    };

    // Test hook: simulate the production failure mode where `tmux list-panes`
    // transiently returned without our managed panes.  Used by the reconcile
    // false-positive regression test.
    let live_ids = if std::env::var("SLOPD_TEST_RECONCILE_FORCE_EMPTY").is_ok() {
        std::collections::HashSet::new()
    } else {
        live_ids
    };

    let candidates: Vec<String> = managed_panes.snapshot()
        .into_iter()
        .filter(|id| !live_ids.contains(id))
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

    let mut config = libslop::SlopdConfig::load();

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
    if let Some(executable) = cli.executable {
        config.run.executable = if executable.len() == 1 {
            libslop::Executable::String(executable.into_iter().next().unwrap())
        } else {
            libslop::Executable::Array(executable)
        };
    }

    config.run.slopctl = libslop::resolve_slopctl(&config.run.slopctl);

    if let Some(CliCommand::UninjectHooks) = cli.command {
        let settings_path = config.claude_settings_path();
        if let Err(e) = libslop::remove_hooks_from_file(&settings_path) {
            error!("failed to remove hooks from {}: {}", settings_path.display(), e);
            std::process::exit(1);
        }
        info!("removed slopctl hooks from {}", settings_path.display());
        return;
    }

    let config = Arc::new(config);

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
    let has_session = tmux(&config)
        .args(["has-session", "-t", "slopd"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .expect("failed to run tmux has-session");
    if !has_session.success() {
        tmux(&config)
            .args(["new-session", "-d", "-s", "slopd"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .expect("failed to create slopd tmux session");
    }

    // Mark the session with a user option so it can be identified
    tmux(&config)
        .args(["set-option", "-t", "slopd", libslop::TmuxOption::SlopdManaged.as_str(), "true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .expect("failed to set @slopd_managed option on tmux session");

    register_tmux_hooks(&config).await;

    let socket_path = libslop::socket_path();
    let socket_dir = socket_path.parent().unwrap();

    tokio::fs::create_dir_all(&socket_dir).await.unwrap();

    let lock_path = socket_path.with_extension("lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
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

    // Recover managed pane IDs from the tmux session so panes that existed
    // before a slopd restart are still recognized. This must happen before
    // binding the socket so that clients cannot create panes in the slopd
    // session while the scan is in progress.
    load_managed_panes(&config, &managed_panes, &event_tx, &panes).await;

    // Re-inject hooks if there are recovered panes — the previous slopd instance
    // removed them on exit, but the Claude sessions are still running in tmux.
    if !managed_panes.is_empty() {
        let settings_path = config.claude_settings_path();
        if let Err(e) = libslop::inject_hooks_into_file(&settings_path, &config.run.slopctl) {
            warn!("failed to re-inject hooks into {}: {}", settings_path.display(), e);
        }
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

    // Background task: periodically reconcile managed_panes against live tmux
    // panes to detect panes that exited without going through slopctl kill.
    // This catches cases that tmux session-scope hooks cannot (e.g. process
    // exit, which only fires pane-scope hooks).
    let session_lock: SessionLock = Arc::new(Mutex::new(()));
    let reconcile_config = config.clone();
    let reconcile_panes_map = panes.clone();
    let reconcile_managed = managed_panes.clone();
    let reconcile_tx = event_tx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            interval.tick().await;
            reconcile_panes(&reconcile_config, &reconcile_panes_map, &reconcile_managed, &reconcile_tx).await;
        }
    });

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _addr) = result.unwrap();
                debug!("accepted connection");
                tokio::spawn(handle_connection(stream, start_time, config.clone(), panes.clone(), managed_panes.clone(), event_tx.clone(), pane_registered.clone(), session_lock.clone()));
            }
            _ = sigterm.recv() => {
                info!("received SIGTERM, shutting down");
                break;
            }
            _ = sigint.recv() => {
                info!("received SIGINT, shutting down");
                break;
            }
        }
    }

    let settings_path = config.claude_settings_path();
    if let Err(e) = libslop::remove_hooks_from_file(&settings_path) {
        warn!("failed to remove hooks from {} on shutdown: {}", settings_path.display(), e);
    } else {
        info!("removed slopctl hooks from {}", settings_path.display());
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
                if let Some(dedup) = dedup {
                    if record.source == "transcript"
                        && record.pane_id.as_deref() == Some(&dedup.pane_id)
                        && record.cursor.map_or(false, |o| o < dedup.file_end_offset)
                    {
                        continue;
                    }
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

async fn handle_connection(
    stream: tokio::net::UnixStream,
    start_time: u64,
    config: Arc<libslop::SlopdConfig>,
    panes: PaneMap,
    managed_panes: ManagedPanes,
    event_tx: EventTx,
    pane_registered: PaneRegistered,
    session_lock: SessionLock,
) {
    let (reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(writer));
    let mut lines = BufReader::new(reader).lines();
    // Track active subscriptions so they can be cancelled via Unsubscribe.
    let mut subscriptions: std::collections::HashMap<u64, tokio_util::sync::CancellationToken> =
        std::collections::HashMap::new();

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
                    event_type: None,
                    pane_id: Some(pane_id.clone()),
                    session_id: None,
                    payload_match: serde_json::Map::new(),
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
                let body = handle_request(body, start_time, &config, &panes, &managed_panes, &event_tx, &pane_registered, &session_lock).await;
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
        } else if let Some(tag) = key.strip_prefix(libslop::TAG_OPTION_PREFIX) {
            tags.push(tag.to_string());
        }
    }
    ParsedPaneOptions { slopd_managed, session_id, ancestor_panes, tags, detailed_state, created_at }
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
        });
    }
    Ok(panes)
}

async fn send_interrupt_keys(config: &libslop::SlopdConfig, pane_id: &str) -> Result<(), libslop::ResponseBody> {
    for key in &["C-c", "C-d", "Escape"] {
        if let Err(e) = tmux_send_keys(config, pane_id, key).await {
            return Err(libslop::ResponseBody::Error { message: e.to_string() });
        }
    }
    Ok(())
}

async fn handle_request(
    body: libslop::RequestBody,
    start_time: u64,
    config: &Arc<libslop::SlopdConfig>,
    panes: &PaneMap,
    managed_panes: &ManagedPanes,
    event_tx: &EventTx,
    pane_registered: &PaneRegistered,
    session_lock: &SessionLock,
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
            if event == "SessionStart" {
                if let Some(session_id) = payload.get("session_id").and_then(|v| v.as_str()) {
                    debug!("SessionStart: pane={} session_id={}", pane, session_id);
                    if let Err(e) = tmux_set_pane_option(config, pane, libslop::TmuxOption::SlopdClaudeSessionId.as_str(), session_id).await {
                        warn!("failed to set @slopd_claude_session_id on pane {}: {}", pane, e);
                    }
                }
            }
            if event == "UserPromptSubmit" {
                debug!("UserPromptSubmit: notifying pending senders for pane {}", pane);
                panes.get_or_insert(pane).prompt_submitted.notify_waiters();
            }

            // Unified state transition via reducer.
            {
                let current = panes.get_or_insert(pane).detailed_state.lock().unwrap().clone();
                if let Some(new_state) = reduce_pane_state(&current, &PaneStateEvent::Hook { event: &event }) {
                    set_pane_detailed_state(config, pane, &new_state, Some(&current), event_tx, panes).await;
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

        libslop::RequestBody::Run { parent_pane_id, extra_args, start_directory, env } => {
            let settings_path = config.claude_settings_path();
            if let Err(e) = libslop::inject_hooks_into_file(&settings_path, &config.run.slopctl) {
                warn!("failed to inject hooks into {}: {}", settings_path.display(), e);
            }
            let xdg_runtime_dir = libslop::runtime_dir();
            // Resolve start directory: per-session flag takes precedence over config default.
            // Config value is expanded (~ and $VAR); the CLI value is already shell-expanded.
            let effective_start_dir = start_directory.or_else(|| {
                config.run.start_directory.as_ref().map(|p| libslop::expand_path(p))
            });
            let profile_file = std::env::var("LLVM_PROFILE_FILE").ok();
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
            let output = tmux_session_output(config, session_lock, |c| {
                let mut cmd = tmux(c);
                cmd.args(["new-window", "-t", "slopd", "-P", "-F", "#{pane_id}"])
                    .args(["-e", &format!("XDG_RUNTIME_DIR={}", xdg_runtime_dir.display())])
                    .args(["-e", &format!("SLOPCTL={}", c.run.slopctl)]);
                if let Some(ref dir) = effective_start_dir {
                    if let Some(dir_str) = dir.to_str() {
                        cmd.args(["-c", dir_str]);
                    }
                }
                if let Some(ref custom_dir) = c.claude_config_dir {
                    cmd.args(["-e", &format!("CLAUDE_CONFIG_DIR={}", custom_dir.display())]);
                }
                // Forward LLVM_PROFILE_FILE so instrumented child binaries (e.g. mock_claude)
                // write their coverage data even when launched inside a tmux window.
                if let Some(ref pf) = profile_file {
                    cmd.args(["-e", &format!("LLVM_PROFILE_FILE={}", pf)]);
                }
                for (k, v) in &merged_env {
                    cmd.args(["-e", &format!("{}={}", k, v)]);
                }
                cmd.arg(c.run.executable.program())
                    .args(c.run.executable.args())
                    .args(&extra_args);
                cmd
            }).await;
            match output {
                Ok(out) if out.status.success() => {
                    let pane_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    debug!("spawned {:?} in pane {}", config.run.executable, pane_id);
                    managed_panes.insert(pane_id.clone());
                    // Wake any hook handlers that arrived before managed_panes.insert()
                    // (race between tmux creating the pane and this task resuming).
                    pane_registered.notify_waiters();
                    let _ = tmux_set_pane_option(config, &pane_id, libslop::TmuxOption::SlopdManaged.as_str(), "true").await;
                    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
                    let _ = tmux_set_pane_option(config, &pane_id, libslop::TmuxOption::SlopdCreatedAt.as_str(), &now.to_string()).await;
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
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    libslop::ResponseBody::Error { message: stderr }
                }
                Err(e) => libslop::ResponseBody::Error { message: e.to_string() },
            }
        }

        libslop::RequestBody::Send { pane_id, prompt, timeout_secs, interrupt } => {
            if !managed_panes.contains(&pane_id) {
                return libslop::ResponseBody::Error {
                    message: format!("pane {} is not managed by slopd", pane_id),
                };
            }
            let state = panes.get_or_insert(&pane_id);
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

        libslop::RequestBody::ReadTranscript { pane_id, before_cursor, limit } => {
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
