use std::path::{Path, PathBuf};
use std::time::Duration;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};
use serde_json::{Map, Value, json};
use tokio::net::UnixStream;

#[derive(Clone)]
pub struct SlopdMcp {
    socket: PathBuf,
}

impl SlopdMcp {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    pub fn tools(&self) -> Vec<Tool> {
        crate::tools::all()
    }

    async fn dispatch(&self, request: CallToolRequestParams) -> Result<CallToolResult, McpError> {
        match request.name.as_ref() {
            "status" => self.status().await,
            "ps" => self.ps(request.arguments.as_ref()).await,
            "fork" => self.fork(request.arguments.as_ref()).await,
            "kill" => self.kill(request.arguments.as_ref()).await,
            "transcript" => self.transcript(request.arguments.as_ref()).await,
            "send" => self.send(request.arguments.as_ref()).await,
            "interrupt" => self.interrupt(request.arguments.as_ref()).await,
            "listen" => self.listen(request.arguments.as_ref()).await,
            "wait" => self.wait(request.arguments.as_ref()).await,
            "tag" => self.tag(request.arguments.as_ref()).await,
            "untag" => self.untag(request.arguments.as_ref()).await,
            "tags" => self.tags(request.arguments.as_ref()).await,
            "backup" => self.backup().await,
            "restore" => self.restore().await,
            "graveyard" => self.graveyard(request.arguments.as_ref()).await,
            "revive" => self.revive(request.arguments.as_ref()).await,
            "run" => self.run(request.arguments.as_ref()).await,
            name => Err(McpError::invalid_params(
                format!("unknown tool {name}"),
                None,
            )),
        }
    }

    async fn status(&self) -> Result<CallToolResult, McpError> {
        let mut client = connect(&self.socket).await?;
        let state = client.status().await.map_err(slopd_error)?;
        ok_json(json!({
            "uptime_secs": state.uptime_secs,
            "subscriber_count": state.subscriber_count,
            "config_generation": state.config_generation,
            "pending_restore": state.pending_restore,
        }))
    }

    async fn ps(&self, arguments: Option<&Map<String, Value>>) -> Result<CallToolResult, McpError> {
        let filters = pane_filters(arguments)?;
        let mut client = connect(&self.socket).await?;
        let panes = client.ps().await.map_err(slopd_error)?;
        let panes = libslopctl::apply_filters(panes, &filters);
        ok_json(json!({ "panes": panes }))
    }

    async fn fork(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let source_pane_id = required_string(arguments, "pane_id")?;
        let spawn = spawn_arguments(arguments)?;
        let mut client = connect(&self.socket).await?;
        let mut subscription = if spawn.no_wait {
            None
        } else {
            Some(
                client
                    .subscribe(ready_event_filters())
                    .await
                    .map_err(slopd_error)?,
            )
        };
        let (pane_id, session_id) = client
            .fork(
                source_pane_id,
                spawn.start_directory,
                spawn.env,
                spawn.extra_args,
            )
            .await
            .map_err(slopd_error)?;
        if let Some(subscription) = subscription.as_mut()
            && let Err(message) = wait_pane_ready(subscription, &pane_id, spawn.ready_timeout).await
        {
            return tool_json_error(json!({
                "pane_id": pane_id,
                "session_id": session_id,
                "ready": false,
                "error": message,
            }));
        }
        ok_json(json!({
            "pane_id": pane_id,
            "session_id": session_id,
            "ready": !spawn.no_wait,
        }))
    }

    async fn kill(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let pane_id = required_string(arguments, "pane_id")?;
        let mut client = connect(&self.socket).await?;
        let pane_id = client.kill(pane_id).await.map_err(slopd_error)?;
        ok_json(json!({ "pane_id": pane_id }))
    }

    async fn transcript(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let pane_id = required_string(arguments, "pane_id")?;
        let before = optional_u64(arguments, "before")?;
        let limit = optional_u64(arguments, "limit")?
            .unwrap_or(50)
            .clamp(1, 500);
        let mut client = connect(&self.socket).await?;
        let records = client
            .read_transcript(pane_id, before, limit)
            .await
            .map_err(slopd_error)?;
        ok_json(json!({ "records": records }))
    }

