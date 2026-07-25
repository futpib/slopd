use clap::{Parser, Subcommand};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{Mutex, Notify};
use tracing::{debug, error, info, trace, warn};

mod codex;
mod opencode;
mod backend;
use backend::*;

#[derive(Parser)]
#[command(name = "slopd", about = "Agent session manager daemon", version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT"), ")"))]
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
    /// Remove slopctl hook entries from Claude and Codex hook files.
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
    /// A hook fired by a hook-driven agent (received via the hook socket).
    Hook { event: &'a str, notification_type: Option<&'a str> },
    /// A transcript record was observed.
    TranscriptRecord {
        backend: libslop::Backend,
        record_type: &'a str,
        record: &'a serde_json::Value,
    },
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

        PaneStateEvent::TranscriptRecord {
            backend: libslop::Backend::Codex,
            record,
            ..
        } => {
            let next = codex::transcript_state(record);
            if matches!(
                current,
                libslop::PaneDetailedState::AwaitingInputPermission
                    | libslop::PaneDetailedState::AwaitingInputElicitation
            ) && matches!(
                next,
                Some(
                    libslop::PaneDetailedState::BusyProcessing
                        | libslop::PaneDetailedState::BusyToolUse
                )
            ) {
                None
            } else {
                next
            }
        }

        PaneStateEvent::TranscriptRecord { record_type, record, .. } => {
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

/// Why a managed pane was torn down. Recorded on every death so the systemd
/// journal (and the `PaneDestroyed` event) can answer "what happened to pane
/// %N?" without any surviving tmux state.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DeathCause {
    /// Torn down by an explicit `slopctl kill` (the Kill RPC).
    DeliberateKill,
    /// The agent process exited on its own; slopd caught the remain-on-exit
    /// husk and recorded its exit status + final screen.
    SelfExit,
    /// The whole tmux server / slopd session was gone when reconcile looked —
    /// every managed pane died at once (server crash, `tmux kill-server`).
    ServerGone,
    /// The pane was removed from tmux by something outside slopd (an external
    /// `tmux kill-pane` / `kill-window`), leaving neither a husk nor a Kill RPC.
    Vanished,
}

impl DeathCause {
    fn as_str(self) -> &'static str {
        match self {
            DeathCause::DeliberateKill => "deliberate_kill",
            DeathCause::SelfExit => "self_exit",
            DeathCause::ServerGone => "server_gone",
            DeathCause::Vanished => "vanished",
        }
    }

    /// Whether this death is unexpected enough to warrant a WARN (always visible
    /// in the journal at slopd's default verbosity). A clean `slopctl kill` or a
    /// zero-exit process is routine (INFO); a vanished pane, a nonzero exit, or a
    /// lost server is exactly the mystery a post-mortem needs, so it is a warning.
    fn is_abnormal(self, exit_status: Option<i64>) -> bool {
        match self {
            DeathCause::DeliberateKill => false,
            DeathCause::SelfExit => exit_status != Some(0),
            DeathCause::ServerGone | DeathCause::Vanished => true,
        }
    }
}

/// Which slopd code path noticed the death — finer-grained than [`DeathCause`],
/// recorded alongside it to pin down how slopd learned of the teardown.
#[derive(Clone, Copy)]
enum DeathDetectedBy {
    /// The Kill RPC handler (`slopctl kill`).
    KillRpc,
    /// The reconcile loop's DEAD-pane (remain-on-exit husk) path.
    ReconcileDeadPane,
    /// The reconcile loop's vanished-pane path.
    ReconcileVanished,
}

impl DeathDetectedBy {
    fn as_str(self) -> &'static str {
        match self {
            DeathDetectedBy::KillRpc => "kill_rpc",
            DeathDetectedBy::ReconcileDeadPane => "reconcile_dead_pane",
            DeathDetectedBy::ReconcileVanished => "reconcile_vanished",
        }
    }
}

/// A durable snapshot of a pane's identity, kept in memory so a death can be
/// fully described after the pane's tmux options are already gone. Populated at
/// spawn (backend/parent/working_dir/created_at) and kept fresh as slopd learns
/// the session id (SessionStart hook / opencode bind / fork pin) and title.
#[derive(Clone, Default)]
struct PaneIdentity {
    backend: libslop::Backend,
    session_id: Option<String>,
    parent_pane_id: Option<String>,
    working_dir: Option<String>,
    title: Option<String>,
    created_at: Option<u64>,
}

/// Per-pane state shared across connection handlers.
struct PaneState {
    /// Serialises the type-then-enter sequence so two concurrent sends don't interleave.
    type_mutex: Mutex<()>,
    /// Notified whenever UserPromptSubmit fires for this pane.
    prompt_submitted: Notify,
    /// Notified whenever SessionStart binds or re-binds a session id.
    session_bound: Notify,
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
    /// Exactly one backend runtime owns this pane. Backend-specific mutable
    /// state and cancellation live inside the corresponding variant.
    runtime: std::sync::Mutex<PaneRuntime>,
    /// For a Claude pane created by `fork`: the fork's session id (which slopd
    /// minted and passed via `--session-id`). Real Claude fires its `SessionStart`
    /// hook with the *resumed* source session id — not the forked id — so without
    /// this pin the pane would be mis-bound to the source session. When set, the
    /// SessionStart handler uses this id instead of the hook payload's.
    pinned_session_id: std::sync::Mutex<Option<String>>,
    /// Durable identity snapshot (backend, session id, parent, cwd, title,
    /// spawn time), populated at spawn and kept fresh as slopd learns more. Read
    /// at teardown to describe the death after the pane's tmux options are gone.
    identity: std::sync::Mutex<PaneIdentity>,
}

impl PaneState {
    fn new() -> Self {
        Self {
            type_mutex: Mutex::new(()),
            prompt_submitted: Notify::new(),
            session_bound: Notify::new(),
            detailed_state: std::sync::Mutex::new(libslop::PaneDetailedState::BootingUp),
            transcript_cancel: std::sync::Mutex::new(tokio_util::sync::CancellationToken::new()),
            transcript_path: std::sync::Mutex::new(None),
            retry_state: std::sync::Mutex::new(None),
            expecting_auto_continue: std::sync::atomic::AtomicBool::new(false),
            runtime: std::sync::Mutex::new(PaneRuntime::default()),
            pinned_session_id: std::sync::Mutex::new(None),
            identity: std::sync::Mutex::new(PaneIdentity::default()),
        }
    }

    /// Merge a freshly-learned session id into the identity snapshot. Called
    /// wherever slopd binds/rebinds a session (SessionStart, opencode bind,
    /// fork pin) so a later death record names the right session.
    fn note_session_id(&self, session_id: &str) {
        self.identity.lock().unwrap().session_id = Some(session_id.to_string());
        self.session_bound.notify_waiters();
    }

    fn runtime(&self) -> PaneRuntime {
        self.runtime.lock().unwrap().clone()
    }

    fn set_runtime(&self, runtime: PaneRuntime) {
        self.runtime().cancel();
        self.identity.lock().unwrap().backend = runtime.backend();
        *self.runtime.lock().unwrap() = runtime;
    }

    fn mark_unbound_backend(&self, backend: libslop::Backend) {
        let mut runtime = self.runtime.lock().unwrap();
        if matches!(*runtime, PaneRuntime::Unbound(_)) {
            *runtime = PaneRuntime::Unbound(UnboundState { backend });
        }
        self.identity.lock().unwrap().backend = backend;
    }

    fn opencode(&self) -> Option<OpencodeState> {
        match self.runtime() {
            PaneRuntime::Opencode(runtime) => Some(runtime),
            _ => None,
        }
    }

    fn update_opencode_session(&self, session_id: String) -> Option<opencode::OpencodeClient> {
        let mut runtime = self.runtime.lock().unwrap();
        match &mut *runtime {
            PaneRuntime::Opencode(runtime) => {
                runtime.session_id = session_id;
                Some(runtime.client.clone())
            }
            _ => None,
        }
    }

    /// Stop every background task attached to this pane. Called from every
    /// teardown path (explicit kill + both reconcile-driven reaps) so a pane that
    /// exits keeps nothing running behind it. For opencode panes this is what
    /// stops the SSE reader + backstop poll from reconnecting to a now-dead server
    /// forever; Claude panes only have the transcript tailer.
    fn cancel_drivers(&self) {
        self.transcript_cancel.lock().unwrap().cancel();
        self.runtime().cancel();
    }
}

/// Live subagents (opencode child sessions) of the pane's current session, mapped
/// to when slopd first saw each. Shared between the SSE reader (which adds on
/// `session.created` and removes on the child's terminal event) and the backstop
/// (which prunes any child no longer *working* per `/session/status`, so a missed
/// terminal event can't pin the pane to `busy_subagent` forever). Non-empty ⇒ the
/// pane's detailed state is `busy_subagent`.
type SubagentSet = Arc<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>>;

/// Grace before the backstop prunes a tracked subagent that is not currently
/// working: a child that was just spawned may not have reported `busy` in
/// `/session/status` yet, so give it one poll cycle's worth of slack before
/// treating "not working" as "finished".
const SUBAGENT_PRUNE_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// OpenCode pane driver. Real-time state + transcript come from the server's SSE
/// stream (`GET /event`); a slow `/session` + `/session/status` poll is a backstop
/// and also drives initial readiness — a freshly-spawned opencode session is idle
/// and therefore absent from `/session/status`, so readiness is "session exists in
/// `/session` and is not busy". opencode events normalize onto slopd's existing
/// state machine via the shared [`set_pane_detailed_state`] path (the same one
/// Claude's hook handler uses).
async fn run_opencode_driver(
    client: opencode::OpencodeClient,
    session_id: String,
    pane_id: String,
    config: Arc<libslop::SlopdConfig>,
    panes: PaneMap,
    event_tx: EventTx,
    cancel: tokio_util::sync::CancellationToken,
) {
    // Initial reconcile so the pane reaches Ready quickly after spawn.
    reconcile_opencode_status(&client, &session_id, &pane_id, &config, &panes, &event_tx).await;

    // Set of live subagents (child sessions), shared by the SSE reader and the
    // backstop below so the backstop can prune a child whose terminal event the
    // SSE reader never saw.
    let subagents: SubagentSet = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

    // SSE reader (reconnects with backoff). Shares the cancel token so it stops
    // when the pane is killed.
    {
        let client = client.clone();
        let session_id = session_id.clone();
        let pane_id = pane_id.clone();
        let config = config.clone();
        let panes = panes.clone();
        let event_tx = event_tx.clone();
        let cancel = cancel.clone();
        let subagents = subagents.clone();
        tokio::spawn(async move {
            run_opencode_sse(client, session_id, pane_id, config, panes, event_tx, cancel, subagents).await
        });
    }

    // Backstop poll every 3s: re-reconciles state if SSE drops or an event is
    // missed, and is the authority on initial readiness.
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await; // first tick is immediate; we already reconciled, so absorb it
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = interval.tick() => {
                // Read the live session id: the SSE reader re-points it when the
                // human navigates the TUI, and this backstop must poll whatever
                // session is currently being followed, not the spawn-time one.
                let sid = panes.get(&pane_id)
                    .and_then(|s| s.opencode().map(|oc| oc.session_id))
                    .unwrap_or_else(|| session_id.clone());
                // Authoritatively reconcile live subagents first: drop any child
                // that is no longer working per /session/status (finished, or
                // wedged in `retry`) even if its terminal SSE event was missed.
                let live_subagents =
                    reconcile_opencode_subagents(&client, &sid, &pane_id, &subagents, &event_tx).await;
                if live_subagents > 0 {
                    // A subagent is genuinely running → busy_subagent wins over the
                    // main session's own status (mirrors the SSE reader's override).
                    let current = panes.get_or_insert(&pane_id).detailed_state.lock().unwrap().clone();
                    if current != libslop::PaneDetailedState::BusySubagent {
                        set_pane_detailed_state(&config, &pane_id, &libslop::PaneDetailedState::BusySubagent, Some(&current), &event_tx, &panes).await;
                    }
                } else {
                    // No live subagent → the pane reflects its own session's status.
                    reconcile_opencode_status(&client, &sid, &pane_id, &config, &panes, &event_tx).await;
                }
            }
        }
    }
}

