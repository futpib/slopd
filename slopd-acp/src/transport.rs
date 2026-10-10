use std::path::PathBuf;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;

pub type BoxReader = Box<dyn AsyncRead + Unpin + Send>;
pub type BoxWriter = Box<dyn AsyncWrite + Unpin + Send>;
pub type Client = libslopctl::Client<BoxReader, BoxWriter>;

#[derive(Clone)]
pub struct Transport(PathBuf);

impl Transport {
    pub fn local(path: PathBuf) -> Self {
        Self(path)
    }

    pub async fn connect(&self) -> Result<Client, String> {
        let stream = UnixStream::connect(&self.0)
            .await
            .map_err(|error| format!("failed to connect to {}: {error}", self.0.display()))?;
        let (reader, writer) = stream.into_split();
        Ok(libslopctl::Client::new(
            Box::new(reader) as BoxReader,
            Box::new(writer) as BoxWriter,
        ))
    }
}
