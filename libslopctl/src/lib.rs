use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Subcommand;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::debug;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Parse(serde_json::Error),
    Server(String),
    UnexpectedResponse(String),
    ConnectionClosed,
    Timeout,
    FilterError(String),
    SelectError(String),
    /// A `run` failed after the pane was created (e.g. it died before becoming
    /// ready). The message is already user-facing; no prefix is added.
    RunFailed(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {}", e),
            Error::Parse(e) => write!(f, "parse error: {}", e),
            Error::Server(msg) => write!(f, "server error: {}", msg),
            Error::UnexpectedResponse(r) => write!(f, "unexpected response: {}", r),
            Error::ConnectionClosed => write!(f, "connection closed unexpectedly"),
            Error::Timeout => write!(f, "timed out waiting for response from slopd"),
            Error::FilterError(msg) => write!(f, "filter error: {}", msg),
            Error::SelectError(msg) => write!(f, "select error: {}", msg),
            Error::RunFailed(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Parse(e)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum SelectMode {
    /// Require exactly one matching pane; error otherwise.
    One,
    /// Pick one at random from matches; error if none.
    Any,
    /// Send to all matching panes; error if none.
    All,
}

#[derive(Subcommand)]
pub enum CommonCommand {
    /// Show slopd uptime and state.
    Status,
    /// List panes in the slopd session.
    Ps {
        /// Filter by key=value (repeatable, AND semantics). Supported keys: tag.
        #[arg(long = "filter", value_name = "KEY=VALUE")]
        filters: Vec<String>,
        /// Output as JSON array instead of table.
        #[arg(long)]
        json: bool,
    },
    /// Open a new Claude pane in the slopd tmux session.
    Run {
        /// Working directory for the new pane. Supports ~ and $VAR / ${VAR}
        /// expansion (applied by slopd, so a quoted ~ works and resolves against
        /// the daemon's home). Overrides [run] start_directory from config.toml.
        #[arg(short = 'c', long, value_name = "DIR")]
        start_directory: Option<PathBuf>,
        /// Extra environment variables for the new pane (repeatable).
        /// Format: KEY=VALUE. The value supports $VAR / ${VAR} expansion
        /// against slopctl's environment; missing variables are an error.
        /// Overrides values from --env-file and [run.env]/[run.env_files] in config.
        #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
        envs: Vec<String>,
        /// Path to a dotenv-style file of KEY=VALUE lines (repeatable). Supports
        /// ~ and $VAR / ${VAR} expansion (against slopctl's environment).
        /// Files are loaded in order; later files (and --env) override earlier ones.
        /// Overrides entries from [run.env_files] / [run.env] in config.
        #[arg(long = "env-file", value_name = "PATH")]
        env_files: Vec<PathBuf>,
        /// Named account to launch the pane under (configured in slopd's
        /// config.toml under [accounts], or the reserved "default"). When
        /// omitted, the pane inherits the current pane's account, then falls
        /// back to slopd's default_account.
        #[arg(short = 'a', long, value_name = "NAME")]
        account: Option<String>,
        /// Instead of waiting for the pane to become ready, hand off to a viewer
        /// once it exists: run slopctl's configured [run] interactive_command
        /// (default `tmux attach -t slopd`), with `{{pane_id}}` replaced by the
        /// new pane id. `exec`s by default (replacing slopctl); set [run]
        /// interactive_type = "forking" to run it in the background instead.
        /// Local slopctl only.
        #[arg(short = 'i', long)]
        interactive: bool,
        /// Don't wait for the new pane to become ready; print the pane id as soon
        /// as it is created (the historical fire-and-forget behaviour). By
        /// default `run` waits until the pane is ready, or fails if it dies or
        /// times out first.
        #[arg(long)]
        no_wait: bool,
        /// Seconds to wait for the new pane to become ready before giving up.
        /// On timeout the pane id is still printed (so it can be investigated)
        /// and the exit code is non-zero. Ignored with --no-wait.
        #[arg(long, default_value = "30", value_name = "SECS")]
        ready_timeout: u64,
        /// Extra arguments passed to the Claude executable (after --).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        extra_args: Vec<String>,
    },
    /// Terminate a Claude pane.
    Kill {
        /// Tmux pane ID (e.g. %42).
        pane_id: String,
    },
    /// Type a prompt into pane(s) and wait for UserPromptSubmit confirmation.
    Send {
        /// Tmux pane ID (e.g. %42) or filter (e.g. tag=worker).
        pane_id: String,
        /// Prompt text to send.
        prompt: String,
        /// Additional filter by key=value (repeatable, AND semantics). Supported keys: tag.
        #[arg(long = "filter", value_name = "KEY=VALUE")]
        filters: Vec<String>,
        /// How to select among matching panes: one (error if not exactly one), any (pick one at random), all.
        #[arg(long, default_value = "one")]
        select: SelectMode,
        /// Seconds to wait for UserPromptSubmit confirmation per pane.
        #[arg(long, default_value = "60")]
        timeout: u64,
        /// Interrupt the pane before sending (equivalent to slopctl interrupt then send).
        #[arg(long, short = 'i')]
        interrupt: bool,
    },
    /// Send Ctrl+C, Ctrl+D, and Escape to interrupt a running agent.
    Interrupt {
        /// Tmux pane ID (e.g. %42).
        pane_id: String,
    },
    /// Subscribe to a stream of events and print each as a JSON line.
    Listen {
        /// Filter by hook event name (repeatable; omit for all events). Matches source:hook events.
        #[arg(long = "hook", value_name = "EVENT")]
        hooks: Vec<String>,
        /// Filter by slopd event name (repeatable). Matches source:slopd events (e.g. StateChange, DetailedStateChange).
        #[arg(long = "event", value_name = "EVENT")]
        events: Vec<String>,
        /// Filter by transcript record type (repeatable). Matches source:transcript events (e.g. user, assistant, progress).
        #[arg(long = "transcript", value_name = "TYPE")]
        transcripts: Vec<String>,
        /// Only receive events from this tmux pane. Must be a tmux pane id
        /// (e.g. %42); a UUID-shaped value is rejected with a hint pointing
        /// at --session-id (use that flag for Claude session UUIDs).
        #[arg(long, value_name = "PANE_ID")]
        pane_id: Option<String>,
        /// Only receive events from this Claude session.
        #[arg(long, value_name = "SESSION_ID")]
        session_id: Option<String>,
        /// Server-side payload predicate(s) of the form KEY=VALUE (repeatable, AND).
        /// KEY is a jq-style path into the event's `payload`. Supports `.foo`,
        /// `.foo[]` (any array element), `.foo[3]`, and combinations
        /// (e.g. `message.content[].type`). The leading dot is optional:
        /// `state=ready` and `.state=ready` are equivalent. Comparison is
        /// string-equality against the reachable scalar; arrays/objects never
        /// match. Pushed down to slopd so non-matching events are not delivered.
        #[arg(long = "where", value_name = "KEY=VALUE")]
        where_preds: Vec<String>,
        /// Replay the last N transcript records before switching to live events (requires --pane-id).
        #[arg(long, value_name = "N")]
        replay: Option<u64>,
    },
    /// Subscribe to events and exit on the first one matching the filters
    /// (and optional --until predicates), or fail with non-zero exit on timeout.
    /// Mirrors the filter surface of `listen`.
    Wait {
        /// Filter by hook event name (repeatable; omit for all events). Matches source:hook events.
        #[arg(long = "hook", value_name = "EVENT")]
        hooks: Vec<String>,
        /// Filter by slopd event name (repeatable). Matches source:slopd events (e.g. StateChange, DetailedStateChange).
        #[arg(long = "event", value_name = "EVENT")]
        events: Vec<String>,
        /// Filter by transcript record type (repeatable). Matches source:transcript events (e.g. user, assistant, progress).
        #[arg(long = "transcript", value_name = "TYPE")]
        transcripts: Vec<String>,
        /// Only receive events from this tmux pane. Must be a tmux pane id
        /// (e.g. %42); a UUID-shaped value is rejected with a hint pointing
        /// at --session-id (use that flag for Claude session UUIDs).
        #[arg(long, value_name = "PANE_ID")]
        pane_id: Option<String>,
        /// Only receive events from this Claude session.
        #[arg(long, value_name = "SESSION_ID")]
        session_id: Option<String>,
        /// Server-side payload predicate(s) (same syntax as --until; leading
        /// dot optional). Pushed down to slopd so non-matching events are not
        /// delivered. Use this when the listener is expensive or the predicate
        /// is selective.
        #[arg(long = "where", value_name = "KEY=VALUE")]
        where_preds: Vec<String>,
        /// Client-side stop predicate(s) of the form KEY=VALUE (repeatable, AND).
        /// KEY is a jq-style path into the event's `payload`: `.foo`,
        /// `.foo[]` (any array element), `.foo[3]`, and combinations
        /// (e.g. `message.content[].type`). Comparison is string-equality
        /// against the reachable scalar; arrays/objects never match.
        /// The leading dot is optional: `state=ready` and `.state=ready` are equivalent.
        #[arg(long = "until", value_name = "KEY=VALUE")]
        until: Vec<String>,
        /// Skip the pre-wait pane-state snapshot and go straight to live events.
        /// By default `wait` snapshots the targeted pane via `ps` and exits 0
        /// with a synthetic `CurrentState` record if the snapshot already
        /// satisfies `--where` / `--until`. Use this flag when you want to wait
        /// for the next transition specifically, ignoring whatever state the
        /// pane is in right now.
        #[arg(long = "no-snapshot")]
        no_snapshot: bool,
        /// Seconds to wait before failing with non-zero exit. 0 disables the timeout.
        #[arg(long, default_value = "60")]
        timeout: u64,
    },
    /// Read historical transcript records from a pane.
    Transcript {
        /// Tmux pane ID (e.g. %42).
        pane_id: String,
        /// Byte-offset cursor; return records strictly before this offset.
        #[arg(long)]
        before: Option<u64>,
        /// Maximum number of records to return.
        #[arg(long, default_value = "50")]
        limit: u64,
    },
    /// Add a tag to a pane.
    Tag {
        /// Tmux pane ID (e.g. %42).
        pane_id: String,
        /// Tag name (ASCII letters, digits, _, -).
        tag: String,
    },
    /// Remove a tag from a pane.
    Untag {
        /// Tmux pane ID (e.g. %42).
        pane_id: String,
        /// Tag name to remove.
        tag: String,
    },
    /// List all tags on a pane.
    Tags {
        /// Tmux pane ID (e.g. %42). Defaults to $TMUX_PANE if omitted.
        pane_id: Option<String>,
    },
}

/// Context that differs between slopctl (local) and iroh-slopctl (remote).
pub struct CommandContext {
    /// For `Run`: the parent pane ID. slopctl sets this to $TMUX_PANE, iroh sets None.
    /// The daemon also uses it to inherit the parent pane's account when `--account`
    /// is not given.
    pub parent_pane_id: Option<String>,
    /// For `Tags` when pane_id is None: fallback pane ID.
    pub fallback_pane_id: Option<String>,
    /// For `run --interactive`: how to launch the viewer command. `None` means
    /// interactive run is unsupported here (remote iroh — can't attach to a
    /// remote tmux). slopctl populates it from its config.
    pub interactive: Option<InteractiveRun>,
}

/// Resolved `run --interactive` settings: the command template (with `{{name}}`
/// placeholders), how to run it, and the substitution variables known ahead of
/// time (e.g. `socket`, `session`). `pane_id` is added once the pane exists.
pub struct InteractiveRun {
    pub command: Vec<String>,
    pub run_type: libslop::RunType,
    pub vars: Vec<(String, String)>,
}

pub fn die(msg: &str) -> ! {
    eprintln!("error: {}", msg);
    std::process::exit(1);
}

pub fn die_err(e: Error) -> ! {
    die(&e.to_string());
}

/// Build the merged env list for `slopctl run`: entries from env-files (in flag
/// order) followed by entries from --env flags (in flag order). Values in
/// --env are expanded against slopctl's environment; entries from env-files
/// are returned as dotenvy parses them. Env-file paths are `~` / `$VAR`-expanded
/// (against slopctl's environment) so a quoted `~` works. Later entries override
/// earlier ones (the wire format preserves order; slopd and tmux both apply last-wins).
pub fn build_cli_env(
    env_files: &[PathBuf],
    envs: &[String],
) -> Result<Vec<(String, String)>, Error> {
    let mut out = Vec::new();
    for path in env_files {
        let path = libslop::expand_path(path);
        let pairs = libslop::load_env_file(&path).map_err(Error::FilterError)?;
        out.extend(pairs);
    }
    for raw in envs {
        let pair = libslop::parse_env_kv(raw).map_err(Error::FilterError)?;
        out.push(pair);
    }
    Ok(out)
}

/// Parse "key=value" filter strings. Returns an error on malformed input.
pub fn parse_filters(raw: Vec<String>) -> Result<Vec<(String, String)>, Error> {
    raw.into_iter().map(|f| {
        match f.split_once('=') {
            Some((k, v)) => {
                if k != "tag" {
                    return Err(Error::FilterError(
                        format!("unknown filter key {:?}: only 'tag' is supported", k),
                    ));
                }
                Ok((k.to_string(), v.to_string()))
            }
            None => Err(Error::FilterError(
                format!("invalid filter {:?}: expected key=value", f),
            )),
        }
    }).collect()
}

/// Apply parsed filters to a pane list. AND semantics: pane must satisfy all filters.
pub fn apply_filters(panes: Vec<libslop::PaneInfo>, filters: &[(String, String)]) -> Vec<libslop::PaneInfo> {
    if filters.is_empty() {
        return panes;
    }
    panes.into_iter().filter(|pane| {
        filters.iter().all(|(key, value)| {
            match key.as_str() {
                "tag" => pane.tags.iter().any(|t| t == value),
                _ => false,
            }
        })
    }).collect()
}

/// Validate filters for a command before connecting.
pub fn validate_command_filters(command: &CommonCommand) -> Result<(), Error> {
    match command {
        CommonCommand::Ps { filters, .. } => {
            parse_filters(filters.clone())?;
        }
        CommonCommand::Send { pane_id, filters, .. } => {
            if pane_id.contains('=') {
                let mut all = vec![pane_id.clone()];
                all.extend(filters.clone());
                parse_filters(all)?;
            } else {
                parse_filters(filters.clone())?;
            }
        }
        CommonCommand::Listen { where_preds, pane_id, session_id, .. } => {
            parse_payload_predicates(where_preds.clone())?;
            resolve_pane_id_or_session(pane_id.clone(), session_id.clone())?;
        }
        CommonCommand::Wait { where_preds, until, pane_id, session_id, .. } => {
            parse_payload_predicates(where_preds.clone())?;
            parse_payload_predicates(until.clone())?;
            resolve_pane_id_or_session(pane_id.clone(), session_id.clone())?;
        }
        _ => {}
    }
    Ok(())
}

/// Return true for strings shaped like a UUID (8-4-4-4-12 hex digits). Used to
/// detect when a caller has passed a Claude session UUID to `--pane-id` so we
/// can point them at `--session-id` instead of silently subscribing to nothing.
fn looks_like_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    const EXPECTED: [usize; 5] = [8, 4, 4, 4, 12];
    parts.iter().zip(EXPECTED.iter()).all(|(p, &n)| {
        p.len() == n && p.chars().all(|c| c.is_ascii_hexdigit())
    })
}

/// Validate the `--pane-id` argument shape. Accepts only tmux pane ids
/// (`%<digits>`). A UUID-shaped value is rejected with a hint pointing at
/// `--session-id` — we deliberately do NOT silently route across flags, to
/// keep filter semantics explicit. Anything else is rejected as garbage.
fn validate_pane_id_arg(arg: &str) -> Result<(), Error> {
    if let Some(rest) = arg.strip_prefix('%')
        && !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            return Ok(());
        }
    if looks_like_uuid(arg) {
        return Err(Error::FilterError(format!(
            "--pane-id {:?} looks like a Claude session UUID; use --session-id for UUIDs \
             (`--pane-id` is for tmux pane ids like %42)",
            arg
        )));
    }
    Err(Error::FilterError(format!(
        "--pane-id {:?} is not a tmux pane id (expected `%<digits>`, e.g. %42); \
         use --session-id for a Claude session UUID",
        arg
    )))
}