    async fn send(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let prompt = required_string(arguments, "prompt")?;
        if prompt.trim().is_empty() {
            return tool_error("prompt must not be empty");
        }
        let interrupt = optional_bool(arguments, "interrupt")?.unwrap_or(false);
        let timeout = optional_u64(arguments, "timeout")?
            .unwrap_or(60)
            .clamp(1, 300);
        let select = parse_select(optional_string(arguments, "select").as_deref())?;
        let pane_id = optional_string(arguments, "pane_id");
        let filters = pane_filters(arguments)?;
        if pane_id.is_none() && filters.is_empty() {
            return tool_error("send requires pane_id or at least one of tag, backend, account");
        }

        let mut client = connect(&self.socket).await?;
        let pane_ids = if filters.is_empty() {
            let pane_id = pane_id.expect("pane_id present when filters empty");
            vec![
                client
                    .send_prompt(pane_id, prompt, timeout, interrupt)
                    .await
                    .map_err(slopd_error)?,
            ]
        } else {
            if let Some(pane_id) = pane_id {
                client
                    .send_prompt(pane_id, prompt, timeout, interrupt)
                    .await
                    .map_err(slopd_error)
                    .map(|id| vec![id])?
            } else {
                client
                    .send_filtered(&filters, &prompt, &select, timeout, interrupt)
                    .await
                    .map_err(slopd_error)?
            }
        };
        ok_json(json!({ "pane_ids": pane_ids }))
    }

    async fn interrupt(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let pane_id = required_string(arguments, "pane_id")?;
        let mut client = connect(&self.socket).await?;
        let pane_id = client.interrupt(pane_id).await.map_err(slopd_error)?;
        ok_json(json!({ "pane_id": pane_id }))
    }

    async fn listen(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let events = event_arguments(arguments)?;
        let replay = optional_u64(arguments, "replay")?;
        let limit = optional_u64(arguments, "limit")?
            .unwrap_or(50)
            .clamp(1, 500) as usize;
        let where_parsed = libslopctl::parse_payload_predicates(events.where_preds.clone())
            .map_err(slopd_error)?;
        let (pane_id, session_id) = libslopctl::resolve_pane_id_or_session(
            events.pane_id.clone(),
            events.session_id.clone(),
        )
        .map_err(slopd_error)?;
        let mut client = connect(&self.socket).await?;
        let mut subscription = if let Some(last_n) = replay {
            if !where_parsed.is_empty() {
                return Err(McpError::invalid_params(
                    "where is incompatible with replay",
                    None,
                ));
            }
            let pane_id =
                pane_id.ok_or_else(|| McpError::invalid_params("replay requires pane_id", None))?;
            client
                .subscribe_transcript(pane_id, last_n)
                .await
                .map_err(slopd_error)?
        } else {
            let filters = libslopctl::build_listen_filters(
                events.hooks,
                events.events,
                events.transcripts,
                pane_id,
                session_id,
                where_parsed,
            );
            client.subscribe(filters).await.map_err(slopd_error)?
        };

        let deadline = (events.timeout != 0)
            .then(|| tokio::time::Instant::now() + Duration::from_secs(events.timeout));
        let mut records = Vec::new();
        let mut timed_out = false;
        while records.len() < limit {
            let next = async { subscription.next().await.map_err(slopd_error) };
            let item = if let Some(deadline) = deadline {
                match tokio::time::timeout_at(deadline, next).await {
                    Ok(result) => result?,
                    Err(_) => {
                        timed_out = true;
                        break;
                    }
                }
            } else {
                next.await?
            };
            match item {
                Some(libslopctl::SubscriptionItem::Record(record)) => records.push(record),
                Some(libslopctl::SubscriptionItem::Subscribed) => {}
                None => break,
            }
        }
        ok_json(json!({ "records": records, "timed_out": timed_out }))
    }

