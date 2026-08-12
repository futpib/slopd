use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tracing::debug;

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

    /// Read configuration from this file instead of the default
    /// `$XDG_CONFIG_HOME/iroh-slopctl/config.toml`. Supports `~` and `$VAR` expansion.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print this client's EndpointId (for server authorization).
    Info,
    #[command(flatten)]
    Common(libslopctl::CommonCommand),
}

fn die_iroh(error: libslopiroh::Error) -> ! {
    if let Some(client_id) = error.unauthorized_client() {
        eprintln!("hint: the remote endpoint rejected this client as unauthorized");
        eprintln!(
            "hint: ask the remote to run: iroh-slopd authorize {}",
            client_id
        );
    }
    eprintln!("{error}");
    std::process::exit(1);
}

fn unwrap_iroh<T>(result: Result<T, libslopiroh::Error>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => die_iroh(error),
    }
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

    let config_path = cli
        .config
        .as_deref()
        .map(libslop::expand_path)
        .unwrap_or_else(libslopiroh::default_client_config_path);
    let mut config = libslopiroh::ClientConfig::load(config_path);

    if let Command::Info = cli.command {
        let secret_key = unwrap_iroh(config.secret_key());
        println!("{}", secret_key.public());
        return;
    }

    // Validate filters eagerly.
    if let Command::Common(ref cmd) = cli.command {
        libslopctl::validate_command_filters(cmd).unwrap_or_else(|e| libslopctl::die_err(e));
    }
    if matches!(
        &cli.command,
        Command::Common(
            libslopctl::CommonCommand::Fork { pane_id: None, .. }
                | libslopctl::CommonCommand::Tags { pane_id: None }
        )
    ) {
        eprintln!("error: <PANE_ID> is required for iroh-slopctl (no $TMUX_PANE available)");
        std::process::exit(2);
    }

    let secret_key = unwrap_iroh(config.secret_key());
    let client_id = secret_key.public();

    let addr = if let Some(ref addr_file) = cli.addr_file {
        unwrap_iroh(libslopiroh::read_addr_file(addr_file))
    } else {
        unwrap_iroh(config.resolve_endpoint(cli.endpoint.as_deref()))
    };

    debug!("connecting to endpoint {:?}", addr);

    let connector = unwrap_iroh(libslopiroh::Connector::bind(secret_key, addr).await);
    let stream = unwrap_iroh(connector.open().await);
    let connection = stream.connection.clone();

    let mut client = libslopctl::Client::new(stream.recv, stream.send);

    if let Command::Common(cmd) = cli.command {
        let ctx = libslopctl::CommandContext {
            parent_pane_id: None,
            fallback_pane_id: None,
            // Interactive run attaches to a local tmux; meaningless for a remote
            // endpoint, so it's unsupported here (errors if --interactive is used).
            interactive: None,
            // Remote endpoint: the client's cwd is meaningless on the server, so
            // a relative --start-directory is rejected rather than misinterpreted.
            local: false,
        };
        if let Err(e) = libslopctl::execute_command(&mut client, cmd, &ctx).await {
            if libslopiroh::is_unauthorized(connection.close_reason().as_ref()) {
                eprintln!("hint: the remote endpoint rejected this client as unauthorized");
                eprintln!(
                    "hint: ask the remote to run: iroh-slopd authorize {}",
                    client_id
                );
            }
            libslopctl::die_err(e);
        }
    }

    connector.close().await;
}
