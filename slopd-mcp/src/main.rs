use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tokio::net::TcpListener;

use slopd_mcp::{ServeConfig, default_allowed_hosts, is_loopback, serve};

#[derive(Parser)]
#[command(
    name = "slopd-mcp",
    about = "Expose slopd-managed panes as a Streamable HTTP MCP supervisor",
    version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT"), ")")
)]
struct Cli {
    #[arg(
        short,
        long,
        action = clap::ArgAction::Count,
        help = "Increase stderr log verbosity (-v INFO, -vv DEBUG, -vvv TRACE)"
    )]
    verbose: u8,

    /// Connect to this local slopd socket.
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,

    /// Listen address for the MCP HTTP server.
    #[arg(long, default_value = "127.0.0.1:8780", value_name = "ADDR")]
    bind: SocketAddr,

    /// HTTP path of the MCP endpoint.
    #[arg(long, default_value = "/mcp", value_name = "PATH")]
    path: String,

    /// Bearer token required on Authorization. Also read from SLOPD_MCP_TOKEN.
    #[arg(long, env = "SLOPD_MCP_TOKEN", value_name = "TOKEN")]
    token: Option<String>,

    /// Read the bearer token from this file (first line).
    #[arg(long, value_name = "PATH")]
    token_file: Option<PathBuf>,

    /// Extra Host header values to accept. Always includes loopback and the bind address.
    #[arg(long = "allowed-host", value_name = "HOST")]
    allowed_hosts: Vec<String>,

    /// Advertise and implement the run tool, which can spawn new panes.
    #[arg(long)]
    allow_run: bool,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    if let Err(error) = run(cli).await {
        eprintln!("slopd-mcp: {error}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    if cli.token.is_some() && cli.token_file.is_some() {
        return Err("use either --token or --token-file, not both".into());
    }
    let token = match (cli.token, cli.token_file) {
        (Some(token), None) => Some(token),
        (None, Some(path)) => Some(read_token_file(&path)?),
        (None, None) => None,
        (Some(_), Some(_)) => unreachable!(),
    };
    let token = token.filter(|value| !value.is_empty());
    if !is_loopback(cli.bind) && token.is_none() {
        return Err(
            "a bearer token is required when binding a non-loopback address; pass --token, --token-file, or SLOPD_MCP_TOKEN"
                .into(),
        );
    }

    let socket = cli
        .socket
        .as_deref()
        .map(libslop::expand_path)
        .unwrap_or_else(libslop::socket_path);

    let listener = TcpListener::bind(cli.bind)
        .await
        .map_err(|error| format!("failed to bind {}: {error}", cli.bind))?;
    let bound = listener
        .local_addr()
        .map_err(|error| format!("failed to read bind address: {error}"))?;

    let mut allowed_hosts = default_allowed_hosts(bound);
    allowed_hosts.extend(cli.allowed_hosts);
    allowed_hosts.sort();
    allowed_hosts.dedup();

    let path = if cli.path.starts_with('/') {
        cli.path
    } else {
        format!("/{}", cli.path)
    };
    tracing::info!(
        "listening on http://{bound}{path} (slopd socket {})",
        socket.display()
    );

    serve(
        listener,
        ServeConfig {
            socket,
            allow_run: cli.allow_run,
            token: token.map(Arc::from),
            allowed_hosts,
            path,
        },
    )
    .await
    .map_err(|error| error.to_string())
}

fn read_token_file(path: &std::path::Path) -> Result<String, String> {
    let path = libslop::expand_path(path);
    let contents = std::fs::read_to_string(&path)
        .map_err(|error| format!("failed to read token file {}: {error}", path.display()))?;
    let token = contents.lines().next().unwrap_or("").trim().to_string();
    if token.is_empty() {
        return Err(format!("token file {} is empty", path.display()));
    }
    Ok(token)
}

fn init_logging(verbose: u8) {
    let fallback = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(fallback)),
        )
        .with_writer(std::io::stderr)
        .init();
}
