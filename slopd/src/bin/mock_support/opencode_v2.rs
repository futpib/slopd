//! V2 launch, session and prompt subset for the managed-TUI integration tests.
//! The server and TUI are separate processes, with real HTTP/SSE and terminal
//! input. Rich tool/permission scenarios remain in the real OpenCode suite.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

#[derive(Default)]
struct State {
    sessions: HashMap<String, Vec<Value>>,
    subscribers: Vec<mpsc::Sender<Value>>,
}

pub fn run() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--version") {
        println!("opencode v2.1.2");
        return;
    }
    for arg in &args {
        if arg.starts_with("--mock-") && arg != "--mock-v2" {
            super::reject_unknown_mock_option("mock_opencode v2", arg);
        }
    }
    let argument = |flag| {
        args.windows(2)
            .find(|pair| pair[0] == flag)
            .map(|pair| pair[1].clone())
    };
    let server = args.iter().any(|arg| arg == "serve");
    #[cfg(feature = "testing")]
    if let Ok(path) = std::env::var("MOCK_OPENCODE_LAUNCH_LOG") {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(
            file,
            "{}",
            json!({"role":if server {"server"} else {"tui"},
            "cli_config":std::env::var("OPENCODE_CLI_CONFIG_CONTENT").ok()})
        )
        .unwrap();
    }
    if server {
        assert!(args.iter().any(|arg| arg == "--stdio"));
        let port = argument("--port").expect("v2 server requires --port");
        serve(&format!("127.0.0.1:{port}"));
    } else {
        tui(
            &argument("--server").expect("v2 TUI requires --server"),
            &argument("--session").expect("v2 TUI requires --session"),
        );
    }
}

fn serve(address: &str) {
    let listener = TcpListener::bind(address).unwrap();
    let state = Arc::new(Mutex::new(State::default()));
    let request = reqwest::Client::new()
        .get(format!("http://{address}"))
        .basic_auth(
            "opencode",
            Some(std::env::var("OPENCODE_PASSWORD").unwrap()),
        )
        .build()
        .unwrap();
    let authorization = request.headers()["authorization"]
        .to_str()
        .unwrap()
        .to_string();
    println!("{}", json!({"url":format!("http://{address}")}));
    std::io::stdout().flush().unwrap();
    // Match serve --stdio ownership: loss of the launcher's pipe ends the server.
    std::thread::spawn(|| {
        let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
        std::process::exit(0);
    });
    for stream in listener.incoming() {
        let state = state.clone();
        let authorization = authorization.clone();
        std::thread::spawn(move || handle(state, stream.unwrap(), &authorization));
    }
}