    async fn wait(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let events = event_arguments(arguments)?;
        let until =
            libslopctl::parse_payload_predicates(optional_string_array(arguments, "until")?)
                .map_err(slopd_error)?;
        let where_parsed = libslopctl::parse_payload_predicates(events.where_preds.clone())
            .map_err(slopd_error)?;
        let (pane_id, session_id) = libslopctl::resolve_pane_id_or_session(
            events.pane_id.clone(),
            events.session_id.clone(),
        )
        .map_err(slopd_error)?;
        let filters = libslopctl::build_listen_filters(
            events.hooks.clone(),
            events.events.clone(),
            events.transcripts.clone(),
            pane_id.clone(),
            session_id.clone(),
            where_parsed.clone(),
        );
        let mut client = connect(&self.socket).await?;
        let mut subscription = client.subscribe(filters).await.map_err(slopd_error)?;

        if !optional_bool(arguments, "no_snapshot")?.unwrap_or(false)
            && (pane_id.is_some() || session_id.is_some())
            && events.hooks.is_empty()
            && events.transcripts.is_empty()
            && state_events_requested(&events.events)
        {
            let panes = client.ps().await.map_err(slopd_error)?;
            if let Some(pane) = panes.iter().find(|pane| {
                pane_id
                    .as_ref()
                    .is_none_or(|expected| pane.pane_id == *expected)
                    && session_id.as_ref().is_none_or(|expected| {
                        pane.session_id.as_deref() == Some(expected.as_str())
                    })
            }) {
                let record = current_state_record(pane);
                if libslop::predicates_match(&record.payload, &where_parsed)
                    && libslop::predicates_match(&record.payload, &until)
                {
                    return ok_json(json!({ "record": record, "snapshot": true }));
                }
            }
        }

        let wait_for_match = async {
            loop {
                match subscription.next().await.map_err(slopd_error)? {
                    Some(libslopctl::SubscriptionItem::Record(record)) => {
                        if libslop::predicates_match(&record.payload, &until) {
                            return Ok::<_, McpError>(record);
                        }
                    }
                    Some(libslopctl::SubscriptionItem::Subscribed) => {}
                    None => {
                        return Err(McpError::internal_error("event subscription closed", None));
                    }
                }
            }
        };
        let record = if events.timeout == 0 {
            wait_for_match.await?
        } else {
            match tokio::time::timeout(Duration::from_secs(events.timeout), wait_for_match).await {
                Ok(result) => result?,
                Err(_) => {
                    return tool_error(format!(
                        "timed out after {}s waiting for a matching event",
                        events.timeout
                    ));
                }
            }
        };
        ok_json(json!({ "record": record, "snapshot": false }))
    }

    async fn tag(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        self.change_tag(arguments, false).await
    }

    async fn untag(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        self.change_tag(arguments, true).await
    }

    async fn change_tag(
        &self,
        arguments: Option<&Map<String, Value>>,
        remove: bool,
    ) -> Result<CallToolResult, McpError> {
        let pane_id = required_string(arguments, "pane_id")?;
        let tag = required_string(arguments, "tag")?;
        let mut client = connect(&self.socket).await?;
        let (pane_id, tag) = if remove {
            client.untag(pane_id, tag).await
        } else {
            client.tag(pane_id, tag).await
        }
        .map_err(slopd_error)?;
        ok_json(json!({ "pane_id": pane_id, "tag": tag }))
    }

    async fn tags(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let pane_id = required_string(arguments, "pane_id")?;
        let mut client = connect(&self.socket).await?;
        let tags = client.tags(pane_id.clone()).await.map_err(slopd_error)?;
        ok_json(json!({ "pane_id": pane_id, "tags": tags }))
    }

    async fn backup(&self) -> Result<CallToolResult, McpError> {
        let mut client = connect(&self.socket).await?;
        let count = client.backup().await.map_err(slopd_error)?;
        ok_json(json!({ "count": count }))
    }

    async fn restore(&self) -> Result<CallToolResult, McpError> {
        let mut client = connect(&self.socket).await?;
        let restored = client.restore().await.map_err(slopd_error)?;
        ok_json(json!({ "restored": restored }))
    }

    async fn graveyard(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let boot = optional_i32(arguments, "boot")?;
        let limit = optional_u64(arguments, "limit")?
            .unwrap_or(50)
            .clamp(1, 500) as usize;
        let mut client = connect(&self.socket).await?;
        let entries = client.graveyard(boot, limit).await.map_err(slopd_error)?;
        ok_json(json!({ "entries": entries }))
    }

