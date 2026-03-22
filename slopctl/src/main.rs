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
    /// List panes in the slopd session.
    Ps {
        /// Filter by key=value (repeatable, AND semantics). Supported keys: tag.
        #[arg(long = "filter", value_name = "KEY=VALUE")]
        filters: Vec<String>,
    },
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
    /// Add a tag to a pane.
    Tag {
        pane_id: String,
        tag: String,
    },
    /// Remove a tag from a pane.
    Untag {
        pane_id: String,
        tag: String,
    },
    /// List all tags on a pane.
    Tags {
        pane_id: String,
    },
    /// Send a prompt to panes selected by filters.
    SendFiltered {
        prompt: String,
        /// Filter by key=value (repeatable, AND semantics). Supported keys: tag.
        #[arg(long = "filter", value_name = "KEY=VALUE")]
        filters: Vec<String>,
        /// How to select among matching panes: one (default), any, all.
        #[arg(long, default_value = "one")]
        select: SelectMode,
        /// Seconds to wait for UserPromptSubmit confirmation per pane (default: 60).
        #[arg(long, default_value = "60")]
        timeout: u64,
    },
}

#[derive(Clone, clap::ValueEnum)]
enum SelectMode {
    /// Require exactly one matching pane; error otherwise.
    One,
    /// Pick one at random from matches; error if none.
    Any,
    /// Send to all matching panes; error if none.
    All,
}

fn verbosity_to_level(verbosity: u8) -> tracing::Level {
    match verbosity {
        0 => tracing::Level::WARN,
        1 => tracing::Level::INFO,
        2 => tracing::Level::DEBUG,
        _ => tracing::Level::TRACE,
    }
}

/// Parse "key=value" filter strings and exit on malformed input.
fn parse_filters(raw: Vec<String>) -> Vec<(String, String)> {
    raw.into_iter().map(|f| {
        match f.split_once('=') {
            Some((k, v)) => {
                if k != "tag" {
                    eprintln!("unknown filter key {:?}: only 'tag' is supported", k);
                    std::process::exit(1);
                }
                (k.to_string(), v.to_string())
            }
            None => {
                eprintln!("invalid filter {:?}: expected key=value", f);
                std::process::exit(1);
            }
        }
    }).collect()
}

