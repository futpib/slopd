use super::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

const ACP_PROTOCOL_VERSION: u32 = 1;
const ACP_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const INITIAL_RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_millis(500);
const MAX_RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_secs(30);
const LEADER_SOCKET_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
type RpcResult = Result<Value, String>;
type PendingRequests = std::sync::Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<RpcResult>>>>;

#[derive(Clone)]
pub(super) struct AttachSpec {
    pub(super) executable: PathBuf,
    pub(super) executable_args: Vec<String>,
    pub(super) leader_socket: PathBuf,
    pub(super) session_id: String,
    pub(super) cwd: PathBuf,
    pub(super) config_dir: Option<PathBuf>,
    pub(super) env: Vec<(String, String)>,
}

#[derive(Clone)]
pub(super) struct GrokClient {
    sender: mpsc::Sender<Value>,
    pending: PendingRequests,
    next_id: std::sync::Arc<AtomicU64>,
    disconnected: tokio_util::sync::CancellationToken,
    cancel: tokio_util::sync::CancellationToken,
    session_id: String,
}

impl GrokClient {
    async fn connect(
        spec: &AttachSpec,
        pane_id: &str,
        pane_state: std::sync::Arc<PaneState>,
        panes: PaneMap,
        config: std::sync::Arc<libslop::SlopdConfig>,
        event_tx: EventTx,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Self, String> {
        let mut command = tokio::process::Command::new(&spec.executable);
        command
            .args(&spec.executable_args)
            .args([
                "agent",
                "--leader",
                "--leader-socket",
                spec.leader_socket.to_string_lossy().as_ref(),
                "stdio",
            ])
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env("XDG_RUNTIME_DIR", libslop::runtime_dir())
            .env("SLOPCTL", &config.run.slopctl);
        if let Some(config_dir) = &spec.config_dir {
            command.env("GROK_HOME", config_dir);
        }
        for (key, value) in &spec.env {
            command.env(key, value);
        }

        let mut child = command.spawn().map_err(|error| {
            format!("failed to start Grok ACP sidecar for pane {pane_id}: {error}")
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Grok ACP sidecar has no stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Grok ACP sidecar has no stdout".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "Grok ACP sidecar has no stderr".to_string())?;

        let stderr_pane = pane_id.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                debug!("Grok ACP sidecar stderr for pane {}: {}", stderr_pane, line);
            }
        });

        let (sender, mut receiver) = mpsc::channel::<Value>(64);
        let pending: PendingRequests = Default::default();
        let disconnected = tokio_util::sync::CancellationToken::new();

        let writer_disconnected = disconnected.clone();
        tokio::spawn(async move {
            let mut writer = tokio::io::BufWriter::new(stdin);
            while let Some(message) = receiver.recv().await {
                let Ok(mut line) = serde_json::to_vec(&message) else {
                    continue;
                };
                line.push(b'\n');
                if writer.write_all(&line).await.is_err() || writer.flush().await.is_err() {
                    break;
                }
            }
            writer_disconnected.cancel();
        });

        let reader_pending = pending.clone();
        let reader_disconnected = disconnected.clone();
        let reader_pane = pane_id.to_string();
        let reader_config = config.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                let line = tokio::select! {
                    _ = reader_disconnected.cancelled() => break,
                    line = lines.next_line() => line,
                };
                let Ok(Some(line)) = line else { break };
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    debug!("ignoring malformed Grok ACP frame for pane {}", reader_pane);
                    continue;
                };
                if let Some(id) = message.get("id").and_then(Value::as_u64)
                    && message.get("method").is_none()
                {
                    if let Some(waiter) = reader_pending.lock().unwrap().remove(&id) {
                        let result = if let Some(error) = message.get("error") {
                            Err(format!("Grok ACP request failed: {error}"))
                        } else {
                            Ok(message.get("result").cloned().unwrap_or(Value::Null))
                        };
                        let _ = waiter.send(result);
                    }
                    continue;
                }

