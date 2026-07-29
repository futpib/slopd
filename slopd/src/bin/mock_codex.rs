//! Deterministic standalone Codex CLI double used by integration tests.
//!
//! It reads `$CODEX_HOME/hooks.json`, emits Codex-shaped rollout JSONL, and
//! runs entirely inside its tmux pane. There is intentionally no app-server.

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

mod mock_support;

use mock_support::{
    CODEX_HELP as MOCK_HELP, MockCommand, parse as parse_mock_command, reject_unknown_mock_option,
};

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
        "session_id": session_id,
        "transcript_path": transcript,
        "cwd": cwd,
        "hook_event_name": event,
        "model": "mock-codex",
        "turn_id": format!("turn-{}", counter()),
    })
}

fn write_record(path: &Path, record: Value) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open mock Codex rollout");
    writeln!(file, "{}", record).expect("write mock Codex rollout");
}

fn response_message(path: &Path, role: &str, text: &str) {
    let text_kind = if role == "user" {
        "input_text"
    } else {
        "output_text"
    };
    write_record(
        path,
        json!({
            "type":"response_item",
            "payload":{
                "type":"message",
                "role":role,
                "content":[{"type":text_kind,"text":text}]
            }
        }),
    );
}

fn counter() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn session_id(args: &[String]) -> String {
    for command in ["resume", "fork"] {
        if let Some(index) = args.iter().position(|arg| arg == command) {
            if command == "resume" {
                if let Some(id) = args.get(index + 1) {
                    return id.clone();
                }
            } else {
                return format!("mock-codex-{}", uuid::Uuid::new_v4());
            }
        }
    }
    format!("mock-codex-{}", uuid::Uuid::new_v4())
}