fn handle(state: Arc<Mutex<State>>, mut stream: TcpStream, authorization: &str) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let request = line.clone();
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("").split('?').next().unwrap_or("");
    let mut size = 0;
    let mut authorized = false;
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                size = value.trim().parse().unwrap();
            }
            if key.eq_ignore_ascii_case("authorization") {
                authorized = value.trim() == authorization;
            }
        }
    }
    if !authorized {
        let _ = stream.write_all(super::json_response(401, "{}").as_bytes());
        return;
    }
    if path == "/api/event" {
        let (tx, rx) = mpsc::channel();
        state.lock().unwrap().subscribers.push(tx);
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n");
        loop {
            let data = match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(event) => format!("data: {event}\n\n"),
                Err(mpsc::RecvTimeoutError::Timeout) => ": heartbeat\n\n".into(),
                Err(_) => break,
            };
            if stream.write_all(data.as_bytes()).is_err() {
                break;
            }
        }
        return;
    }
    let mut body = vec![0; size];
    reader.read_exact(&mut body).unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let mut state = state.lock().unwrap();
    let session = path.strip_prefix("/api/session/").unwrap_or("");
    let (id, suffix) = session.split_once('/').unwrap_or((session, ""));
    let data = match (method, path) {
        ("GET", "/api/session/active") => json!({}),
        ("GET", "/api/session") => json!(
            state
                .sessions
                .keys()
                .map(|id| json!({"id":id}))
                .collect::<Vec<_>>()
        ),
        ("POST", "/api/session") => {
            let id = body["id"].as_str().unwrap().to_string();
            state.sessions.entry(id.clone()).or_default();
            json!({"id":id})
        }
        ("GET", _) if state.sessions.contains_key(id) => match suffix {
            "" => json!({"id":id}),
            "message" => json!(state.sessions[id]),
            "permission" | "form" => json!([]),
            _ => Value::Null,
        },
        ("POST", _) if suffix == "prompt" && state.sessions.contains_key(id) => {
            let text = body["text"].as_str().unwrap().to_string();
            let user = format!("msg_{}", uuid::Uuid::now_v7().simple());
            let assistant = format!("msg_{}", uuid::Uuid::now_v7().simple());
            let reply = format!("echo: {text}");
            state.sessions.get_mut(id).unwrap().extend([
                json!({"id":user,"type":"user","text":text}),
                json!({"id":assistant,"type":"assistant","content":[{"type":"text","text":reply}]}),
            ]);
            for event in [
                json!({"type":"session.inbox.enqueued","data":{"sessionID":id,"inboxID":user,
                    "item":{"type":"user","payload":{"text":text}}}}),
                json!({"type":"session.execution.started","data":{"sessionID":id}}),
                json!({"type":"session.text.ended","data":{"sessionID":id,
                    "assistantMessageID":assistant,"ordinal":0,"text":reply}}),
                json!({"type":"session.execution.succeeded","data":{"sessionID":id}}),
            ] {
                state
                    .subscribers
                    .retain(|tx| tx.send(event.clone()).is_ok());
            }
            json!({})
        }
        _ => Value::Null,
    };
    let status = if data.is_null() { 404 } else { 200 };
    let response = json!({"data":data,"cursor":{"next":null}}).to_string();
    let _ = stream.write_all(super::json_response(status, &response).as_bytes());
}

fn tui(server: &str, session: &str) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let client = reqwest::Client::new();
    let config = std::env::var("OPENCODE_CLI_CONFIG_CONTENT").unwrap_or_else(|_| "{}".into());
    let config: Value = json5::from_str(&config).unwrap();
    let mode = config["tabs"]["mode"].as_str().unwrap_or_else(|| {
        if config["tabs"]["enabled"] == false {
            "off"
        } else {
            "on"
        }
    });
    println!("mock OpenCode v2 session={session} tabs={mode}");
    let original = unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(0, &mut termios), 0);
        let original = termios;
        libc::cfmakeraw(&mut termios);
        libc::tcsetattr(0, libc::TCSANOW, &termios);
        original
    };
    print!("\x1b[?2004h");
    std::io::stdout().flush().unwrap();
    let mut input = Vec::new();
    let mut escape = Vec::new();
    let mut pasted = false;
    for byte in std::io::stdin().lock().bytes() {
        let Ok(byte) = byte else { break };
        if pasted {
            input.push(byte);
            if input.ends_with(b"\x1b[201~") {
                input.truncate(input.len() - 6);
                pasted = false;
            }
            continue;
        }
        if byte == 0x1b || !escape.is_empty() {
            escape.push(byte);
            if escape.len() > 2 && (byte.is_ascii_alphabetic() || byte == b'~') {
                if escape == b"\x1b[200~" {
                    pasted = true;
                }
                if escape == b"\x1b[1;2H" || escape == b"\x1b[1;2F" {
                    input.clear();
                }
                escape.clear();
            }
            continue;
        }
        match byte {
            b'\r' | b'\n' if !input.is_empty() => {
                let text = String::from_utf8(std::mem::take(&mut input)).unwrap();
                super::record_tui_input(&text);
                runtime.block_on(async {
                    client
                        .post(format!("{server}/api/session/{session}/prompt"))
                        .basic_auth(
                            "opencode",
                            Some(std::env::var("OPENCODE_PASSWORD").unwrap()),
                        )
                        .json(&json!({"text":text}))
                        .send()
                        .await
                        .unwrap()
                        .error_for_status()
                        .unwrap();
                });
                print!("\r\necho: {text}\r\n");
                std::io::stdout().flush().unwrap();
            }
            0x7f | 0x08 => {
                input.pop();
            }
            0x03 | 0x04 => break,
            byte if !byte.is_ascii_control() => input.push(byte),
            _ => {}
        }
    }
    unsafe {
        libc::tcsetattr(0, libc::TCSANOW, &original);
    }
}
