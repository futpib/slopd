use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use libsloptest::{sighup_pid, tempfile};
use reqwest::{Client, Response, StatusCode};
use serde_json::{Value, json};

struct Server {
    child: Child,
    url: String,
    client: Client,
    _dir: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let log = dir.path().join("server.log");
        let child = Command::new(env!("CARGO_BIN_EXE_slopd-mcp"))
            .args(["--bind", &addr.to_string(), "--token", "test-secret", "-vv"])
            .arg("--socket")
            .arg(dir.path().join("absent.sock"))
            .arg("--oauth-state")
            .arg(dir.path().join("oauth.jsonl"))
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        let server = Self {
            child,
            url: format!("http://{addr}"),
            client: Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
            _dir: dir,
        };
        let ready = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(response) = server
                    .client
                    .get(format!("{}/mcp", server.url))
                    .send()
                    .await
                    && response.status() == StatusCode::UNAUTHORIZED
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(ready.is_ok(), "{}", std::fs::read_to_string(log).unwrap());
        server
    }

    async fn post(&self, path: &str, session: Option<&str>, body: Value) -> Response {
        let mut request = self
            .client
            .post(format!("{}{path}", self.url))
            .bearer_auth("test-secret")
            .header("Accept", "application/json, text/event-stream")
            .json(&body);
        if let Some(session) = session {
            request = request.header("Mcp-Session-Id", session);
        }
        if let Some(version) =
            body["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"].as_str()
        {
            request = request
                .header("Mcp-Protocol-Version", version)
                .header("Mcp-Method", body["method"].as_str().unwrap());
            if let Some(name) = body["params"]["name"].as_str() {
                request = request.header("Mcp-Name", name);
            }
        }
        tokio::time::timeout(Duration::from_secs(5), request.send())
            .await
            .unwrap()
            .unwrap()
    }

    async fn legacy(&self, path: &str) -> (String, Messages) {
        let response = self
            .post(
                path,
                None,
                json!({
                    "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25", "capabilities": {},
                        "clientInfo": {"name": "refresh-test", "version": "1"}
                    }
                }),
            )
            .await;
        let session = response.headers()["mcp-session-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let info = Messages::new(response).result().await;
        assert_eq!(info["result"]["capabilities"]["tools"]["listChanged"], true);
        let response = self
            .post(
                path,
                Some(&session),
                json!({
                    "jsonrpc": "2.0", "method": "notifications/initialized"
                }),
            )
            .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let response = self
            .client
            .get(format!("{}{path}", self.url))
            .bearer_auth("test-secret")
            .header("Accept", "text/event-stream")
            .header("Mcp-Session-Id", &session)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut messages = Messages::new(response);
        assert_changed(&messages.next().await, None);
        (session, messages)
    }

    async fn modern(&self, path: &str, id: u64, method: &str, mut params: Value) -> Response {
        params["_meta"] = json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": {"name": "refresh-test", "version": "1"},
            "io.modelcontextprotocol/clientCapabilities": {}
        });
        self.post(
            path,
            None,
            json!({
                "jsonrpc": "2.0", "id": id, "method": method, "params": params
            }),
        )
        .await
    }

    async fn subscription(&self, path: &str, id: u64) -> Messages {
        let response = self
            .modern(
                path,
                id,
                "subscriptions/listen",
                json!({
                    "notifications": {"toolsListChanged": true, "promptsListChanged": true}
                }),
            )
            .await;
        if response.status() != StatusCode::OK {
            panic!(
                "subscription HTTP {}: {}",
                response.status(),
                response.text().await.unwrap()
            );
        }
        let mut messages = Messages::new(response);
        let ack = messages.next().await;
        assert_eq!(ack["method"], "notifications/subscriptions/acknowledged");
        assert_eq!(
            ack["params"]["notifications"]["toolsListChanged"], true,
            "{ack}"
        );
        assert!(
            ack["params"]["notifications"]["promptsListChanged"].is_null(),
            "{ack}"
        );
        assert_changed(&messages.next().await, Some(id));
        messages
    }
}

struct Messages {
    response: Response,
    buffer: Vec<u8>,
    json: bool,
}

impl Messages {
    fn new(response: Response) -> Self {
        let json = response
            .headers()
            .get("content-type")
            .is_some_and(|value| value.to_str().unwrap().starts_with("application/json"));
        Self {
            response,
            buffer: Vec::new(),
            json,
        }
    }

