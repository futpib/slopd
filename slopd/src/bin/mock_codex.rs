//! Deterministic Codex app-server/TUI double used by integration tests.

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, broadcast};
use tokio_websockets::{ClientBuilder, Message, ServerBuilder};

#[derive(Default)]
struct State {
    threads: Mutex<HashMap<String, Vec<Value>>>,
    active: Mutex<HashMap<String, String>>,
    next_thread: AtomicU64,
    next_turn: AtomicU64,
}

fn socket_path() -> PathBuf {
    PathBuf::from(std::env::var_os("CODEX_HOME").unwrap_or_else(|| ".codex".into()))
        .join("app-server-control/app-server-control.sock")
}

async fn send_json<S>(ws: &mut tokio_websockets::WebSocketStream<S>, value: Value)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let _ = ws.send(Message::text(value.to_string())).await;
}

async fn connection(
    stream: UnixStream,
    state: Arc<State>,
    events: broadcast::Sender<Value>,
) -> Result<(), String> {
    let (_, mut ws) = ServerBuilder::new()
        .accept(stream)
        .await
        .map_err(|e| e.to_string())?;
    let mut event_rx = events.subscribe();
    let mut pending_approval: Option<(String, String)> = None;
    let mut overloaded_once = false;
    let mut may_drive_turns = false;
    loop {
        tokio::select! {
            event = event_rx.recv() => if let Ok(event) = event {
                send_json(&mut ws, event).await;
            },
            message = ws.next() => {
                let Some(Ok(message)) = message else { return Ok(()) };
                let Some(text) = message.as_text() else { continue };
                let Ok(value) = serde_json::from_str::<Value>(text) else { continue };
                if value.get("method").is_none() && value.get("id") == Some(&json!(900)) {
                    if let Some((thread_id, turn_id)) = pending_approval.take() {
                        state.active.lock().await.remove(&thread_id);
                        let _ = events.send(json!({"method":"turn/completed","params":{"threadId":thread_id,"turn":{"id":turn_id,"status":"completed"}}}));
                    }
                    continue;
                }
                let Some(method) = value.get("method").and_then(Value::as_str) else { continue };
                let id = value.get("id").cloned();
                let result = match method {
                    "initialize" => {
                        may_drive_turns = value.pointer("/params/clientInfo/name").and_then(Value::as_str) == Some("mock_codex_tui");
                        json!({"userAgent":"mock-codex","platformFamily":"unix","platformOs":"linux"})
                    },
                    "initialized" => continue,
                    "thread/start" => {
                        let id = format!("mock-thread-{}", state.next_thread.fetch_add(1, Ordering::Relaxed) + 1);
                        state.threads.lock().await.insert(id.clone(), Vec::new());
                        let thread = json!({"id":id,"turns":[],"cwd":std::env::current_dir().unwrap_or_default()});
                        let _ = events.send(json!({"method":"thread/started","params":{"thread":thread.clone()}}));
                        json!({"thread":thread})
                    }
                    "thread/resume" => {
                        let thread_id = value.pointer("/params/threadId").and_then(Value::as_str).unwrap_or("");
                        if let Some((pending_thread, turn_id)) = pending_approval.as_ref()
                            && pending_thread == thread_id {
                            send_json(&mut ws, json!({"method":"item/commandExecution/requestApproval","id":901,"params":{"threadId":thread_id,"turnId":turn_id,"itemId":"mock-item-replay","reason":"replayed approval"}})).await;
                        }
                        let threads = state.threads.lock().await;
                        let Some(turns) = threads.get(thread_id) else {
                            if let Some(id) = id { send_json(&mut ws, json!({"id":id,"error":{"code":-32000,"message":"thread not found"}})).await; }
                            continue;
                        };
                        json!({"thread":{"id":thread_id,"turns":turns}})
                    }
                    "thread/read" => {
                        let thread_id = value.pointer("/params/threadId").and_then(Value::as_str).unwrap_or("");
                        let threads = state.threads.lock().await;
                        let mut turns = threads.get(thread_id).cloned().unwrap_or_default();
                        let active_turn = state.active.lock().await.get(thread_id).cloned();
                        if let Some(active_turn) = active_turn.as_deref()
                            && let Some(turn) = turns.iter_mut().find(|turn| turn.get("id").and_then(Value::as_str) == Some(active_turn)) {
                            turn["status"] = json!("inProgress");
                        }
                        let status = if active_turn.is_some() { "active" } else { "idle" };
                        json!({"thread":{"id":thread_id,"status":{"type":status},"turns":turns}})
                    }
                    "thread/list" => {
                        let threads = state.threads.lock().await;
                        json!({"data":threads.keys().map(|id| json!({"id":id})).collect::<Vec<_>>(),"nextCursor":null})
                    }
                    "thread/fork" => {
                        let source = value.pointer("/params/threadId").and_then(Value::as_str).unwrap_or("");
                        let mut threads = state.threads.lock().await;
                        let turns = threads.get(source).cloned().unwrap_or_default();
                        let id = format!("mock-thread-{}", state.next_thread.fetch_add(1, Ordering::Relaxed) + 1);
                        threads.insert(id.clone(), turns.clone());
                        json!({"thread":{"id":id,"turns":turns}})
                    }
                    "turn/start" => {
                        if !may_drive_turns {
                            if let Some(id) = id { send_json(&mut ws, json!({"id":id,"error":{"code":-32600,"message":"only the TUI may start turns"}})).await; }
                            continue;
                        }
                        let thread_id = value.pointer("/params/threadId").and_then(Value::as_str).unwrap_or("").to_string();
                        let prompt = value.pointer("/params/input/0/text").and_then(Value::as_str).unwrap_or("").to_string();
                        if prompt == "__disconnect__" { return Ok(()); }
                        if prompt == "__overload__" && !overloaded_once {
                            overloaded_once = true;
                            if let Some(id) = id { send_json(&mut ws, json!({"id":id,"error":{"code":-32001,"message":"Server overloaded; retry later."}})).await; }
                            continue;
                        }
                        let turn_id = format!("mock-turn-{}", state.next_turn.fetch_add(1, Ordering::Relaxed) + 1);
                        state.active.lock().await.insert(thread_id.clone(), turn_id.clone());
                        let _ = events.send(json!({"method":"turn/started","params":{"threadId":thread_id,"turn":{"id":turn_id}}}));
                        let turn = json!({"id":turn_id,"items":[
                            {"type":"userMessage","content":[{"type":"text","text":prompt}]},
                            {"type":"agentMessage","text":format!("echo: {prompt}")}
                        ]});
                        state.threads.lock().await.entry(thread_id.clone()).or_default().push(turn.clone());
                        let _ = events.send(json!({"method":"item/completed","params":{"threadId":thread_id,"turnId":turn_id,"item":turn["items"][1].clone()}}));
                        if prompt == "__active__" {
                            // Keep the turn active until a turn/steer request.
                        } else if prompt == "__approval__" {
                            pending_approval = Some((thread_id.clone(), turn_id.clone()));
                            let _ = events.send(json!({"method":"item/commandExecution/requestApproval","id":900,"params":{"threadId":thread_id,"turnId":turn_id,"itemId":"mock-item","reason":"mock approval"}}));
                        } else {
                            state.active.lock().await.remove(&thread_id);
                            let _ = events.send(json!({"method":"turn/completed","params":{"threadId":thread_id,"turn":turn}}));
                        }
                        json!({"turn":{"id":turn_id}})
                    }
                    "turn/steer" => {
                        if !may_drive_turns {
                            if let Some(id) = id { send_json(&mut ws, json!({"id":id,"error":{"code":-32600,"message":"only the TUI may steer turns"}})).await; }
                            continue;
                        }
                        let Some(turn_id) = value.pointer("/params/expectedTurnId").and_then(Value::as_str) else {
                            if let Some(id) = id { send_json(&mut ws, json!({"id":id,"error":{"code":-32600,"message":"missing expectedTurnId"}})).await; }
                            continue;
                        };
                        let thread_id = value.pointer("/params/threadId").and_then(Value::as_str).unwrap_or("");
                        state.active.lock().await.remove(thread_id);
                        let _ = events.send(json!({"method":"turn/completed","params":{"threadId":thread_id,"turn":{"id":turn_id,"status":"completed"}}}));
                        json!({})
                    },
                    "turn/interrupt" => {
                        let thread_id = value.pointer("/params/threadId").and_then(Value::as_str).unwrap_or("");
                        state.active.lock().await.remove(thread_id);
                        json!({})
                    },
                    "mock/disconnect" => return Ok(()),
                    _ => {
                        if let Some(id) = id { send_json(&mut ws, json!({"id":id,"error":{"code":-32601,"message":"method not found"}})).await; }
                        continue;
                    }
                };
                if let Some(id) = id { send_json(&mut ws, json!({"id":id,"result":result})).await; }
            }
        }
    }
}

