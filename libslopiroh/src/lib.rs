//! Shared client-side iroh transport for slopd.
//!
//! Both `iroh-slopctl` and protocol adapters use this module so they share the
//! same ALPN, endpoint aliases, persisted client identity, address-file format,
//! and unauthorized-peer detection.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use iroh::endpoint::{Connection, ConnectionError, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, PublicKey, SecretKey};
use serde::{Deserialize, Serialize};

pub const ALPN: &[u8] = b"iroh-slopd/0";

pub fn default_client_config_path() -> PathBuf {
    libslop::config_dir().join("iroh-slopctl/config.toml")
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ClientConfig {
    secret_key: Option<String>,
    default: Option<String>,
    #[serde(default)]
    endpoints: HashMap<String, EndpointConfig>,
    /// The file this config was loaded from / is saved to.
    #[serde(skip)]
    path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EndpointConfig {
    endpoint_id: String,
}

impl ClientConfig {
    /// Load a client config. This preserves iroh-slopctl's historical behavior:
    /// a missing file produces an empty config, while malformed or unreadable
    /// files warn and fall back to an empty config.
    pub fn load(path: PathBuf) -> Self {
        let mut config = match std::fs::read_to_string(&path) {
            Ok(contents) => toml::from_str(&contents).unwrap_or_else(|error| {
                eprintln!("warning: failed to parse {}: {error}", path.display());
                ClientConfig::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => ClientConfig::default(),
            Err(error) => {
                eprintln!("warning: failed to read {}: {error}", path.display());
                ClientConfig::default()
            }
        };
        config.path = path;
        config
    }

    fn save(&self) -> Result<(), Error> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| Error::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let contents = toml::to_string_pretty(self).map_err(Error::ConfigSerialize)?;
        std::fs::write(&self.path, contents).map_err(|source| Error::Io {
            path: self.path.clone(),
            source,
        })
    }

    /// Return the persisted client key, generating and saving one on first use.
    pub fn secret_key(&mut self) -> Result<SecretKey, Error> {
        if let Some(ref key_str) = self.secret_key {
            let bytes = data_encoding::BASE32_NOPAD
                .decode(key_str.as_bytes())
                .map_err(|source| Error::InvalidSecretKey(source.to_string()))?;
            let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
                Error::InvalidSecretKey("decoded key must contain exactly 32 bytes".into())
            })?;
            return Ok(SecretKey::from(bytes));
        }

        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).map_err(|source| Error::Random(source.to_string()))?;
        let key = SecretKey::from(bytes);
        self.secret_key = Some(data_encoding::BASE32_NOPAD.encode(&key.to_bytes()));
        self.save()?;
        Ok(key)
    }

    /// Resolve a configured endpoint alias or a raw EndpointId.
    pub fn resolve_endpoint(&self, endpoint_override: Option<&str>) -> Result<EndpointAddr, Error> {
        let endpoint = if let Some(name_or_id) = endpoint_override {
            self.endpoints
                .get(name_or_id)
                .map(|entry| entry.endpoint_id.as_str())
                .unwrap_or(name_or_id)
        } else if let Some(default_name) = self.default.as_deref() {
            self.endpoints
                .get(default_name)
                .map(|entry| entry.endpoint_id.as_str())
                .ok_or_else(|| Error::UnknownDefaultEndpoint(default_name.to_string()))?
        } else {
            return Err(Error::MissingEndpoint);
        };

        let id = endpoint
            .parse::<PublicKey>()
            .map_err(|source| Error::InvalidEndpoint {
                value: endpoint.to_string(),
                message: source.to_string(),
            })?;
        Ok(EndpointAddr::from(id))
    }
}

pub fn read_addr_file(path: &Path) -> Result<EndpointAddr, Error> {
    let contents = std::fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_str(&contents).map_err(|source| Error::AddressParse {
        path: path.to_path_buf(),
        source,
    })
}

#[derive(Clone)]
pub struct Connector {
    endpoint: Endpoint,
    remote: EndpointAddr,
    client_id: PublicKey,
}

