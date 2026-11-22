use super::*;

fn tool_payload(response: &Value) -> Value {
    let value = response["result"]["structuredContent"].clone();
    assert!(value.is_object(), "{response}");
    value
}

struct Events {
    response: reqwest::Response,
    buffer: Vec<u8>,
}

impl Events {
    fn new(response: reqwest::Response) -> Self {
        assert!(response.status().is_success(), "{}", response.status());
        Self {
            response,
            buffer: Vec::new(),
        }
    }

    async fn next(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                while let Some(end) = self.buffer.windows(2).position(|b| b == b"\n\n") {
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
                self.buffer.extend_from_slice(
                    &self
                        .response
                        .chunk()
                        .await
                        .unwrap()
                        .expect("event stream closed"),
                );
            }
        })
        .await
        .expect("notification timed out")
    }

    async fn updated(&mut self, uri: &str) {
        let event = self.next().await;
        assert_eq!(
            event["method"], "notifications/resources/updated",
            "{event}"
        );
        assert_eq!(event["params"]["uri"], uri, "{event}");
    }

    async fn quiet(&mut self) {
        assert!(
            tokio::time::timeout(Duration::from_millis(200), self.next())
                .await
                .is_err()
        );
    }
}

pub(super) async fn modern(
    client: &reqwest::Client,
    addr: SocketAddr,
    id: u64,
    method: &str,
    mut params: Value,
) -> reqwest::Response {
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "notification-test", "version": "1"},
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    let mut request = client.post(format!("http://{addr}/mcp"));
    if let Some(name) = params
        .get("uri")
        .or_else(|| params.get("name"))
        .and_then(Value::as_str)
    {
        request = request.header("Mcp-Name", name);
    }
    request
        .header("Accept", "application/json, text/event-stream")
        .header("Mcp-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", method)
        .json(&json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}))
        .send()
        .await
        .unwrap()
}

async fn subscribe_modern(client: &reqwest::Client, addr: SocketAddr, uri: &str) -> Events {
    let mut events = Events::new(modern(client, addr, 300, "subscriptions/listen", json!({"notifications":{"resourceSubscriptions":[uri, "slopd://notifications/unknown"]}})).await);
    let ack = events.next().await;
    assert_eq!(
        ack["method"], "notifications/subscriptions/acknowledged",
        "{ack}"
    );
    assert_eq!(
        ack["params"]["notifications"]["resourceSubscriptions"],
        json!([uri]),
        "{ack}"
    );
    events
}

async fn read(client: &reqwest::Client, addr: SocketAddr, uri: &str) -> Value {
    let response =
        read_rpc_message(modern(client, addr, 301, "resources/read", json!({"uri":uri})).await)
            .await;
    serde_json::from_str(
        response["result"]["contents"][0]["text"]
            .as_str()
            .expect("resource text"),
    )
    .unwrap()
}

