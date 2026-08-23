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

async fn start_mcp(socket: std::path::PathBuf, token: Option<&str>) -> SocketAddr {
    start_mcp_with_public_url(socket, token, None).await
}

async fn start_mcp_with_public_url(
    socket: std::path::PathBuf,
    token: Option<&str>,
    public_url: Option<String>,
) -> SocketAddr {
    let oauth_state = socket.with_extension("mcp-oauth.jsonl");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mcp");
    let addr = listener.local_addr().expect("local addr");
    let config = ServeConfig {
        socket,
        oauth_state,
        token: token.map(Arc::from),
        allowed_hosts: default_allowed_hosts(addr),
        path: "/mcp".into(),
        public_url,
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

fn spawn_codex_env() -> Option<(TestEnv, Daemon, libsloptest::tempfile::TempDir)> {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_codex");
    let mock = cargo_bin("mock_codex");
    let slopctl = cargo_bin("slopctl");
    let codex_home = libsloptest::tempfile::tempdir().unwrap();
    let env = TestEnv::new_full(None, Some(slopctl.to_str().unwrap()), None)?;
    env.append_config(&format!(
        "\n[accounts.codex]\nbackend = \"codex\"\nexecutable = {:?}\nconfig_dir = {:?}\n",
        mock.to_str().unwrap(),
        codex_home.path().to_str().unwrap(),
    ));
    let daemon = Daemon(Some(env.spawn_slopd()));
    Some((env, daemon, codex_home))
}

fn tool_names(list: &Value) -> Vec<String> {
    list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect()
}

async fn call_tool(
    client: &reqwest::Client,
    addr: SocketAddr,
    token: Option<&str>,
    session: Option<&str>,
    id: u64,
    name: &str,
    arguments: Value,
) -> Value {
    rpc_json(
        client,
        addr,
        token,
        session,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }),
    )
    .await
}

fn tool_payload(response: &Value) -> Value {
    assert_eq!(response["result"]["content"], json!([]), "{response}");
    response["result"]["structuredContent"].clone()
}

