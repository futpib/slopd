mod adapter;
mod transport;
mod wire;

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, ValueEnum};

use adapter::{Adapter, SystemPromptMode};
use transport::Transport;

const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Backend {
    Claude,
    Opencode,
    Codex,
}

impl From<Backend> for libslop::Backend {
    fn from(backend: Backend) -> Self {
        match backend {
            Backend::Claude => libslop::Backend::Claude,
            Backend::Opencode => libslop::Backend::Opencode,
            Backend::Codex => libslop::Backend::Codex,
        }
    }
}

#[derive(Parser)]
#[command(
    name = "slopd-acp",
    about = "Expose slopd-managed agent panes as an ACP stdio agent",
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

    /// Connect through iroh. Implied by --endpoint or --addr-file.
    #[arg(long)]
    iroh: bool,

    /// Iroh endpoint alias or raw EndpointId. Uses the default from the shared
    /// iroh-slopctl config when omitted.
    #[arg(long, value_name = "NAME_OR_ID")]
    endpoint: Option<String>,

    /// Read the remote iroh EndpointAddr from this JSON file.
    #[arg(long, value_name = "PATH")]
    addr_file: Option<PathBuf>,

    /// Iroh client config. Defaults to the same config used by iroh-slopctl, so
    /// both programs have the same client EndpointId and server authorization.
    #[arg(long, value_name = "PATH")]
    iroh_config: Option<PathBuf>,

    /// Named slopd account used for newly-created panes.
    #[arg(short, long, value_name = "NAME")]
    account: Option<String>,

    /// Underlying CLI backend used for newly-created panes.
    #[arg(long, value_enum)]
    backend: Option<Backend>,

    /// Override ACP's cwd when starting the underlying pane. This is useful
    /// when iroh connects to a host with a different filesystem layout.
    #[arg(long, value_name = "REMOTE_PATH")]
    working_directory: Option<PathBuf>,

    /// Extra environment variable for each pane (repeatable KEY=VALUE).
    #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
    env: Vec<String>,

    /// Extra argument passed to the underlying agent CLI (repeatable).
    #[arg(long = "agent-arg", value_name = "ARG", allow_hyphen_values = true)]
    agent_args: Vec<String>,

    /// How to handle ACP's systemPrompt, which has no backend-neutral slopd
    /// equivalent.
    #[arg(long, value_enum, default_value = "prepend")]
    system_prompt_mode: SystemPromptMode,

    /// Seconds to wait for a newly-created pane to become live.
    #[arg(long, default_value_t = 30)]
    ready_timeout: u64,

    /// Seconds slopd may spend accepting a prompt.
    #[arg(long, default_value_t = 30)]
    send_timeout: u64,

    /// Maximum wall-clock seconds for one ACP turn.
    #[arg(long, default_value_t = 3600)]
    turn_timeout: u64,

    /// Maximum ACP sessions (and therefore managed panes) in this process.
    #[arg(long, default_value_t = 64)]
    max_sessions: usize,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    let transport = match build_transport(&cli).await {
        Ok(transport) => transport,
        Err(error) => {
            eprintln!("slopd-acp: {error}");
            std::process::exit(1);
        }
    };
    let env = match parse_env(&cli.env) {
        Ok(env) => env,
        Err(error) => {
            eprintln!("slopd-acp: {error}");
            std::process::exit(2);
        }
    };
    if cli.max_sessions == 0 {
        eprintln!("slopd-acp: --max-sessions must be greater than zero");
        std::process::exit(2);
    }

    let adapter = Adapter::new(adapter::Config {
        transport,
        account: cli.account.clone(),
        backend: cli.backend.map(Into::into),
        extra_args: cli.agent_args.clone(),
        env,
        working_directory: cli.working_directory.clone(),
        system_prompt_mode: cli.system_prompt_mode,
        ready_timeout: Duration::from_secs(cli.ready_timeout),
        send_timeout_secs: cli.send_timeout,
        turn_timeout: Duration::from_secs(cli.turn_timeout),
        max_sessions: cli.max_sessions,
    });

    let (sender, receiver) = tokio::sync::mpsc::channel(256);
    let writer = tokio::spawn(wire::writer_task(receiver));
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
    loop {
        let line = match wire::read_bounded_line(&mut stdin, MAX_FRAME_BYTES).await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                tracing::error!("failed to read ACP frame: {error}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(message) => adapter.dispatch(message, &sender).await,
            Err(error) => {
                wire::send(
                    &sender,
                    wire::error(
                        serde_json::Value::Null,
                        wire::PARSE_ERROR,
                        format!("jsonrpc: parse error: {error}"),
                    ),
                )
                .await;
            }
        }
    }

    drop(sender);
    let _ = writer.await;
}

async fn build_transport(cli: &Cli) -> Result<Transport, String> {
    let remote = cli.iroh || cli.endpoint.is_some() || cli.addr_file.is_some();
    if remote && cli.socket.is_some() {
        return Err("--socket cannot be combined with iroh transport options".into());
    }
    if !remote && cli.iroh_config.is_some() {
        return Err("--iroh-config requires --iroh, --endpoint, or --addr-file".into());
    }
    if !remote {
        let socket = cli
            .socket
            .as_deref()
            .map(libslop::expand_path)
            .unwrap_or_else(libslop::socket_path);
        return Ok(Transport::Local(socket));
    }

    let config_path = cli
        .iroh_config
        .as_deref()
        .map(libslop::expand_path)
        .unwrap_or_else(libslopiroh::default_client_config_path);
    let mut config = libslopiroh::ClientConfig::load(config_path);
    let secret_key = config.secret_key().map_err(|error| error.to_string())?;
    let remote = if let Some(addr_file) = cli.addr_file.as_deref() {
        let path = libslop::expand_path(addr_file);
        libslopiroh::read_addr_file(&path).map_err(|error| error.to_string())?
    } else {
        config
            .resolve_endpoint(cli.endpoint.as_deref())
            .map_err(|error| error.to_string())?
    };
    let connector = libslopiroh::Connector::bind(secret_key, remote)
        .await
        .map_err(|error| error.to_string())?;
    tracing::info!("iroh client EndpointId: {}", connector.client_id());
    Ok(Transport::Iroh(connector))
}

fn parse_env(raw: &[String]) -> Result<Vec<(String, String)>, String> {
    raw.iter()
        .map(|entry| {
            let (key, value) = entry
                .split_once('=')
                .ok_or_else(|| format!("invalid --env {entry:?}: expected KEY=VALUE"))?;
            if key.is_empty()
                || !key.chars().enumerate().all(|(index, character)| {
                    character == '_'
                        || character.is_ascii_alphabetic()
                        || (index > 0 && character.is_ascii_digit())
                })
            {
                return Err(format!("invalid environment variable name {key:?}"));
            }
            Ok((key.to_string(), value.to_string()))
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_parser_is_strict() {
        assert_eq!(
            parse_env(&["A=1".into(), "B_C=two=three".into()]).unwrap(),
            vec![("A".into(), "1".into()), ("B_C".into(), "two=three".into())]
        );
        assert!(parse_env(&["1A=no".into()]).is_err());
        assert!(parse_env(&["missing".into()]).is_err());
    }
}
