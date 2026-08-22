use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, ListToolsResult,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};
use serde_json::{Map, Value, json};
use tokio::net::UnixStream;

const MCP_PANE_TAG: &str = "slopd-mcp";
const MAILBOX_LIMIT: usize = 100;
const MAILBOX_WORKER_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Default)]
struct Mailbox {
    entries: Arc<Mutex<VecDeque<Arc<MailboxEntry>>>>,
}

struct MailboxEntry {
    request_id: String,
    pane_id: String,
    prompt: String,
    created_at_unix_ms: u64,
    state: Mutex<MailboxState>,
}

#[derive(Clone)]
enum MailboxState {
    Pending,
    Completed(String),
    Failed(String),
}

impl Mailbox {
    fn insert(&self, entry: Arc<MailboxEntry>) {
        let mut entries = self.entries.lock().unwrap_or_else(|lock| lock.into_inner());
        while entries.len() >= MAILBOX_LIMIT {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    fn get(&self, request_id: &str) -> Option<Arc<MailboxEntry>> {
        self.entries
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .iter()
            .find(|entry| entry.request_id == request_id)
            .cloned()
    }

    fn recent(&self, pane_id: Option<&str>, limit: usize) -> Vec<Arc<MailboxEntry>> {
        self.entries
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .iter()
            .rev()
            .filter(|entry| pane_id.is_none_or(|pane_id| entry.pane_id == pane_id))
            .take(limit)
            .cloned()
            .collect()
    }

    fn recent_pane_ids(&self) -> Vec<String> {
        let entries = self.entries.lock().unwrap_or_else(|lock| lock.into_inner());
        let mut pane_ids = Vec::new();
        for entry in entries.iter().rev() {
            if !pane_ids.contains(&entry.pane_id) {
                pane_ids.push(entry.pane_id.clone());
            }
        }
        pane_ids
    }
}

impl MailboxEntry {
    fn new(pane_id: String, prompt: String) -> Self {
        let created_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Self {
            request_id: uuid::Uuid::new_v4().to_string(),
            pane_id,
            prompt,
            created_at_unix_ms,
            state: Mutex::new(MailboxState::Pending),
        }
    }

    fn state(&self) -> MailboxState {
        self.state
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .clone()
    }

    fn finish(&self, state: MailboxState) {
        *self.state.lock().unwrap_or_else(|lock| lock.into_inner()) = state;
    }

    fn json(&self) -> Value {
        let (status, reply, error) = match self.state() {
            MailboxState::Pending => ("pending", Value::Null, Value::Null),
            MailboxState::Completed(reply) => ("completed", json!(reply), Value::Null),
            MailboxState::Failed(error) => ("failed", Value::Null, json!(error)),
        };
        json!({
            "request_id": self.request_id,
            "pane_id": self.pane_id,
            "prompt": self.prompt,
            "created_at_unix_ms": self.created_at_unix_ms,
            "status": status,
            "reply": reply,
            "error": error,
        })
    }
}

#[derive(Clone)]
pub struct SlopdMcp {
    socket: PathBuf,
    mailbox: Mailbox,
    spawn_lock: Arc<tokio::sync::Mutex<()>>,
}

impl SlopdMcp {
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            mailbox: Mailbox::default(),
            spawn_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub fn tools(&self) -> Vec<Tool> {
        crate::tools::all()
    }

    async fn dispatch(&self, request: CallToolRequestParams) -> Result<CallToolResult, McpError> {
        let name = canonical_tool_name(request.name.as_ref());
        if matches!(name, "collect_events" | "wait_for_event")
            && !optional_bool(request.arguments.as_ref(), "advanced")?.unwrap_or(false)
        {
            return Err(invalid_argument(format!(
                "{name} is an advanced tool; pass advanced=true to use raw event access"
            )));
        }
        match name {
            "get_status" => self.status().await,
            "list_panes" => self.ps(request.arguments.as_ref()).await,
            "fork_pane" => self.fork(request.arguments.as_ref()).await,
            "kill_pane" => self.kill(request.arguments.as_ref()).await,
            "read_transcript" => self.transcript(request.arguments.as_ref()).await,
            "ask_agent" => self.ask_agent(request.arguments.as_ref()).await,
            "read_mailbox" => self.read_mailbox(request.arguments.as_ref()).await,
            "send_prompt" => self.send(request.arguments.as_ref()).await,
            "wait_for_reply" => self.wait_for_reply(request.arguments.as_ref()).await,
            "interrupt_pane" => self.interrupt(request.arguments.as_ref()).await,
            "collect_events" => self.listen(request.arguments.as_ref()).await,
            "wait_for_event" => self.wait(request.arguments.as_ref()).await,
            "add_tag" => self.tag(request.arguments.as_ref()).await,
            "remove_tag" => self.untag(request.arguments.as_ref()).await,
            "list_tags" => self.tags(request.arguments.as_ref()).await,
            "create_backup" => self.backup().await,
            "restore_backup" => self.restore().await,
            "list_dead_panes" => self.graveyard(request.arguments.as_ref()).await,
            "revive_pane" => self.revive(request.arguments.as_ref()).await,
            "create_pane" => self.run(request.arguments.as_ref()).await,
            name => Err(invalid_argument(format!("unknown tool {name}"))),
        }
    }

    async fn status(&self) -> Result<CallToolResult, McpError> {
        let mut client = connect(&self.socket).await?;
        let result = client.status().await;
        let state = self.slopd_result(result).await?;
        ok_json(json!({
            "uptime_secs": state.uptime_secs,
            "subscriber_count": state.subscriber_count,
            "config_generation": state.config_generation,
            "pending_restore": state.pending_restore,
        }))
    }

    async fn ps(&self, arguments: Option<&Map<String, Value>>) -> Result<CallToolResult, McpError> {
        let filters = pane_filters(arguments)?;
        let raw = optional_bool(arguments, "raw")?.unwrap_or(false);
        let mut client = connect(&self.socket).await?;
        let result = client.ps().await;
        let panes = self.slopd_result(result).await?;
        let panes = libslopctl::apply_filters(panes, &filters);
        let count = panes.len();
        let state_counts = json!({
            "busy": panes.iter().filter(|pane| pane.state == libslop::PaneState::Busy).count(),
            "ready": panes.iter().filter(|pane| pane.state == libslop::PaneState::Ready).count(),
            "awaiting_input": panes.iter().filter(|pane| pane.state == libslop::PaneState::AwaitingInput).count(),
            "booting_up": panes.iter().filter(|pane| pane.state == libslop::PaneState::BootingUp).count(),
        });
        let panes = if raw {
            serde_json::to_value(panes).unwrap_or_default()
        } else {
            Value::Array(panes.iter().map(compact_pane).collect())
        };
        ok_json(json!({ "count": count, "state_counts": state_counts, "panes": panes }))
    }

    async fn fork(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let source_pane_id = self.required_pane_id(arguments, "pane_id").await?;
        let spawn = spawn_arguments(arguments)?;
        let mut client = connect(&self.socket).await?;
        let mut subscription = if spawn.no_wait {
            None
        } else {
            let result = client.subscribe(ready_event_filters()).await;
            Some(self.slopd_result(result).await?)
        };
        let result = client
            .fork(
                source_pane_id,
                spawn.start_directory,
                spawn.env,
                spawn.extra_args,
            )
            .await;
        let (pane_id, session_id) = self.slopd_result(result).await?;
        let result = client.tag(pane_id.clone(), MCP_PANE_TAG.into()).await;
        self.slopd_result(result).await?;
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
        let pane_id = self.required_pane_id(arguments, "pane_id").await?;
        let mut client = connect(&self.socket).await?;
        let result = client.kill(pane_id).await;
        let pane_id = self.slopd_result(result).await?;
        ok_json(json!({ "pane_id": pane_id }))
    }

    async fn transcript(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let pane_id = self.required_pane_id(arguments, "pane_id").await?;
        let advanced = optional_bool(arguments, "advanced")?.unwrap_or(false);
        if !advanced
            && arguments
                .is_some_and(|values| values.contains_key("before") || values.contains_key("raw"))
        {
            return Err(invalid_argument(
                "read_transcript only accepts pane_id and limit by default; pass advanced=true for cursors and raw records",
            ));
        }
        let limit = optional_u64(arguments, "limit")?.unwrap_or(if advanced { 50 } else { 20 });
        let mut client = connect(&self.socket).await?;
        let result = if advanced {
            client
                .read_transcript(
                    pane_id,
                    optional_u64(arguments, "before")?,
                    limit.clamp(1, 500),
                )
                .await
        } else {
            client.read_transcript(pane_id, None, 500).await
        };
        let records = self.slopd_result(result).await?;
        let records = if advanced {
            if optional_bool(arguments, "raw")?.unwrap_or(true) {
                serde_json::to_value(records).unwrap_or_default()
            } else {
                Value::Array(records.iter().map(compact_record).collect())
            }
        } else {
            let mut records = records.iter().filter_map(simple_record).collect::<Vec<_>>();
            let keep = limit.clamp(1, 100) as usize;
            if records.len() > keep {
                records.drain(..records.len() - keep);
            }
            Value::Array(records)
        };
        let count = records.as_array().map_or(0, Vec::len);
        ok_json(json!({ "count": count, "records": records }))
    }

    async fn ask_agent(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let pane_id = self.resolve_agent_pane(arguments).await?;
        let prompt = required_string(arguments, "prompt")?;
        if prompt.trim().is_empty() {
            return tool_error("prompt must not be empty");
        }
        let wait_seconds = optional_u64(arguments, "wait_seconds")?
            .unwrap_or(45)
            .min(300);
        let interrupt = optional_bool(arguments, "interrupt")?.unwrap_or(false);

        let mut client = connect(&self.socket).await?;
        let result = client.subscribe(reply_event_filters(&pane_id)).await;
        let subscription = self.slopd_result(result).await?;
        let result = client.read_transcript(pane_id.clone(), None, 500).await;
        let records = self.slopd_result(result).await?;
        let after_cursor = records
            .iter()
            .filter_map(|record| record.cursor)
            .max()
            .unwrap_or(0);
        let result = client
            .send_prompt(pane_id.clone(), prompt.clone(), 60, interrupt)
            .await;
        self.slopd_result(result).await?;

        let entry = Arc::new(MailboxEntry::new(pane_id.clone(), prompt.clone()));
        self.mailbox.insert(Arc::clone(&entry));
        let worker_entry = Arc::clone(&entry);
        tokio::spawn(async move {
            let state =
                match wait_for_mailbox_reply(client, subscription, &pane_id, &prompt, after_cursor)
                    .await
                {
                    Ok(reply) => MailboxState::Completed(reply),
                    Err(error) => MailboxState::Failed(error),
                };
            worker_entry.finish(state);
        });

        wait_for_mailbox_entry(&entry, wait_seconds).await;
        ok_json(entry.json())
    }

    async fn resolve_agent_pane(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<String, McpError> {
        if let Some(pane_id) = self.optional_pane_id(arguments, "pane_id").await? {
            return Ok(pane_id);
        }
        if let Some(pane_id) = self.find_agent_pane(arguments).await? {
            return Ok(pane_id);
        }
        let _guard = self.spawn_lock.lock().await;
        if let Some(pane_id) = self.find_agent_pane(arguments).await? {
            return Ok(pane_id);
        }
        self.create_agent_pane(arguments).await
    }

    async fn find_agent_pane(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<Option<String>, McpError> {
        let filters = pane_filters(arguments)?;
        let mut client = connect(&self.socket).await?;
        let result = client.ps().await;
        let panes = libslopctl::apply_filters(self.slopd_result(result).await?, &filters);
        Ok(preferred_mcp_pane_id(
            panes,
            &self.mailbox.recent_pane_ids(),
        ))
    }

    async fn create_agent_pane(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<String, McpError> {
        let account = optional_string(arguments, "account");
        let backend = match optional_string(arguments, "backend") {
            Some(name) => Some(parse_backend(&name).map_err(invalid_argument)?),
            None => None,
        };
        let extra_tag = optional_string(arguments, "tag");
        let mut client = connect(&self.socket).await?;
        let result = client.subscribe(ready_event_filters()).await;
        let mut subscription = self.slopd_result(result).await?;
        let result = client
            .run(None, Vec::new(), None, Vec::new(), account, backend)
            .await;
        let pane_id = self.slopd_result(result).await?;
        let result = client.tag(pane_id.clone(), MCP_PANE_TAG.into()).await;
        self.slopd_result(result).await?;
        if let Some(tag) = extra_tag.filter(|tag| tag != MCP_PANE_TAG) {
            let result = client.tag(pane_id.clone(), tag).await;
            self.slopd_result(result).await?;
        }
        if let Err(message) = wait_pane_ready(&mut subscription, &pane_id, 30).await {
            let cleanup = client.kill(pane_id.clone()).await.err();
            let cleanup = cleanup
                .map(|error| format!("; cleanup also failed: {error}"))
                .unwrap_or_default();
            return Err(internal_failure(format!(
                "created pane {pane_id} but it did not become ready: {message}{cleanup}"
            )));
        }
        Ok(pane_id)
    }

    async fn read_mailbox(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let request_id = optional_string(arguments, "request_id");
        let pane_id = self.optional_pane_id(arguments, "pane_id").await?;
        let limit = optional_u64(arguments, "limit")?
            .unwrap_or(20)
            .clamp(1, 100) as usize;
        let wait_seconds = optional_u64(arguments, "wait_seconds")?
            .unwrap_or(0)
            .min(300);

        let entries = if let Some(request_id) = request_id {
            let entry = self.mailbox.get(&request_id).ok_or_else(|| {
                invalid_argument(format!(
                    "unknown mailbox request_id {request_id:?}; omit request_id to list recent mailbox entries"
                ))
            })?;
            wait_for_mailbox_entry(&entry, wait_seconds).await;
            vec![entry]
        } else {
            let entries = self.mailbox.recent(pane_id.as_deref(), limit);
            wait_for_any_mailbox_entry(&entries, wait_seconds).await;
            self.mailbox.recent(pane_id.as_deref(), limit)
        };
        let entries = entries.iter().map(|entry| entry.json()).collect::<Vec<_>>();
        ok_json(json!({ "count": entries.len(), "entries": entries }))
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
        let pane_id = self.optional_pane_id(arguments, "pane_id").await?;
        let mut client = connect(&self.socket).await?;
        let pane_ids = if let Some(pane_id) = pane_id {
            let result = client
                .send_prompt(pane_id, prompt, timeout, interrupt)
                .await;
            vec![self.slopd_result(result).await?]
        } else {
            match select {
                libslopctl::SelectMode::One => {
                    let pane_id = self.resolve_agent_pane(arguments).await?;
                    let result = client
                        .send_prompt(pane_id, prompt, timeout, interrupt)
                        .await;
                    vec![self.slopd_result(result).await?]
                }
                libslopctl::SelectMode::Any | libslopctl::SelectMode::All => {
                    let mut filters = pane_filters(arguments)?;
                    filters.push(("tag".into(), MCP_PANE_TAG.into()));
                    let result = client.ps().await;
                    let matches =
                        libslopctl::apply_filters(self.slopd_result(result).await?, &filters);
                    if matches.is_empty() {
                        let pane_id = self.resolve_agent_pane(arguments).await?;
                        let result = client
                            .send_prompt(pane_id, prompt, timeout, interrupt)
                            .await;
                        vec![self.slopd_result(result).await?]
                    } else {
                        let result = client
                            .send_filtered(&filters, &prompt, &select, timeout, interrupt)
                            .await;
                        self.slopd_result(result).await?
                    }
                }
            }
        };
        ok_json(json!({ "pane_ids": pane_ids }))
    }

    async fn wait_for_reply(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let pane_id = self.required_pane_id(arguments, "pane_id").await?;
        let timeout = optional_u64(arguments, "timeout")?
            .unwrap_or(120)
            .clamp(1, 300);
        let mut client = connect(&self.socket).await?;
        let result = client.subscribe(reply_event_filters(&pane_id)).await;
        let mut subscription = self.slopd_result(result).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout);

        loop {
            let result = client.read_transcript(pane_id.clone(), None, 500).await;
            let records = self.slopd_result(result).await?;
            let snapshot = reply_snapshot(&records);
            let result = client.ps().await;
            let panes = self.slopd_result(result).await?;
            let pane = panes
                .iter()
                .find(|pane| pane.pane_id == pane_id)
                .ok_or_else(|| invalid_argument(format!("pane {pane_id} no longer exists")))?;
            if let Some(reply) = snapshot.reply
                && (snapshot.explicit_complete || pane.state == libslop::PaneState::Ready)
            {
                return ok_json(json!({ "pane_id": pane_id, "reply": reply }));
            }

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return tool_error(format!(
                    "timed out after {timeout}s waiting for pane {pane_id} to finish; do not resend the prompt automatically"
                ));
            }
            match tokio::time::timeout(remaining, subscription.next()).await {
                Ok(Ok(Some(libslopctl::SubscriptionItem::Record(record))))
                    if record.source == "slopd" && record.event_type == "PaneDestroyed" =>
                {
                    return tool_error(format!("pane {pane_id} exited before replying"));
                }
                Ok(Ok(Some(_))) => {}
                Ok(Ok(None)) | Ok(Err(_)) => {
                    return Err(internal_failure("reply subscription closed"));
                }
                Err(_) => {
                    return tool_error(format!(
                        "timed out after {timeout}s waiting for pane {pane_id} to finish; do not resend the prompt automatically"
                    ));
                }
            }
        }
    }

    async fn interrupt(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let pane_id = self.required_pane_id(arguments, "pane_id").await?;
        let mut client = connect(&self.socket).await?;
        let result = client.interrupt(pane_id).await;
        let pane_id = self.slopd_result(result).await?;
        ok_json(json!({ "pane_id": pane_id }))
    }

    async fn listen(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let mut events = event_arguments(arguments)?;
        events.pane_id = self.optional_pane_id(arguments, "pane_id").await?;
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
                return Err(invalid_argument("where is incompatible with replay"));
            }
            let pane_id = pane_id.ok_or_else(|| invalid_argument("replay requires pane_id"))?;
            let result = client.subscribe_transcript(pane_id, last_n).await;
            self.slopd_result(result).await?
        } else {
            let filters = libslopctl::build_listen_filters(
                events.hooks,
                events.events,
                events.transcripts,
                pane_id,
                session_id,
                where_parsed,
            );
            let result = client.subscribe(filters).await;
            self.slopd_result(result).await?
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
        let mut events = event_arguments(arguments)?;
        events.pane_id = self.optional_pane_id(arguments, "pane_id").await?;
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
            expand_wait_transcripts(events.transcripts.clone()),
            pane_id.clone(),
            session_id.clone(),
            where_parsed.clone(),
        );
        let mut client = connect(&self.socket).await?;
        let result = client.subscribe(filters).await;
        let mut subscription = self.slopd_result(result).await?;

        if !optional_bool(arguments, "no_snapshot")?.unwrap_or(false)
            && (pane_id.is_some() || session_id.is_some())
            && events.hooks.is_empty()
            && events.transcripts.is_empty()
            && state_events_requested(&events.events)
        {
            let result = client.ps().await;
            let panes = self.slopd_result(result).await?;
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
                        return Err(internal_failure("event subscription closed"));
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
        let pane_id = self.required_pane_id(arguments, "pane_id").await?;
        let tag = required_string(arguments, "tag")?;
        let mut client = connect(&self.socket).await?;
        let result = if remove {
            client.untag(pane_id, tag).await
        } else {
            client.tag(pane_id, tag).await
        };
        let (pane_id, tag) = self.slopd_result(result).await?;
        ok_json(json!({ "pane_id": pane_id, "tag": tag }))
    }

    async fn tags(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let pane_id = self.required_pane_id(arguments, "pane_id").await?;
        let mut client = connect(&self.socket).await?;
        let result = client.tags(pane_id.clone()).await;
        let tags = self.slopd_result(result).await?;
        ok_json(json!({ "pane_id": pane_id, "tags": tags }))
    }

    async fn backup(&self) -> Result<CallToolResult, McpError> {
        let mut client = connect(&self.socket).await?;
        let result = client.backup().await;
        let count = self.slopd_result(result).await?;
        ok_json(json!({ "count": count }))
    }

    async fn restore(&self) -> Result<CallToolResult, McpError> {
        let mut client = connect(&self.socket).await?;
        let result = client.restore().await;
        let restored = self.slopd_result(result).await?;
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
        let raw = optional_bool(arguments, "raw")?.unwrap_or(false);
        let mut client = connect(&self.socket).await?;
        let result = client.graveyard(boot, limit).await;
        let entries = self.slopd_result(result).await?;
        let count = entries.len();
        let entries = if raw {
            serde_json::to_value(entries).unwrap_or_default()
        } else {
            Value::Array(entries.iter().map(compact_grave).collect())
        };
        ok_json(json!({ "count": count, "entries": entries }))
    }

    async fn revive(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let target = optional_string(arguments, "target");
        let boot = optional_i32(arguments, "boot")?;
        let env = environment(arguments)?;
        let mut client = connect(&self.socket).await?;
        let result = client.revive(target, boot, env).await;
        let (pane_id, grave_id) = self.slopd_result(result).await?;
        let result = client.tag(pane_id.clone(), MCP_PANE_TAG.into()).await;
        self.slopd_result(result).await?;
        ok_json(json!({ "pane_id": pane_id, "grave_id": grave_id }))
    }

    async fn run(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let spawn = spawn_arguments(arguments)?;
        let prompt = optional_string(arguments, "prompt");
        let parent_pane_id = self.optional_pane_id(arguments, "parent_pane_id").await?;
        let account = optional_string(arguments, "account");
        let backend = match optional_string(arguments, "backend") {
            Some(name) => Some(parse_backend(&name).map_err(invalid_argument)?),
            None => None,
        };
        let mut client = connect(&self.socket).await?;
        let wait_for_ready = !spawn.no_wait || prompt.is_some();
        let mut subscription = if !wait_for_ready {
            None
        } else {
            let result = client.subscribe(ready_event_filters()).await;
            Some(self.slopd_result(result).await?)
        };
        let result = client
            .run(
                parent_pane_id,
                spawn.extra_args,
                spawn.start_directory,
                spawn.env,
                account,
                backend,
            )
            .await;
        let pane_id = self.slopd_result(result).await?;
        let result = client.tag(pane_id.clone(), MCP_PANE_TAG.into()).await;
        self.slopd_result(result).await?;
        if let Some(subscription) = subscription.as_mut()
            && let Err(message) = wait_pane_ready(subscription, &pane_id, spawn.ready_timeout).await
        {
            return tool_json_error(json!({
                "pane_id": pane_id,
                "ready": false,
                "error": message,
            }));
        }
        let prompt_sent = if let Some(prompt) = prompt {
            let result = client.send_prompt(pane_id.clone(), prompt, 60, false).await;
            self.slopd_result(result).await?;
            true
        } else {
            false
        };
        ok_json(json!({
            "pane_id": pane_id,
            "ready": wait_for_ready,
            "prompt_sent": prompt_sent,
        }))
    }

    async fn required_pane_id(
        &self,
        arguments: Option<&Map<String, Value>>,
        key: &str,
    ) -> Result<String, McpError> {
        match arguments.and_then(|values| values.get(key)) {
            None | Some(Value::Null) => {
                let valid_panes = self.valid_pane_ids().await;
                Err(actionable_invalid_params(
                    "missing_pane_id",
                    format!(
                        "Missing {key}. Copy a pane_id exactly from list_panes, including its leading %."
                    ),
                    Some("list_panes"),
                    valid_panes,
                ))
            }
            Some(Value::String(value)) if valid_pane_id(value) => Ok(value.clone()),
            Some(value) => Err(self.invalid_pane_id(key, value).await),
        }
    }

    async fn optional_pane_id(
        &self,
        arguments: Option<&Map<String, Value>>,
        key: &str,
    ) -> Result<Option<String>, McpError> {
        match arguments.and_then(|values| values.get(key)) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) if valid_pane_id(value) => Ok(Some(value.clone())),
            Some(value) => Err(self.invalid_pane_id(key, value).await),
        }
    }