/// Validate `--pane-id` and `--session-id` argument shapes. `--pane-id` must
/// be a tmux pane id (`%<n>`); passing a UUID errors with a `--session-id`
/// hint rather than silently routing across flags. Returns the values
/// unchanged on success.
pub fn resolve_pane_id_or_session(
    pane_id: Option<String>,
    session_id: Option<String>,
) -> Result<(Option<String>, Option<String>), Error> {
    if let Some(ref pane_arg) = pane_id {
        validate_pane_id_arg(pane_arg)?;
    }
    Ok((pane_id, session_id))
}

/// Print a table of pane info to stdout.
pub fn print_ps(panes: Vec<libslop::PaneInfo>) {
    let now = std::time::SystemTime::now();
    let fmt = timeago::Formatter::new();
    struct Row {
        pane: String,
        created: String,
        last_active: String,
        session: String,
        parent: String,
        account: String,
        tags: String,
        state: String,
        detailed_state: String,
        working_dir: String,
    }
    let rows: Vec<Row> = panes.iter().map(|p| {
        let epoch = now.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        Row {
            pane: p.pane_id.clone(),
            created: fmt.convert(epoch.saturating_sub(std::time::Duration::from_secs(p.created_at))),
            last_active: fmt.convert(epoch.saturating_sub(std::time::Duration::from_secs(p.last_active))),
            session: p.session_id.as_deref().unwrap_or("-").to_string(),
            parent: p.parent_pane_id.as_deref().unwrap_or("-").to_string(),
            account: p.account.clone(),
            tags: if p.tags.is_empty() { "-".to_string() } else { p.tags.join(",") },
            state: p.state.as_str().to_string(),
            detailed_state: p.detailed_state.as_str().to_string(),
            working_dir: p.working_dir.as_deref().unwrap_or("-").to_string(),
        }
    }).collect();

    let pane_w          = rows.iter().map(|r| r.pane.len()).max().unwrap_or(0).max(4);
    let created_w       = rows.iter().map(|r| r.created.len()).max().unwrap_or(0).max(7);
    let last_active_w   = rows.iter().map(|r| r.last_active.len()).max().unwrap_or(0).max(11);
    let session_w       = rows.iter().map(|r| r.session.len()).max().unwrap_or(0).max(7);
    let parent_w        = rows.iter().map(|r| r.parent.len()).max().unwrap_or(0).max(6);
    let account_w       = rows.iter().map(|r| r.account.len()).max().unwrap_or(0).max(7);
    let tags_w          = rows.iter().map(|r| r.tags.len()).max().unwrap_or(0).max(4);
    let state_w         = rows.iter().map(|r| r.state.len()).max().unwrap_or(0).max(5);
    let detailed_w      = rows.iter().map(|r| r.detailed_state.len()).max().unwrap_or(0).max(14);
    let working_dir_w   = rows.iter().map(|r| r.working_dir.len()).max().unwrap_or(0).max(11);

    println!("{:<pane_w$}  {:<created_w$}  {:<last_active_w$}  {:<session_w$}  {:<parent_w$}  {:<account_w$}  {:<tags_w$}  {:<state_w$}  {:<detailed_w$}  {:<working_dir_w$}",
        "PANE", "CREATED", "LAST_ACTIVE", "SESSION", "PARENT", "ACCOUNT", "TAGS", "STATE", "DETAILED_STATE", "WORKING_DIR",
        pane_w=pane_w, created_w=created_w, last_active_w=last_active_w, session_w=session_w,
        parent_w=parent_w, account_w=account_w, tags_w=tags_w, state_w=state_w, detailed_w=detailed_w, working_dir_w=working_dir_w);

    for r in &rows {
        println!("{:<pane_w$}  {:<created_w$}  {:<last_active_w$}  {:<session_w$}  {:<parent_w$}  {:<account_w$}  {:<tags_w$}  {:<state_w$}  {:<detailed_w$}  {:<working_dir_w$}",
            r.pane, r.created, r.last_active, r.session, r.parent, r.account, r.tags, r.state, r.detailed_state, r.working_dir,
            pane_w=pane_w, created_w=created_w, last_active_w=last_active_w, session_w=session_w,
            parent_w=parent_w, account_w=account_w, tags_w=tags_w, state_w=state_w, detailed_w=detailed_w, working_dir_w=working_dir_w);
    }
}

