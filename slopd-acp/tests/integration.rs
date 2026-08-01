use std::io::{BufRead, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use libsloptest::{TestEnv, build_bin, cargo_bin, kill_child, kill_slopd};
use serde_json::{Value, json};

struct Harness {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    receiver: mpsc::Receiver<Value>,
}

impl Harness {
    fn spawn(socket: &std::path::Path, adapter_args: &[&str]) -> Self {
        let mut command = Command::new(cargo_bin("slopd-acp"));
        command
            .args(["--socket", socket.to_str().unwrap()])
            .args(adapter_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command.spawn().expect("spawn slopd-acp");
        let stdin = child.stdin.take().expect("adapter stdin");
        let stdout = child.stdout.take().expect("adapter stdout");
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                let value = serde_json::from_str(&line)
                    .unwrap_or_else(|error| panic!("invalid ACP output {line:?}: {error}"));
                if sender.send(value).is_err() {
                    break;
                }
            }
        });
        Self {
            child: Some(child),
            stdin: Some(stdin),
            receiver,
        }
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().expect("adapter stdin is closed");
        serde_json::to_writer(&mut *stdin, &message).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    fn receive(&self) -> Value {
        self.receiver
            .recv_timeout(Duration::from_secs(30))
            .expect("timed out waiting for ACP output")
    }

    fn response(&self, id: u64, notifications: &mut Vec<Value>) -> Value {
        loop {
            let message = self.receive();
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                return message;
            }
            notifications.push(message);
        }
    }

    fn close_stdin_and_wait(mut self) {
        drop(self.stdin.take());
        let mut child = self.child.take().expect("adapter child");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match child.try_wait().expect("wait for slopd-acp") {
                Some(status) => {
                    assert!(status.success(), "slopd-acp exited with {status}");
                    return;
                }
                None if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                None => {
                    kill_child(child);
                    panic!("slopd-acp did not exit after stdin closed");
                }
            }
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            kill_child(child);
        }
    }
}

struct Daemon(Option<Child>);

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(child) = self.0.take() {
            kill_slopd(child);
        }
    }
}

fn initialize(harness: &mut Harness) {
    let mut notifications = Vec::new();
    harness.send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": 2,
            "clientCapabilities": {},
            "clientInfo": {
                "name": "buzz-acp",
                "version": "test"
            }
        },
    }));
    let initialized = harness.response(1, &mut notifications);
    assert_eq!(initialized["result"]["protocolVersion"], 2);
    assert_eq!(initialized["result"]["agentInfo"]["name"], "slopd-acp");
    assert_eq!(
        initialized["result"]["agentCapabilities"]["loadSession"],
        false
    );
    assert!(
        initialized["result"]["agentCapabilities"]["sessionCapabilities"]["resume"].is_object()
    );
    assert!(initialized["result"]["agentCapabilities"]["sessionCapabilities"]["list"].is_object());
    assert!(
        initialized["result"]["agentCapabilities"]["sessionCapabilities"]["delete"].is_object()
    );
    assert!(initialized["result"]["agentCapabilities"]["sessionCapabilities"]["close"].is_object());
}

fn new_session(harness: &mut Harness, cwd: &std::path::Path, system_prompt: &str) -> String {
    let mut notifications = Vec::new();
    harness.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": {
            "cwd": cwd,
            "mcpServers": [],
            "systemPrompt": system_prompt,
            "_meta": {
                "sessionTitle": "slopd ACP integration test"
            }
        },
    }));
    let created = harness.response(2, &mut notifications);
    let session_id = created["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    let opaque_id = session_id.strip_prefix("slopd:").expect("slopd session id");
    uuid::Uuid::parse_str(opaque_id).expect("new ACP sessions should use durable UUID IDs");
    session_id
}

fn prompt(
    harness: &mut Harness,
    request_id: u64,
    session_id: &str,
    text: &str,
) -> (Value, Vec<Value>) {
    let mut notifications = Vec::new();
    harness.send(json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": text }],
        },
    }));
    let response = harness.response(request_id, &mut notifications);
    (response, notifications)
}

fn resume_session(
    harness: &mut Harness,
    request_id: u64,
    session_id: &str,
    cwd: &std::path::Path,
) -> Value {
    let mut notifications = Vec::new();
    harness.send(json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "session/resume",
        "params": {
            "sessionId": session_id,
            "cwd": cwd,
            "mcpServers": [],
        },
    }));
    harness.response(request_id, &mut notifications)
}