async fn serve() -> Result<(), String> {
    let socket = socket_path();
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(
        socket.parent().unwrap().join("mock-codex.pid"),
        std::process::id().to_string(),
    )
    .map_err(|e| e.to_string())?;
    if socket.exists() {
        let _ = std::fs::remove_file(&socket);
    }
    let listener = UnixListener::bind(&socket).map_err(|e| e.to_string())?;
    let state = Arc::new(State::default());
    let (events, _) = broadcast::channel(1024);
    loop {
        let (stream, _) = listener.accept().await.map_err(|e| e.to_string())?;
        tokio::spawn(connection(stream, state.clone(), events.clone()));
    }
}

async fn tui(args: &[String]) -> Result<(), String> {
    let cwd = args
        .iter()
        .position(|arg| arg == "-C")
        .and_then(|index| args.get(index + 1))
        .ok_or_else(|| "remote TUI requires an explicit -C working directory".to_string())?;
    if !PathBuf::from(cwd).is_absolute() {
        return Err("remote TUI -C must be absolute".to_string());
    }
    let stream = UnixStream::connect(socket_path())
        .await
        .map_err(|e| e.to_string())?;
    let uri = "ws://localhost/".parse().unwrap();
    let (mut ws, _) = ClientBuilder::from_uri(uri)
        .connect_on(stream)
        .await
        .map_err(|e| e.to_string())?;
    send_json(&mut ws, json!({"method":"initialize","id":1,"params":{"clientInfo":{"name":"mock_codex_tui","title":"Mock Codex","version":"1"}}})).await;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut initialized = false;
    let mut thread_id: Option<String> = None;
    let mut active_turn: Option<String> = None;
    let mut pending_dialog: Option<Value> = None;
    let mut next_id = 3_u64;
    loop {
        tokio::select! {
            line = lines.next_line(), if initialized && thread_id.is_some() => {
                let Some(line) = line.map_err(|e| e.to_string())? else { break };
                let prompt = line.trim_end_matches('\r');
                if prompt.is_empty() { continue; }
                if let Some(id) = pending_dialog.take() {
                    let decision = if matches!(prompt.to_ascii_lowercase().as_str(), "y" | "yes" | "accept" | "approve") { "accept" } else { "decline" };
                    send_json(&mut ws, json!({"id":id,"result":{"decision":decision}})).await;
                    continue;
                }
                let id = next_id;
                next_id += 1;
                let thread_id = thread_id.as_deref().unwrap();
                if let Some(turn_id) = active_turn.as_deref() {
                    send_json(&mut ws, json!({"method":"turn/steer","id":id,"params":{"threadId":thread_id,"expectedTurnId":turn_id,"input":[{"type":"text","text":prompt}]}})).await;
                } else {
                    send_json(&mut ws, json!({"method":"turn/start","id":id,"params":{"threadId":thread_id,"input":[{"type":"text","text":prompt}]}})).await;
                }
            }
            message = ws.next() => {
                let Some(Ok(message)) = message else { break };
                let Some(text) = message.as_text() else { continue };
                let value: Value = serde_json::from_str(text).unwrap_or_default();
                if value.get("id") == Some(&json!(1)) {
                    initialized = true;
                    send_json(&mut ws, json!({"method":"initialized","params":{}})).await;
                    if let Some(index) = args.iter().position(|arg| arg == "resume")
                        && let Some(resume_id) = args.get(index + 1)
                    {
                        thread_id = Some(resume_id.clone());
                        send_json(&mut ws, json!({"method":"thread/resume","id":2,"params":{"threadId":resume_id}})).await;
                    } else {
                        send_json(&mut ws, json!({"method":"thread/start","id":2,"params":{}})).await;
                    }
                }
                if value.get("id") == Some(&json!(2))
                    && let Some(id) = value.pointer("/result/thread/id").and_then(Value::as_str) {
                    thread_id = Some(id.to_string());
                }
                match value.get("method").and_then(Value::as_str) {
                    Some("turn/started") => active_turn = value.pointer("/params/turn/id").and_then(Value::as_str).map(str::to_string),
                    Some("turn/completed") => active_turn = None,
                    Some(method) if method.ends_with("requestApproval") || method.contains("requestUserInput") => {
                        pending_dialog = value.get("id").cloned();
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = if args.first().map(String::as_str) == Some("serve") {
        serve().await
    } else if args.starts_with(&[
        "app-server".to_string(),
        "daemon".to_string(),
        "start".to_string(),
    ]) {
        if UnixStream::connect(socket_path()).await.is_ok() {
            return;
        }
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("serve")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        child.map(|_| ()).map_err(|e| e.to_string())
    } else {
        tui(&args).await
    };
    if let Err(error) = result {
        eprintln!("mock_codex: {error}");
        std::process::exit(1);
    }
}
