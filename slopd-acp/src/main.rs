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
    Grok,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum OrphanPolicy {
    Preserve,
    Kill,
}

impl From<OrphanPolicy> for libslop::LeaseExpiryPolicy {
    fn from(policy: OrphanPolicy) -> Self {
        match policy {
            OrphanPolicy::Preserve => libslop::LeaseExpiryPolicy::Preserve,
            OrphanPolicy::Kill => libslop::LeaseExpiryPolicy::Kill,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ShutdownPolicy {
    Preserve,
    Close,
}

impl From<Backend> for libslop::Backend {
    fn from(backend: Backend) -> Self {
        match backend {
            Backend::Claude => libslop::Backend::Claude,
            Backend::Opencode => libslop::Backend::Opencode,
            Backend::Codex => libslop::Backend::Codex,
            Backend::Grok => libslop::Backend::Grok,
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

    /// Stable ownership scope for durable sessions. Keep this unchanged when
    /// renaming an account or changing its backend.
    #[arg(long, value_name = "ID")]
    session_scope: Option<String>,

    /// Override ACP's cwd when starting the underlying pane. This is useful
    /// when iroh connects to a host with a different filesystem layout.
    #[arg(long, value_name = "REMOTE_PATH")]
    working_directory: Option<PathBuf>,

    /// Extra environment variable for each pane (repeatable KEY=VALUE).
    #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
    env: Vec<String>,

    /// Inherit a named environment variable into each managed pane (repeatable).
    /// This is opt-in because iroh may target a different machine.
    #[arg(long = "inherit-env", value_name = "NAME")]
    inherit_env: Vec<String>,

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

    /// Maximum live managed panes. The least-recently-used inactive pane is
    /// evicted at the limit, then natively resumed when possible if reused.
    #[arg(long, default_value_t = 4)]
    max_sessions: usize,

    /// Reclaim inactive live panes after this many seconds. Zero disables it.
    #[arg(long, default_value_t = 0)]
    idle_timeout: u64,

    /// Seconds between live-pane reconciliation passes.
    #[arg(long, default_value_t = 15)]
    reconcile_interval: u64,

    /// Seconds between the last heartbeat and lease expiry.
    #[arg(long, default_value_t = 60)]
    lease_ttl: u64,

    /// Seconds an expired lease remains available for replacement handoff.
    #[arg(long, default_value_t = 300)]
    handoff_grace: u64,

    /// What slopd does with ready panes after lease expiry and handoff grace.
    #[arg(long, value_enum, default_value = "kill")]
    orphan_policy: OrphanPolicy,

    /// Stop listing closed logical sessions older than this many seconds.
    /// Zero retains them indefinitely.
    #[arg(long, default_value_t = 0)]
    session_retention: u64,

    /// What a normal EOF, SIGTERM, or SIGINT does with resident panes.
    #[arg(long, value_enum, default_value = "preserve")]
    shutdown_policy: ShutdownPolicy,
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
    let mut env = match parse_env(&cli.env) {
        Ok(env) => env,
        Err(error) => {
            eprintln!("slopd-acp: {error}");
            std::process::exit(2);
        }
    };
    if let Err(error) = inherit_named_env(&mut env, &cli.inherit_env) {
        eprintln!("slopd-acp: {error}");
        std::process::exit(2);
    }
    if cli.max_sessions == 0 {
        eprintln!("slopd-acp: --max-sessions must be greater than zero");
        std::process::exit(2);
    }
    if cli.reconcile_interval == 0 || cli.lease_ttl == 0 {
        eprintln!("slopd-acp: --reconcile-interval and --lease-ttl must be greater than zero");
        std::process::exit(2);
    }
    let session_scope = cli.session_scope.clone().unwrap_or_else(|| {
        let account = cli.account.as_deref().unwrap_or(libslop::DEFAULT_ACCOUNT);
        let backend = cli
            .backend
            .map(|backend| match backend {
                Backend::Claude => "claude",
                Backend::Opencode => "opencode",
                Backend::Codex => "codex",
                Backend::Grok => "grok",
            })
            .unwrap_or("auto");
        format!("{account}:{backend}")
    });
    if session_scope.is_empty() || session_scope.len() > 128 {
        eprintln!("slopd-acp: --session-scope must contain 1 to 128 bytes");
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
        session_scope: session_scope.clone(),
        idle_timeout: (cli.idle_timeout != 0).then(|| Duration::from_secs(cli.idle_timeout)),
        lease_ttl_secs: cli.lease_ttl,
        handoff_grace_secs: cli.handoff_grace,
        orphan_policy: cli.orphan_policy.into(),
        session_retention_secs: (cli.session_retention != 0).then_some(cli.session_retention),
    });
    tracing::debug!(scope = %session_scope, "acquiring ownership lease");
    if let Err(error) = adapter.acquire_lease().await {
        eprintln!("slopd-acp: failed to acquire ownership lease: {error}");
        std::process::exit(1);
    }
    tracing::debug!("acquired ownership lease");
    if let Err(error) = adapter.recover_sessions().await {
        eprintln!("slopd-acp: failed to recover durable ACP sessions: {error}");
        std::process::exit(1);
    }

    let background_stop = tokio_util::sync::CancellationToken::new();
    let lease_lost = tokio_util::sync::CancellationToken::new();
    let heartbeat = {
        let adapter = adapter.clone();
        let stop = background_stop.clone();
        let lost = lease_lost.clone();
        let interval = Duration::from_secs((cli.lease_ttl / 3).max(1));
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval(interval);
            ticks.tick().await;
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = ticks.tick() => {
                        if let Err(error) = adapter.renew_lease().await {
                            tracing::error!("ownership lease was lost: {error}");
                            lost.cancel();
                            break;
                        }
                    }
                }
            }
        })
    };
    let maintenance = {
        let adapter = adapter.clone();
        let stop = background_stop.clone();
        let interval = Duration::from_secs(cli.reconcile_interval);
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval(interval);
            ticks.tick().await;
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = ticks.tick() => {
                        if let Err(error) = adapter.reconcile_sessions().await {
                            tracing::warn!("ACP session reconciliation failed: {error}");
                        }
                    }
                }
            }
        })
    };

    let (sender, receiver) = tokio::sync::mpsc::channel(256);
    let writer = tokio::spawn(wire::writer_task(receiver));
    let (input_sender, mut input_receiver) = tokio::sync::mpsc::channel(16);
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut stdin = std::io::BufReader::new(stdin.lock());
        loop {
            let line = wire::read_bounded_line_sync(&mut stdin, MAX_FRAME_BYTES);
            let finished = !matches!(line, Ok(Some(_)));
            if input_sender.blocking_send(line).is_err() || finished {
                break;
            }
        }
    });
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .expect("install SIGINT handler");
    let mut sigusr1 = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
        .expect("install SIGUSR1 handler");
    let mut close_panes = matches!(cli.shutdown_policy, ShutdownPolicy::Close);
    loop {
        tokio::select! {
            line = input_receiver.recv() => {
                let line = match line {
                    Some(Ok(Some(line))) => line,
                    Some(Ok(None)) | None => break,
                    Some(Err(error)) => {
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
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
            _ = sigusr1.recv() => {
                close_panes = true;
                break;
            }
            _ = lease_lost.cancelled() => break,
        }
    }

    background_stop.cancel();
    tracing::debug!("stopping ACP background tasks");
    let _ = heartbeat.await;
    let _ = maintenance.await;
    tracing::debug!("running ACP shutdown policy");
    adapter.shutdown(close_panes).await;
    tracing::debug!("waiting for ACP output writer");
    drop(sender);
    let _ = writer.await;
    tracing::debug!("ACP adapter stopped");
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
            validate_env_name(key)?;
            Ok((key.to_string(), value.to_string()))
        })
        .collect()
}

