use std::net::SocketAddr;
use std::process::Child;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use libsloptest::{TestEnv, build_bin, cargo_bin, kill_slopd};
use serde_json::{Value, json};
use slopd_mcp::{ServeConfig, default_allowed_hosts, serve};
use tokio::net::TcpListener;

#[test]
fn extract_parses_sse_tool_result() {
    let buf = concat!(
        "data: \n",
        "id: 0/0\n",
        "retry: 3000\n",
        "\n",
        "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[]}}\n",
        "id: 1/0\n",
    );
    let value = extract_rpc_message(buf.as_bytes()).expect("parsed SSE");
    assert_eq!(value["id"], 2);
}

struct Daemon(Option<Child>);

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(child) = self.0.take() {
            kill_slopd(child);
        }
    }
}

async fn start_mcp(socket: std::path::PathBuf, token: Option<&str>, allow_run: bool) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mcp");
    let addr = listener.local_addr().expect("local addr");
    let config = ServeConfig {
        socket,
        allow_run,
        token: token.map(Arc::from),
        allowed_hosts: default_allowed_hosts(addr),
        path: "/mcp".into(),
        public_url: None,
    };
    tokio::spawn(async move {
        serve(listener, config).await.expect("serve mcp");
    });
    addr
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(90))
        .build()
        .unwrap()
}

async fn rpc(
    client: &reqwest::Client,
    addr: SocketAddr,
    token: Option<&str>,
    session: Option<&str>,
    body: Value,
) -> reqwest::Response {
    let mut request = client
        .post(format!("http://{addr}/mcp"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&body);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    if let Some(session) = session {
        request = request.header("Mcp-Session-Id", session);
    }
    request.send().await.expect("mcp request")
}

fn extract_rpc_message(buf: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(buf).ok()?;
    if text.trim_start().starts_with('{') {
        return serde_json::from_str(text.trim())
            .ok()
            .filter(|value: &Value| value.get("result").is_some() || value.get("error").is_some());
    }
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if !data.starts_with('{') {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(data)
            && (value.get("result").is_some() || value.get("error").is_some())
        {
            return Some(value);
        }
    }
    None
}

async fn read_rpc_message(response: reqwest::Response) -> Value {
    let status = response.status();
    let mut stream = response.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk.expect("mcp chunk"));
        if let Some(value) = extract_rpc_message(&buf) {
            assert!(status.is_success(), "mcp HTTP {status}: {value}");
            return value;
        }
    }
    panic!(
        "mcp stream ended without a JSON-RPC result: {}",
        String::from_utf8_lossy(&buf)
    )
}

async fn rpc_json(
    client: &reqwest::Client,
    addr: SocketAddr,
    token: Option<&str>,
    session: Option<&str>,
    body: Value,
) -> Value {
    read_rpc_message(rpc(client, addr, token, session, body).await).await
}

async fn initialize(
    client: &reqwest::Client,
    addr: SocketAddr,
    token: Option<&str>,
) -> (Value, Option<String>) {
    let response = rpc(
        client,
        addr,
        token,
        None,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" }
            }
        }),
    )
    .await;
    let session = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = read_rpc_message(response).await;
    let _ = rpc(
        client,
        addr,
        token,
        session.as_deref(),
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
    )
    .await;
    (body, session)
}

fn spawn_env() -> Option<(TestEnv, Daemon, libsloptest::tempfile::TempDir)> {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");
    let mock = cargo_bin("mock_claude");
    let slopctl = cargo_bin("slopctl");
    let claude_config = libsloptest::tempfile::tempdir().unwrap();
    let claude_config_path = claude_config.path().to_path_buf();
    let env = TestEnv::new_full(
        Some(&[mock.to_str().unwrap()]),
        Some(slopctl.to_str().unwrap()),
        Some(&claude_config_path),
    )?;
    let daemon = Daemon(Some(env.spawn_slopd()));
    Some((env, daemon, claude_config))
}

fn tool_names(list: &Value) -> Vec<String> {
    list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn lists_supervisor_tools_and_requires_bearer() {
    let Some((env, _daemon, _claude_config)) = spawn_env() else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    let addr = start_mcp(env.socket_path(), Some("secret"), false).await;
    let client = http_client();

    let unauthorized = rpc(
        &client,
        addr,
        None,
        None,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" }
            }
        }),
    )
    .await;
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);
    let www = unauthorized
        .headers()
        .get("www-authenticate")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    assert!(
        www.contains("resource_metadata="),
        "expected OAuth discovery challenge, got {www:?}"
    );

    let (initialized, session) = initialize(&client, addr, Some("secret")).await;
    assert_eq!(initialized["result"]["serverInfo"]["name"], "slopd-mcp");
    assert_eq!(initialized["result"]["capabilities"]["tools"], json!({}));

    let listed = rpc_json(
        &client,
        addr,
        Some("secret"),
        session.as_deref(),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    )
    .await;
    let names = tool_names(&listed);
    assert_eq!(
        names,
        vec!["status", "ps", "transcript", "send", "interrupt"]
    );
}

