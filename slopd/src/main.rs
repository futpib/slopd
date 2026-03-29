use clap::Parser;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{Mutex, Notify};
use tracing::{debug, error, info, warn};

#[derive(Parser)]
#[command(name = "slopd", about = "Claude session manager daemon", version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT"), ")"))]
struct Cli {
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
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

fn hook_event_to_detailed_state(event: &str) -> Option<libslop::PaneDetailedState> {
    match event {
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
    *pane_state(panes, pane_id).detailed_state.lock().unwrap() = detailed.clone();
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

type PaneMap = Arc<dashmap::DashMap<String, Arc<PaneState>>>;

fn pane_state(panes: &PaneMap, pane_id: &str) -> Arc<PaneState> {
    panes
        .entry(pane_id.to_string())
        .or_insert_with(|| Arc::new(PaneState::new()))
        .clone()
}

/// Set of pane IDs in the `slopd` tmux session.
/// Populated from tmux on startup (so it survives slopd restarts) and kept
/// in sync as panes are created/killed.
type ManagedPanes = Arc<dashmap::DashSet<String>>;

/// Populate the managed-pane set from the `slopd` tmux session.
/// Only panes that have the `@slopd_managed` pane option set are considered managed
/// (i.e. were registered via `slopctl run`). This filters out panes that were
/// created directly in the slopd session without going through slopd.
/// Resets state to booting_up for all recovered panes since we don't know
/// where Claude actually is after a slopd restart.
async fn load_managed_panes(config: &libslop::SlopdConfig, managed: &ManagedPanes, event_tx: &EventTx, panes: &PaneMap) {
    let output = tmux(config)
        .args(["list-panes", "-s", "-t", "slopd", "-F",
               &format!("#{{pane_id}} #{{{}}}",
                        libslop::TmuxOption::SlopdManaged.as_str())])
        .output()
        .await;
    if let Ok(out) = output {
        if out.status.success() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let mut parts = line.splitn(2, ' ');
                let pane_id = parts.next().unwrap_or("").trim();
                let slopd_managed = parts.next().unwrap_or("").trim();
                if pane_id.is_empty() || slopd_managed != "true" {
                    continue;
                }
                managed.insert(pane_id.to_string());
                set_pane_detailed_state(config, pane_id, &libslop::PaneDetailedState::BootingUp, None, event_tx, panes).await;
            }
        }
    }
}

type EventTx = Arc<tokio::sync::broadcast::Sender<libslop::Record>>;

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

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let level = libslop::verbosity_to_level(cli.verbose);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level.as_str())),
        )
        .with_writer(std::io::stderr)
        .init();

    let config = Arc::new(libslop::SlopdConfig::load());

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

    // Create the slopd session if it doesn't exist (-A: attach if exists, -d: keep detached)
    tmux(&config)
        .args(["new-session", "-d", "-A", "-s", "slopd"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .expect("failed to create slopd tmux session");

    // Mark the session with a user option so it can be identified
    tmux(&config)
        .args(["set-option", "-t", "slopd", libslop::TmuxOption::SlopdManaged.as_str(), "true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .expect("failed to set @slopd_managed option on tmux session");

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

    let panes: PaneMap = Arc::new(dashmap::DashMap::new());
    let managed_panes: ManagedPanes = Arc::new(dashmap::DashSet::new());

    let (event_tx, _) = tokio::sync::broadcast::channel::<libslop::Record>(256);
    let event_tx: EventTx = Arc::new(event_tx);

    // Recover managed pane IDs from the tmux session so panes that existed
    // before a slopd restart are still recognized. This must happen before
    // binding the socket so that clients cannot create panes in the slopd
    // session while the scan is in progress.
    load_managed_panes(&config, &managed_panes, &event_tx, &panes).await;

    let _ = tokio::fs::remove_file(&socket_path).await;

    let listener = UnixListener::bind(&socket_path).unwrap();
    info!("listening on {}", socket_path.display());

    let mut sigterm = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::terminate(),
    ).expect("failed to install SIGTERM handler");

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _addr) = result.unwrap();
                debug!("accepted connection");
                tokio::spawn(handle_connection(stream, start_time, config.clone(), panes.clone(), managed_panes.clone(), event_tx.clone()));
            }
            _ = sigterm.recv() => {
                info!("received SIGTERM, shutting down");
                break;
            }
        }
    }
}