#[tokio::test]
async fn lists_supervisor_tools_and_requires_bearer() {
    let Some((env, _daemon, _claude_config)) = spawn_env() else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    let addr = start_mcp(env.socket_path(), Some("secret")).await;
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
    let instructions = initialized["result"]["instructions"].as_str().unwrap();
    assert!(
        instructions.contains("never require the user to name MCP"),
        "{initialized}"
    );
    assert!(instructions.contains("get_work_overview"), "{initialized}");
    assert!(
        instructions.contains("Never combine an overview with get_agent_result"),
        "{initialized}"
    );
    assert!(
        instructions.contains("Write prompts sent to agents in English"),
        "{initialized}"
    );
    assert!(
        instructions.contains("Preserve the language of agent replies"),
        "{initialized}"
    );

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
        vec![
            "get_status",
            "get_work_overview",
            "list_panes",
            "create_pane",
            "fork_pane",
            "kill_pane",
            "ask_agent",
            "get_agent_result",
            "send_prompt",
            "wait_for_reply",
            "interrupt_pane",
            "read_transcript",
            "add_tag",
            "remove_tag",
            "list_tags",
            "create_backup",
            "restore_backup",
            "list_dead_panes",
            "revive_pane",
        ]
    );
    let send = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "send_prompt")
        .unwrap();
    assert_eq!(
        send["inputSchema"]["properties"]["pane_id"]["pattern"],
        "^%[0-9]+$"
    );
    assert!(
        send["description"]
            .as_str()
            .unwrap()
            .contains("including %")
    );
    assert_eq!(send["annotations"]["readOnlyHint"], false);
    assert_eq!(send["annotations"]["destructiveHint"], false);
    assert_eq!(send["annotations"]["idempotentHint"], false);
    assert_eq!(send["annotations"]["openWorldHint"], false);
    assert_eq!(send["outputSchema"]["type"], "object");
    let get_agent_result = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "get_agent_result")
        .unwrap();
    assert!(
        get_agent_result["description"]
            .as_str()
            .unwrap()
            .contains("latest request submitted through ask_agent")
    );
    assert!(
        get_agent_result["description"]
            .as_str()
            .unwrap()
            .contains("not a live-pane inventory")
    );
    assert!(
        get_agent_result["inputSchema"]["properties"]
            .get("limit")
            .is_none()
    );
    assert_eq!(
        get_agent_result["outputSchema"]["properties"]["reply"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(
        get_agent_result["outputSchema"]["properties"]["finished"]["type"],
        json!(["boolean", "null"])
    );
    assert_eq!(
        get_agent_result["outputSchema"]["properties"]["answer"]["type"],
        "string"
    );
    assert!(
        get_agent_result["outputSchema"]["properties"]["reply"]["description"]
            .as_str()
            .unwrap()
            .contains("original language")
    );
    let overview = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "get_work_overview")
        .unwrap();
    assert!(
        overview["description"]
            .as_str()
            .unwrap()
            .contains("where work left off")
    );
    assert_eq!(
        overview["outputSchema"]["properties"]["panes"]["items"]["properties"]["latest_reply_excerpt"]
            ["type"],
        json!(["string", "null"])
    );
    assert_eq!(
        overview["outputSchema"]["properties"]["answer"]["type"],
        "string"
    );
    let list_panes = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "list_panes")
        .unwrap();
    assert!(
        list_panes["description"]
            .as_str()
            .unwrap()
            .contains("busy means actively working")
    );
    assert_eq!(
        list_panes["outputSchema"]["properties"]["panes"]["items"]["properties"]["state"]["enum"],
        json!(["busy", "ready", "awaiting_input", "booting_up"])
    );
    for tool in listed["result"]["tools"].as_array().unwrap() {
        assert!(tool["title"].is_string(), "{tool}");
        assert!(tool["outputSchema"].is_object(), "{tool}");
        assert!(tool["annotations"].is_object(), "{tool}");
        assert_eq!(tool["annotations"]["openWorldHint"], false, "{tool}");
    }
    let expected_annotations = [
        ("get_status", true, false, true),
        ("get_work_overview", true, false, true),
        ("list_panes", true, false, true),
        ("create_pane", false, false, false),
        ("fork_pane", false, false, false),
        ("kill_pane", false, true, true),
        ("ask_agent", false, false, false),
        ("get_agent_result", true, false, true),
        ("send_prompt", false, false, false),
        ("wait_for_reply", true, false, true),
        ("interrupt_pane", false, true, false),
        ("read_transcript", true, false, true),
        ("add_tag", false, false, true),
        ("remove_tag", false, true, true),
        ("list_tags", true, false, true),
        ("create_backup", false, false, false),
        ("restore_backup", false, false, true),
        ("list_dead_panes", true, false, true),
        ("revive_pane", false, false, false),
    ];
    for (name, read_only, destructive, idempotent) in expected_annotations {
        let tool = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap();
        assert_eq!(tool["annotations"]["readOnlyHint"], read_only, "{name}");
        assert_eq!(
            tool["annotations"]["destructiveHint"], destructive,
            "{name}"
        );
        assert_eq!(tool["annotations"]["idempotentHint"], idempotent, "{name}");
    }

    let missing = call_tool(
        &client,
        addr,
        Some("secret"),
        session.as_deref(),
        3,
        "read_transcript",
        json!({ "pane_id": "%999999" }),
    )
    .await;
    assert_eq!(missing["error"]["code"], -32602, "{missing}");
    assert!(
        missing["error"]["message"]
            .as_str()
            .unwrap()
            .contains("pane %999999 is not managed by slopd"),
        "{missing}"
    );
    assert_eq!(missing["error"]["data"]["code"], "unknown_pane_id");
    assert_eq!(missing["error"]["data"]["retry_with"]["tool"], "list_panes");
    assert!(missing["error"]["data"]["valid_panes"].is_array());
}

