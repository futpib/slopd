//! OpenCode backend support.
//!
//! `opencode` runs its TUI as a client of an embedded HTTP server (see
//! <https://opencode.ai/docs/server>). slopd spawns the TUI pane with a pinned
//! `--port` on 127.0.0.1 and drives it over that HTTP API instead of via
//! Claude-style hooks + jsonl tailing. This module is the transport + mapping
//! layer; the per-pane driver loop and RPC dispatch live in [`crate`]
//! (they need the daemon's shared state/event types).

use libslop::PaneDetailedState;
use serde_json::{json, Value};

/// Basic-auth username the opencode server expects (its default).
const AUTH_USER: &str = "opencode";

/// Connection details for one opencode pane's embedded HTTP server.
///
/// Cheap to clone (shares a `reqwest::Client`); the driver and RPC handlers
/// each hold one.
#[derive(Clone)]
pub struct OpencodeClient {
    base: String,
    token: Option<String>,
    client: reqwest::Client,
}

impl OpencodeClient {
    /// Connect to an opencode server on `port` with an optional basic-auth token.
    pub fn new(port: u16, token: Option<String>) -> Self {
        Self {
            base: format!("http://127.0.0.1:{}", port),
            token,
            client: reqwest::Client::builder()
                // No per-request timeout: SSE / long polls would hit it. The
                // driver's individual calls are short; send/transcript cap
                // themselves where needed.
                .build()
                .expect("reqwest client build"),
        }
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{}", self.base, path);
        let mut r = self.client.request(method, url);
        if let Some(t) = &self.token {
            r = r.basic_auth(AUTH_USER, Some(t));
        }
        r
    }

