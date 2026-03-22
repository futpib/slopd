use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::debug;

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

    let _config = libslop::SlopctlConfig::load();

    let command = args.iter().find(|a| !a.starts_with('-') && *a != &args[0]).map(String::as_str).unwrap_or("ping");

    let socket_path = libslop::socket_path();
    debug!("connecting to {}", socket_path.display());

    let stream = UnixStream::connect(&socket_path).await.unwrap_or_else(|e| {
        eprintln!("Failed to connect to {}: {}", socket_path.display(), e);
        std::process::exit(1);
    });

    let (reader, mut writer) = stream.into_split();

    let body = match command {
        "status" => libslop::RequestBody::Status,
        "ping" => libslop::RequestBody::Ping,
        "run" => libslop::RequestBody::Run,
        "hook" => {
            let event = args.get(2).cloned().unwrap_or_else(|| {
                eprintln!("Usage: slopctl hook <EventName>");
                std::process::exit(1);
            });
            let mut stdin = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut stdin).unwrap();
            let payload: serde_json::Value = serde_json::from_str(&stdin).unwrap_or_else(|e| {
                eprintln!("Failed to parse hook payload: {}", e);
                std::process::exit(1);
            });
            let pane_id = std::env::var("TMUX_PANE").ok();
            libslop::RequestBody::Hook { event, payload, pane_id }
        }
        "kill" => {
            let pane_id = args.get(2).cloned().unwrap_or_else(|| {
                eprintln!("Usage: slopctl kill <pane_id>");
                std::process::exit(1);
            });
            libslop::RequestBody::Kill { pane_id }
        }
        other => {
            eprintln!("Unknown command: {}", other);
            std::process::exit(1);
        }
    };

    let request = libslop::Request { id: 1, body };
    let mut json = serde_json::to_string(&request).unwrap();
    debug!("sending: {}", json);
    json.push('\n');
    writer.write_all(json.as_bytes()).await.unwrap();

    let mut lines = BufReader::new(reader).lines();
    if let Ok(Some(line)) = lines.next_line().await {
        debug!("received: {}", line);
        let response: libslop::Response = serde_json::from_str(&line).unwrap();
        match response.body {
            libslop::ResponseBody::Run { pane_id } => println!("{}", pane_id),
            libslop::ResponseBody::Kill { pane_id } => println!("{}", pane_id),
            other => println!("{:?}", other),
        }
    }
}