    async fn invalid_pane_id(&self, key: &str, value: &Value) -> McpError {
        let shown = value.as_str().unwrap_or_else(|| value_type(value));
        let message = format!(
            "Invalid {key} {shown:?}. Expected exactly \"%<digits>\", for example \"%151\". Copy pane_id unchanged from list_panes, create_pane, fork_pane, or revive_pane. Do not spell \"%\" as \"percent\" or remove it."
        );
        actionable_invalid_params(
            "invalid_pane_id",
            message,
            Some("list_panes"),
            self.valid_pane_ids().await,
        )
    }

    async fn valid_pane_ids(&self) -> Vec<String> {
        let Ok(mut client) = connect(&self.socket).await else {
            return Vec::new();
        };
        let Ok(panes) = client.ps().await else {
            return Vec::new();
        };
        let mut pane_ids = panes
            .into_iter()
            .map(|pane| pane.pane_id)
            .collect::<Vec<_>>();
        pane_ids.sort();
        pane_ids
    }

    async fn slopd_result<T>(&self, result: Result<T, libslopctl::Error>) -> Result<T, McpError> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => Err(slopd_error_with_panes(error, self.valid_pane_ids().await)),
        }
    }
}

fn canonical_tool_name(name: &str) -> &str {
    match name {
        "status" => "get_status",
        "ps" => "list_panes",
        "run" => "create_pane",
        "fork" => "fork_pane",
        "kill" => "kill_pane",
        "send" => "send_prompt",
        "interrupt" => "interrupt_pane",
        "listen" => "collect_events",
        "wait" => "wait_for_event",
        "transcript" => "read_transcript",
        "tag" => "add_tag",
        "untag" => "remove_tag",
        "tags" => "list_tags",
        "backup" => "create_backup",
        "restore" => "restore_backup",
        "graveyard" => "list_dead_panes",
        "revive" => "revive_pane",
        current => current,
    }
}

