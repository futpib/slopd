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

    fn write_slopd_config(&self, config_dir: &tempfile::TempDir, executable: Option<&str>) {
        let slopd_config_dir = config_dir.path().join("slopd");
        std::fs::create_dir_all(&slopd_config_dir).unwrap();
        let mut config = format!("[tmux]\nsocket = {:?}\n", self.socket.to_str().unwrap());
        if let Some(exe) = executable {
            config.push_str(&format!("\n[run]\nexecutable = {:?}\n", exe));
        }
        std::fs::write(slopd_config_dir.join("config.toml"), config).unwrap();
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

#[test]
fn status_with_slopd_running() {
    build_bin("slopd");

    let Some(tmux) = TmuxServer::start() else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let runtime_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    tmux.write_slopd_config(&config_dir, None);

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