fn list_sessions(harness: &mut Harness, request_id: u64) -> Vec<Value> {
    let mut notifications = Vec::new();
    harness.send(json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "session/list",
        "params": {},
    }));
    harness
        .response(request_id, &mut notifications)
        .pointer("/result/sessions")
        .and_then(Value::as_array)
        .expect("session/list result")
        .clone()
}

fn close_session(harness: &mut Harness, request_id: u64, session_id: &str) -> Value {
    let mut notifications = Vec::new();
    harness.send(json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "session/close",
        "params": { "sessionId": session_id },
    }));
    harness.response(request_id, &mut notifications)
}

fn delete_session(harness: &mut Harness, request_id: u64, session_id: &str) -> Value {
    let mut notifications = Vec::new();
    harness.send(json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "session/delete",
        "params": { "sessionId": session_id },
    }));
    harness.response(request_id, &mut notifications)
}

fn streamed_text(notifications: &[Value]) -> String {
    notifications
        .iter()
        .filter(|message| {
            message
                .pointer("/params/update/sessionUpdate")
                .and_then(Value::as_str)
                == Some("agent_message_chunk")
        })
        .filter_map(|message| {
            message
                .pointer("/params/update/content/text")
                .and_then(Value::as_str)
        })
        .collect()
}

fn session_pane_id(env: &TestEnv, session_id: &str) -> String {
    let encoded = session_id
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let tag = format!("acp-session-{encoded}");
    panes(env)
        .into_iter()
        .find(|pane| pane.tags.iter().any(|candidate| candidate == &tag))
        .map(|pane| pane.pane_id)
        .unwrap_or_else(|| panic!("no live pane carries the durable session tag {tag}"))
}

fn panes(env: &TestEnv) -> Vec<libslop::PaneInfo> {
    serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap()
}

#[test]
fn local_acp_session_streams_a_turn_and_cancels_the_next_one() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");
    build_bin("slopd-acp");

    let mock = cargo_bin("mock_claude");
    let slopctl = cargo_bin("slopctl");
    let claude_config = libsloptest::tempfile::tempdir().unwrap();
    let claude_config_path = claude_config.path().to_path_buf();
    let Some(env) = TestEnv::new_full(
        Some(&[mock.to_str().unwrap()]),
        Some(slopctl.to_str().unwrap()),
        Some(&claude_config_path),
    ) else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    let _daemon = Daemon(Some(env.spawn_slopd()));
    let mut harness = Harness::spawn(&env.socket_path(), &[]);
    initialize(&mut harness);
    let session_id = new_session(&mut harness, env.config_dir.path(), "SYSTEM_CANARY");

    let (completed, mut notifications) = prompt(&mut harness, 3, &session_id, "USER_CANARY");
    assert_eq!(completed["result"]["stopReason"], "end_turn");
    let streamed = streamed_text(&notifications);
    assert!(streamed.contains("SYSTEM_CANARY"), "streamed: {streamed}");
    assert!(streamed.contains("USER_CANARY"), "streamed: {streamed}");
    assert!(notifications.iter().any(|message| {
        message
            .pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("agent_message_chunk")
            && message
                .pointer("/params/update/messageId")
                .and_then(Value::as_str)
                .is_some()
    }));

    notifications.clear();
    harness.send(json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": "::mock busy 10s" }],
        },
    }));
    // mock_claude writes its assistant record before entering the simulated
    // long-running tool, proving the prompt was accepted before cancellation.
    let first_update = harness.receive();
    assert_eq!(first_update["method"], "session/update");
    harness.send(json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": { "sessionId": session_id },
    }));
    let cancelled = harness.response(4, &mut notifications);
    assert_eq!(cancelled["result"]["stopReason"], "cancelled");

    drop(harness);
}