    async fn revive(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let target = optional_string(arguments, "target");
        let boot = optional_i32(arguments, "boot")?;
        let env = environment(arguments)?;
        let mut client = connect(&self.socket).await?;
        let (pane_id, grave_id) = client
            .revive(target, boot, env)
            .await
            .map_err(slopd_error)?;
        ok_json(json!({ "pane_id": pane_id, "grave_id": grave_id }))
    }

    async fn run(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let spawn = spawn_arguments(arguments)?;
        let parent_pane_id = optional_string(arguments, "parent_pane_id");
        let account = optional_string(arguments, "account");
        let backend = match optional_string(arguments, "backend") {
            Some(name) => Some(
                parse_backend(&name).map_err(|message| McpError::invalid_params(message, None))?,
            ),
            None => None,
        };
        let mut client = connect(&self.socket).await?;
        let mut subscription = if spawn.no_wait {
            None
        } else {
            Some(
                client
                    .subscribe(ready_event_filters())
                    .await
                    .map_err(slopd_error)?,
            )
        };
        let pane_id = client
            .run(
                parent_pane_id,
                spawn.extra_args,
                spawn.start_directory,
                spawn.env,
                account,
                backend,
            )
            .await
            .map_err(slopd_error)?;
        if let Some(subscription) = subscription.as_mut()
            && let Err(message) = wait_pane_ready(subscription, &pane_id, spawn.ready_timeout).await
        {
            return tool_json_error(json!({
                "pane_id": pane_id,
                "ready": false,
                "error": message,
            }));
        }
        ok_json(json!({ "pane_id": pane_id, "ready": !spawn.no_wait }))
    }
}

impl ServerHandler for SlopdMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "slopd-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Supervisor for slopd-managed agent panes. Call ps to find a pane, send to submit a prompt, then transcript to read the answer. send returns when slopd accepts the prompt, not when the agent finishes. interrupt stops an in-flight turn.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.tools()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools().into_iter().find(|tool| tool.name == name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.dispatch(request).await.map(CallToolResponse::Complete)
    }
}

async fn connect(
    socket: &Path,
) -> Result<
    libslopctl::Client<tokio::net::unix::OwnedReadHalf, tokio::net::unix::OwnedWriteHalf>,
    McpError,
> {
    let stream = UnixStream::connect(socket).await.map_err(|error| {
        McpError::internal_error(
            format!("failed to connect to {}: {error}", socket.display()),
            None,
        )
    })?;
    let (reader, writer) = stream.into_split();
    Ok(libslopctl::Client::new(reader, writer))
}

struct SpawnArguments {
    start_directory: Option<PathBuf>,
    env: Vec<(String, String)>,
    extra_args: Vec<String>,
    no_wait: bool,
    ready_timeout: u64,
}

fn spawn_arguments(arguments: Option<&Map<String, Value>>) -> Result<SpawnArguments, McpError> {
    let start_directory = optional_string(arguments, "start_directory").map(PathBuf::from);
    if let Some(path) = start_directory.as_ref() {
        let raw = path.to_string_lossy();
        if !path.is_absolute() && !raw.starts_with('~') && !raw.contains('$') {
            return Err(McpError::invalid_params(
                "start_directory must be absolute or start with ~ or $VAR",
                None,
            ));
        }
    }
    Ok(SpawnArguments {
        start_directory,
        env: environment(arguments)?,
        extra_args: optional_string_array(arguments, "extra_args")?,
        no_wait: optional_bool(arguments, "no_wait")?.unwrap_or(false),
        ready_timeout: optional_u64(arguments, "ready_timeout")?
            .unwrap_or(30)
            .clamp(1, 300),
    })
}

fn environment(arguments: Option<&Map<String, Value>>) -> Result<Vec<(String, String)>, McpError> {
    let env = optional_string_array(arguments, "env")?;
    let env_files = optional_string_array(arguments, "env_files")?
        .into_iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    libslopctl::build_cli_env(&env_files, &env).map_err(slopd_error)
}

struct EventArguments {
    hooks: Vec<String>,
    events: Vec<String>,
    transcripts: Vec<String>,
    pane_id: Option<String>,
    session_id: Option<String>,
    where_preds: Vec<String>,
    timeout: u64,
}