    /// Create the session slopd will drive and return its id, via `POST /session`.
    ///
    /// We deliberately do NOT adopt a *discovered* session here. A freshly-booted
    /// TUI opens its own empty session, but opencode garbage-collects an unused
    /// empty session, so latching onto "the latest existing session" can leave
    /// slopd pointed at an id the server later 404s on (observed live: send fails
    /// with "Session not found"). POSTing our own session yields one that is
    /// guaranteed to persist and accept prompts; the caller then points the TUI at
    /// it via `select_session`, converging the two views. Verified against real
    /// opencode 1.17.x.
    pub async fn ensure_session(&self) -> Result<String, String> {
        let resp = self
            .req(reqwest::Method::POST, "/session")
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("POST /session {}: {}", status, resp.text().await.unwrap_or_default()));
        }
        let v = resp.json::<Value>().await.map_err(|e| e.to_string())?;
        v.get("id")
            .and_then(|i| i.as_str())
            .map(str::to_string)
            .ok_or_else(|| "POST /session returned no id".to_string())
    }

    /// `POST /tui/select-session` — command the TUI to display `session_id`, so the
    /// session slopd drives is the one the human sees in the pane. Without this the
    /// TUI may sit on a different (e.g. restored) session than slopd discovered,
    /// leaving `slopctl ps`/`send`/`transcript` describing a conversation the pane
    /// never shows. Best-effort: a failure is logged by the caller, not fatal.
    pub async fn select_session(&self, session_id: &str) -> Result<(), String> {
        let resp = self
            .req(reqwest::Method::POST, "/tui/select-session")
            .json(&serde_json::json!({ "sessionID": session_id }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("POST /tui/select-session {}: {}", status, resp.text().await.unwrap_or_default()));
        }
        Ok(())
    }

    /// `GET /session` → every session id (used to tell "idle but exists" from
    /// "still booting", since idle sessions are absent from `/session/status`).
    pub async fn session_ids(&self) -> Result<Vec<String>, String> {
        let resp = self.req(reqwest::Method::GET, "/session").send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("GET /session {}: {}", status, resp.text().await.unwrap_or_default()));
        }
        let arr = resp.json::<Vec<Value>>().await.map_err(|e| e.to_string())?;
        Ok(arr
            .into_iter()
            .filter_map(|s| s.get("id").and_then(|i| i.as_str()).map(str::to_string))
            .collect())
    }

    /// `GET /session/status` → the status object for one session. `Ok(None)` if the
    /// session is absent from the map (e.g. not yet created).
    pub async fn session_status(&self, session_id: &str) -> Result<Option<Value>, String> {
        let resp = self
            .req(reqwest::Method::GET, "/session/status")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("GET /session/status {}: {}", status, resp.text().await.unwrap_or_default()));
        }
        let v = resp.json::<Value>().await.map_err(|e| e.to_string())?;
        Ok(v.get(session_id).cloned())
    }

    /// `POST /session/:id/prompt_async` — non-blocking prompt submit. Returns once
    /// the server acknowledges (204 / 2xx), which is slopd's "prompt accepted"
    /// signal (the analogue of Claude's `UserPromptSubmit` hook).
    pub async fn send_message(&self, session_id: &str, text: &str) -> Result<(), String> {
        let body = json!({ "parts": [{ "type": "text", "text": text }] });
        let resp = self
            .req(reqwest::Method::POST, &format!("/session/{}/prompt_async", session_id))
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        if status.as_u16() == 204 || status.is_success() {
            Ok(())
        } else {
            Err(format!("prompt_async {}: {}", status, resp.text().await.unwrap_or_default()))
        }
    }

    /// `POST /session/:id/command` — execute a slash command.
    pub async fn send_command(&self, session_id: &str, command: &str) -> Result<(), String> {
        // A bare "/foo" becomes command="foo" (no leading slash) per the API shape.
        let cmd = command.trim_start_matches('/');
        let body = json!({ "command": cmd, "arguments": "" });
        let resp = self
            .req(reqwest::Method::POST, &format!("/session/{}/command", session_id))
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(format!("command {}: {}", status, resp.text().await.unwrap_or_default()))
        }
    }

    /// `POST /session/:id/abort` — interrupt the running turn.
    pub async fn abort(&self, session_id: &str) -> Result<(), String> {
        let resp = self
            .req(reqwest::Method::POST, &format!("/session/{}/abort", session_id))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(format!("abort {}: {}", status, resp.text().await.unwrap_or_default()))
        }
    }

    /// `GET /session/:id/message` — full conversation as `{ info, parts }[]`.
    pub async fn messages(&self, session_id: &str) -> Result<Vec<Value>, String> {
        let resp = self
            .req(reqwest::Method::GET, &format!("/session/{}/message", session_id))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("GET /message {}: {}", status, resp.text().await.unwrap_or_default()));
        }
        resp.json::<Vec<Value>>().await.map_err(|e| e.to_string())
    }

    /// `GET /event` as a prepared SSE request. The driver reads the response
    /// body chunk-by-chunk and parses `data:` lines (see [`event_from_line`]).
    pub fn events(&self) -> reqwest::RequestBuilder {
        self.req(reqwest::Method::GET, "/event")
            .header(reqwest::header::ACCEPT, "text/event-stream")
    }
}

/// Bind 127.0.0.1:0 and return the kernel-assigned port for an opencode pane to
/// listen on. There is a tiny window between dropping the listener and opencode
/// binding, but for a local single-user daemon this is acceptable.
pub fn alloc_port() -> Result<u16, String> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .map(|l| l.local_addr().map(|a| a.port()).unwrap_or(0))
        .map_err(|e| e.to_string())
}

/// Map an opencode session-status object onto a slopd [`PaneDetailedState`].
///
/// Verified shape (real opencode 1.17.x): a busy session is present in
/// `GET /session/status` as `{"<sessionID>":{"type":"busy"}}`; an idle session is
/// **absent** from the map. So this maps the per-session value's `type` field
/// (`busy`, …). `status`/`state` are also accepted for tolerance. Returns `None`
/// when nothing recognized is present (caller leaves state unchanged).
pub fn status_to_detailed(status: &Value) -> Option<PaneDetailedState> {
    // Real opencode uses `type`; accept `status`/`state` as aliases.
    let s = status
        .get("type")
        .and_then(|v| v.as_str())
        .or_else(|| status.get("status").and_then(|v| v.as_str()))
        .or_else(|| status.get("state").and_then(|v| v.as_str()))
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    if s == "idle" || s == "ready" || s == "waiting" || s == "completed" {
        return Some(PaneDetailedState::Ready);
    }
    if s == "busy" || s == "running" || s == "processing" || s == "streaming" {
        return Some(PaneDetailedState::BusyProcessing);
    }
    if s == "tool" || s == "tool_use" || s == "busy_tool_use" {
        return Some(PaneDetailedState::BusyToolUse);
    }
    if s == "permission" || s == "awaiting_permission" || s == "awaiting_input_permission" {
        return Some(PaneDetailedState::AwaitingInputPermission);
    }
    if s == "compacting" {
        return Some(PaneDetailedState::BusyCompacting);
    }
    // Boolean fallbacks.
    if status.get("busy").and_then(|v| v.as_bool()) == Some(true) {
        return Some(PaneDetailedState::BusyProcessing);
    }
    if status.get("awaitingPermission").and_then(|v| v.as_bool()) == Some(true) {
        return Some(PaneDetailedState::AwaitingInputPermission);
    }
    None
}