#[test]
fn codex_acp_session_streams_a_complete_turn() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_codex");
    build_bin("slopd-acp");

    let mock = cargo_bin("mock_codex");
    let slopctl = cargo_bin("slopctl");
    let codex_home = libsloptest::tempfile::tempdir().unwrap();
    let Some(env) = TestEnv::new_full(None, Some(slopctl.to_str().unwrap()), None) else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    env.append_config(&format!(
        "\n[accounts.acp-codex]\nbackend = \"codex\"\nexecutable = {:?}\nconfig_dir = {:?}\n",
        mock.to_str().unwrap(),
        codex_home.path().to_str().unwrap(),
    ));

    let _daemon = Daemon(Some(env.spawn_slopd()));
    let mut harness = Harness::spawn(&env.socket_path(), &["--account", "acp-codex"]);
    initialize(&mut harness);
    let session_id = new_session(&mut harness, env.config_dir.path(), "CODEX_SYSTEM_CANARY");
    let (completed, notifications) = prompt(&mut harness, 3, &session_id, "CODEX_USER_CANARY");

    assert_eq!(completed["result"]["stopReason"], "end_turn");
    let streamed = streamed_text(&notifications);
    assert!(
        streamed.contains("mock response:"),
        "Codex assistant response was not streamed: {streamed}"
    );
    assert!(
        streamed.contains("CODEX_SYSTEM_CANARY"),
        "Codex system prompt was not delivered: {streamed}"
    );
    assert!(
        streamed.contains("CODEX_USER_CANARY"),
        "Codex user prompt was not delivered: {streamed}"
    );
}

#[test]
fn buzz_native_steer_reuses_the_existing_codex_pane() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_codex");
    build_bin("slopd-acp");

    let mock = cargo_bin("mock_codex");
    let slopctl = cargo_bin("slopctl");
    let codex_home = libsloptest::tempfile::tempdir().unwrap();
    let Some(env) = TestEnv::new_full(None, Some(slopctl.to_str().unwrap()), None) else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    env.append_config(&format!(
        "\n[accounts.acp-codex]\nbackend = \"codex\"\nexecutable = {:?}\nconfig_dir = {:?}\n",
        mock.to_str().unwrap(),
        codex_home.path().to_str().unwrap(),
    ));

    let _daemon = Daemon(Some(env.spawn_slopd()));
    let mut harness = Harness::spawn(&env.socket_path(), &["--account", "acp-codex"]);
    initialize(&mut harness);
    let session_id = new_session(&mut harness, env.config_dir.path(), "");
    let pane_id = session_pane_id(&env, &session_id);

    harness.send(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": "::mock active" }],
        },
    }));

    let mut messages = Vec::new();
    let active_run_id = loop {
        let message = harness.receive();
        assert_ne!(
            message.get("id").and_then(Value::as_u64),
            Some(3),
            "the active prompt completed before Buzz could steer it: {message}"
        );
        if let Some(run_id) = message
            .pointer("/params/update/_meta/goose/activeRunId")
            .and_then(Value::as_str)
            .map(str::to_owned)
        {
            messages.push(message);
            break run_id;
        }
        messages.push(message);
    };
    assert!(active_run_id.starts_with("slopd-turn-"));

    // This is the exact extension request Buzz sends after observing
    // `_meta.goose.activeRunId`. slopd-acp must route it through slopd's
    // ordinary send path, which steers a busy Codex pane without cancellation.
    harness.send(json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "_goose/unstable/session/steer",
        "params": {
            "sessionId": session_id,
            "expectedRunId": active_run_id,
            "prompt": [{ "type": "text", "text": "BUZZ_STEER_CANARY" }],
        },
    }));

    let mut prompt_response = None;
    let mut steer_response = None;
    let mut active_run_cleared = false;
    while prompt_response.is_none() || steer_response.is_none() || !active_run_cleared {
        let message = harness.receive();
        match message.get("id").and_then(Value::as_u64) {
            Some(3) => prompt_response = Some(message),
            Some(4) => steer_response = Some(message),
            _ => {
                active_run_cleared |= message
                    .pointer("/params/update/_meta/goose/activeRunId")
                    .is_some_and(Value::is_null);
                messages.push(message);
            }
        }
    }

    let steer_response = steer_response.unwrap();
    assert!(
        steer_response.get("error").is_none(),
        "Buzz would fall back to cancel-and-reprompt after this response: {steer_response}"
    );
    assert!(steer_response.get("result").is_some());
    assert_eq!(prompt_response.unwrap()["result"]["stopReason"], "end_turn");
    let streamed = streamed_text(&messages);
    assert!(
        streamed.contains("steered: BUZZ_STEER_CANARY"),
        "steered Codex response was not streamed through the original ACP turn: {streamed}"
    );

    let panes: Vec<libslop::PaneInfo> =
        serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap();
    assert_eq!(panes.len(), 1, "steering must not create another pane");
    assert_eq!(panes[0].pane_id, pane_id);
}