/// Shared state for the background demux task.
struct DemuxState {
    /// Pending one-shot request waiters, keyed by request ID.
    pending: HashMap<u64, oneshot::Sender<libslop::Response>>,
    /// Active subscription channels, keyed by subscription request ID.
    subscriptions: HashMap<u64, mpsc::UnboundedSender<libslop::Response>>,
}

/// Background demux task: reads lines from the transport, routes responses to
/// the appropriate waiter (oneshot for request/response, mpsc for subscriptions).
async fn demux_loop<R: tokio::io::AsyncRead + Unpin>(
    mut lines: tokio::io::Lines<BufReader<R>>,
    state: Arc<Mutex<DemuxState>>,
) {
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) | Err(_) => {
                // Connection closed — wake all waiters with an error-like drop.
                let mut s = state.lock().await;
                s.pending.clear();
                s.subscriptions.clear();
                return;
            }
        };
        debug!("received: {}", line);
        let response: libslop::Response = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                debug!("demux: failed to parse response: {}", e);
                continue;
            }
        };
        let id = response.id;
        let mut s = state.lock().await;
        if let Some(tx) = s.subscriptions.get(&id) {
            let _ = tx.send(response);
        } else if let Some(tx) = s.pending.remove(&id) {
            let _ = tx.send(response);
        }
        // Responses for unknown IDs are silently dropped (same as before).
    }
}

/// Transport-agnostic client for the slopd JSON-RPC protocol.
///
/// Supports two modes:
/// - **Direct mode** (before any subscription): reads/writes synchronously on
///   the connection. Simple and zero-overhead.
/// - **Multiplexed mode** (after the first `subscribe`/`subscribe_transcript`):
///   a background task demuxes incoming lines by ID, routing subscription
///   records to mpsc channels and request-response pairs to oneshot channels.
pub struct Client<R: tokio::io::AsyncRead + Unpin, W: tokio::io::AsyncWrite + Unpin> {
    /// Present in direct mode; taken when transitioning to multiplexed.
    lines: Option<tokio::io::Lines<BufReader<R>>>,
    writer: W,
    next_id: u64,
    /// Present in multiplexed mode.
    demux: Option<Arc<Mutex<DemuxState>>>,
}

