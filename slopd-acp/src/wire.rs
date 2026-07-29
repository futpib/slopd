use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const SERVER_ERROR: i32 = -32000;

#[derive(Debug)]
pub enum Inbound {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Ignored,
    Invalid {
        id: Value,
        code: i32,
        message: String,
    },
}

pub fn classify(message: &Value) -> Inbound {
    if !message.is_object() || message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Inbound::Invalid {
            id: message.get("id").cloned().unwrap_or(Value::Null),
            code: INVALID_REQUEST,
            message: "jsonrpc: missing or invalid version".into(),
        };
    }

    let id = message.get("id").cloned();
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    match (method, id) {
        (Some(method), Some(id)) => Inbound::Request { id, method, params },
        (Some(method), None) => Inbound::Notification { method, params },
        (None, Some(_)) => Inbound::Ignored,
        (None, None) => Inbound::Invalid {
            id: Value::Null,
            code: INVALID_REQUEST,
            message: "jsonrpc: missing method and id".into(),
        },
    }
}

pub fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn error(id: Value, code: i32, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() },
    })
}

pub fn session_update(session_id: &str, update: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": update,
        },
    })
}

pub type Sender = mpsc::Sender<Value>;

pub async fn send(sender: &Sender, message: Value) {
    let _ = sender.send(message).await;
}

pub async fn read_bounded_line<R>(reader: &mut R, maximum: usize) -> std::io::Result<Option<String>>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "unterminated ACP frame at EOF",
                ))
            };
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if line.len().saturating_add(take) > maximum {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("ACP frame exceeds {maximum} bytes"),
            ));
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if line.ends_with(b"\n") {
            line.pop();
            if line.ends_with(b"\r") {
                line.pop();
            }
            return String::from_utf8(line).map(Some).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "ACP frame is not valid UTF-8",
                )
            });
        }
    }
}

pub async fn writer_task(mut receiver: mpsc::Receiver<Value>) {
    let mut stdout = tokio::io::BufWriter::new(tokio::io::stdout());
    while let Some(message) = receiver.recv().await {
        let mut line = match serde_json::to_vec(&message) {
            Ok(line) => line,
            Err(error) => {
                tracing::error!("failed to serialize ACP frame: {error}");
                continue;
            }
        };
        line.push(b'\n');
        if stdout.write_all(&line).await.is_err() || stdout.flush().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_requests_and_notifications() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {},
        });
        assert!(matches!(
            classify(&request),
            Inbound::Request { method, .. } if method == "initialize"
        ));

        let notification = json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": {},
        });
        assert!(matches!(
            classify(&notification),
            Inbound::Notification { method, .. } if method == "session/cancel"
        ));
    }
}
