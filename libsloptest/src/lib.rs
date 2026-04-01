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
    if cfg!(coverage) && cargo_bin(name).exists() {
        return;
    }
    use std::collections::HashSet;
    use std::sync::Mutex;
    static BUILT: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let mut guard = BUILT.lock().unwrap();
    let built = guard.get_or_insert_with(HashSet::new);
    if built.contains(name) {
        return;
    }
    let status = Command::new(env!("CARGO"))
        .args(["build", "--workspace", "--bin", name, "--all-features"])
        .status()
        .expect("failed to run cargo build");
    assert!(status.success(), "cargo build --bin {} failed", name);
    built.insert(name.to_string());
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
        self.write_slopd_config_full(config_dir, executable, None, None, None);
    }

    pub fn write_slopd_config_full(
        &self,
        config_dir: &tempfile::TempDir,
        executable: Option<&[&str]>,
        slopctl: Option<&str>,
        claude_config_dir: Option<&PathBuf>,
        start_directory: Option<&str>,
    ) {
        let slopd_config_dir = config_dir.path().join("slopd");
        std::fs::create_dir_all(&slopd_config_dir).unwrap();
        let mut config = String::new();
        if let Some(path) = claude_config_dir {
            config.push_str(&format!("claude_config_dir = {:?}\n\n", path.to_str().unwrap()));
        }
        config.push_str(&format!("[tmux]\nsocket = {:?}\n", self.socket.to_str().unwrap()));
        let has_run_section = executable.is_some() || slopctl.is_some() || start_directory.is_some();
        if has_run_section {
            config.push_str("\n[run]\n");
            if let Some(exe) = executable {
                let toml_array: Vec<String> = exe.iter().map(|s| format!("{:?}", s)).collect();
                config.push_str(&format!("executable = [{}]\n", toml_array.join(", ")));
            }
            if let Some(s) = slopctl {
                config.push_str(&format!("slopctl = {:?}\n", s));
            }
            if let Some(dir) = start_directory {
                config.push_str(&format!("start_directory = {:?}\n", dir));
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
        tmux.write_slopd_config(&config_dir, executable);
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
        tmux.write_slopd_config_full(&config_dir, executable, slopctl, claude_config_dir, None);
        Some(TestEnv { tmux, runtime_dir, config_dir })
    }

    pub fn new_with_start_directory(
        executable: Option<&[&str]>,
        start_directory: &str,
    ) -> Option<Self> {
        let tmux = TmuxServer::start()?;
        let runtime_dir = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        tmux.write_slopd_config_full(&config_dir, executable, None, None, Some(start_directory));
        Some(TestEnv { tmux, runtime_dir, config_dir })
    }

    pub fn spawn_slopd(&self) -> Child {
        self.spawn_slopd_inner(None, &[])
    }

    /// Like `spawn_slopd` but sets `SLOPD_TEST_RUN_YIELD_MS` to the given value.
    /// This causes slopd to sleep for `delay_ms` milliseconds inside the Run handler
    /// after inserting the pane into managed_panes, making it certain that any
    /// concurrent hook (e.g. SessionStart fired by mock_claude at startup) is
    /// processed before the Run handler's pane-state guard runs.  Used to write
    /// deterministic regression tests for the race described in:
    ///   fix: guard Run handler from resetting pane state that a concurrent hook already advanced
    pub fn spawn_slopd_with_run_yield(&self, delay_ms: u64) -> Child {
        self.spawn_slopd_inner(Some(delay_ms), &[])
    }

    /// Like `spawn_slopd` but passes extra CLI arguments to the slopd binary.
    pub fn spawn_slopd_with_args(&self, extra_args: &[&str]) -> Child {
        self.spawn_slopd_inner(None, extra_args)
    }

    fn spawn_slopd_inner(&self, run_yield_ms: Option<u64>, extra_args: &[&str]) -> Child {
        let mut cmd = Command::new(cargo_bin("slopd"));
        cmd.args(extra_args)
            .env("XDG_RUNTIME_DIR", self.runtime_dir.path())
            .env("XDG_CONFIG_HOME", self.config_dir.path())
            .env("HOME", self.config_dir.path())
            .env_remove("TMUX")
            .env_remove("TMUX_TMPDIR")
            .env_remove("TMPDIR")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(ms) = run_yield_ms {
            cmd.env("SLOPD_TEST_RUN_YIELD_MS", ms.to_string());
        }
        let child = cmd.spawn().expect("failed to spawn slopd");
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

    /// Spawn a `slopctl listen --hook SessionStart` subscriber and wait until
    /// the subscription is confirmed. Call this before `slopctl run` to
    /// guarantee no race where the event fires before we subscribe.
    /// Pass the returned child to `wait_for_session_start`.
    pub fn spawn_session_start_listener(&self) -> Child {
        let mut child = Command::new(cargo_bin("slopctl"))
            .args(["listen", "--hook", "SessionStart"])
            .env("XDG_RUNTIME_DIR", self.runtime_dir.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn slopctl listen");
        // Read the {"subscribed":true} confirmation before returning so the
        // caller knows the subscription is active and no events will be missed.
        // Read byte-by-byte to avoid buffering bytes that wait_for_session_start needs.
        let stdout = child.stdout.as_mut().expect("listener has no stdout");
        let mut line = Vec::new();
        let mut buf = [0u8; 1];
        loop {
            use std::io::Read;
            stdout.read_exact(&mut buf).expect("failed to read subscription confirmation");
            if buf[0] == b'\n' { break; }
            line.push(buf[0]);
        }
        let line = String::from_utf8_lossy(&line);
        assert!(line.contains("subscribed"), "unexpected first line from slopctl listen: {:?}", line);
        child
    }

    /// Read from a listener spawned by `spawn_session_start_listener` until a
    /// SessionStart event for `pane_id` arrives. Returns the `session_id` from
    /// the event payload and kills the listener. Panics if no event arrives
    /// within 10 seconds.
    pub fn wait_for_session_start(&self, mut listener: Child, pane_id: &str) -> String {
        use std::io::BufRead;
        let stdout = listener.stdout.take().expect("listener has no stdout");
        let pane_id = pane_id.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => { let _ = tx.send(Err(())); return; }
                    Ok(_) => {}
                }
                let v: serde_json::Value = match serde_json::from_str(line.trim()) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if v["pane_id"] == pane_id {
                    let session_id = v["payload"]["session_id"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    let _ = tx.send(Ok(session_id));
                    return;
                }
            }
        });
        let session_id = rx.recv_timeout(Duration::from_secs(30))
            .expect("timed out waiting for SessionStart")
            .expect("slopctl listen closed before SessionStart");
        kill_child(listener);
        session_id
    }

    /// Query `slopctl ps --json` and return the `(state, detailed_state)` for `pane_id`.
    /// Panics if the pane is not found.
    pub fn pane_state(&self, pane_id: &str) -> (libslop::PaneState, libslop::PaneDetailedState) {
        let output = self.slopctl(&["ps", "--json"]);
        assert!(output.status.success(), "slopctl ps --json failed: {:?}", output);
        let panes: Vec<libslop::PaneInfo> = serde_json::from_slice(&output.stdout)
            .expect("failed to parse slopctl ps --json output");
        let pane = panes.into_iter().find(|p| p.pane_id == pane_id)
            .unwrap_or_else(|| panic!("pane {} not found in slopctl ps output", pane_id));
        (pane.state, pane.detailed_state)
    }

    /// Like `wait_for_session_start` but waits for SessionStart on all `pane_ids`.
    /// Uses a single listener, so spawn it before issuing any `slopctl run` calls.
    /// Panics if not all events arrive within 10 seconds.
    pub fn wait_for_session_starts(&self, mut listener: Child, pane_ids: &[&str]) {
        use std::io::BufRead;
        let stdout = listener.stdout.take().expect("listener has no stdout");
        let pane_ids_owned: Vec<String> = pane_ids.iter().map(|s| s.to_string()).collect();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            let mut remaining: std::collections::HashSet<String> = pane_ids_owned.into_iter().collect();
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => { let _ = tx.send(Err(remaining)); return; }
                    Ok(_) => {}
                }
                let v: serde_json::Value = match serde_json::from_str(line.trim()) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(pane_id) = v["pane_id"].as_str() {
                    remaining.remove(pane_id);
                }
                if remaining.is_empty() {
                    let _ = tx.send(Ok(()));
                    return;
                }
            }
        });
        rx.recv_timeout(Duration::from_secs(30))
            .expect("timed out waiting for SessionStart on all panes")
            .expect("slopctl listen closed before all SessionStart events");
        kill_child(listener);
    }
}
