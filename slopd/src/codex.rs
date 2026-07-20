//! Codex app-server backend support.
//!
//! Codex's managed daemon exposes a JSON-RPC-like protocol as WebSocket frames
//! over a Unix socket. A [`CodexClient`] owns one connection, multiplexes RPC
//! responses, and broadcasts notifications to the pane driver.

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_websockets::{ClientBuilder, Message};

async fn connect_initialized(
    socket_path: &Path,
) -> Result<tokio_websockets::WebSocketStream<UnixStream>, String> {
    let stream = UnixStream::connect(socket_path).await.map_err(|e| {
        format!(
            "connect to Codex app-server {}: {}",
            socket_path.display(),
            e
        )
    })?;
    let uri = "ws://localhost/"
        .parse()
        .map_err(|e| format!("Codex websocket URI: {e}"))?;
    let (mut ws, _) = ClientBuilder::from_uri(uri)
        .connect_on(stream)
        .await
        .map_err(|e| format!("Codex websocket handshake: {e}"))?;
    ws.send(Message::text(
        json!({
            "method": "initialize", "id": 1,
            "params": {"clientInfo": {
                "name": "slopd", "title": "slopd", "version": env!("CARGO_PKG_VERSION")
            }}
        })
        .to_string(),
    ))
    .await
    .map_err(|e| format!("Codex initialize send: {e}"))?;
    loop {
        let message = ws
            .next()
            .await
            .ok_or_else(|| "Codex app-server closed during initialize".to_string())?
            .map_err(|e| format!("Codex initialize receive: {e}"))?;
        let Some(text) = message.as_text() else {
            continue;
        };
        let value: Value =
            serde_json::from_str(text).map_err(|e| format!("Codex initialize JSON: {e}"))?;
        if value.get("id").and_then(Value::as_u64) == Some(1) {
            if let Some(error) = value.get("error") {
                return Err(format!("Codex initialize failed: {error}"));
            }
            break;
        }
    }
    ws.send(Message::text(
        json!({"method":"initialized","params":{}}).to_string(),
    ))
    .await
    .map_err(|e| format!("Codex initialized send: {e}"))?;
    Ok(ws)
}

#[derive(Clone)]
pub struct CodexClient {
    commands: mpsc::Sender<Command>,
    notifications: broadcast::Sender<Value>,
}