fn event_arguments(arguments: Option<&Map<String, Value>>) -> Result<EventArguments, McpError> {
    Ok(EventArguments {
        hooks: optional_string_array(arguments, "hooks")?,
        events: optional_string_array(arguments, "events")?,
        transcripts: optional_string_array(arguments, "transcripts")?,
        pane_id: optional_string(arguments, "pane_id"),
        session_id: optional_string(arguments, "session_id"),
        where_preds: optional_string_array(arguments, "where")?,
        timeout: optional_u64(arguments, "timeout")?.unwrap_or(60).min(300),
    })
}

fn ready_event_filters() -> Vec<libslop::EventFilter> {
    vec![
        libslop::EventFilter {
            source: Some("slopd".into()),
            event_type: Some("DetailedStateChange".into()),
            ..Default::default()
        },
        libslop::EventFilter {
            source: Some("slopd".into()),
            event_type: Some("PaneDestroyed".into()),
            ..Default::default()
        },
        libslop::EventFilter {
            source: Some("hook".into()),
            event_type: Some("SessionEnd".into()),
            ..Default::default()
        },
    ]
}

async fn wait_pane_ready(
    subscription: &mut libslopctl::Subscription,
    pane_id: &str,
    timeout_secs: u64,
) -> Result<(), String> {
    let overall_deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    let mut settle_deadline = None;
    loop {
        let deadline = settle_deadline.unwrap_or(overall_deadline);
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return if settle_deadline.is_some() {
                Ok(())
            } else {
                Err(format!(
                    "timed out after {timeout_secs}s waiting for pane {pane_id} to become ready"
                ))
            };
        }
        match tokio::time::timeout(remaining, subscription.next()).await {
            Err(_) => {}
            Ok(Ok(Some(libslopctl::SubscriptionItem::Record(record)))) => {
                if record.pane_id.as_deref() != Some(pane_id) {
                    continue;
                }
                match (record.source.as_str(), record.event_type.as_str()) {
                    ("hook", "SessionEnd") => {
                        let reason = record
                            .payload
                            .get("reason")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown reason");
                        return Err(format!("pane {pane_id} ended before ready: {reason}"));
                    }
                    ("slopd", "PaneDestroyed") => {
                        let status = record
                            .payload
                            .get("exit_status")
                            .and_then(Value::as_i64)
                            .map(|value| format!(" with exit status {value}"))
                            .unwrap_or_default();
                        return Err(format!("pane {pane_id} died before ready{status}"));
                    }
                    ("slopd", "DetailedStateChange") => {
                        let live = record
                            .payload
                            .get("detailed_state")
                            .and_then(Value::as_str)
                            .is_some_and(|state| {
                                state != libslop::PaneDetailedState::BootingUp.as_str()
                            });
                        if live && settle_deadline.is_none() {
                            settle_deadline =
                                Some(tokio::time::Instant::now() + Duration::from_secs(3));
                        }
                    }
                    _ => {}
                }
            }
            Ok(Ok(Some(libslopctl::SubscriptionItem::Subscribed))) => {}
            Ok(Ok(None)) | Ok(Err(_)) => {
                return Err(format!(
                    "lost connection while waiting for pane {pane_id} to become ready"
                ));
            }
        }
    }
}

fn state_events_requested(events: &[String]) -> bool {
    events.is_empty()
        || events.iter().any(|event| {
            matches!(
                event.as_str(),
                "CurrentState" | "StateChange" | "DetailedStateChange"
            )
        })
}

fn current_state_record(pane: &libslop::PaneInfo) -> libslop::Record {
    libslop::Record {
        cursor: None,
        source: "slopd".into(),
        event_type: "CurrentState".into(),
        pane_id: Some(pane.pane_id.clone()),
        payload: json!({
            "state": pane.state.as_str(),
            "detailed_state": pane.detailed_state.as_str(),
            "session_id": pane.session_id,
            "seeded_current": true,
        }),
    }
}

fn pane_filters(arguments: Option<&Map<String, Value>>) -> Result<Vec<(String, String)>, McpError> {
    let mut filters = Vec::new();
    if let Some(tag) = optional_string(arguments, "tag") {
        filters.push(("tag".into(), tag));
    }
    if let Some(backend) = optional_string(arguments, "backend") {
        parse_backend(&backend).map_err(|message| McpError::invalid_params(message, None))?;
        filters.push(("backend".into(), backend));
    }
    if let Some(account) = optional_string(arguments, "account") {
        filters.push(("account".into(), account));
    }
    Ok(filters)
}

