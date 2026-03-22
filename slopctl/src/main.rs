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
        /// Seconds to wait for UserPromptSubmit confirmation (default: 60).
        #[arg(long, default_value = "60")]
        timeout: u64,
    },
    /// Send Ctrl+C, Ctrl+D, and Escape to interrupt a running agent.
    Interrupt {
        pane_id: String,
    },
    /// Subscribe to a stream of events and print each as a JSON line.
    Listen {
        /// Filter by hook event name (repeatable; omit for all events).
        #[arg(long = "hook", value_name = "EVENT")]
        hooks: Vec<String>,
        /// Only receive events from this tmux pane.
        #[arg(long)]
        pane_id: Option<String>,
        /// Only receive events from this Claude session.
        #[arg(long)]
        session_id: Option<String>,
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
    let mut lines = BufReader::new(reader).lines();

    if let Command::Listen { hooks, pane_id, session_id } = cli.command {
        let filters: Vec<libslop::EventFilter> = if hooks.is_empty() && pane_id.is_none() && session_id.is_none() {
            vec![]
        } else if hooks.is_empty() {
            vec![libslop::EventFilter {
                source: None,
                event_type: None,
                pane_id,
                session_id,
                payload_match: serde_json::Map::new(),
            }]
        } else {
            hooks.into_iter().map(|h| libslop::EventFilter {
                source: Some("hook".to_string()),
                event_type: Some(h),
                pane_id: pane_id.clone(),
                session_id: session_id.clone(),
                payload_match: serde_json::Map::new(),
            }).collect()
        };

        let request = libslop::Request {
            id: 1,
            body: libslop::RequestBody::Subscribe { filters },
        };
        let mut json = serde_json::to_string(&request).unwrap();
        debug!("sending: {}", json);
        json.push('\n');
        writer.write_all(json.as_bytes()).await.unwrap();

        while let Ok(Some(line)) = lines.next_line().await {
            debug!("received: {}", line);
            let response: libslop::Response = serde_json::from_str(&line).unwrap_or_else(|e| {
                eprintln!("failed to parse response: {}", e);
                std::process::exit(1);
            });
            match response.body {
                libslop::ResponseBody::Subscribed => {}
                libslop::ResponseBody::Event { source, event_type, pane_id, payload } => {
                    let out = serde_json::json!({
                        "source": source,
                        "event_type": event_type,
                        "pane_id": pane_id,
                        "payload": payload,
                    });
                    println!("{}", out);
                }
                libslop::ResponseBody::Error { message } => {
                    eprintln!("error: {}", message);
                    std::process::exit(1);
                }
                _ => {}
            }
        }
        return;
    }

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
        Command::Send { pane_id, prompt, timeout } => libslop::RequestBody::Send { pane_id, prompt, timeout_secs: timeout },
        Command::Interrupt { pane_id } => libslop::RequestBody::Interrupt { pane_id },
        Command::Listen { .. } => unreachable!(),
    };

    let request = libslop::Request { id: 1, body };
    let mut json = serde_json::to_string(&request).unwrap();
    debug!("sending: {}", json);
    json.push('\n');
    writer.write_all(json.as_bytes()).await.unwrap();

    if let Ok(Some(line)) = lines.next_line().await {
        debug!("received: {}", line);
        let response: libslop::Response = serde_json::from_str(&line).unwrap();
        match response.body {
            libslop::ResponseBody::Run { pane_id } => println!("{}", pane_id),
            libslop::ResponseBody::Kill { pane_id } => println!("{}", pane_id),
            libslop::ResponseBody::Sent { pane_id } => println!("{}", pane_id),
            libslop::ResponseBody::Interrupted { pane_id } => println!("{}", pane_id),
            libslop::ResponseBody::Error { message } => {
                eprintln!("error: {}", message);
                std::process::exit(1);
            }
            other => println!("{:?}", other),
        }
    }
}
