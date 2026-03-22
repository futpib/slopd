use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

#[tokio::main]
async fn main() {
    let tmux_status = std::process::Command::new("tmux")
        .arg("list-sessions")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("failed to run tmux");
    if !tmux_status.success() {
        eprintln!("slopd: tmux is not running");
        std::process::exit(1);
    }

    let socket_path = slop_proto::socket_path();
    let socket_dir = socket_path.parent().unwrap();

    tokio::fs::create_dir_all(&socket_dir).await.unwrap();
    let _ = tokio::fs::remove_file(&socket_path).await;

    let listener = UnixListener::bind(&socket_path).unwrap();
    eprintln!("slopd listening on {}", socket_path.display());

    let start_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    loop {
        let (stream, _addr) = listener.accept().await.unwrap();
        tokio::spawn(handle_connection(stream, start_time));
    }
}

async fn handle_connection(stream: tokio::net::UnixStream, start_time: u64) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Ok(Some(line)) = lines.next_line().await {
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
            Err(e) => (0, slop_proto::ResponseBody::Error { message: e.to_string() }),
        };

        let response = slop_proto::Response { id, body };
        let mut json = serde_json::to_string(&response).unwrap();
        json.push('\n');
        if writer.write_all(json.as_bytes()).await.is_err() {
            break;
        }
    }
}