    async fn next(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if self.json {
                    if let Ok(value) = serde_json::from_slice(&self.buffer) {
                        self.buffer.clear();
                        return value;
                    }
                } else {
                    while let Some(end) = self.buffer.windows(2).position(|bytes| bytes == b"\n\n")
                    {
                        let block: Vec<_> = self.buffer.drain(..end + 2).collect();
                        let block = String::from_utf8(block).unwrap();
                        let data = block
                            .lines()
                            .filter_map(|line| line.strip_prefix("data:"))
                            .map(str::trim)
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !data.is_empty() {
                            return serde_json::from_str(&data).unwrap();
                        }
                    }
                }
                let chunk = self
                    .response
                    .chunk()
                    .await
                    .unwrap()
                    .expect("stream ended before message");
                self.buffer.extend_from_slice(&chunk);
            }
        })
        .await
        .expect("timed out waiting for MCP message")
    }

    async fn result(&mut self) -> Value {
        loop {
            let value = self.next().await;
            if value.get("result").is_some() || value.get("error").is_some() {
                return value;
            }
        }
    }
}

fn assert_changed(message: &Value, subscription: Option<u64>) {
    assert_eq!(
        message["method"], "notifications/tools/list_changed",
        "{message}"
    );
    if let Some(id) = subscription {
        assert_eq!(
            message["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
            id
        );
    }
}

#[tokio::test]
async fn sighup_refreshes_legacy_clients_on_both_endpoints() {
    let server = Server::start().await;
    let (simple, mut simple_events) = server.legacy("/mcp").await;
    let (full, mut full_events) = server.legacy("/mcp/slopctl").await;
    for _ in 0..2 {
        sighup_pid(server.child.id());
        assert_changed(&simple_events.next().await, None);
        assert_changed(&full_events.next().await, None);
        for (path, session, count) in [("/mcp", &simple, 5), ("/mcp/slopctl", &full, 22)] {
            let response = server
                .post(
                    path,
                    Some(session),
                    json!({
                        "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
                    }),
                )
                .await;
            let listed = Messages::new(response).result().await;
            assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), count);
        }
    }
    let response = server
        .post(
            "/mcp",
            Some(&simple),
            json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "list_panes", "arguments": {}}
            }),
        )
        .await;
    let result = Messages::new(response).result().await;
    assert_eq!(result["error"]["code"], -32602);
    assert!(
        result["error"]["message"]
            .as_str()
            .unwrap()
            .contains("refresh tools/list")
    );
    assert_changed(&simple_events.next().await, None);
    server
        .client
        .delete(format!("{}/mcp", server.url))
        .bearer_auth("test-secret")
        .header("Mcp-Session-Id", &simple)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    drop(simple_events);
    sighup_pid(server.child.id());
    assert_changed(&full_events.next().await, None);
}

#[tokio::test]
async fn modern_subscriptions_refresh_and_survive_another_client_disconnect() {
    let server = Server::start().await;
    let discovery = server.modern("/mcp", 1, "server/discover", json!({})).await;
    let discovery = Messages::new(discovery).result().await;
    assert_eq!(
        discovery["result"]["capabilities"]["tools"]["listChanged"], true,
        "{discovery}"
    );
    let mut simple = server.subscription("/mcp", 10).await;
    let mut full = server.subscription("/mcp/slopctl", 11).await;
    sighup_pid(server.child.id());
    assert_changed(&simple.next().await, Some(10));
    assert_changed(&full.next().await, Some(11));
    for (path, count) in [("/mcp", 5), ("/mcp/slopctl", 22)] {
        let response = server.modern(path, 12, "tools/list", json!({})).await;
        let listed = Messages::new(response).result().await;
        assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), count);
    }
    let response = server
        .modern(
            "/mcp",
            13,
            "tools/call",
            json!({
                "name": "list_panes", "arguments": {}
            }),
        )
        .await;
    let error = Messages::new(response).result().await;
    assert_eq!(error["error"]["code"], -32602, "{error}");
    assert_changed(&simple.next().await, Some(10));
    assert_changed(&full.next().await, Some(11));
    drop(simple);
    sighup_pid(server.child.id());
    assert_changed(&full.next().await, Some(11));
    let response = server
        .modern(
            "/mcp",
            14,
            "tools/call",
            json!({
                "name": "get_agent_result", "arguments": {}
            }),
        )
        .await;
    let result = Messages::new(response).result().await;
    assert_eq!(result["result"]["structuredContent"]["status"], "not_found");
}

#[tokio::test]
async fn subscriptions_require_authentication_and_tool_notification_opt_in() {
    let server = Server::start().await;
    let response = server
        .client
        .post(format!("{}/mcp", server.url))
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "subscriptions/listen"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let response = server
        .modern(
            "/mcp",
            2,
            "subscriptions/listen",
            json!({
                "notifications": {"promptsListChanged": true}
            }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut messages = Messages::new(response);
    let ack = messages.next().await;
    assert_eq!(ack["method"], "notifications/subscriptions/acknowledged");
    assert_eq!(ack["params"]["notifications"], json!({}));
    sighup_pid(server.child.id());
    assert!(
        tokio::time::timeout(Duration::from_millis(200), messages.next())
            .await
            .is_err()
    );
}