#[tokio::test]
async fn allow_run_advertises_run_tool() {
    let Some((env, _daemon, _claude_config)) = spawn_env() else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    let addr = start_mcp(env.socket_path(), None, true).await;
    let client = http_client();
    let (_, session) = initialize(&client, addr, None).await;
    let listed = rpc_json(
        &client,
        addr,
        None,
        session.as_deref(),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    )
    .await;
    let names = tool_names(&listed);
    assert!(names.contains(&"run".to_string()), "{names:?}");
}

#[tokio::test]
async fn ps_send_and_transcript_drive_a_mock_pane() {
    let Some((env, _daemon, _claude_config)) = spawn_env() else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    let run = env.slopctl_raw(&["run", "--ready-timeout", "20"]);
    assert!(run.status.success(), "slopctl run failed: {run:?}");
    let pane_id = String::from_utf8_lossy(&run.stdout).trim().to_string();

    let addr = start_mcp(env.socket_path(), Some("secret"), false).await;
    let client = http_client();
    let (_, session) = initialize(&client, addr, Some("secret")).await;
    let session = session.as_deref();

    let listed = rpc_json(
        &client,
        addr,
        Some("secret"),
        session,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "ps", "arguments": {} }
        }),
    )
    .await;
    let text = listed["result"]["content"][0]["text"].as_str().unwrap();
    let payload: Value = serde_json::from_str(text).unwrap();
    assert!(
        payload["panes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|pane| pane["pane_id"] == pane_id),
        "{payload}"
    );

    let sent = rpc_json(
        &client,
        addr,
        Some("secret"),
        session,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "send",
                "arguments": {
                    "pane_id": pane_id,
                    "prompt": "MCP_CANARY"
                }
            }
        }),
    )
    .await;
    let sent_text = sent["result"]["content"][0]["text"].as_str().unwrap();
    assert!(sent_text.contains(&pane_id), "{sent_text}");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut transcript_text = String::new();
    while std::time::Instant::now() < deadline {
        let transcript = rpc_json(
            &client,
            addr,
            Some("secret"),
            session,
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "transcript",
                    "arguments": { "pane_id": pane_id, "limit": 50 }
                }
            }),
        )
        .await;
        transcript_text = transcript["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        if transcript_text.contains("MCP_CANARY") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        transcript_text.contains("MCP_CANARY"),
        "transcript missing canary: {transcript_text}"
    );

    let interrupted = rpc_json(
        &client,
        addr,
        Some("secret"),
        session,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "interrupt",
                "arguments": { "pane_id": pane_id }
            }
        }),
    )
    .await;
    let interrupt_text = interrupted["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(interrupt_text.contains(&pane_id), "{interrupt_text}");
}

#[tokio::test]
async fn oauth_discovery_and_code_flow_issue_a_usable_bearer() {
    let Some((env, _daemon, _claude_config)) = spawn_env() else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    let addr = start_mcp(env.socket_path(), Some("secret"), false).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(90))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let metadata: Value = client
        .get(format!(
            "http://{addr}/.well-known/oauth-protected-resource"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(metadata["resource"], format!("http://{addr}/mcp"));
    assert!(
        metadata["authorization_servers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|server| server == &json!(format!("http://{addr}")))
    );

    let as_meta: Value = client
        .get(format!(
            "http://{addr}/.well-known/oauth-authorization-server"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        as_meta["authorization_endpoint"],
        format!("http://{addr}/oauth/authorize")
    );

    let registered: Value = client
        .post(format!("http://{addr}/oauth/register"))
        .json(&json!({
            "redirect_uris": ["http://127.0.0.1:9/cb"],
            "token_endpoint_auth_method": "none"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let client_id = registered["client_id"].as_str().unwrap();

    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    let authorize = client
        .post(format!("http://{addr}/oauth/authorize"))
        .form(&[
            ("password", "secret"),
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", "http://127.0.0.1:9/cb"),
            ("state", "xyz"),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(authorize.status(), reqwest::StatusCode::SEE_OTHER);
    let location = authorize
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(location.starts_with("http://127.0.0.1:9/cb?"), "{location}");
    let code = location
        .split(['?', '&'])
        .find_map(|part| part.strip_prefix("code="))
        .expect("code");

    let token: Value = client
        .post(format!("http://{addr}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", "http://127.0.0.1:9/cb"),
            ("client_id", client_id),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let access = token["access_token"].as_str().unwrap();

    let (initialized, _) = initialize(&http_client(), addr, Some(access)).await;
    assert_eq!(initialized["result"]["serverInfo"]["name"], "slopd-mcp");
}