impl ServerHandler for SlopdMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("slopd-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Supervisor for slopd-managed agent panes. Prefer ask_agent for a request and reply. For requests such as 'ask my Codex agent', call ask_agent with backend=codex; do not ask the user for a pane ID or call list_panes first. Without an explicit pane_id, mutating tools use only slopd-mcp-tagged panes and create one when needed, so they never select a human-owned pane. Fast replies return inline and slow replies remain in the server-wide mailbox. Call read_mailbox with no arguments later, even from a new MCP session; no request ID is required. Never resend a pending mailbox request.",
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
        let message = format!("failed to connect to {}: {error}", socket.display());
        McpError::internal_error(
            message.clone(),
            Some(actionable_error(
                "slopd_unavailable",
                message,
                Some("get_status"),
                Vec::new(),
            )),
        )
    })?;
    let (reader, writer) = stream.into_split();
    Ok(libslopctl::Client::new(reader, writer))
}

async fn wait_for_mailbox_reply(
    mut client: libslopctl::Client<
        tokio::net::unix::OwnedReadHalf,
        tokio::net::unix::OwnedWriteHalf,
    >,
    mut subscription: libslopctl::Subscription,
    pane_id: &str,
    prompt: &str,
    after_cursor: u64,
) -> Result<String, String> {
    let deadline = tokio::time::Instant::now() + MAILBOX_WORKER_TIMEOUT;
    loop {
        let records = client
            .read_transcript(pane_id.to_string(), None, 500)
            .await
            .map_err(|error| error.to_string())?;
        let snapshot = request_reply_snapshot(&records, after_cursor, prompt);
        let panes = client.ps().await.map_err(|error| error.to_string())?;
        let pane = panes
            .iter()
            .find(|pane| pane.pane_id == pane_id)
            .ok_or_else(|| format!("pane {pane_id} no longer exists"))?;
        if let Some(reply) = snapshot.reply
            && (snapshot.explicit_complete || pane.state == libslop::PaneState::Ready)
        {
            return Ok(reply);
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(format!("pane {pane_id} did not reply within 24 hours"));
        }
        match tokio::time::timeout(remaining, subscription.next()).await {
            Ok(Ok(Some(libslopctl::SubscriptionItem::Record(record))))
                if record.source == "slopd" && record.event_type == "PaneDestroyed" =>
            {
                return Err(format!("pane {pane_id} exited before replying"));
            }
            Ok(Ok(Some(_))) => {}
            Ok(Ok(None)) | Ok(Err(_)) => return Err("reply subscription closed".into()),
            Err(_) => return Err(format!("pane {pane_id} did not reply within 24 hours")),
        }
    }
}