/// One normalized transcript record: (`type`, payload) — matches the shape slopd
/// broadcasts as `source: "transcript"` events.
pub type TranscriptRecord = (String, Value);

/// Map `GET /session/:id/message` output (`{ info, parts }[]`) onto slopd
/// transcript records. Each message becomes a record typed by its role
/// (`user` / `assistant` / …), carrying the message info + its parts.
pub fn messages_to_records(messages: &[Value]) -> Vec<TranscriptRecord> {
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        let role = m
            .get("info")
            .and_then(|i| i.get("role"))
            .and_then(|r| r.as_str())
            .unwrap_or("assistant")
            .to_string();
        let payload = json!({ "info": m.get("info").cloned().unwrap_or(Value::Null), "parts": m.get("parts").cloned().unwrap_or(Value::Array(vec![])) });
        out.push((role, payload));
    }
    out
}

// --- SSE event mapping (verified against real opencode 1.17.x) ---
//
// Events arrive as `data: {"id":"evt_…","type":"<name>","properties":{…}}\n\n`.
// Session-scoped events carry `properties.sessionID`; server-lifecycle events
// (server.connected, server.heartbeat, plugin.added, catalog.updated, …) do not.

/// Parse one SSE `data:` payload into a JSON value. Returns None for blanks/comments.
pub fn event_from_line(payload: &str) -> Option<Value> {
    let payload = payload.trim();
    if payload.is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(payload).ok()
}

/// The session id an event pertains to (from `properties.sessionID`/`sessionId`),
/// or None for server-lifecycle events.
pub fn event_session_id(event: &Value) -> Option<&str> {
    event
        .get("properties")
        .and_then(|p| p.get("sessionID").or_else(|| p.get("sessionId")))
        .and_then(|v| v.as_str())
}

/// The event `type` (e.g. `session.idle`, `message.part.updated`).
pub fn event_type(event: &Value) -> Option<&str> {
    event.get("type").and_then(|v| v.as_str())
}

/// For a `session.created` event, the parent session id
/// (`properties.info.parentID`). opencode runs subagents as child sessions, so a
/// `session.created` whose parent is the pane's main session means a subagent was
/// spawned (verified against real opencode 1.17.x).
pub fn session_created_parent(event: &Value) -> Option<&str> {
    if event_type(event) != Some("session.created") {
        return None;
    }
    event
        .get("properties")
        .and_then(|p| p.get("info"))
        .and_then(|i| i.get("parentID"))
        .and_then(|v| v.as_str())
}

/// For a `tui.session.select` event, the session id the TUI navigated to
/// (`properties.sessionID`). slopd follows this so the session it drives stays the
/// one the human is actually looking at in the pane.
pub fn tui_selected_session(event: &Value) -> Option<&str> {
    if event_type(event) != Some("tui.session.select") {
        return None;
    }
    event_session_id(event)
}

