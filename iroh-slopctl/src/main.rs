use std::collections::HashMap;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use iroh::{Endpoint, PublicKey, SecretKey, endpoint::presets};
use serde::{Deserialize, Serialize};
use tracing::debug;

const ALPN: &[u8] = b"iroh-slopd/0";

#[derive(Parser)]
#[command(name = "iroh-slopctl", about = "Remote control for slopd via iroh", version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT"), ")"))]
struct Cli {
    #[arg(short, long, action = clap::ArgAction::Count, help = "Increase log verbosity")]
    verbose: u8,

    /// Endpoint name (from config) or raw EndpointId to connect to. Overrides the default.
    #[arg(long, global = true)]
    endpoint: Option<String>,

    /// Read the server's full EndpointAddr from this JSON file (for direct connections without discovery).
    #[arg(long, global = true, value_name = "PATH")]
    addr_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print this client's EndpointId (for server authorization).
    Info,
    /// Show slopd uptime and state.
    Status,
    /// List panes in the slopd session.
    Ps {
        #[arg(long = "filter", value_name = "KEY=VALUE")]
        filters: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Open a new Claude pane in the slopd tmux session.
    Run {
        #[arg(short = 'c', long, value_name = "DIR")]
        start_directory: Option<PathBuf>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        extra_args: Vec<String>,
    },
    /// Terminate a Claude pane.
    Kill {
        pane_id: String,
    },
    /// Type a prompt into pane(s) and wait for UserPromptSubmit confirmation.
    Send {
        pane_id: String,
        prompt: String,
        #[arg(long = "filter", value_name = "KEY=VALUE")]
        filters: Vec<String>,
        #[arg(long, default_value = "one")]
        select: SelectMode,
        #[arg(long, default_value = "60")]
        timeout: u64,
        #[arg(long, short = 'i')]
        interrupt: bool,
    },
    /// Send Ctrl+C, Ctrl+D, and Escape to interrupt a running agent.
    Interrupt {
        pane_id: String,
    },
    /// Subscribe to a stream of events and print each as a JSON line.
    Listen {
        #[arg(long = "hook", value_name = "EVENT")]
        hooks: Vec<String>,
        #[arg(long = "event", value_name = "EVENT")]
        events: Vec<String>,
        #[arg(long = "transcript", value_name = "TYPE")]
        transcripts: Vec<String>,
        #[arg(long, value_name = "PANE_ID")]
        pane_id: Option<String>,
        #[arg(long, value_name = "SESSION_ID")]
        session_id: Option<String>,
        #[arg(long, value_name = "N")]
        replay: Option<u64>,
    },
    /// Read historical transcript records from a pane.
    Transcript {
        pane_id: String,
        #[arg(long)]
        before: Option<u64>,
        #[arg(long, default_value = "50")]
        limit: u64,
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
        pane_id: Option<String>,
    },
}

#[derive(Clone, clap::ValueEnum)]
enum SelectMode {
    One,
    Any,
    All,
}

impl From<&SelectMode> for libslopctl::SelectMode {
    fn from(s: &SelectMode) -> Self {
        match s {
            SelectMode::One => libslopctl::SelectMode::One,
            SelectMode::Any => libslopctl::SelectMode::Any,
            SelectMode::All => libslopctl::SelectMode::All,
        }
    }
}

fn config_path() -> PathBuf {
    libslop::config_dir().join("iroh-slopctl/config.toml")
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Config {
    secret_key: Option<String>,
    default: Option<String>,
    #[serde(default)]
    endpoints: HashMap<String, EndpointConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
struct EndpointConfig {
    endpoint_id: String,
}

impl Config {
    fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => toml::from_str(&contents).unwrap_or_else(|e| {
                eprintln!("warning: failed to parse {}: {}", path.display(), e);
                Config::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => {
                eprintln!("warning: failed to read {}: {}", path.display(), e);
                Config::default()
            }
        }
    }

    fn save(&self) {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap_or_else(|e| {
                eprintln!("failed to create config dir: {}", e);
                std::process::exit(1);
            });
        }
        let contents = toml::to_string_pretty(self).unwrap();
        std::fs::write(&path, contents).unwrap_or_else(|e| {
            eprintln!("failed to write config: {}", e);
            std::process::exit(1);
        });
    }

    fn secret_key(&mut self) -> SecretKey {
        if let Some(ref key_str) = self.secret_key {
            let bytes = data_encoding::BASE32_NOPAD.decode(key_str.as_bytes()).unwrap_or_else(|e| {
                eprintln!("invalid secret_key in config (bad base32): {}", e);
                std::process::exit(1);
            });
            let bytes: [u8; 32] = bytes.try_into().unwrap_or_else(|_| {
                eprintln!("invalid secret_key in config: expected 32 bytes");
                std::process::exit(1);
            });
            SecretKey::from(bytes)
        } else {
            let mut bytes = [0u8; 32];
            getrandom::fill(&mut bytes).expect("failed to generate random key");
            let key = SecretKey::from(bytes);
            self.secret_key = Some(data_encoding::BASE32_NOPAD.encode(&key.to_bytes()));
            self.save();
            key
        }
    }

    fn resolve_endpoint(&self, override_endpoint: Option<&str>) -> iroh::EndpointAddr {
        let endpoint_str = if let Some(name_or_id) = override_endpoint {
            if let Some(ep) = self.endpoints.get(name_or_id) {
                ep.endpoint_id.clone()
            } else {
                name_or_id.to_string()
            }
        } else if let Some(ref default_name) = self.default {
            if let Some(ep) = self.endpoints.get(default_name) {
                ep.endpoint_id.clone()
            } else {
                eprintln!("default endpoint {:?} not found in config", default_name);
                std::process::exit(1);
            }
        } else {
            eprintln!("no endpoint specified and no default configured");
            eprintln!("use --endpoint <name-or-id> or set 'default' in config");
            std::process::exit(1);
        };

        let id = endpoint_str.parse::<PublicKey>().unwrap_or_else(|e| {
            eprintln!("invalid endpoint_id {:?}: {}", endpoint_str, e);
            std::process::exit(1);
        });
        iroh::EndpointAddr::from(id)
    }
}

fn die(msg: &str) -> ! {
    eprintln!("error: {}", msg);
    std::process::exit(1);
}

fn die_err(e: libslopctl::Error) -> ! {
    die(&e.to_string());
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

    let mut config = Config::load();

    if let Command::Info = cli.command {
        let secret_key = config.secret_key();
        println!("{}", secret_key.public());
        return;
    }

    // Validate filters eagerly.
    if let Command::Ps { ref filters, .. } = cli.command {
        libslopctl::parse_filters(filters.clone()).unwrap_or_else(|e| die_err(e));
    }
    if let Command::Send { ref pane_id, ref filters, .. } = cli.command {
        if pane_id.contains('=') {
            let mut all = vec![pane_id.clone()];
            all.extend(filters.clone());
            libslopctl::parse_filters(all).unwrap_or_else(|e| die_err(e));
        } else {
            libslopctl::parse_filters(filters.clone()).unwrap_or_else(|e| die_err(e));
        }
    }
    if let Command::Tags { pane_id: None } = cli.command {
        eprintln!("error: <PANE_ID> is required for iroh-slopctl (no $TMUX_PANE available)");
        std::process::exit(2);
    }

    let secret_key = config.secret_key();

    let addr = if let Some(ref addr_file) = cli.addr_file {
        let contents = std::fs::read_to_string(addr_file).unwrap_or_else(|e| {
            eprintln!("failed to read addr file {}: {}", addr_file.display(), e);
            std::process::exit(1);
        });
        serde_json::from_str::<iroh::EndpointAddr>(&contents).unwrap_or_else(|e| {
            eprintln!("failed to parse addr file: {}", e);
            std::process::exit(1);
        })
    } else {
        config.resolve_endpoint(cli.endpoint.as_deref())
    };

    debug!("connecting to endpoint {:?}", addr);

    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret_key)
        .bind()
        .await
        .unwrap_or_else(|e| {
            eprintln!("failed to bind iroh endpoint: {}", e);
            std::process::exit(1);
        });

    let connection = endpoint.connect(addr, ALPN).await.unwrap_or_else(|e| {
        eprintln!("failed to connect to remote endpoint: {}", e);
        std::process::exit(1);
    });

    let (send, recv) = connection.open_bi().await.unwrap_or_else(|e| {
        eprintln!("failed to open stream: {}", e);
        std::process::exit(1);
    });

    let mut client = libslopctl::Client::new(recv, send);

    if let Command::Listen { hooks, events, transcripts, pane_id, session_id, replay } = cli.command {
        let mut subscription = if let Some(last_n) = replay {
            let replay_pane_id = match pane_id {
                Some(ref id) => id.clone(),
                None => {
                    eprintln!("error: --replay requires --pane-id");
                    std::process::exit(2);
                }
            };
            client.subscribe_transcript(replay_pane_id, last_n).await.unwrap_or_else(|e| die_err(e))
        } else {
            let filters: Vec<libslop::EventFilter> = if hooks.is_empty() && events.is_empty() && transcripts.is_empty() && pane_id.is_none() && session_id.is_none() {
                vec![]
            } else if hooks.is_empty() && events.is_empty() && transcripts.is_empty() {
                vec![libslop::EventFilter {
                    source: None,
                    event_type: None,
                    pane_id,
                    session_id,
                    payload_match: serde_json::Map::new(),
                }]
            } else {
                let hook_filters = hooks.into_iter().map(|h| libslop::EventFilter {
                    source: Some("hook".to_string()),
                    event_type: Some(h),
                    pane_id: pane_id.clone(),
                    session_id: session_id.clone(),
                    payload_match: serde_json::Map::new(),
                });
                let event_filters = events.into_iter().map(|e| libslop::EventFilter {
                    source: Some("slopd".to_string()),
                    event_type: Some(e),
                    pane_id: pane_id.clone(),
                    session_id: None,
                    payload_match: serde_json::Map::new(),
                });
                let transcript_filters = transcripts.into_iter().map(|t| libslop::EventFilter {
                    source: Some("transcript".to_string()),
                    event_type: Some(t),
                    pane_id: pane_id.clone(),
                    session_id: session_id.clone(),
                    payload_match: serde_json::Map::new(),
                });
                hook_filters.chain(event_filters).chain(transcript_filters).collect()
            };
            client.subscribe(filters).await.unwrap_or_else(|e| die_err(e))
        };

        println!("{{\"subscribed\":true}}");

        let mut sigterm = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        ).expect("failed to install SIGTERM handler");

        loop {
            tokio::select! {
                _ = sigterm.recv() => break,
                result = subscription.next() => {
                    match result {
                        Ok(Some(libslopctl::SubscriptionItem::Record(record))) => {
                            println!("{}", serde_json::to_string(&record).unwrap());
                        }
                        Ok(Some(libslopctl::SubscriptionItem::Subscribed)) => {}
                        Ok(None) => break,
                        Err(e) => die_err(e),
                    }
                }
            }
        }

        endpoint.close().await;
        return;
    }