async fn wait_for_mailbox_entry(entry: &MailboxEntry, wait_seconds: u64) {
    if wait_seconds == 0 {
        return;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_seconds);
    while matches!(entry.state(), MailboxState::Pending) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        tokio::time::sleep(remaining.min(Duration::from_millis(100))).await;
    }
}

async fn wait_for_any_mailbox_entry(entries: &[Arc<MailboxEntry>], wait_seconds: u64) {
    let pending = entries
        .iter()
        .filter(|entry| matches!(entry.state(), MailboxState::Pending))
        .cloned()
        .collect::<Vec<_>>();
    if wait_seconds == 0 || pending.is_empty() {
        return;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_seconds);
    while pending
        .iter()
        .all(|entry| matches!(entry.state(), MailboxState::Pending))
    {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        tokio::time::sleep(remaining.min(Duration::from_millis(100))).await;
    }
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
            return Err(invalid_argument(
                "start_directory must be absolute or start with ~ or $VAR",
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

fn expand_wait_transcripts(transcripts: Vec<String>) -> Vec<String> {
    let mut expanded = Vec::new();
    for transcript in transcripts {
        let aliases: &[&str] = match transcript.as_str() {
            "assistant" => &["assistant", "agentMessage", "turn_completed"],
            "user" => &["user", "userMessage", "user_message_chunk"],
            _ => {
                if !expanded.contains(&transcript) {
                    expanded.push(transcript);
                }
                continue;
            }
        };
        for alias in aliases {
            if !expanded.iter().any(|existing| existing == alias) {
                expanded.push((*alias).to_string());
            }
        }
    }
    expanded
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

fn compact_pane(pane: &libslop::PaneInfo) -> Value {
    json!({
        "pane_id": pane.pane_id,
        "backend": pane.backend,
        "account": pane.account,
        "state": pane.state,
        "detailed_state": pane.detailed_state,
        "tags": pane.tags,
        "title": pane.pane_title,
        "working_dir": pane.working_dir,
        "parent_pane_id": pane.parent_pane_id,
    })
}

fn preferred_mcp_pane_id(panes: Vec<libslop::PaneInfo>, recent: &[String]) -> Option<String> {
    let priority = |pane: &libslop::PaneInfo| {
        let used = recent
            .iter()
            .position(|pane_id| pane_id == &pane.pane_id)
            .map_or(0, |index| recent.len() - index);
        (
            used,
            pane.last_active,
            pane.created_at,
            pane.pane_id.clone(),
        )
    };
    panes
        .into_iter()
        .filter(|pane| pane.tags.iter().any(|tag| tag == MCP_PANE_TAG))
        .max_by(|left, right| priority(left).cmp(&priority(right)))
        .map(|pane| pane.pane_id)
}

fn compact_grave(entry: &libslop::GraveEntry) -> Value {
    json!({
        "grave_id": entry.grave_id,
        "destroyed_at": entry.destroyed_at,
        "cause": entry.cause,
        "pane_id": entry.pane.pane_id,
        "backend": entry.pane.backend,
        "account": entry.pane.account,
        "title": entry.pane.pane_title,
        "working_dir": entry.pane.working_dir,
        "revived_at": entry.revived_at,
        "revived_as": entry.revived_as,
    })
}

struct ReplySnapshot {
    reply: Option<String>,
    explicit_complete: bool,
}

fn reply_event_filters(pane_id: &str) -> Vec<libslop::EventFilter> {
    vec![
        libslop::EventFilter {
            source: Some("transcript".into()),
            pane_id: Some(pane_id.into()),
            ..Default::default()
        },
        libslop::EventFilter {
            source: Some("slopd".into()),
            event_type: Some("DetailedStateChange".into()),
            pane_id: Some(pane_id.into()),
            ..Default::default()
        },
        libslop::EventFilter {
            source: Some("slopd".into()),
            event_type: Some("PaneDestroyed".into()),
            pane_id: Some(pane_id.into()),
            ..Default::default()
        },
    ]
}

fn reply_snapshot(records: &[libslop::Record]) -> ReplySnapshot {
    let last_user = records.iter().rposition(|record| {
        conversation_role(record) == Some("user")
            && transcript_text(&record.payload).is_some_and(|text| !internal_text(&text))
    });
    let Some(last_user) = last_user else {
        return ReplySnapshot {
            reply: None,
            explicit_complete: false,
        };
    };
    reply_snapshot_from(records, last_user)
}

fn request_reply_snapshot(
    records: &[libslop::Record],
    after_cursor: u64,
    prompt: &str,
) -> ReplySnapshot {
    let matching_user = records.iter().position(|record| {
        record.cursor.is_some_and(|cursor| cursor > after_cursor)
            && conversation_role(record) == Some("user")
            && transcript_text(&record.payload).is_some_and(|text| text.trim() == prompt.trim())
    });
    let Some(matching_user) = matching_user else {
        return ReplySnapshot {
            reply: None,
            explicit_complete: false,
        };
    };
    reply_snapshot_from(records, matching_user)
}

fn reply_snapshot_from(records: &[libslop::Record], user_index: usize) -> ReplySnapshot {
    let mut reply = None;
    let mut chunks = String::new();
    let mut explicit_complete = false;
    for record in records.iter().skip(user_index + 1) {
        if record.event_type == "turn_completed" {
            explicit_complete = true;
            break;
        }
        if conversation_role(record) == Some("user") && reply.is_some() {
            break;
        }
        if conversation_role(record) != Some("assistant") || progress_record(record) {
            continue;
        }
        let Some(text) = transcript_text(&record.payload) else {
            continue;
        };
        if record.event_type == "agent_message_chunk" {
            chunks.push_str(&text);
            reply = Some(chunks.clone());
        } else {
            reply = Some(text);
        }
        if record.payload.get("phase").and_then(Value::as_str) == Some("final_answer") {
            explicit_complete = true;
            break;
        }
    }
    ReplySnapshot {
        reply,
        explicit_complete,
    }
}

fn simple_record(record: &libslop::Record) -> Option<Value> {
    let role = conversation_role(record)?;
    if progress_record(record) {
        return None;
    }
    let text = transcript_text(&record.payload)?;
    if role == "user" && internal_text(&text) {
        return None;
    }
    Some(json!({ "role": role, "text": text }))
}

fn conversation_role(record: &libslop::Record) -> Option<&'static str> {
    match record.event_type.as_str() {
        "user" | "userMessage" | "user_message_chunk" => Some("user"),
        "assistant" | "agentMessage" | "agent_message_chunk" => Some("assistant"),
        _ => match record.payload.get("role").and_then(Value::as_str) {
            Some("user") => Some("user"),
            Some("assistant") => Some("assistant"),
            _ => None,
        },
    }
}

fn progress_record(record: &libslop::Record) -> bool {
    record.payload.get("phase").and_then(Value::as_str) == Some("commentary")
}

fn internal_text(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with("# AGENTS.md instructions")
        || text.starts_with("<environment_context>")
        || text.starts_with("<permissions instructions>")
}

fn compact_record(record: &libslop::Record) -> Value {
    json!({
        "cursor": record.cursor,
        "type": record.event_type,
        "text": transcript_text(&record.payload),
    })
}

fn transcript_text(payload: &Value) -> Option<String> {
    [
        "/text",
        "/message/content",
        "/content",
        "/parts",
        "/part/text",
        "/params/update/content/text",
        "/delta/text",
    ]
    .into_iter()
    .find_map(|path| payload.pointer(path).and_then(text_value))
}

fn text_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Array(items) => {
            let text = items
                .iter()
                .filter_map(text_value)
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        Value::Object(object) => object
            .get("text")
            .and_then(text_value)
            .or_else(|| object.get("content").and_then(text_value)),
        _ => None,
    }
}

fn pane_filters(arguments: Option<&Map<String, Value>>) -> Result<Vec<(String, String)>, McpError> {
    let mut filters = Vec::new();
    if let Some(tag) = optional_string(arguments, "tag") {
        libslop::tag_option_name(&tag).map_err(invalid_argument)?;
        filters.push(("tag".into(), tag));
    }
    if let Some(backend) = optional_string(arguments, "backend") {
        parse_backend(&backend).map_err(invalid_argument)?;
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
    optional_string(arguments, key).ok_or_else(|| invalid_argument(format!("{key} is required")))
}

fn valid_pane_id(pane_id: &str) -> bool {
    pane_id.strip_prefix('%').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn optional_bool(
    arguments: Option<&Map<String, Value>>,
    key: &str,
) -> Result<Option<bool>, McpError> {
    match arguments.and_then(|args| args.get(key)) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(invalid_argument(format!("{key} must be a boolean"))),
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
            .ok_or_else(|| invalid_argument(format!("{key} must be a non-negative integer")))
            .map(Some),
        Some(_) => Err(invalid_argument(format!(
            "{key} must be a non-negative integer"
        ))),
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
            .ok_or_else(|| invalid_argument(format!("{key} must be a 32-bit integer")))
            .map(Some),
        Some(_) => Err(invalid_argument(format!("{key} must be an integer"))),
    }
}

fn parse_select(value: Option<&str>) -> Result<libslopctl::SelectMode, McpError> {
    match value.unwrap_or("one") {
        "one" => Ok(libslopctl::SelectMode::One),
        "any" => Ok(libslopctl::SelectMode::Any),
        "all" => Ok(libslopctl::SelectMode::All),
        other => Err(invalid_argument(format!(
            "unknown select mode {other:?}; expected one, any, or all"
        ))),
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
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| invalid_argument(format!("{key} items must be strings")))
            })
            .collect(),
        Some(_) => Err(invalid_argument(format!(
            "{key} must be an array of strings"
        ))),
    }
}