impl Connector {
    pub async fn bind(secret_key: SecretKey, remote: EndpointAddr) -> Result<Self, Error> {
        let client_id = secret_key.public();
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .bind()
            .await
            .map_err(|source| Error::Bind(source.to_string()))?;
        Ok(Self {
            endpoint,
            remote,
            client_id,
        })
    }

    pub fn client_id(&self) -> PublicKey {
        self.client_id
    }

    /// Open one slopd protocol stream. Callers may open many independent
    /// streams from the same connector; the iroh endpoint and client identity
    /// are reused across all of them.
    pub async fn open(&self) -> Result<IrohStream, Error> {
        let connection = self
            .endpoint
            .connect(self.remote.clone(), ALPN)
            .await
            .map_err(|source| Error::Connect(source.to_string()))?;
        let (send, recv) = connection.open_bi().await.map_err(|source| {
            let unauthorized = is_unauthorized(Some(&source));
            Error::OpenStream {
                message: source.to_string(),
                unauthorized,
                client_id: self.client_id,
            }
        })?;
        Ok(IrohStream {
            send,
            recv,
            connection,
        })
    }

    pub async fn close(&self) {
        self.endpoint.close().await;
    }
}

pub struct IrohStream {
    pub send: SendStream,
    pub recv: RecvStream,
    /// Keep the connection handle available for diagnostics and alive for the
    /// full lifetime of the stream pair.
    pub connection: Connection,
}

pub fn is_unauthorized(close_reason: Option<&ConnectionError>) -> bool {
    matches!(
        close_reason,
        Some(ConnectionError::ApplicationClosed(close))
            if close.reason.as_ref() == b"unauthorized"
    )
}

#[derive(Debug)]
pub enum Error {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    ConfigSerialize(toml::ser::Error),
    AddressParse {
        path: PathBuf,
        source: serde_json::Error,
    },
    InvalidSecretKey(String),
    Random(String),
    MissingEndpoint,
    UnknownDefaultEndpoint(String),
    InvalidEndpoint {
        value: String,
        message: String,
    },
    Bind(String),
    Connect(String),
    OpenStream {
        message: String,
        unauthorized: bool,
        client_id: PublicKey,
    },
}

impl Error {
    pub fn unauthorized_client(&self) -> Option<PublicKey> {
        match self {
            Error::OpenStream {
                unauthorized: true,
                client_id,
                ..
            } => Some(*client_id),
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { path, source } => write!(f, "{}: {}", path.display(), source),
            Error::ConfigSerialize(source) => write!(f, "failed to serialize config: {source}"),
            Error::AddressParse { path, source } => {
                write!(
                    f,
                    "failed to parse address file {}: {}",
                    path.display(),
                    source
                )
            }
            Error::InvalidSecretKey(message) => {
                write!(f, "invalid secret_key in client config: {message}")
            }
            Error::Random(message) => write!(f, "failed to generate client key: {message}"),
            Error::MissingEndpoint => write!(
                f,
                "no endpoint specified and no default configured; use --endpoint or set default"
            ),
            Error::UnknownDefaultEndpoint(name) => {
                write!(f, "default endpoint {name:?} not found in config")
            }
            Error::InvalidEndpoint { value, message } => {
                write!(f, "invalid endpoint_id {value:?}: {message}")
            }
            Error::Bind(message) => write!(f, "failed to bind iroh endpoint: {message}"),
            Error::Connect(message) => write!(f, "failed to connect to remote endpoint: {message}"),
            Error::OpenStream { message, .. } => write!(f, "failed to open stream: {message}"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_and_raw_ids_resolve_to_the_same_endpoint() {
        let key = SecretKey::from([7_u8; 32]);
        let id = key.public();
        let mut endpoints = HashMap::new();
        endpoints.insert(
            "home".to_string(),
            EndpointConfig {
                endpoint_id: id.to_string(),
            },
        );
        let config = ClientConfig {
            default: Some("home".into()),
            endpoints,
            ..Default::default()
        };

        assert_eq!(config.resolve_endpoint(None).unwrap().id, id);
        assert_eq!(config.resolve_endpoint(Some("home")).unwrap().id, id);
        assert_eq!(
            config.resolve_endpoint(Some(&id.to_string())).unwrap().id,
            id
        );
    }
}
