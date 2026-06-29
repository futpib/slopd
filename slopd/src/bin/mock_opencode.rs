//! A minimal fake `opencode` for slopd integration tests.
//!
//! slopd spawns the agent binary in a tmux pane and (for the opencode backend)
//! passes `--port <P> --hostname 127.0.0.1` plus `OPENCODE_SERVER_PASSWORD`. This
//! mock binds that port and serves the small HTTP API subset slopd's
//! [`OpencodeClient`](../../src/opencode.rs) uses, simulating turn state
//! transitions (idle → busy on prompt → idle) so the daemon's status-poll driver
//! has something real to observe.
//!
//! It blocks on `incoming()` so the tmux pane stays alive for the test, exactly
//! like a real long-running agent process.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SID: &str = "ses_mock";

#[derive(Default)]
struct MockState {
    busy: bool,
    /// (role, text) pairs — the conversation.
    messages: Vec<(String, String)>,
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
    // Accept quickly so slopd's session discovery (GET /session) doesn't time out.
    listener
        .set_nonblocking(false)
        .ok();

    let state: Arc<Mutex<MockState>> = Arc::new(Mutex::new(MockState::default()));

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let st = state.clone();
        // Single-threaded handling is fine — test traffic is tiny and serial.
        handle(st, stream);
    }
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

fn route(state: Arc<Mutex<MockState>>, method: &str, path: &str, body: &str) -> String {
    let (status, body_out) = match (method, path) {
        ("GET", "/global/health") => (200, r#"{"healthy":true,"version":"mock"}"#.to_string()),

        ("GET", "/session") => (
            200,
            r#"[{"id":"ses_mock","timeCreated":1,"title":"mock"}]"#.to_string(),
        ),

        ("GET", "/session/status") => {
            let busy = state.lock().unwrap().busy;
            let st = if busy { "busy" } else { "idle" };
            (200, format!(r#"{{"{SID}":{{"status":"{st}"}}}}"#))
        }

        ("POST", p) if p == format!("/session/{SID}/prompt_async") => {
            let text = extract_text(body);
            {
                let mut s = state.lock().unwrap();
                s.busy = true;
                s.messages.push(("user".to_string(), text.clone()));
                s.messages.push(("assistant".to_string(), format!("echo: {text}")));
            }
            // Simulate a turn: busy briefly, then idle (the driver observes the
            // transition back to ready).
            let st = state.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                st.lock().unwrap().busy = false;
            });
            (204, String::new())
        }

        ("POST", p) if p == format!("/session/{SID}/command") => (200, "{}".to_string()),

        ("POST", p) if p == format!("/session/{SID}/abort") => {
            state.lock().unwrap().busy = false;
            (200, "{}".to_string())
        }

        ("GET", p) if p == format!("/session/{SID}/message") => {
            let msgs = state.lock().unwrap().messages.clone();
            let arr: Vec<String> = msgs
                .iter()
                .map(|(role, text)| {
                    let text_json = serde_json::Value::String(text.clone()).to_string();
                    format!(r#"{{"info":{{"role":{}}},"parts":[{{"type":"text","text":{}}}]}}"#,
                        serde_json::Value::String(role.clone()).to_string(),
                        text_json)
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
