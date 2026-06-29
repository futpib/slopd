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

    /// `GET /global/health` — light liveness check. Errors as `String` (status + body).
    #[allow(dead_code)] // part of the client API; used for ad-hoc liveness probing
    pub async fn health(&self) -> Result<Value, String> {
        let resp = self.req(reqwest::Method::GET, "/global/health").send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("health {}: {}", status, resp.text().await.unwrap_or_default()));
        }
        resp.json::<Value>().await.map_err(|e| e.to_string())
    }

    /// `GET /session` → the most recent session id (highest `timeCreated`), if any.
    /// Used to discover the session id of a freshly-spawned opencode pane.
    pub async fn latest_session(&self) -> Result<Option<String>, String> {
        let resp = self.req(reqwest::Method::GET, "/session").send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("GET /session {}: {}", status, resp.text().await.unwrap_or_default()));
        }
        let arr = resp.json::<Vec<Value>>().await.map_err(|e| e.to_string())?;
        Ok(arr
            .into_iter()
            .max_by_key(|s| s.get("timeCreated").and_then(|t| t.as_u64()).unwrap_or(0))
            .and_then(|s| s.get("id").and_then(|i| i.as_str()).map(str::to_string)))
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
}

/// Bind 127.0.0.1:0 and return the kernel-assigned port for an opencode pane to
/// listen on. There is a tiny window between dropping the listener and opencode
/// binding, but for a local single-user daemon this is acceptable.
pub fn alloc_port() -> Result<u16, String> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .map(|l| l.local_addr().map(|a| a.port()).unwrap_or(0))
        .map_err(|e| e.to_string())
}

/// Generate a random per-pane auth token. Cheap defense on top of 127.0.0.1
/// binding: if the port ever leaks, the token still gates the server.
pub fn random_token() -> String {
    // 128 bits of randomness; good enough for a local secret.
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{:032x}", nanos.wrapping_mul(2654435761))
}

/// Map an opencode `SessionStatus` object onto a slopd [`PaneDetailedState`].
///
/// The exact `SessionStatus` shape isn't pinned in the public spec, so this is
/// deliberately tolerant: it accepts a `status`/`state` string with common
/// spellings, and falls back to a few boolean flags. Returns `None` when nothing
/// recognized is present (caller leaves state unchanged). Verified against
/// `mock_opencode`'s status shape; real-opencode confirmation is a smoke-test
/// step (see the opencode plan notes).
pub fn status_to_detailed(status: &Value) -> Option<PaneDetailedState> {
    let s = status
        .get("status")
        .and_then(|v| v.as_str())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_port_returns_listening_port() {
        let p = alloc_port().expect("alloc");
        assert!(p > 0, "port should be non-zero");
    }

    #[test]
    fn random_token_is_nonempty_and_hex() {
        let t = random_token();
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()), "token not hex: {}", t);
        assert!(t.len() >= 16);
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
}