#[tokio::test]
async fn lifecycle_and_metadata_tools_round_trip() {
    let Some((env, _daemon, _claude_config)) = spawn_env() else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    let addr = start_mcp(env.socket_path(), None).await;
    let client = http_client();
    let (_, session) = initialize(&client, addr, None).await;
    let session = session.as_deref();

    let run = call_tool(
        &client,
        addr,
        None,
        session,
        10,
        "create_pane",
        json!({ "ready_timeout": 20 }),
    )
    .await;
    let run = tool_payload(&run);
    let pane_id = run["pane_id"].as_str().unwrap().to_string();
    assert_eq!(run["ready"], true, "{run}");
    let tags = call_tool(
        &client,
        addr,
        None,
        session,
        100,
        "list_tags",
        json!({ "pane_id": pane_id }),
    )
    .await;
    assert_eq!(tool_payload(&tags)["tags"], json!(["slopd-mcp"]));

    let tagged = call_tool(
        &client,
        addr,
        None,
        session,
        11,
        "add_tag",
        json!({ "pane_id": pane_id, "tag": "mcp-parity" }),
    )
    .await;
    assert_eq!(tool_payload(&tagged)["tag"], "mcp-parity");
    let tags = call_tool(
        &client,
        addr,
        None,
        session,
        12,
        "list_tags",
        json!({ "pane_id": pane_id }),
    )
    .await;
    let tags = tool_payload(&tags);
    let tags = tags["tags"].as_array().unwrap();
    assert!(tags.iter().any(|tag| tag == "slopd-mcp"), "{tags:?}");
    assert!(tags.iter().any(|tag| tag == "mcp-parity"), "{tags:?}");
    let untagged = call_tool(
        &client,
        addr,
        None,
        session,
        13,
        "remove_tag",
        json!({ "pane_id": pane_id, "tag": "mcp-parity" }),
    )
    .await;
    assert_eq!(tool_payload(&untagged)["tag"], "mcp-parity");
    let untagged = call_tool(
        &client,
        addr,
        None,
        session,
        101,
        "remove_tag",
        json!({ "pane_id": pane_id, "tag": "slopd-mcp" }),
    )
    .await;
    assert_eq!(tool_payload(&untagged)["tag"], "slopd-mcp");

    let waited = call_tool(
        &client,
        addr,
        None,
        session,
        14,
        "wait_for_event",
        json!({
            "advanced": true,
            "pane_id": pane_id,
            "until": ["seeded_current=true"],
            "timeout": 5
        }),
    )
    .await;
    let waited = tool_payload(&waited);
    assert_eq!(waited["snapshot"], true, "{waited}");

    let backup = call_tool(&client, addr, None, session, 17, "create_backup", json!({})).await;
    assert!(tool_payload(&backup)["count"].as_u64().unwrap() >= 1);

    let forked = call_tool(
        &client,
        addr,
        None,
        session,
        18,
        "fork_pane",
        json!({ "pane_id": pane_id, "no_wait": true }),
    )
    .await;
    let forked = tool_payload(&forked);
    let fork_id = forked["pane_id"].as_str().unwrap().to_string();
    assert_eq!(forked["ready"], false, "{forked}");
    let tags = call_tool(
        &client,
        addr,
        None,
        session,
        102,
        "list_tags",
        json!({ "pane_id": fork_id }),
    )
    .await;
    assert_eq!(tool_payload(&tags)["tags"], json!(["slopd-mcp"]));
    let untagged = call_tool(
        &client,
        addr,
        None,
        session,
        103,
        "remove_tag",
        json!({ "pane_id": fork_id, "tag": "slopd-mcp" }),
    )
    .await;
    assert_eq!(tool_payload(&untagged)["tag"], "slopd-mcp");

    let killed = call_tool(
        &client,
        addr,
        None,
        session,
        19,
        "kill_pane",
        json!({ "pane_id": fork_id }),
    )
    .await;
    assert_eq!(tool_payload(&killed)["pane_id"], fork_id);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let grave_id = loop {
        let graveyard = call_tool(
            &client,
            addr,
            None,
            session,
            20,
            "list_dead_panes",
            json!({ "limit": 20 }),
        )
        .await;
        let graveyard = tool_payload(&graveyard);
        if let Some(entry) = graveyard["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["pane_id"] == fork_id)
        {
            assert!(entry.get("tmux_boot_id").is_none(), "{entry}");
            assert!(entry.get("tmux_session_id").is_none(), "{entry}");
            break entry["grave_id"].as_str().unwrap().to_string();
        }
        assert!(std::time::Instant::now() < deadline, "{graveyard}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let raw_graveyard = call_tool(
        &client,
        addr,
        None,
        session,
        23,
        "list_dead_panes",
        json!({ "limit": 20, "raw": true }),
    )
    .await;
    let raw_graveyard = tool_payload(&raw_graveyard);
    let raw_entry = raw_graveyard["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["grave_id"] == grave_id)
        .unwrap();
    assert!(raw_entry.get("tmux_boot_id").is_some(), "{raw_entry}");
    assert!(raw_entry.get("pane").is_some(), "{raw_entry}");

    let revived = call_tool(
        &client,
        addr,
        None,
        session,
        21,
        "revive_pane",
        json!({ "target": grave_id }),
    )
    .await;
    let revived = tool_payload(&revived);
    assert_eq!(revived["grave_id"], grave_id);
    let revived_id = revived["pane_id"].as_str().unwrap();
    let tags = call_tool(
        &client,
        addr,
        None,
        session,
        104,
        "list_tags",
        json!({ "pane_id": revived_id }),
    )
    .await;
    assert_eq!(tool_payload(&tags)["tags"], json!(["slopd-mcp"]));

    let restored = call_tool(
        &client,
        addr,
        None,
        session,
        22,
        "restore_backup",
        json!({}),
    )
    .await;
    assert!(tool_payload(&restored)["restored"].is_u64(), "{restored}");
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

    let addr = start_mcp(env.socket_path(), Some("secret")).await;
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
            "params": { "name": "list_panes", "arguments": {} }
        }),
    )
    .await;
    let payload = tool_payload(&listed);
    assert_eq!(payload["count"], 1, "{payload}");
    assert_eq!(
        payload["state_counts"],
        json!({ "busy": 0, "ready": 1, "awaiting_input": 0, "booting_up": 0 }),
        "{payload}"
    );
    let compact_pane = payload["panes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|pane| pane["pane_id"] == pane_id)
        .unwrap();
    assert!(compact_pane.get("session_id").is_none(), "{compact_pane}");
    assert!(
        compact_pane.get("transcript_path").is_none(),
        "{compact_pane}"
    );
    assert!(
        payload["panes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|pane| pane["pane_id"] == pane_id),
        "{payload}"
    );
    let raw_panes = call_tool(
        &client,
        addr,
        Some("secret"),
        session,
        20,
        "list_panes",
        json!({ "raw": true }),
    )
    .await;
    let raw_panes = tool_payload(&raw_panes);
    let raw_pane = raw_panes["panes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|pane| pane["pane_id"] == pane_id)
        .unwrap();
    assert!(raw_pane.get("session_id").is_some(), "{raw_pane}");
    assert!(raw_pane.get("transcript_path").is_some(), "{raw_pane}");

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
                "name": "send_prompt",
                "arguments": {
                    "pane_id": pane_id,
                    "prompt": "MCP_CANARY"
                }
            }
        }),
    )
    .await;
    assert_eq!(tool_payload(&sent)["pane_ids"], json!([pane_id.clone()]));

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut transcript_payload = Value::Null;
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
                    "name": "read_transcript",
                    "arguments": { "pane_id": pane_id, "limit": 50 }
                }
            }),
        )
        .await;
        transcript_payload = tool_payload(&transcript);
        if transcript_payload["records"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|record| record["text"].as_str())
            .any(|text| text.contains("MCP_CANARY"))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        transcript_payload["records"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|record| record["text"].as_str())
            .any(|text| text.contains("MCP_CANARY")),
        "transcript missing canary: {transcript_payload}"
    );
    let raw_transcript = call_tool(
        &client,
        addr,
        Some("secret"),
        session,
        21,
        "read_transcript",
        json!({ "pane_id": pane_id, "limit": 50, "advanced": true, "raw": true }),
    )
    .await;
    let raw_transcript = tool_payload(&raw_transcript);
    assert!(
        raw_transcript["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record.get("payload").is_some()),
        "{raw_transcript}"
    );

    let replayed = call_tool(
        &client,
        addr,
        Some("secret"),
        session,
        5,
        "collect_events",
        json!({
            "advanced": true,
            "pane_id": pane_id,
            "replay": 1,
            "limit": 1,
            "timeout": 5
        }),
    )
    .await;
    let replayed = tool_payload(&replayed);
    assert_eq!(
        replayed["records"].as_array().unwrap().len(),
        1,
        "{replayed}"
    );
    assert_eq!(replayed["timed_out"], false, "{replayed}");

    let interrupted = rpc_json(
        &client,
        addr,
        Some("secret"),
        session,
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "interrupt_pane",
                "arguments": { "pane_id": pane_id }
            }
        }),
    )
    .await;
    assert_eq!(tool_payload(&interrupted)["pane_id"], pane_id);
}

