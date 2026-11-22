use super::*;

async fn modern_tool(
    client: &reqwest::Client,
    addr: SocketAddr,
    token: Option<&str>,
    path: &str,
    name: &str,
    arguments: Value,
) -> reqwest::Response {
    let mut request = client
        .post(format!("http://{addr}{path}"))
        .header("Accept", "application/json, text/event-stream")
        .header("Mcp-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "tools/call")
        .header("Mcp-Name", name)
        .json(&json!({
            "jsonrpc": "2.0", "id": 500, "method": "tools/call",
            "params": {
                "name": name, "arguments": arguments,
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientInfo": {"name":"higher-order-test", "version":"1"},
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        }));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    request.send().await.unwrap()
}

#[tokio::test]
async fn discovery_matches_the_current_visible_registry_on_both_endpoints() {
    let dir = libsloptest::tempfile::tempdir().unwrap();
    let addr = start_mcp(dir.path().join("absent.sock"), Some("secret")).await;
    let client = http_client();
    for (path, count) in [("/mcp", 14), ("/mcp/slopctl", 31)] {
        let (_, session) = initialize_at(&client, addr, Some("secret"), path).await;
        let listed = read_rpc_message(
            rpc_at(
                &client,
                addr,
                Some("secret"),
                session.as_deref(),
                path,
                json!({"jsonrpc":"2.0","id":501,"method":"tools/list"}),
            )
            .await,
        )
        .await;
        let discovered = call_tool_at(
            &client,
            addr,
            Some("secret"),
            session.as_deref(),
            path,
            502,
            "discover_tools",
            json!({}),
        )
        .await;
        let payload = tool_payload(&discovered);
        assert_eq!(payload["tools"], listed["result"]["tools"]);
        assert_eq!(payload["tools"].as_array().unwrap().len(), count);
        assert_eq!(
            payload["dispatch_excluded"],
            json!(["discover_tools", "dispatch_tool"])
        );
        for tool in payload["tools"].as_array().unwrap() {
            assert!(tool["name"].is_string() && tool["description"].is_string());
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert!(tool["outputSchema"].is_object() && tool["annotations"].is_object());
        }
        let modern = read_rpc_message(
            modern_tool(
                &client,
                addr,
                Some("secret"),
                path,
                "discover_tools",
                json!({}),
            )
            .await,
        )
        .await;
        assert_eq!(tool_payload(&modern), payload);
        let direct = call_tool_at(
            &client,
            addr,
            Some("secret"),
            session.as_deref(),
            path,
            503,
            "get_agent_result",
            json!({}),
        )
        .await;
        let forwarded = read_rpc_message(
            modern_tool(
                &client,
                addr,
                Some("secret"),
                path,
                "dispatch_tool",
                json!({"name":"get_agent_result","arguments":{}}),
            )
            .await,
        )
        .await;
        assert_eq!(
            forwarded["result"]["structuredContent"],
            direct["result"]["structuredContent"]
        );
        assert_eq!(forwarded["result"]["content"], direct["result"]["content"]);
        assert_eq!(forwarded["result"]["isError"], direct["result"]["isError"]);
    }
}

#[tokio::test]
async fn higher_order_tools_require_auth_and_reject_hidden_recursive_or_unstructured_calls() {
    let dir = libsloptest::tempfile::tempdir().unwrap();
    let addr = start_mcp(dir.path().join("absent.sock"), Some("secret")).await;
    let client = http_client();
    for path in ["/mcp", "/mcp/slopctl"] {
        for token in [None, Some("wrong")] {
            for (name, args) in [
                ("discover_tools", json!({})),
                (
                    "dispatch_tool",
                    json!({"name":"get_agent_result","arguments":{}}),
                ),
            ] {
                let denied = modern_tool(&client, addr, token, path, name, args).await;
                assert_eq!(denied.status(), reqwest::StatusCode::UNAUTHORIZED);
            }
        }
        for args in [
            json!({}),
            json!({"name":"get_agent_result"}),
            json!({"name":"get_agent_result","arguments":"{}"}),
            json!({"name":"get_agent_result","arguments":null}),
            json!({"name":"get_agent_result","arguments":[]}),
            json!({"name":"get_agent_result","arguments":{},"code":"anything"}),
            json!({"name":"dispatch_tool","arguments":{"name":"close_agent","arguments":{"pane_id":"%1"}}}),
            json!({"name":"discover_tools","arguments":{}}),
        ] {
            let result: Value =
                modern_tool(&client, addr, Some("secret"), path, "dispatch_tool", args)
                    .await
                    .json()
                    .await
                    .unwrap();
            assert_eq!(result["error"]["code"], -32602, "{result}");
        }
        for name in [
            "missing_tool",
            "collect_events",
            "wait_for_event",
            "listen",
            "run",
            "ps",
            "get_agent_result({})",
        ] {
            let result: Value = modern_tool(
                &client,
                addr,
                Some("secret"),
                path,
                "dispatch_tool",
                json!({"name":name,"arguments":{"advanced":true}}),
            )
            .await
            .json()
            .await
            .unwrap();
            assert_eq!(result["error"]["code"], -32602, "{name}: {result}");
            assert!(
                result["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("not available for dispatch")
            );
        }
        let invalid: Value = modern_tool(
            &client,
            addr,
            Some("secret"),
            path,
            "discover_tools",
            json!({"endpoint":"/mcp/slopctl"}),
        )
        .await
        .json()
        .await
        .unwrap();
        assert_eq!(invalid["error"]["code"], -32602);
    }
    let denied: Value = modern_tool(
        &client,
        addr,
        Some("secret"),
        "/mcp",
        "dispatch_tool",
        json!({"name":"create_pane","arguments":{}}),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(denied["error"]["code"], -32602);
    let allowed: Value = modern_tool(
        &client,
        addr,
        Some("secret"),
        "/mcp/slopctl",
        "dispatch_tool",
        json!({"name":"get_status","arguments":{}}),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(allowed["error"]["data"]["code"], "slopd_unavailable");
}

#[tokio::test]
async fn dispatch_preserves_handler_validation_error_shapes_and_notification_resources() {
    let dir = libsloptest::tempfile::tempdir().unwrap();
    let addr = start_mcp(dir.path().join("absent.sock"), None).await;
    let client = http_client();
    let (_, session) = initialize(&client, addr, None).await;
    for (name, args) in [
        ("close_agent", json!({"pane_id":"33"})),
        ("get_agent_result", json!({"wait_seconds":"forever"})),
        ("start_new_agent", json!({"prompt":""})),
        ("get_status", json!({})),
    ] {
        let direct = call_tool(
            &client,
            addr,
            None,
            session.as_deref(),
            510,
            name,
            args.clone(),
        )
        .await;
        let forwarded = call_tool(
            &client,
            addr,
            None,
            session.as_deref(),
            511,
            "dispatch_tool",
            json!({"name":name,"arguments":args}),
        )
        .await;
        assert_eq!(forwarded["error"], direct["error"], "{name}");
        assert_eq!(forwarded["result"], direct["result"], "{name}");
    }
    let failed = call_tool(
        &client,
        addr,
        None,
        session.as_deref(),
        512,
        "dispatch_tool",
        json!({"name":"start_new_agent","arguments":{"prompt":"NO_DAEMON","wait_seconds":1}}),
    )
    .await;
    let request_id = tool_payload(&failed)["request_id"].clone();
    assert_eq!(tool_payload(&failed)["status"], "failed");
    let watch = call_tool(
        &client,
        addr,
        None,
        session.as_deref(),
        513,
        "dispatch_tool",
        json!({"name":"configure_notifications","arguments":{"request_id":request_id}}),
    )
    .await;
    assert_eq!(watch["result"]["content"][0]["type"], "resource_link");
    assert!(watch["result"]["structuredContent"]["resource_uri"].is_string());
}

#[tokio::test]
async fn dispatched_mutations_use_the_normal_daemon_and_pending_work_guard() {
    let (env, _daemon, _codex_home) = spawn_codex_env().expect("tmux is required for dispatch E2E");
    let addr = start_mcp(env.socket_path(), Some("secret")).await;
    let client = http_client();
    let (_, session) = initialize_at(&client, addr, Some("secret"), "/mcp").await;
    let started = call_tool_at(&client, addr, Some("secret"), session.as_deref(), "/mcp", 520, "dispatch_tool", json!({"name":"start_new_agent","arguments":{"backend":"codex","account":"codex","prompt":"::mock active","wait_seconds":3}})).await;
    let pending = tool_payload(&started);
    assert_eq!(pending["status"], "pending");
    let pane = pending["pane_id"].as_str().unwrap();
    let args = json!({"prompt":"ANOTHER_INDEPENDENT_AGENT","wait_seconds":0});
    let direct = call_tool_at(
        &client,
        addr,
        Some("secret"),
        session.as_deref(),
        "/mcp",
        521,
        "start_new_agent",
        args.clone(),
    )
    .await;
    let blocked = call_tool_at(
        &client,
        addr,
        Some("secret"),
        session.as_deref(),
        "/mcp",
        522,
        "dispatch_tool",
        json!({"name":"start_new_agent","arguments":args}),
    )
    .await;
    assert_eq!(blocked["result"], direct["result"]);
    assert_eq!(tool_payload(&blocked)["status"], "blocked_pending_agent");
    let closed = call_tool_at(
        &client,
        addr,
        Some("secret"),
        session.as_deref(),
        "/mcp",
        523,
        "dispatch_tool",
        json!({"name":"close_agent","arguments":{"pane_id":pane}}),
    )
    .await;
    assert_eq!(tool_payload(&closed)["closed"], true);
    let (_, full) = initialize(&client, addr, Some("secret")).await;
    let panes = call_tool(
        &client,
        addr,
        Some("secret"),
        full.as_deref(),
        524,
        "list_panes",
        json!({}),
    )
    .await;
    assert_eq!(tool_payload(&panes)["count"], 0);
}
