use std::io::Write;
use std::process::{Command, Stdio};

fn main() {
    let session_id = "mock-session-id-1234";

    let payload = serde_json::json!({
        "session_id": session_id,
        "hook_event_name": "SessionStart",
        "transcript_path": "/dev/null",
        "cwd": std::env::current_dir().unwrap_or_default(),
        "source": "startup",
        "model": "mock"
    });

    let slopctl = std::env::var("SLOPCTL").unwrap_or_else(|_| "slopctl".to_string());

    let mut child = Command::new(&slopctl)
        .args(["hook", "SessionStart"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| {
            eprintln!("mock_claude: failed to spawn slopctl: {}", e);
            std::process::exit(1);
        });

    let stdin = child.stdin.as_mut().unwrap();
    stdin
        .write_all(payload.to_string().as_bytes())
        .expect("failed to write to slopctl stdin");
    drop(child.stdin.take());

    let status = child.wait().expect("failed to wait for slopctl");
    if !status.success() {
        eprintln!("mock_claude: slopctl hook SessionStart failed: {:?}", status);
        std::process::exit(1);
    }

    // Keep the pane alive so the test can read the pane option before the pane closes
    std::thread::sleep(std::time::Duration::from_secs(10));
}
