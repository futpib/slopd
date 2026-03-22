use std::process::{Command, Stdio};
use std::time::Duration;

fn cargo_bin(name: &str) -> std::path::PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join(name)
}

fn build_bin(name: &str) {
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", name, "--bin", name])
        .status()
        .expect("failed to run cargo build");
    assert!(status.success(), "cargo build --bin {} failed", name);
}

struct TmuxServer {
    tmpdir: tempfile::TempDir,
}

impl TmuxServer {
    fn start() -> Self {
        let tmpdir = tempfile::tempdir().unwrap();
        let status = Command::new("tmux")
            .args(["-S", "default", "new-session", "-d", "-s", "test"])
            .env("TMUX_TMPDIR", tmpdir.path())
            .status()
            .expect("failed to start tmux");
        assert!(status.success(), "failed to start tmux server");
        TmuxServer { tmpdir }
    }

    fn env(&self) -> (&str, &std::path::Path) {
        ("TMUX_TMPDIR", self.tmpdir.path())
    }
}

impl Drop for TmuxServer {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-S", "default", "kill-server"])
            .env("TMUX_TMPDIR", self.tmpdir.path())
            .status();
    }
}

#[test]
fn status_with_slopd_running() {
    build_bin("slopd");

    let tmux = TmuxServer::start();
    let (tmux_env_key, tmux_env_val) = tmux.env();

    let runtime_dir = tempfile::tempdir().unwrap();

    let mut slopd = Command::new(cargo_bin("slopd"))
        .env("XDG_RUNTIME_DIR", runtime_dir.path())
        .env(tmux_env_key, tmux_env_val)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopd");

    // Give slopd time to bind the socket
    std::thread::sleep(Duration::from_millis(100));

    let output = Command::new(cargo_bin("slopctl"))
        .arg("status")
        .env("XDG_RUNTIME_DIR", runtime_dir.path())
        .output()
        .expect("failed to run slopctl");

    slopd.kill().unwrap();
    slopd.wait().unwrap();

    assert!(output.status.success(), "slopctl exited with failure: {:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Status"), "unexpected output: {}", stdout);
}

#[test]
fn status_without_slopd_running() {
    let runtime_dir = tempfile::tempdir().unwrap();

    let output = Command::new(cargo_bin("slopctl"))
        .arg("status")
        .env("XDG_RUNTIME_DIR", runtime_dir.path())
        .output()
        .expect("failed to run slopctl");

    assert!(!output.status.success(), "slopctl should have failed but succeeded");
}
