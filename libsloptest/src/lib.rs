pub use tempfile;

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

pub fn cargo_bin(name: &str) -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join(name)
}

pub fn build_bin(name: &str) {
    if cfg!(coverage) {
        return;
    }
    let status = Command::new(env!("CARGO"))
        .args(["build", "--workspace", "--bin", name])
        .status()
        .expect("failed to run cargo build");
    assert!(status.success(), "cargo build --bin {} failed", name);
}

/// Send SIGTERM and wait. Use instead of Child::kill() so instrumented binaries
/// can flush LLVM coverage data before exiting.
pub fn kill_child(mut child: Child) {
    let pid = nix::unistd::Pid::from_raw(child.id() as i32);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).unwrap();
    child.wait().unwrap();
}

pub fn kill_slopd(child: Child) {
    kill_child(child);
}

pub struct TmuxServer {
    #[allow(dead_code)]
    tmpdir: tempfile::TempDir,
    pub socket: PathBuf,
}

impl TmuxServer {
    pub fn start() -> Option<Self> {
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

    pub fn tmux(&self) -> Command {
        let mut cmd = Command::new("tmux");
        cmd.args(["-S", self.socket.to_str().unwrap()])
            .env_remove("TMUX")
            .env_remove("TMUX_TMPDIR")
            .env_remove("TMPDIR");
        cmd
    }

    pub fn write_slopd_config(&self, config_dir: &tempfile::TempDir, executable: Option<&[&str]>) {
        self.write_slopd_config_full(config_dir, executable, None, None);
    }

    pub fn write_slopd_config_full(
        &self,
        config_dir: &tempfile::TempDir,
        executable: Option<&[&str]>,
        slopctl: Option<&str>,
        claude_config_dir: Option<&PathBuf>,
    ) {
        let slopd_config_dir = config_dir.path().join("slopd");
        std::fs::create_dir_all(&slopd_config_dir).unwrap();
        let mut config = String::new();
        if let Some(path) = claude_config_dir {
            config.push_str(&format!("claude_config_dir = {:?}\n\n", path.to_str().unwrap()));
        }
        config.push_str(&format!("[tmux]\nsocket = {:?}\n", self.socket.to_str().unwrap()));
        let has_run_section = executable.is_some() || slopctl.is_some();
        if has_run_section {
            config.push_str("\n[run]\n");
            if let Some(exe) = executable {
                let toml_array: Vec<String> = exe.iter().map(|s| format!("{:?}", s)).collect();
                config.push_str(&format!("executable = [{}]\n", toml_array.join(", ")));
            }
            if let Some(s) = slopctl {
                config.push_str(&format!("slopctl = {:?}\n", s));
            }
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

pub struct TestEnv {
    pub tmux: TmuxServer,
    pub runtime_dir: tempfile::TempDir,
    pub config_dir: tempfile::TempDir,
}

impl TestEnv {
    pub fn new(executable: Option<&[&str]>) -> Option<Self> {
        let tmux = TmuxServer::start()?;
        let runtime_dir = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let claude_config_dir = config_dir.path().join(".claude");
        tmux.write_slopd_config_full(&config_dir, executable, None, Some(&claude_config_dir));
        Some(TestEnv { tmux, runtime_dir, config_dir })
    }

    pub fn new_full(
        executable: Option<&[&str]>,
        slopctl: Option<&str>,
        claude_config_dir: Option<&PathBuf>,
    ) -> Option<Self> {
        let tmux = TmuxServer::start()?;
        let runtime_dir = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        tmux.write_slopd_config_full(&config_dir, executable, slopctl, claude_config_dir);
        Some(TestEnv { tmux, runtime_dir, config_dir })
    }

    pub fn spawn_slopd(&self) -> Child {
        let child = Command::new(cargo_bin("slopd"))
            .env("XDG_RUNTIME_DIR", self.runtime_dir.path())
            .env("XDG_CONFIG_HOME", self.config_dir.path())
            .env_remove("TMUX")
            .env_remove("TMUX_TMPDIR")
            .env_remove("TMPDIR")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn slopd");
        // Wait for slopd to be ready by polling until a connection to its socket succeeds.
        let socket = self.runtime_dir.path().join("slopd/slopd.sock");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("timed out waiting for slopd to accept connections at {}", socket.display());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        child
    }

    pub fn slopctl(&self, args: &[&str]) -> std::process::Output {
        Command::new(cargo_bin("slopctl"))
            .args(args)
            .env("XDG_RUNTIME_DIR", self.runtime_dir.path())
            .output()
            .expect("failed to run slopctl")
    }

    pub fn socket_path(&self) -> PathBuf {
        self.runtime_dir.path().join("slopd/slopd.sock")
    }
}
