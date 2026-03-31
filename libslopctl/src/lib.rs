use std::path::PathBuf;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tracing::debug;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Parse(serde_json::Error),
    Server(String),
    UnexpectedResponse(String),
    ConnectionClosed,
    FilterError(String),
    SelectError(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {}", e),
            Error::Parse(e) => write!(f, "parse error: {}", e),
            Error::Server(msg) => write!(f, "server error: {}", msg),
            Error::UnexpectedResponse(r) => write!(f, "unexpected response: {}", r),
            Error::ConnectionClosed => write!(f, "connection closed unexpectedly"),
            Error::FilterError(msg) => write!(f, "filter error: {}", msg),
            Error::SelectError(msg) => write!(f, "select error: {}", msg),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Parse(e)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectMode {
    One,
    Any,
    All,
}

/// Parse "key=value" filter strings. Returns an error on malformed input.
pub fn parse_filters(raw: Vec<String>) -> Result<Vec<(String, String)>, Error> {
    raw.into_iter().map(|f| {
        match f.split_once('=') {
            Some((k, v)) => {
                if k != "tag" {
                    return Err(Error::FilterError(
                        format!("unknown filter key {:?}: only 'tag' is supported", k),
                    ));
                }
                Ok((k.to_string(), v.to_string()))
            }
            None => Err(Error::FilterError(
                format!("invalid filter {:?}: expected key=value", f),
            )),
        }
    }).collect()
}

/// Apply parsed filters to a pane list. AND semantics: pane must satisfy all filters.
pub fn apply_filters(panes: Vec<libslop::PaneInfo>, filters: &[(String, String)]) -> Vec<libslop::PaneInfo> {
    if filters.is_empty() {
        return panes;
    }
    panes.into_iter().filter(|pane| {
        filters.iter().all(|(key, value)| {
            match key.as_str() {
                "tag" => pane.tags.iter().any(|t| t == value),
                _ => false,
            }
        })
    }).collect()
}

/// Transport-agnostic client for the slopd JSON-RPC protocol.
pub struct Client<R: tokio::io::AsyncRead + Unpin, W: tokio::io::AsyncWrite + Unpin> {
    lines: Lines<BufReader<R>>,
    writer: W,
    next_id: u64,
}

impl<R: tokio::io::AsyncRead + Unpin, W: tokio::io::AsyncWrite + Unpin> Client<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            lines: BufReader::new(reader).lines(),
            writer,
            next_id: 1,
        }
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Send a request and wait for the response with matching id.
    pub async fn request(&mut self, body: libslop::RequestBody) -> Result<libslop::ResponseBody, Error> {
        let id = self.alloc_id();
        let request = libslop::Request { id, body };
        let mut json = serde_json::to_string(&request)?;
        debug!("sending: {}", json);
        json.push('\n');
        self.writer.write_all(json.as_bytes()).await?;
        loop {
            match self.lines.next_line().await? {
                Some(line) => {
                    debug!("received: {}", line);
                    let response: libslop::Response = serde_json::from_str(&line)?;
                    if response.id == id {
                        return match response.body {
                            libslop::ResponseBody::Error { message } => Err(Error::Server(message)),
                            body => Ok(body),
                        };
                    }
                }
                None => return Err(Error::ConnectionClosed),
            }
        }
    }

    pub async fn status(&mut self) -> Result<libslop::DaemonState, Error> {
        match self.request(libslop::RequestBody::Status).await? {
            libslop::ResponseBody::Status { state } => Ok(state),
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }

    pub async fn ps(&mut self) -> Result<Vec<libslop::PaneInfo>, Error> {
        match self.request(libslop::RequestBody::Ps).await? {
            libslop::ResponseBody::Ps { panes } => Ok(panes),
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }

    pub async fn run(
        &mut self,
        parent_pane_id: Option<String>,
        extra_args: Vec<String>,
        start_directory: Option<PathBuf>,
    ) -> Result<String, Error> {
        match self.request(libslop::RequestBody::Run { parent_pane_id, extra_args, start_directory }).await? {
            libslop::ResponseBody::Run { pane_id } => Ok(pane_id),
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }

    pub async fn kill(&mut self, pane_id: String) -> Result<String, Error> {
        match self.request(libslop::RequestBody::Kill { pane_id }).await? {
            libslop::ResponseBody::Kill { pane_id } => Ok(pane_id),
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }

    pub async fn send_prompt(
        &mut self,
        pane_id: String,
        prompt: String,
        timeout_secs: u64,
        interrupt: bool,
    ) -> Result<String, Error> {
        match self.request(libslop::RequestBody::Send { pane_id, prompt, timeout_secs, interrupt }).await? {
            libslop::ResponseBody::Sent { pane_id } => Ok(pane_id),
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }

    pub async fn interrupt(&mut self, pane_id: String) -> Result<String, Error> {
        match self.request(libslop::RequestBody::Interrupt { pane_id }).await? {
            libslop::ResponseBody::Interrupted { pane_id } => Ok(pane_id),
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }

    pub async fn hook(
        &mut self,
        event: String,
        payload: serde_json::Value,
        pane_id: Option<String>,
    ) -> Result<(), Error> {
        match self.request(libslop::RequestBody::Hook { event, payload, pane_id }).await? {
            libslop::ResponseBody::Hooked => Ok(()),
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }

    pub async fn tag(&mut self, pane_id: String, tag: String) -> Result<(String, String), Error> {
        match self.request(libslop::RequestBody::Tag { pane_id, tag, remove: false }).await? {
            libslop::ResponseBody::Tagged { pane_id, tag } => Ok((pane_id, tag)),
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }

    pub async fn untag(&mut self, pane_id: String, tag: String) -> Result<(String, String), Error> {
        match self.request(libslop::RequestBody::Tag { pane_id, tag, remove: true }).await? {
            libslop::ResponseBody::Untagged { pane_id, tag } => Ok((pane_id, tag)),
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }

    pub async fn tags(&mut self, pane_id: String) -> Result<Vec<String>, Error> {
        match self.request(libslop::RequestBody::Tags { pane_id }).await? {
            libslop::ResponseBody::Tags { pane_id: _, tags } => Ok(tags),
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }

    pub async fn read_transcript(
        &mut self,
        pane_id: String,
        before_cursor: Option<u64>,
        limit: u64,
    ) -> Result<Vec<libslop::Record>, Error> {
        match self.request(libslop::RequestBody::ReadTranscript { pane_id, before_cursor, limit }).await? {
            libslop::ResponseBody::TranscriptPage { records } => Ok(records),
            other => Err(Error::UnexpectedResponse(format!("{:?}", other))),
        }
    }

    /// Send a prompt to panes matching filters, with selection mode.
    ///
    /// Returns the list of pane IDs that were successfully sent to.
    pub async fn send_filtered(
        &mut self,
        filters: &[(String, String)],
        prompt: &str,
        select: &SelectMode,
        timeout_secs: u64,
        interrupt: bool,
    ) -> Result<Vec<String>, Error> {
        let all_panes = self.ps().await?;
        let matched = apply_filters(all_panes, filters);

        let filter_desc = filters.iter().map(|(k, v)| format!("{}={}", k, v)).collect::<Vec<_>>().join(", ");

        let target_pane_ids: Vec<String> = match select {
            SelectMode::One => {
                if matched.len() != 1 {
                    return Err(Error::SelectError(format!(
                        "expected exactly one pane matching {}, found {}",
                        filter_desc, matched.len()
                    )));
                }
                vec![matched.into_iter().next().unwrap().pane_id]
            }
            SelectMode::Any => {
                if matched.is_empty() {
                    return Err(Error::SelectError(format!(
                        "no panes match filter {}", filter_desc
                    )));
                }
                use std::time::{SystemTime, UNIX_EPOCH};
                let idx = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos() as usize % matched.len();
                vec![matched.into_iter().nth(idx).unwrap().pane_id]
            }
            SelectMode::All => {
                if matched.is_empty() {
                    return Err(Error::SelectError(format!(
                        "no panes match filter {}", filter_desc
                    )));
                }
                matched.into_iter().map(|p| p.pane_id).collect()
            }
        };

        // Send all requests on the same connection, each with a unique ID,
        // then read responses correlating by ID.
        let mut pending: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
        for pane_id in &target_pane_ids {
            let id = self.alloc_id();
            let body = libslop::RequestBody::Send {
                pane_id: pane_id.clone(),
                prompt: prompt.to_string(),
                timeout_secs,
                interrupt,
            };
            let request = libslop::Request { id, body };
            let mut json = serde_json::to_string(&request)?;
            debug!("sending: {}", json);
            json.push('\n');
            self.writer.write_all(json.as_bytes()).await?;
            pending.insert(id, pane_id.clone());
        }

        // Collect all responses; order may differ from send order.
        let mut results: std::collections::HashMap<u64, libslop::ResponseBody> = std::collections::HashMap::new();
        while results.len() < pending.len() {
            match self.lines.next_line().await? {
                Some(line) => {
                    debug!("received: {}", line);
                    let response: libslop::Response = serde_json::from_str(&line)?;
                    if pending.contains_key(&response.id) {
                        results.insert(response.id, response.body);
                    }
                }
                None => return Err(Error::ConnectionClosed),
            }
        }

        // Return results in send order.
        let mut out = Vec::new();
        let mut ids: Vec<u64> = pending.keys().copied().collect();
        ids.sort();
        for req_id in ids {
            let pane_id = &pending[&req_id];
            match &results[&req_id] {
                libslop::ResponseBody::Sent { pane_id } => out.push(pane_id.clone()),
                libslop::ResponseBody::Error { message } => {
                    return Err(Error::Server(format!("error sending to {}: {}", pane_id, message)));
                }
                _ => {
                    return Err(Error::Server(format!("unexpected response for {}", pane_id)));
                }
            }
        }

        Ok(out)
    }

    /// Subscribe to events. Consumes the client and returns a Subscription
    /// that yields Record items.
    pub async fn subscribe(mut self, filters: Vec<libslop::EventFilter>) -> Result<Subscription<R>, Error> {
        let id = self.alloc_id();
        let request = libslop::Request {
            id,
            body: libslop::RequestBody::Subscribe { filters },
        };
        let mut json = serde_json::to_string(&request)?;
        debug!("sending: {}", json);
        json.push('\n');
        self.writer.write_all(json.as_bytes()).await?;

        // Read the Subscribed confirmation.
        loop {
            match self.lines.next_line().await? {
                Some(line) => {
                    debug!("received: {}", line);
                    let response: libslop::Response = serde_json::from_str(&line)?;
                    if response.id == id {
                        match response.body {
                            libslop::ResponseBody::Subscribed => break,
                            libslop::ResponseBody::Error { message } => return Err(Error::Server(message)),
                            other => return Err(Error::UnexpectedResponse(format!("{:?}", other))),
                        }
                    }
                }
                None => return Err(Error::ConnectionClosed),
            }
        }

        Ok(Subscription { lines: self.lines, id })
    }

    /// Subscribe to a pane's transcript with replay. Consumes the client and
    /// returns a Subscription that yields Record items.
    pub async fn subscribe_transcript(mut self, pane_id: String, last_n: u64) -> Result<Subscription<R>, Error> {
        let id = self.alloc_id();
        let request = libslop::Request {
            id,
            body: libslop::RequestBody::SubscribeTranscript { pane_id, last_n },
        };
        let mut json = serde_json::to_string(&request)?;
        debug!("sending: {}", json);
        json.push('\n');
        self.writer.write_all(json.as_bytes()).await?;

        // Read the Subscribed confirmation.
        loop {
            match self.lines.next_line().await? {
                Some(line) => {
                    debug!("received: {}", line);
                    let response: libslop::Response = serde_json::from_str(&line)?;
                    if response.id == id {
                        match response.body {
                            libslop::ResponseBody::Subscribed => break,
                            libslop::ResponseBody::Error { message } => return Err(Error::Server(message)),
                            other => return Err(Error::UnexpectedResponse(format!("{:?}", other))),
                        }
                    }
                }
                None => return Err(Error::ConnectionClosed),
            }
        }

        Ok(Subscription { lines: self.lines, id })
    }
}

/// A subscription stream that yields Record items from slopd.
pub struct Subscription<R: tokio::io::AsyncRead + Unpin> {
    lines: Lines<BufReader<R>>,
    id: u64,
}

/// The result of calling `next()` on a Subscription.
pub enum SubscriptionItem {
    Record(libslop::Record),
    Subscribed,
}

impl<R: tokio::io::AsyncRead + Unpin> Subscription<R> {
    /// Read the next record from the subscription.
    /// Returns `Ok(None)` when the connection closes cleanly.
    pub async fn next(&mut self) -> Result<Option<SubscriptionItem>, Error> {
        match self.lines.next_line().await? {
            Some(line) => {
                debug!("received: {}", line);
                let response: libslop::Response = serde_json::from_str(&line)?;
                if response.id != self.id {
                    return Ok(None);
                }
                match response.body {
                    libslop::ResponseBody::Record(record) => Ok(Some(SubscriptionItem::Record(record))),
                    libslop::ResponseBody::Subscribed => Ok(Some(SubscriptionItem::Subscribed)),
                    libslop::ResponseBody::Error { message } => Err(Error::Server(message)),
                    _ => Ok(None),
                }
            }
            None => Ok(None),
        }
    }
}
