use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{Mutex, Notify};
use tracing::{debug, error, info, warn};

fn verbosity_to_level(verbosity: u8) -> tracing::Level {
    match verbosity {
        0 => tracing::Level::WARN,
        1 => tracing::Level::INFO,
        2 => tracing::Level::DEBUG,
        _ => tracing::Level::TRACE,
    }
}

fn tmux(config: &libslop::SlopdConfig) -> std::process::Command {
    let mut cmd = std::process::Command::new("tmux");
    if let Some(socket) = &config.tmux.socket {
        cmd.args(["-S", socket.to_str().unwrap()]);
    }
    cmd
}

/// Per-pane state shared across connection handlers.
struct PaneState {
    /// Serialises the type-then-enter sequence so two concurrent sends don't interleave.
    type_mutex: Mutex<()>,
    /// Notified whenever UserPromptSubmit fires for this pane.
    prompt_submitted: Notify,
}

impl PaneState {
    fn new() -> Self {
        Self {
            type_mutex: Mutex::new(()),
            prompt_submitted: Notify::new(),
        }
    }
}

type PaneMap = Arc<dashmap::DashMap<String, Arc<PaneState>>>;

fn pane_state(panes: &PaneMap, pane_id: &str) -> Arc<PaneState> {
    panes
        .entry(pane_id.to_string())
        .or_insert_with(|| Arc::new(PaneState::new()))
        .clone()
}

/// An event broadcast to all active subscribers.
#[derive(Debug, Clone)]
struct BroadcastEvent {
    source: String,
    event_type: String,
    pane_id: Option<String>,
    payload: serde_json::Value,
}

type EventTx = Arc<tokio::sync::broadcast::Sender<BroadcastEvent>>;

fn filters_match(filters: &[libslop::EventFilter], ev: &BroadcastEvent) -> bool {
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
    let args: Vec<String> = std::env::args().collect();
    let verbosity = args.iter().filter(|a| *a == "-v").count() as u8;

    let level = verbosity_to_level(verbosity);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level.as_str())),
        )
        .with_writer(std::io::stderr)
        .init();

    let config = Arc::new(libslop::SlopdConfig::load());

    let status = tmux(&config)
        .arg("list-sessions")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("failed to run tmux");
    if !status.success() {
        error!("tmux is not running");
        std::process::exit(1);
    }

    // Create the slopd session if it doesn't exist (-A: attach if exists, -d: keep detached)
    tmux(&config)
        .args(["new-session", "-d", "-A", "-s", "slopd"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("failed to create slopd tmux session");

    // Mark the session with a user option so it can be identified
    tmux(&config)
        .args(["set-option", "-t", "slopd", libslop::TmuxOption::SlopdManaged.as_str(), "true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("failed to set @slopd_managed option on tmux session");

    let socket_path = libslop::socket_path();
    let socket_dir = socket_path.parent().unwrap();

    tokio::fs::create_dir_all(&socket_dir).await.unwrap();
    let _ = tokio::fs::remove_file(&socket_path).await;

    let listener = UnixListener::bind(&socket_path).unwrap();
    info!("listening on {}", socket_path.display());

    let start_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let panes: PaneMap = Arc::new(dashmap::DashMap::new());

    let (event_tx, _) = tokio::sync::broadcast::channel::<BroadcastEvent>(256);
    let event_tx: EventTx = Arc::new(event_tx);

    let mut sigterm = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::terminate(),
    ).expect("failed to install SIGTERM handler");

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _addr) = result.unwrap();
                debug!("accepted connection");
                tokio::spawn(handle_connection(stream, start_time, config.clone(), panes.clone(), event_tx.clone()));
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

async fn handle_connection(
    stream: tokio::net::UnixStream,
    start_time: u64,
    config: Arc<libslop::SlopdConfig>,
    panes: PaneMap,
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

        if let libslop::RequestBody::Subscribe { filters } = req.body {
            if write_response(&mut writer, req.id, libslop::ResponseBody::Subscribed).await.is_err() {
                return;
            }
            let mut rx = event_tx.subscribe();
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if filters_match(&filters, &ev) {
                            let body = libslop::ResponseBody::Event {
                                source: ev.source,
                                event_type: ev.event_type,
                                pane_id: ev.pane_id,
                                payload: ev.payload,
                            };
                            if write_response(&mut writer, req.id, body).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("subscriber lagged, dropped {} events", n);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return;
                    }
                }
            }
        }

        let body = handle_request(req.body, start_time, &config, &panes, &event_tx).await;
        if write_response(&mut writer, req.id, body).await.is_err() {
            break;
        }
    }
}

