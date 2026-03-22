use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
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

    loop {
        let (stream, _addr) = listener.accept().await.unwrap();
        debug!("accepted connection");
        tokio::spawn(handle_connection(stream, start_time, config.clone()));
    }
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    start_time: u64,
    config: Arc<libslop::SlopdConfig>,
) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        debug!("received: {}", line);
        let (id, body) = match serde_json::from_str::<libslop::Request>(&line) {
            Ok(req) => {
                let body = match req.body {
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
                        let output = tmux(&config)
                            .args(["kill-pane", "-t", &pane_id])
                            .output();
                        match output {
                            Ok(out) if out.status.success() => {
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
                                let result = tmux(&config)
                                    .args([
                                        "set-option",
                                        "-t",
                                        pane,
                                        "-p",
                                        libslop::TmuxOption::SlopdClaudeSessionId.as_str(),
                                        session_id,
                                    ])
                                    .stdout(std::process::Stdio::null())
                                    .stderr(std::process::Stdio::null())
                                    .status();
                                if let Err(e) = result {
                                    warn!("failed to set @claude_session_id on pane {}: {}", pane, e);
                                }
                            }
                        }
                        libslop::ResponseBody::Hooked
                    }
                    libslop::RequestBody::Run => {
                        let settings_path = config.claude_settings_path();
                        if let Err(e) = libslop::inject_hooks_into_file(
                            &settings_path,
                            &config.run.slopctl,
                        ) {
                            warn!("failed to inject hooks into {}: {}", settings_path.display(), e);
                        }
                        let xdg_runtime_dir = libslop::runtime_dir();
                        let output = tmux(&config)
                            .args(["new-window", "-t", "slopd", "-P", "-F", "#{pane_id}"])
                            .args(["-e", &format!("XDG_RUNTIME_DIR={}", xdg_runtime_dir.display())])
                            .args(["-e", &format!("SLOPCTL={}", config.run.slopctl)])
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
                };
                (req.id, body)
            }
            Err(e) => {
                warn!("failed to parse request: {}", e);
                (0, libslop::ResponseBody::Error { message: e.to_string() })
            }
        };

        let response = libslop::Response { id, body };
        let mut json = serde_json::to_string(&response).unwrap();
        json.push('\n');
        debug!("sending: {}", json.trim());
        if writer.write_all(json.as_bytes()).await.is_err() {
            break;
        }
    }
}
