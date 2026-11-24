use super::*;

fn account_pool_journey(backend: &str, mock_name: &str) {
    for binary in ["slopd", "slopctl", "slopd-acp", mock_name] {
        build_bin(binary);
    }
    let mock = cargo_bin(mock_name);
    let slopctl = cargo_bin("slopctl");
    let homes = libsloptest::tempfile::tempdir().unwrap();
    let a = homes.path().join("primary");
    let b = homes.path().join("spare");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(a.join("credentials-canary"), "primary-secret").unwrap();
    std::fs::write(b.join("credentials-canary"), "spare-secret").unwrap();
    let Some(env) = TestEnv::new_full(None, Some(slopctl.to_str().unwrap()), None) else {
        return;
    };
    env.append_config(&format!(
        r#"
        [accounts.primary]
        backend = {backend:?}
        executable = {:?}
        config_dir = {a:?}
        [accounts.primary-alias]
        backend = {backend:?}
        executable = {:?}
        config_dir = {a:?}
        [accounts.spare]
        backend = {backend:?}
        executable = {:?}
        config_dir = {b:?}
        [account_pools.work]
        accounts = ["primary", "primary-alias", "spare"]
    "#,
        mock.to_str().unwrap(),
        mock.to_str().unwrap(),
        mock.to_str().unwrap()
    ));
    let mut daemon = Daemon(Some(env.spawn_slopd()));
    let mut harness = Harness::spawn(&env.socket_path(), &["--account-pool", "work"]);
    initialize(&mut harness);
    let session = new_session(&mut harness, env.config_dir.path(), "SYSTEM_HISTORY_CANARY");
    let (first, _) = prompt(&mut harness, 3, &session, "COMPLETED_WORK_CANARY");
    assert_eq!(first["result"]["stopReason"], "end_turn", "{first}");
    let original = env.slopctl(&["ps", "--json"]);
    let original: Value = serde_json::from_slice(&original.stdout).unwrap();
    let source_path = original[0]["transcript_path"].as_str().unwrap();
    let mut history = std::fs::OpenOptions::new()
        .append(true)
        .open(source_path)
        .unwrap();
    let completed_tool = if backend == "codex" {
        vec![
            json!({"type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"completed-tool-canary","arguments":"{}"}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"completed-tool-canary","output":"already done"}}),
        ]
    } else {
        vec![
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"completed-tool-canary","name":"Bash","input":{}}]}}),
            json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"completed-tool-canary","content":"already done"}]}}),
        ]
    };
    for record in completed_tool {
        writeln!(history, "{record}").unwrap();
    }
    std::fs::write(a.join("mock-depleted"), "exhausted").unwrap();
    let (continued, messages) = prompt(&mut harness, 4, &session, "UNFINISHED_REQUEST_CANARY");
    assert_eq!(continued["result"]["stopReason"], "end_turn", "{continued}");
    let text = streamed_text(&messages);
    assert!(text.contains("continue"), "{text}");
    assert!(
        !serde_json::to_string(&messages)
            .unwrap()
            .contains("completed-tool-canary"),
        "completed tools were replayed"
    );
    assert!(
        !text.contains("COMPLETED_WORK_CANARY"),
        "history was replayed: {text}"
    );
    let panes: Value = serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap();
    assert_eq!(panes.as_array().unwrap().len(), 1, "{panes}");
    assert_eq!(panes[0]["account"], "spare");
    assert_ne!(panes[0]["pane_id"], original[0]["pane_id"]);
    let transcript =
        std::fs::read_to_string(panes[0]["transcript_path"].as_str().unwrap()).unwrap();
    assert!(transcript.contains("SYSTEM_HISTORY_CANARY"));
    assert!(transcript.contains("COMPLETED_WORK_CANARY"));
    assert!(transcript.contains("UNFINISHED_REQUEST_CANARY"));
    assert!(transcript.contains("completed-tool-canary"));
    assert_eq!(
        std::fs::read_to_string(b.join("credentials-canary")).unwrap(),
        "spare-secret"
    );

    // The winning native account and logical session survive both restarts.
    harness.close_stdin_and_wait();
    kill_slopd(daemon.0.take().unwrap());
    daemon.0 = Some(env.spawn_slopd());
    let mut harness = Harness::spawn(&env.socket_path(), &["--account-pool", "work"]);
    initialize(&mut harness);
    let resumed = resume_session(&mut harness, 5, &session, env.config_dir.path());
    assert!(resumed.get("result").is_some(), "{resumed}");
    let (next, _) = prompt(&mut harness, 6, &session, "AFTER_RESTART");
    assert_eq!(next["result"]["stopReason"], "end_turn", "{next}");
    assert!(
        close_session(&mut harness, 8, &session)
            .get("result")
            .is_some()
    );
    assert!(
        resume_session(&mut harness, 9, &session, env.config_dir.path())
            .get("result")
            .is_some()
    );
    let (revived, _) = prompt(&mut harness, 10, &session, "AFTER_GRAVE_REVIVAL");
    assert_eq!(revived["result"]["stopReason"], "end_turn", "{revived}");
    let other = new_session(&mut harness, env.config_dir.path(), "");
    let panes: Value = serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap();
    assert!(
        panes
            .as_array()
            .unwrap()
            .iter()
            .all(|pane| pane["account"] == "spare")
    );
    // All-depleted terminates without looping back through an alias.
    std::fs::write(b.join("mock-depleted"), "exhausted").unwrap();
    let (failed, _) = prompt(&mut harness, 7, &other, "ALL_DEPLETED");
    assert!(
        failed["error"]["message"]
            .as_str()
            .unwrap()
            .contains("exhausted"),
        "{failed}"
    );
}

