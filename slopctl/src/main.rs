use clap::{Parser, Subcommand};
use tokio::net::UnixStream;
use tracing::debug;

#[derive(Parser)]
#[command(name = "slopctl", about = "Control a running slopd daemon", version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT"), ")"))]
struct Cli {
    #[arg(short, long, action = clap::ArgAction::Count, help = "Increase log verbosity (-v INFO, -vv DEBUG, -vvv TRACE)")]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Forward a Claude lifecycle hook event to slopd (called by Claude hooks).
    Hook {
        /// Hook event name (e.g. UserPromptSubmit).
        event: String,
    },
    /// Forward a tmux hook event to slopd (called by tmux hooks).
    TmuxHook {
        /// Tmux hook event name (e.g. pane-exited).
        event: String,
        /// Pane ID from the hook (#{hook_pane}), if available.
        pane_id: Option<String>,
    },
    #[command(flatten)]
    Common(libslopctl::CommonCommand),
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

    // Validate filter arguments eagerly before touching the socket.
    if let Command::Common(ref cmd) = cli.command {
        libslopctl::validate_command_filters(cmd).unwrap_or_else(|e| libslopctl::die_err(e));
    }
    if let Command::Common(libslopctl::CommonCommand::Tags { pane_id: None }) = cli.command {
        if std::env::var("TMUX_PANE").is_err() {
            eprintln!("error: <PANE_ID> is required when $TMUX_PANE is not set");
            std::process::exit(2);
        }
    }

    let _config = libslop::SlopctlConfig::load();

    let socket_path = libslop::socket_path();
    debug!("connecting to {}", socket_path.display());

    // Hook must never exit 2 — that would block the Claude action.
    // Exit 1 on errors (so failures are visible), but never 2.
    if let Command::Hook { event } = cli.command {
        let mut stdin = String::new();
        if let Err(e) = std::io::Read::read_to_string(&mut std::io::stdin(), &mut stdin) {
            eprintln!("slopctl hook: failed to read stdin: {}", e);
            std::process::exit(1);
        }
        let payload: serde_json::Value = match serde_json::from_str(&stdin) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("slopctl hook: failed to parse payload: {}", e);
                std::process::exit(1);
            }
        };
        let pane_id = std::env::var("TMUX_PANE").ok();
        let stream = match UnixStream::connect(&socket_path).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("slopctl hook: failed to connect to {}: {}", socket_path.display(), e);
                std::process::exit(1);
            }
        };
        let (reader, writer) = stream.into_split();
        let mut client = libslopctl::Client::new(reader, writer);
        match client.hook(event, payload, pane_id).await {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!("slopctl hook: {}", e);
                std::process::exit(1);
            }
        }
    }

    // tmux-hook is fire-and-forget like hook; exit 1 on errors, never 2.
    if let Command::TmuxHook { event, pane_id } = cli.command {
        let stream = match UnixStream::connect(&socket_path).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("slopctl tmux-hook: failed to connect to {}: {}", socket_path.display(), e);
                std::process::exit(1);
            }
        };
        let (reader, writer) = stream.into_split();
        let mut client = libslopctl::Client::new(reader, writer);
        match client.tmux_hook(event, pane_id).await {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!("slopctl tmux-hook: {}", e);
                std::process::exit(1);
            }
        }
    }

    let stream = UnixStream::connect(&socket_path).await.unwrap_or_else(|e| {
        eprintln!("Failed to connect to {}: {}", socket_path.display(), e);
        std::process::exit(1);
    });

    let (reader, writer) = stream.into_split();
    let mut client = libslopctl::Client::new(reader, writer);

    if let Command::Common(cmd) = cli.command {
        let ctx = libslopctl::CommandContext {
            parent_pane_id: std::env::var("TMUX_PANE").ok(),
            fallback_pane_id: std::env::var("TMUX_PANE").ok(),
        };
        libslopctl::execute_command(&mut client, cmd, &ctx)
            .await.unwrap_or_else(|e| libslopctl::die_err(e));
    }
}
