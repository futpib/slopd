//! A minimal fake `opencode` for slopd integration tests.
//!
//! slopd spawns the agent binary in a tmux pane and (for the opencode backend)
//! passes `--port <P> --hostname 127.0.0.1` plus `OPENCODE_SERVER_PASSWORD`. This
//! mock binds that port and serves the HTTP API subset slopd's [`OpencodeClient`]
//! uses, with the REAL opencode shapes (busy = present in `/session/status` as
//! `{"<sid>":{"type":"busy"}}`; idle = absent; an SSE `/event` stream carrying
//! `session.status` / `message.*` / `session.idle`). It simulates a turn
//! (idle → busy → idle) so the SSE driver has something real to observe.
//!
//! Blocks on `incoming()` so the tmux pane stays alive for the test, like a real
//! long-running agent. Connections are handled one-per-thread so a long-lived SSE
//! stream doesn't block other API calls.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SID: &str = "ses_mock";
/// A child session id used to simulate an opencode subagent (parent = SID).
const CHILD_SID: &str = "ses_mock_child";
/// A second top-level session the human can switch the TUI to (no parent), used
/// to exercise slopd following `tui.session.select`.
const SID_2: &str = "ses_mock2";

struct MockState {
    busy: bool,
    /// (role, text) pairs — the conversation.
    messages: Vec<(String, String)>,
    /// SSE event broadcast (JSON strings). Only one subscriber (the driver).
    event_rx: Option<mpsc::Receiver<String>>,
    event_tx: mpsc::Sender<String>,
    /// Test hook: the first "boom" prompt fails (session.error) so slopd's
    /// auto-continue can be exercised; subsequent "boom" prompts succeed.
    boom_failed: bool,
    /// Mirrors real opencode: a fresh TUI lists NO session until one is created
    /// (POST /session). Exercises slopd's ensure_session() create-if-absent path.
    session_created: bool,
    /// True once a second session (SID_2) exists — the human navigated to / created
    /// it in the TUI. When set, GET /session lists it too.
    second_session: bool,
    /// The session id last selected via POST /tui/select-session (records that
    /// slopd imposed its session on the TUI at spawn).
    selected_session: Option<String>,
}

fn main() {
    let mut port: Option<u16> = None;
    let mut hostname = "127.0.0.1".to_string();
    // `-s <id>` means a session is being resumed → it already exists (mirrors
    // real opencode, where `opencode -s <id>` makes that session present).
    let mut resumed_session = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => port = args.next().and_then(|s| s.parse().ok()),
            "--hostname" => hostname = args.next().unwrap_or(hostname),
            "-s" | "--session" => {
                resumed_session = true;
                let _ = args.next(); // consume the session id
            }
            _ => { /* ignore unknown flags (e.g. opencode passthrough) */ }
        }
    }
    let port = port.expect("mock_opencode: --port is required");

    let listener = TcpListener::bind((hostname.as_str(), port)).expect("mock_opencode: bind failed");
    listener.set_nonblocking(false).ok();

    let (event_tx, event_rx) = mpsc::channel::<String>();
    let state: Arc<Mutex<MockState>> = Arc::new(Mutex::new(MockState {
        busy: false,
        messages: Vec::new(),
        event_rx: Some(event_rx),
        event_tx,
        boom_failed: false,
        session_created: resumed_session,
        second_session: false,
        selected_session: None,
    }));

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let st = state.clone();
        std::thread::spawn(move || handle(st, stream));
    }
}

fn emit(state: &Arc<Mutex<MockState>>, event_json: String) {
    // Ignore send errors (no SSE subscriber connected yet).
    let _ = state.lock().unwrap().event_tx.send(event_json);
}