/// Map an SSE event to a detailed-state transition, if it implies one.
///
/// Verified against real opencode 1.17.x. Tool activity rides on
/// `message.part.updated` with `properties.part.type == "tool"` and
/// `part.state.status` (pending/running → BusyToolUse; completed/error →
/// BusyProcessing) — there is no separate `tool.execute.*` bus event (those are
/// in-process plugin events). `permission.asked` and `session.compacted` are
/// mapped per the plugin docs (not yet individually observed on the bus).
pub fn event_to_detailed(event: &Value) -> Option<PaneDetailedState> {
    let t = event_type(event)?;
    let props = event.get("properties").unwrap_or(&Value::Null);
    match t {
        "session.idle" => Some(PaneDetailedState::Ready),
        // A failed turn leaves the agent idle (opencode doesn't always follow
        // session.error with session.idle); recover to Ready so the pane isn't
        // stuck busy. (Auto-RETRY is handled separately on this event.)
        "session.error" => Some(PaneDetailedState::Ready),
        // session.status carries the status object in properties.status.
        "session.status" => props.get("status").and_then(|s| status_to_detailed(s)),
        "session.compacted" => Some(PaneDetailedState::BusyCompacting),
        "permission.asked" => Some(PaneDetailedState::AwaitingInputPermission),
        // A message is being produced → working. For a tool part, distinguish
        // tool-use from plain processing via part.state.status.
        "message.part.updated" => {
            let part = props.get("part").unwrap_or(&Value::Null);
            if part.get("type").and_then(|v| v.as_str()) == Some("tool") {
                let tool = part.get("tool").and_then(|v| v.as_str()).unwrap_or("");
                let status = part
                    .get("state")
                    .and_then(|s| s.get("status"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("running");
                // opencode's `question` tool IS Claude's elicitation (the agent
                // asking the user a clarifying question) — not a regular tool.
                if tool == "question" {
                    return match status {
                        "completed" | "error" => Some(PaneDetailedState::BusyProcessing),
                        _ => Some(PaneDetailedState::AwaitingInputElicitation),
                    };
                }
                match status {
                    "completed" | "error" => Some(PaneDetailedState::BusyProcessing),
                    _ => Some(PaneDetailedState::BusyToolUse),
                }
            } else {
                Some(PaneDetailedState::BusyProcessing)
            }
        }
        "message.updated" | "message.removed" | "message.part.removed" => {
            Some(PaneDetailedState::BusyProcessing)
        }
        "step-start" | "step-finish" => Some(PaneDetailedState::BusyProcessing),
        _ => None,
    }
}

/// Map an SSE event onto a Claude-hook event name (+ a hook-shaped payload) so
/// `slopctl listen --hook` / `wait --hook` work uniformly across backends. The
/// hook *name* is the cross-backend contract; the payload carries the opencode
/// `properties` under `opencode` plus best-effort hook fields (so `--where`
/// predicates on `hook_event_name`/`tool_name` work; deeper fields are
/// backend-specific).
pub fn event_to_hook(event: &Value) -> Option<(&'static str, Value)> {
    let t = event_type(event)?;
    let props = event.get("properties").unwrap_or(&Value::Null);
    let sid = event_session_id(event).unwrap_or("");
    let part = || props.get("part").unwrap_or(&Value::Null);

    let (name, extra): (&'static str, Value) = match t {
        "session.idle" => ("Stop", json!({})),
        "session.error" => ("StopFailure", json!({})),
        "permission.asked" => ("PermissionRequest", json!({})),
        "session.compacted" => ("PreCompact", json!({})),
        "message.updated" if props.get("info").and_then(|i| i.get("role")).and_then(|r| r.as_str()) == Some("user") => {
            ("UserPromptSubmit", json!({}))
        }
        // Tool start/end ride on message.part.updated (part.type == "tool").
        "message.part.updated" if part().get("type").and_then(|v| v.as_str()) == Some("tool") => {
            let tool_name = part().get("tool").and_then(|v| v.as_str()).unwrap_or("");
            let status = part()
                .get("state")
                .and_then(|s| s.get("status"))
                .and_then(|v| v.as_str())
                .unwrap_or("running");
            let input = part().get("state").and_then(|s| s.get("input")).cloned().unwrap_or(Value::Null);
            // The `question` tool is opencode's elicitation → Claude Elicitation hooks.
            if tool_name == "question" {
                let name = match status {
                    "completed" | "error" => "ElicitationResult",
                    _ => "Elicitation",
                };
                (name, json!({ "tool_name": tool_name, "tool_input": input }))
            } else {
                match status {
                    "completed" | "error" => (
                        "PostToolUse",
                        json!({ "tool_name": tool_name, "tool_input": input }),
                    ),
                    _ => (
                        "PreToolUse",
                        json!({ "tool_name": tool_name, "tool_input": input }),
                    ),
                }
            }
        }
        _ => return None,
    };
    let payload = json!({
        "session_id": sid,
        "hook_event_name": name,
        "opencode_event": t,
        "properties": props,
        "extra": extra,
    });
    Some((name, payload))
}

/// Map an SSE event to a transcript record (type, payload), if it carries
/// conversation content. `message.updated` → a message record (role from
/// `properties.info.role`); `message.part.updated` → a part record.
pub fn event_to_transcript(event: &Value) -> Option<TranscriptRecord> {
    let t = event_type(event)?;
    let props = event.get("properties").unwrap_or(&Value::Null);
    match t {
        "message.updated" => {
            let role = props.get("info").and_then(|i| i.get("role")).and_then(|r| r.as_str()).unwrap_or("user").to_string();
            Some((role, props.clone()))
        }
        "message.part.updated" => {
            // A part (text/tool/etc.). Type it by the part's own type if present.
            let part_type = props.get("part").and_then(|p| p.get("type")).and_then(|v| v.as_str()).unwrap_or("part").to_string();
            Some((part_type, props.clone()))
        }
        _ => None,
    }
}

/// Whether an SSE event signals a turn failure (for auto-continue). opencode
/// emits `session.error` when a turn errors.
pub fn event_is_failure(event: &Value) -> bool {
    event_type(event) == Some("session.error")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_port_returns_listening_port() {
        let p = alloc_port().expect("alloc");
        assert!(p > 0, "port should be non-zero");
    }

    #[test]
    fn status_to_detailed_idle_states() {
        for s in ["idle", "ready", "waiting", "completed"] {
            assert_eq!(status_to_detailed(&json!({"status": s})), Some(PaneDetailedState::Ready), "{}", s);
        }
    }

    #[test]
    fn status_to_detailed_busy_states() {
        assert_eq!(status_to_detailed(&json!({"status": "running"})), Some(PaneDetailedState::BusyProcessing));
        assert_eq!(status_to_detailed(&json!({"status": "tool"})), Some(PaneDetailedState::BusyToolUse));
        assert_eq!(status_to_detailed(&json!({"status": "compacting"})), Some(PaneDetailedState::BusyCompacting));
        assert_eq!(status_to_detailed(&json!({"status": "permission"})), Some(PaneDetailedState::AwaitingInputPermission));
    }

    #[test]
    fn status_to_detailed_accepts_state_alias_and_case() {
        assert_eq!(status_to_detailed(&json!({"state": "IDLE"})), Some(PaneDetailedState::Ready));
        assert_eq!(status_to_detailed(&json!({"state": "Running"})), Some(PaneDetailedState::BusyProcessing));
    }

    #[test]
    fn status_to_detailed_boolean_fallbacks() {
        assert_eq!(status_to_detailed(&json!({"busy": true})), Some(PaneDetailedState::BusyProcessing));
        assert_eq!(status_to_detailed(&json!({"awaitingPermission": true})), Some(PaneDetailedState::AwaitingInputPermission));
    }

    #[test]
    fn status_to_detailed_unknown_is_none() {
        assert_eq!(status_to_detailed(&json!({"status": "frobnicating"})), None);
        assert_eq!(status_to_detailed(&json!({})), None);
    }

    #[test]
    fn messages_to_records_maps_roles() {
        let msgs = vec![
            json!({ "info": { "role": "user", "id": "u1" }, "parts": [{ "type": "text", "text": "hi" }] }),
            json!({ "info": { "role": "assistant", "id": "a1" }, "parts": [{ "type": "text", "text": "hello" }] }),
        ];
        let recs = messages_to_records(&msgs);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].0, "user");
        assert_eq!(recs[1].0, "assistant");
        assert_eq!(recs[0].1["parts"][0]["text"], "hi");
    }

    // --- SSE event mapping (real opencode shapes) ---

    fn ev(t: &str, session_id: &str) -> Value {
        json!({ "id": "evt_test", "type": t, "properties": { "sessionID": session_id } })
    }

    #[test]
    fn event_session_id_and_type_extract() {
        let e = ev("session.idle", "ses_x");
        assert_eq!(event_session_id(&e), Some("ses_x"));
        assert_eq!(event_type(&e), Some("session.idle"));
        // server-lifecycle events have no sessionID.
        let no_sid = json!({ "type": "server.heartbeat", "properties": {} });
        assert_eq!(event_session_id(&no_sid), None);
    }

    #[test]
    fn event_to_detailed_maps_real_event_types() {
        let sid = "ses_x";
        assert_eq!(event_to_detailed(&ev("session.idle", sid)), Some(PaneDetailedState::Ready));
        assert_eq!(
            event_to_detailed(&json!({"type":"session.status","properties":{"sessionID":sid,"status":{"type":"busy"}}})),
            Some(PaneDetailedState::BusyProcessing)
        );
        assert_eq!(event_to_detailed(&ev("message.updated", sid)), Some(PaneDetailedState::BusyProcessing));
        // Real tool shape: message.part.updated with part.type=tool + state.status.
        let tool_running = json!({"type":"message.part.updated","properties":{"sessionID":sid,"part":{"type":"tool","tool":"bash","state":{"status":"running"}}}});
        let tool_completed = json!({"type":"message.part.updated","properties":{"sessionID":sid,"part":{"type":"tool","tool":"bash","state":{"status":"completed"}}}});
        let text_part = json!({"type":"message.part.updated","properties":{"sessionID":sid,"part":{"type":"text","text":"hi"}}});
        assert_eq!(event_to_detailed(&tool_running), Some(PaneDetailedState::BusyToolUse));
        assert_eq!(event_to_detailed(&tool_completed), Some(PaneDetailedState::BusyProcessing));
        assert_eq!(event_to_detailed(&text_part), Some(PaneDetailedState::BusyProcessing));
        // plugin-event names (tool.execute.*) are NOT on the SSE bus → no transition.
        assert_eq!(event_to_detailed(&ev("tool.execute.before", sid)), None);
        assert_eq!(event_to_detailed(&ev("step-start", sid)), Some(PaneDetailedState::BusyProcessing));
        assert_eq!(event_to_detailed(&ev("permission.asked", sid)), Some(PaneDetailedState::AwaitingInputPermission));
        assert_eq!(event_to_detailed(&ev("session.compacted", sid)), Some(PaneDetailedState::BusyCompacting));
        // A failed turn recovers to Ready.
        assert_eq!(event_to_detailed(&ev("session.error", sid)), Some(PaneDetailedState::Ready));
        // Unrelated event → no transition.
        assert_eq!(event_to_detailed(&ev("catalog.updated", sid)), None);
    }

    #[test]
    fn event_to_hook_synthesizes_hook_names() {
        let sid = "ses_x";
        // session.idle → Stop
        let (n, p) = event_to_hook(&ev("session.idle", sid)).unwrap();
        assert_eq!(n, "Stop");
        assert_eq!(p["hook_event_name"], "Stop");
        // session.error → StopFailure
        assert_eq!(event_to_hook(&ev("session.error", sid)).unwrap().0, "StopFailure");
        // user message → UserPromptSubmit (assistant message → no hook)
        let user_msg = json!({"type":"message.updated","properties":{"sessionID":sid,"info":{"role":"user"}}});
        assert_eq!(event_to_hook(&user_msg).unwrap().0, "UserPromptSubmit");
        let asst_msg = json!({"type":"message.updated","properties":{"sessionID":sid,"info":{"role":"assistant"}}});
        assert!(event_to_hook(&asst_msg).is_none());
        // tool part pending → PreToolUse; completed → PostToolUse (carries tool name)
        let tool_pending = json!({"type":"message.part.updated","properties":{"sessionID":sid,"part":{"type":"tool","tool":"bash","state":{"status":"pending","input":{"command":"ls"}}}}});
        let (n, p) = event_to_hook(&tool_pending).unwrap();
        assert_eq!(n, "PreToolUse");
        assert_eq!(p["extra"]["tool_name"], "bash");
        let tool_done = json!({"type":"message.part.updated","properties":{"sessionID":sid,"part":{"type":"tool","tool":"bash","state":{"status":"completed"}}}});
        assert_eq!(event_to_hook(&tool_done).unwrap().0, "PostToolUse");
        // permission/compaction
        assert_eq!(event_to_hook(&ev("permission.asked", sid)).unwrap().0, "PermissionRequest");
        assert_eq!(event_to_hook(&ev("session.compacted", sid)).unwrap().0, "PreCompact");
        // unrelated → none
        assert!(event_to_hook(&ev("server.heartbeat", sid)).is_none());
    }

    #[test]
    fn event_to_detailed_question_tool_is_elicitation() {
        // opencode's `question` tool = Claude's elicitation.
        let pending = json!({"type":"message.part.updated","properties":{"sessionID":"s","part":{"type":"tool","tool":"question","state":{"status":"pending"}}}});
        let completed = json!({"type":"message.part.updated","properties":{"sessionID":"s","part":{"type":"tool","tool":"question","state":{"status":"completed"}}}});
        assert_eq!(event_to_detailed(&pending), Some(PaneDetailedState::AwaitingInputElicitation));
        assert_eq!(event_to_detailed(&completed), Some(PaneDetailedState::BusyProcessing));
    }

    #[test]
    fn event_to_hook_question_tool_is_elicitation() {
        let pending = json!({"type":"message.part.updated","properties":{"sessionID":"s","part":{"type":"tool","tool":"question","state":{"status":"pending","input":{"message":"size?"}}}}});
        let completed = json!({"type":"message.part.updated","properties":{"sessionID":"s","part":{"type":"tool","tool":"question","state":{"status":"completed"}}}});
        assert_eq!(event_to_hook(&pending).unwrap().0, "Elicitation");
        assert_eq!(event_to_hook(&completed).unwrap().0, "ElicitationResult");
    }

    #[test]
    fn session_created_parent_detects_subagent() {
        // A child session whose parent is the main session = a subagent.
        let child = json!({"type":"session.created","properties":{"sessionID":"ses_child","info":{"id":"ses_child","parentID":"ses_main","agent":"general"}}});
        assert_eq!(session_created_parent(&child), Some("ses_main"));
        // A session.created with no parent (top-level) → None.
        let top = json!({"type":"session.created","properties":{"sessionID":"ses_top","info":{"id":"ses_top"}}});
        assert_eq!(session_created_parent(&top), None);
        // Non-session.created events → None.
        assert_eq!(session_created_parent(&ev("session.idle", "s")), None);
    }

    #[test]
    fn tui_selected_session_extracts_target() {
        // The real event shape captured from opencode 1.17.x.
        let e = json!({"id":"evt_1","type":"tui.session.select","properties":{"sessionID":"ses_target"}});
        assert_eq!(tui_selected_session(&e), Some("ses_target"));
        // Other event types → None (only tui.session.select drives a follow).
        assert_eq!(tui_selected_session(&ev("session.idle", "ses_x")), None);
        assert_eq!(tui_selected_session(&ev("session.created", "ses_x")), None);
    }

    #[test]
    fn event_to_transcript_maps_messages() {
        let sid = "ses_x";
        let msg = json!({"type":"message.updated","properties":{"sessionID":sid,"info":{"role":"assistant"}}});
        let (rtype, _payload) = event_to_transcript(&msg).expect("message.updated → transcript");
        assert_eq!(rtype, "assistant");
        let part = json!({"type":"message.part.updated","properties":{"sessionID":sid,"part":{"type":"text","text":"hi"}}});
        let (rtype, payload) = event_to_transcript(&part).expect("message.part.updated → transcript");
        assert_eq!(rtype, "text");
        assert_eq!(payload["part"]["text"], "hi");
        assert!(event_to_transcript(&ev("session.idle", sid)).is_none());
    }

    #[test]
    fn event_is_failure_detects_session_error() {
        assert!(event_is_failure(&ev("session.error", "ses_x")));
        assert!(!event_is_failure(&ev("session.idle", "ses_x")));
    }

    #[test]
    fn status_to_detailed_matches_real_opencode_shape() {
        // Real shape: {"type":"busy"} for a busy session.
        assert_eq!(status_to_detailed(&json!({"type":"busy"})), Some(PaneDetailedState::BusyProcessing));
        assert_eq!(status_to_detailed(&json!({"type":"idle"})), Some(PaneDetailedState::Ready));
        // status/state still accepted as aliases.
        assert_eq!(status_to_detailed(&json!({"status":"busy"})), Some(PaneDetailedState::BusyProcessing));
    }

    #[test]
    fn event_from_line_parses_data_payload() {
        let v = event_from_line(r#"{"type":"session.idle","properties":{"sessionID":"s"}}"#).unwrap();
        assert_eq!(v["type"], "session.idle");
        assert!(event_from_line("").is_none());
        assert!(event_from_line("not json").is_none());
    }
}