pub fn parse_backend(name: &str) -> Result<libslop::Backend, String> {
    match name {
        "claude" => Ok(libslop::Backend::Claude),
        "opencode" => Ok(libslop::Backend::Opencode),
        "codex" => Ok(libslop::Backend::Codex),
        "grok" => Ok(libslop::Backend::Grok),
        other => Err(format!(
            "unknown backend {other:?}; expected claude, opencode, codex, or grok"
        )),
    }
}

fn optional_string(arguments: Option<&Map<String, Value>>, key: &str) -> Option<String> {
    arguments?
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.is_empty())
}

fn required_string(arguments: Option<&Map<String, Value>>, key: &str) -> Result<String, McpError> {
    optional_string(arguments, key)
        .ok_or_else(|| McpError::invalid_params(format!("{key} is required"), None))
}

fn optional_bool(
    arguments: Option<&Map<String, Value>>,
    key: &str,
) -> Result<Option<bool>, McpError> {
    match arguments.and_then(|args| args.get(key)) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(McpError::invalid_params(
            format!("{key} must be a boolean"),
            None,
        )),
    }
}

fn optional_u64(
    arguments: Option<&Map<String, Value>>,
    key: &str,
) -> Result<Option<u64>, McpError> {
    match arguments.and_then(|args| args.get(key)) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .ok_or_else(|| {
                McpError::invalid_params(format!("{key} must be a non-negative integer"), None)
            })
            .map(Some),
        Some(_) => Err(McpError::invalid_params(
            format!("{key} must be a non-negative integer"),
            None,
        )),
    }
}

fn optional_i32(
    arguments: Option<&Map<String, Value>>,
    key: &str,
) -> Result<Option<i32>, McpError> {
    match arguments.and_then(|args| args.get(key)) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
            .ok_or_else(|| {
                McpError::invalid_params(format!("{key} must be a 32-bit integer"), None)
            })
            .map(Some),
        Some(_) => Err(McpError::invalid_params(
            format!("{key} must be an integer"),
            None,
        )),
    }
}

fn parse_select(value: Option<&str>) -> Result<libslopctl::SelectMode, McpError> {
    match value.unwrap_or("one") {
        "one" => Ok(libslopctl::SelectMode::One),
        "any" => Ok(libslopctl::SelectMode::Any),
        "all" => Ok(libslopctl::SelectMode::All),
        other => Err(McpError::invalid_params(
            format!("unknown select mode {other:?}; expected one, any, or all"),
            None,
        )),
    }
}

fn optional_string_array(
    arguments: Option<&Map<String, Value>>,
    key: &str,
) -> Result<Vec<String>, McpError> {
    match arguments.and_then(|args| args.get(key)) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_string).ok_or_else(|| {
                    McpError::invalid_params(format!("{key} items must be strings"), None)
                })
            })
            .collect(),
        Some(_) => Err(McpError::invalid_params(
            format!("{key} must be an array of strings"),
            None,
        )),
    }
}

fn slopd_error(error: libslopctl::Error) -> McpError {
    match error {
        libslopctl::Error::SelectError(message) | libslopctl::Error::FilterError(message) => {
            McpError::invalid_params(message, None)
        }
        other => McpError::internal_error(other.to_string(), None),
    }
}

fn ok_json(value: Value) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::text(
        serde_json::to_string(&value).unwrap_or_else(|_| value.to_string()),
    )]))
}

fn tool_error(message: impl Into<String>) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::error(vec![ContentBlock::text(
        message.into(),
    )]))
}

fn tool_json_error(value: Value) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::error(vec![ContentBlock::text(
        serde_json::to_string(&value).unwrap_or_else(|_| value.to_string()),
    )]))
}

#[cfg(test)]
mod tests {
    use super::parse_backend;

    #[test]
    fn parse_backend_accepts_canonical_names() {
        assert_eq!(parse_backend("grok").unwrap(), libslop::Backend::Grok);
        assert!(parse_backend("claude-code").is_err());
    }
}