async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    id: u64,
    body: libslop::ResponseBody,
) -> std::io::Result<()> {
    let response = libslop::Response { id, body };
    let mut json = serde_json::to_string(&response).unwrap();
    debug!("sending: {}", json);
    json.push('\n');
    writer.write_all(json.as_bytes()).await
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
    writer: &mut tokio::net::unix::OwnedWriteHalf,
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

async fn handle_connection(
    stream: tokio::net::UnixStream,
    start_time: u64,
    config: Arc<libslop::SlopdConfig>,
    panes: PaneMap,
    managed_panes: ManagedPanes,
    event_tx: EventTx,
) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        debug!("received: {}", line);
        let req = match serde_json::from_str::<libslop::Request>(&line) {
            Ok(req) => req,
            Err(e) => {
                warn!("failed to parse request: {}", e);
                let _ = write_response(&mut writer, 0, libslop::ResponseBody::Error { message: e.to_string() }).await;
                continue;
            }
        };

        match req.body {
            libslop::RequestBody::Subscribe { filters } => {
                let mut rx = event_tx.subscribe();
                if write_response(&mut writer, req.id, libslop::ResponseBody::Subscribed).await.is_err() {
                    return;
                }
                if stream_events(&mut rx, &mut writer, req.id, &filters, None).await.is_err() {
                    return;
                }
                continue;
            }

            libslop::RequestBody::SubscribeTranscript { pane_id, last_n } => {
                // Step 1: Subscribe to broadcast FIRST to avoid gaps.
                let mut rx = event_tx.subscribe();

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
                if write_response(&mut writer, req.id, libslop::ResponseBody::Subscribed).await.is_err() {
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
                    if write_response(&mut writer, req.id, libslop::ResponseBody::Record(record)).await.is_err() {
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
                if write_response(&mut writer, req.id, libslop::ResponseBody::Record(replay_end)).await.is_err() {
                    return;
                }

                // Step 6: Enter live event loop, skipping transcript records already replayed.
                // Only forward transcript records for this pane.
                let transcript_filter = vec![libslop::EventFilter {
                    source: Some("transcript".to_string()),
                    event_type: None,
                    pane_id: Some(pane_id.clone()),
                    session_id: None,
                    payload_match: serde_json::Map::new(),
                }];
                let dedup = Some(Dedup { pane_id, file_end_offset });
                if stream_events(&mut rx, &mut writer, req.id, &transcript_filter, dedup.as_ref()).await.is_err() {
                    return;
                }
                continue;
            }

            body => {
                let body = handle_request(body, start_time, &config, &panes, &managed_panes, &event_tx).await;
                if write_response(&mut writer, req.id, body).await.is_err() {
                    break;
                }
            }
        }
    }
}

fn parse_pane_options(stdout: &str) -> (bool, Option<String>, Option<String>, Vec<String>, Option<libslop::PaneDetailedState>, Option<u64>) {
    let mut slopd_managed = false;
    let mut session_id = None;
    let mut parent_pane_id = None;
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
        } else if key == libslop::TmuxOption::SlopdParentPane.as_str() {
            parent_pane_id = Some(val.to_string());
        } else if key == libslop::TmuxOption::SlopdDetailedState.as_str() {
            detailed_state = libslop::PaneDetailedState::from_str(val);
        } else if key == libslop::TmuxOption::SlopdCreatedAt.as_str() {
            created_at = val.parse::<u64>().ok();
        } else if let Some(tag) = key.strip_prefix(libslop::TAG_OPTION_PREFIX) {
            tags.push(tag.to_string());
        }
    }
    (slopd_managed, session_id, parent_pane_id, tags, detailed_state, created_at)
}