/// Reconcile an opencode pane's state from the HTTP API: busy via
/// `/session/status`; ready via "session exists in `/session` and is not busy".
/// Leaves state unchanged when the session isn't listed yet (still booting).
async fn reconcile_opencode_status(
    client: &opencode::OpencodeClient,
    session_id: &str,
    pane_id: &str,
    config: &Arc<libslop::SlopdConfig>,
    panes: &PaneMap,
    event_tx: &EventTx,
) {
    let target = match client.session_status(session_id).await {
        Ok(Some(status)) => opencode::status_to_detailed(&status),
        Ok(None) => match client.session_ids().await {
            // Idle sessions are absent from /session/status; confirm existence → Ready.
            Ok(ids) if ids.iter().any(|id| id == session_id) => Some(libslop::PaneDetailedState::Ready),
            _ => None,
        },
        Err(_) => None,
    };
    if let Some(new) = target {
        let current = panes.get_or_insert(pane_id).detailed_state.lock().unwrap().clone();
        if new != current {
            set_pane_detailed_state(config, pane_id, &new, Some(&current), event_tx, panes).await;
        }
    }
}

/// Reconcile the live-subagent set against the authoritative `/session/status`.
///
/// The SSE reader adds a child on `session.created` and removes it on the child's
/// `session.idle`/`deleted`/`error`. But that terminal event can be missed — an
/// SSE reconnect between spawn and finish, or a child that never emits it because
/// it is wedged in `retry` (rate-limit backoff). Left unchecked, the stale entry
/// pins the pane to `busy_subagent` indefinitely and poisons every main-session
/// event. This backstop drops any tracked child that is not currently *working*
/// per `/session/status` (finished ⇒ absent; wedged ⇒ `retry`), after a short
/// grace so a just-spawned child isn't pruned before it reports busy. Returns the
/// number of subagents still live and emits a synthetic `SubagentStop` for each
/// pruned child so `wait`/`listen` stay correct without the SSE event.
async fn reconcile_opencode_subagents(
    client: &opencode::OpencodeClient,
    session_id: &str,
    pane_id: &str,
    subagents: &SubagentSet,
    event_tx: &EventTx,
) -> usize {
    // On an API error, leave the set unchanged rather than churn state.
    let map = match client.status_map().await {
        Ok(m) => m,
        Err(_) => return subagents.lock().unwrap().len(),
    };
    let now = std::time::Instant::now();
    let mut stopped: Vec<String> = Vec::new();
    let remaining = {
        let mut set = subagents.lock().unwrap();
        set.retain(|sid, added| {
            let working = map.get(sid).map(opencode::status_is_working).unwrap_or(false);
            if working || now.duration_since(*added) < SUBAGENT_PRUNE_GRACE {
                return true;
            }
            stopped.push(sid.clone());
            false
        });
        set.len()
    };
    for sid in stopped {
        debug!("opencode pane {}: pruning stale subagent {} (not working in /session/status)", pane_id, sid);
        let _ = event_tx.send(libslop::Record {
            source: "hook".to_string(),
            event_type: "SubagentStop".to_string(),
            pane_id: Some(pane_id.to_string()),
            payload: serde_json::json!({
                "session_id": session_id,
                "hook_event_name": "SubagentStop",
                "opencode_child_session": sid,
            }),
            cursor: None,
        });
    }
    remaining
}