#[tokio::test]
async fn cached_run_alias_creates_and_prompts_a_pane() {
    let Some((env, _daemon, _claude_config)) = spawn_env() else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    let addr = start_mcp(env.socket_path(), None).await;
    let client = http_client();
    let (_, session) = initialize(&client, addr, None).await;
    let session = session.as_deref();

    let created = call_tool(
        &client,
        addr,
        None,
        session,
        70,
        "run",
        json!({
            "backend": "claude",
            "prompt": "CACHED_RUN_CANARY",
            "ready_timeout": 20
        }),
    )
    .await;
    let created = tool_payload(&created);
    let pane_id = created["pane_id"].as_str().unwrap().to_string();
    assert_eq!(created["ready"], true, "{created}");
    assert_eq!(created["prompt_sent"], true, "{created}");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let transcript = call_tool(
            &client,
            addr,
            None,
            session,
            71,
            "transcript",
            json!({ "pane_id": pane_id, "limit": 50 }),
        )
        .await;
        let transcript = tool_payload(&transcript);
        if transcript["records"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|record| record["text"].as_str())
            .any(|text| text.contains("CACHED_RUN_CANARY"))
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{transcript}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let killed = call_tool(
        &client,
        addr,
        None,
        session,
        72,
        "kill",
        json!({ "pane_id": pane_id }),
    )
    .await;
    assert_eq!(tool_payload(&killed)["pane_id"], pane_id);
}