    // Client-side filter resolution for Send with filter target.
    if let Command::Send { ref pane_id, .. } = cli.command {
        if pane_id.contains('=') {
            if let Command::Send { pane_id, prompt, filters, select, timeout, interrupt } = cli.command {
                let mut all_filters = vec![pane_id];
                all_filters.extend(filters);
                let parsed = libslopctl::parse_filters(all_filters).unwrap_or_else(|e| die_err(e));

                let pane_ids = client.send_filtered(
                    &parsed, &prompt, &(&select).into(), timeout, interrupt,
                ).await.unwrap_or_else(|e| die_err(e));

                for pane_id in pane_ids {
                    println!("{}", pane_id);
                }

                endpoint.close().await;
                return;
            }
        }
    }

    match cli.command {
        Command::Status => {
            let state = client.status().await.unwrap_or_else(|e| die_err(e));
            println!("uptime: {}s", state.uptime_secs);
        }
        Command::Ps { filters, json } => {
            let parsed = libslopctl::parse_filters(filters).unwrap_or_else(|e| die_err(e));
            let all_panes = client.ps().await.unwrap_or_else(|e| die_err(e));
            let panes = libslopctl::apply_filters(all_panes, &parsed);
            if json {
                println!("{}", serde_json::to_string(&panes).unwrap());
            } else {
                print_ps(panes);
            }
        }
        Command::Run { extra_args, start_directory } => {
            let pane_id = client.run(None, extra_args, start_directory)
                .await.unwrap_or_else(|e| die_err(e));
            println!("{}", pane_id);
        }
        Command::Kill { pane_id } => {
            let pane_id = client.kill(pane_id).await.unwrap_or_else(|e| die_err(e));
            println!("{}", pane_id);
        }
        Command::Send { pane_id, prompt, timeout, interrupt, .. } => {
            let pane_id = client.send_prompt(pane_id, prompt, timeout, interrupt)
                .await.unwrap_or_else(|e| die_err(e));
            println!("{}", pane_id);
        }
        Command::Interrupt { pane_id } => {
            let pane_id = client.interrupt(pane_id).await.unwrap_or_else(|e| die_err(e));
            println!("{}", pane_id);
        }
        Command::Tag { pane_id, tag } => {
            let (pane_id, tag) = client.tag(pane_id, tag).await.unwrap_or_else(|e| die_err(e));
            println!("{} {}", pane_id, tag);
        }
        Command::Untag { pane_id, tag } => {
            let (pane_id, tag) = client.untag(pane_id, tag).await.unwrap_or_else(|e| die_err(e));
            println!("{} {}", pane_id, tag);
        }
        Command::Tags { pane_id } => {
            let pane_id = pane_id.unwrap();
            let tags = client.tags(pane_id).await.unwrap_or_else(|e| die_err(e));
            for tag in tags {
                println!("{}", tag);
            }
        }
        Command::Transcript { pane_id, before, limit } => {
            let records = client.read_transcript(pane_id, before, limit)
                .await.unwrap_or_else(|e| die_err(e));
            let out = serde_json::json!({ "records": records });
            println!("{}", out);
        }
        Command::Info | Command::Listen { .. } => unreachable!(),
    }