async fn list_panes(config: &libslop::SlopdConfig) -> Result<Vec<libslop::PaneInfo>, String> {
    let list_out = tmux(config)
        .args(["list-panes", "-s", "-t", "slopd", "-F", "#{pane_id} #{window_activity}"])
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if !list_out.status.success() {
        return Err(String::from_utf8_lossy(&list_out.stderr).trim().to_string());
    }

    let mut panes = Vec::new();
    for line in String::from_utf8_lossy(&list_out.stdout).lines() {
        let mut parts = line.splitn(2, ' ');
        let pane_id = match parts.next() {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => continue,
        };
        let last_active: u64 = parts.next().unwrap_or("0").trim().parse().unwrap_or(0);

        let opts_out = tmux(config)
            .args(["show-options", "-t", &pane_id, "-p"])
            .output()
            .await;
        let (session_id, parent_pane_id, tags, state, detailed_state, created_at) = match opts_out {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let (slopd_managed, session_id, parent_pane_id, tags, detailed_state, created_at) = parse_pane_options(&stdout);
                if !slopd_managed {
                    continue;
                }
                let detailed_state = detailed_state.unwrap_or(libslop::PaneDetailedState::BootingUp);
                let state = detailed_state.to_simple();
                let created_at = created_at.unwrap_or(last_active);
                (session_id, parent_pane_id, tags, state, detailed_state, created_at)
            }
            _ => continue,
        };

        panes.push(libslop::PaneInfo { pane_id, created_at, last_active, session_id, parent_pane_id, tags, state, detailed_state });
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
            let output = tmux(config)
                .args(["kill-pane", "-t", &pane_id])
                .output()
                .await;
            match output {
                Ok(out) if out.status.success() => {
                    if let Some((_, state)) = panes.remove(&pane_id) {
                        state.transcript_cancel.lock().unwrap().cancel();
                    }
                    managed_panes.remove(&pane_id);
                    libslop::ResponseBody::Kill { pane_id }
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    libslop::ResponseBody::Error { message: stderr }
                }
                Err(e) => libslop::ResponseBody::Error { message: e.to_string() },
            }
        }

        libslop::RequestBody::Hook { event, payload, pane_id } => {
            debug!("hook: {} pane={:?}", event, pane_id);

            // Ignore hooks from panes that were not spawned by slopd. This can happen
            // when an external Claude instance shares the same settings.json with
            // injected hooks.
            if let Some(pane) = pane_id.as_deref() {
                if !managed_panes.contains(pane) {
                    debug!("ignoring hook from unmanaged pane {}", pane);
                    return libslop::ResponseBody::Hooked;
                }
            }

            // Start (or re-start) tailing the transcript file whenever a hook
            // includes a transcript_path we haven't seen yet for this pane.
            // This covers both SessionStart and any hook fired after a slopd
            // restart where the tailer is no longer running.
            if let (Some(pane), Some(transcript_path)) = (
                pane_id.as_deref(),
                payload.get("transcript_path").and_then(|v| v.as_str()),
            ) {
                let state = pane_state(panes, pane);
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
                        event_tx.clone(),
                        new_cancel,
                    ));
                }
            }

            if event == "SessionStart" {
                if let (Some(pane), Some(session_id)) = (
                    pane_id.as_deref(),
                    payload.get("session_id").and_then(|v| v.as_str()),
                ) {
                    debug!("SessionStart: pane={} session_id={}", pane, session_id);
                    if let Err(e) = tmux_set_pane_option(config, pane, libslop::TmuxOption::SlopdClaudeSessionId.as_str(), session_id).await {
                        warn!("failed to set @slopd_claude_session_id on pane {}: {}", pane, e);
                    }

                    set_pane_detailed_state(config, pane, &libslop::PaneDetailedState::Ready, None, event_tx, panes).await;
                }
            }

            if event == "UserPromptSubmit" {
                if let Some(pane) = pane_id.as_deref() {
                    debug!("UserPromptSubmit: notifying pending senders for pane {}", pane);
                    pane_state(panes, pane).prompt_submitted.notify_waiters();
                    set_pane_detailed_state(config, pane, &libslop::PaneDetailedState::BusyProcessing, None, event_tx, panes).await;
                }
            }

            let detailed_state = hook_event_to_detailed_state(event.as_str());
            if let (Some(pane), Some(state)) = (pane_id.as_deref(), detailed_state) {
                set_pane_detailed_state(config, pane, &state, None, event_tx, panes).await;
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

        libslop::RequestBody::Run { parent_pane_id, extra_args, start_directory } => {
            let settings_path = config.claude_settings_path();
            if let Err(e) = libslop::inject_hooks_into_file(&settings_path, &config.run.slopctl) {
                warn!("failed to inject hooks into {}: {}", settings_path.display(), e);
            }
            let xdg_runtime_dir = libslop::runtime_dir();
            let mut cmd = tmux(config);
            cmd.args(["new-window", "-t", "slopd", "-P", "-F", "#{pane_id}"])
                .args(["-e", &format!("XDG_RUNTIME_DIR={}", xdg_runtime_dir.display())])
                .args(["-e", &format!("SLOPCTL={}", config.run.slopctl)]);
            // Resolve start directory: per-session flag takes precedence over config default.
            // Config value is expanded (~ and $VAR); the CLI value is already shell-expanded.
            let effective_start_dir = start_directory.or_else(|| {
                config.run.start_directory.as_ref().map(|p| libslop::expand_path(p))
            });
            if let Some(ref dir) = effective_start_dir {
                match dir.to_str() {
                    Some(dir_str) => { cmd.args(["-c", dir_str]); }
                    None => warn!("start_directory contains non-UTF-8 characters, ignoring: {}", dir.display()),
                }
            }
            if let Some(ref custom_dir) = config.claude_config_dir {
                cmd.args(["-e", &format!("CLAUDE_CONFIG_DIR={}", custom_dir.display())]);
            }
            // Forward LLVM_PROFILE_FILE so instrumented child binaries (e.g. mock_claude)
            // write their coverage data even when launched inside a tmux window.
            if let Ok(profile_file) = std::env::var("LLVM_PROFILE_FILE") {
                cmd.args(["-e", &format!("LLVM_PROFILE_FILE={}", profile_file)]);
            }
            let output = cmd
                .arg(config.run.executable.program())
                .args(config.run.executable.args())
                .args(&extra_args)
                .output()
                .await;
            match output {
                Ok(out) if out.status.success() => {
                    let pane_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    debug!("spawned {:?} in pane {}", config.run.executable, pane_id);
                    managed_panes.insert(pane_id.clone());
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
                    let current_state = pane_state(panes, &pane_id).detailed_state.lock().unwrap().clone();
                    if current_state == libslop::PaneDetailedState::BootingUp {
                        set_pane_detailed_state(config, &pane_id, &libslop::PaneDetailedState::BootingUp, None, event_tx, panes).await;
                    }
                    if let Some(ref parent) = parent_pane_id {
                        if let Err(e) = tmux_set_pane_option(config, &pane_id, libslop::TmuxOption::SlopdParentPane.as_str(), parent).await {
                            warn!("failed to set @slopd_parent_pane on pane {}: {}", pane_id, e);
                        }
                    }
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
            let state = pane_state(panes, &pane_id);
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
            let state = pane_state(panes, &pane_id);

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
            match list_panes(config).await {
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

        libslop::RequestBody::Subscribe { .. } | libslop::RequestBody::SubscribeTranscript { .. } => {
            // Handled in handle_connection before reaching here.
            unreachable!("Subscribe/SubscribeTranscript should be handled before handle_request")
        }
    }
}
