use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use iroh::{Endpoint, PublicKey, SecretKey};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tracing::{debug, info, warn};

const ALPN: &[u8] = b"iroh-slopd/0";

#[derive(Parser)]
#[command(name = "iroh-slopd", about = "Expose slopd over iroh with EndpointId allowlist auth")]
struct Cli {
    #[arg(short, long, action = clap::ArgAction::Count, help = "Increase log verbosity")]
    verbose: u8,

    /// Write the full EndpointAddr as JSON to this file on startup (useful for scripts/tests).
    #[arg(long, value_name = "PATH")]
    addr_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Add a client EndpointId to the authorized list.
    Authorize {
        /// Client EndpointId (z-base-32 encoded public key).
        endpoint_id: String,
    },
    /// Remove a client EndpointId from the authorized list.
    Revoke {
        /// Client EndpointId to remove.
        endpoint_id: String,
    },
    /// Print this server's EndpointId.
    Info,
}

fn config_path() -> PathBuf {
    libslop::config_dir().join("iroh-slopd/config.toml")
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Config {
    secret_key: Option<String>,
    #[serde(default)]
    authorized_clients: Vec<String>,
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

    fn is_authorized(&self, id: &PublicKey) -> bool {
        let id_str = id.to_string();
        self.authorized_clients.iter().any(|c| c == &id_str)
    }
}

async fn proxy_connection(
    mut iroh_send: iroh::endpoint::SendStream,
    mut iroh_recv: iroh::endpoint::RecvStream,
    socket_path: PathBuf,
) {
    let unix_stream = match UnixStream::connect(&socket_path).await {
        Ok(s) => s,
        Err(e) => {
            warn!("failed to connect to slopd socket: {}", e);
            return;
        }
    };
    let (mut unix_read, mut unix_write) = unix_stream.into_split();

    tokio::select! {
        result = tokio::io::copy(&mut iroh_recv, &mut unix_write) => {
            if let Err(e) = result {
                debug!("iroh->unix copy ended: {}", e);
            }
            let _ = unix_write.shutdown().await;
        }
        result = tokio::io::copy(&mut unix_read, &mut iroh_send) => {
            if let Err(e) = result {
                debug!("unix->iroh copy ended: {}", e);
            }
            let _ = iroh_send.finish();
        }
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

    let mut config = Config::load();

    match cli.command {
        Some(Command::Authorize { endpoint_id }) => {
            // Validate the endpoint_id parses as a PublicKey.
            endpoint_id.parse::<PublicKey>().unwrap_or_else(|e| {
                eprintln!("invalid endpoint_id: {}", e);
                std::process::exit(1);
            });
            if !config.authorized_clients.contains(&endpoint_id) {
                config.authorized_clients.push(endpoint_id.clone());
                config.save();
                eprintln!("authorized: {}", endpoint_id);
            } else {
                eprintln!("already authorized: {}", endpoint_id);
            }
            return;
        }
        Some(Command::Revoke { endpoint_id }) => {
            let before = config.authorized_clients.len();
            config.authorized_clients.retain(|c| c != &endpoint_id);
            if config.authorized_clients.len() < before {
                config.save();
                eprintln!("revoked: {}", endpoint_id);
            } else {
                eprintln!("not found: {}", endpoint_id);
            }
            return;
        }
        Some(Command::Info) => {
            let secret_key = config.secret_key();
            println!("{}", secret_key.public());
            return;
        }
        None => {}
    }

    // Main server mode.
    let secret_key = config.secret_key();
    let endpoint_id = secret_key.public();

    if config.authorized_clients.is_empty() {
        eprintln!("warning: no authorized clients configured");
        eprintln!("use `iroh-slopd authorize <endpoint-id>` to add clients");
    }

    let config = Arc::new(config);

    let endpoint = Endpoint::builder()
        .secret_key(secret_key)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .unwrap_or_else(|e| {
            eprintln!("failed to bind iroh endpoint: {}", e);
            std::process::exit(1);
        });

    let addr = endpoint.addr();
    let addr_json = serde_json::to_string(&addr).unwrap();
    eprintln!("iroh-slopd endpoint: {}", endpoint_id);
    eprintln!("iroh-slopd addr: {}", addr_json);
    eprintln!("waiting for connections...");

    if let Some(ref addr_file) = cli.addr_file {
        std::fs::write(addr_file, &addr_json).unwrap_or_else(|e| {
            eprintln!("failed to write addr file: {}", e);
            std::process::exit(1);
        });
    }

    let socket_path = libslop::socket_path();

    let mut sigterm = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::terminate(),
    ).expect("failed to install SIGTERM handler");

    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else {
                    info!("endpoint closed");
                    break;
                };

                let config = config.clone();
                let socket_path = socket_path.clone();

                tokio::spawn(async move {
                    let accepting = match incoming.accept() {
                        Ok(accepting) => accepting,
                        Err(e) => {
                            warn!("failed to accept connection: {}", e);
                            return;
                        }
                    };

                    let connection = match accepting.await {
                        Ok(conn) => conn,
                        Err(e) => {
                            warn!("failed to complete connection handshake: {}", e);
                            return;
                        }
                    };

                    let remote_id = connection.remote_id();

                    if !config.is_authorized(&remote_id) {
                        warn!("rejected unauthorized client: {}", remote_id);
                        connection.close(1u32.into(), b"unauthorized");
                        return;
                    }

                    info!("accepted connection from {}", remote_id);

                    let (send, recv) = match connection.accept_bi().await {
                        Ok(pair) => pair,
                        Err(e) => {
                            warn!("failed to accept stream: {}", e);
                            return;
                        }
                    };

                    proxy_connection(send, recv, socket_path).await;
                    info!("connection from {} closed", remote_id);
                });
            }
            _ = sigterm.recv() => {
                info!("received SIGTERM, shutting down");
                break;
            }
        }
    }

    endpoint.close().await;
}
