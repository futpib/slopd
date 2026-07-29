use std::io::{BufRead, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use libsloptest::{TestEnv, build_bin, cargo_bin, kill_child, kill_slopd};
use serde_json::{Value, json};

struct Harness {
    child: Option<Child>,
    stdin: ChildStdin,
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
            stdin,
            receiver,
        }
    }

    fn send(&mut self, message: Value) {
        serde_json::to_writer(&mut self.stdin, &message).unwrap();
        self.stdin.write_all(b"\n").unwrap();
        self.stdin.flush().unwrap();
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
    assert!(session_id.starts_with("slopd:%"));
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

fn streamed_text(notifications: &[Value]) -> String {
    notifications
        .iter()
        .filter_map(|message| {
            message
                .pointer("/params/update/content/text")
                .and_then(Value::as_str)
        })
        .collect()
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

    notifications.clear();
    harness.send(json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": "/busy 10" }],
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
