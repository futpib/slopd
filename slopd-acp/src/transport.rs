use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};

use iroh::endpoint::{Connection, RecvStream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UnixStream;

pub type BoxReader = Box<dyn AsyncRead + Unpin + Send>;
pub type BoxWriter = Box<dyn AsyncWrite + Unpin + Send>;
pub type Client = libslopctl::Client<BoxReader, BoxWriter>;

#[derive(Clone)]
pub enum Transport {
    Local(PathBuf),
    Iroh(libslopiroh::Connector),
}

impl Transport {
    pub async fn connect(&self) -> Result<Client, String> {
        match self {
            Transport::Local(path) => {
                let stream = UnixStream::connect(path)
                    .await
                    .map_err(|error| format!("failed to connect to {}: {error}", path.display()))?;
                let (reader, writer) = stream.into_split();
                Ok(libslopctl::Client::new(
                    Box::new(reader) as BoxReader,
                    Box::new(writer) as BoxWriter,
                ))
            }
            Transport::Iroh(connector) => {
                let stream = connector.open().await.map_err(|error| {
                    if let Some(client_id) = error.unauthorized_client() {
                        format!(
                            "{error}; authorize this client on the server with: \
                             iroh-slopd authorize {client_id}"
                        )
                    } else {
                        error.to_string()
                    }
                })?;
                // Retain the connection handle alongside the reader. The stream
                // objects also retain it internally, but this makes the intended
                // lifetime explicit and preserves close diagnostics.
                let reader = ConnectedReader {
                    inner: stream.recv,
                    _connection: stream.connection,
                };
                Ok(libslopctl::Client::new(
                    Box::new(reader) as BoxReader,
                    Box::new(stream.send) as BoxWriter,
                ))
            }
        }
    }
}

struct ConnectedReader {
    inner: RecvStream,
    _connection: Connection,
}

impl AsyncRead for ConnectedReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}