                let Some(method) = message.get("method").and_then(Value::as_str) else {
                    continue;
                };
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                if matches!(method, "session/update" | "_x.ai/session/update") {
                    publish_update(method, params, &reader_pane, &pane_state, &event_tx).await;
                } else if message.get("id").is_some() {
                    // Permission and elicitation reverse requests are deliberately
                    // left to the visible TUI. Grok's leader broadcasts those
                    // interactions and accepts the first client response.
                    let (interaction_method, interaction_params) =
                        interaction_parts(method, &params);
                    if let Some((event_type, detailed_state)) =
                        reverse_interaction(interaction_method)
                    {
                        observe_reverse_interaction(
                            interaction_method,
                            interaction_params.clone(),
                            event_type,
                            detailed_state,
                            &reader_pane,
                            &pane_state,
                            &panes,
                            &reader_config,
                            &event_tx,
                        )
                        .await;
                    } else {
                        trace!(
                            "Grok ACP reverse request {} for pane {} is owned by the TUI",
                            method, reader_pane
                        );
                    }
                }
            }

            let waiters = std::mem::take(&mut *reader_pending.lock().unwrap());
            for (_, waiter) in waiters {
                let _ = waiter.send(Err("Grok ACP sidecar disconnected".to_string()));
            }
            reader_disconnected.cancel();
        });

        let child_disconnected = disconnected.clone();
        let child_cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = child_cancel.cancelled() => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                }
                _ = child_disconnected.cancelled() => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                }
                _ = child.wait() => {}
            }
            child_disconnected.cancel();
        });

        let client = Self {
            sender,
            pending,
            next_id: std::sync::Arc::new(AtomicU64::new(1)),
            disconnected,
            cancel,
            session_id: spec.session_id.clone(),
        };
        client
            .request(
                "initialize",
                json!({
                    "protocolVersion": ACP_PROTOCOL_VERSION,
                    "clientCapabilities": {},
                    "clientInfo": {
                        "name": "slopd",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            )
            .await?;
        client
            .request(
                "session/load",
                json!({
                    "sessionId": spec.session_id,
                    "cwd": spec.cwd,
                    "mcpServers": [],
                }),
            )
            .await?;
        Ok(client)
    }

    async fn start_request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<oneshot::Receiver<RpcResult>, String> {
        if self.disconnected.is_cancelled() {
            return Err("Grok ACP sidecar is disconnected".to_string());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, sender);
        if self
            .sender
            .send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }))
            .await
            .is_err()
        {
            self.pending.lock().unwrap().remove(&id);
            return Err("Grok ACP writer stopped".to_string());
        }
        Ok(receiver)
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let response = self.start_request(method, params).await?;
        tokio::time::timeout(ACP_REQUEST_TIMEOUT, response)
            .await
            .map_err(|_| format!("timed out waiting for Grok ACP {method} response"))?
            .map_err(|_| "Grok ACP response waiter closed".to_string())?
    }

    pub(super) async fn prompt(
        &self,
        prompt: &str,
    ) -> Result<oneshot::Receiver<RpcResult>, String> {
        self.start_request(
            "session/prompt",
            json!({
                "sessionId": self.session_id,
                "prompt": [{"type": "text", "text": prompt}],
            }),
        )
        .await
    }

    pub(super) async fn interrupt(&self) -> Result<(), String> {
        self.sender
            .send(json!({
                "jsonrpc": "2.0",
                "method": "session/cancel",
                "params": {
                    "sessionId": self.session_id,
                    "_meta": {"cancelTrigger": "slopctl"},
                },
            }))
            .await
            .map_err(|_| "Grok ACP writer stopped".to_string())
    }

    pub(super) fn disconnected(&self) -> tokio_util::sync::CancellationToken {
        self.disconnected.clone()
    }

    pub(super) fn is_disconnected(&self) -> bool {
        self.disconnected.is_cancelled()
    }

    pub(super) fn same_connection(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.pending, &other.pending)
    }

    pub(super) fn stop(&self) {
        self.cancel.cancel();
        self.disconnected.cancel();
    }
}

