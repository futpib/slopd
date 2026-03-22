use std::path::PathBuf;
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
    socket: PathBuf,
}

impl TmuxServer {
    fn start() -> Option<Self> {
        let tmpdir = tempfile::tempdir().unwrap();
        let socket = tmpdir.path().join("tmux.sock");
        let result = Command::new("tmux")
            .args(["-S", socket.to_str().unwrap(), "new-session", "-d", "-s", "test"])
            .env_remove("TMUX")
            .env_remove("TMUX_TMPDIR")
            .env_remove("TMPDIR")
            .status();
        match result {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => panic!("failed to start tmux: {}", e),
            Ok(status) => assert!(status.success(), "failed to start tmux server"),
        }
        Some(TmuxServer { tmpdir, socket })
    }

    // Sets env vars so that slopd's `tmux list-sessions` (no -S flag) finds this server.
    fn apply(&self, cmd: &mut Command) {
        cmd.env_remove("TMUX")
            .env("TMUX_TMPDIR", self.tmpdir.path())
            .env_remove("TMPDIR");
    }
}

impl Drop for TmuxServer {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-S", self.socket.to_str().unwrap(), "kill-server"])
            .env_remove("TMUX")
            .env_remove("TMUX_TMPDIR")
            .env_remove("TMPDIR")
            .status();
    }
}

fn tmux_available() -> bool {
    match Command::new("tmux").arg("-V").status() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => panic!("unexpected error checking for tmux: {}", e),
        Ok(_) => true,
    }
}

#[test]
fn slopd_starts_with_tmux_running() {
    build_bin("slopd");

    let Some(tmux) = TmuxServer::start() else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let runtime_dir = tempfile::tempdir().unwrap();

    let mut cmd = Command::new(cargo_bin("slopd"));
    cmd.env("XDG_RUNTIME_DIR", runtime_dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    tmux.apply(&mut cmd);

    let mut slopd = cmd.spawn().expect("failed to spawn slopd");

    std::thread::sleep(Duration::from_millis(100));

    let still_running = slopd.try_wait().unwrap().is_none();
    slopd.kill().unwrap();
    slopd.wait().unwrap();

    assert!(still_running, "slopd exited early");
}

#[test]
fn slopd_fails_without_tmux_running() {
    build_bin("slopd");

    if !tmux_available() {
        eprintln!("skipping: tmux not found");
        return;
    }

    let runtime_dir = tempfile::tempdir().unwrap();
    let tmux_tmpdir = tempfile::tempdir().unwrap();

    let status = Command::new(cargo_bin("slopd"))
        .env("XDG_RUNTIME_DIR", runtime_dir.path())
        .env_remove("TMUX")
        .env("TMUX_TMPDIR", tmux_tmpdir.path())
        .env_remove("TMPDIR")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("failed to run slopd");

    assert!(!status.success(), "slopd should have failed without tmux");
}