#[test]
fn session_limit_evicts_and_lazily_restores_lru_inactive_panes() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_codex");
    build_bin("slopd-acp");

    let mock = cargo_bin("mock_codex");
    let slopctl = cargo_bin("slopctl");
    let codex_home = libsloptest::tempfile::tempdir().unwrap();
    let Some(env) = TestEnv::new_full(None, Some(slopctl.to_str().unwrap()), None) else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    env.append_config(&format!(
        "\n[accounts.acp-codex]\nbackend = \"codex\"\nexecutable = {:?}\nconfig_dir = {:?}\n",
        mock.to_str().unwrap(),
        codex_home.path().to_str().unwrap(),
    ));

    let _daemon = Daemon(Some(env.spawn_slopd()));
    let mut harness = Harness::spawn(
        &env.socket_path(),
        &["--account", "acp-codex", "--max-sessions", "2"],
    );
    initialize(&mut harness);
    let first = new_session(&mut harness, env.config_dir.path(), "");
    let second = new_session(
        &mut harness,
        env.config_dir.path(),
        "RESUMED_SYSTEM_PROMPT_CANARY",
    );
    let first_pane = session_pane_id(&env, &first);
    let second_pane = session_pane_id(&env, &second);

    // Give the second pane conversation state worth resuming, including a
    // system prompt that must not be injected again after native resume.
    let (second_turn, _) = prompt(&mut harness, 9, &second, "SECOND_CONTEXT_CANARY");
    assert_eq!(second_turn["result"]["stopReason"], "end_turn");
    let second_native_session = panes(&env)
        .into_iter()
        .find(|pane| pane.pane_id == second_pane)
        .and_then(|pane| pane.session_id)
        .expect("second pane should expose its backend-native session id");

    // Make the first session newer than the second, so the second is the LRU
    // eviction victim when a third resident pane is requested.
    let (first_turn, _) = prompt(&mut harness, 10, &first, "KEEP_FIRST_RECENT");
    assert_eq!(first_turn["result"]["stopReason"], "end_turn");

    let third = new_session(&mut harness, env.config_dir.path(), "");
    let third_pane = session_pane_id(&env, &third);
    let resident = panes(&env);
    assert_eq!(resident.len(), 2);
    assert!(resident.iter().any(|pane| pane.pane_id == first_pane));
    assert!(resident.iter().any(|pane| pane.pane_id == third_pane));
    assert!(
        resident.iter().all(|pane| pane.pane_id != second_pane),
        "the least-recently-used pane was not evicted: {resident:?}"
    );

    // Buzz still holds the second logical ACP session ID. Reusing it must
    // restore a pane transparently instead of returning "unknown session".
    let (restored, notifications) = prompt(&mut harness, 11, &second, "RESTORED_SESSION_CANARY");
    assert_eq!(restored["result"]["stopReason"], "end_turn");
    assert!(
        streamed_text(&notifications).contains("RESTORED_SESSION_CANARY"),
        "restored session did not run its prompt: {notifications:?}"
    );
    assert!(
        !streamed_text(&notifications).contains("RESUMED_SYSTEM_PROMPT_CANARY"),
        "native resume re-injected a system prompt already present in context: {notifications:?}"
    );

    let resident = panes(&env);
    assert_eq!(resident.len(), 2, "live pane limit must remain enforced");
    assert!(resident.iter().any(|pane| pane.pane_id == third_pane));
    assert!(resident.iter().all(|pane| pane.pane_id != first_pane));
    assert!(resident.iter().all(|pane| pane.pane_id != second_pane));
    let restored_pane = resident
        .iter()
        .find(|pane| pane.session_id.as_deref() == Some(second_native_session.as_str()))
        .expect("restored pane should resume the original backend-native session");
    assert_ne!(restored_pane.pane_id, second_pane);
}

