use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::debug;

#[derive(Parser)]
#[command(name = "slopctl")]
struct Cli {
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Ping,
    Status,
    Run,
    Kill {
        pane_id: String,
    },
    Hook {
        event: String,
    },
    Send {
        pane_id: String,
        prompt: String,
    },
}

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
    let cli = Cli::parse();

    let level = verbosity_to_level(cli.verbose);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level.as_str())),
        )
        .with_writer(std::io::stderr)
        .init();

    let _config = libslop::SlopctlConfig::load();

    let socket_path = libslop::socket_path();
    debug!("connecting to {}", socket_path.display());

    let stream = UnixStream::connect(&socket_path).await.unwrap_or_else(|e| {
        eprintln!("Failed to connect to {}: {}", socket_path.display(), e);
        std::process::exit(1);
    });

    let (reader, mut writer) = stream.into_split();

    let body = match cli.command {
        Command::Ping => libslop::RequestBody::Ping,
        Command::Status => libslop::RequestBody::Status,
        Command::Run => libslop::RequestBody::Run,
        Command::Kill { pane_id } => libslop::RequestBody::Kill { pane_id },
        Command::Hook { event } => {
            let mut stdin = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut stdin).unwrap();
            let payload: serde_json::Value = serde_json::from_str(&stdin).unwrap_or_else(|e| {
                eprintln!("Failed to parse hook payload: {}", e);
                std::process::exit(1);
            });
            let pane_id = std::env::var("TMUX_PANE").ok();
            libslop::RequestBody::Hook { event, payload, pane_id }
        }
        Command::Send { pane_id, prompt } => libslop::RequestBody::Send { pane_id, prompt },
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
            libslop::ResponseBody::Sent { pane_id } => println!("{}", pane_id),
            libslop::ResponseBody::Error { message } => {
                eprintln!("error: {}", message);
                std::process::exit(1);
            }
            other => println!("{:?}", other),
        }
    }
}
