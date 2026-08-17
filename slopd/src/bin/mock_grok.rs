//! Deterministic Grok Build TUI + leader/ACP double used by integration tests.

use serde_json::{Value, json};
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod mock_support;
use mock_support::{
    MockCommand, SubagentMode, parse as parse_mock_command, reject_unknown_mock_option,
};

fn counter() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn grok_home() -> PathBuf {
    PathBuf::from(
        std::env::var_os("GROK_HOME")
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".grok").into())
            })
            .unwrap_or_else(|| ".grok".into()),
    )
}

fn argument_value(args: &[String], names: &[&str]) -> Option<String> {
    let mut args = args.iter();
    while let Some(argument) = args.next() {
        if names.contains(&argument.as_str()) {
            return args.next().cloned();
        }
        for name in names {
            if let Some(value) = argument.strip_prefix(&format!("{name}=")) {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn session_id(args: &[String]) -> String {
    if let Some(id) = argument_value(args, &["--session-id", "-s"]) {
        return id;
    }
    if !args.iter().any(|argument| argument == "--fork-session")
        && let Some(id) = argument_value(args, &["--resume", "-r"])
    {
        return id;
    }
    uuid::Uuid::new_v4().to_string()
}

fn leader_socket(args: &[String]) -> PathBuf {
    argument_value(args, &["--leader-socket"])
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("GROK_LEADER_SOCKET").map(PathBuf::from))
        .unwrap_or_else(|| grok_home().join("leader.sock"))
}

fn leader_enabled(args: &[String]) -> bool {
    let sandbox =
        argument_value(args, &["--sandbox"]).or_else(|| std::env::var("GROK_SANDBOX").ok());
    !args.iter().any(|argument| argument == "--no-leader")
        && sandbox.as_deref().is_none_or(|profile| profile == "off")
}

fn command_path(socket: &Path) -> PathBuf {
    PathBuf::from(format!("{}.mock-commands", socket.display()))
}

fn transcript_path(home: &Path, session_id: &str) -> PathBuf {
    home.join("sessions")
        .join("mock")
        .join(session_id)
        .join("updates.jsonl")
}

fn load_hooks(home: &Path) -> Value {
    std::fs::read_to_string(home.join("hooks").join("slopd.json"))
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_else(|| json!({}))
}

fn fire_hooks(settings: &Value, event: &str, payload: &Value) {
    let Some(entries) = settings
        .pointer(&format!("/hooks/{event}"))
        .and_then(Value::as_array)
    else {
        return;
    };
    for entry in entries {
        let Some(hooks) = entry.get("hooks").and_then(Value::as_array) else {
            continue;
        };
        for hook in hooks {
            let Some(command) = hook.get("command").and_then(Value::as_str) else {
                continue;
            };
            let Ok(mut child) = Command::new("sh")
                .args(["-c", command])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            else {
                continue;
            };
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(payload.to_string().as_bytes());
            }
            let _ = child.wait();
        }
    }
}

fn hook_payload(event: &str, session_id: &str, cwd: &Path, transcript: &Path) -> Value {
    json!({
        "hookEventName": event,
        "sessionId": session_id,
        "cwd": cwd,
        "workspaceRoot": cwd,
        "timestamp": counter(),
        "transcriptPath": transcript,
        "clientIdentifier": "mock-grok",
        "promptId": format!("prompt-{}", counter()),
        "permissionMode": "default",
    })
}

fn append_json(path: &Path, value: &Value) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create mock Grok state directory");
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open mock Grok JSONL");
    writeln!(file, "{value}").expect("append mock Grok JSONL");
}

fn update(session_id: &str, kind: &str, fields: Value) -> Value {
    let mut update = json!({
        "sessionUpdate": kind,
        "_meta": {"eventId": format!("mock-event-{}", counter())},
    });
    if let (Some(update), Some(fields)) = (update.as_object_mut(), fields.as_object()) {
        update.extend(fields.clone());
    }
    json!({
        "timestamp": counter(),
        "method": "session/update",
        "params": {"sessionId": session_id, "update": update},
    })
}

fn xai_update(session_id: &str, kind: &str, fields: Value) -> Value {
    let mut envelope = update(session_id, kind, fields);
    envelope["method"] = json!("_x.ai/session/update");
    envelope
}

fn write_prompt(transcript: &Path, session_id: &str, prompt: &str, prompt_index: u64) {
    append_json(
        transcript,
        &update(
            session_id,
            "user_message_chunk",
            json!({
                "content": {"type": "text", "text": prompt},
                "_meta": {
                    "eventId": format!("mock-event-{}", counter()),
                    "promptIndex": prompt_index,
                },
            }),
        ),
    );
}

fn write_response(transcript: &Path, session_id: &str, response: &str) {
    append_json(
        transcript,
        &update(
            session_id,
            "agent_message_chunk",
            json!({"content": {"type": "text", "text": response}}),
        ),
    );
}

fn append_command(path: &Path, value: Value) {
    append_json(path, &value);
}

fn emit_line(stdout: &Arc<Mutex<std::io::Stdout>>, value: &Value) {
    let mut stdout = stdout.lock().unwrap();
    writeln!(stdout, "{value}").expect("write mock Grok ACP frame");
    stdout.flush().expect("flush mock Grok ACP frame");
}

fn replay_transcript(stdout: &Arc<Mutex<std::io::Stdout>>, transcript: &Path) {
    let Ok(contents) = std::fs::read_to_string(transcript) else {
        return;
    };
    for line in contents.lines() {
        let Ok(mut envelope) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        envelope["jsonrpc"] = json!("2.0");
        if let Some(update) = envelope.pointer_mut("/params/update") {
            if !update.get("_meta").is_some_and(Value::is_object) {
                update["_meta"] = json!({});
            }
            update["_meta"]["isReplay"] = json!(true);
        }
        emit_line(stdout, &envelope);
    }
}

fn stream_turn(
    stdout: Arc<Mutex<std::io::Stdout>>,
    transcript: PathBuf,
    request_id: Value,
    starting_offset: u64,
) {
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut offset = starting_offset;
        let mut finished = false;
        while std::time::Instant::now() < deadline && !finished {
            if let Ok(mut file) = std::fs::File::open(&transcript) {
                let _ = file.seek(SeekFrom::Start(offset));
                let mut reader = std::io::BufReader::new(file);
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) != 0 {
                    offset += line.len() as u64;
                    if let Ok(mut envelope) = serde_json::from_str::<Value>(line.trim()) {
                        if envelope
                            .pointer("/params/update/sessionUpdate")
                            .and_then(Value::as_str)
                            == Some("mock_reverse_ready")
                        {
                            emit_line(
                                &stdout,
                                &json!({
                                    "jsonrpc": "2.0",
                                    "id": counter(),
                                    "method": "_x.ai/ask_user_question",
                                    "params": {
                                        "method": "x.ai/ask_user_question",
                                        "params": {
                                            "sessionId": envelope.pointer("/params/sessionId"),
                                            "toolCallId": "mock-question",
                                            "question": "Mock question?",
                                        },
                                    },
                                }),
                            );
                        }
                        finished = envelope
                            .pointer("/params/update/sessionUpdate")
                            .and_then(Value::as_str)
                            == Some("agent_message_chunk");
                        envelope["jsonrpc"] = json!("2.0");
                        emit_line(&stdout, &envelope);
                    }
                    line.clear();
                }
            }
            if !finished {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        if finished {
            emit_line(
                &stdout,
                &json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": {"stopReason": "end_turn"},
                }),
            );
        }
    });
}