#[test]
fn evicted_session_without_native_context_restarts_fresh() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_codex");
    build_bin("slopd-acp");

    let mock = cargo_bin("mock_codex");
    let slopctl = cargo_bin("slopctl");
    let codex_home = libsloptest::tempfile::tempdir().unwrap();
    let Some(env) = TestEnv::new_full(None, Some(slopctl.to_str().unwrap()), None) else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    env.append_config(&format!(
        "\n[accounts.acp-codex]\nbackend = \"codex\"\nexecutable = {:?}\nconfig_dir = {:?}\n",
        mock.to_str().unwrap(),
        codex_home.path().to_str().unwrap(),
    ));

    let _daemon = Daemon(Some(env.spawn_slopd()));
    let mut harness = Harness::spawn(
        &env.socket_path(),
        &[
            "--account",
            "acp-codex",
            "--max-sessions",
            "1",
            "--agent-arg",
            "--mock-session-start=lazy",
        ],
    );
    initialize(&mut harness);
    let empty = new_session(
        &mut harness,
        env.config_dir.path(),
        "FRESH_SYSTEM_PROMPT_CANARY",
    );
    let empty_pane = session_pane_id(&env, &empty);
    assert_eq!(
        panes(&env)
            .into_iter()
            .find(|pane| pane.pane_id == empty_pane)
            .and_then(|pane| pane.session_id),
        None,
        "the test pane unexpectedly created native context before its first prompt"
    );

    let replacement = new_session(&mut harness, env.config_dir.path(), "");
    let replacement_pane = session_pane_id(&env, &replacement);
    assert_eq!(panes(&env).len(), 1);
    assert!(panes(&env).iter().all(|pane| pane.pane_id != empty_pane));

    let (restored, notifications) = prompt(&mut harness, 10, &empty, "FRESH_RESTORE_CANARY");
    assert_eq!(restored["result"]["stopReason"], "end_turn");
    let streamed = streamed_text(&notifications);
    assert!(streamed.contains("FRESH_SYSTEM_PROMPT_CANARY"));
    assert!(streamed.contains("FRESH_RESTORE_CANARY"));

    let resident = panes(&env);
    assert_eq!(resident.len(), 1);
    assert!(resident.iter().all(|pane| pane.pane_id != replacement_pane));
    assert!(
        resident[0].session_id.is_some(),
        "freshly restored pane did not create native context on its first prompt"
    );
}

#[test]
fn session_limit_never_evicts_an_active_pane() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_codex");
    build_bin("slopd-acp");

    let mock = cargo_bin("mock_codex");
    let slopctl = cargo_bin("slopctl");
    let codex_home = libsloptest::tempfile::tempdir().unwrap();
    let Some(env) = TestEnv::new_full(None, Some(slopctl.to_str().unwrap()), None) else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    env.append_config(&format!(
        "\n[accounts.acp-codex]\nbackend = \"codex\"\nexecutable = {:?}\nconfig_dir = {:?}\n",
        mock.to_str().unwrap(),
        codex_home.path().to_str().unwrap(),
    ));

    let _daemon = Daemon(Some(env.spawn_slopd()));
    let mut harness = Harness::spawn(
        &env.socket_path(),
        &["--account", "acp-codex", "--max-sessions", "1"],
    );
    initialize(&mut harness);
    let active = new_session(&mut harness, env.config_dir.path(), "");
    let active_pane = session_pane_id(&env, &active);

    harness.send(json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "session/prompt",
        "params": {
            "sessionId": active,
            "prompt": [{ "type": "text", "text": "::mock active" }],
        },
    }));
    loop {
        let message = harness.receive();
        if message
            .pointer("/params/update/_meta/goose/activeRunId")
            .and_then(Value::as_str)
            .is_some()
        {
            break;
        }
    }

    harness.send(json!({
        "jsonrpc": "2.0",
        "id": 11,
        "method": "session/new",
        "params": {
            "cwd": env.config_dir.path(),
            "mcpServers": [],
        },
    }));
    let mut notifications = Vec::new();
    let rejected = harness.response(11, &mut notifications);
    assert!(
        rejected["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("every pane has an active turn")),
        "active pane should block eviction: {rejected}"
    );

    let resident = panes(&env);
    assert_eq!(resident.len(), 1);
    assert_eq!(resident[0].pane_id, active_pane);
}