#[tokio::test]
async fn wait_assistant_alias_catches_a_codex_reply() {
    let Some((env, _daemon, _codex_home)) = spawn_codex_env() else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    let addr = start_mcp(env.socket_path(), None).await;
    let client = http_client();
    let (_, control_session) = initialize(&client, addr, None).await;
    let run = call_tool(
        &client,
        addr,
        None,
        control_session.as_deref(),
        30,
        "create_pane",
        json!({ "account": "codex", "backend": "codex", "ready_timeout": 20 }),
    )
    .await;
    let pane_id = tool_payload(&run)["pane_id"].as_str().unwrap().to_string();

    let malformed = format!("percent{}", pane_id.trim_start_matches('%'));
    let invalid_calls = [
        (
            "fork_pane",
            json!({ "pane_id": malformed, "no_wait": true }),
        ),
        ("kill_pane", json!({ "pane_id": malformed })),
        (
            "ask_agent",
            json!({ "pane_id": malformed, "prompt": "must not be sent" }),
        ),
        ("get_work_overview", json!({ "pane_id": malformed })),
        ("get_agent_result", json!({ "pane_id": malformed })),
        (
            "send_prompt",
            json!({ "pane_id": malformed, "prompt": "must not be sent" }),
        ),
        ("interrupt_pane", json!({ "pane_id": malformed })),
        (
            "collect_events",
            json!({ "advanced": true, "pane_id": malformed, "timeout": 1 }),
        ),
        (
            "wait_for_event",
            json!({ "advanced": true, "pane_id": malformed, "timeout": 1 }),
        ),
        ("read_transcript", json!({ "pane_id": malformed })),
        (
            "add_tag",
            json!({ "pane_id": malformed, "tag": "must-not-exist" }),
        ),
        (
            "remove_tag",
            json!({ "pane_id": malformed, "tag": "must-not-exist" }),
        ),
        ("list_tags", json!({ "pane_id": malformed })),
        (
            "create_pane",
            json!({ "parent_pane_id": malformed, "no_wait": true }),
        ),
    ];
    for (offset, (tool, arguments)) in invalid_calls.into_iter().enumerate() {
        let invalid = call_tool(
            &client,
            addr,
            None,
            control_session.as_deref(),
            100 + offset as u64,
            tool,
            arguments,
        )
        .await;
        assert_eq!(invalid["error"]["code"], -32602, "{tool}: {invalid}");
        assert_eq!(
            invalid["error"]["data"]["code"], "invalid_pane_id",
            "{tool}: {invalid}"
        );
        assert!(
            invalid["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("Expected exactly \"%<digits>\"")
                    && message.contains("Do not spell \"%\" as \"percent\"")),
            "{tool}: {invalid}"
        );
        assert_eq!(
            invalid["error"]["data"]["retry_with"]["tool"], "list_panes",
            "{tool}: {invalid}"
        );
        assert!(
            invalid["error"]["data"]["valid_panes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|valid| valid == &json!(pane_id)),
            "{tool}: {invalid}"
        );
    }

    let (_, wait_session) = initialize(&client, addr, None).await;
    let (_, send_session) = initialize(&client, addr, None).await;
    let wait = call_tool(
        &client,
        addr,
        None,
        wait_session.as_deref(),
        32,
        "wait_for_event",
        json!({
            "advanced": true,
            "pane_id": pane_id.clone(),
            "transcripts": ["assistant"],
            "timeout": 10
        }),
    );
    let send = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        call_tool(
            &client,
            addr,
            None,
            send_session.as_deref(),
            33,
            "send_prompt",
            json!({ "pane_id": pane_id.clone(), "prompt": "MCP_WAIT_CANARY" }),
        )
        .await
    };
    let (waited, sent) = tokio::join!(wait, send);
    assert_eq!(tool_payload(&sent)["pane_ids"], json!([pane_id]));
    let waited = tool_payload(&waited);
    assert_eq!(waited["record"]["event_type"], "agentMessage", "{waited}");
    assert!(
        waited["record"]["payload"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("MCP_WAIT_CANARY")),
        "{waited}"
    );
}