enum Command {
    Request {
        method: String,
        params: Value,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Respond {
        id: Value,
        result: Value,
        reply: oneshot::Sender<Result<Value, String>>,
    },
}

struct PendingRequest {
    method: String,
    params: Value,
    attempts: u8,
    reply: oneshot::Sender<Result<Value, String>>,
}

fn fail_command(command: Command, message: &str) {
    let reply = match command {
        Command::Request { reply, .. } | Command::Respond { reply, .. } => reply,
    };
    let _ = reply.send(Err(message.to_string()));
}

impl CodexClient {
    pub async fn connect(socket_path: &Path) -> Result<Self, String> {
        let mut ws = connect_initialized(socket_path).await?;
        let socket_path = socket_path.to_path_buf();

        let (commands_tx, mut commands_rx) = mpsc::channel::<Command>(32);
        let (notifications, _) = broadcast::channel::<Value>(1024);
        let notifications_task = notifications.clone();
        tokio::spawn(async move {
            let mut next_id = 2_u64;
            loop {
                let mut pending = std::collections::HashMap::<u64, PendingRequest>::new();
                'connected: loop {
                    tokio::select! {
                    command = commands_rx.recv() => {
                        let Some(command) = command else { return };
                        match command {
                            Command::Request { method, params, reply } => {
                                let id = next_id;
                                next_id += 1;
                                let message = json!({"method": method, "id": id, "params": params});
                                match ws.send(Message::text(message.to_string())).await {
                                    Ok(()) => { pending.insert(id, PendingRequest { method, params, attempts: 0, reply }); }
                                    Err(e) => { let _ = reply.send(Err(e.to_string())); break 'connected; }
                                }
                            }
                            Command::Respond { id, result, reply } => {
                                let message = json!({"id": id, "result": result});
                                match ws.send(Message::text(message.to_string())).await {
                                    Ok(()) => { let _ = reply.send(Ok(Value::Null)); }
                                    Err(e) => { let _ = reply.send(Err(e.to_string())); break 'connected; }
                                }
                            }
                        }
                    }
                    incoming = ws.next() => {
                        let Some(incoming) = incoming else { break 'connected };
                        let Ok(incoming) = incoming else { break 'connected };
                        let Some(text) = incoming.as_text() else { continue };
                        let Ok(value) = serde_json::from_str::<Value>(text) else { continue };
                        if let Some(id) = value.get("id").and_then(Value::as_u64) {
                            if value.get("method").is_none() {
                                if let Some(mut request) = pending.remove(&id) {
                                    let overloaded = value.pointer("/error/code").and_then(Value::as_i64) == Some(-32001);
                                    if overloaded && request.attempts < 5 {
                                        request.attempts += 1;
                                        tokio::time::sleep(std::time::Duration::from_millis(25_u64 << request.attempts)).await;
                                        let retry_id = next_id;
                                        next_id += 1;
                                        let message = json!({"method": request.method, "id": retry_id, "params": request.params});
                                        if ws.send(Message::text(message.to_string())).await.is_ok() {
                                            pending.insert(retry_id, request);
                                        } else {
                                            let _ = request.reply.send(Err("Codex app-server connection lost during overload retry".to_string()));
                                            break 'connected;
                                        }
                                    } else {
                                        let result = match value.get("error") {
                                            Some(error) => Err(error.to_string()),
                                            None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
                                        };
                                        let _ = request.reply.send(result);
                                    }
                                }
                            } else {
                                // Server-initiated requests are forwarded with their
                                // request id so slopctl can answer on this connection.
                                let _ = notifications_task.send(value);
                            }
                        } else {
                            let _ = notifications_task.send(value);
                        }
                    }
                    }
                }
                for (_, request) in pending.drain() {
                    let _ = request.reply.send(Err(
                        "Codex app-server connection lost; retry the request".to_string(),
                    ));
                }
                let _ = notifications_task.send(json!({
                    "method":"slopd/codexConnection",
                    "params":{"status":"reconnecting"}
                }));
                let mut delay = std::time::Duration::from_millis(100);
                loop {
                    tokio::select! {
                        command = commands_rx.recv() => match command {
                            Some(command) => fail_command(command, "Codex app-server is reconnecting; retry the request"),
                            None => return,
                        },
                        _ = tokio::time::sleep(delay) => match connect_initialized(&socket_path).await {
                            Ok(new_ws) => {
                                ws = new_ws;
                                let _ = notifications_task.send(json!({
                                    "method":"slopd/codexConnection",
                                    "params":{"status":"connected"}
                                }));
                                break;
                            }
                            Err(_) => {
                                delay = std::cmp::min(delay * 2, std::time::Duration::from_secs(5));
                            }
                        }
                    }
                }
            }
        });

        Ok(Self {
            commands: commands_tx,
            notifications,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.notifications.subscribe()
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::Request {
                method: method.to_string(),
                params,
                reply,
            })
            .await
            .map_err(|_| "Codex app-server connection closed".to_string())?;
        receive
            .await
            .map_err(|_| "Codex app-server connection closed".to_string())?
    }

    /// Answer a server-initiated approval or elicitation request.
    pub async fn respond(&self, id: Value, result: Value) -> Result<(), String> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::Respond { id, result, reply })
            .await
            .map_err(|_| "Codex app-server connection closed".to_string())?;
        receive
            .await
            .map_err(|_| "Codex app-server connection closed".to_string())??;
        Ok(())
    }

    pub async fn resume_thread(&self, thread_id: &str) -> Result<Value, String> {
        self.request("thread/resume", json!({"threadId": thread_id}))
            .await
    }

    pub async fn read_thread(&self, thread_id: &str) -> Result<Value, String> {
        self.request(
            "thread/read",
            json!({"threadId": thread_id, "includeTurns": true}),
        )
        .await
    }

    pub async fn thread_ids(&self) -> Result<std::collections::HashSet<String>, String> {
        let result = self.request("thread/list", json!({"limit":100})).await?;
        Ok(result
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|thread| thread.get("id").and_then(Value::as_str).map(str::to_string))
            .collect())
    }

    pub async fn fork_thread(&self, thread_id: &str) -> Result<String, String> {
        let result = self
            .request("thread/fork", json!({"threadId": thread_id}))
            .await?;
        response_thread_id(&result)
    }

    pub async fn start_turn(&self, thread_id: &str, prompt: &str) -> Result<String, String> {
        let result = self
            .request(
                "turn/start",
                json!({
                    "threadId": thread_id,
                    "input": [{"type":"text", "text":prompt}]
                }),
            )
            .await?;
        result
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("Codex turn/start returned no turn id: {result}"))
    }

    pub async fn steer_turn(
        &self,
        thread_id: &str,
        turn_id: &str,
        prompt: &str,
    ) -> Result<(), String> {
        self.request(
            "turn/steer",
            json!({
                "threadId": thread_id,
                "expectedTurnId": turn_id,
                "input": [{"type":"text", "text":prompt}]
            }),
        )
        .await
        .map(|_| ())
    }