#[test]
fn dead_panes_are_pruned_before_limit_eviction() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_codex");
    build_bin("slopd-acp");

    let mock = cargo_bin("mock_codex");
    let slopctl = cargo_bin("slopctl");
    let codex_home = libsloptest::tempfile::tempdir().unwrap();
    let Some(env) = TestEnv::new_full(None, Some(slopctl.to_str().unwrap()), None) else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    env.append_config(&format!(
        "\n[accounts.acp-codex]\nbackend = \"codex\"\nexecutable = {:?}\nconfig_dir = {:?}\n",
        mock.to_str().unwrap(),
        codex_home.path().to_str().unwrap(),
    ));

    let _daemon = Daemon(Some(env.spawn_slopd()));
    let mut harness = Harness::spawn(
        &env.socket_path(),
        &["--account", "acp-codex", "--max-sessions", "1"],
    );
    initialize(&mut harness);
    let dead = new_session(&mut harness, env.config_dir.path(), "");
    let dead_pane = session_pane_id(&env, &dead);
    let killed = env.slopctl(&["kill", &dead_pane]);
    assert!(
        killed.status.success(),
        "failed to kill test pane: {killed:?}"
    );

    let replacement = new_session(&mut harness, env.config_dir.path(), "");
    let resident = panes(&env);
    assert_eq!(resident.len(), 1);
    assert_eq!(resident[0].pane_id, session_pane_id(&env, &replacement));
}

#[test]
fn graceful_adapter_eof_detaches_panes_for_a_replacement_to_resume() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_codex");
    build_bin("slopd-acp");

    let mock = cargo_bin("mock_codex");
    let slopctl = cargo_bin("slopctl");
    let codex_home = libsloptest::tempfile::tempdir().unwrap();
    let Some(env) = TestEnv::new_full(None, Some(slopctl.to_str().unwrap()), None) else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    env.append_config(&format!(
        "\n[accounts.acp-codex]\nbackend = \"codex\"\nexecutable = {:?}\nconfig_dir = {:?}\n",
        mock.to_str().unwrap(),
        codex_home.path().to_str().unwrap(),
    ));

    let _daemon = Daemon(Some(env.spawn_slopd()));
    let mut harness = Harness::spawn(&env.socket_path(), &["--account", "acp-codex"]);
    initialize(&mut harness);
    let session_id = new_session(&mut harness, env.config_dir.path(), "");
    let (completed, _) = prompt(&mut harness, 3, &session_id, "PERSIST_EOF_CANARY");
    assert_eq!(completed["result"]["stopReason"], "end_turn");
    let pane_id = session_pane_id(&env, &session_id);
    assert_eq!(panes(&env).len(), 1);

    harness.close_stdin_and_wait();
    assert_eq!(
        panes(&env).len(),
        1,
        "graceful adapter shutdown should detach its resumable pane"
    );

    let mut replacement = Harness::spawn(&env.socket_path(), &["--account", "acp-codex"]);
    initialize(&mut replacement);
    assert!(
        list_sessions(&mut replacement, 4)
            .iter()
            .any(|session| { session["sessionId"].as_str() == Some(session_id.as_str()) })
    );
    let resumed = resume_session(&mut replacement, 5, &session_id, env.config_dir.path());
    assert!(resumed.get("error").is_none(), "resume failed: {resumed}");
    let resident = panes(&env);
    assert_eq!(resident.len(), 1);
    assert_eq!(
        resident[0].pane_id, pane_id,
        "resume should adopt the live pane"
    );
}