#[tokio::test]
async fn implicit_mutations_create_owned_panes_instead_of_using_untagged_matches() {
    let Some((env, _daemon, _codex_home)) = spawn_codex_env() else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    let addr = start_mcp(env.socket_path(), None).await;
    let client = http_client();
    let (_, session) = initialize(&client, addr, None).await;

    let created = call_tool(
        &client,
        addr,
        None,
        session.as_deref(),
        73,
        "create_pane",
        json!({ "account": "codex", "backend": "codex", "ready_timeout": 20 }),
    )
    .await;
    let unowned = tool_payload(&created)["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let removed = call_tool(
        &client,
        addr,
        None,
        session.as_deref(),
        74,
        "remove_tag",
        json!({ "pane_id": unowned, "tag": "slopd-mcp" }),
    )
    .await;
    assert_eq!(tool_payload(&removed)["pane_id"], unowned);

    let asked = call_tool(
        &client,
        addr,
        None,
        session.as_deref(),
        75,
        "ask_agent",
        json!({
            "account": "codex",
            "backend": "codex",
            "prompt": "IMPLICIT_OWNERSHIP_CANARY",
            "wait_seconds": 10
        }),
    )
    .await;
    let asked = tool_payload(&asked);
    let owned = asked["pane_id"].as_str().unwrap().to_string();
    assert_ne!(owned, unowned);
    assert_eq!(asked["status"], "completed");
    assert_eq!(asked["reply"], "mock response: IMPLICIT_OWNERSHIP_CANARY");

    let tags = call_tool(
        &client,
        addr,
        None,
        session.as_deref(),
        76,
        "list_tags",
        json!({ "pane_id": owned }),
    )
    .await;
    assert_eq!(tool_payload(&tags)["tags"], json!(["slopd-mcp"]));

    let sent = call_tool(
        &client,
        addr,
        None,
        session.as_deref(),
        77,
        "send_prompt",
        json!({
            "account": "codex",
            "backend": "codex",
            "prompt": "FILTERED_OWNERSHIP_CANARY"
        }),
    )
    .await;
    assert_eq!(tool_payload(&sent)["pane_ids"], json!([owned]));

    for (id, pane_id) in [(78, owned), (79, unowned)] {
        let killed = call_tool(
            &client,
            addr,
            None,
            session.as_deref(),
            id,
            "kill_pane",
            json!({ "pane_id": pane_id }),
        )
        .await;
        assert_eq!(tool_payload(&killed)["pane_id"], pane_id);
    }
}

