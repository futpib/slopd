use std::path::PathBuf;

use clap::Subcommand;
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

#[derive(Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum SelectMode {
    /// Require exactly one matching pane; error otherwise.
    One,
    /// Pick one at random from matches; error if none.
    Any,
    /// Send to all matching panes; error if none.
    All,
}

#[derive(Subcommand)]
pub enum CommonCommand {
    /// Show slopd uptime and state.
    Status,
    /// List panes in the slopd session.
    Ps {
        /// Filter by key=value (repeatable, AND semantics). Supported keys: tag.
        #[arg(long = "filter", value_name = "KEY=VALUE")]
        filters: Vec<String>,
        /// Output as JSON array instead of table.
        #[arg(long)]
        json: bool,
    },
    /// Open a new Claude pane in the slopd tmux session.
    Run {
        /// Working directory for the new pane. The shell expands ~ and
        /// environment variables before this value reaches slopctl.
        /// Overrides [run] start_directory from config.toml for this session.
        #[arg(short = 'c', long, value_name = "DIR")]
        start_directory: Option<PathBuf>,
        /// Extra arguments passed to the Claude executable (after --).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        extra_args: Vec<String>,
    },
    /// Terminate a Claude pane.
    Kill {
        /// Tmux pane ID (e.g. %42).
        pane_id: String,
    },
    /// Type a prompt into pane(s) and wait for UserPromptSubmit confirmation.
    Send {
        /// Tmux pane ID (e.g. %42) or filter (e.g. tag=worker).
        pane_id: String,
        /// Prompt text to send.
        prompt: String,
        /// Additional filter by key=value (repeatable, AND semantics). Supported keys: tag.
        #[arg(long = "filter", value_name = "KEY=VALUE")]
        filters: Vec<String>,
        /// How to select among matching panes: one (error if not exactly one), any (pick one at random), all.
        #[arg(long, default_value = "one")]
        select: SelectMode,
        /// Seconds to wait for UserPromptSubmit confirmation per pane.
        #[arg(long, default_value = "60")]
        timeout: u64,
        /// Interrupt the pane before sending (equivalent to slopctl interrupt then send).
        #[arg(long, short = 'i')]
        interrupt: bool,
    },
    /// Send Ctrl+C, Ctrl+D, and Escape to interrupt a running agent.
    Interrupt {
        /// Tmux pane ID (e.g. %42).
        pane_id: String,
    },
    /// Subscribe to a stream of events and print each as a JSON line.
    Listen {
        /// Filter by hook event name (repeatable; omit for all events). Matches source:hook events.
        #[arg(long = "hook", value_name = "EVENT")]
        hooks: Vec<String>,
        /// Filter by slopd event name (repeatable). Matches source:slopd events (e.g. StateChange, DetailedStateChange).
        #[arg(long = "event", value_name = "EVENT")]
        events: Vec<String>,
        /// Filter by transcript record type (repeatable). Matches source:transcript events (e.g. user, assistant, progress).
        #[arg(long = "transcript", value_name = "TYPE")]
        transcripts: Vec<String>,
        /// Only receive events from this tmux pane.
        #[arg(long, value_name = "PANE_ID")]
        pane_id: Option<String>,
        /// Only receive events from this Claude session.
        #[arg(long, value_name = "SESSION_ID")]
        session_id: Option<String>,
        /// Replay the last N transcript records before switching to live events (requires --pane-id).
        #[arg(long, value_name = "N")]
        replay: Option<u64>,
    },
    /// Read historical transcript records from a pane.
    Transcript {
        /// Tmux pane ID (e.g. %42).
        pane_id: String,
        /// Byte-offset cursor; return records strictly before this offset.
        #[arg(long)]
        before: Option<u64>,
        /// Maximum number of records to return.
        #[arg(long, default_value = "50")]
        limit: u64,
    },
    /// Add a tag to a pane.
    Tag {
        /// Tmux pane ID (e.g. %42).
        pane_id: String,
        /// Tag name (ASCII letters, digits, _, -).
        tag: String,
    },
    /// Remove a tag from a pane.
    Untag {
        /// Tmux pane ID (e.g. %42).
        pane_id: String,
        /// Tag name to remove.
        tag: String,
    },
    /// List all tags on a pane.
    Tags {
        /// Tmux pane ID (e.g. %42). Defaults to $TMUX_PANE if omitted.
        pane_id: Option<String>,
    },
}

/// Context that differs between slopctl (local) and iroh-slopctl (remote).
pub struct CommandContext {
    /// For `Run`: the parent pane ID. slopctl sets this to $TMUX_PANE, iroh sets None.
    pub parent_pane_id: Option<String>,
    /// For `Tags` when pane_id is None: fallback pane ID.
    pub fallback_pane_id: Option<String>,
}

pub fn die(msg: &str) -> ! {
    eprintln!("error: {}", msg);
    std::process::exit(1);
}