#[test]
fn replacement_recovers_a_session_after_abrupt_adapter_exit() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_codex");
    build_bin("slopd-acp");

    let mock = cargo_bin("mock_codex");
    let slopctl = cargo_bin("slopctl");
    let codex_home = libsloptest::tempfile::tempdir().unwrap();
    let Some(env) = TestEnv::new_full(None, Some(slopctl.to_str().unwrap()), None) else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    env.append_config(&format!(
        "\n[accounts.acp-codex]\nbackend = \"codex\"\nexecutable = {:?}\nconfig_dir = {:?}\n",
        mock.to_str().unwrap(),
        codex_home.path().to_str().unwrap(),
    ));

    let _daemon = Daemon(Some(env.spawn_slopd()));
    let mut original = Harness::spawn(&env.socket_path(), &["--account", "acp-codex"]);
    initialize(&mut original);
    let session_id = new_session(&mut original, env.config_dir.path(), "");
    let (completed, _) = prompt(&mut original, 3, &session_id, "CRASH_RECOVERY_CANARY");
    assert_eq!(completed["result"]["stopReason"], "end_turn");
    let pane_id = session_pane_id(&env, &session_id);

    // Harness::drop terminates the adapter without closing stdin, reproducing
    // the service-level signal race that originally orphaned panes.
    drop(original);
    assert_eq!(panes(&env).len(), 1);

    let mut replacement = Harness::spawn(&env.socket_path(), &["--account", "acp-codex"]);
    initialize(&mut replacement);
    let listed = list_sessions(&mut replacement, 4);
    assert!(
        listed
            .iter()
            .any(|session| { session["sessionId"].as_str() == Some(session_id.as_str()) })
    );
    let resumed = resume_session(&mut replacement, 5, &session_id, env.config_dir.path());
    assert!(resumed.get("error").is_none(), "resume failed: {resumed}");
    assert_eq!(panes(&env)[0].pane_id, pane_id);

    let (continued, notifications) = prompt(
        &mut replacement,
        6,
        &session_id,
        "AFTER_ADAPTER_RESTART_CANARY",
    );
    assert_eq!(continued["result"]["stopReason"], "end_turn");
    assert!(streamed_text(&notifications).contains("AFTER_ADAPTER_RESTART_CANARY"));
}

#[test]
fn closed_session_is_listed_and_revived_from_the_graveyard() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_codex");
    build_bin("slopd-acp");

    let mock = cargo_bin("mock_codex");
    let slopctl = cargo_bin("slopctl");
    let codex_home = libsloptest::tempfile::tempdir().unwrap();
    let Some(env) = TestEnv::new_full(None, Some(slopctl.to_str().unwrap()), None) else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    env.append_config(&format!(
        "\n[accounts.acp-codex]\nbackend = \"codex\"\nexecutable = {:?}\nconfig_dir = {:?}\n",
        mock.to_str().unwrap(),
        codex_home.path().to_str().unwrap(),
    ));

    let _daemon = Daemon(Some(env.spawn_slopd()));
    let mut harness = Harness::spawn(&env.socket_path(), &["--account", "acp-codex"]);
    initialize(&mut harness);
    let session_id = new_session(&mut harness, env.config_dir.path(), "");
    let original_pane = session_pane_id(&env, &session_id);
    let (completed, _) = prompt(&mut harness, 3, &session_id, "GRAVEYARD_CONTEXT_CANARY");
    assert_eq!(completed["result"]["stopReason"], "end_turn");
    let native_session = panes(&env)[0]
        .session_id
        .clone()
        .expect("native session id");

    let closed = close_session(&mut harness, 4, &session_id);
    assert!(closed.get("error").is_none(), "close failed: {closed}");
    assert!(
        panes(&env).is_empty(),
        "session/close must free its live pane"
    );
    assert!(
        list_sessions(&mut harness, 5)
            .iter()
            .any(|session| { session["sessionId"].as_str() == Some(session_id.as_str()) })
    );
    drop(harness);

    let mut replacement = Harness::spawn(&env.socket_path(), &["--account", "acp-codex"]);
    initialize(&mut replacement);
    assert!(
        list_sessions(&mut replacement, 6)
            .iter()
            .any(|session| { session["sessionId"].as_str() == Some(session_id.as_str()) }),
        "a replacement adapter did not reconstruct the closed logical session"
    );

    let resumed = resume_session(&mut replacement, 7, &session_id, env.config_dir.path());
    assert!(resumed.get("error").is_none(), "resume failed: {resumed}");
    let revived = panes(&env);
    assert_eq!(revived.len(), 1);
    assert_ne!(revived[0].pane_id, original_pane);
    assert_eq!(
        revived[0].session_id.as_deref(),
        Some(native_session.as_str())
    );

    let (continued, notifications) = prompt(
        &mut replacement,
        8,
        &session_id,
        "AFTER_GRAVEYARD_REVIVE_CANARY",
    );
    assert_eq!(continued["result"]["stopReason"], "end_turn");
    assert!(streamed_text(&notifications).contains("AFTER_GRAVEYARD_REVIVE_CANARY"));

    let deleted = delete_session(&mut replacement, 9, &session_id);
    assert!(deleted.get("error").is_none(), "delete failed: {deleted}");
    assert!(panes(&env).is_empty());
    assert!(
        list_sessions(&mut replacement, 10)
            .iter()
            .all(|session| { session["sessionId"].as_str() != Some(session_id.as_str()) })
    );
    drop(replacement);

    let mut restarted = Harness::spawn(&env.socket_path(), &["--account", "acp-codex"]);
    initialize(&mut restarted);
    assert!(
        list_sessions(&mut restarted, 11)
            .iter()
            .all(|session| { session["sessionId"].as_str() != Some(session_id.as_str()) })
    );
    let deleted_again = delete_session(&mut restarted, 12, &session_id);
    assert!(
        deleted_again.get("error").is_none(),
        "idempotent delete failed: {deleted_again}"
    );
}

