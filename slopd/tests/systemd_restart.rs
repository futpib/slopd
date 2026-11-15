use libsloptest::{build_bin, cargo_bin};

/// Exercise the shipped units through the real user manager and tmux scopes.
#[test]
#[ignore = "requires a systemd user manager, systemd-enabled tmux, and python3"]
fn systemd_restart_preserves_agent_processes() {
    for name in ["slopd", "slopctl", "mock_claude"] {
        build_bin(name);
    }
    let status = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/systemd_restart.py"
        ))
        .env("SLOPD_TEST_BINARY", cargo_bin("slopd"))
        .env("SLOPCTL_TEST_BINARY", cargo_bin("slopctl"))
        .env("MOCK_CLAUDE_TEST_BINARY", cargo_bin("mock_claude"))
        .status()
        .expect("start systemd restart test");
    assert!(status.success(), "systemd restart regression failed");
}