/// SSE reader: connect `GET /event`, parse `data:` payloads, apply state +
/// transcript updates for this pane's session. Reconnects with backoff on error.
#[allow(clippy::too_many_arguments)] // wiring fn threading shared daemon state
async fn run_opencode_sse(
    client: opencode::OpencodeClient,
    session_id: String,
    pane_id: String,
    config: Arc<libslop::SlopdConfig>,
    panes: PaneMap,
    event_tx: EventTx,
    cancel: tokio_util::sync::CancellationToken,
    subagents: SubagentSet,
) {
    let mut backoff = std::time::Duration::from_secs(1);
    loop {
        if cancel.is_cancelled() {
            return;
        }
        match client.events().send().await {
            Ok(resp) => {
                backoff = std::time::Duration::from_secs(1);
                if let Err(e) = read_opencode_sse(resp, &session_id, &pane_id, &config, &panes, &event_tx, &cancel, &subagents).await {
                    debug!("opencode SSE stream for {} ended: {}", pane_id, e);
                }
            }
            Err(e) => debug!("opencode SSE connect failed for {}: {}", pane_id, e),
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(std::time::Duration::from_secs(15));
    }
}

/// Read one SSE connection to completion, dispatching each session-scoped event.
#[allow(clippy::too_many_arguments)] // wiring fn threading shared daemon state
async fn read_opencode_sse(
    mut resp: reqwest::Response,
    session_id: &str,
    pane_id: &str,
    config: &Arc<libslop::SlopdConfig>,
    panes: &PaneMap,
    event_tx: &EventTx,
    cancel: &tokio_util::sync::CancellationToken,
    subagents: &SubagentSet,
) -> Result<(), String> {
    let mut buf = String::new();
    // Child sessions spawned by this pane's main session (opencode subagents) live
    // in the shared `subagents` set: added here on `session.created`, removed on
    // the child's terminal event, and pruned by the backstop if that event is
    // missed. Persists across SSE reconnects (the set is owned by the driver), so a
    // subagent that spans a reconnect stays tracked.
    // The main session slopd currently tracks. Seeded from the spawn-time id but
    // re-pointed when the human navigates the TUI to another session (see the
    // `tui.session.select` handling below) — so `ps`/`send`/`transcript` follow
    // whatever conversation the pane is actually showing.
    let mut current_session = session_id.to_string();
    loop {
        let chunk = tokio::select! {
            c = resp.chunk() => c.map_err(|e| e.to_string())?,
            _ = cancel.cancelled() => return Ok(()),
        };
        let Some(chunk) = chunk else { return Ok(()); };
        buf.push_str(&String::from_utf8_lossy(&chunk));
        // SSE events are separated by a blank line.
        while let Some(idx) = buf.find("\n\n") {
            let block: String = buf.drain(..idx + 2).collect();
            let Some(payload) = extract_sse_data(&block) else { continue };
            let Some(event) = opencode::event_from_line(&payload) else { continue };

            // The human switched the TUI to another session — follow it: re-point
            // the shared OpencodeState (so send/interrupt/transcript target it),
            // persist the new id on the pane, and forget the old session's
            // subagents. Skip no-op re-selects of the session we already track.
            if let Some(selected) = opencode::tui_selected_session(&event) {
                if selected != current_session {
                    debug!("opencode pane {}: TUI selected session {} (was {})", pane_id, selected, current_session);
                    current_session = selected.to_string();
                    subagents.lock().unwrap().clear();
                    // Re-point the shared state so send/interrupt/transcript target
                    // the followed session; grab a client clone for the reconcile.
                    let client = panes.get(pane_id).and_then(|state| {
                        state.note_session_id(&current_session);
                        state.update_opencode_session(current_session.clone())
                    });
                    let _ = tmux_set_pane_option(config, pane_id, libslop::TmuxOption::SlopdSessionId.as_str(), &current_session).await;
                    // Re-reconcile so state reflects the newly-followed session (it
                    // may be idle, so no further events would arrive on their own).
                    if let Some(client) = client {
                        reconcile_opencode_status(&client, &current_session, pane_id, config, panes, event_tx).await;
                    }
                }
                continue;
            }

            let ev_sid = opencode::event_session_id(&event).map(str::to_string);

            // Register a child session spawned by this pane (opencode subagent).
            if opencode::event_type(&event) == Some("session.created")
                && opencode::session_created_parent(&event) == Some(current_session.as_str())
            {
                if let Some(ref sid) = ev_sid {
                    subagents.lock().unwrap().insert(sid.clone(), std::time::Instant::now());
                    // Synthesize a SubagentStart hook (opencode subagent = child session).
                    let _ = event_tx.send(libslop::Record {
                        source: "hook".to_string(),
                        event_type: "SubagentStart".to_string(),
                        pane_id: Some(pane_id.to_string()),
                        payload: serde_json::json!({
                            "session_id": current_session,
                            "hook_event_name": "SubagentStart",
                            "opencode_child_session": sid,
                            "properties": event.get("properties").cloned().unwrap_or(serde_json::Value::Null),
                        }),
                        cursor: None,
                    });
                }
            }
            let is_main = ev_sid.as_deref() == Some(current_session.as_str());
            let is_child = ev_sid.as_deref().is_some_and(|s| subagents.lock().unwrap().contains_key(s));
            if !is_main && !is_child {
                continue;
            }
            // A child session ending → drop it so the main session's state resurfaces.
            if is_child
                && matches!(
                    opencode::event_type(&event),
                    Some("session.idle") | Some("session.deleted") | Some("session.error")
                )
            {
                if let Some(ref sid) = ev_sid {
                    subagents.lock().unwrap().remove(sid);
                    let _ = event_tx.send(libslop::Record {
                        source: "hook".to_string(),
                        event_type: "SubagentStop".to_string(),
                        pane_id: Some(pane_id.to_string()),
                        payload: serde_json::json!({
                            "session_id": current_session,
                            "hook_event_name": "SubagentStop",
                            "opencode_child_session": sid,
                        }),
                        cursor: None,
                    });
                }
            }

            // STATE: an active child session (subagent) overrides → busy_subagent.
            let target = if !subagents.lock().unwrap().is_empty() {
                Some(libslop::PaneDetailedState::BusySubagent)
            } else if is_main {
                opencode::event_to_detailed(&event)
            } else {
                None
            };
            if let Some(new) = target {
                let current = panes.get_or_insert(pane_id).detailed_state.lock().unwrap().clone();
                if new != current {
                    set_pane_detailed_state(config, pane_id, &new, Some(&current), event_tx, panes).await;
                }
            }

            // Side effects apply to the main session only (a subagent's internal
            // traffic isn't this pane's transcript/hooks).
            if !is_main {
                continue;
            }
            // A clean turn end (session.idle) resets the auto-continue retry budget.
            if opencode::event_type(&event) == Some("session.idle") {
                *panes.get_or_insert(pane_id).retry_state.lock().unwrap() = None;
            }
            // Transcript record (live `listen --transcript`).
            if let Some((rtype, payload)) = opencode::event_to_transcript(&event) {
                let _ = event_tx.send(libslop::Record {
                    source: "transcript".to_string(),
                    event_type: rtype,
                    pane_id: Some(pane_id.to_string()),
                    payload,
                    cursor: None,
                });
            }
            // Synthesized hook event (unifies `listen --hook`/`wait --hook` across
            // backends — opencode has no native hooks, so we emit hook-NAMED events
            // derived from its bus).
            if let Some((hook_name, payload)) = opencode::event_to_hook(&event) {
                let _ = event_tx.send(libslop::Record {
                    source: "hook".to_string(),
                    event_type: hook_name.to_string(),
                    pane_id: Some(pane_id.to_string()),
                    payload,
                    cursor: None,
                });
            }
            // Auto-continue: a failed turn (session.error) re-sends the last prompt.
            if opencode::event_is_failure(&event) {
                schedule_opencode_auto_continue(pane_id, config, panes).await;
            }
        }
    }
}

/// Concatenate the `data:` lines of one SSE event block into its payload.
fn extract_sse_data(block: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            parts.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// OpenCode analogue of Claude's `StopFailure` auto-continue: on a
/// `session.error` event, re-send the last non-command prompt via `prompt_async`
/// with exponential backoff, up to `[run] max_retry_attempts`. Mirrors the Claude
/// retry path (`expecting_auto_continue` + `RetryState`) so a manual prompt or a
/// later successful turn cancels a pending retry.
async fn schedule_opencode_auto_continue(
    pane_id: &str,
    config: &Arc<libslop::SlopdConfig>,
    panes: &PaneMap,
) {
    if !config.run.auto_continue_on_failure {
        return;
    }
    let pane_state = panes.get_or_insert(pane_id);
    let oc = match pane_state.opencode() {
        Some(oc) => oc,
        None => return,
    };
    let prompt = match oc.last_prompt.lock().unwrap().clone() {
        Some(p) => p,
        None => return,
    };
    let policy = BackoffPolicy::from_config(&config.run);
    let (attempt, send_at) = {
        let mut guard = pane_state.retry_state.lock().unwrap();
        match RetryState::next(guard.as_ref(), &policy, tokio::time::Instant::now()) {
            Some(n) => {
                let attempt = n.attempt_count;
                let send_at = n.next_send_at;
                *guard = Some(n);
                (attempt, send_at)
            }
            None => {
                *guard = None;
                debug!("opencode session.error: pane {} exceeded max retry attempts, giving up", pane_id);
                return;
            }
        }
    };
    pane_state
        .expecting_auto_continue
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let client = oc.client;
    let session_id = oc.session_id;
    let pid = pane_id.to_string();
    let panes = panes.clone();
    tokio::spawn(async move {
        let delay = send_at.saturating_duration_since(tokio::time::Instant::now());
        if !delay.is_zero() {
            debug!("opencode session.error: pane {} will retry in {:?}", pid, delay);
            tokio::time::sleep(delay).await;
        }
        // Re-validate the retry is still current (a manual prompt or a successful
        // turn resets retry_state / clears the last prompt).
        let still_valid = panes
            .get(&pid)
            .map(|s| {
                s.retry_state
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|r| r.matches(attempt, send_at))
            })
            .unwrap_or(false);
        if !still_valid {
            return;
        }
        debug!("opencode session.error: retrying pane {} (attempt {})", pid, attempt);
        if client.send_message(&session_id, &prompt).await.is_ok() {
            if let Some(s) = panes.get(&pid) {
                s.prompt_submitted.notify_waiters();
            }
        }
    });
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
                        let backend = pane_state.runtime().backend();
                        let record_type = record
                            .get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string();

                        // A queue-operation enqueue means a prompt was accepted
                        // while Claude was busy. Notify pending senders so
                        // slopctl send unblocks immediately.
                        if backend == libslop::Backend::Claude
                            && record_type == "queue-operation"
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
                        let is_slash_command_record = backend == libslop::Backend::Claude
                            && match record_type.as_str() {
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
                        if backend == libslop::Backend::Codex
                            && codex::prompt_submitted(&record)
                        {
                            pane_state.prompt_submitted.notify_waiters();
                        }
                        if is_slash_command_record {
                            debug!("transcript slash-command: notifying pending senders for pane {}", pane_id);
                            pane_state.prompt_submitted.notify_waiters();
                        }

                        // Check if this transcript record triggers a state transition.
                        {
                            let current = pane_state.detailed_state.lock().unwrap().clone();
                            let event = PaneStateEvent::TranscriptRecord {
                                backend,
                                record_type: &record_type,
                                record: &record,
                            };
                            if let Some(new_state) = reduce_pane_state(&current, &event) {
                                debug!("transcript {} event while pane {} in {:?} — transitioning to {:?}", record_type, pane_id, current, new_state);
                                set_pane_detailed_state(
                                    &config, &pane_id, &new_state,
                                    Some(&current), &event_tx, &panes,
                                ).await;
                            }
                        }

                        if let Some((event_type, payload)) =
                            decode_transcript_record(backend, &record)
                        {
                            let _ = event_tx.send(libslop::Record {
                                source: "transcript".to_string(),
                                event_type,
                                pane_id: Some(pane_id.clone()),
                                payload,
                                cursor: Some(line_start),
                            });
                        }
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
async fn read_raw_transcript_tail(
    path: &std::path::Path,
    n: u64,
) -> std::io::Result<(Vec<(u64, serde_json::Value)>, u64)> {
    use tokio::io::AsyncBufReadExt;

    let file = tokio::fs::File::open(path).await?;
    let file_len = file.metadata().await?.len();
    if n == 0 {
        return Ok((Vec::new(), file_len));
    }
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

fn decode_transcript_record(
    backend: libslop::Backend,
    record: &serde_json::Value,
) -> Option<(String, serde_json::Value)> {
    match backend {
        libslop::Backend::Codex => codex::transcript_record(record),
        libslop::Backend::Claude => Some((
            record
                .get("type")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown")
                .to_string(),
            record.clone(),
        )),
        libslop::Backend::Opencode => None,
    }
}

async fn read_transcript_tail(
    path: &std::path::Path,
    n: u64,
    backend: libslop::Backend,
) -> std::io::Result<(Vec<(u64, String, serde_json::Value)>, u64)> {
    use tokio::io::AsyncBufReadExt;

    let file = tokio::fs::File::open(path).await?;
    let file_len = file.metadata().await?.len();
    if n == 0 {
        return Ok((Vec::new(), file_len));
    }
    let mut reader = tokio::io::BufReader::new(file);
    let mut line = String::new();
    let mut byte_pos = 0_u64;
    let mut window = std::collections::VecDeque::with_capacity(n as usize + 1);
    while reader.read_line(&mut line).await? != 0 {
        let line_start = byte_pos;
        byte_pos += line.len() as u64;
        if let Ok(raw) = serde_json::from_str::<serde_json::Value>(line.trim())
            && let Some((event_type, payload)) = decode_transcript_record(backend, &raw)
        {
            if window.len() == n as usize {
                window.pop_front();
            }
            window.push_back((line_start, event_type, payload));
        }
        line.clear();
    }
    Ok((window.into(), file_len))
}

/// Read up to `limit` public transcript records that start
/// strictly before `before_offset` bytes. Returns `(records, at_beginning)`
/// where records are ordered oldest-first.
async fn read_transcript_before(
    path: &std::path::Path,
    before_offset: u64,
    limit: u64,
    backend: libslop::Backend,
) -> std::io::Result<(Vec<(u64, String, serde_json::Value)>, bool)> {
    use tokio::io::AsyncBufReadExt;

    let file = tokio::fs::File::open(path).await?;
    if limit == 0 {
        return Ok((Vec::new(), before_offset == 0));
    }
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
                if let Ok(record) = serde_json::from_str::<serde_json::Value>(trimmed)
                    && let Some((event_type, payload)) =
                        decode_transcript_record(backend, &record)
                {
                    if window.len() == limit {
                        window.pop_front();
                    }
                    window.push_back((line_start, event_type, payload));
                }
            }
        }
    }

    let at_beginning = window.front().is_none_or(|(offset, _, _)| *offset == 0);
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

/// Read a pane's recorded opencode HTTP port (`@slopd_opencode_port`). `None`
/// if the option is unset (not an opencode pane) or unparseable.
async fn read_pane_opencode_port(config: &libslop::SlopdConfig, pane_id: &str) -> Option<u16> {
    let out = tmux(config)
        .args(["show-options", "-t", pane_id, "-p", "-v",
               libslop::TmuxOption::SlopdOpencodePort.as_str()])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Scan the slopd session for managed panes and return the distinct
/// backend hook files whose hooks must be re-injected — one per account/backend
/// a recovered pane belongs to (the unnamed default included).
///
/// Only panes that have the `@slopd_managed` pane option set are considered managed
/// (i.e. were registered via `slopctl run`). For each recovered pane, replays the
/// last N transcript records through the state reducer to recover the real state
/// instead of leaving it stuck at BootingUp.
async fn load_managed_panes(config: &Arc<libslop::SlopdConfig>, managed: &ManagedPanes, event_tx: &EventTx, panes: &PaneMap) -> std::collections::HashSet<(std::path::PathBuf, libslop::Backend)> {
    let mut hook_paths = std::collections::HashSet::new();
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
    let Ok(out) = output else { return hook_paths };
    if !out.status.success() {
        return hook_paths;
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
        let mut resolved = config
            .resolve_account(opts.account.as_deref())
            .or_else(|_| config.resolve_account(Some(libslop::DEFAULT_ACCOUNT)))
            .expect("the reserved default account always resolves");
        let recovered_backend = opts.backend.unwrap_or(libslop::Backend::Claude);
        resolved.backend = recovered_backend;
        panes.get_or_insert(pane_id).mark_unbound_backend(recovered_backend);
        if recovered_backend.uses_injected_hooks() {
            hook_paths.insert((
                config.resolved_hook_path(&resolved),
                recovered_backend,
            ));
        }

        // Replay the last N transcript records to recover the real state.
        let transcript_path = opts.transcript_path.clone();
        let recovered_state = match transcript_path.as_deref() {
            Some(path) => recover_state_from_transcript(path, recovered_backend).await,
            None => None,
        };
        let initial_state = recovered_state
            .or_else(|| opts.detailed_state.clone())
            .unwrap_or(libslop::PaneDetailedState::BootingUp);
        set_pane_detailed_state(config, pane_id, &initial_state, None, event_tx, panes).await;

        // Rebuild the identity snapshot from the pane's tmux options so a pane
        // recovered across a daemon restart is just as describable at death as a
        // freshly-spawned one (the death record's only other source, the pane's
        // options, is gone by the time it dies). working_dir isn't a slopd option
        // so it stays unset here; a later `ps` fills the title.
        {
            let state = panes.get_or_insert(pane_id);
            let mut id = state.identity.lock().unwrap();
            id.backend = opts.backend.unwrap_or(libslop::Backend::Claude);
            // Immediate parent is the first ancestor; read the field directly since
            // `opts` is already partially moved (transcript_path) above, which would
            // block the `parent_pane_id()` method's whole-`self` borrow.
            id.parent_pane_id = opts.ancestor_panes.first().cloned();
            id.created_at = opts.created_at;
            if let Some(sid) = opts.session_id.as_ref().filter(|s| !s.is_empty()) {
                id.session_id = Some(sid.clone());
            }
        }

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

        if let Err(error) = backend_lifecycle(recovered_backend).recover(RecoverContext {
            pane_id, options: &opts, config, panes, event_tx,
        }).await {
            warn!("failed to recover {} pane {}: {}", recovered_backend.canonical_executable(), pane_id, error);
        }
    }
    hook_paths
}

/// Replay the last N records from a transcript file through the state reducer
/// to recover the pane's actual state after a slopd restart.
async fn recover_state_from_transcript(
    transcript_path: &str,
    backend: libslop::Backend,
) -> Option<libslop::PaneDetailedState> {
    let path = std::path::Path::new(transcript_path);
    let (records, _) = read_raw_transcript_tail(path, 100).await.ok()?;
    if records.is_empty() {
        return None;
    }

    // Replay records through the reducer starting from BootingUp.
    let mut state = libslop::PaneDetailedState::BootingUp;
    for (_offset, record) in &records {
        let record_type = record.get("type").and_then(|v| v.as_str()).unwrap_or("unknown");
        let event = PaneStateEvent::TranscriptRecord {
            backend,
            record_type,
            record,
        };
        if let Some(new_state) = reduce_pane_state(&state, &event) {
            state = new_state;
        }
    }
    Some(state)
}

type EventTx = Arc<tokio::sync::broadcast::Sender<libslop::Record>>;
type PaneRegistered = Arc<tokio::sync::Notify>;
/// The most recent time each relevant tmux lifecycle hook fired, shared between
/// the TmuxHook handler and the reconcile loop. When a pane is found to have
/// vanished, a hook that landed just before it disambiguates the cause. tmux
/// fires these with no pane id (the pane is already gone), so temporal
/// correlation is the only signal available — and both may fire for one kill
/// (killing a window's last pane fires `after-kill-pane` *and* `window-unlinked`),
/// so they are tracked separately and `after-kill-pane`, the deliberate-kill
/// signal, is preferred over `window-unlinked`, which also fires for plain
/// window closes.
#[derive(Default)]
struct RecentHooks {
    after_kill_pane: Option<std::time::Instant>,
    window_unlinked: Option<std::time::Instant>,
}
type HookLog = Arc<std::sync::Mutex<RecentHooks>>;
/// How recently a tmux hook must have fired to be attributed to a vanished pane.
const HOOK_CORRELATION_WINDOW: std::time::Duration = std::time::Duration::from_secs(3);
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

/// The single chokepoint for recording a pane's death. Every teardown path
/// (explicit kill, process exit, external removal, lost server) routes through
/// here so the death is described exactly once, completely, and consistently:
///
///  1. a structured log line to stderr → the systemd journal (`journalctl --user
///     -u slopd`). Abnormal deaths (vanished / nonzero exit / server gone) log at
///     WARN so they are visible at slopd's default verbosity; routine ones (a
///     clean `slopctl kill` or zero exit) log at INFO.
///  2. the `PaneDestroyed` broadcast, enriched with the same fields so a live
///     `slopctl` listener sees them too (and `slopctl run` keeps reading the
///     `exit_status`/`output` it always has).
///
/// `state` is the [`PaneState`] the caller just removed from the map — its
/// identity snapshot is the only surviving description of the pane once tmux has
/// dropped it. This makes "what happened to pane %N?" answerable from the journal
/// alone, which is exactly what a lingering `pane=None` kill-hook could not do.
#[allow(clippy::too_many_arguments)]
fn record_pane_death(
    event_tx: &EventTx,
    pane_id: &str,
    cause: DeathCause,
    detected_by: DeathDetectedBy,
    state: Option<&Arc<PaneState>>,
    exit_status: Option<i64>,
    output_tail: Option<String>,
    preceding_hook: Option<String>,
) {
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let (identity, last_state) = match state {
        Some(s) => (
            s.identity.lock().unwrap().clone(),
            Some(s.detailed_state.lock().unwrap().clone()),
        ),
        None => (PaneIdentity::default(), None),
    };
    let lived_secs = identity.created_at.map(|c| ts.saturating_sub(c));
    let output_tail = output_tail.filter(|s| !s.is_empty());
    let last_state_str = last_state.as_ref().map(|s| s.as_str());

    // 1. Structured journal line — one line, all forensic fields, greppable.
    let line = format!(
        "pane {} died: cause={} detected_by={} backend={} session={} parent={} state={} exit={} lived={} title={:?} cwd={} preceding_hook={}",
        pane_id,
        cause.as_str(),
        detected_by.as_str(),
        identity.backend.canonical_executable(),
        identity.session_id.as_deref().unwrap_or("?"),
        identity.parent_pane_id.as_deref().unwrap_or("-"),
        last_state_str.unwrap_or("?"),
        exit_status.map(|c| c.to_string()).unwrap_or_else(|| "-".into()),
        lived_secs.map(|s| format!("{s}s")).unwrap_or_else(|| "?".into()),
        identity.title.as_deref().unwrap_or("-"),
        identity.working_dir.as_deref().unwrap_or("-"),
        preceding_hook.as_deref().unwrap_or("-"),
    );
    if cause.is_abnormal(exit_status) {
        warn!("{line}");
    } else {
        info!("{line}");
    }
    // For an abnormal SelfExit the pane printed its dying words; surface them too
    // (the ephemeral PaneDestroyed broadcast may have no listener at death time).
    if let Some(out) = output_tail.as_ref().filter(|_| cause.is_abnormal(exit_status)) {
        warn!("pane {} death output:\n{}", pane_id, out);
    }

    // 2. Enriched PaneDestroyed broadcast. Keeps the legacy `exit_status`/`output`
    // keys `slopctl run` reads, plus the full identity/cause for richer listeners.
    let mut payload = serde_json::json!({
        "pane_id": pane_id,
        "cause": cause.as_str(),
        "detected_by": detected_by.as_str(),
        "backend": identity.backend.canonical_executable(),
        "ts": ts,
    });
    if let Some(sid) = identity.session_id {
        payload["session_id"] = serde_json::json!(sid);
    }
    if let Some(parent) = identity.parent_pane_id {
        payload["parent_pane_id"] = serde_json::json!(parent);
    }
    if let Some(cwd) = identity.working_dir {
        payload["working_dir"] = serde_json::json!(cwd);
    }
    if let Some(title) = identity.title {
        payload["title"] = serde_json::json!(title);
    }
    if let Some(s) = last_state_str {
        payload["last_state"] = serde_json::json!(s);
    }
    if let Some(spawned) = identity.created_at {
        payload["spawned_at"] = serde_json::json!(spawned);
    }
    if let Some(l) = lived_secs {
        payload["lived_secs"] = serde_json::json!(l);
    }
    if let Some(code) = exit_status {
        payload["exit_status"] = serde_json::json!(code);
    }
    if let Some(out) = output_tail {
        payload["output"] = serde_json::json!(out);
    }
    if let Some(hook) = preceding_hook {
        payload["preceding_hook"] = serde_json::json!(hook);
    }
    let _ = event_tx.send(libslop::Record {
        source: "slopd".to_string(),
        event_type: "PaneDestroyed".to_string(),
        pane_id: Some(pane_id.to_string()),
        payload,
        cursor: None,
    });
}

/// Attribute a vanished pane's cause to a recent tmux hook, if one fired within
/// [`HOOK_CORRELATION_WINDOW`]. `after-kill-pane` (an explicit external
/// kill-pane/kill-window) is preferred over `window-unlinked` (which also fires
/// as a side effect of that same kill, and for plain window closes), so a
/// deliberate external kill is reported as such even when both hooks landed.
fn recent_hook(hook_log: &HookLog) -> Option<String> {
    let fresh = |at: Option<std::time::Instant>| at.is_some_and(|t| t.elapsed() <= HOOK_CORRELATION_WINDOW);
    let hooks = hook_log.lock().unwrap();
    if fresh(hooks.after_kill_pane) {
        Some("after-kill-pane".to_string())
    } else if fresh(hooks.window_unlinked) {
        Some("window-unlinked".to_string())
    } else {
        None
    }
}

/// Reconcile managed_panes against live tmux panes, emitting PaneDestroyed
/// for any managed pane that no longer exists.
async fn reconcile_panes(
    config: &libslop::SlopdConfig,
    panes: &PaneMap,
    managed_panes: &ManagedPanes,
    event_tx: &EventTx,
    hook_log: &HookLog,
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
    // `server_gone` distinguishes a vanished-pane cause: when the whole tmux
    // server/session is gone every managed pane died together (ServerGone),
    // versus a single pane externally removed while the server lived (Vanished).
    let mut server_gone = false;
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
            server_gone = true;
            (std::collections::HashSet::new(), std::collections::HashMap::new())
        }
        _ => return,
    };

    // Test hook: simulate the production failure mode where `tmux list-panes`
    // transiently returned without our managed panes.  Used by the reconcile
    // false-positive regression test. Compiled only under the `testing` feature so
    // the env-var branch never exists in a production daemon.
    #[cfg(feature = "testing")]
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

        reparent_children_of(config, managed_panes, &pane_id).await;
        let state = panes.remove(&pane_id);
        if let Some(ref state) = state {
            state.cancel_drivers();
        }
        managed_panes.remove(&pane_id);
        // A gone server killed every pane at once (ServerGone); otherwise this
        // single pane was removed out from under slopd (Vanished). For a vanished
        // pane, correlate the most recent tmux lifecycle hook to say whether it
        // was an external kill-pane (`after-kill-pane`) or a closed window.
        let cause = if server_gone { DeathCause::ServerGone } else { DeathCause::Vanished };
        let preceding_hook = (cause == DeathCause::Vanished).then(|| recent_hook(hook_log)).flatten();
        record_pane_death(
            event_tx,
            &pane_id,
            cause,
            DeathDetectedBy::ReconcileVanished,
            state.as_ref(),
            None,
            None,
            preceding_hook,
        );
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

/// Extract a resume-target session id from `slopctl run`'s passthrough args.
/// Accepts the uniform `--resume <id>` (matching Claude's own flag) as well as
/// opencode's native `-s <id>` / `--session <id>`, plus the `--flag=<id>`
/// spellings. Returns the first id found. The run handler uses this to bind an
/// opencode pane to the resumed session instead of POSTing a fresh one (which
/// would strand the resumed conversation on a new empty session).
fn extract_resume_target(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--resume" | "-s" | "--session" => {
                if let Some(v) = it.next() {
                    if !v.starts_with('-') {
                        return Some(v.clone());
                    }
                }
            }
            other => {
                for pfx in ["--resume=", "--session=", "-s="] {
                    if let Some(v) = other.strip_prefix(pfx) {
                        return Some(v.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Remove every resume flag (and its separate value) from an arg list, in any
/// spelling [`extract_resume_target`] recognizes. The run handler uses this to
/// re-express an opencode resume as the canonical `-s <id>`: opencode rejects
/// `--resume`, prints its usage, and exits — killing the pane.
fn strip_resume_flags(args: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--resume" | "-s" | "--session" => {
                // Drop the flag and its value token.
                let _ = it.next();
            }
            other
                if other.starts_with("--resume=")
                    || other.starts_with("--session=")
                    || other.starts_with("-s=") => {}
            _ => out.push(a),
        }
    }
    out
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

    reparent_children_of(config, managed_panes, pane_id).await;
    let state = panes.remove(pane_id);
    if let Some(ref state) = state {
        state.cancel_drivers();
    }
    managed_panes.remove(pane_id);

    // Record the death once, completely: the process exited on its own, so this
    // is a SelfExit carrying its exit status and the dying-words screen tail.
    // record_pane_death logs it (WARN when the exit was nonzero, so it stays in
    // the journal at default verbosity) and broadcasts the enriched
    // PaneDestroyed that `slopctl run` reads.
    record_pane_death(
        event_tx,
        pane_id,
        DeathCause::SelfExit,
        DeathDetectedBy::ReconcileDeadPane,
        state.as_ref(),
        exit_status,
        Some(output_tail),
        None,
    );

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
        for (hook_path, backend) in config.all_hook_paths() {
            if let Err(e) = libslop::remove_backend_hooks_from_file(&hook_path, backend) {
                error!("failed to remove hooks from {}: {}", hook_path.display(), e);
                failed = true;
            } else {
                info!("removed slopctl hooks from {}", hook_path.display());
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
    // Tracks the most recent tmux lifecycle hooks so a vanished pane can be
    // attributed to an external kill vs a closed window (see [`HookLog`]).
    let hook_log: HookLog = Arc::new(std::sync::Mutex::new(RecentHooks::default()));

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
    let recovered_hook_paths = load_managed_panes(&config, &managed_panes, &event_tx, &panes).await;

    // Re-inject hooks if there are recovered panes — the previous slopd instance
    // removed them on exit, but the Claude sessions are still running in tmux.
    // Each recovered pane reports its account, so we re-inject only the dirs that
    // are actually in use rather than every configured account.
    for (hook_path, backend) in &recovered_hook_paths {
        if let Err(e) = libslop::inject_backend_hooks_into_file(
            hook_path,
            &config.hook_slopctl(),
            *backend,
        ) {
            warn!("failed to re-inject hooks into {}: {}", hook_path.display(), e);
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
    let reconcile_hook_log = hook_log.clone();
    // The background reconcile is a backstop for deaths the tmux hooks miss.
    // Tests that assert the hook-driven path (e.g. tmux-hook cause correlation)
    // lengthen this so only the hook path fires; production is always 2s.
    let reconcile_interval_ms: u64 = 2000;
    #[cfg(feature = "testing")]
    let reconcile_interval_ms = std::env::var("SLOPD_TEST_RECONCILE_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(reconcile_interval_ms);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(reconcile_interval_ms));
        loop {
            interval.tick().await;
            // Snapshot the current config for this reconcile pass.
            let config_snapshot = reconcile_config_rx.borrow().clone();
            reconcile_panes(&config_snapshot, &reconcile_panes_map, &reconcile_managed, &reconcile_tx, &reconcile_hook_log).await;
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
                tokio::spawn(handle_connection(stream, start_time, config_snapshot, panes.clone(), managed_panes.clone(), event_tx.clone(), pane_registered.clone(), session_lock.clone(), config_generation.clone(), pending_restore.clone(), hook_log.clone()));
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
    for (hook_path, backend) in shutdown_config.all_hook_paths() {
        if let Err(e) = libslop::remove_backend_hooks_from_file(&hook_path, backend) {
            warn!("failed to remove hooks from {} on shutdown: {}", hook_path.display(), e);
        } else {
            info!("removed slopctl hooks from {}", hook_path.display());
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
    hook_log: HookLog,
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
                let pane_state = panes.get(&pane_id);
                let transcript_path = pane_state
                    .as_ref()
                    .and_then(|state| state.transcript_path.lock().unwrap().clone());
                let backend = pane_state
                    .map(|state| state.runtime().backend())
                    .unwrap_or(libslop::Backend::Claude);

                let (records, file_end_offset) = match transcript_path {
                    Some(ref path) => {
                        let path = std::path::PathBuf::from(path);
                        match read_transcript_tail(&path, last_n, backend).await {
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
                for (cursor, event_type, payload) in &records {
                    let record = libslop::Record {
                        cursor: Some(*cursor),
                        source: "transcript".to_string(),
                        event_type: event_type.clone(),
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
                let body = handle_request(body, start_time, &config, &panes, &managed_panes, &event_tx, &pane_registered, &session_lock, &config_generation, &pending_restore, &hook_log).await;
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
        } else if key == libslop::TmuxOption::SlopdSessionId.as_str()
            || key == "@slopd_claude_session_id" {
            // The second clause is a migration fallback: panes created before the
            // rename still carry the old option name. Reading both lets daemon
            // recovery pick up existing panes after a deploy.
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
                "codex" => Some(libslop::Backend::Codex),
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
        title: Option<String>,
        opts: ParsedPaneOptions,
    }
    let mut raw_panes = Vec::new();
    for pane_id in managed_panes.snapshot() {
        // Tab-delimited so the path and the (space-bearing) pane title stay
        // separable; window_activity is numeric so it never contains a tab.
        let dm_out = tmux(config)
            .args(["display-message", "-p", "-t", &pane_id, "-F",
                   "#{window_activity}\t#{pane_current_path}\t#{pane_title}"])
            .output()
            .await;
        let (last_active, working_dir, title) = match dm_out {
            Ok(out) if out.status.success() => {
                let line = String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_string();
                let mut parts = line.splitn(3, '\t');
                let activity: u64 = parts.next().unwrap_or("0").trim().parse().unwrap_or(0);
                let cwd = parts.next().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
                let title = parts.next().and_then(libslop::normalize_pane_title);
                (activity, cwd, title)
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

        raw_panes.push(RawPane { pane_id, last_active, working_dir, title, opts });
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
            pane_title: raw.title,
        });
    }
    // managed_panes is a DashSet, so its iteration order is hash-arbitrary and
    // varies between slopd instances. Sort by the pane's numeric tmux id (`%N`)
    // so `ps` (and `ps --json`) list panes in a stable, intuitive spawn order
    // instead of shuffling. Ids that don't parse sort last, by string, as a
    // deterministic fallback.
    panes.sort_by_key(|p| pane_id_sort_key(&p.pane_id));
    Ok(panes)
}

/// Sort key for a tmux pane id: `%N` → `(0, N, "")` so panes order by their
/// numeric id; anything unparseable → `(1, 0, id)` so it sorts last but stably.
fn pane_id_sort_key(pane_id: &str) -> (u8, u64, String) {
    match pane_id.strip_prefix('%').and_then(|n| n.parse::<u64>().ok()) {
        Some(n) => (0, n, String::new()),
        None => (1, 0, pane_id.to_string()),
    }
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
/// [`spawn_pane`], the single chokepoint both go through.
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
async fn spawn_pane(
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
/// the user fixes their PATH. The actual spawn in [`spawn_pane`] resolves
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

/// The launch cwd recorded in an agent transcript. Claude stores it at top
/// level; Codex stores it in the `session_meta` payload.
fn transcript_launch_cwd(transcript_path: &str) -> Option<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(transcript_path).ok()?;
    // The cwd appears in the earliest records; scan a bounded prefix so a huge
    // transcript is never read end-to-end.
    for line in std::io::BufReader::new(file).lines().take(50) {
        let Ok(line) = line else { break };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        if let Some(cwd) = value
            .get("cwd")
            .or_else(|| value.pointer("/payload/cwd"))
            .and_then(|c| c.as_str())
            && !cwd.is_empty()
        {
            return Some(cwd.to_string());
        }
    }
    None
}

/// Return the main-thread transcript advertised by a hook.
///
/// Codex's `SubagentStart.transcript_path` is the child agent's rollout (unlike
/// `SubagentStop`, which provides the child separately as
/// `agent_transcript_path` and keeps `transcript_path` on the main thread).
/// Switching the pane tailer to that child file corrupts main-thread state and
/// can make backup persist the wrong session path.
fn hook_transcript_path<'a>(
    backend: libslop::Backend,
    event: &str,
    payload: &'a serde_json::Value,
) -> Option<&'a str> {
    if backend == libslop::Backend::Codex && event == "SubagentStart" {
        return None;
    }
    payload.get("transcript_path").and_then(|value| value.as_str())
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
    // a second agent on an already-open session. Also seed the old→live pane map:
    // a missing child can then retain a parent that was skipped because it is
    // still running, rather than being incorrectly promoted to a root pane.
    let live_panes = list_panes(config, managed_panes).await.unwrap_or_default();
    let live_by_session: std::collections::HashMap<String, String> = live_panes
        .iter()
        .filter_map(|p| {
            p.session_id
                .clone()
                .map(|session| (session, p.pane_id.clone()))
        })
        .collect();
    let live_parent: std::collections::HashMap<String, String> = live_panes
        .iter()
        .filter_map(|p| {
            p.parent_pane_id
                .clone()
                .map(|parent| (p.pane_id.clone(), parent))
        })
        .collect();
    for live in &live_panes {
        let mut chain = Vec::new();
        let mut current = live.pane_id.as_str();
        let mut seen = std::collections::HashSet::new();
        while let Some(parent) = live_parent.get(current) {
            if !seen.insert(parent.clone()) {
                break;
            }
            chain.push(parent.clone());
            current = parent;
        }
        ancestors_of_new.insert(live.pane_id.clone(), chain);
    }
    for pane in &ordered {
        if let Some(session) = pane.session_id.as_deref()
            && let Some(live_id) = live_by_session.get(session)
        {
            old_to_new.insert(pane.pane_id.clone(), live_id.clone());
        }
    }
    // Two manifest entries can also share a session id — e.g. an in-pane
    // `claude --resume` overwrites a pane's recorded id — so add each session as
    // we restore it and resume every session at most once.
    let mut seen_sessions: std::collections::HashSet<String> =
        live_by_session.into_keys().collect();
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
        // config) for its executable + config dir. The BACKEND is taken from the
        // manifest entry (authoritative — a pane created via `--backend opencode`
        // on the default account must restore as opencode even though the default
        // account now resolves to claude).
        let mut resolved = config
            .resolve_account(Some(account.as_str()))
            .or_else(|_| config.resolve_account(Some(libslop::DEFAULT_ACCOUNT)))
            .expect("the reserved default account always resolves");
        let manifest_backend = p.backend;
        if resolved.backend != manifest_backend {
            resolved.backend = manifest_backend;
            // Recompute the executable to match: keep a custom/unrecognized path,
            // else swap a recognized name to the backend's canonical binary.
            resolved.executable = match libslop::Backend::infer_from_program(resolved.executable.program()) {
                Some(inferred) if inferred == manifest_backend => resolved.executable.clone(),
                Some(_) => libslop::Executable::String(manifest_backend.canonical_executable().to_string()),
                None => resolved.executable.clone(),
            };
        }
        // Resume the recorded session. Claude: `claude --resume` from the launch
        // cwd. OpenCode: `opencode -s <id>` over a freshly-allocated HTTP port,
        // then reattach the status-poll driver.
        let new_id = match backend_lifecycle(resolved.backend).restore(RestoreContext {
            old_pane_id: &old_id, session_id: &session_id, working_dir: &working_dir,
            transcript_path: &transcript_path, resolved: &resolved, config, session_lock,
            panes, event_tx,
        }).await {
            Ok(id) => id,
            Err(error) => {
                warn!("backup: {}", error);
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
        let _ = tmux_set_pane_option(config, &new_id, libslop::TmuxOption::SlopdSessionId.as_str(), &session_id).await;

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

        // A resumed Codex TUI is usable as soon as its history is rendered, but
        // (like a fresh/forked TUI) does not fire SessionStart until the next
        // prompt. Mark it ready now so send can cause that hook. Claude still
        // uses SessionStart as its boot authority; OpenCode's HTTP driver owns
        // its initial state and may already have advanced it.
        if resolved.backend == libslop::Backend::Codex {
            let current = panes
                .get_or_insert(&new_id)
                .detailed_state
                .lock()
                .unwrap()
                .clone();
            if current == libslop::PaneDetailedState::BootingUp {
                set_pane_detailed_state(
                    config,
                    &new_id,
                    &libslop::PaneDetailedState::Ready,
                    Some(&current),
                    event_tx,
                    panes,
                )
                .await;
            }
        } else if !resolved.backend.driver_owns_initial_state() {
            let current = panes
                .get_or_insert(&new_id)
                .detailed_state
                .lock()
                .unwrap()
                .clone();
            if current == libslop::PaneDetailedState::BootingUp {
                set_pane_detailed_state(
                    config,
                    &new_id,
                    &libslop::PaneDetailedState::BootingUp,
                    None,
                    event_tx,
                    panes,
                )
                .await;
            }
        }

        old_to_new.insert(old_id.clone(), new_id.clone());
        restored += 1;
        info!("backup: restored pane {} -> {} (session {})", old_id, new_id, session_id);
    }

    info!("backup: restored {}/{} recorded pane(s)", restored, total);
    restored
}

/// After sending the interrupt Escape on a `send --interrupt`, wait this long
/// before any further keystrokes. A lone Escape must register as an interrupt on
/// its own; if the next keystroke is glued to it the terminal reads the pair as
/// an escape sequence and swallows that keystroke (which ate the first character
/// of interrupt-delivered prompts). Comfortably larger than the mock terminal's
/// Escape window and any real terminal's ~25-50ms.
const INTERRUPT_SETTLE: std::time::Duration = std::time::Duration::from_millis(300);

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
    hook_log: &HookLog,
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
            // Disown the pane BEFORE tmux kill-pane. kill-pane fires the
            // `after-kill-pane`/`window-unlinked` hooks, which trigger a reconcile
            // in a concurrent connection task; if the pane were still in
            // managed_panes when that reconcile ran, it would race us and record a
            // spurious `vanished` death for a pane we are deliberately killing.
            // Removing it first makes this Kill the sole, authoritative recorder.
            managed_panes.remove(&pane_id);
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
            let state = panes.remove(&pane_id);
            if let Some(ref state) = state {
                state.cancel_drivers();
            }
            // managed_panes was already cleared above (before kill-pane).
            // An explicit `slopctl kill` — the one unambiguous death. Recording it
            // is what lets a later post-mortem say "slopd killed it" instead of
            // guessing, as the %119 investigation had to.
            record_pane_death(
                event_tx,
                &pane_id,
                DeathCause::DeliberateKill,
                DeathDetectedBy::KillRpc,
                state.as_ref(),
                None,
                None,
                None,
            );
            libslop::ResponseBody::Kill { pane_id }
        }

        libslop::RequestBody::TmuxHook { event, pane_id } => {
            debug!("tmux-hook: {} pane={:?}", event, pane_id);
            // Remember this hook so a vanished pane found in the reconcile below
            // (or in a background tick moments later) can attribute its cause:
            // `after-kill-pane` ⇒ external kill, `window-unlinked` ⇒ closed window.
            {
                let now = std::time::Instant::now();
                let mut hooks = hook_log.lock().unwrap();
                match event.as_str() {
                    "after-kill-pane" => hooks.after_kill_pane = Some(now),
                    "window-unlinked" => hooks.window_unlinked = Some(now),
                    _ => {}
                }
            }
            reconcile_panes(config, panes, managed_panes, event_tx, hook_log).await;
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
                    // The registration wait only makes sense for hooks that can race a
                    // just-spawned pane's managed_panes.insert(): the very first hooks
                    // (SessionStart, InstructionsLoaded, an early PreToolUse). SessionEnd
                    // is a *terminal* event — the pane is exiting and, if it isn't
                    // already managed, it never will be. Waiting the full
                    // PANE_REGISTRATION_WAIT here would just delay the reply, and Claude
                    // cancels a SessionEnd hook that doesn't return promptly on shutdown
                    // ("SessionEnd hook [...] failed: Hook cancelled"). Answer an
                    // unmanaged pane's SessionEnd immediately instead.
                    if event != "SessionEnd" {
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
                    }
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
            let hook_backend = panes.get_or_insert(pane).runtime().backend();
            if let Some(raw_transcript_path) =
                hook_transcript_path(hook_backend, &event, &payload)
            {
                let state = panes.get_or_insert(pane);
                // A forked Claude pane: real Claude's hooks report the SOURCE
                // transcript file until the fork's first turn writes its own file.
                // slopd minted (and pinned) the fork id, so rewrite the path to the
                // fork's own file in the same directory — the tailer waits for it to
                // appear. Non-fork panes, and hooks that already name the fork file,
                // pass through unchanged.
                let corrected = (state.runtime().backend() == libslop::Backend::Claude)
                    .then(|| state.pinned_session_id.lock().unwrap().clone())
                    .flatten()
                    .and_then(|fork_id| {
                        let p = std::path::Path::new(raw_transcript_path);
                        if p.file_stem().and_then(|s| s.to_str()) == Some(fork_id.as_str()) {
                            None
                        } else {
                            p.parent().map(|dir| {
                                dir.join(format!("{fork_id}.jsonl"))
                                    .to_string_lossy()
                                    .into_owned()
                            })
                        }
                    });
                let transcript_path: &str = corrected.as_deref().unwrap_or(raw_transcript_path);
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
                let pane_state = panes.get_or_insert(pane);
                match pane_state.runtime().backend() {
                    libslop::Backend::Claude => {
                        pane_state.set_runtime(PaneRuntime::Claude(ClaudeState))
                    }
                    libslop::Backend::Codex => {
                        pane_state.set_runtime(PaneRuntime::Codex(CodexState))
                    }
                    libslop::Backend::Opencode => {}
                }
                // A forked Claude pane pins the (minted) fork session id: real
                // Claude reports the *resumed source* id in this hook, not the
                // fork's, so trusting the payload would mis-bind the pane to the
                // source session. The pin wins when present; otherwise use the
                // payload id (the normal fresh/resume path).
                let pinned = (pane_state.runtime().backend() == libslop::Backend::Claude)
                    .then(|| pane_state.pinned_session_id.lock().unwrap().clone())
                    .flatten();
                let session_id = pinned.or_else(|| {
                    payload.get("session_id").and_then(|v| v.as_str()).map(str::to_string)
                });
                if let Some(session_id) = session_id {
                    debug!("SessionStart: pane={} session_id={}", pane, session_id);
                    pane_state.note_session_id(&session_id);
                    if let Err(e) = tmux_set_pane_option(config, pane, libslop::TmuxOption::SlopdSessionId.as_str(), &session_id).await {
                        warn!("failed to set @slopd_claude_session_id on pane {}: {}", pane, e);
                    }
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

        libslop::RequestBody::Fork { pane_id, start_directory, env, extra_args } => {
            if !managed_panes.contains(&pane_id) {
                return libslop::ResponseBody::Error {
                    message: format!("pane {} is not managed by slopd", pane_id),
                };
            }
            // Resolve the source pane's session id, cwd, account, and backend in
            // one shot. (list_panes is a couple of tmux calls per pane; fork is a
            // rare, interactive operation, so the cost is irrelevant here.)
            let src = match list_panes(config, managed_panes).await {
                Ok(list) => list.into_iter().find(|p| p.pane_id == pane_id),
                Err(message) => return libslop::ResponseBody::Error { message },
            };
            let Some(src) = src else {
                return libslop::ResponseBody::Error {
                    message: format!("pane {} not found", pane_id),
                };
            };
            let Some(src_session) = src.session_id.clone() else {
                return libslop::ResponseBody::Error {
                    message: format!(
                        "pane {} has no known session id yet; cannot fork (is the agent still booting?)",
                        pane_id
                    ),
                };
            };
            // Produce the resume args that reconstruct the source session's
            // history in a *fresh* session, and learn that fresh session's id:
            //   - Claude: mint the id ourselves and let `--fork-session` copy the
            //     transcript into it (verified: `--session-id` is honored with
            //     `--fork-session`, and the original session is left untouched).
            //   - opencode: ask the source pane's own server to fork (it owns the
            //     live session in the shared store); the response carries the new
            //     id, which a pane spawned with `-s <id>` will then bind to via
            //     the existing resume path.
            let (fork_args, new_session_id) = match backend_lifecycle(src.backend).fork(ForkContext {
                pane_id: &pane_id,
                session_id: &src_session,
                extra_args,
                config,
            }).await {
                Ok(plan) => plan,
                Err(message) => return libslop::ResponseBody::Error { message },
            };
            // Default the fork's cwd to the source pane's cwd. Claude resolves a
            // resumed transcript by (encoded) cwd, so a mismatch would make
            // `--resume` fail to find the session; opencode's fork inherits the
            // source directory, so the pane cwd should match it too.
            let start_directory = start_directory
                .or_else(|| src.working_dir.as_ref().map(std::path::PathBuf::from));
            // Reuse the entire Run spawn path (account resolution, env merge, port
            // allocation, spawn, session binding, ancestor-chain linkage, events)
            // by dispatching a synthetic Run. Boxed because handle_request recurses.
            // Claude only: pin the minted fork id THROUGH the Run so the Run handler
            // sets it before the pane is registered — beating the pane's SessionStart
            // hook (which reports the resumed source id, not the fork's, and would
            // otherwise mis-bind both the session id and the transcript path).
            // opencode tracks its id via the resume path, so it needs no pin.
            let pin_session_id = if src.backend == libslop::Backend::Claude {
                Some(new_session_id.clone())
            } else {
                None
            };
            let run = libslop::RequestBody::Run {
                parent_pane_id: Some(pane_id.clone()),
                extra_args: fork_args,
                start_directory,
                env,
                account: Some(src.account.clone()),
                backend: Some(src.backend),
                pin_session_id,
            };
            match Box::pin(handle_request(
                run, start_time, config, panes, managed_panes, event_tx,
                pane_registered, session_lock, config_generation, pending_restore, hook_log,
            )).await {
                libslop::ResponseBody::Run { pane_id } => {
                    // Standalone Codex, like a fresh TUI, materializes a fork's
                    // new rollout lazily on its first submitted prompt. Return
                    // the already-usable pane now; SessionStart binds the real
                    // id later. Other backends already know the fork id here.
                    libslop::ResponseBody::Forked {
                        pane_id,
                        session_id: new_session_id,
                    }
                }
                other => other,
            }
        }

        libslop::RequestBody::Run { parent_pane_id, extra_args, start_directory, env, account, backend, pin_session_id } => {
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
            // Inject hooks into the backend-specific config file the pane reads:
            // Claude settings.json or Codex hooks.json.
            if resolved.backend.uses_injected_hooks() {
                let hook_path = config.resolved_hook_path(&resolved);
                if let Err(e) = libslop::inject_backend_hooks_into_file(
                    &hook_path,
                    &config.hook_slopctl(),
                    resolved.backend,
                ) {
                    warn!("failed to inject hooks into {}: {}", hook_path.display(), e);
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

            // Prepare backend-specific standalone launch arguments/resources.
            let prepared = match backend_lifecycle(resolved.backend).prepare_run(PrepareRunContext {
                extra_args: &extra_args,
            }).await {
                Ok(prepared) => prepared,
                Err(message) => return libslop::ResponseBody::Error { message },
            };
            // Detect a resume target in the passthrough args (any spelling:
            // `--resume <id>`, opencode's `-s <id>` / `--session <id>`). For
            // opencode the pane must be spawned with the canonical `-s <id>`
            // AND slopd must bind its tracking to this id below, skipping the
            // "POST a fresh session" step — otherwise the resumed conversation
            // is stranded on a new empty session. Claude keeps `--resume` as-is.
            let spawn_cwd = effective_start_dir.clone()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            let mut trailing = prepared.trailing_args(extra_args, &spawn_cwd);
            let spawn_env = merged_env;
            if let Some(port) = prepared.opencode_port() {
                let mut v = trailing.clone();
                v.extend([
                    "--port".to_string(),
                    port.to_string(),
                    "--hostname".to_string(),
                    "127.0.0.1".to_string(),
                ]);
                trailing = v;
                // NOTE: deliberately do NOT set OPENCODE_SERVER_PASSWORD. The
                // opencode TUI is itself a client of its embedded server, and its
                // internal client does not authenticate — setting a password makes
                // the TUI 401 against its own server (`GET /config/providers`) and
                // crash on startup (verified against real opencode 1.17.x). The
                // server is therefore open on 127.0.0.1, which is the local-only
                // threat model slopd already assumes.
            }

            // Spawn through the shared chokepoint, which resolves the executable
            // to an absolute path (so the pane can't fail to find it on its PATH)
            // and surfaces a clear error if it's missing.
            let output = spawn_pane(config, session_lock, &SpawnSpec {
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
                    // Pin a forked Claude pane's session id BEFORE registering it, so
                    // the SessionStart hook (which won't be processed until the pane is
                    // in managed_panes) sees the pin and binds the fork id, not the
                    // resumed source id it reports. Also set @slopd_session_id up front
                    // so `ps` is correct before the first hook arrives. (opencode forks
                    // pass None here.)
                    if let Some(ref pin) = pin_session_id {
                        *panes.get_or_insert(&pane_id).pinned_session_id.lock().unwrap() = Some(pin.clone());
                        let _ = tmux_set_pane_option(config, &pane_id, libslop::TmuxOption::SlopdSessionId.as_str(), pin).await;
                    }
                    managed_panes.insert(pane_id.clone());
                    // Wake any hook handlers that arrived before managed_panes.insert()
                    // (race between tmux creating the pane and this task resuming).
                    pane_registered.notify_waiters();
                    let _ = tmux_set_pane_option(config, &pane_id, libslop::TmuxOption::SlopdManaged.as_str(), "true").await;
                    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
                    let _ = tmux_set_pane_option(config, &pane_id, libslop::TmuxOption::SlopdCreatedAt.as_str(), &now.to_string()).await;
                    // Seed the in-memory identity snapshot with everything known at
                    // spawn. This is the durable record a death is described from
                    // once the pane's tmux options are gone — session id/title are
                    // merged in later as slopd learns them. A forked Claude pane
                    // already has its session id (the pin); others fill it on bind.
                    {
                        let state = panes.get_or_insert(&pane_id);
                        state.mark_unbound_backend(resolved.backend);
                        let mut id = state.identity.lock().unwrap();
                        id.backend = resolved.backend;
                        id.parent_pane_id = parent_pane_id.clone();
                        id.working_dir = effective_start_dir.as_ref().and_then(|d| d.to_str().map(str::to_string));
                        id.created_at = Some(now);
                        if let Some(ref pin) = pin_session_id {
                            id.session_id = Some(pin.clone());
                        }
                    }
                    // Record the account so `ps` can show it, child panes can
                    // inherit it, and a slopd restart re-injects the right hooks
                    // for this pane (see load_managed_panes).
                    let _ = tmux_set_pane_option(config, &pane_id, libslop::TmuxOption::SlopdAccount.as_str(), &resolved.name).await;

                    // OpenCode panes: record backend/port/token, discover the session
                    // id, attach the HTTP runtime, and start the status-poll driver
                    // (which advances BootingUp → Ready/idle). Claude panes skip this
                    // and rely on the SessionStart hook + jsonl tailer instead.
                    match prepared {
                    PreparedBackendRun::Opencode { port, resume_session } => {
                        let _ = tmux_set_pane_option(config, &pane_id, libslop::TmuxOption::SlopdBackend.as_str(), "opencode").await;
                        let _ = tmux_set_pane_option(config, &pane_id, libslop::TmuxOption::SlopdOpencodePort.as_str(), &port.to_string()).await;

                        let client = opencode::OpencodeClient::new(port, None);
                        // Bind the session slopd will drive. Resume: the pane was
                        // spawned with `-s <id>`, so bind directly to that id —
                        // wait (bounded) for the server to list it as it finishes
                        // booting, then track it regardless. Do NOT call
                        // ensure_session here: POSTing a fresh session would strand
                        // the resumed conversation on a new empty one. Fresh run:
                        // discover-or-create via ensure_session as before (a fresh
                        // TUI has no session until the first message).
                        let session_id = if let Some(sid) = resume_session.clone() {
                            let _ = tokio::time::timeout(std::time::Duration::from_secs(20), async {
                                loop {
                                    match client.session_ids().await {
                                        Ok(ids) if ids.iter().any(|i| *i == sid) => return,
                                        _ => tokio::time::sleep(std::time::Duration::from_millis(300)).await,
                                    }
                                }
                            }).await;
                            sid
                        } else {
                            match tokio::time::timeout(std::time::Duration::from_secs(20), async {
                                loop {
                                    // OpenCode binds its listener before the
                                    // instance behind it has finished booting.
                                    // A mutating POST /session sent during that
                                    // gap can hang until our entire attachment
                                    // deadline expires. Require a successful
                                    // non-mutating listing before creating.
                                    if client.session_ids().await.is_ok() {
                                        match client.ensure_session().await {
                                            Ok(id) if !id.is_empty() => return id,
                                            _ => {}
                                        }
                                    }
                                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                                }
                            }).await {
                                Ok(id) => id,
                                Err(_) => {
                                    warn!("opencode pane {}: timed out creating/discovering a session; state tracking will be limited", pane_id);
                                    String::new()
                                }
                            }
                        };
                        let driver_cancel = tokio_util::sync::CancellationToken::new();
                        if !session_id.is_empty() {
                            panes.get_or_insert(&pane_id).note_session_id(&session_id);
                            let _ = tmux_set_pane_option(config, &pane_id, libslop::TmuxOption::SlopdSessionId.as_str(), &session_id).await;
                            // Point the TUI at the session slopd drives, so the pane
                            // shows the same conversation slopctl operates on. The
                            // HTTP server accepts select-session immediately, but the
                            // TUI client only acts on it once it has finished booting
                            // its UI and subscribed — calling once at spawn races that
                            // and the TUI lands on its own welcome screen instead. So
                            // re-assert it a few times over the first couple seconds;
                            // it's idempotent, and once the TUI honors it (and emits
                            // tui.session.select) the SSE follow keeps the two in sync.
                            // Shares the driver cancel token so it stops immediately if
                            // the pane is killed mid-boot (rather than poking a dead
                            // server for the rest of its 10 attempts).
                            let client = client.clone();
                            let sid = session_id.clone();
                            let pane = pane_id.clone();
                            let cancel = driver_cancel.clone();
                            tokio::spawn(async move {
                                for _ in 0..10 {
                                    if cancel.is_cancelled() {
                                        return;
                                    }
                                    if let Err(e) = client.select_session(&sid).await {
                                        debug!("opencode pane {}: select-session attempt failed: {}", pane, e);
                                    }
                                    tokio::select! {
                                        _ = cancel.cancelled() => return,
                                        _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
                                    }
                                }
                            });
                        }
                        let pane_state = panes.get_or_insert(&pane_id);
                        pane_state.set_runtime(PaneRuntime::Opencode(OpencodeState::new(
                            client.clone(), session_id.clone(), driver_cancel.clone(),
                        )));
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
                    PreparedBackendRun::Codex(_) => {
                        let _ = tmux_set_pane_option(config, &pane_id, libslop::TmuxOption::SlopdBackend.as_str(), "codex").await;
                        let pane_state = panes.get_or_insert(&pane_id);
                        if matches!(pane_state.runtime(), PaneRuntime::Unbound(_)) {
                            pane_state.set_runtime(PaneRuntime::Codex(CodexState));
                        }
                        // Interactive Codex creates or re-opens its rollout
                        // lazily and fires SessionStart on the first submitted
                        // prompt. This is true for fresh, fork, and resume TUIs.
                        // The composer is already usable before that, so waiting
                        // for SessionStart deadlocks `run` and `send`.
                        let current = pane_state.detailed_state.lock().unwrap().clone();
                        if current == libslop::PaneDetailedState::BootingUp {
                            set_pane_detailed_state(
                                config,
                                &pane_id,
                                &libslop::PaneDetailedState::Ready,
                                Some(&current),
                                event_tx,
                                panes,
                            )
                            .await;
                        }
                    }
                    PreparedBackendRun::Claude => {}
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
            let send_transport = state.runtime().send_transport();
            if let SendTransport::Unavailable(backend) = send_transport {
                return libslop::ResponseBody::Error {
                    message: format!("{} pane {} runtime is still attaching", backend.canonical_executable(), pane_id),
                };
            }
            let settle_before_enter = matches!(
                send_transport,
                SendTransport::Tui { settle_before_enter: true }
            );

            // OpenCode panes are HTTP-driven: handle the whole send here and
            // return. Claude and Codex panes fall through to the tmux path.
            if let SendTransport::Opencode(oc) = send_transport {
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
                        // Record the prompt so a `session.error` can auto-retry it.
                        if !is_command {
                            *oc.last_prompt.lock().unwrap() = Some(prompt.clone());
                            *state.retry_state.lock().unwrap() = None;
                        }
                        state.prompt_submitted.notify_waiters();
                        libslop::ResponseBody::Sent { pane_id }
                    }
                    Err(e) => libslop::ResponseBody::Error { message: e },
                };
            }

            let deadline = tokio::time::Instant::now()
                + std::time::Duration::from_secs(timeout_secs);

            // If --interrupt was requested, preempt the running turn with Escape
            // (Claude's cancel key). Unlike the Ctrl-C/Ctrl-D sequence the
            // standalone `interrupt` command uses, a lone Escape can't quit an
            // idle session and, being a single key, is cleanly separated from the
            // keystrokes that follow.
            if interrupt {
                {
                    let _guard = state.type_mutex.lock().await;
                    if let Err(e) = tmux_send_keys(config, &pane_id, "Escape").await {
                        return libslop::ResponseBody::Error { message: e.to_string() };
                    }
                }
                // Let the Escape settle as a standalone interrupt before typing;
                // otherwise the terminal swallows the first prompt character.
                tokio::time::sleep(INTERRUPT_SETTLE).await;
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

            // Clear any residual input first with Ctrl-U (a queued draft, a
            // ghosted autocomplete suggestion, or leftover keystrokes) so the
            // prompt is submitted verbatim instead of concatenated onto whatever
            // was already in the box.
            if let Err(e) = tmux_send_keys(config, &pane_id, "C-u").await {
                return libslop::ResponseBody::Error { message: e.to_string() };
            }

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
                    // Codex may not acknowledge every composer submission hook
                    // before this RPC returns (notably an in-flight steer).
                    // Submit exactly once through its visible TUI; hooks and the
                    // rollout tail drive subsequent state.
                    if settle_before_enter {
                        // Ratatui can render the pasted text one event-loop tick
                        // after tmux reports send-keys complete. An immediate
                        // Enter is then ignored and leaves the text as a draft.
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        return match tmux_send_keys(config, &pane_id, "Enter").await {
                            Ok(out) if out.success() => libslop::ResponseBody::Sent { pane_id },
                            Ok(_) => libslop::ResponseBody::Error { message: format!("tmux send-keys failed for pane {}", pane_id) },
                            Err(e) => libslop::ResponseBody::Error { message: e.to_string() },
                        };
                    }
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

            if let Some(result) = state.runtime().interrupt().await {
                return match result {
                    Ok(()) => libslop::ResponseBody::Interrupted { pane_id },
                    Err(message) => libslop::ResponseBody::Error { message },
                };
            }

            // Acquire the type-mutex so we don't interleave with concurrent sends.
            let _guard = state.type_mutex.lock().await;

            if state.runtime().backend() == libslop::Backend::Codex {
                // Ctrl-D exits the standalone Codex CLI, so the generic
                // Claude sequence (Ctrl-C, Ctrl-D, Escape) destroys the pane.
                // Escape is Codex's native in-turn cancel key and is harmless
                // at the idle composer.
                if let Err(e) = tmux_send_keys(config, &pane_id, "Escape").await {
                    return libslop::ResponseBody::Error {
                        message: e.to_string(),
                    };
                }
            } else if let Err(e) = send_interrupt_keys(config, &pane_id).await {
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
                Ok(pane_infos) => {
                    // Opportunistically refresh each pane's identity snapshot from
                    // this fresh listing — the pane title (which only tmux knows,
                    // set by the agent after spawn) and a session-id/cwd backstop.
                    // `ps` is the natural refresh point; it keeps the eventual death
                    // record's title current without any extra tmux round-trips.
                    for info in &pane_infos {
                        if let Some(state) = panes.get(&info.pane_id) {
                            let mut id = state.identity.lock().unwrap();
                            if info.pane_title.is_some() {
                                id.title = info.pane_title.clone();
                            }
                            if id.session_id.is_none() {
                                id.session_id = info.session_id.clone();
                            }
                            if id.working_dir.is_none() {
                                id.working_dir = info.working_dir.clone();
                            }
                        }
                    }
                    libslop::ResponseBody::Ps { panes: pane_infos }
                }
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
            if let Some(state) = panes.get(&pane_id) {
                if let Some(result) = state.runtime().transcript(&pane_id).await {
                    return match result {
                        Ok(mut records) => {
                            if let Some(before) = before_cursor {
                                records.retain(|record| record.cursor.is_some_and(|cursor| cursor < before));
                            }
                            if limit > 0 && records.len() > limit as usize {
                                records = records.split_off(records.len() - limit as usize);
                            }
                            libslop::ResponseBody::TranscriptPage { records }
                        }
                        Err(message) => libslop::ResponseBody::Error { message },
                    };
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
                    let backend = panes
                        .get(&pane_id)
                        .map(|state| state.runtime().backend())
                        .unwrap_or(libslop::Backend::Claude);
                    match read_transcript_before(&path, effective_before, limit, backend).await {
                        Ok((records, _at_beginning)) => {
                            let records = records.into_iter().map(|(cursor, event_type, payload)| {
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

    #[test]
    fn pane_id_sort_orders_numerically_then_unparseable_last() {
        let mut ids = vec!["%83", "%9", "%60", "weird", "%100", "%7"];
        ids.sort_by(|a, b| pane_id_sort_key(a).cmp(&pane_id_sort_key(b)));
        // %9 before %60/%83/%100 (numeric, not lexicographic); non-%N sorts last.
        assert_eq!(ids, vec!["%7", "%9", "%60", "%83", "%100", "weird"]);
    }

    #[test]
    fn extract_resume_target_recognizes_every_spelling() {
        let id = "ses_0e54cad77ffeghO0xI5t1A27d2";
        // Uniform --resume (Claude-native), plus opencode's -s / --session, and
        // the --flag=<id> forms all yield the id.
        for args in [
            vec!["--resume".to_string(), id.to_string()],
            vec!["-s".to_string(), id.to_string()],
            vec!["--session".to_string(), id.to_string()],
            vec![format!("--resume={id}")],
            vec![format!("--session={id}")],
            vec![format!("-s={id}")],
        ] {
            assert_eq!(extract_resume_target(&args).as_deref(), Some(id), "args: {:?}", args);
        }
        // No resume flag → None; a flag with no value → None.
        assert_eq!(extract_resume_target(&["--port".to_string(), "8080".to_string()]), None);
        assert_eq!(extract_resume_target(&["--resume".to_string()]), None);
    }

    #[test]
    fn strip_resume_flags_removes_flag_and_value_preserving_the_rest() {
        let id = "ses_abc";
        // Two-token form: both the flag and its value are dropped; the rest stays.
        let out = strip_resume_flags(vec![
            "--resume".to_string(), id.to_string(),
            "--port".to_string(), "0".to_string(),
        ]);
        assert_eq!(out, vec!["--port".to_string(), "0".to_string()]);
        // -s and --session forms, and the --flag=<id> forms, are all stripped.
        assert_eq!(strip_resume_flags(vec!["-s".to_string(), id.to_string()]), Vec::<String>::new());
        assert_eq!(strip_resume_flags(vec![format!("--session={id}")]), Vec::<String>::new());
        // Nothing to strip → unchanged.
        assert_eq!(
            strip_resume_flags(vec!["--agent".to_string(), "build".to_string()]),
            vec!["--agent".to_string(), "build".to_string()],
        );
    }

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

    #[test]
    fn backend_policy_keeps_protocol_quirks_at_the_boundary() {
        assert!(!libslop::Backend::Claude.driver_owns_initial_state());
        assert!(!libslop::Backend::Codex.driver_owns_initial_state());
    }

    #[test]
    fn codex_subagent_start_does_not_replace_the_main_transcript() {
        let payload = serde_json::json!({
            "transcript_path": "/sessions/subagent.jsonl",
        });
        assert_eq!(
            hook_transcript_path(
                libslop::Backend::Codex,
                "SubagentStart",
                &payload,
            ),
            None,
        );
        assert_eq!(
            hook_transcript_path(libslop::Backend::Codex, "SubagentStop", &payload),
            Some("/sessions/subagent.jsonl"),
        );
        assert_eq!(
            hook_transcript_path(libslop::Backend::Claude, "SubagentStart", &payload),
            Some("/sessions/subagent.jsonl"),
        );
    }

    #[test]
    fn prepared_opencode_run_normalizes_resume_arguments() {
        let prepared = PreparedBackendRun::Opencode {
            port: 4321,
            resume_session: Some("session-1".to_string()),
        };
        assert_eq!(prepared.opencode_port(), Some(4321));
        assert_eq!(
            prepared.trailing_args(
                vec!["--resume".to_string(), "old".to_string(), "--flag".to_string()],
                std::path::Path::new("/tmp"),
            ),
            vec!["--flag", "-s", "session-1"],
        );
    }

    #[test]
    fn prepared_codex_run_is_standalone_and_resumes_locally() {
        let prepared = PreparedBackendRun::Codex(PreparedCodexRun {
            resume_session: Some("session-1".to_string()),
        });
        let args = prepared.trailing_args(
            vec!["--resume".to_string(), "old".to_string(), "--flag".to_string()],
            std::path::Path::new("/work"),
        );
        assert_eq!(
            args,
            vec![
                "--dangerously-bypass-hook-trust",
                "--no-alt-screen",
                "-C",
                "/work",
                "resume",
                "session-1",
                "--flag",
            ]
        );
        assert!(!args.iter().any(|arg| arg == "--remote"));
    }

    #[test]
    fn pane_runtime_is_explicitly_unbound_until_backend_attaches() {
        let state = PaneState::new();
        assert!(matches!(state.runtime(), PaneRuntime::Unbound(UnboundState {
            backend: libslop::Backend::Claude,
        })));
        state.mark_unbound_backend(libslop::Backend::Codex);
        assert!(matches!(state.runtime().send_transport(),
            SendTransport::Unavailable(libslop::Backend::Codex)));
        state.set_runtime(PaneRuntime::Claude(ClaudeState));
        assert!(matches!(state.runtime().send_transport(),
            SendTransport::Tui { settle_before_enter: false }));
    }
}