#[test]
fn replacement_trims_recovered_live_panes_but_keeps_their_sessions() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_codex");
    build_bin("slopd-acp");

    let mock = cargo_bin("mock_codex");
    let slopctl = cargo_bin("slopctl");
    let codex_home = libsloptest::tempfile::tempdir().unwrap();
    let Some(env) = TestEnv::new_full(None, Some(slopctl.to_str().unwrap()), None) else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    env.append_config(&format!(
        "\n[accounts.acp-codex]\nbackend = \"codex\"\nexecutable = {:?}\nconfig_dir = {:?}\n",
        mock.to_str().unwrap(),
        codex_home.path().to_str().unwrap(),
    ));

    let _daemon = Daemon(Some(env.spawn_slopd()));
    let mut original = Harness::spawn(
        &env.socket_path(),
        &["--account", "acp-codex", "--max-sessions", "3"],
    );
    initialize(&mut original);
    let mut session_ids = Vec::new();
    for request_id in 10..13 {
        let session_id = new_session(&mut original, env.config_dir.path(), "");
        let (completed, _) = prompt(
            &mut original,
            request_id,
            &session_id,
            &format!("RECOVERED_LIMIT_CANARY_{request_id}"),
        );
        assert_eq!(completed["result"]["stopReason"], "end_turn");
        session_ids.push(session_id);
    }
    assert_eq!(panes(&env).len(), 3);
    drop(original);

    let mut replacement = Harness::spawn(
        &env.socket_path(),
        &["--account", "acp-codex", "--max-sessions", "2"],
    );
    initialize(&mut replacement);
    assert_eq!(
        panes(&env).len(),
        2,
        "startup recovery must enforce the live pane limit"
    );
    let listed = list_sessions(&mut replacement, 20);
    assert_eq!(
        listed.len(),
        3,
        "eviction must preserve logical ACP sessions"
    );
    for session_id in session_ids {
        assert!(
            listed
                .iter()
                .any(|session| { session["sessionId"].as_str() == Some(session_id.as_str()) })
        );
    }
}

#[test]
fn opencode_acp_session_streams_only_the_assistant_turn() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_opencode");
    build_bin("slopd-acp");

    let mock = cargo_bin("mock_opencode");
    let slopctl = cargo_bin("slopctl");
    let opencode_home = libsloptest::tempfile::tempdir().unwrap();
    let Some(env) = TestEnv::new_full(None, Some(slopctl.to_str().unwrap()), None) else {
        eprintln!("skipping: tmux is unavailable");
        return;
    };
    env.append_config(&format!(
        "\n[accounts.acp-opencode]\nbackend = \"opencode\"\nexecutable = {:?}\nconfig_dir = {:?}\n",
        mock.to_str().unwrap(),
        opencode_home.path().to_str().unwrap(),
    ));

    let _daemon = Daemon(Some(env.spawn_slopd()));
    let mut harness = Harness::spawn(&env.socket_path(), &["--account", "acp-opencode"]);
    initialize(&mut harness);
    let session_id = new_session(
        &mut harness,
        env.config_dir.path(),
        "OPENCODE_SYSTEM_CANARY",
    );
    let (completed, notifications) = prompt(&mut harness, 3, &session_id, "OPENCODE_USER_CANARY");

    assert_eq!(completed["result"]["stopReason"], "end_turn");
    let streamed = streamed_text(&notifications);
    assert!(
        streamed.starts_with("echo:"),
        "OpenCode user content leaked into the agent stream: {streamed}"
    );
    assert!(
        streamed.contains("OPENCODE_SYSTEM_CANARY"),
        "OpenCode system prompt was not delivered: {streamed}"
    );
    assert!(
        streamed.contains("OPENCODE_USER_CANARY"),
        "OpenCode user prompt was not delivered: {streamed}"
    );
}