/// Apply parsed filters to a pane list. AND semantics: pane must satisfy all filters.
fn apply_filters(panes: Vec<libslop::PaneInfo>, filters: &[(String, String)]) -> Vec<libslop::PaneInfo> {
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

async fn send_request(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    id: u64,
    body: libslop::RequestBody,
) -> libslop::ResponseBody {
    let request = libslop::Request { id, body };
    let mut json = serde_json::to_string(&request).unwrap();
    debug!("sending: {}", json);
    json.push('\n');
    writer.write_all(json.as_bytes()).await.unwrap_or_else(|e| {
        eprintln!("failed to send request: {}", e);
        std::process::exit(1);
    });
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                debug!("received: {}", line);
                let response: libslop::Response = serde_json::from_str(&line).unwrap_or_else(|e| {
                    eprintln!("failed to parse response: {}", e);
                    std::process::exit(1);
                });
                if response.id == id {
                    return response.body;
                }
            }
            _ => {
                eprintln!("connection closed unexpectedly");
                std::process::exit(1);
            }
        }
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

    // Client-side filter resolution for SendFiltered: query Ps first, then Send per pane.
    if let Command::SendFiltered { prompt, filters, select, timeout } = cli.command {
        let parsed = parse_filters(filters);
        let filter_desc = parsed.iter().map(|(k, v)| format!("{}={}", k, v)).collect::<Vec<_>>().join(", ");

        let all_panes = match send_request(&mut writer, &mut lines, 1, libslop::RequestBody::Ps).await {
            libslop::ResponseBody::Ps { panes } => panes,
            libslop::ResponseBody::Error { message } => {
                eprintln!("error: {}", message);
                std::process::exit(1);
            }
            other => {
                eprintln!("unexpected response: {:?}", other);
                std::process::exit(1);
            }
        };

        let matched = apply_filters(all_panes, &parsed);

        let target_pane_ids: Vec<String> = match select {
            SelectMode::One => {
                if matched.len() != 1 {
                    eprintln!(
                        "error: expected exactly one pane matching {}, found {}",
                        filter_desc, matched.len()
                    );
                    std::process::exit(1);
                }
                vec![matched.into_iter().next().unwrap().pane_id]
            }
            SelectMode::Any => {
                if matched.is_empty() {
                    eprintln!("error: no panes match filter {}", filter_desc);
                    std::process::exit(1);
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
                    eprintln!("error: no panes match filter {}", filter_desc);
                    std::process::exit(1);
                }
                matched.into_iter().map(|p| p.pane_id).collect()
            }
        };

        // Send all requests on the same connection, each with a unique ID,
        // then read responses correlating by ID.
        let mut id: u64 = 2; // 1 was used by Ps above
        let mut pending: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
        for pane_id in &target_pane_ids {
            let body = libslop::RequestBody::Send {
                pane_id: pane_id.clone(),
                prompt: prompt.clone(),
                timeout_secs: timeout,
            };
            let request = libslop::Request { id, body };
            let mut json = serde_json::to_string(&request).unwrap();
            debug!("sending: {}", json);
            json.push('\n');
            writer.write_all(json.as_bytes()).await.unwrap_or_else(|e| {
                eprintln!("failed to send request: {}", e);
                std::process::exit(1);
            });
            pending.insert(id, pane_id.clone());
            id += 1;
        }
        // Collect all responses; order may differ from send order.
        let mut results: std::collections::HashMap<u64, libslop::ResponseBody> = std::collections::HashMap::new();
        while results.len() < pending.len() {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    debug!("received: {}", line);
                    let response: libslop::Response = serde_json::from_str(&line).unwrap_or_else(|e| {
                        eprintln!("failed to parse response: {}", e);
                        std::process::exit(1);
                    });
                    if pending.contains_key(&response.id) {
                        results.insert(response.id, response.body);
                    }
                }
                _ => {
                    eprintln!("connection closed unexpectedly");
                    std::process::exit(1);
                }
            }
        }
        // Print results in send order.
        for req_id in 2..id {
            let pane_id = &pending[&req_id];
            match &results[&req_id] {
                libslop::ResponseBody::Sent { pane_id } => println!("{}", pane_id),
                libslop::ResponseBody::Error { message } => {
                    eprintln!("error sending to {}: {}", pane_id, message);
                    std::process::exit(1);
                }
                other => {
                    eprintln!("unexpected response for {}: {:?}", pane_id, other);
                    std::process::exit(1);
                }
            }
        }
        return;
    }

    let body = match cli.command {
        Command::Ping => libslop::RequestBody::Ping,
        Command::Status => libslop::RequestBody::Status,
        Command::Ps { filters } => {
            // Ps with filters: fetch all, filter client-side, print.
            let parsed = parse_filters(filters);
            let all_panes = match send_request(&mut writer, &mut lines, 1, libslop::RequestBody::Ps).await {
                libslop::ResponseBody::Ps { panes } => panes,
                libslop::ResponseBody::Error { message } => {
                    eprintln!("error: {}", message);
                    std::process::exit(1);
                }
                other => {
                    eprintln!("unexpected response: {:?}", other);
                    std::process::exit(1);
                }
            };
            let panes = apply_filters(all_panes, &parsed);
            print_ps(panes);
            return;
        }
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
        Command::Tag { pane_id, tag } => libslop::RequestBody::Tag { pane_id, tag, remove: false },
        Command::Untag { pane_id, tag } => libslop::RequestBody::Tag { pane_id, tag, remove: true },
        Command::Tags { pane_id } => libslop::RequestBody::Tags { pane_id },
        Command::Listen { .. } | Command::SendFiltered { .. } => unreachable!(),
    };

    match send_request(&mut writer, &mut lines, 1, body).await {
            libslop::ResponseBody::Ps { panes } => print_ps(panes),
            libslop::ResponseBody::Run { pane_id } => println!("{}", pane_id),
            libslop::ResponseBody::Kill { pane_id } => println!("{}", pane_id),
            libslop::ResponseBody::Sent { pane_id } => println!("{}", pane_id),
            libslop::ResponseBody::Interrupted { pane_id } => println!("{}", pane_id),
            libslop::ResponseBody::Tagged { pane_id, tag } => println!("{} {}", pane_id, tag),
            libslop::ResponseBody::Untagged { pane_id, tag } => println!("{} {}", pane_id, tag),
            libslop::ResponseBody::Tags { pane_id: _, tags } => {
                for tag in tags {
                    println!("{}", tag);
                }
            }
            libslop::ResponseBody::Error { message } => {
                eprintln!("error: {}", message);
                std::process::exit(1);
            }
            other => println!("{:?}", other),
    }
}

fn print_ps(panes: Vec<libslop::PaneInfo>) {
    let now = std::time::SystemTime::now();
    let fmt = timeago::Formatter::new();
    let rows: Vec<(String, String, String, String)> = panes.iter().map(|p| {
        let age = now.duration_since(std::time::UNIX_EPOCH).unwrap_or_default()
            .saturating_sub(std::time::Duration::from_secs(p.created_at));
        let created = fmt.convert(age);
        let session = p.session_id.as_deref().unwrap_or("-").to_string();
        let tags = if p.tags.is_empty() { "-".to_string() } else { p.tags.join(",") };
        (p.pane_id.clone(), created, session, tags)
    }).collect();

    let pane_w = rows.iter().map(|r| r.0.len()).max().unwrap_or(0).max(4);
    let created_w = rows.iter().map(|r| r.1.len()).max().unwrap_or(0).max(7);
    let session_w = rows.iter().map(|r| r.2.len()).max().unwrap_or(0).max(7);
    let tags_w = rows.iter().map(|r| r.3.len()).max().unwrap_or(0).max(4);

    println!("{:<pane_w$}  {:<created_w$}  {:<session_w$}  {:<tags_w$}",
        "PANE", "CREATED", "SESSION", "TAGS",
        pane_w = pane_w, created_w = created_w, session_w = session_w, tags_w = tags_w);

    for (pane_id, created, session, tags) in &rows {
        println!("{:<pane_w$}  {:<created_w$}  {:<session_w$}  {:<tags_w$}",
            pane_id, created, session, tags,
            pane_w = pane_w, created_w = created_w, session_w = session_w, tags_w = tags_w);
    }
}