    pub async fn interrupt_turn(&self, thread_id: &str, turn_id: &str) -> Result<(), String> {
        self.request(
            "turn/interrupt",
            json!({"threadId":thread_id, "turnId":turn_id}),
        )
        .await
        .map(|_| ())
    }
}

pub async fn wait_for_thread_started(
    events: &mut broadcast::Receiver<Value>,
    timeout: std::time::Duration,
    existing: &std::collections::HashSet<String>,
) -> Result<String, String> {
    tokio::time::timeout(timeout, async {
        loop {
            match events.recv().await {
                Ok(value)
                    if value.get("method").and_then(Value::as_str) == Some("thread/started") =>
                {
                    if let Some(id) = value.pointer("/params/thread/id").and_then(Value::as_str)
                        && !existing.contains(id)
                    {
                        return Ok(id.to_string());
                    }
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return Err("Codex app-server connection closed".to_string()),
            }
        }
    })
    .await
    .map_err(|_| "timed out waiting for Codex TUI to create a thread".to_string())?
}

fn response_thread_id(result: &Value) -> Result<String, String> {
    result
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("Codex response returned no thread id: {result}"))
}

pub fn socket_path(codex_home: Option<&Path>) -> PathBuf {
    codex_home
        .map(Path::to_path_buf)
        .unwrap_or_else(|| dirs_home().join(".codex"))
        .join("app-server-control/app-server-control.sock")
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub async fn ensure_daemon(
    program: &Path,
    codex_home: Option<&Path>,
    socket: &Path,
) -> Result<(), String> {
    if socket.exists() && UnixStream::connect(socket).await.is_ok() {
        return Ok(());
    }
    let mut command = tokio::process::Command::new(program);
    command.args(["app-server", "daemon", "start"]);
    if let Some(home) = codex_home {
        command.env("CODEX_HOME", home);
    }
    let output = command
        .output()
        .await
        .map_err(|e| format!("start Codex daemon: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "start Codex daemon: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    for _ in 0..50 {
        if socket.exists() && UnixStream::connect(socket).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Err(format!("Codex daemon did not create {}", socket.display()))
}

pub fn thread_records(result: &Value, pane_id: &str) -> Vec<libslop::Record> {
    let mut records = Vec::new();
    let Some(turns) = result.pointer("/thread/turns").and_then(Value::as_array) else {
        return records;
    };
    for turn in turns {
        let Some(items) = turn.get("items").and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            let event_type = item
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("item")
                .to_string();
            records.push(libslop::Record {
                source: "transcript".to_string(),
                event_type,
                pane_id: Some(pane_id.to_string()),
                payload: item.clone(),
                cursor: Some(records.len() as u64),
            });
        }
    }
    records
}

/// Map app-server methods onto slopd's backend-neutral lifecycle vocabulary.
/// The original method remains available in each record's payload.
pub fn normalized_event_type(method: &str) -> &str {
    match method {
        "thread/started" => "SessionStart",
        "thread/archived" => "SessionEnd",
        "turn/started" => "UserPromptSubmit",
        "turn/completed" => "Stop",
        "item/commandExecution/requestApproval"
        | "item/fileChange/requestApproval"
        | "item/permissions/requestApproval" => "PermissionRequest",
        "item/tool/requestUserInput" | "mcpServer/elicitation/request" => "Elicitation",
        "item/started" => "PreToolUse",
        "item/completed" => "PostToolUse",
        _ => method,
    }
}

pub fn thread_runtime(result: &Value) -> (bool, Option<String>) {
    let active = result
        .pointer("/thread/status/type")
        .and_then(Value::as_str)
        == Some("active");
    let active_turn = result
        .pointer("/thread/turns")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .rev()
        .find(|turn| turn.get("status").and_then(Value::as_str) == Some("inProgress"))
        .and_then(|turn| turn.get("id").and_then(Value::as_str))
        .map(str::to_string);
    (active, active_turn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_is_scoped_to_codex_home() {
        assert_eq!(
            socket_path(Some(Path::new("/tmp/codex-a"))),
            PathBuf::from("/tmp/codex-a/app-server-control/app-server-control.sock")
        );
    }

    #[test]
    fn thread_records_flattens_turn_items() {
        let value = json!({"thread":{"turns":[
            {"items":[{"type":"userMessage","text":"hi"},{"type":"agentMessage","text":"hello"}]},
            {"items":[{"type":"commandExecution","command":"true"}]}
        ]}});
        let records = thread_records(&value, "%7");
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].event_type, "userMessage");
        assert_eq!(records[2].event_type, "commandExecution");
        assert_eq!(records[2].cursor, Some(2));
        assert_eq!(records[0].pane_id.as_deref(), Some("%7"));
    }

    #[test]
    fn notifications_use_backend_neutral_event_names() {
        assert_eq!(normalized_event_type("turn/completed"), "Stop");
        assert_eq!(
            normalized_event_type("item/commandExecution/requestApproval"),
            "PermissionRequest"
        );
        assert_eq!(
            normalized_event_type("mcpServer/elicitation/request"),
            "Elicitation"
        );
        assert_eq!(normalized_event_type("future/event"), "future/event");
    }

    #[test]
    fn thread_runtime_recovers_active_turn() {
        let result = json!({"thread":{"status":{"type":"active"},"turns":[
            {"id":"done","status":"completed"},{"id":"live","status":"inProgress"}
        ]}});
        assert_eq!(thread_runtime(&result), (true, Some("live".to_string())));
    }
}