impl<
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin,
> Client<R, W> {
    const REQUEST_TIMEOUT_SECS: u64 = 15;

    pub fn new(reader: R, writer: W) -> Self {
        Self {
            lines: Some(BufReader::new(reader).lines()),
            writer,
            next_id: 1,
            demux: None,
        }
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Ensure the background demux task is running (transition to multiplexed mode).
    fn ensure_demux(&mut self) {
        if self.demux.is_some() {
            return;
        }
        let lines = self.lines.take().expect("lines must be present in direct mode");
        let state = Arc::new(Mutex::new(DemuxState {
            pending: HashMap::new(),
            subscriptions: HashMap::new(),
        }));
        self.demux = Some(Arc::clone(&state));
        tokio::spawn(demux_loop(lines, state));
    }

    /// Write a request to the transport.
    async fn write_request(&mut self, request: &libslop::Request) -> Result<(), Error> {
        let mut json = serde_json::to_string(request)?;
        debug!("sending: {}", json);
        json.push('\n');
        self.writer.write_all(json.as_bytes()).await?;
        Ok(())
    }

    /// Send a request and wait for the response with matching id.
    pub async fn request(&mut self, body: libslop::RequestBody) -> Result<libslop::ResponseBody, Error> {
        let timeout = std::time::Duration::from_secs(Self::REQUEST_TIMEOUT_SECS);
        self.request_with_timeout(body, timeout).await
    }

    async fn request_with_timeout(&mut self, body: libslop::RequestBody, timeout: std::time::Duration) -> Result<libslop::ResponseBody, Error> {
        let id = self.alloc_id();
        let request = libslop::Request { id, body };

        if let Some(ref demux) = self.demux {
            // Multiplexed mode: register a oneshot, send, then await.
            let (tx, rx) = oneshot::channel();
            demux.lock().await.pending.insert(id, tx);
            self.write_request(&request).await?;
            let response = tokio::time::timeout(timeout, rx)
                .await
                .map_err(|_| Error::Timeout)?
                .map_err(|_| Error::ConnectionClosed)?;
            return match response.body {
                libslop::ResponseBody::Error { message } => Err(Error::Server(message)),
                body => Ok(body),
            };
        }

        // Direct mode: read lines until the matching response arrives.
        self.write_request(&request).await?;
        let lines = self.lines.as_mut().expect("lines must be present in direct mode");
        let read_loop = async {
            loop {
                match lines.next_line().await? {
                    Some(line) => {
                        debug!("received: {}", line);
                        let response: libslop::Response = serde_json::from_str(&line)?;
                        if response.id == id {
                            return match response.body {
                                libslop::ResponseBody::Error { message } => Err(Error::Server(message)),
                                body => Ok(body),
                            };
                        }
                    }
                    None => return Err(Error::ConnectionClosed),
                }
            }
        };
        tokio::time::timeout(timeout, read_loop)
            .await
            .map_err(|_| Error::Timeout)?
    }

    pub async fn status(&mut self) -> Result<libslop::DaemonState, Error> {
        match self.request(libslop::RequestBody::Status).await? {
            libslop::ResponseBody::Status { state } => Ok(state),
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }

    pub async fn ps(&mut self) -> Result<Vec<libslop::PaneInfo>, Error> {
        match self.request(libslop::RequestBody::Ps).await? {
            libslop::ResponseBody::Ps { panes } => Ok(panes),
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }

    pub async fn run(
        &mut self,
        parent_pane_id: Option<String>,
        extra_args: Vec<String>,
        start_directory: Option<PathBuf>,
        env: Vec<(String, String)>,
        account: Option<String>,
    ) -> Result<String, Error> {
        match self.request(libslop::RequestBody::Run { parent_pane_id, extra_args, start_directory, env, account }).await? {
            libslop::ResponseBody::Run { pane_id } => Ok(pane_id),
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }

    pub async fn kill(&mut self, pane_id: String) -> Result<String, Error> {
        match self.request(libslop::RequestBody::Kill { pane_id }).await? {
            libslop::ResponseBody::Kill { pane_id } => Ok(pane_id),
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }

    pub async fn send_prompt(
        &mut self,
        pane_id: String,
        prompt: String,
        timeout_secs: u64,
        interrupt: bool,
    ) -> Result<String, Error> {
        // Send waits for slopd's server-side timeout plus a margin.
        let client_timeout = std::time::Duration::from_secs(timeout_secs + Self::REQUEST_TIMEOUT_SECS);
        match self.request_with_timeout(libslop::RequestBody::Send { pane_id, prompt, timeout_secs, interrupt }, client_timeout).await? {
            libslop::ResponseBody::Sent { pane_id } => Ok(pane_id),
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }

    pub async fn interrupt(&mut self, pane_id: String) -> Result<String, Error> {
        match self.request(libslop::RequestBody::Interrupt { pane_id }).await? {
            libslop::ResponseBody::Interrupted { pane_id } => Ok(pane_id),
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }

    pub async fn hook(
        &mut self,
        event: String,
        payload: serde_json::Value,
        pane_id: Option<String>,
    ) -> Result<(), Error> {
        match self.request(libslop::RequestBody::Hook { event, payload, pane_id }).await? {
            libslop::ResponseBody::Hooked => Ok(()),
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }

    pub async fn tmux_hook(
        &mut self,
        event: String,
        pane_id: Option<String>,
    ) -> Result<(), Error> {
        match self.request(libslop::RequestBody::TmuxHook { event, pane_id }).await? {
            libslop::ResponseBody::TmuxHooked => Ok(()),
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }

    pub async fn tag(&mut self, pane_id: String, tag: String) -> Result<(String, String), Error> {
        match self.request(libslop::RequestBody::Tag { pane_id, tag, remove: false }).await? {
            libslop::ResponseBody::Tagged { pane_id, tag } => Ok((pane_id, tag)),
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }

    pub async fn untag(&mut self, pane_id: String, tag: String) -> Result<(String, String), Error> {
        match self.request(libslop::RequestBody::Tag { pane_id, tag, remove: true }).await? {
            libslop::ResponseBody::Untagged { pane_id, tag } => Ok((pane_id, tag)),
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }

    pub async fn tags(&mut self, pane_id: String) -> Result<Vec<String>, Error> {
        match self.request(libslop::RequestBody::Tags { pane_id }).await? {
            libslop::ResponseBody::Tags { pane_id: _, tags } => Ok(tags),
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }

    pub async fn read_transcript(
        &mut self,
        pane_id: String,
        before_cursor: Option<u64>,
        limit: u64,
    ) -> Result<Vec<libslop::Record>, Error> {
        match self.request(libslop::RequestBody::ReadTranscript { pane_id, before_cursor, limit }).await? {
            libslop::ResponseBody::TranscriptPage { records } => Ok(records),
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }

    /// Send a prompt to panes matching filters, with selection mode.
    ///
    /// Returns the list of pane IDs that were successfully sent to.
    pub async fn send_filtered(
        &mut self,
        filters: &[(String, String)],
        prompt: &str,
        select: &SelectMode,
        timeout_secs: u64,
        interrupt: bool,
    ) -> Result<Vec<String>, Error> {
        let all_panes = self.ps().await?;
        let matched = apply_filters(all_panes, filters);

        let filter_desc = filters.iter().map(|(k, v)| format!("{}={}", k, v)).collect::<Vec<_>>().join(", ");

        let target_pane_ids: Vec<String> = match select {
            SelectMode::One => {
                if matched.len() != 1 {
                    return Err(Error::SelectError(format!(
                        "expected exactly one pane matching {}, found {}",
                        filter_desc, matched.len()
                    )));
                }
                vec![matched.into_iter().next().unwrap().pane_id]
            }
            SelectMode::Any => {
                if matched.is_empty() {
                    return Err(Error::SelectError(format!(
                        "no panes match filter {}", filter_desc
                    )));
                }
                use std::time::{SystemTime, UNIX_EPOCH};
                let idx = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos() as usize % matched.len();
                vec![matched.into_iter().nth(idx).unwrap().pane_id]
            }
            SelectMode::All => {
                if matched.is_empty() {
                    return Err(Error::SelectError(format!(
                        "no panes match filter {}", filter_desc
                    )));
                }
                matched.into_iter().map(|p| p.pane_id).collect()
            }
        };

        // In multiplexed mode we must go through request() one at a time,
        // since the demux task handles routing.
        let client_timeout = std::time::Duration::from_secs(timeout_secs + Self::REQUEST_TIMEOUT_SECS);
        if self.demux.is_some() {
            let mut out = Vec::new();
            for pane_id in target_pane_ids {
                let body = libslop::RequestBody::Send {
                    pane_id: pane_id.clone(),
                    prompt: prompt.to_string(),
                    timeout_secs,
                    interrupt,
                };
                match self.request_with_timeout(body, client_timeout).await? {
                    libslop::ResponseBody::Sent { pane_id } => out.push(pane_id),
                    libslop::ResponseBody::Error { message } => {
                        return Err(Error::Server(format!("error sending to {}: {}", pane_id, message)));
                    }
                    _ => {
                        return Err(Error::Server(format!("unexpected response for {}", pane_id)));
                    }
                }
            }
            return Ok(out);
        }

        // Direct mode: pipeline all requests, then collect responses by ID.
        let mut pending: HashMap<u64, String> = HashMap::new();
        for pane_id in &target_pane_ids {
            let id = self.alloc_id();
            let body = libslop::RequestBody::Send {
                pane_id: pane_id.clone(),
                prompt: prompt.to_string(),
                timeout_secs,
                interrupt,
            };
            let request = libslop::Request { id, body };
            self.write_request(&request).await?;
            pending.insert(id, pane_id.clone());
        }

        let lines = self.lines.as_mut().expect("lines must be present in direct mode");
        let mut results: HashMap<u64, libslop::ResponseBody> = HashMap::new();
        let read_loop = async {
            while results.len() < pending.len() {
                match lines.next_line().await? {
                    Some(line) => {
                        debug!("received: {}", line);
                        let response: libslop::Response = serde_json::from_str(&line)?;
                        if pending.contains_key(&response.id) {
                            results.insert(response.id, response.body);
                        }
                    }
                    None => return Err(Error::ConnectionClosed),
                }
            }
            Ok::<(), Error>(())
        };
        tokio::time::timeout(client_timeout, read_loop)
            .await
            .map_err(|_| Error::Timeout)??;

        // Return results in send order.
        let mut out = Vec::new();
        let mut ids: Vec<u64> = pending.keys().copied().collect();
        ids.sort();
        for req_id in ids {
            let pane_id = &pending[&req_id];
            match &results[&req_id] {
                libslop::ResponseBody::Sent { pane_id } => out.push(pane_id.clone()),
                libslop::ResponseBody::Error { message } => {
                    return Err(Error::Server(format!("error sending to {}: {}", pane_id, message)));
                }
                _ => {
                    return Err(Error::Server(format!("unexpected response for {}", pane_id)));
                }
            }
        }

        Ok(out)
    }

    /// Subscribe to events. Returns a Subscription handle; the client remains
    /// usable for further requests on the same connection.
    pub async fn subscribe(&mut self, filters: Vec<libslop::EventFilter>) -> Result<Subscription, Error> {
        self.ensure_demux();

        let id = self.alloc_id();
        let request = libslop::Request {
            id,
            body: libslop::RequestBody::Subscribe { filters },
        };

        // Register the subscription channel *before* sending so we don't miss
        // the Subscribed confirmation.
        let (tx, rx) = mpsc::unbounded_channel();
        let demux = Arc::clone(self.demux.as_ref().unwrap());
        demux.lock().await.subscriptions.insert(id, tx);

        self.write_request(&request).await?;

        // Wait for the Subscribed confirmation.
        let mut rx = rx;
        // Exactly one response is expected: the Subscribed confirmation, or an
        // error. (Not a loop — every branch resolves on the first message.)
        match rx.recv().await {
            Some(response) => match response.body {
                libslop::ResponseBody::Subscribed => {}
                libslop::ResponseBody::Error { message } => {
                    demux.lock().await.subscriptions.remove(&id);
                    return Err(Error::Server(message));
                }
                other => {
                    demux.lock().await.subscriptions.remove(&id);
                    return Err(Error::UnexpectedResponse(format!("{:?}", other)));
                }
            },
            None => {
                return Err(Error::ConnectionClosed);
            }
        }

        Ok(Subscription { rx, id })
    }

    /// Subscribe to a pane's transcript with replay. Returns a Subscription
    /// handle; the client remains usable for further requests.
    pub async fn subscribe_transcript(&mut self, pane_id: String, last_n: u64) -> Result<Subscription, Error> {
        self.ensure_demux();

        let id = self.alloc_id();
        let request = libslop::Request {
            id,
            body: libslop::RequestBody::SubscribeTranscript { pane_id, last_n },
        };

        let (tx, rx) = mpsc::unbounded_channel();
        let demux = Arc::clone(self.demux.as_ref().unwrap());
        demux.lock().await.subscriptions.insert(id, tx);

        self.write_request(&request).await?;

        // Wait for the Subscribed confirmation.
        let mut rx = rx;
        // Exactly one response is expected: the Subscribed confirmation, or an
        // error. (Not a loop — every branch resolves on the first message.)
        match rx.recv().await {
            Some(response) => match response.body {
                libslop::ResponseBody::Subscribed => {}
                libslop::ResponseBody::Error { message } => {
                    demux.lock().await.subscriptions.remove(&id);
                    return Err(Error::Server(message));
                }
                other => {
                    demux.lock().await.subscriptions.remove(&id);
                    return Err(Error::UnexpectedResponse(format!("{:?}", other)));
                }
            },
            None => {
                return Err(Error::ConnectionClosed);
            }
        }

        Ok(Subscription { rx, id })
    }

    /// Cancel an active subscription. The server will stop streaming records
    /// for the given subscription.
    pub async fn unsubscribe(&mut self, subscription: &Subscription) -> Result<(), Error> {
        self.unsubscribe_by_id(subscription.id).await
    }

    /// Cancel an active subscription by its request ID.
    pub async fn unsubscribe_by_id(&mut self, subscription_id: u64) -> Result<(), Error> {
        match self.request(libslop::RequestBody::Unsubscribe { subscription_id }).await? {
            libslop::ResponseBody::Unsubscribed { .. } => {
                if let Some(ref demux) = self.demux {
                    demux.lock().await.subscriptions.remove(&subscription_id);
                }
                Ok(())
            }
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }
}

/// A subscription stream that yields Record items from slopd.
pub struct Subscription {
    rx: mpsc::UnboundedReceiver<libslop::Response>,
    id: u64,
}

/// The result of calling `next()` on a Subscription.
pub enum SubscriptionItem {
    Record(libslop::Record),
    Subscribed,
}

impl Subscription {
    /// The request ID of this subscription (needed for unsubscribe).
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Read the next record from the subscription.
    /// Returns `Ok(None)` when the connection closes or the subscription is cancelled.
    pub async fn next(&mut self) -> Result<Option<SubscriptionItem>, Error> {
        match self.rx.recv().await {
            Some(response) => {
                debug!("subscription {}: received {:?}", self.id, response.body);
                match response.body {
                    libslop::ResponseBody::Record(record) => Ok(Some(SubscriptionItem::Record(record))),
                    libslop::ResponseBody::Subscribed => Ok(Some(SubscriptionItem::Subscribed)),
                    libslop::ResponseBody::Error { message } => Err(Error::Server(message)),
                    _ => Ok(None),
                }
            }
            None => Ok(None),
        }
    }
}

/// Build the EventFilter list for `listen`/`wait` from CLI-shaped inputs.
///
/// Empty hooks/events/transcripts and no pane/session/where means "match
/// everything" (empty filter list). If only pane/session/where are set,
/// returns a single catch-all filter scoped to those constraints. Path
/// predicates from `--where` are AND-ed into every emitted filter so they
/// apply uniformly across hook/event/transcript sources.
pub fn build_listen_filters(
    hooks: Vec<String>,
    events: Vec<String>,
    transcripts: Vec<String>,
    pane_id: Option<String>,
    session_id: Option<String>,
    where_preds: Vec<libslop::PayloadPredicate>,
) -> Vec<libslop::EventFilter> {
    if hooks.is_empty() && events.is_empty() && transcripts.is_empty() && pane_id.is_none() && session_id.is_none() && where_preds.is_empty() {
        return vec![];
    }
    if hooks.is_empty() && events.is_empty() && transcripts.is_empty() {
        return vec![libslop::EventFilter {
            pane_id,
            session_id,
            payload_path_match: where_preds,
            ..Default::default()
        }];
    }
    let hook_filters = hooks.into_iter().map(|h| libslop::EventFilter {
        source: Some("hook".to_string()),
        event_type: Some(h),
        pane_id: pane_id.clone(),
        session_id: session_id.clone(),
        payload_path_match: where_preds.clone(),
        ..Default::default()
    });
    let event_filters = events.into_iter().map(|e| libslop::EventFilter {
        source: Some("slopd".to_string()),
        event_type: Some(e),
        pane_id: pane_id.clone(),
        session_id: None,
        payload_path_match: where_preds.clone(),
        ..Default::default()
    });
    let transcript_filters = transcripts.into_iter().map(|t| libslop::EventFilter {
        source: Some("transcript".to_string()),
        event_type: Some(t),
        pane_id: pane_id.clone(),
        session_id: session_id.clone(),
        payload_path_match: where_preds.clone(),
        ..Default::default()
    });
    hook_filters.chain(event_filters).chain(transcript_filters).collect()
}

/// CLI helper: parse `--until` / `--where` flag values into the shared
/// `libslop::PayloadPredicate` type, mapping libslop's String error into our
/// `Error::FilterError`.
pub fn parse_payload_predicates(raw: Vec<String>) -> Result<Vec<libslop::PayloadPredicate>, Error> {
    libslop::parse_payload_predicates(raw).map_err(Error::FilterError)
}

/// Print the `{"subscribed":true}` confirmation, then print every Record from
/// the subscription as a JSON line. After printing each record, `should_stop`
/// is consulted; returning true ends the loop with `Ok(())`. Returns
/// `Err(ConnectionClosed)` if the subscription closes before that. Used by
/// both `listen` (never stops on its own) and `wait` (stops on first match).
async fn print_subscription_until<F>(
    subscription: &mut Subscription,
    mut should_stop: F,
) -> Result<(), Error>
where
    F: FnMut(&libslop::Record) -> bool,
{
    println!("{{\"subscribed\":true}}");
    loop {
        match subscription.next().await? {
            Some(SubscriptionItem::Record(record)) => {
                println!("{}", serde_json::to_string(&record).unwrap());
                if should_stop(&record) {
                    return Ok(());
                }
            }
            Some(SubscriptionItem::Subscribed) => {}
            None => return Err(Error::ConnectionClosed),
        }
    }
}

/// Build a synthetic `CurrentState` record for a pane's current state. Used by
/// `--seed-current` to short-circuit the wait when the pane already satisfies
/// the predicates. The payload mirrors what a real `DetailedStateChange` carries
/// (`state` and `detailed_state`) plus the pane's `session_id`, so predicates
/// against any of those work against the seed.
fn build_seed_record(pane: &libslop::PaneInfo) -> libslop::Record {
    let payload = serde_json::json!({
        "state": pane.state.as_str(),
        "detailed_state": pane.detailed_state.as_str(),
        "session_id": pane.session_id,
        "seeded_current": true,
    });
    libslop::Record {
        cursor: None,
        source: "slopd".to_string(),
        event_type: "CurrentState".to_string(),
        pane_id: Some(pane.pane_id.clone()),
        payload,
    }
}

/// If `--seed-current` is set and the current snapshot of the targeted pane
/// satisfies all `--where` and `--until` predicates, return a synthetic
/// `CurrentState` record. Returns `Ok(None)` to fall through to live event
/// waiting. Skipped (returns None) when `--hook`/`--transcript` filters are
/// set or when `--event` excludes state-relevant types, because the seed
/// represents a steady state — not a hook/transcript record.
#[allow(clippy::too_many_arguments)] // mirrors the wait/listen CLI filter surface
async fn seed_current_if_match<R, W>(
    client: &mut Client<R, W>,
    pane_id: Option<&str>,
    session_id: Option<&str>,
    hooks: &[String],
    events: &[String],
    transcripts: &[String],
    where_preds: &[libslop::PayloadPredicate],
    until_preds: &[libslop::PayloadPredicate],
) -> Result<Option<libslop::Record>, Error>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin,
{
    // The synthetic event is conceptually "slopd / state", not a hook or a
    // transcript record. If the user is filtering on those sources we can't
    // honestly seed.
    if !hooks.is_empty() || !transcripts.is_empty() {
        return Ok(None);
    }
    // If --event is set, only seed when one of the requested event types is a
    // state-relevant kind. Otherwise the seed would silently match an event
    // the user didn't actually ask for.
    if !events.is_empty() {
        const STATE_EVENTS: &[&str] = &["CurrentState", "StateChange", "DetailedStateChange"];
        if !events.iter().any(|e| STATE_EVENTS.contains(&e.as_str())) {
            return Ok(None);
        }
    }

    let panes = client.ps().await?;
    for pane in &panes {
        if let Some(pid) = pane_id
            && pane.pane_id != pid {
                continue;
            }
        if let Some(sid) = session_id
            && pane.session_id.as_deref() != Some(sid) {
                continue;
            }
        let record = build_seed_record(pane);
        if !libslop::predicates_match(&record.payload, where_preds) {
            continue;
        }
        if !libslop::predicates_match(&record.payload, until_preds) {
            continue;
        }
        return Ok(Some(record));
    }
    Ok(None)
}

/// Run the Listen command: build filters, subscribe, print events until SIGTERM or EOF.
#[allow(clippy::too_many_arguments)] // mirrors the `listen` CLI flag surface
pub async fn execute_listen<R, W>(
    client: &mut Client<R, W>,
    hooks: Vec<String>,
    events: Vec<String>,
    transcripts: Vec<String>,
    pane_id: Option<String>,
    session_id: Option<String>,
    where_preds: Vec<String>,
    replay: Option<u64>,
) -> Result<(), Error>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin,
{
    let where_parsed = parse_payload_predicates(where_preds)?;
    let (pane_id, session_id) = resolve_pane_id_or_session(pane_id, session_id)?;
    let mut subscription = if let Some(last_n) = replay {
        if !where_parsed.is_empty() {
            eprintln!("error: --where is incompatible with --replay (transcript replay does not filter by payload)");
            std::process::exit(2);
        }
        let replay_pane_id = match pane_id {
            Some(ref id) => id.clone(),
            None => {
                // The user may have passed a UUID for --pane-id; we already
                // routed it to session_id, but --replay needs a real tmux pane.
                if session_id.is_some() {
                    eprintln!("error: --replay requires a tmux pane id (e.g. %42), not a session UUID");
                } else {
                    eprintln!("error: --replay requires --pane-id");
                }
                std::process::exit(2);
            }
        };
        client.subscribe_transcript(replay_pane_id, last_n).await?
    } else {
        let filters = build_listen_filters(hooks, events, transcripts, pane_id, session_id, where_parsed);
        client.subscribe(filters).await?
    };

    let mut sigterm = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::terminate(),
    ).expect("failed to install SIGTERM handler");

    // listen never stops voluntarily, so should_stop is always false; only
    // SIGTERM or EOF (returned as Err::ConnectionClosed) breaks the loop.
    tokio::select! {
        _ = sigterm.recv() => Ok(()),
        result = print_subscription_until(&mut subscription, |_| false) => {
            match result {
                Err(Error::ConnectionClosed) => Ok(()),
                other => other,
            }
        }
    }
}

/// Run the Wait command: subscribe with the listen-shaped filters, then print
/// records as JSON lines exactly like `listen` does. Exits 0 after printing the
/// first record that satisfies all `--until` predicates; exits 2 on timeout.
///
/// Before entering the live wait, the pane's current state is snapshotted via
/// `ps` and checked against the predicates: if the snapshot already matches, a
/// synthetic `CurrentState` record is emitted and the wait exits 0 without
/// consuming any event. This closes the subscribe-then-snapshot race. Skipped
/// (falls through to live waiting) when `no_snapshot` is set, when neither
/// `--pane-id` nor `--session-id` is set, or when filters constrain to
/// sources other than slopd state.
#[allow(clippy::too_many_arguments)] // mirrors the `wait` CLI flag surface
pub async fn execute_wait<R, W>(
    client: &mut Client<R, W>,
    hooks: Vec<String>,
    events: Vec<String>,
    transcripts: Vec<String>,
    pane_id: Option<String>,
    session_id: Option<String>,
    where_preds: Vec<String>,
    until: Vec<String>,
    no_snapshot: bool,
    timeout_secs: u64,
) -> Result<(), Error>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin,
{
    let predicates = parse_payload_predicates(until)?;
    let where_parsed = parse_payload_predicates(where_preds)?;
    let (pane_id, session_id) = resolve_pane_id_or_session(pane_id, session_id)?;

    // Subscribe FIRST so we don't miss a transition between seed-check and
    // wait-start. If the snapshot matches we emit the synthetic record and
    // exit without consuming any event.
    let filters = build_listen_filters(
        hooks.clone(),
        events.clone(),
        transcripts.clone(),
        pane_id.clone(),
        session_id.clone(),
        where_parsed.clone(),
    );
    let mut subscription = client.subscribe(filters).await?;
    println!("{{\"subscribed\":true}}");

    if !no_snapshot && (pane_id.is_some() || session_id.is_some()) {
        let seeded = seed_current_if_match(
            client,
            pane_id.as_deref(),
            session_id.as_deref(),
            &hooks,
            &events,
            &transcripts,
            &where_parsed,
            &predicates,
        ).await?;
        if let Some(record) = seeded {
            println!("{}", serde_json::to_string(&record).unwrap());
            return Ok(());
        }
    }

    let wait_loop = async {
        loop {
            match subscription.next().await? {
                Some(SubscriptionItem::Record(record)) => {
                    println!("{}", serde_json::to_string(&record).unwrap());
                    if libslop::predicates_match(&record.payload, &predicates) {
                        return Ok(());
                    }
                }
                Some(SubscriptionItem::Subscribed) => {}
                None => return Err(Error::ConnectionClosed),
            }
        }
    };

    if timeout_secs == 0 {
        return wait_loop.await;
    }

    let duration = std::time::Duration::from_secs(timeout_secs);
    match tokio::time::timeout(duration, wait_loop).await {
        Ok(result) => result,
        Err(_) => {
            eprintln!("error: timed out after {}s waiting for matching event", timeout_secs);
            std::process::exit(2);
        }
    }
}

/// Settle window: once the freshly-spawned pane first reaches a non-booting
/// detailed state we keep watching this long for an early death (SessionEnd or
/// PaneDestroyed) before declaring success. The pane reaches `ready` at
/// SessionStart but a broken session (e.g. a bad `--resume` target) exits
/// ~1-2s *later* with `reason=prompt_input_exit`, so success cannot be declared
/// the instant `ready` is observed — we must outlast that failure window.
const RUN_SETTLE: std::time::Duration = std::time::Duration::from_secs(3);

/// Build the user-facing error for a pane that died during the readiness wait,
/// including the SessionEnd `reason` when we observed it.
fn run_died_error(pane_id: &str, reason: Option<&str>) -> Error {
    match reason {
        Some(r) => Error::RunFailed(format!(
            "pane {} died before becoming ready (session ended: {})",
            pane_id, r,
        )),
        None => Error::RunFailed(format!("pane {} died before becoming ready", pane_id)),
    }
}

/// Run the Run command. By default this waits for the freshly-spawned pane to
/// become ready before returning, turning a silently-dead pane into a visible
/// failure:
///
/// - the pane reaches a live state and survives a short settle window →
///   print the pane id and exit 0 (same output as before).
/// - a `SessionEnd` hook or `PaneDestroyed` event for the pane arrives first →
///   return an error (non-zero exit), including the SessionEnd `reason` if seen.
/// - the ready timeout elapses before the pane becomes live → print the pane id
///   (so the caller can still investigate) and exit 2.
///
/// With `no_wait` it restores the historical fire-and-forget behaviour: issue
/// the Run request and print the pane id as soon as the pane is created.
///
/// Correctness rests on slopd handling a connection's requests sequentially: we
/// Subscribe (which registers our broadcast receiver) and wait for the
/// confirmation *before* issuing Run, so we cannot miss the new pane's state
/// transitions or an early death — including the `SessionStart`/`SessionEnd`
/// hooks, which broadcast to the same channel. The pane id isn't known at
/// subscribe time, so we subscribe to the relevant event *types* across all
/// panes and filter by pane id on the client side once Run returns.
#[allow(clippy::too_many_arguments)] // mirrors the `run` CLI flag surface (+account)
pub async fn execute_run<R, W>(
    client: &mut Client<R, W>,
    parent_pane_id: Option<String>,
    extra_args: Vec<String>,
    start_directory: Option<PathBuf>,
    env: Vec<(String, String)>,
    account: Option<String>,
    no_wait: bool,
    ready_timeout_secs: u64,
) -> Result<(), Error>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin,
{
    if no_wait {
        let pane_id = client.run(parent_pane_id, extra_args, start_directory, env, account).await?;
        println!("{}", pane_id);
        return Ok(());
    }

    let filters = vec![
        libslop::EventFilter {
            source: Some("slopd".to_string()),
            event_type: Some("DetailedStateChange".to_string()),
            ..Default::default()
        },
        libslop::EventFilter {
            source: Some("slopd".to_string()),
            event_type: Some("PaneDestroyed".to_string()),
            ..Default::default()
        },
        libslop::EventFilter {
            source: Some("hook".to_string()),
            event_type: Some("SessionEnd".to_string()),
            ..Default::default()
        },
    ];
    let mut subscription = client.subscribe(filters).await?;

    let pane_id = client.run(parent_pane_id, extra_args, start_directory, env, account).await?;

    let overall_deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(ready_timeout_secs);
    // Set once the pane first reaches a live (non-booting) state; thereafter the
    // wait races this settle deadline instead of the overall ready timeout.
    let mut settle_deadline: Option<std::time::Instant> = None;

    loop {
        let deadline = settle_deadline.unwrap_or(overall_deadline);
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            if settle_deadline.is_some() {
                // Became live and survived the settle window: healthy.
                println!("{}", pane_id);
                return Ok(());
            }
            // Never became live within the budget. Print the id anyway so the
            // caller can investigate, then exit non-zero.
            println!("{}", pane_id);
            eprintln!(
                "error: timed out after {}s waiting for pane {} to become ready",
                ready_timeout_secs, pane_id,
            );
            std::process::exit(2);
        }

        match tokio::time::timeout(remaining, subscription.next()).await {
            Err(_) => continue, // deadline hit mid-recv; re-evaluate at top of loop
            Ok(Ok(Some(SubscriptionItem::Record(record)))) => {
                if record.pane_id.as_deref() != Some(pane_id.as_str()) {
                    continue;
                }
                match (record.source.as_str(), record.event_type.as_str()) {
                    ("hook", "SessionEnd") => {
                        let reason = record.payload.get("reason").and_then(|v| v.as_str());
                        return Err(run_died_error(&pane_id, reason));
                    }
                    ("slopd", "PaneDestroyed") => {
                        return Err(run_died_error(&pane_id, None));
                    }
                    ("slopd", "DetailedStateChange") => {
                        let live = record
                            .payload
                            .get("detailed_state")
                            .and_then(|v| v.as_str())
                            .is_some_and(|s| s != libslop::PaneDetailedState::BootingUp.as_str());
                        if live && settle_deadline.is_none() {
                            settle_deadline = Some(std::time::Instant::now() + RUN_SETTLE);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Ok(Some(SubscriptionItem::Subscribed))) => {}
            Ok(Ok(None)) | Ok(Err(_)) => {
                return Err(Error::RunFailed(format!(
                    "lost connection to slopd while waiting for pane {} to become ready",
                    pane_id,
                )));
            }
        }
    }
}

/// Run for `run --interactive`: create the pane (without waiting for it to
/// become ready), then hand off to the viewer command — its `{}` placeholders
/// replaced with the new pane id — per the configured run type.
#[allow(clippy::too_many_arguments)] // mirrors the `run` CLI flag surface
pub async fn execute_run_interactive<R, W>(
    client: &mut Client<R, W>,
    parent_pane_id: Option<String>,
    extra_args: Vec<String>,
    start_directory: Option<PathBuf>,
    env: Vec<(String, String)>,
    account: Option<String>,
    viewer: &InteractiveRun,
) -> Result<(), Error>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin,
{
    let pane_id = client.run(parent_pane_id, extra_args, start_directory, env, account).await?;
    // Pre-resolved vars (socket, session, …) plus the now-known pane id.
    let mut vars: Vec<(&str, &str)> = viewer.vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    vars.push(("pane_id", &pane_id));
    let argv = libslop::SlopctlConfig::substitute(&viewer.command, &vars);
    run_viewer_command(&argv, viewer.run_type, &pane_id)
}

/// Launch the (already pane-id-substituted) viewer command. `Exec` replaces this
/// process with it (only returns on failure); `Forking` runs it detached in the
/// background, prints the pane id, and returns.
fn run_viewer_command(argv: &[String], run_type: libslop::RunType, pane_id: &str) -> Result<(), Error> {
    let Some((program, args)) = argv.split_first() else {
        return Err(Error::RunFailed("interactive_command is empty".to_string()));
    };
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    match run_type {
        libslop::RunType::Exec => {
            use std::os::unix::process::CommandExt;
            // Replaces the slopctl process on success; only returns on error.
            let err = cmd.exec();
            Err(Error::RunFailed(format!(
                "failed to exec interactive command {:?}: {}", argv, err,
            )))
        }
        libslop::RunType::Forking => {
            use std::os::unix::process::CommandExt;
            use std::process::Stdio;
            // Detach: a fresh process group + null stdio so it survives slopctl
            // exiting and doesn't fight over the terminal.
            cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            cmd.process_group(0);
            cmd.spawn().map_err(|e| Error::RunFailed(format!(
                "failed to spawn interactive command {:?}: {}", argv, e,
            )))?;
            // Background mode reports the pane id like `run --no-wait` does.
            println!("{}", pane_id);
            Ok(())
        }
    }
}

/// Execute a CommonCommand against the given client.
pub async fn execute_command<R, W>(
    client: &mut Client<R, W>,
    command: CommonCommand,
    ctx: &CommandContext,
) -> Result<(), Error>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin,
{
    // Handle Send-with-filter first.
    if let CommonCommand::Send { ref pane_id, .. } = command
        && pane_id.contains('=')
            && let CommonCommand::Send { pane_id, prompt, filters, select, timeout, interrupt } = command {
                let mut all_filters = vec![pane_id];
                all_filters.extend(filters);
                let parsed = parse_filters(all_filters)?;
                let pane_ids = client.send_filtered(&parsed, &prompt, &select, timeout, interrupt).await?;
                for id in pane_ids {
                    println!("{}", id);
                }
                return Ok(());
            }

    match command {
        CommonCommand::Status => {
            let state = client.status().await?;
            println!("uptime: {}s", state.uptime_secs);
            println!("subscribers: {}", state.subscriber_count);
            println!("config_generation: {}", state.config_generation);
        }
        CommonCommand::Ps { filters, json } => {
            let parsed = parse_filters(filters)?;
            let all_panes = client.ps().await?;
            let panes = apply_filters(all_panes, &parsed);
            if json {
                println!("{}", serde_json::to_string(&panes).unwrap());
            } else {
                print_ps(panes);
            }
        }
        CommonCommand::Run { extra_args, start_directory, envs, env_files, account, interactive, no_wait, ready_timeout } => {
            let env = build_cli_env(&env_files, &envs)?;
            // Pass --account through verbatim; when it's None the daemon inherits
            // the parent pane's account (via parent_pane_id), then default_account.
            if interactive {
                let Some(viewer) = ctx.interactive.as_ref() else {
                    return Err(Error::RunFailed(
                        "`run --interactive` is not supported for remote endpoints".to_string(),
                    ));
                };
                execute_run_interactive(
                    client,
                    ctx.parent_pane_id.clone(),
                    extra_args,
                    start_directory,
                    env,
                    account,
                    viewer,
                ).await?;
            } else {
                execute_run(
                    client,
                    ctx.parent_pane_id.clone(),
                    extra_args,
                    start_directory,
                    env,
                    account,
                    no_wait,
                    ready_timeout,
                ).await?;
            }
        }
        CommonCommand::Kill { pane_id } => {
            let pane_id = client.kill(pane_id).await?;
            println!("{}", pane_id);
        }
        CommonCommand::Send { pane_id, prompt, timeout, interrupt, .. } => {
            let pane_id = client.send_prompt(pane_id, prompt, timeout, interrupt).await?;
            println!("{}", pane_id);
        }
        CommonCommand::Interrupt { pane_id } => {
            let pane_id = client.interrupt(pane_id).await?;
            println!("{}", pane_id);
        }
        CommonCommand::Tag { pane_id, tag } => {
            let (pane_id, tag) = client.tag(pane_id, tag).await?;
            println!("{} {}", pane_id, tag);
        }
        CommonCommand::Untag { pane_id, tag } => {
            let (pane_id, tag) = client.untag(pane_id, tag).await?;
            println!("{} {}", pane_id, tag);
        }
        CommonCommand::Tags { pane_id } => {
            let pane_id = pane_id.or(ctx.fallback_pane_id.clone()).unwrap();
            let tags = client.tags(pane_id).await?;
            for tag in tags {
                println!("{}", tag);
            }
        }
        CommonCommand::Transcript { pane_id, before, limit } => {
            let records = client.read_transcript(pane_id, before, limit).await?;
            let out = serde_json::json!({ "records": records });
            println!("{}", out);
        }
        CommonCommand::Listen { hooks, events, transcripts, pane_id, session_id, where_preds, replay } => {
            execute_listen(client, hooks, events, transcripts, pane_id, session_id, where_preds, replay).await?;
        }
        CommonCommand::Wait { hooks, events, transcripts, pane_id, session_id, where_preds, until, no_snapshot, timeout } => {
            execute_wait(client, hooks, events, transcripts, pane_id, session_id, where_preds, until, no_snapshot, timeout).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_like_uuid_accepts_canonical_uuid() {
        assert!(looks_like_uuid("31a02dee-3e6d-42f0-b7c4-4382305b7e10"));
        assert!(looks_like_uuid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"));
        assert!(looks_like_uuid("01234567-89ab-cdef-0123-456789abcdef"));
    }

    #[test]
    fn looks_like_uuid_rejects_non_uuid() {
        assert!(!looks_like_uuid("%79"));
        assert!(!looks_like_uuid("not-a-uuid"));
        assert!(!looks_like_uuid(""));
        // Right length but contains a non-hex char.
        assert!(!looks_like_uuid("31a02dee-3e6d-42f0-b7c4-4382305b7e1z"));
        // Wrong group lengths.
        assert!(!looks_like_uuid("31a02de-3e6d-42f0-b7c4-4382305b7e10"));
    }

    #[test]
    fn validate_pane_id_arg_accepts_tmux_pane() {
        assert!(validate_pane_id_arg("%0").is_ok());
        assert!(validate_pane_id_arg("%79").is_ok());
        assert!(validate_pane_id_arg("%12345").is_ok());
    }

    #[test]
    fn validate_pane_id_arg_rejects_uuid_with_session_id_hint() {
        let err = validate_pane_id_arg("31a02dee-3e6d-42f0-b7c4-4382305b7e10").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--pane-id"), "missing flag name in error: {}", msg);
        assert!(msg.contains("UUID"), "missing UUID hint in error: {}", msg);
        assert!(msg.contains("--session-id"), "must point at --session-id: {}", msg);
    }

    #[test]
    fn validate_pane_id_arg_rejects_garbage() {
        let err = validate_pane_id_arg("not-a-pane-id").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--pane-id"), "missing flag name in error: {}", msg);
        assert!(msg.contains("tmux pane id"), "missing shape hint: {}", msg);
        assert!(msg.contains("--session-id"), "should still hint at session-id: {}", msg);
    }

    #[test]
    fn validate_pane_id_arg_rejects_percent_without_digits() {
        // `%` alone or `%abc` shouldn't be treated as a tmux pane id.
        assert!(validate_pane_id_arg("%").is_err());
        assert!(validate_pane_id_arg("%abc").is_err());
    }

    #[test]
    fn resolve_pane_id_or_session_passes_through_tmux_pane() {
        let (p, s) = resolve_pane_id_or_session(Some("%79".into()), None).unwrap();
        assert_eq!(p.as_deref(), Some("%79"));
        assert_eq!(s, None);
    }

    #[test]
    fn resolve_pane_id_or_session_rejects_uuid_in_pane_id() {
        let uuid = "31a02dee-3e6d-42f0-b7c4-4382305b7e10";
        let err = resolve_pane_id_or_session(Some(uuid.into()), None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("UUID"), "should explain it's a UUID: {}", msg);
        assert!(msg.contains("--session-id"), "should point at --session-id: {}", msg);
    }

    #[test]
    fn resolve_pane_id_or_session_keeps_explicit_session_id() {
        let uuid = "31a02dee-3e6d-42f0-b7c4-4382305b7e10";
        let (p, s) = resolve_pane_id_or_session(None, Some(uuid.into())).unwrap();
        assert_eq!(p, None);
        assert_eq!(s.as_deref(), Some(uuid));
    }

    #[test]
    fn resolve_pane_id_or_session_rejects_garbage() {
        let err = resolve_pane_id_or_session(Some("garbage".into()), None).unwrap_err();
        assert!(err.to_string().contains("--pane-id"));
    }

    fn fake_pane(pane_id: &str, state: libslop::PaneDetailedState, session_id: Option<&str>) -> libslop::PaneInfo {
        libslop::PaneInfo {
            pane_id: pane_id.to_string(),
            created_at: 0,
            last_active: 0,
            session_id: session_id.map(String::from),
            parent_pane_id: None,
            tags: vec![],
            state: state.to_simple(),
            detailed_state: state,
            working_dir: None,
            account: libslop::DEFAULT_ACCOUNT.to_string(),
        }
    }

    #[test]
    fn build_seed_record_has_state_and_detailed_state_in_payload() {
        let pane = fake_pane("%79", libslop::PaneDetailedState::Ready, Some("abc-123"));
        let record = build_seed_record(&pane);
        assert_eq!(record.source, "slopd");
        assert_eq!(record.event_type, "CurrentState");
        assert_eq!(record.pane_id.as_deref(), Some("%79"));
        assert_eq!(record.payload["state"], "ready");
        assert_eq!(record.payload["detailed_state"], "ready");
        assert_eq!(record.payload["session_id"], "abc-123");
        assert_eq!(record.payload["seeded_current"], true);
    }

    #[test]
    fn build_seed_record_predicate_match_against_state() {
        let pane = fake_pane("%79", libslop::PaneDetailedState::Ready, None);
        let record = build_seed_record(&pane);
        let predicates = parse_payload_predicates(vec!["state=ready".into()]).unwrap();
        assert!(libslop::predicates_match(&record.payload, &predicates));
        let no_match = parse_payload_predicates(vec!["state=busy".into()]).unwrap();
        assert!(!libslop::predicates_match(&record.payload, &no_match));
    }

    #[test]
    fn build_seed_record_predicate_match_against_detailed_state() {
        let pane = fake_pane("%79", libslop::PaneDetailedState::AwaitingInputPermission, None);
        let record = build_seed_record(&pane);
        let predicates = parse_payload_predicates(
            vec!["detailed_state=awaiting_input_permission".into()],
        ).unwrap();
        assert!(libslop::predicates_match(&record.payload, &predicates));
    }

    /// Verify that the leading-dot equivalence promised in --help text is the
    /// behavior of the underlying parser; this guards the help docs against
    /// drift from the implementation.
    #[test]
    fn where_predicate_leading_dot_optional() {
        let with_dot = parse_payload_predicates(vec![".state=ready".into()]).unwrap();
        let without_dot = parse_payload_predicates(vec!["state=ready".into()]).unwrap();
        assert_eq!(with_dot.len(), 1);
        assert_eq!(without_dot.len(), 1);
        assert_eq!(with_dot[0].path, without_dot[0].path);
        assert_eq!(with_dot[0].expected, "ready");
        assert_eq!(without_dot[0].expected, "ready");
    }

    /// A nested path's leading dot is also optional (consistency with top-level).
    #[test]
    fn where_predicate_leading_dot_optional_nested() {
        let with_dot = parse_payload_predicates(vec![".message.content[].type=text".into()]).unwrap();
        let without_dot = parse_payload_predicates(vec!["message.content[].type=text".into()]).unwrap();
        assert_eq!(with_dot[0].path, without_dot[0].path);
    }
}
