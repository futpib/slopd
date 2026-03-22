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
            .env("TMUX_TMPDIR", tmpdir.path())
            .status();
        match result {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => panic!("failed to start tmux: {}", e),
            Ok(status) => assert!(status.success(), "failed to start tmux server"),
        }
        Some(TmuxServer { tmpdir })
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
fn slopd_starts_with_tmux_running() {
    build_bin("slopd");

    let Some(tmux) = TmuxServer::start() else {
        eprintln!("skipping: tmux not found");
        return;
    };
    let (tmux_env_key, tmux_env_val) = tmux.env();

    let runtime_dir = tempfile::tempdir().unwrap();

    let mut slopd = Command::new(cargo_bin("slopd"))
        .env("XDG_RUNTIME_DIR", runtime_dir.path())
        .env(tmux_env_key, tmux_env_val)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopd");

    std::thread::sleep(Duration::from_millis(100));

    let still_running = slopd.try_wait().unwrap().is_none();
    slopd.kill().unwrap();
    slopd.wait().unwrap();

    assert!(still_running, "slopd exited early");
}

#[test]
fn slopd_fails_without_tmux_running() {
    build_bin("slopd");

    let Some(_tmux_check) = (match Command::new("tmux").arg("-V").status() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipping: tmux not found");
            return;
        }
        Err(e) => panic!("unexpected error checking for tmux: {}", e),
        Ok(_) => Some(()),
    }) else {
        return;
    };

    let runtime_dir = tempfile::tempdir().unwrap();
    let tmux_tmpdir = tempfile::tempdir().unwrap();

    let status = Command::new(cargo_bin("slopd"))
        .env("XDG_RUNTIME_DIR", runtime_dir.path())
        .env("TMUX_TMPDIR", tmux_tmpdir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("failed to run slopd");

    assert!(!status.success(), "slopd should have failed without tmux");
}
