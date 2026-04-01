use std::collections::HashMap;
use std::path::PathBuf;

use clap::Parser;
use iroh::{Endpoint, PublicKey, SecretKey, endpoint::presets};
use serde::{Deserialize, Serialize};
use tracing::debug;

const ALPN: &[u8] = b"iroh-slopd/0";

#[derive(Parser)]
#[command(
    name = "iroh-sloptui",
    about = "Remote TUI process viewer for slopd via iroh",
    version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT"), ")")
)]
struct Cli {
    #[arg(
        short,
        long,
        action = clap::ArgAction::Count,
        help = "Increase log verbosity"
    )]
    verbose: u8,

    /// Endpoint name (from config) or raw EndpointId to connect to. Overrides the default.
    #[arg(long, global = true)]
    endpoint: Option<String>,

    /// Read the server's full EndpointAddr from this JSON file (for direct connections without discovery).
    #[arg(long, global = true, value_name = "PATH")]
    addr_file: Option<PathBuf>,
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
            let bytes = data_encoding::BASE32_NOPAD
                .decode(key_str.as_bytes())
                .unwrap_or_else(|e| {
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

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let _log_guard = libsloptui_ratatui::setup_logging(cli.verbose);

    let mut config = Config::load();
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

    libsloptui_ratatui::run(&mut client).await.unwrap_or_else(|e| {
        eprintln!("error: {}", e);
        std::process::exit(1);
    });

    endpoint.close().await;
}
