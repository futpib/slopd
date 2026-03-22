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

    let tmux_status = std::process::Command::new("tmux")
        .arg("list-sessions")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("failed to run tmux");
    if !tmux_status.success() {
        error!("tmux is not running");
        std::process::exit(1);
    }

    let socket_path = slop_proto::socket_path();
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
        tokio::spawn(handle_connection(stream, start_time));
    }
}

async fn handle_connection(stream: tokio::net::UnixStream, start_time: u64) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        debug!("received: {}", line);
        let (id, body) = match serde_json::from_str::<slop_proto::Request>(&line) {
            Ok(req) => {
                let body = match req.body {
                    slop_proto::RequestBody::Ping => slop_proto::ResponseBody::Pong,
                    slop_proto::RequestBody::Status => {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs();
                        slop_proto::ResponseBody::Status {
                            state: slop_proto::DaemonState {
                                uptime_secs: now.saturating_sub(start_time),
                            },
                        }
                    }
                };
                (req.id, body)
            }
            Err(e) => {
                warn!("failed to parse request: {}", e);
                (0, slop_proto::ResponseBody::Error { message: e.to_string() })
            }
        };

        let response = slop_proto::Response { id, body };
        let mut json = serde_json::to_string(&response).unwrap();
        json.push('\n');
        debug!("sending: {}", json.trim());
        if writer.write_all(json.as_bytes()).await.is_err() {
            break;
        }
    }
}