#[tokio::test]
async fn simple_mode_hides_events_and_waits_for_the_final_reply() {
    let Some((env, _daemon, _codex_home)) = spawn_codex_env() else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    let addr = start_mcp(env.socket_path(), None).await;
    let client = http_client();
    let (initialized, control_session) = initialize(&client, addr, None).await;
    assert!(
        initialized["result"]["instructions"]
            .as_str()
            .is_some_and(|text| text.contains("get_agent_result with no arguments")),
        "{initialized}"
    );
    let listed = rpc_json(
        &client,
        addr,
        None,
        control_session.as_deref(),
        json!({ "jsonrpc": "2.0", "id": 80, "method": "tools/list" }),
    )
    .await;
    let names = tool_names(&listed);
    assert!(names.iter().any(|name| name == "ask_agent"));
    assert!(names.iter().any(|name| name == "get_agent_result"));
    assert!(names.iter().any(|name| name == "wait_for_reply"));
    assert!(!names.iter().any(|name| name == "collect_events"));
    assert!(!names.iter().any(|name| name == "wait_for_event"));
    let transcript_tool = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "read_transcript")
        .unwrap();
    assert_eq!(
        transcript_tool["inputSchema"]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["advanced", "limit", "pane_id"]
    );
    let ask_tool = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "ask_agent")
        .unwrap();
    assert_eq!(ask_tool["inputSchema"]["required"], json!(["prompt"]));
    assert!(
        ask_tool["inputSchema"]["properties"]
            .get("backend")
            .is_some()
    );

    let hidden = call_tool(
        &client,
        addr,
        None,
        control_session.as_deref(),
        81,
        "collect_events",
        json!({ "timeout": 1 }),
    )
    .await;
    assert_eq!(hidden["error"]["code"], -32602, "{hidden}");
    assert!(
        hidden["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("advanced=true")),
        "{hidden}"
    );

    let run = call_tool(
        &client,
        addr,
        None,
        control_session.as_deref(),
        82,
        "create_pane",
        json!({ "account": "codex", "backend": "codex", "ready_timeout": 20 }),
    )
    .await;
    let pane_id = tool_payload(&run)["pane_id"].as_str().unwrap().to_string();
    let (_, wait_session) = initialize(&client, addr, None).await;
    let (_, send_session) = initialize(&client, addr, None).await;
    let wait = call_tool(
        &client,
        addr,
        None,
        wait_session.as_deref(),
        83,
        "wait_for_reply",
        json!({ "pane_id": pane_id.clone(), "timeout": 10 }),
    );
    let send = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        call_tool(
            &client,
            addr,
            None,
            send_session.as_deref(),
            84,
            "send_prompt",
            json!({ "pane_id": pane_id.clone(), "prompt": "FOREIGN_HELPER_CANARY" }),
        )
        .await
    };
    let (waited, sent) = tokio::join!(wait, send);
    assert_eq!(tool_payload(&sent)["pane_ids"], json!([pane_id]));
    let waited = tool_payload(&waited);
    assert_eq!(waited["pane_id"], pane_id);
    assert_eq!(waited["reply"], "main session finished");

    let asked = call_tool(
        &client,
        addr,
        None,
        control_session.as_deref(),
        89,
        "ask_agent",
        json!({
            "backend": "codex",
            "prompt": "MAILBOX_SYNC_CANARY",
            "wait_seconds": 10
        }),
    )
    .await;
    let asked = tool_payload(&asked);
    assert_eq!(asked["pane_id"], pane_id);
    assert_eq!(asked["status"], "completed");
    assert_eq!(asked["reply"], "mock response: MAILBOX_SYNC_CANARY");

    let transcript = call_tool(
        &client,
        addr,
        None,
        control_session.as_deref(),
        85,
        "read_transcript",
        json!({ "pane_id": pane_id.clone(), "limit": 20 }),
    )
    .await;
    let transcript = tool_payload(&transcript);
    let records = transcript["records"].as_array().unwrap();
    assert!(records.iter().all(|record| {
        record.as_object().is_some_and(|record| {
            record.len() == 2 && record.contains_key("role") && record.contains_key("text")
        })
    }));
    assert!(
        records
            .iter()
            .any(|record| record["text"] == "FOREIGN_HELPER_CANARY")
    );
    assert!(
        records
            .iter()
            .any(|record| record["text"] == "main session finished")
    );
    assert!(
        !records
            .iter()
            .any(|record| { record["text"] == "I am still working after the helper runs." })
    );

    let overview = call_tool(
        &client,
        addr,
        None,
        control_session.as_deref(),
        96,
        "get_work_overview",
        json!({ "pane_id": pane_id.clone() }),
    )
    .await;
    let overview = tool_payload(&overview);
    assert_eq!(overview["count"], 1, "{overview}");
    assert_eq!(overview["state_counts"]["ready"], 1, "{overview}");
    assert_eq!(overview["panes"][0]["backend"], "codex", "{overview}");
    assert_eq!(
        overview["panes"][0]["last_request_excerpt"], "MAILBOX_SYNC_CANARY",
        "{overview}"
    );
    assert_eq!(
        overview["panes"][0]["latest_reply_excerpt"], "mock response: MAILBOX_SYNC_CANARY",
        "{overview}"
    );
    assert!(
        overview["answer"]
            .as_str()
            .is_some_and(|answer| answer.contains("codex, ready")),
        "{overview}"
    );

    let advanced = call_tool(
        &client,
        addr,
        None,
        control_session.as_deref(),
        86,
        "read_transcript",
        json!({ "pane_id": pane_id.clone(), "advanced": true, "limit": 20 }),
    )
    .await;
    let advanced = tool_payload(&advanced);
    assert!(
        advanced["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record.get("payload").is_some()),
        "{advanced}"
    );

    let raw = call_tool(
        &client,
        addr,
        None,
        control_session.as_deref(),
        87,
        "read_transcript",
        json!({ "pane_id": pane_id.clone(), "raw": true }),
    )
    .await;
    assert_eq!(raw["error"]["code"], -32602, "{raw}");
    assert!(
        raw["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("advanced=true")),
        "{raw}"
    );

    let pending = call_tool(
        &client,
        addr,
        None,
        control_session.as_deref(),
        90,
        "ask_agent",
        json!({
            "pane_id": pane_id.clone(),
            "prompt": "::mock active",
            "wait_seconds": 0
        }),
    )
    .await;
    let pending = tool_payload(&pending);
    assert_eq!(pending["status"], "pending");
    assert_eq!(pending["finished"], false);
    assert_eq!(
        pending["answer"],
        "The agent is still running and has not replied yet."
    );
    let pending_id = pending["request_id"].as_str().unwrap().to_string();

    let (_, fresh_session) = initialize(&client, addr, None).await;
    let mailbox = call_tool(
        &client,
        addr,
        None,
        fresh_session.as_deref(),
        91,
        "get_agent_result",
        json!({}),
    )
    .await;
    let mailbox = tool_payload(&mailbox);
    assert_eq!(mailbox["found"], true);
    assert_eq!(mailbox["request_id"], pending_id);
    assert_eq!(mailbox["prompt"], "::mock active");
    assert_eq!(mailbox["status"], "pending");
    assert_eq!(mailbox["finished"], false);

    let steered = call_tool(
        &client,
        addr,
        None,
        control_session.as_deref(),
        92,
        "send_prompt",
        json!({ "pane_id": pane_id.clone(), "prompt": "MAILBOX_ASYNC_FINISH" }),
    )
    .await;
    assert_eq!(tool_payload(&steered)["pane_ids"], json!([pane_id]));

    let completed = call_tool(
        &client,
        addr,
        None,
        fresh_session.as_deref(),
        93,
        "get_agent_result",
        json!({ "wait_seconds": 10 }),
    )
    .await;
    let completed = tool_payload(&completed);
    assert_eq!(completed["found"], true);
    assert_eq!(completed["request_id"], pending_id);
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["finished"], true);
    assert_eq!(completed["reply"], "steered: MAILBOX_ASYNC_FINISH");
    assert_eq!(completed["answer"], "steered: MAILBOX_ASYNC_FINISH");

    let killed = call_tool(
        &client,
        addr,
        None,
        control_session.as_deref(),
        88,
        "kill_pane",
        json!({ "pane_id": pane_id }),
    )
    .await;
    assert_eq!(tool_payload(&killed)["pane_id"], waited["pane_id"]);
}

