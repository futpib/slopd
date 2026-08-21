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
    let oauth_state = socket.with_extension("mcp-oauth.jsonl");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mcp");
    let addr = listener.local_addr().expect("local addr");
    let config = ServeConfig {
        socket,
        oauth_state,
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
            "list_panes",
            "create_pane",
            "fork_pane",
            "kill_pane",
            "send_prompt",
            "interrupt_pane",
            "collect_events",
            "wait_for_event",
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
    for tool in listed["result"]["tools"].as_array().unwrap() {
        assert!(tool["title"].is_string(), "{tool}");
        assert!(tool["outputSchema"].is_object(), "{tool}");
        assert!(tool["annotations"].is_object(), "{tool}");
        assert_eq!(tool["annotations"]["openWorldHint"], false, "{tool}");
    }
    let expected_annotations = [
        ("get_status", true, false, true),
        ("list_panes", true, false, true),
        ("create_pane", false, false, false),
        ("fork_pane", false, false, false),
        ("kill_pane", false, true, true),
        ("send_prompt", false, false, false),
        ("interrupt_pane", false, true, false),
        ("collect_events", true, false, true),
        ("wait_for_event", true, false, true),
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
    assert_eq!(tool_payload(&tags)["tags"], json!(["mcp-parity"]));
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

    let waited = call_tool(
        &client,
        addr,
        None,
        session,
        14,
        "wait_for_event",
        json!({
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
    assert_eq!(tool_payload(&sent)["pane_ids"], json!([pane_id]));

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
        json!({ "pane_id": pane_id, "limit": 50, "raw": true }),
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
            "send_prompt",
            json!({ "pane_id": malformed, "prompt": "must not be sent" }),
        ),
        ("interrupt_pane", json!({ "pane_id": malformed })),
        (
            "collect_events",
            json!({ "pane_id": malformed, "timeout": 1 }),
        ),
        (
            "wait_for_event",
            json!({ "pane_id": malformed, "timeout": 1 }),
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
async fn oauth_discovery_and_code_flow_issue_a_usable_bearer() {
    let Some((env, _daemon, _claude_config)) = spawn_env() else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    let addr = start_mcp(env.socket_path(), Some("secret")).await;
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
    assert!(token.get("expires_in").is_none(), "{token}");
    let access = token["access_token"].as_str().unwrap();

    let (initialized, _) = initialize(&http_client(), addr, Some(access)).await;
    assert_eq!(initialized["result"]["serverInfo"]["name"], "slopd-mcp");

    let restarted = start_mcp(env.socket_path(), Some("secret")).await;
    let (initialized, _) = initialize(&http_client(), restarted, Some(access)).await;
    assert_eq!(initialized["result"]["serverInfo"]["name"], "slopd-mcp");
}