fn agent_main(args: &[String]) {
    let home = grok_home();
    let socket = leader_socket(args);
    let commands = command_path(&socket);
    let _ = std::fs::write(format!("{}.mock-sidecar", socket.display()), b"attached\n");
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let stdin = std::io::stdin();
    let mut session: Option<String> = None;
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = message.get("id").cloned();
        match message.get("method").and_then(Value::as_str) {
            Some("initialize") => {
                emit_line(
                    &stdout,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": 1,
                            "agentCapabilities": {"loadSession": true},
                            "agentInfo": {"name": "mock-grok", "version": "1.0.5"},
                        },
                    }),
                );
            }
            Some("session/load") => {
                let session_id = message
                    .pointer("/params/sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                append_command(
                    &commands,
                    json!({
                        "type": "load",
                        "cwd": message.pointer("/params/cwd"),
                    }),
                );
                session = Some(session_id.clone());
                emit_line(
                    &stdout,
                    &json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":session_id}}),
                );
                replay_transcript(&stdout, &transcript_path(&home, &session_id));
            }
            Some("session/prompt") => {
                let Some(session_id) = session.clone() else {
                    continue;
                };
                let prompt = message
                    .pointer("/params/prompt/0/text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let transcript = transcript_path(&home, &session_id);
                let offset = std::fs::metadata(&transcript).map(|m| m.len()).unwrap_or(0);
                append_command(&commands, json!({"type":"prompt","text":prompt}));
                if let Some(id) = id {
                    stream_turn(stdout.clone(), transcript, id, offset);
                }
            }
            Some("session/cancel") => {
                append_command(&commands, json!({"type":"cancel"}));
            }
            _ => {}
        }
    }
}

struct TuiState {
    active: bool,
    awaiting_permission: bool,
    prompt_index: u64,
}