fn finish_turn(settings: &Value, session_id: &str, cwd: &Path, transcript: &Path, response: &str) {
    response_message(transcript, "assistant", response);
    write_record(
        transcript,
        json!({"type":"event_msg","payload":{"type":"task_complete"}}),
    );
    fire_hooks(
        settings,
        "Stop",
        &hook_payload("Stop", session_id, cwd, transcript),
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    for arg in args.iter().filter(|arg| arg.starts_with("--mock-")) {
        if matches!(
            arg.as_str(),
            "--mock-session-start=lazy" | "--mock-require-bracketed-paste"
        ) || arg
            .strip_prefix("--mock-submit-after=")
            .is_some_and(|value| value.parse::<u8>().is_ok_and(|count| count > 0))
        {
            continue;
        }
        reject_unknown_mock_option("mock_codex", arg);
    }
    let submit_after = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--mock-submit-after="))
        .and_then(|value| value.parse::<u8>().ok())
        .unwrap_or(1);
    let require_bracketed_paste = args
        .iter()
        .any(|arg| arg == "--mock-require-bracketed-paste");
    let codex_home = PathBuf::from(
        std::env::var_os("CODEX_HOME")
            .unwrap_or_else(|| std::env::var_os("HOME").unwrap_or_else(|| ".".into())),
    );
    let settings: Value = std::fs::read_to_string(codex_home.join("hooks.json"))
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_else(|| json!({}));
    let session_id = session_id(&args);
    let cwd = std::env::current_dir().unwrap_or_default();
    let sessions = codex_home.join("sessions");
    std::fs::create_dir_all(&sessions).expect("create mock Codex sessions");
    let transcript = sessions.join(format!("rollout-{session_id}.jsonl"));
    write_record(
        &transcript,
        json!({
            "type":"session_meta",
            "payload":{"id":session_id,"cwd":cwd,"originator":"mock_codex"}
        }),
    );

    // Real interactive Codex creates a fresh session lazily on the first
    // submitted prompt. Tests can request that behavior explicitly while
    // resume/fork and the older eager-path tests retain their startup hook.
    let mut session_started = !args.iter().any(|arg| arg == "--mock-session-start=lazy");
    if session_started {
        fire_hooks(
            &settings,
            "SessionStart",
            &hook_payload("SessionStart", &session_id, &cwd, &transcript),
        );
    }

    let yolo = args
        .iter()
        .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox");
    let mut policy = if yolo {
        json!({"approvalPolicy":"never","sandbox":"danger-full-access"})
    } else {
        json!({"approvalPolicy":"on-request","sandbox":"workspace-write"})
    };

    let stdin_fd = 0;
    let original = unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        libc::tcgetattr(stdin_fd, &mut termios);
        let original = termios;
        libc::cfmakeraw(&mut termios);
        libc::tcsetattr(stdin_fd, libc::TCSANOW, &termios);
        original
    };
    if require_bracketed_paste {
        let mut stdout = std::io::stdout();
        stdout
            .write_all(b"\x1b[?2004h")
            .expect("enable bracketed paste");
        stdout.flush().expect("flush bracketed paste mode");
    }

    let mut stdin = std::io::stdin();
    let mut byte = [0_u8; 1];
    let mut line = Vec::new();
    let mut active = false;
    let mut awaiting_approval = false;
    let mut enters_until_submit = submit_after;
    let mut bracket_sequence = Vec::new();
    let mut in_bracketed_paste = false;
    let mut saw_bracketed_paste = false;
    loop {
        match stdin.read(&mut byte) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if require_bracketed_paste && (!bracket_sequence.is_empty() || byte[0] == 0x1b) {
            const START: &[u8] = b"\x1b[200~";
            const END: &[u8] = b"\x1b[201~";
            bracket_sequence.push(byte[0]);
            if bracket_sequence == START {
                bracket_sequence.clear();
                in_bracketed_paste = true;
                saw_bracketed_paste = true;
            } else if bracket_sequence == END {
                bracket_sequence.clear();
                in_bracketed_paste = false;
            } else if !START.starts_with(&bracket_sequence) && !END.starts_with(&bracket_sequence) {
                // This option is a focused transport test double. Treat any
                // other escape sequence as Escape and discard its suffix.
                bracket_sequence.clear();
                if active || awaiting_approval {
                    active = false;
                    awaiting_approval = false;
                    write_record(
                        &transcript,
                        json!({"type":"event_msg","payload":{"type":"turn_aborted"}}),
                    );
                    fire_hooks(
                        &settings,
                        "Stop",
                        &hook_payload("Stop", &session_id, &cwd, &transcript),
                    );
                }
            }
            continue;
        }
        match byte[0] {
            0x03 | 0x04 => break,
            0x15 => line.clear(),
            0x1b => {
                if active || awaiting_approval {
                    active = false;
                    awaiting_approval = false;
                    write_record(
                        &transcript,
                        json!({"type":"event_msg","payload":{"type":"turn_aborted"}}),
                    );
                    fire_hooks(
                        &settings,
                        "Stop",
                        &hook_payload("Stop", &session_id, &cwd, &transcript),
                    );
                }
            }
            b'\r' | b'\n' => {
                if in_bracketed_paste {
                    line.push(b'\n');
                    continue;
                }
                if require_bracketed_paste && !saw_bracketed_paste {
                    continue;
                }
                if enters_until_submit > 1 {
                    enters_until_submit -= 1;
                    continue;
                }
                enters_until_submit = submit_after;
                saw_bracketed_paste = false;
                let prompt = String::from_utf8_lossy(&line).trim().to_string();
                line.clear();
                if prompt.is_empty() {
                    continue;
                }
                if awaiting_approval {
                    awaiting_approval = false;
                    finish_turn(
                        &settings,
                        &session_id,
                        &cwd,
                        &transcript,
                        "approval accepted",
                    );
                    continue;
                }

                let mock_command = match parse_mock_command(&prompt) {
                    Ok(command) => command,
                    Err(error) => {
                        eprintln!("mock_codex: {error}");
                        unsafe {
                            libc::tcsetattr(stdin_fd, libc::TCSANOW, &original);
                        }
                        std::process::exit(2);
                    }
                };
                if let Some(command) = mock_command
                    && !matches!(
                        command,
                        MockCommand::Help
                            | MockCommand::Active
                            | MockCommand::Permission(None)
                            | MockCommand::PolicyShow
                            | MockCommand::PolicyRestrict
                    )
                {
                    eprintln!("mock_codex: unsupported command {command:?}");
                    unsafe {
                        libc::tcsetattr(stdin_fd, libc::TCSANOW, &original);
                    }
                    std::process::exit(2);
                }

                if !session_started {
                    fire_hooks(
                        &settings,
                        "SessionStart",
                        &hook_payload("SessionStart", &session_id, &cwd, &transcript),
                    );
                    session_started = true;
                }
                let mut submitted =
                    hook_payload("UserPromptSubmit", &session_id, &cwd, &transcript);
                submitted["prompt"] = json!(prompt);
                fire_hooks(&settings, "UserPromptSubmit", &submitted);
                write_record(
                    &transcript,
                    json!({"type":"event_msg","payload":{"type":"task_started"}}),
                );
                response_message(&transcript, "user", &prompt);

                if matches!(mock_command, Some(MockCommand::Active)) {
                    active = true;
                    continue;
                }
                if matches!(mock_command, Some(MockCommand::Permission(None))) {
                    awaiting_approval = true;
                    fire_hooks(
                        &settings,
                        "PermissionRequest",
                        &hook_payload("PermissionRequest", &session_id, &cwd, &transcript),
                    );
                    continue;
                }
                if matches!(mock_command, Some(MockCommand::PolicyRestrict)) {
                    policy = json!({"approvalPolicy":"on-request","sandbox":"workspace-write"});
                    finish_turn(&settings, &session_id, &cwd, &transcript, "restricted");
                    continue;
                }
                let response = if matches!(mock_command, Some(MockCommand::PolicyShow)) {
                    policy.to_string()
                } else if matches!(mock_command, Some(MockCommand::Help)) {
                    MOCK_HELP.to_string()
                } else if active {
                    active = false;
                    format!("steered: {prompt}")
                } else {
                    format!("mock response: {prompt}")
                };
                finish_turn(&settings, &session_id, &cwd, &transcript, &response);
            }
            value => line.push(value),
        }
    }

    fire_hooks(
        &settings,
        "SessionEnd",
        &hook_payload("SessionEnd", &session_id, &cwd, &transcript),
    );
    unsafe {
        libc::tcsetattr(stdin_fd, libc::TCSANOW, &original);
    }
}
