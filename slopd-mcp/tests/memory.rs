use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use libsloptest::tempfile;
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};

struct Server {
    child: Child,
    url: String,
    client: Client,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    async fn start(dir: &Path) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let child = Command::new(env!("CARGO_BIN_EXE_slopd-mcp"))
            .args(["--bind", &addr.to_string(), "--token", "test-secret"])
            .arg("--socket")
            .arg(dir.join("absent.sock"))
            .arg("--oauth-state")
            .arg(dir.join("oauth.jsonl"))
            .arg("--memory-state")
            .arg(dir.join("memory.jsonl"))
            .arg("--request-state")
            .arg(dir.join("requests.json"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut server = Self {
            child,
            url: format!("http://{addr}"),
            client: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                assert!(
                    server.child.try_wait().unwrap().is_none(),
                    "MCP process exited before becoming ready"
                );
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
        .await
        .unwrap();
        server
    }

    async fn rpc(&self, path: &str, method: &str, mut params: Value) -> Value {
        params["_meta"] = json!({
            "io.modelcontextprotocol/protocolVersion":"2026-07-28",
            "io.modelcontextprotocol/clientInfo":{"name":"memory-test","version":"1"},
            "io.modelcontextprotocol/clientCapabilities":{}
        });
        let mut request = self
            .client
            .post(format!("{}{path}", self.url))
            .bearer_auth("test-secret")
            .header("Accept", "application/json, text/event-stream")
            .header("Mcp-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", method);
        if let Some(name) = params["name"].as_str() {
            request = request.header("Mcp-Name", name);
        }
        let response = request
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await
            .unwrap();
        let text = response.text().await.unwrap();
        serde_json::from_str(&text).unwrap_or_else(|_| {
            text.lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .find(|v| v.get("result").is_some() || v.get("error").is_some())
                .expect("RPC result")
        })
    }

    async fn call(&self, path: &str, name: &str, arguments: Value) -> Value {
        self.rpc(
            path,
            "tools/call",
            json!({"name":name,"arguments":arguments}),
        )
        .await
    }
}

fn payload(response: &Value) -> Value {
    assert!(response.get("error").is_none(), "{response}");
    let value = response["result"]["structuredContent"].clone();
    assert!(value.is_object(), "{response}");
    value
}

#[tokio::test]
async fn memories_survive_real_process_restarts_and_are_listed_on_both_surfaces() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path()).await;
    for path in ["/mcp", "/mcp/slopctl"] {
        let listed = server.rpc(path, "tools/list", json!({})).await;
        for name in [
            "save_memory",
            "get_memory",
            "search_memories",
            "delete_memory",
        ] {
            let tool = listed["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == name)
                .unwrap();
            assert!(tool["outputSchema"].is_object());
            assert_eq!(
                tool["annotations"]["readOnlyHint"],
                matches!(name, "get_memory" | "search_memories")
            );
            assert_eq!(
                tool["annotations"]["destructiveHint"],
                matches!(name, "save_memory" | "delete_memory")
            );
        }
    }
    let args = json!({"namespace":"project:orchard","key":"ui-language","content":"Prefer Georgian labels","tags":["preference","ui"],"expected_revision":0});
    let saved = payload(&server.call("/mcp", "save_memory", args.clone()).await);
    assert_eq!(saved["created"], true);
    let revision = saved["memory"]["revision"].as_u64().unwrap();
    let same = payload(&server.call("/mcp/slopctl", "save_memory", json!({"namespace":"project:orchard","key":"ui-language","content":"Prefer Georgian labels","tags":["ui","preference"]})).await);
    assert_eq!(same["changed"], false);
    assert_eq!(same["memory"], saved["memory"]);
    let fetched = payload(
        &server
            .call(
                "/mcp/slopctl",
                "get_memory",
                json!({"namespace":"project:orchard","key":"ui-language"}),
            )
            .await,
    );
    assert_eq!(fetched["memory"], saved["memory"]);
    drop(server);

    let server = Server::start(dir.path()).await;
    let found = payload(
        &server
            .call(
                "/mcp",
                "search_memories",
                json!({"query":"GEORGIAN orchard","tags":["ui"]}),
            )
            .await,
    );
    assert_eq!(found["total"], 1);
    assert_eq!(found["memories"][0], saved["memory"]);
    let other = payload(
        &server
            .call(
                "/mcp",
                "save_memory",
                json!({"namespace":"other","key":"ui-language","content":"Use English"}),
            )
            .await,
    );
    assert_eq!(other["created"], true);
    let conflict = server.call("/mcp", "save_memory", args).await;
    assert_eq!(conflict["error"]["code"], -32602);
    let updated = payload(&server.call("/mcp", "save_memory", json!({"namespace":"project:orchard","key":"ui-language","content":"Prefer Georgian and English labels","tags":["ui"],"expected_revision":revision})).await);
    assert_eq!(updated["created"], false);
    assert_eq!(
        updated["memory"]["created_at_unix_ms"],
        saved["memory"]["created_at_unix_ms"]
    );
    let latest_revision = updated["memory"]["revision"].as_u64().unwrap();
    assert!(latest_revision > revision);
    let stale_delete = server
        .call(
            "/mcp",
            "delete_memory",
            json!({"namespace":"project:orchard","key":"ui-language","expected_revision":revision}),
        )
        .await;
    assert_eq!(stale_delete["error"]["code"], -32602);
    let page = payload(
        &server
            .call("/mcp", "search_memories", json!({"limit":1}))
            .await,
    );
    assert_eq!(page["total"], 2);
    assert_eq!(page["next_offset"], 1);
    assert_eq!(page["memories"][0], updated["memory"]);
    let next = payload(
        &server
            .call("/mcp", "search_memories", json!({"limit":1,"offset":1}))
            .await,
    );
    assert_eq!(next["memories"][0], other["memory"]);
    assert!(next["next_offset"].is_null());
    drop(server);

    let server = Server::start(dir.path()).await;
    let recalled = payload(
        &server
            .call(
                "/mcp",
                "get_memory",
                json!({"namespace":"project:orchard","key":"ui-language"}),
            )
            .await,
    );
    assert_eq!(recalled["memory"], updated["memory"]);
    let deleted = payload(&server.call("/mcp/slopctl", "delete_memory", json!({"namespace":"project:orchard","key":"ui-language","expected_revision":latest_revision})).await);
    assert_eq!(deleted["deleted"], true);
    let again = payload(
        &server
            .call(
                "/mcp",
                "delete_memory",
                json!({"namespace":"project:orchard","key":"ui-language"}),
            )
            .await,
    );
    assert_eq!(again["deleted"], false);
    drop(server);

    let server = Server::start(dir.path()).await;
    let missing = payload(
        &server
            .call(
                "/mcp",
                "get_memory",
                json!({"namespace":"project:orchard","key":"ui-language"}),
            )
            .await,
    );
    assert_eq!(missing, json!({"found":false,"memory":null}));
    let remaining = payload(&server.call("/mcp", "search_memories", json!({})).await);
    assert_eq!(remaining["total"], 1);
    assert_eq!(remaining["memories"][0], other["memory"]);
    let recreated = payload(&server.call("/mcp", "save_memory", json!({"namespace":"project:orchard","key":"ui-language","content":"New preference"})).await);
    assert!(recreated["memory"]["revision"].as_u64().unwrap() > latest_revision);
}

#[tokio::test]
async fn validation_authentication_and_independent_stores_protect_memories() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path()).await;
    let other_dir = tempfile::tempdir().unwrap();
    let other = Server::start(other_dir.path()).await;
    let unauthorized = server.client.post(format!("{}/mcp", server.url))
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"save_memory","arguments":{"key":"hidden","content":"must not save"}}}))
        .send().await.unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    for (name, args) in [
        ("save_memory", json!({"key":" ","content":"value"})),
        (
            "save_memory",
            json!({"key":"key","content":" ","account":"someone-else"}),
        ),
        (
            "save_memory",
            json!({"key":"key","content":"a".repeat(16385)}),
        ),
        (
            "save_memory",
            json!({"key":"key","content":"value","tags":[""]}),
        ),
        ("search_memories", json!({"limit":0})),
        ("search_memories", json!({"limit":101})),
        ("search_memories", json!({"offset":-1})),
        ("delete_memory", json!({"key":"key","extra":true})),
    ] {
        let rejected = server.call("/mcp", name, args).await;
        assert_eq!(rejected["error"]["code"], -32602, "{rejected}");
    }
    let saved = payload(
        &server
            .call(
                "/mcp",
                "save_memory",
                json!({"key":"default-key","content":"Only in the first store"}),
            )
            .await,
    );
    assert_eq!(saved["memory"]["namespace"], "default");
    assert_eq!(
        payload(&other.call("/mcp", "search_memories", json!({})).await)["total"],
        0
    );
    assert_eq!(
        payload(
            &server
                .call("/mcp", "search_memories", json!({"namespace":"other"}))
                .await
        )["total"],
        0
    );
    assert_eq!(
        payload(
            &server
                .call("/mcp", "search_memories", json!({"query":"unmatched"}))
                .await
        )["total"],
        0
    );
}
