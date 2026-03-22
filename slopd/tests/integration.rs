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
    #[allow(dead_code)]
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

    fn write_slopd_config(&self, config_dir: &tempfile::TempDir) {
        let slopd_config_dir = config_dir.path().join("slopd");
        std::fs::create_dir_all(&slopd_config_dir).unwrap();
        std::fs::write(
            slopd_config_dir.join("config.toml"),
            format!("[tmux]\nsocket = {:?}\n", self.socket.to_str().unwrap()),
        )
        .unwrap();
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

impl TmuxServer {
    fn tmux(&self) -> Command {
        let mut cmd = Command::new("tmux");
        cmd.args(["-S", self.socket.to_str().unwrap()])
            .env_remove("TMUX")
            .env_remove("TMUX_TMPDIR")
            .env_remove("TMPDIR");
        cmd
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
    let config_dir = tempfile::tempdir().unwrap();
    tmux.write_slopd_config(&config_dir);

    let mut slopd = Command::new(cargo_bin("slopd"))
        .env("XDG_RUNTIME_DIR", runtime_dir.path())
        .env("XDG_CONFIG_HOME", config_dir.path())
        .env_remove("TMUX")
        .env_remove("TMUX_TMPDIR")
        .env_remove("TMPDIR")
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

    if !tmux_available() {
        eprintln!("skipping: tmux not found");
        return;
    }

    let runtime_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();

    let status = Command::new(cargo_bin("slopd"))
        .env("XDG_RUNTIME_DIR", runtime_dir.path())
        .env("XDG_CONFIG_HOME", config_dir.path())
        .env_remove("TMUX")
        .env_remove("TMUX_TMPDIR")
        .env_remove("TMPDIR")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("failed to run slopd");

    assert!(!status.success(), "slopd should have failed without tmux");
}

#[test]
fn slopd_creates_marked_tmux_session() {
    build_bin("slopd");

    let Some(tmux) = TmuxServer::start() else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let runtime_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    tmux.write_slopd_config(&config_dir);

    let mut slopd = Command::new(cargo_bin("slopd"))
        .env("XDG_RUNTIME_DIR", runtime_dir.path())
        .env("XDG_CONFIG_HOME", config_dir.path())
        .env_remove("TMUX")
        .env_remove("TMUX_TMPDIR")
        .env_remove("TMPDIR")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopd");

    std::thread::sleep(Duration::from_millis(100));

    // Verify the slopd session exists
    let session_exists = tmux.tmux()
        .args(["has-session", "-t", "slopd"])
        .status()
        .expect("failed to run tmux has-session")
        .success();

    // Verify the @slopd user option is set on the session
    let option_output = tmux.tmux()
        .args(["show-options", "-t", "slopd", "-v", "@slopd"])
        .output()
        .expect("failed to run tmux show-options");
    let option_value = String::from_utf8_lossy(&option_output.stdout);

    slopd.kill().unwrap();
    slopd.wait().unwrap();

    assert!(session_exists, "slopd tmux session does not exist");
    assert_eq!(option_value.trim(), "true", "@slopd option not set correctly");
}