pub fn die_err(e: Error) -> ! {
    die(&e.to_string());
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

/// Validate filters for a command before connecting.
pub fn validate_command_filters(command: &CommonCommand) -> Result<(), Error> {
    match command {
        CommonCommand::Ps { filters, .. } => {
            parse_filters(filters.clone())?;
        }
        CommonCommand::Send { pane_id, filters, .. } => {
            if pane_id.contains('=') {
                let mut all = vec![pane_id.clone()];
                all.extend(filters.clone());
                parse_filters(all)?;
            } else {
                parse_filters(filters.clone())?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Print a table of pane info to stdout.
pub fn print_ps(panes: Vec<libslop::PaneInfo>) {
    let now = std::time::SystemTime::now();
    let fmt = timeago::Formatter::new();
    let rows: Vec<(String, String, String, String, String, String, String, String, String)> = panes.iter().map(|p| {
        let epoch = now.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let created = fmt.convert(epoch.saturating_sub(std::time::Duration::from_secs(p.created_at)));
        let last_active = fmt.convert(epoch.saturating_sub(std::time::Duration::from_secs(p.last_active)));
        let session = p.session_id.as_deref().unwrap_or("-").to_string();
        let parent = p.parent_pane_id.as_deref().unwrap_or("-").to_string();
        let tags = if p.tags.is_empty() { "-".to_string() } else { p.tags.join(",") };
        let state = p.state.as_str().to_string();
        let detailed_state = p.detailed_state.as_str().to_string();
        let working_dir = p.working_dir.as_deref().unwrap_or("-").to_string();
        (p.pane_id.clone(), created, last_active, session, parent, tags, state, detailed_state, working_dir)
    }).collect();

    let pane_w          = rows.iter().map(|r| r.0.len()).max().unwrap_or(0).max(4);
    let created_w       = rows.iter().map(|r| r.1.len()).max().unwrap_or(0).max(7);
    let last_active_w   = rows.iter().map(|r| r.2.len()).max().unwrap_or(0).max(11);
    let session_w       = rows.iter().map(|r| r.3.len()).max().unwrap_or(0).max(7);
    let parent_w        = rows.iter().map(|r| r.4.len()).max().unwrap_or(0).max(6);
    let tags_w          = rows.iter().map(|r| r.5.len()).max().unwrap_or(0).max(4);
    let state_w         = rows.iter().map(|r| r.6.len()).max().unwrap_or(0).max(5);
    let detailed_w      = rows.iter().map(|r| r.7.len()).max().unwrap_or(0).max(14);
    let working_dir_w   = rows.iter().map(|r| r.8.len()).max().unwrap_or(0).max(11);

    println!("{:<pane_w$}  {:<created_w$}  {:<last_active_w$}  {:<session_w$}  {:<parent_w$}  {:<tags_w$}  {:<state_w$}  {:<detailed_w$}  {:<working_dir_w$}",
        "PANE", "CREATED", "LAST_ACTIVE", "SESSION", "PARENT", "TAGS", "STATE", "DETAILED_STATE", "WORKING_DIR",
        pane_w=pane_w, created_w=created_w, last_active_w=last_active_w, session_w=session_w,
        parent_w=parent_w, tags_w=tags_w, state_w=state_w, detailed_w=detailed_w, working_dir_w=working_dir_w);

    for (pane_id, created, last_active, session, parent, tags, state, detailed_state, working_dir) in &rows {
        println!("{:<pane_w$}  {:<created_w$}  {:<last_active_w$}  {:<session_w$}  {:<parent_w$}  {:<tags_w$}  {:<state_w$}  {:<detailed_w$}  {:<working_dir_w$}",
            pane_id, created, last_active, session, parent, tags, state, detailed_state, working_dir,
            pane_w=pane_w, created_w=created_w, last_active_w=last_active_w, session_w=session_w,
            parent_w=parent_w, tags_w=tags_w, state_w=state_w, detailed_w=detailed_w, working_dir_w=working_dir_w);
    }
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

    pub async fn tmux_hook(
        &mut self,
        event: String,
        pane_id: Option<String>,
    ) -> Result<(), Error> {
        match self.request(libslop::RequestBody::TmuxHook { event, pane_id }).await? {
            libslop::ResponseBody::TmuxHooked => Ok(()),
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

/// Run the Listen command: build filters, subscribe, print events until SIGTERM or EOF.
/// Consumes the client because subscribe() takes ownership.
pub async fn execute_listen<R, W>(
    client: Client<R, W>,
    hooks: Vec<String>,
    events: Vec<String>,
    transcripts: Vec<String>,
    pane_id: Option<String>,
    session_id: Option<String>,
    replay: Option<u64>,
) -> Result<(), Error>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut subscription = if let Some(last_n) = replay {
        let replay_pane_id = match pane_id {
            Some(ref id) => id.clone(),
            None => {
                eprintln!("error: --replay requires --pane-id");
                std::process::exit(2);
            }
        };
        client.subscribe_transcript(replay_pane_id, last_n).await?
    } else {
        let filters: Vec<libslop::EventFilter> = if hooks.is_empty() && events.is_empty() && transcripts.is_empty() && pane_id.is_none() && session_id.is_none() {
            vec![]
        } else if hooks.is_empty() && events.is_empty() && transcripts.is_empty() {
            vec![libslop::EventFilter {
                source: None,
                event_type: None,
                pane_id,
                session_id,
                payload_match: serde_json::Map::new(),
            }]
        } else {
            let hook_filters = hooks.into_iter().map(|h| libslop::EventFilter {
                source: Some("hook".to_string()),
                event_type: Some(h),
                pane_id: pane_id.clone(),
                session_id: session_id.clone(),
                payload_match: serde_json::Map::new(),
            });
            let event_filters = events.into_iter().map(|e| libslop::EventFilter {
                source: Some("slopd".to_string()),
                event_type: Some(e),
                pane_id: pane_id.clone(),
                session_id: None,
                payload_match: serde_json::Map::new(),
            });
            let transcript_filters = transcripts.into_iter().map(|t| libslop::EventFilter {
                source: Some("transcript".to_string()),
                event_type: Some(t),
                pane_id: pane_id.clone(),
                session_id: session_id.clone(),
                payload_match: serde_json::Map::new(),
            });
            hook_filters.chain(event_filters).chain(transcript_filters).collect()
        };
        client.subscribe(filters).await?
    };

    println!("{{\"subscribed\":true}}");

    let mut sigterm = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::terminate(),
    ).expect("failed to install SIGTERM handler");

    loop {
        tokio::select! {
            _ = sigterm.recv() => break,
            result = subscription.next() => {
                match result {
                    Ok(Some(SubscriptionItem::Record(record))) => {
                        println!("{}", serde_json::to_string(&record).unwrap());
                    }
                    Ok(Some(SubscriptionItem::Subscribed)) => {}
                    Ok(None) => break,
                    Err(e) => return Err(e),
                }
            }
        }
    }
    Ok(())
}

/// Execute a CommonCommand against the given client.
/// Listen must be handled separately before calling this (it consumes the client).
pub async fn execute_command<R, W>(
    client: &mut Client<R, W>,
    command: CommonCommand,
    ctx: &CommandContext,
) -> Result<(), Error>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    // Handle Send-with-filter first.
    if let CommonCommand::Send { ref pane_id, .. } = command {
        if pane_id.contains('=') {
            if let CommonCommand::Send { pane_id, prompt, filters, select, timeout, interrupt } = command {
                let mut all_filters = vec![pane_id];
                all_filters.extend(filters);
                let parsed = parse_filters(all_filters)?;
                let pane_ids = client.send_filtered(&parsed, &prompt, &select, timeout, interrupt).await?;
                for id in pane_ids {
                    println!("{}", id);
                }
                return Ok(());
            }
        }
    }

    match command {
        CommonCommand::Status => {
            let state = client.status().await?;
            println!("uptime: {}s", state.uptime_secs);
        }
        CommonCommand::Ps { filters, json } => {
            let parsed = parse_filters(filters)?;
            let all_panes = client.ps().await?;
            let panes = apply_filters(all_panes, &parsed);
            if json {
                println!("{}", serde_json::to_string(&panes).unwrap());
            } else {
                print_ps(panes);
            }
        }
        CommonCommand::Run { extra_args, start_directory } => {
            let pane_id = client.run(ctx.parent_pane_id.clone(), extra_args, start_directory).await?;
            println!("{}", pane_id);
        }
        CommonCommand::Kill { pane_id } => {
            let pane_id = client.kill(pane_id).await?;
            println!("{}", pane_id);
        }
        CommonCommand::Send { pane_id, prompt, timeout, interrupt, .. } => {
            let pane_id = client.send_prompt(pane_id, prompt, timeout, interrupt).await?;
            println!("{}", pane_id);
        }
        CommonCommand::Interrupt { pane_id } => {
            let pane_id = client.interrupt(pane_id).await?;
            println!("{}", pane_id);
        }
        CommonCommand::Tag { pane_id, tag } => {
            let (pane_id, tag) = client.tag(pane_id, tag).await?;
            println!("{} {}", pane_id, tag);
        }
        CommonCommand::Untag { pane_id, tag } => {
            let (pane_id, tag) = client.untag(pane_id, tag).await?;
            println!("{} {}", pane_id, tag);
        }
        CommonCommand::Tags { pane_id } => {
            let pane_id = pane_id.or(ctx.fallback_pane_id.clone()).unwrap();
            let tags = client.tags(pane_id).await?;
            for tag in tags {
                println!("{}", tag);
            }
        }
        CommonCommand::Transcript { pane_id, before, limit } => {
            let records = client.read_transcript(pane_id, before, limit).await?;
            let out = serde_json::json!({ "records": records });
            println!("{}", out);
        }
        CommonCommand::Listen { .. } => unreachable!("Listen must be handled before calling execute_command"),
    }
    Ok(())
}