fn handle_prompt(
    prompt: &str,
    state: &mut TuiState,
    settings: &Value,
    session_id: &str,
    cwd: &Path,
    transcript: &Path,
) {
    let mut submitted = hook_payload("UserPromptSubmit", session_id, cwd, transcript);
    submitted["prompt"] = json!(prompt);
    fire_hooks(settings, "UserPromptSubmit", &submitted);
    write_prompt(transcript, session_id, prompt, state.prompt_index);
    state.prompt_index += 1;

    let command = parse_mock_command(prompt).ok().flatten();
    match command {
        Some(MockCommand::Active) => {
            state.active = true;
        }
        Some(MockCommand::Permission(_)) => {
            state.awaiting_permission = true;
            let mut notification = hook_payload("Notification", session_id, cwd, transcript);
            notification["notificationType"] = json!("permission_prompt");
            fire_hooks(settings, "Notification", &notification);
        }
        Some(MockCommand::Tool) => {
            let tool_id = format!("tool-{}", counter());
            let mut pre = hook_payload("PreToolUse", session_id, cwd, transcript);
            pre["toolName"] = json!("run_terminal_command");
            pre["toolUseId"] = json!(tool_id);
            fire_hooks(settings, "PreToolUse", &pre);
            append_json(
                transcript,
                &update(
                    session_id,
                    "tool_call",
                    json!({
                        "toolCallId": tool_id,
                        "title": "run_terminal_command",
                        "kind": "execute",
                        "status": "in_progress",
                        "rawInput": {"command": "cargo test --workspace"},
                    }),
                ),
            );
            std::thread::sleep(Duration::from_millis(250));
            let mut post = hook_payload("PostToolUse", session_id, cwd, transcript);
            post["toolName"] = json!("run_terminal_command");
            post["toolUseId"] = json!(tool_id);
            fire_hooks(settings, "PostToolUse", &post);
            append_json(
                transcript,
                &update(
                    session_id,
                    "tool_call_update",
                    json!({"toolCallId":tool_id,"status":"completed"}),
                ),
            );
            finish_turn(settings, session_id, cwd, transcript, "tool complete");
        }
        Some(MockCommand::Subagent(SubagentMode::Normal)) => {
            let child = format!("child-{}", counter());
            let mut start = hook_payload("SubagentStart", session_id, cwd, transcript);
            start["agentId"] = json!(child);
            fire_hooks(settings, "SubagentStart", &start);
            append_json(
                transcript,
                &xai_update(
                    session_id,
                    "subagent_spawned",
                    json!({"sessionId":child,"agentType":"explore"}),
                ),
            );
            std::thread::sleep(Duration::from_millis(250));
            let mut stop = hook_payload("SubagentStop", session_id, cwd, transcript);
            stop["agentId"] = json!(child);
            fire_hooks(settings, "SubagentStop", &stop);
            append_json(
                transcript,
                &xai_update(session_id, "subagent_finished", json!({"sessionId":child})),
            );
            finish_turn(settings, session_id, cwd, transcript, "subagent complete");
        }
        None if prompt == "::mock compact" => {
            fire_hooks(
                settings,
                "PreCompact",
                &hook_payload("PreCompact", session_id, cwd, transcript),
            );
            std::thread::sleep(Duration::from_millis(250));
            fire_hooks(
                settings,
                "PostCompact",
                &hook_payload("PostCompact", session_id, cwd, transcript),
            );
            finish_turn(settings, session_id, cwd, transcript, "compact complete");
        }
        None if prompt == "::mock elicitation" => {
            append_json(
                transcript,
                &xai_update(session_id, "mock_reverse_ready", json!({})),
            );
            std::thread::sleep(Duration::from_millis(250));
            finish_turn(
                settings,
                session_id,
                cwd,
                transcript,
                "elicitation complete",
            );
        }
        _ => {
            let response = match command {
                Some(MockCommand::Env(key)) => format!(
                    "::mock env {key}={}",
                    std::env::var(key).unwrap_or_else(|_| "UNSET".to_string())
                ),
                Some(MockCommand::Help) => "mock Grok help".to_string(),
                _ if state.active => {
                    state.active = false;
                    format!("steered: {prompt}")
                }
                _ => format!("mock response: {prompt}"),
            };
            finish_turn(settings, session_id, cwd, transcript, &response);
        }
    }
}

fn finish_turn(settings: &Value, session_id: &str, cwd: &Path, transcript: &Path, response: &str) {
    write_response(transcript, session_id, response);
    fire_hooks(
        settings,
        "Stop",
        &hook_payload("Stop", session_id, cwd, transcript),
    );
}

fn cancel_turn(
    state: &mut TuiState,
    settings: &Value,
    session_id: &str,
    cwd: &Path,
    transcript: &Path,
) {
    if !state.active && !state.awaiting_permission {
        return;
    }
    state.active = false;
    state.awaiting_permission = false;
    let mut payload = hook_payload("StopCancelled", session_id, cwd, transcript);
    payload["reason"] = json!("user_interrupt");
    payload["cancelledBy"] = json!("client");
    fire_hooks(settings, "StopCancelled", &payload);
}