async fn handle_request(
    body: libslop::RequestBody,
    start_time: u64,
    config: &Arc<libslop::SlopdConfig>,
    panes: &PaneMap,
    event_tx: &EventTx,
) -> libslop::ResponseBody {
    match body {
        libslop::RequestBody::Ping => libslop::ResponseBody::Pong,

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
            let output = tmux(config)
                .args(["kill-pane", "-t", &pane_id])
                .output();
            match output {
                Ok(out) if out.status.success() => {
                    panes.remove(&pane_id);
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

            if event == "SessionStart" {
                if let (Some(pane), Some(session_id)) = (
                    pane_id.as_deref(),
                    payload.get("session_id").and_then(|v| v.as_str()),
                ) {
                    debug!("SessionStart: pane={} session_id={}", pane, session_id);
                    let result = tmux(config)
                        .args([
                            "set-option", "-t", pane, "-p",
                            libslop::TmuxOption::SlopdClaudeSessionId.as_str(),
                            session_id,
                        ])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                    if let Err(e) = result {
                        warn!("failed to set @slopd_claude_session_id on pane {}: {}", pane, e);
                    }
                }
            }

            if event == "UserPromptSubmit" {
                if let Some(pane) = pane_id.as_deref() {
                    debug!("UserPromptSubmit: notifying pending senders for pane {}", pane);
                    pane_state(panes, pane).prompt_submitted.notify_waiters();
                }
            }

            let _ = event_tx.send(BroadcastEvent {
                source: "hook".to_string(),
                event_type: event,
                pane_id,
                payload,
            });

            libslop::ResponseBody::Hooked
        }

        libslop::RequestBody::Run => {
            let settings_path = config.claude_settings_path();
            if let Err(e) = libslop::inject_hooks_into_file(&settings_path, &config.run.slopctl) {
                warn!("failed to inject hooks into {}: {}", settings_path.display(), e);
            }
            let xdg_runtime_dir = libslop::runtime_dir();
            let output = tmux(config)
                .args(["new-window", "-t", "slopd", "-P", "-F", "#{pane_id}"])
                .args(["-e", &format!("XDG_RUNTIME_DIR={}", xdg_runtime_dir.display())])
                .args(["-e", &format!("CLAUDE_CONFIG_DIR={}", config.claude_config_dir().display())])
                .arg(config.run.executable.program())
                .args(config.run.executable.args())
                .output();
            match output {
                Ok(out) if out.status.success() => {
                    let pane_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    debug!("spawned {:?} in pane {}", config.run.executable, pane_id);
                    libslop::ResponseBody::Run { pane_id }
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    libslop::ResponseBody::Error { message: stderr }
                }
                Err(e) => libslop::ResponseBody::Error { message: e.to_string() },
            }
        }

        libslop::RequestBody::Send { pane_id, prompt } => {
            let state = pane_state(panes, &pane_id);

            // Acquire the type-mutex so concurrent sends don't interleave keystrokes.
            let _guard = state.type_mutex.lock().await;

            // Subscribe to the notify *before* sending keys so we don't miss a fast delivery.
            let notified = state.prompt_submitted.notified();

            let result = tmux(config)
                .args(["send-keys", "-t", &pane_id, &prompt, "Enter"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();

            // Release the type-mutex before awaiting delivery so other senders can type.
            drop(_guard);

            match result {
                Err(e) => libslop::ResponseBody::Error { message: e.to_string() },
                Ok(out) if !out.success() => {
                    let msg = format!("tmux send-keys failed for pane {}", pane_id);
                    libslop::ResponseBody::Error { message: msg }
                }
                Ok(_) => {
                    // Wait for UserPromptSubmit from this pane to confirm delivery.
                    notified.await;
                    libslop::ResponseBody::Sent { pane_id }
                }
            }
        }

        libslop::RequestBody::Subscribe { .. } => {
            // Handled in handle_connection before reaching here.
            unreachable!("Subscribe should be handled before handle_request")
        }
    }
}