fn handle(state: Arc<Mutex<MockState>>, mut stream: std::net::TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    if path == "/event" {
        // SSE stream — long-lived, handled in its own thread.
        stream_sse(state, stream);
        return;
    }

    let mut content_len: usize = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(v) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
            content_len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_len];
    if content_len > 0 {
        let _ = reader.read_exact(&mut body);
    }
    let body = String::from_utf8_lossy(&body).to_string();

    let response = route(state, &method, &path, &body);
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Serve the SSE event stream until the client disconnects.
fn stream_sse(state: Arc<Mutex<MockState>>, mut stream: std::net::TcpStream) {
    // Write the response headers, then stream events.
    let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
    if stream.write_all(header.as_bytes()).is_err() {
        return;
    }
    let rx = match state.lock().unwrap().event_rx.take() {
        Some(rx) => rx,
        None => return, // another subscriber already took it
    };
    // server.connected first, matching real opencode.
    let _ = stream.write_all(b"event: server.connected\ndata: {\"type\":\"server.connected\",\"properties\":{}}\n\n");
    loop {
        match rx.recv_timeout(Duration::from_millis(800)) {
            Ok(json) => {
                let msg = format!("data: {json}\n\n");
                if stream.write_all(msg.as_bytes()).is_err() {
                    return; // client gone
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Heartbeat comment keeps the connection alive.
                if stream.write_all(b": heartbeat\n\n").is_err() {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn route(state: Arc<Mutex<MockState>>, method: &str, path: &str, body: &str) -> String {
    let (status, body_out) = match (method, path) {
        ("GET", "/global/health") => (200, r#"{"healthy":true,"version":"mock"}"#.to_string()),

        ("GET", "/session") => {
            // Real opencode 1.17.x shape: creation time nested at time.created (NOT
            // a top-level timeCreated). A fresh TUI lists no session until created.
            let s = state.lock().unwrap();
            let mut sessions: Vec<String> = Vec::new();
            if s.session_created {
                sessions.push(format!(r#"{{"id":"{SID}","time":{{"created":100,"updated":100}},"title":"mock"}}"#));
            }
            if s.second_session {
                // Newer than SID, so a (correct) latest-by-created discovery would
                // even pick this one — but the test asserts slopd follows the
                // explicit tui.session.select, not "latest".
                sessions.push(format!(r#"{{"id":"{SID_2}","time":{{"created":200,"updated":200}},"title":"mock2"}}"#));
            }
            (200, format!("[{}]", sessions.join(",")))
        }

        ("POST", "/session") => {
            state.lock().unwrap().session_created = true;
            (200, format!(r#"{{"id":"{SID}","time":{{"created":100,"updated":100}},"title":"mock"}}"#))
        }

        // slopd imposes its session on the TUI at spawn (and the TUI reports the
        // human's navigation via the tui.session.select SSE event). Record the
        // selection so a test can assert slopd called it.
        ("POST", "/tui/select-session") => {
            let sid = extract_session_id(body);
            state.lock().unwrap().selected_session = sid;
            (200, "true".to_string())
        }

        ("GET", "/session/status") => {
            // REAL shape: busy sessions present with {"type":"busy"}; idle absent.
            let busy = state.lock().unwrap().busy;
            if busy {
                (200, format!(r#"{{"{SID}":{{"type":"busy"}}}}"#))
            } else {
                (200, "{}".to_string())
            }
        }

        ("POST", p) if p == format!("/session/{SID}/prompt_async") => {
            let text = extract_text(body);
            // Test hook: a "switch" prompt simulates the human navigating the TUI to
            // a different top-level session (SID_2). The server creates that session
            // and emits the tui.session.select event slopd follows; the prompt
            // itself produces no turn.
            if text == "switch" {
                state.lock().unwrap().second_session = true;
                emit(&state, serde_json::json!({"type":"tui.session.select","properties":{"sessionID":SID_2}}).to_string());
                (204, String::new())
            } else {
            // Test hook: a "boom" prompt fails the first time (session.error) so
            // slopd's auto-continue retry can be exercised; it succeeds on retry.
            let fail_this = text == "boom" && !state.lock().unwrap().boom_failed;
            if fail_this {
                state.lock().unwrap().boom_failed = true;
                state.lock().unwrap().messages.push(("user".to_string(), text));
                emit(&state, format!(r#"{{"type":"session.status","properties":{{"sessionID":"{SID}","status":{{"type":"busy"}}}}}}"#));
                emit(&state, format!(r#"{{"type":"session.error","properties":{{"sessionID":"{SID}"}}}}"#));
                state.lock().unwrap().busy = false;
                (204, String::new())
            } else {
                {
                    let mut s = state.lock().unwrap();
                    s.busy = true;
                    s.messages.push(("user".to_string(), text.clone()));
                    s.messages.push(("assistant".to_string(), format!("echo: {text}")));
                }
                let uses_tool = text.contains("tool");
                let uses_subagent = text.contains("subagent");
                let asks_question = text.contains("question");
                // busy + user message
                emit(&state, serde_json::json!({"type":"session.status","properties":{"sessionID":SID,"status":{"type":"busy"}}}).to_string());
                emit(&state, serde_json::json!({"type":"message.updated","properties":{"sessionID":SID,"info":{"role":"user"}}}).to_string());
                emit(&state, part_updated_event(SID, "user", &text));
                if uses_subagent {
                    // Spawn a child session (opencode subagent): session.created with parentID.
                    emit(&state, serde_json::json!({"type":"session.created","properties":{"sessionID":CHILD_SID,"info":{"id":CHILD_SID,"parentID":SID,"agent":"general","title":"mock subagent"}}}).to_string());
                    emit(&state, serde_json::json!({"type":"session.status","properties":{"sessionID":CHILD_SID,"status":{"type":"busy"}}}).to_string());
                    emit(&state, serde_json::json!({"type":"message.updated","properties":{"sessionID":CHILD_SID,"info":{"role":"assistant"}}}).to_string());
                } else if asks_question {
                    // The `question` tool is opencode's elicitation (agent asking the user).
                    emit(&state, tool_part_event(SID, "question", "pending", serde_json::json!({"input":{"message":"what size?"}})));
                } else if uses_tool {
                    emit(&state, tool_part_event(SID, "bash", "pending", serde_json::json!({})));
                    emit(&state, tool_part_event(SID, "bash", "running", serde_json::json!({"input":{"command":"cat sample.txt"}})));
                }
                emit(&state, serde_json::json!({"type":"message.updated","properties":{"sessionID":SID,"info":{"role":"assistant"}}}).to_string());
                let st = state.clone();
                let echo = format!("echo: {text}");
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(400));
                    if uses_subagent {
                        emit(&st, serde_json::json!({"type":"session.idle","properties":{"sessionID":CHILD_SID}}).to_string());
                    } else if asks_question {
                        emit(&st, tool_part_event(SID, "question", "completed", serde_json::json!({"output":"large"})));
                    } else if uses_tool {
                        emit(&st, tool_part_event(SID, "bash", "completed", serde_json::json!({"output":"hello-world"})));
                    }
                    emit(&st, part_updated_event(SID, "assistant", &echo));
                    st.lock().unwrap().busy = false;
                    emit(&st, serde_json::json!({"type":"session.idle","properties":{"sessionID":SID}}).to_string());
                });
                (204, String::new())
            }
            }
        }

        ("POST", p) if p == format!("/session/{SID}/command") => (200, "{}".to_string()),

        ("POST", p) if p == format!("/session/{SID}/abort") => {
            state.lock().unwrap().busy = false;
            emit(&state, format!(r#"{{"type":"session.idle","properties":{{"sessionID":"{SID}"}}}}"#));
            (200, "{}".to_string())
        }

        ("GET", p) if p == format!("/session/{SID}/message") => {
            let msgs = state.lock().unwrap().messages.clone();
            let arr: Vec<String> = msgs
                .iter()
                .map(|(role, text)| {
                    let text_json = serde_json::Value::String(text.clone()).to_string();
                    format!(
                        r#"{{"info":{{"role":{}}},"parts":[{{"type":"text","text":{}}}]}}"#,
                        serde_json::Value::String(role.clone()).to_string(),
                        text_json
                    )
                })
                .collect();
            (200, format!("[{}]", arr.join(",")))
        }

        // The followed second session has its own (distinct) transcript, so a test
        // can prove `slopctl transcript` reads SID_2 after the follow, not SID.
        ("GET", p) if p == format!("/session/{SID_2}/message") => {
            (200, r#"[{"info":{"role":"user"},"parts":[{"type":"text","text":"second session message"}]}]"#.to_string())
        }

        _ => (404, r#"{"error":"not found"}"#.to_string()),
    };

    if status == 204 {
        "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
    } else {
        let reason = if status == 200 { "OK" } else { "Not Found" };
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body_out}",
            len = body_out.len(),
        )
    }
}

/// Build a `message.part.updated` event for a TOOL part with the given state
/// status (pending/running/completed), merging `body` into `part.state`. Mirrors
/// how real opencode streams tool activity over the SSE bus.
fn tool_part_event(sid: &str, tool: &str, status: &str, body: serde_json::Value) -> String {
    let mut state = serde_json::json!({ "status": status });
    if let serde_json::Value::Object(m) = body {
        if let serde_json::Value::Object(ref mut s) = state {
            for (k, v) in m {
                s.insert(k, v);
            }
        }
    }
    serde_json::json!({
        "type": "message.part.updated",
        "properties": {
            "sessionID": sid,
            "part": { "type": "tool", "tool": tool, "callID": "call_mock", "state": state }
        }
    })
    .to_string()
}

/// Build a `message.part.updated` event JSON for the SSE stream.
fn part_updated_event(sid: &str, _role: &str, text: &str) -> String {
    let text_json = serde_json::Value::String(text.to_string()).to_string();
    format!(
        r#"{{"type":"message.part.updated","properties":{{"sessionID":"{}","part":{{"type":"text","text":{}}}}}}}"#,
        sid, text_json
    )
}

/// Extract the first `text` part from a `{"parts":[{"type":"text","text":...}]}` body.
fn extract_text(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(parts) = v.get("parts").and_then(|p| p.as_array()) {
            for p in parts {
                if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                        return t.to_string();
                    }
                }
            }
        }
    }
    body.to_string()
}

/// Extract `sessionID` from a `{"sessionID":"ses_..."}` request body.
fn extract_session_id(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("sessionID").and_then(|s| s.as_str()).map(str::to_string))
}