#[tokio::test]
async fn oauth_discovery_and_code_flow_issue_a_usable_bearer() {
    let Some((env, _daemon, _claude_config)) = spawn_env() else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    let public_url = "https://mcp.example";
    let addr =
        start_mcp_with_public_url(env.socket_path(), Some("secret"), Some(public_url.into())).await;
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
    assert_eq!(metadata["resource"], format!("{public_url}/mcp"));
    assert!(
        metadata["authorization_servers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|server| server == &json!(public_url))
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
        format!("{public_url}/oauth/authorize")
    );
    assert_eq!(
        as_meta["authorization_response_iss_parameter_supported"],
        true
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
    let resource = format!("{public_url}/mcp");
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
            ("resource", &resource),
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
    assert!(
        location.contains("iss=https%3A%2F%2Fmcp.example"),
        "{location}"
    );
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
            ("resource", &resource),
        ])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(token.get("expires_in").is_none(), "{token}");
    let access = token["access_token"].as_str().unwrap();

    let (initialized, _) = initialize(&http_client(), addr, Some(access)).await;
    assert_eq!(initialized["result"]["serverInfo"]["name"], "slopd-mcp");

    let restarted =
        start_mcp_with_public_url(env.socket_path(), Some("secret"), Some(public_url.into())).await;
    let (initialized, _) = initialize(&http_client(), restarted, Some(access)).await;
    assert_eq!(initialized["result"]["serverInfo"]["name"], "slopd-mcp");
}