fn slopd_error(error: libslopctl::Error) -> McpError {
    slopd_error_with_panes(error, Vec::new())
}

fn slopd_error_with_panes(error: libslopctl::Error, valid_panes: Vec<String>) -> McpError {
    let message = error.to_string();
    match error {
        libslopctl::Error::SelectError(_) => actionable_invalid_params(
            "invalid_selection",
            message,
            Some("list_panes"),
            valid_panes,
        ),
        libslopctl::Error::FilterError(_) => {
            actionable_invalid_params("invalid_filter", message, Some("list_panes"), valid_panes)
        }
        libslopctl::Error::Server(ref server_message)
            if server_message.contains("not managed by slopd")
                || server_message.contains("unknown pane") =>
        {
            actionable_invalid_params("unknown_pane_id", message, Some("list_panes"), valid_panes)
        }
        libslopctl::Error::Timeout => McpError::internal_error(
            message.clone(),
            Some(actionable_error(
                "slopd_timeout",
                message,
                Some("get_status"),
                valid_panes,
            )),
        ),
        libslopctl::Error::Io(_) | libslopctl::Error::ConnectionClosed => McpError::internal_error(
            message.clone(),
            Some(actionable_error(
                "slopd_unavailable",
                message,
                Some("get_status"),
                valid_panes,
            )),
        ),
        _ => McpError::internal_error(
            message.clone(),
            Some(actionable_error(
                "slopd_error",
                message,
                Some("get_status"),
                valid_panes,
            )),
        ),
    }
}