fn validate_env_name(key: &str) -> Result<(), String> {
    if key.is_empty()
        || !key.chars().enumerate().all(|(index, character)| {
            character == '_'
                || character.is_ascii_alphabetic()
                || (index > 0 && character.is_ascii_digit())
        })
    {
        return Err(format!("invalid environment variable name {key:?}"));
    }
    Ok(())
}

fn inherit_named_env(env: &mut Vec<(String, String)>, names: &[String]) -> Result<(), String> {
    merge_inherited_env(env, names, |key| std::env::var(key).ok())
}

fn merge_inherited_env(
    env: &mut Vec<(String, String)>,
    names: &[String],
    mut lookup: impl FnMut(&str) -> Option<String>,
) -> Result<(), String> {
    for key in names {
        validate_env_name(key)?;
        if env.iter().any(|(existing, _)| existing == key) {
            continue;
        }
        if let Some(value) = lookup(key)
            && !value.is_empty()
        {
            env.push((key.to_string(), value));
        }
    }
    Ok(())
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

    #[test]
    fn named_env_inheritance_is_explicit_and_preserves_explicit_values() {
        let mut env = vec![("SERVICE_TOKEN".into(), "explicit-secret".into())];
        merge_inherited_env(
            &mut env,
            &[
                "SERVICE_TOKEN".into(),
                "SERVICE_URL".into(),
                "EMPTY_VALUE".into(),
            ],
            |key| match key {
                "SERVICE_TOKEN" => Some("ambient-secret".into()),
                "SERVICE_URL" => Some("https://service.example".into()),
                "EMPTY_VALUE" => Some(String::new()),
                "UNREQUESTED_SECRET" => Some("must-not-leak".into()),
                _ => None,
            },
        )
        .unwrap();

        assert_eq!(
            env,
            vec![
                ("SERVICE_TOKEN".into(), "explicit-secret".into()),
                ("SERVICE_URL".into(), "https://service.example".into()),
            ]
        );
    }

    #[test]
    fn named_env_inheritance_rejects_invalid_names() {
        let mut env = Vec::new();
        let error =
            merge_inherited_env(&mut env, &["INVALID-NAME".into()], |_| Some("value".into()))
                .unwrap_err();

        assert_eq!(error, "invalid environment variable name \"INVALID-NAME\"");
        assert!(env.is_empty());
    }
}
