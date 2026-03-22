use std::io::Write;
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

    fire_hooks(
        &settings,
        "SessionStart",
        &serde_json::json!({
            "session_id": session_id,
            "hook_event_name": "SessionStart",
            "transcript_path": "/dev/null",
            "cwd": std::env::current_dir().unwrap_or_default(),
            "source": "startup",
            "model": "mock"
        }),
    );

    // Keep the pane alive so the test can read the tmux pane option before the pane closes.
    std::thread::sleep(std::time::Duration::from_secs(30));
}