fn ok_json(value: Value) -> Result<CallToolResult, McpError> {
    let mut result = CallToolResult::structured(value);
    result.content.clear();
    Ok(result)
}

fn tool_error(message: impl Into<String>) -> Result<CallToolResult, McpError> {
    tool_json_error(json!({ "message": message.into() }))
}

fn tool_json_error(value: Value) -> Result<CallToolResult, McpError> {
    let mut object = value.as_object().cloned().unwrap_or_default();
    let message = object
        .remove("message")
        .or_else(|| object.remove("error"))
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "tool operation failed".to_string());
    object.insert("code".into(), json!("operation_failed"));
    object.insert("message".into(), json!(message));
    object.insert("retry_with".into(), Value::Null);
    object.insert("valid_panes".into(), json!([]));
    let mut result = CallToolResult::structured_error(Value::Object(object));
    result.content.clear();
    Ok(result)
}

fn actionable_invalid_params(
    code: &str,
    message: String,
    retry_tool: Option<&str>,
    valid_panes: Vec<String>,
) -> McpError {
    McpError::invalid_params(
        message.clone(),
        Some(actionable_error(code, message, retry_tool, valid_panes)),
    )
}

fn invalid_argument(message: impl Into<String>) -> McpError {
    actionable_invalid_params("invalid_argument", message.into(), None, Vec::new())
}

