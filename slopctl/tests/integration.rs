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
    fn start() -> Option<Self> {
        let tmpdir = tempfile::tempdir().unwrap();
        let result = Command::new("tmux")
            .args(["-S", "default", "new-session", "-d", "-s", "test"])
            .env_remove("TMUX")
            .env("TMUX_TMPDIR", tmpdir.path())
            .env("TMPDIR", tmpdir.path())
            .status();
        match result {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => panic!("failed to start tmux: {}", e),
            Ok(status) => assert!(status.success(), "failed to start tmux server"),
        }
        Some(TmuxServer { tmpdir })
    }

    fn apply(&self, cmd: &mut Command) {
        cmd.env_remove("TMUX")
            .env("TMUX_TMPDIR", self.tmpdir.path())
            .env("TMPDIR", self.tmpdir.path());
    }
}

impl Drop for TmuxServer {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-S", "default", "kill-server"])
            .env_remove("TMUX")
            .env("TMUX_TMPDIR", self.tmpdir.path())
            .env("TMPDIR", self.tmpdir.path())
            .status();
    }
}

#[test]
fn status_with_slopd_running() {
    build_bin("slopd");

    let Some(tmux) = TmuxServer::start() else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let runtime_dir = tempfile::tempdir().unwrap();

    let mut slopd_cmd = Command::new(cargo_bin("slopd"));
    slopd_cmd
        .env("XDG_RUNTIME_DIR", runtime_dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    tmux.apply(&mut slopd_cmd);

    let mut slopd = slopd_cmd.spawn().expect("failed to spawn slopd");

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
