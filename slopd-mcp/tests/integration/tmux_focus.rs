use super::*;

fn tmux(env: &TestEnv, args: &[&str]) -> String {
    let output = env.tmux.tmux().args(args).output().unwrap();
    assert!(output.status.success(), "{args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn new_window(env: &TestEnv, session: &str) -> (String, String, String) {
    let output = tmux(
        env,
        &[
            "new-window",
            "-d",
            "-t",
            &format!("{session}:"),
            "-P",
            "-F",
            "#{window_id} #{pane_id}",
            "sleep infinity",
        ],
    );
    let (window, first) = output.split_once(' ').unwrap();
    let second = tmux(
        env,
        &[
            "split-window",
            "-d",
            "-t",
            first,
            "-P",
            "-F",
            "#{pane_id}",
            "sleep infinity",
        ],
    );
    (window.into(), first.into(), second)
}

fn snapshot(env: &TestEnv, session: Option<&str>) -> String {
    let mut args = vec!["list-panes"];
    if let Some(session) = session {
        args.extend(["-s", "-t", session]);
    } else {
        args.push("-a");
    }
    args.extend([
        "-F",
        "#{session_id} #{window_id} #{window_active} #{pane_id} #{pane_active}",
    ]);
    let output = tmux(env, &args);
    let mut lines: Vec<_> = output.lines().collect();
    lines.sort_unstable();
    lines.join("\n")
}

#[tokio::test]
async fn focus_switching_checks_membership_and_preserves_other_sessions() {
    build_bin("slopd");
    build_bin("slopctl");
    let slopctl = cargo_bin("slopctl");
    let env = TestEnv::new_with_tmux_session(
        Some(&["sleep", "infinity"]),
        Some(slopctl.to_str().unwrap()),
        "focus-slopd",
    )
    .expect("tmux is required for focus E2E");
    let _daemon = Daemon(Some(env.spawn_slopd()));
    let session_id = tmux(
        &env,
        &[
            "display-message",
            "-p",
            "-t",
            "=focus-slopd:",
            "#{session_id}",
        ],
    );
    let (a, a1, a2) = new_window(&env, "focus-slopd");
    let (b, b1, b2) = new_window(&env, "focus-slopd");
    let (foreign, foreign1, _) = new_window(&env, "test");
    let addr = start_mcp(env.socket_path(), Some("secret")).await;
    let client = http_client();

    for path in ["/mcp", "/mcp/slopctl"] {
        let (_, session) = initialize_at(&client, addr, Some("secret"), path).await;
        let discovered = call_tool_at(
            &client,
            addr,
            Some("secret"),
            session.as_deref(),
            path,
            600,
            "discover_tools",
            json!({}),
        )
        .await;
        let discovered = tool_payload(&discovered);
        let definition = discovered["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "switch_tmux_focus")
            .unwrap();
        assert_eq!(
            definition["inputSchema"]["properties"]["window_id"]["pattern"],
            "^@[0-9]+$"
        );
        assert_eq!(
            definition["inputSchema"]["anyOf"],
            json!([{"required":["window_id"]},{"required":["pane_id"]}])
        );
        assert_eq!(definition["annotations"]["readOnlyHint"], false);
        assert_eq!(definition["annotations"]["destructiveHint"], false);
        assert_eq!(definition["annotations"]["idempotentHint"], true);

        for forwarded in [false, true] {
            tmux(&env, &["select-pane", "-t", &a1]);
            tmux(&env, &["select-pane", "-t", &b1]);
            tmux(&env, &["select-window", "-t", &format!("{session_id}:{b}")]);
            let unrelated = snapshot(&env, Some("=test"));
            for (args, window, pane) in [
                (json!({"window_id":a}), &a, &a1),
                (json!({"pane_id":b2}), &b, &b2),
                (json!({"window_id":a,"pane_id":a2}), &a, &a2),
                (json!({"window_id":b}), &b, &b2),
            ] {
                let (name, args) = if forwarded {
                    (
                        "dispatch_tool",
                        json!({"name":"switch_tmux_focus","arguments":args}),
                    )
                } else {
                    ("switch_tmux_focus", args)
                };
                let result = call_tool_at(
                    &client,
                    addr,
                    Some("secret"),
                    session.as_deref(),
                    path,
                    601,
                    name,
                    args,
                )
                .await;
                assert_eq!(
                    tool_payload(&result),
                    json!({"session_id":session_id,"window_id":window,"pane_id":pane})
                );
                assert_eq!(
                    tmux(
                        &env,
                        &[
                            "display-message",
                            "-p",
                            "-t",
                            "=focus-slopd:",
                            "#{window_id} #{pane_id}"
                        ]
                    ),
                    format!("{window} {pane}")
                );
                assert_eq!(snapshot(&env, Some("=test")), unrelated);
            }

            for args in [
                json!({}),
                json!({"pane_id":"1"}),
                json!({"window_id":"1"}),
                json!({"window_id":"@"}),
                json!({"window_id":"@1; kill-server"}),
                json!({"pane_id":"%1.0"}),
                json!({"window_id":null,"pane_id":a1}),
                json!({"pane_id":12}),
                json!({"window_id":a,"session":"test"}),
                json!({"window_id":"@999999999"}),
                json!({"pane_id":"%999999999"}),
                json!({"window_id":foreign}),
                json!({"pane_id":foreign1}),
                json!({"window_id":foreign,"pane_id":foreign1}),
                json!({"window_id":a,"pane_id":b1}),
                json!({"window_id":a,"pane_id":foreign1}),
                json!({"window_id":foreign,"pane_id":a1}),
            ] {
                let before = snapshot(&env, None);
                let (name, args) = if forwarded {
                    (
                        "dispatch_tool",
                        json!({"name":"switch_tmux_focus","arguments":args}),
                    )
                } else {
                    ("switch_tmux_focus", args)
                };
                let result = call_tool_at(
                    &client,
                    addr,
                    Some("secret"),
                    session.as_deref(),
                    path,
                    602,
                    name,
                    args.clone(),
                )
                .await;
                assert!(result["error"].is_object(), "{args}: {result}");
                assert_eq!(snapshot(&env, None), before, "{args}");
            }
        }
        for token in [None, Some("wrong")] {
            for (name, arguments) in [
                ("switch_tmux_focus", json!({"pane_id":a1})),
                (
                    "dispatch_tool",
                    json!({"name":"switch_tmux_focus","arguments":{"pane_id":a1}}),
                ),
            ] {
                let before = snapshot(&env, None);
                let result = rpc_at(&client, addr, token, session.as_deref(), path, json!({"jsonrpc":"2.0","id":603,"method":"tools/call","params":{"name":name,"arguments":arguments}})).await;
                assert_eq!(result.status(), reqwest::StatusCode::UNAUTHORIZED);
                assert_eq!(snapshot(&env, None), before);
            }
        }
    }

    // A linked window shares its selected pane across sessions. Window-only
    // focus is safe; selecting a pane must fail without altering either session.
    tmux(&env, &["link-window", "-d", "-s", &a, "-t", "test:"]);
    let (_, session) = initialize(&client, addr, Some("secret")).await;
    for args in [json!({"pane_id":a1}), json!({"window_id":a,"pane_id":a1})] {
        let before = snapshot(&env, None);
        let result = call_tool(
            &client,
            addr,
            Some("secret"),
            session.as_deref(),
            604,
            "switch_tmux_focus",
            args,
        )
        .await;
        assert!(
            result["error"]["message"]
                .as_str()
                .unwrap()
                .contains("linked"),
            "{result}"
        );
        assert_eq!(snapshot(&env, None), before);
    }
    let unrelated = snapshot(&env, Some("=test"));
    let result = call_tool(
        &client,
        addr,
        Some("secret"),
        session.as_deref(),
        605,
        "switch_tmux_focus",
        json!({"window_id":a}),
    )
    .await;
    assert_eq!(tool_payload(&result)["window_id"], a);
    assert_eq!(snapshot(&env, Some("=test")), unrelated);

    // Enforce the same ID and membership validation for callers of the daemon
    // RPC itself, without relying on MCP or libslopctl's convenience method.
    let stream = tokio::net::UnixStream::connect(env.socket_path())
        .await
        .unwrap();
    let (read, write) = stream.into_split();
    let mut daemon = libslopctl::Client::new(read, write);
    for (window_id, pane_id) in [
        (None, None),
        (Some("test"), None),
        (None, Some(foreign1.as_str())),
    ] {
        let before = snapshot(&env, None);
        assert!(
            daemon
                .request(libslop::RequestBody::SwitchTmuxFocus {
                    window_id: window_id.map(str::to_owned),
                    pane_id: pane_id.map(str::to_owned)
                })
                .await
                .is_err()
        );
        assert_eq!(snapshot(&env, None), before);
    }
}
