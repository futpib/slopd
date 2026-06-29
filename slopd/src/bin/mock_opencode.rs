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
}

fn main() {
    let mut port: Option<u16> = None;
    let mut hostname = "127.0.0.1".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => port = args.next().and_then(|s| s.parse().ok()),
            "--hostname" => hostname = args.next().unwrap_or(hostname),
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

        ("GET", "/session") => (
            200,
            r#"[{"id":"ses_mock","timeCreated":1,"title":"mock"}]"#.to_string(),
        ),

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
                // Stream a realistic turn over SSE: busy → user msg → assistant
                // msg → (after a beat) idle.
                emit(&state, format!(r#"{{"type":"session.status","properties":{{"sessionID":"{SID}","status":{{"type":"busy"}}}}}}"#));
                emit(&state, format!(r#"{{"type":"message.updated","properties":{{"sessionID":"{SID}","info":{{"role":"user"}}}}}}"#));
                emit(&state, part_updated_event(SID, "user", &text));
                emit(&state, format!(r#"{{"type":"message.updated","properties":{{"sessionID":"{SID}","info":{{"role":"assistant"}}}}}}"#));
                let st = state.clone();
                let echo = format!("echo: {text}");
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(300));
                    emit(&st, part_updated_event(SID, "assistant", &echo));
                    st.lock().unwrap().busy = false;
                    emit(&st, format!(r#"{{"type":"session.idle","properties":{{"sessionID":"{SID}"}}}}"#));
                });
                (204, String::new())
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