#[test]
fn codex_account_pool_preserves_history_and_recovers() {
    account_pool_journey("codex", "mock_codex");
}

#[test]
fn claude_account_pool_preserves_history_and_recovers() {
    account_pool_journey("claude", "mock_claude");
}

fn cancel_during_switch(backend: &str, mock_name: &str) {
    for binary in ["slopd", "slopctl", "slopd-acp", mock_name] {
        build_bin(binary);
    }
    let dir = libsloptest::tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let Some(env) = TestEnv::new_full(None, Some(cargo_bin("slopctl").to_str().unwrap()), None)
    else {
        return;
    };
    for (name, home) in [("a", &a), ("b", &b)] {
        env.append_config(&format!("\n[accounts.{name}]\nbackend = {backend:?}\nconfig_dir = {home:?}\nexecutable = {:?}\n", cargo_bin(mock_name).to_str().unwrap()));
    }
    env.append_config("\n[account_pools.work]\naccounts = [\"a\", \"b\"]\n");
    let _daemon = Daemon(Some(env.spawn_slopd()));
    let mut harness = Harness::spawn(&env.socket_path(), &["--account-pool", "work"]);
    initialize(&mut harness);
    let session = new_session(&mut harness, env.config_dir.path(), "");
    std::fs::write(a.join("mock-depleted"), "exhausted").unwrap();
    std::fs::write(b.join("mock-fork-pause"), "wait").unwrap();
    harness.send(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"cancel the switch"}]}}));
    let deadline = Instant::now() + Duration::from_secs(15);
    while !b.join("mock-fork-waiting").exists() {
        assert!(Instant::now() < deadline, "fork never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    harness.send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}));
    let _ = list_sessions(&mut harness, 10);
    std::fs::remove_file(b.join("mock-fork-pause")).unwrap();
    let response = harness.response(3, &mut vec![]);
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    let panes = panes(&env);
    assert_eq!(panes.len(), 1, "replacement must be cleaned up");
    assert_eq!(panes[0].account, "a");
    let (retried, _) = prompt(
        &mut harness,
        4,
        &session,
        "resume after cancelling the switch",
    );
    assert_eq!(retried["result"]["stopReason"], "end_turn", "{retried}");
}

#[test]
fn codex_pool_cancellation_during_fork_cleans_up() {
    cancel_during_switch("codex", "mock_codex");
}

#[test]
fn claude_pool_cancellation_during_fork_cleans_up() {
    cancel_during_switch("claude", "mock_claude");
}