/// Grok gateway extension requests use an outer `_x.ai/...` method whose real
/// method and parameters are nested one level down. Plain ACP requests such as
/// `session/request_permission` arrive directly.
fn interaction_parts<'a>(method: &'a str, params: &'a Value) -> (&'a str, &'a Value) {
    if method.starts_with('_') {
        let inner_method = params
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_else(|| method.strip_prefix('_').unwrap_or(method));
        let inner_params = params.get("params").unwrap_or(params);
        (inner_method, inner_params)
    } else {
        (method, params)
    }
}

fn reverse_interaction(method: &str) -> Option<(&'static str, libslop::PaneDetailedState)> {
    match method {
        "session/request_permission" | "x.ai/exit_plan_mode" => Some((
            "PermissionRequest",
            libslop::PaneDetailedState::AwaitingInputPermission,
        )),
        "x.ai/ask_user_question" => Some((
            "Elicitation",
            libslop::PaneDetailedState::AwaitingInputElicitation,
        )),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn observe_reverse_interaction(
    method: &str,
    params: Value,
    event_type: &str,
    detailed_state: libslop::PaneDetailedState,
    pane_id: &str,
    pane_state: &std::sync::Arc<PaneState>,
    panes: &PaneMap,
    config: &libslop::SlopdConfig,
    event_tx: &EventTx,
) {
    let current = pane_state.detailed_state.lock().unwrap().clone();
    set_hook_detailed_state(
        config,
        pane_id,
        &detailed_state,
        Some(&current),
        event_tx,
        panes,
    )
    .await;
    let mut payload = normalize_hook_payload(json!({"method": method, "params": params}));
    if let Some(session_id) = payload.pointer("/params/session_id").cloned()
        && let Some(object) = payload.as_object_mut()
    {
        object.insert("session_id".to_string(), session_id);
    }
    let _ = event_tx.send(libslop::Record {
        cursor: None,
        source: "hook".to_string(),
        event_type: event_type.to_string(),
        pane_id: Some(pane_id.to_string()),
        payload,
    });
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_driver(
    mut spec: AttachSpec,
    pane_id: String,
    state: GrokState,
    pane_state: std::sync::Arc<PaneState>,
    panes: PaneMap,
    config: std::sync::Arc<libslop::SlopdConfig>,
    event_tx: EventTx,
    wait_for_session_start: bool,
) {
    if wait_for_session_start {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let started = pane_state.session_started_notify.notified();
            if pane_state.session_started.load(Ordering::Acquire) {
                break;
            }
            tokio::select! {
                _ = state.cancel.cancelled() => return,
                _ = started => {}
                _ = tokio::time::sleep_until(deadline) => {
                    warn!("Grok pane {} did not fire SessionStart; using tmux fallback", pane_id);
                    return;
                }
            }
        }
    }
    if spec.session_id.is_empty() {
        spec.session_id = pane_state
            .identity
            .lock()
            .unwrap()
            .session_id
            .clone()
            .unwrap_or_default();
        if spec.session_id.is_empty() {
            warn!(
                "Grok pane {} did not expose a session id; using tmux fallback",
                pane_id
            );
            return;
        }
    }

    // Grok deliberately disables leader mode whenever a non-off sandbox is
    // active, including profiles restored from session metadata or config that
    // slopd cannot infer from argv. Only attach an ACP peer when the visible TUI
    // actually created its private leader socket; otherwise tmux remains the
    // transport while native hooks and updates.jsonl still provide full state
    // and transcript fidelity.
    let socket_ready = tokio::time::timeout(LEADER_SOCKET_WAIT, async {
        loop {
            if tokio::fs::metadata(&spec.leader_socket).await.is_ok() {
                return;
            }
            tokio::select! {
                _ = state.cancel.cancelled() => return,
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
            }
        }
    })
    .await
    .is_ok();
    if state.cancel.is_cancelled() {
        return;
    }
    if !socket_ready {
        info!(
            "Grok pane {} has no leader socket; using sandbox-compatible tmux transport",
            pane_id
        );
        return;
    }

    let mut reconnect_delay = INITIAL_RECONNECT_DELAY;
    loop {
        if state.cancel.is_cancelled() {
            return;
        }
        match GrokClient::connect(
            &spec,
            &pane_id,
            pane_state.clone(),
            panes.clone(),
            config.clone(),
            event_tx.clone(),
            state.cancel.clone(),
        )
        .await
        {
            Ok(client) => {
                info!("attached Grok ACP sidecar to pane {}", pane_id);
                reconnect_delay = INITIAL_RECONNECT_DELAY;
                state.set_client(Some(client.clone()));
                let disconnected = client.disconnected();
                tokio::select! {
                    _ = state.cancel.cancelled() => {
                        client.stop();
                        return;
                    }
                    _ = disconnected.cancelled() => {}
                }
                state.clear_client(&client);
            }
            Err(error) => {
                debug!("Grok ACP attach failed for pane {}: {}", pane_id, error);
            }
        }
        tokio::select! {
            _ = state.cancel.cancelled() => return,
            _ = tokio::time::sleep(reconnect_delay) => {}
        }
        reconnect_delay = std::cmp::min(reconnect_delay.saturating_mul(2), MAX_RECONNECT_DELAY);
    }
}

async fn publish_update(
    method: &str,
    params: Value,
    pane_id: &str,
    pane_state: &std::sync::Arc<PaneState>,
    event_tx: &EventTx,
) {
    let update = params.get("update").cloned().unwrap_or(Value::Null);
    let event_type = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or(method)
        .to_string();
    let replay = update
        .pointer("/_meta/isReplay")
        .or_else(|| params.pointer("/_meta/isReplay"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // session/load replay is for hydrating the ACP sidecar. Historical reads
    // come from updates.jsonl; rebroadcasting replay here would duplicate an
    // active slopd-acp turn every time the sidecar reconnects.
    if replay {
        return;
    }
    if event_type == "user_message_chunk" {
        pane_state.prompt_submitted.notify_waiters();
    }

    let _ = event_tx.send(libslop::Record {
        cursor: None,
        source: "transcript".to_string(),
        event_type,
        pane_id: Some(pane_id.to_string()),
        payload: json!({"method": method, "params": params}),
    });
}

/// Convert Grok's camelCase hook envelope to slopd's canonical snake_case
/// payload while retaining the original envelope for consumers that need
/// Grok-specific fields.
pub(super) fn normalize_hook_payload(payload: Value) -> Value {
    let raw = payload.clone();
    let mut normalized = normalize_value(payload);
    if let Some(object) = normalized.as_object_mut() {
        object.insert("_grok_raw".to_string(), raw);
    }
    normalized
}

fn normalize_value(value: Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| (camel_to_snake(&key), normalize_value(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(normalize_value).collect()),
        other => other,
    }
}

fn camel_to_snake(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 4);
    let mut previous_lower_or_digit = false;
    for character in input.chars() {
        if character.is_ascii_uppercase() {
            if previous_lower_or_digit {
                out.push('_');
            }
            out.push(character.to_ascii_lowercase());
            previous_lower_or_digit = false;
        } else {
            out.push(character);
            previous_lower_or_digit = character.is_ascii_lowercase() || character.is_ascii_digit();
        }
    }
    out
}

pub(super) fn decode_record(record: &Value) -> Option<(String, Value)> {
    let update = record
        .pointer("/params/update")
        .or_else(|| record.get("update"))?;
    let event_type = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    Some((event_type, record.clone()))
}

pub(super) fn prompt_submitted(record: &Value) -> bool {
    record
        .pointer("/params/update/sessionUpdate")
        .or_else(|| record.pointer("/update/sessionUpdate"))
        .and_then(Value::as_str)
        == Some("user_message_chunk")
}

pub(super) fn transcript_state(
    record: &Value,
    active_prompt_id: Option<&str>,
) -> Option<libslop::PaneDetailedState> {
    let update = record
        .pointer("/params/update")
        .or_else(|| record.get("update"))?;
    let event = update.get("sessionUpdate").and_then(Value::as_str)?;
    let prompt_id = terminal_prompt_id(record);
    if event == "turn_completed" && prompt_id.is_some() && prompt_id == active_prompt_id {
        return Some(libslop::PaneDetailedState::Ready);
    }
    None
}

pub(super) fn terminal_prompt_id(record: &Value) -> Option<&str> {
    record
        .pointer("/params/update/prompt_id")
        .or_else(|| record.pointer("/params/update/promptId"))
        .or_else(|| record.pointer("/update/prompt_id"))
        .or_else(|| record.pointer("/update/promptId"))
        .and_then(Value::as_str)
}

pub(super) fn event_id(record: &Value) -> Option<String> {
    let value = record
        .pointer("/params/update/_meta/eventId")
        .or_else(|| record.pointer("/params/update/_meta/event_id"))
        .or_else(|| record.pointer("/update/_meta/eventId"))
        .or_else(|| record.pointer("/update/_meta/event_id"))?;
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

enum RewindStep {
    Rewind(usize),
    User(Option<usize>),
    Other,
}

fn rewind_step(record: &Value) -> RewindStep {
    let method = record.get("method").and_then(Value::as_str);
    let update = record
        .pointer("/params/update")
        .or_else(|| record.get("update"));
    let Some(update) = update else {
        return RewindStep::Other;
    };
    let kind = update.get("sessionUpdate").and_then(Value::as_str);
    if method == Some("_x.ai/session/update") && kind == Some("rewind_marker") {
        return update
            .get("target_prompt_index")
            .or_else(|| update.get("targetPromptIndex"))
            .and_then(Value::as_u64)
            .map(|target| RewindStep::Rewind(target as usize))
            .unwrap_or(RewindStep::Other);
    }
    let host_turn = update
        .pointer("/_meta/hostTurn")
        .or_else(|| update.pointer("/_meta/host_turn"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if method != Some("_x.ai/session/update") && kind == Some("user_message_chunk") && !host_turn {
        let index = update
            .pointer("/_meta/promptIndex")
            .or_else(|| update.pointer("/_meta/prompt_index"))
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        return RewindStep::User(index);
    }
    RewindStep::Other
}

/// Apply Grok's append-only rewind markers to a raw transcript. The returned
/// records are the live conversation branch; rewind markers themselves are not
/// part of replay, matching Grok's native session/load behavior.
pub(super) fn filter_rewind_records(records: Vec<(u64, Value)>) -> Vec<(u64, Value)> {
    if !records.iter().any(|(_, value)| {
        value
            .pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("rewind_marker")
    }) {
        return records;
    }

    let mut live = Vec::with_capacity(records.len());
    let mut prompt_starts = Vec::new();
    let mut last_prompt_index: Option<Option<usize>> = None;
    for record in records {
        match rewind_step(&record.1) {
            RewindStep::Rewind(target) => {
                let truncate_at = prompt_starts.get(target).copied().unwrap_or(live.len());
                live.truncate(truncate_at);
                prompt_starts.truncate(target);
                last_prompt_index = None;
            }
            RewindStep::User(index) => {
                if last_prompt_index != Some(index) {
                    prompt_starts.push(live.len());
                }
                last_prompt_index = Some(index);
                live.push(record);
            }
            RewindStep::Other => {
                last_prompt_index = None;
                live.push(record);
            }
        }
    }
    live
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(method: &str, update: Value) -> Value {
        json!({"method": method, "params": {"sessionId": "s", "update": update}})
    }

    #[test]
    fn hook_payload_is_normalized_and_preserves_raw() {
        let normalized = normalize_hook_payload(json!({
            "hookEventName": "SessionStart",
            "sessionId": "s",
            "transcriptPath": "/tmp/updates.jsonl",
            "notificationType": "idle_prompt",
        }));
        assert_eq!(normalized["session_id"], "s");
        assert_eq!(normalized["transcript_path"], "/tmp/updates.jsonl");
        assert_eq!(normalized["notification_type"], "idle_prompt");
        assert_eq!(normalized["_grok_raw"]["hookEventName"], "SessionStart");
    }

    #[test]
    fn reverse_requests_map_to_observable_interaction_states() {
        assert_eq!(
            reverse_interaction("session/request_permission"),
            Some((
                "PermissionRequest",
                libslop::PaneDetailedState::AwaitingInputPermission,
            ))
        );
        assert_eq!(
            reverse_interaction("x.ai/exit_plan_mode"),
            Some((
                "PermissionRequest",
                libslop::PaneDetailedState::AwaitingInputPermission,
            ))
        );
        assert_eq!(
            reverse_interaction("x.ai/ask_user_question"),
            Some((
                "Elicitation",
                libslop::PaneDetailedState::AwaitingInputElicitation,
            ))
        );
        assert_eq!(reverse_interaction("fs/read_text_file"), None);

        let wrapped = json!({
            "method": "x.ai/ask_user_question",
            "params": {"sessionId": "s", "toolCallId": "tool-1"},
        });
        let (method, params) = interaction_parts("_x.ai/ask_user_question", &wrapped);
        assert_eq!(method, "x.ai/ask_user_question");
        assert_eq!(params["sessionId"], "s");
    }

    #[test]
    fn rewind_filter_discards_the_superseded_branch() {
        let records = vec![
            (
                0,
                envelope(
                    "session/update",
                    json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p0"},"_meta":{"promptIndex":0}}),
                ),
            ),
            (
                1,
                envelope(
                    "session/update",
                    json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"a0"}}),
                ),
            ),
            (
                2,
                envelope(
                    "session/update",
                    json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p1"},"_meta":{"promptIndex":1}}),
                ),
            ),
            (
                3,
                envelope(
                    "session/update",
                    json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"dead"}}),
                ),
            ),
            (
                4,
                envelope(
                    "_x.ai/session/update",
                    json!({"sessionUpdate":"rewind_marker","target_prompt_index":1}),
                ),
            ),
            (
                5,
                envelope(
                    "session/update",
                    json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"replacement"},"_meta":{"promptIndex":1}}),
                ),
            ),
        ];
        let live = filter_rewind_records(records);
        assert_eq!(live.len(), 3);
        assert_eq!(live[0].0, 0);
        assert_eq!(live[1].0, 1);
        assert_eq!(live[2].0, 5);
    }

    #[test]
    fn transcript_decode_preserves_native_envelope() {
        let raw = envelope(
            "session/update",
            json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"hmm"}}),
        );
        let (kind, payload) = decode_record(&raw).unwrap();
        assert_eq!(kind, "agent_thought_chunk");
        assert_eq!(payload, raw);
    }

    #[test]
    fn completed_turn_is_a_terminal_state_backstop() {
        let completed = envelope(
            "_x.ai/session/update",
            json!({
                "sessionUpdate":"turn_completed",
                "stop_reason":"cancelled",
                "prompt_id":"prompt-1",
            }),
        );
        assert_eq!(
            transcript_state(&completed, Some("prompt-1")),
            Some(libslop::PaneDetailedState::Ready)
        );
        assert_eq!(
            transcript_state(
                &envelope(
                    "session/update",
                    json!({"sessionUpdate":"agent_message_chunk"}),
                ),
                Some("prompt-1")
            ),
            None
        );
        assert_eq!(transcript_state(&completed, Some("prompt-2")), None);
    }
}