#[tokio::test]
async fn configured_pane_notifications_cross_retries_and_reach_both_protocols() {
    let (env, _daemon, _codex_home) =
        spawn_codex_env().expect("tmux is required for notification E2E");
    let addr = start_mcp(env.socket_path(), None).await;
    let client = http_client();
    let (info, session) = initialize(&client, addr, None).await;
    assert_eq!(
        info["result"]["capabilities"]["resources"]["subscribe"],
        true
    );
    let created = call_tool(
        &client,
        addr,
        None,
        session.as_deref(),
        310,
        "create_pane",
        json!({"account":"codex","backend":"codex"}),
    )
    .await;
    let pane = tool_payload(&created)["pane_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let configured = call_tool(
        &client,
        addr,
        None,
        session.as_deref(),
        311,
        "configure_notifications",
        json!({"pane_ids":[pane], "events":["completed","failed","closed"]}),
    )
    .await;
    let watch = tool_payload(&configured);
    let uri = watch["resource_uri"].as_str().unwrap();
    assert!(
        configured["result"]["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|block| block["type"] == "resource_link"),
        "{configured}"
    );
    let mut modern_events = subscribe_modern(&client, addr, uri).await;
    modern_events.updated(uri).await;
    let mut legacy_events = Events::new(
        client
            .get(format!("http://{addr}/mcp/slopctl"))
            .header("Accept", "text/event-stream")
            .header("Mcp-Session-Id", session.as_deref().unwrap())
            .send()
            .await
            .unwrap(),
    );
    let subscribed = rpc_json(
        &client,
        addr,
        None,
        session.as_deref(),
        json!({"jsonrpc":"2.0","id":312,"method":"resources/subscribe","params":{"uri":uri}}),
    )
    .await;
    assert!(subscribed.get("result").is_some(), "{subscribed}");
    legacy_events.updated(uri).await;

    let other = call_tool(
        &client,
        addr,
        None,
        session.as_deref(),
        313,
        "start_new_agent",
        json!({"backend":"codex","account":"codex","prompt":"OTHER_PANE","wait_seconds":10}),
    )
    .await;
    assert_eq!(tool_payload(&other)["status"], "completed");
    modern_events.quiet().await;
    legacy_events.quiet().await;

    for (prompt, kind, count) in [
        ("FAIL_CAPACITY_ONCE", "completed", 1),
        ("FAIL_AUTHENTICATION", "failed", 2),
    ] {
        let sent = call_tool(
            &client,
            addr,
            None,
            session.as_deref(),
            314,
            "send_prompt",
            json!({"pane_id":pane,"prompt":prompt}),
        )
        .await;
        assert_eq!(tool_payload(&sent)["pane_ids"], json!([pane]));
        modern_events.updated(uri).await;
        legacy_events.updated(uri).await;
        let state = read(&client, addr, uri).await;
        assert_eq!(state["sequence"], count, "{state}");
        assert_eq!(state["events"][count as usize - 1]["kind"], kind, "{state}");
        assert_eq!(state["events"][count as usize - 1]["pane_id"], pane);
    }
    let unsubscribed = rpc_json(
        &client,
        addr,
        None,
        session.as_deref(),
        json!({"jsonrpc":"2.0","id":315,"method":"resources/unsubscribe","params":{"uri":uri}}),
    )
    .await;
    assert!(unsubscribed.get("result").is_some());
    call_tool(
        &client,
        addr,
        None,
        session.as_deref(),
        316,
        "kill_pane",
        json!({"pane_id":pane}),
    )
    .await;
    modern_events.updated(uri).await;
    legacy_events.quiet().await;
    let state = read(&client, addr, uri).await;
    assert_eq!(state["events"][2]["kind"], "closed", "{state}");
    let disabled = call_tool(
        &client,
        addr,
        None,
        session.as_deref(),
        317,
        "configure_notifications",
        json!({"watch_id":watch["watch_id"],"enabled":false}),
    )
    .await;
    assert_eq!(tool_payload(&disabled)["active"], false);
    modern_events.updated(uri).await;
    assert_eq!(read(&client, addr, uri).await["closed_reason"], "disabled");
}

#[tokio::test]
async fn request_resources_and_filtered_watches_handle_completion_before_subscribe() {
    let (env, _daemon, _codex_home) =
        spawn_codex_env().expect("tmux is required for notification E2E");
    let addr = start_mcp(env.socket_path(), None).await;
    let client = http_client();
    let (_, session) = initialize(&client, addr, None).await;
    let result = call_tool(&client, addr, None, session.as_deref(), 320, "start_new_agent", json!({"backend":"codex","account":"codex","prompt":"NOTIFICATION_RESULT","wait_seconds":10})).await;
    let result = tool_payload(&result);
    assert_eq!(result["status"], "completed");
    let uri = result["resource_uri"].as_str().unwrap();
    let mut events = subscribe_modern(&client, addr, uri).await;
    events.updated(uri).await;
    assert_eq!(
        read(&client, addr, uri).await["reply"],
        "mock response: NOTIFICATION_RESULT"
    );
    events.quiet().await;
    for (kinds, count) in [(json!(["failed"]), 0), (json!(["completed"]), 1)] {
        let configured = call_tool(
            &client,
            addr,
            None,
            session.as_deref(),
            321,
            "configure_notifications",
            json!({"request_id":result["request_id"],"events":kinds}),
        )
        .await;
        let watch = tool_payload(&configured);
        let uri = watch["resource_uri"].as_str().unwrap();
        let mut events = subscribe_modern(&client, addr, uri).await;
        events.updated(uri).await;
        let state = read(&client, addr, uri).await;
        assert_eq!(state["events"].as_array().unwrap().len(), count, "{state}");
        assert_eq!(state["closed_reason"], "request_finished");
    }
    let listed =
        read_rpc_message(modern(&client, addr, 322, "resources/list", json!({})).await).await;
    assert_eq!(listed["result"]["resources"].as_array().unwrap().len(), 3);
    let invalid = modern(
        &client,
        addr,
        323,
        "resources/read",
        json!({"uri":"slopd://requests/missing"}),
    )
    .await
    .json::<Value>()
    .await
    .unwrap();
    assert_eq!(invalid["error"]["code"], -32602);
}

#[tokio::test]
async fn pending_request_notifies_only_after_its_final_reply() {
    let (env, _daemon, _codex_home) =
        spawn_codex_env().expect("tmux is required for notification E2E");
    let addr = start_mcp(env.socket_path(), None).await;
    let client = http_client();
    let (_, session) = initialize(&client, addr, None).await;
    let result = call_tool(
        &client,
        addr,
        None,
        session.as_deref(),
        340,
        "start_new_agent",
        json!({"backend":"codex","account":"codex","prompt":"::mock active","wait_seconds":3}),
    )
    .await;
    let result = tool_payload(&result);
    assert_eq!(result["status"], "pending");
    let uri = result["resource_uri"].as_str().unwrap();
    let mut events = subscribe_modern(&client, addr, uri).await;
    events.quiet().await;
    call_tool(
        &client,
        addr,
        None,
        session.as_deref(),
        341,
        "send_prompt",
        json!({"pane_id":result["pane_id"],"prompt":"NOTIFICATION_ASYNC_FINISH"}),
    )
    .await;
    events.updated(uri).await;
    let value = read(&client, addr, uri).await;
    assert_eq!(value["status"], "completed");
    assert_eq!(value["reply"], "steered: NOTIFICATION_ASYNC_FINISH");
    events.quiet().await;
}

#[tokio::test]
async fn notification_selection_is_explicit_and_watches_expire() {
    let (env, _daemon, _config) = spawn_env().expect("tmux is required for notification E2E");
    let addr = start_mcp(env.socket_path(), None).await;
    let client = http_client();
    let (_, session) = initialize(&client, addr, None).await;
    for args in [
        json!({}),
        json!({"all_agents":true,"pane_ids":["%1"]}),
        json!({"all_agents":true,"events":[]}),
        json!({"all_agents":true,"duration_seconds":0}),
        json!({"pane_ids":["bad"]}),
    ] {
        let invalid = call_tool(
            &client,
            addr,
            None,
            session.as_deref(),
            330,
            "configure_notifications",
            args,
        )
        .await;
        assert_eq!(invalid["error"]["code"], -32602, "{invalid}");
    }
    let configured = call_tool(
        &client,
        addr,
        None,
        session.as_deref(),
        331,
        "configure_notifications",
        json!({"all_agents":true,"duration_seconds":1}),
    )
    .await;
    let watch = tool_payload(&configured);
    let uri = watch["resource_uri"].as_str().unwrap();
    let mut events = subscribe_modern(&client, addr, uri).await;
    events.updated(uri).await;
    events.updated(uri).await;
    assert_eq!(read(&client, addr, uri).await["closed_reason"], "expired");
}