fn internal_failure(message: impl Into<String>) -> McpError {
    let message = message.into();
    McpError::internal_error(
        message.clone(),
        Some(actionable_error(
            "slopd_error",
            message,
            Some("get_status"),
            Vec::new(),
        )),
    )
}

fn actionable_error(
    code: &str,
    message: String,
    retry_tool: Option<&str>,
    valid_panes: Vec<String>,
) -> Value {
    json!({
        "code": code,
        "message": message,
        "retry_with": retry_tool.map(|tool| json!({ "tool": tool, "arguments": {} })),
        "valid_panes": valid_panes,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        canonical_tool_name, expand_wait_transcripts, parse_backend, preferred_mcp_pane_id,
        reply_snapshot, simple_record, transcript_text, valid_pane_id,
    };
    use serde_json::{Value, json};

    #[test]
    fn parse_backend_accepts_canonical_names() {
        assert_eq!(parse_backend("grok").unwrap(), libslop::Backend::Grok);
        assert!(parse_backend("claude-code").is_err());
    }

    #[test]
    fn wait_transcript_aliases_cover_backend_event_names() {
        assert_eq!(
            expand_wait_transcripts(vec!["assistant".into()]),
            ["assistant", "agentMessage", "turn_completed"]
        );
        assert_eq!(
            expand_wait_transcripts(vec!["user".into()]),
            ["user", "userMessage", "user_message_chunk"]
        );
    }

    #[test]
    fn pane_ids_keep_the_tmux_prefix() {
        assert!(valid_pane_id("%146"));
        assert!(!valid_pane_id("146"));
        assert!(!valid_pane_id("percent146"));
    }

    #[test]
    fn implicit_agent_selection_ignores_unowned_panes() {
        let pane = |pane_id: &str, last_active: u64, tags: &[&str]| libslop::PaneInfo {
            pane_id: pane_id.into(),
            created_at: last_active,
            last_active,
            session_id: None,
            parent_pane_id: None,
            tags: tags.iter().map(|tag| (*tag).into()).collect(),
            state: libslop::PaneState::Ready,
            detailed_state: libslop::PaneDetailedState::Ready,
            working_dir: None,
            transcript_path: None,
            account: "codex".into(),
            backend: libslop::Backend::Codex,
            pane_title: None,
        };
        let panes = || {
            vec![
                pane("%1", 30, &[]),
                pane("%2", 20, &["slopd-mcp"]),
                pane("%3", 10, &[]),
                pane("%4", 5, &["slopd-mcp"]),
            ]
        };

        assert_eq!(preferred_mcp_pane_id(panes(), &[]).as_deref(), Some("%2"));
        assert_eq!(
            preferred_mcp_pane_id(panes(), &["%3".into(), "%1".into()]).as_deref(),
            Some("%2")
        );
        assert_eq!(
            preferred_mcp_pane_id(panes(), &["%4".into()]).as_deref(),
            Some("%4")
        );
    }

    #[test]
    fn legacy_tool_names_dispatch_to_current_tools() {
        let aliases = [
            ("status", "get_status"),
            ("ps", "list_panes"),
            ("run", "create_pane"),
            ("fork", "fork_pane"),
            ("kill", "kill_pane"),
            ("send", "send_prompt"),
            ("interrupt", "interrupt_pane"),
            ("listen", "collect_events"),
            ("wait", "wait_for_event"),
            ("transcript", "read_transcript"),
            ("tag", "add_tag"),
            ("untag", "remove_tag"),
            ("tags", "list_tags"),
            ("backup", "create_backup"),
            ("restore", "restore_backup"),
            ("graveyard", "list_dead_panes"),
            ("revive", "revive_pane"),
        ];
        for (legacy, current) in aliases {
            assert_eq!(canonical_tool_name(legacy), current);
        }
        assert_eq!(canonical_tool_name("create_pane"), "create_pane");
    }

    #[test]
    fn transcript_text_normalizes_supported_backends() {
        assert_eq!(
            transcript_text(&serde_json::json!({ "text": "codex" })).as_deref(),
            Some("codex")
        );
        assert_eq!(
            transcript_text(&serde_json::json!({
                "message": { "content": [{ "type": "text", "text": "claude" }] }
            }))
            .as_deref(),
            Some("claude")
        );
        assert_eq!(
            transcript_text(&serde_json::json!({ "part": { "text": "opencode" } })).as_deref(),
            Some("opencode")
        );
    }

    #[test]
    fn simple_transcript_keeps_only_conversation_text() {
        let record = |event_type: &str, payload: Value| libslop::Record {
            source: "transcript".into(),
            event_type: event_type.into(),
            pane_id: Some("%1".into()),
            payload,
            cursor: Some(1),
        };
        assert!(
            simple_record(&record(
                "userMessage",
                json!({ "text": "# AGENTS.md instructions\nprivate" }),
            ))
            .is_none()
        );
        assert!(
            simple_record(&record(
                "agentMessage",
                json!({ "text": "working", "phase": "commentary" }),
            ))
            .is_none()
        );
        assert_eq!(
            simple_record(&record(
                "agentMessage",
                json!({ "text": "done", "phase": "final_answer" }),
            )),
            Some(json!({ "role": "assistant", "text": "done" }))
        );
        assert!(simple_record(&record("toolCall", json!({ "name": "exec" }))).is_none());
    }

    #[test]
    fn reply_snapshot_ignores_progress_and_joins_grok_chunks() {
        let record = |event_type: &str, payload: Value| libslop::Record {
            source: "transcript".into(),
            event_type: event_type.into(),
            pane_id: Some("%1".into()),
            payload,
            cursor: Some(1),
        };
        let codex = reply_snapshot(&[
            record("userMessage", json!({ "text": "question" })),
            record(
                "agentMessage",
                json!({ "text": "working", "phase": "commentary" }),
            ),
            record(
                "agentMessage",
                json!({ "text": "answer", "phase": "final_answer" }),
            ),
        ]);
        assert_eq!(codex.reply.as_deref(), Some("answer"));
        assert!(codex.explicit_complete);

        let grok = reply_snapshot(&[
            record(
                "user_message_chunk",
                json!({ "params": { "update": { "content": { "text": "question" } } } }),
            ),
            record(
                "agent_message_chunk",
                json!({ "params": { "update": { "content": { "text": "an" } } } }),
            ),
            record(
                "agent_message_chunk",
                json!({ "params": { "update": { "content": { "text": "swer" } } } }),
            ),
            record("turn_completed", json!({})),
        ]);
        assert_eq!(grok.reply.as_deref(), Some("answer"));
        assert!(grok.explicit_complete);
    }
}
