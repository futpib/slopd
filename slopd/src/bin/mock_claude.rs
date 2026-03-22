use std::io::{Read, Write};
use std::process::{Command, Stdio};

/// Run all command hooks registered for the given event, passing payload as JSON on stdin.
/// Mirrors real Claude's hook execution: each command is run via `sh -c` in a non-interactive
/// shell with the JSON payload on stdin.
fn fire_hooks(settings: &serde_json::Value, event: &str, payload: &serde_json::Value) {
    let Some(entries) = settings["hooks"][event].as_array() else {
        return;
    };
    for entry in entries {
        let Some(hooks) = entry["hooks"].as_array() else {
            continue;
        };
        for hook in hooks {
            if hook["type"] != "command" {
                continue;
            }
            let Some(command) = hook["command"].as_str() else {
                continue;
            };
            let mut child = Command::new("sh")
                .args(["-c", command])
                .stdin(Stdio::piped())
                .spawn()
                .unwrap_or_else(|e| {
                    eprintln!("mock_claude: failed to spawn hook {:?}: {}", command, e);
                    std::process::exit(1);
                });
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(payload.to_string().as_bytes())
                .expect("failed to write hook payload to stdin");
            let status = child.wait().expect("failed to wait for hook");
            if !status.success() {
                eprintln!("mock_claude: hook {:?} exited with {:?}", command, status);
            }
        }
    }
}

fn main() {
    // Real Claude reads $CLAUDE_CONFIG_DIR/settings.json (default: ~/.claude/settings.json).
    let settings_path = {
        let config_dir = std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            format!("{}/.claude", home)
        });
        format!("{}/settings.json", config_dir)
    };

    let settings: serde_json::Value = std::fs::read_to_string(&settings_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    let session_id = "mock-session-id-1234";
    let cwd = std::env::current_dir().unwrap_or_default();

    fire_hooks(
        &settings,
        "SessionStart",
        &serde_json::json!({
            "session_id": session_id,
            "hook_event_name": "SessionStart",
            "transcript_path": "/dev/null",
            "cwd": cwd,
            "source": "startup",
            "model": "mock"
        }),
    );

    // Put the terminal in raw mode so we receive key bytes directly (Ctrl+C = 0x03,
    // Ctrl+D = 0x04, Escape = 0x1b) rather than having the terminal driver intercept them.
    // This mirrors real Claude's interactive terminal behaviour.
    let stdin_fd = 0i32; // STDIN_FILENO
    let orig_termios = unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        libc::tcgetattr(stdin_fd, &mut t);
        let orig = t;
        libc::cfmakeraw(&mut t);
        libc::tcsetattr(stdin_fd, libc::TCSANOW, &t);
        orig
    };

    // Read raw bytes from stdin, accumulating lines.
    // Mirrors real Claude terminal behaviour:
    //   - Single Esc, C-c, or C-d: interrupt (drop current work, back to prompt)
    //   - Two consecutive C-c or two consecutive C-d: exit
    //   - Two consecutive Esc: rewind mode (ignored here, not an exit)
    let mut line_buf = Vec::new();
    let mut last_interrupt: Option<u8> = None;
    let mut stdin = std::io::stdin();
    let mut byte = [0u8; 1];

    loop {
        match stdin.read(&mut byte) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let b = byte[0];
        match b {
            0x03 | 0x04 => {
                if last_interrupt == Some(b) {
                    // Two consecutive C-c or two consecutive C-d: exit.
                    break;
                }
                last_interrupt = Some(b);
            }
            0x1b => {
                // Single or double Esc: interrupt / rewind mode — never exit.
                last_interrupt = Some(b);
            }
            0x0d | 0x0a => {
                // Carriage return or newline: complete the line.
                last_interrupt = None;
                let prompt = String::from_utf8_lossy(&line_buf).into_owned();
                line_buf.clear();

                if let Some(secs) = prompt.strip_prefix("/sleep ") {
                    let secs: u64 = secs.trim().parse().unwrap_or(0);
                    std::thread::sleep(std::time::Duration::from_secs(secs));
                    continue;
                }
                if let Some(code) = prompt.strip_prefix("/exit ") {
                    let code: i32 = code.trim().parse().unwrap_or(0);
                    unsafe { libc::tcsetattr(stdin_fd, libc::TCSANOW, &orig_termios); }
                    std::process::exit(code);
                }
                if prompt == "/break-stdin" {
                    break;
                }
                if prompt == "/break-hooks" {
                    let mut buf = [0u8; 256];
                    while stdin.read(&mut buf).unwrap_or(0) > 0 {}
                    break;
                }

                fire_hooks(
                    &settings,
                    "UserPromptSubmit",
                    &serde_json::json!({
                        "session_id": session_id,
                        "hook_event_name": "UserPromptSubmit",
                        "transcript_path": "/dev/null",
                        "cwd": cwd,
                        "prompt": prompt
                    }),
                );
            }
            _ => {
                last_interrupt = None;
                line_buf.push(b);
            }
        }
    }

    unsafe { libc::tcsetattr(stdin_fd, libc::TCSANOW, &orig_termios); }
}