fn read_commands(path: &Path, offset: &mut u64) -> Vec<Value> {
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let _ = file.seek(SeekFrom::Start(*offset));
    let mut reader = std::io::BufReader::new(file);
    let mut values = Vec::new();
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap_or(0) != 0 {
        *offset += line.len() as u64;
        if let Ok(value) = serde_json::from_str(line.trim()) {
            values.push(value);
        }
        line.clear();
    }
    values
}

fn tui_main(args: &[String]) {
    let home = grok_home();
    let session_id = session_id(args);
    let socket = leader_socket(args);
    let commands = command_path(&socket);
    if let Some(parent) = commands.parent() {
        std::fs::create_dir_all(parent).expect("create mock leader directory");
    }
    if leader_enabled(args) {
        std::fs::write(&socket, b"mock Grok leader\n").expect("create mock leader socket");
    }
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&commands);
    let transcript = transcript_path(&home, &session_id);
    append_json(
        &transcript,
        &xai_update(
            &session_id,
            "session_started",
            json!({"cwd":std::env::current_dir().ok()}),
        ),
    );
    let settings = load_hooks(&home);
    let cwd = std::env::current_dir().unwrap_or_default();
    fire_hooks(
        &settings,
        "SessionStart",
        &hook_payload("SessionStart", &session_id, &cwd, &transcript),
    );

    let stdin_fd = 0;
    let original = unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        libc::tcgetattr(stdin_fd, &mut termios);
        let original = termios;
        libc::cfmakeraw(&mut termios);
        libc::tcsetattr(stdin_fd, libc::TCSANOW, &termios);
        original
    };
    let original_flags = unsafe { libc::fcntl(stdin_fd, libc::F_GETFL) };
    unsafe {
        libc::fcntl(stdin_fd, libc::F_SETFL, original_flags | libc::O_NONBLOCK);
    }

    let mut state = TuiState {
        active: false,
        awaiting_permission: false,
        prompt_index: 0,
    };
    let mut input = Vec::new();
    let mut command_offset = 0_u64;
    let mut bracketed = false;
    let mut escape = Vec::new();
    let mut running = true;
    while running {
        for command in read_commands(&commands, &mut command_offset) {
            match command.get("type").and_then(Value::as_str) {
                Some("prompt") => handle_prompt(
                    command
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    &mut state,
                    &settings,
                    &session_id,
                    &cwd,
                    &transcript,
                ),
                Some("cancel") => {
                    cancel_turn(&mut state, &settings, &session_id, &cwd, &transcript)
                }
                _ => {}
            }
        }

        let mut bytes = [0_u8; 256];
        match std::io::stdin().read(&mut bytes) {
            Ok(0) => {}
            Ok(count) => {
                for byte in &bytes[..count] {
                    if !escape.is_empty() || *byte == 0x1b {
                        const START: &[u8] = b"\x1b[200~";
                        const END: &[u8] = b"\x1b[201~";
                        escape.push(*byte);
                        if escape == START {
                            escape.clear();
                            bracketed = true;
                        } else if escape == END {
                            escape.clear();
                            bracketed = false;
                        } else if !START.starts_with(&escape) && !END.starts_with(&escape) {
                            escape.clear();
                            cancel_turn(&mut state, &settings, &session_id, &cwd, &transcript);
                        }
                        continue;
                    }
                    match *byte {
                        0x03 | 0x04 => running = false,
                        0x15 => input.clear(),
                        b'\r' | b'\n' if bracketed => input.push(b'\n'),
                        b'\r' | b'\n' => {
                            let prompt = String::from_utf8_lossy(&input).trim().to_string();
                            input.clear();
                            if !prompt.is_empty() {
                                handle_prompt(
                                    &prompt,
                                    &mut state,
                                    &settings,
                                    &session_id,
                                    &cwd,
                                    &transcript,
                                );
                            }
                        }
                        value => input.push(value),
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => running = false,
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    fire_hooks(
        &settings,
        "SessionEnd",
        &hook_payload("SessionEnd", &session_id, &cwd, &transcript),
    );
    unsafe {
        libc::fcntl(stdin_fd, libc::F_SETFL, original_flags);
        libc::tcsetattr(stdin_fd, libc::TCSANOW, &original);
    }
    let _ = std::fs::remove_file(socket);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(argument) = args.iter().find(|argument| argument.starts_with("--mock-")) {
        reject_unknown_mock_option("mock_grok", argument);
    }
    if args.iter().any(|argument| argument == "agent") {
        agent_main(&args);
    } else {
        tui_main(&args);
    }
}
