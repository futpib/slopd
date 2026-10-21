use libsloptest::{build_bin, cargo_bin};

/// Real TUI + embedded API + bash tool, with a deterministic local model.
/// Run explicitly with OPENCODE_TEST_BINARY=/path/to/opencode and --ignored.
#[test]
#[ignore = "requires a real OpenCode binary and python3"]
fn opencode_live_native_session_switch() {
    build_bin("slopd");
    build_bin("slopctl");
    let status = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/opencode_live.py"
        ))
        .env("SLOPD_TEST_BINARY", cargo_bin("slopd"))
        .env("SLOPCTL_TEST_BINARY", cargo_bin("slopctl"))
        .status()
        .expect("start real OpenCode test");
    assert!(status.success(), "real OpenCode TUI regression failed");
}