    endpoint.close().await;
}

fn print_ps(panes: Vec<libslop::PaneInfo>) {
    let now = std::time::SystemTime::now();
    let fmt = timeago::Formatter::new();
    let rows: Vec<(String, String, String, String, String, String, String, String, String)> = panes.iter().map(|p| {
        let epoch = now.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let created = fmt.convert(epoch.saturating_sub(std::time::Duration::from_secs(p.created_at)));
        let last_active = fmt.convert(epoch.saturating_sub(std::time::Duration::from_secs(p.last_active)));
        let session = p.session_id.as_deref().unwrap_or("-").to_string();
        let parent = p.parent_pane_id.as_deref().unwrap_or("-").to_string();
        let tags = if p.tags.is_empty() { "-".to_string() } else { p.tags.join(",") };
        let state = p.state.as_str().to_string();
        let detailed_state = p.detailed_state.as_str().to_string();
        let working_dir = p.working_dir.as_deref().unwrap_or("-").to_string();
        (p.pane_id.clone(), created, last_active, session, parent, tags, state, detailed_state, working_dir)
    }).collect();

    let pane_w          = rows.iter().map(|r| r.0.len()).max().unwrap_or(0).max(4);
    let created_w       = rows.iter().map(|r| r.1.len()).max().unwrap_or(0).max(7);
    let last_active_w   = rows.iter().map(|r| r.2.len()).max().unwrap_or(0).max(11);
    let session_w       = rows.iter().map(|r| r.3.len()).max().unwrap_or(0).max(7);
    let parent_w        = rows.iter().map(|r| r.4.len()).max().unwrap_or(0).max(6);
    let tags_w          = rows.iter().map(|r| r.5.len()).max().unwrap_or(0).max(4);
    let state_w         = rows.iter().map(|r| r.6.len()).max().unwrap_or(0).max(5);
    let detailed_w      = rows.iter().map(|r| r.7.len()).max().unwrap_or(0).max(14);
    let working_dir_w   = rows.iter().map(|r| r.8.len()).max().unwrap_or(0).max(11);

    println!("{:<pane_w$}  {:<created_w$}  {:<last_active_w$}  {:<session_w$}  {:<parent_w$}  {:<tags_w$}  {:<state_w$}  {:<detailed_w$}  {:<working_dir_w$}",
        "PANE", "CREATED", "LAST_ACTIVE", "SESSION", "PARENT", "TAGS", "STATE", "DETAILED_STATE", "WORKING_DIR",
        pane_w=pane_w, created_w=created_w, last_active_w=last_active_w, session_w=session_w,
        parent_w=parent_w, tags_w=tags_w, state_w=state_w, detailed_w=detailed_w, working_dir_w=working_dir_w);

    for (pane_id, created, last_active, session, parent, tags, state, detailed_state, working_dir) in &rows {
        println!("{:<pane_w$}  {:<created_w$}  {:<last_active_w$}  {:<session_w$}  {:<parent_w$}  {:<tags_w$}  {:<state_w$}  {:<detailed_w$}  {:<working_dir_w$}",
            pane_id, created, last_active, session, parent, tags, state, detailed_state, working_dir,
            pane_w=pane_w, created_w=created_w, last_active_w=last_active_w, session_w=session_w,
            parent_w=parent_w, tags_w=tags_w, state_w=state_w, detailed_w=detailed_w, working_dir_w=working_dir_w);
    }
}
