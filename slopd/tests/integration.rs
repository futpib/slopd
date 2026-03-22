use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn cargo_bin(name: &str) -> std::path::PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join(name)
}

fn build_bin(name: &str) {
    if cfg!(coverage) {
        return;
    }
    let status = Command::new(env!("CARGO"))
        .args(["build", "--workspace", "--bin", name])
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

    fn write_slopd_config(&self, config_dir: &tempfile::TempDir, executable: Option<&[&str]>) {
        self.write_slopd_config_full(config_dir, executable, None, None);
    }

    fn write_slopd_config_full(
        &self,
        config_dir: &tempfile::TempDir,
        executable: Option<&[&str]>,
        slopctl: Option<&str>,
        claude_settings: Option<&PathBuf>,
    ) {
        let slopd_config_dir = config_dir.path().join("slopd");
        std::fs::create_dir_all(&slopd_config_dir).unwrap();
        let mut config = format!("[tmux]\nsocket = {:?}\n", self.socket.to_str().unwrap());
        if let Some(path) = claude_settings {
            config.push_str(&format!("\nclaude_settings = {:?}\n", path.to_str().unwrap()));
        }
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

    fn tmux(&self) -> Command {
        let mut cmd = Command::new("tmux");
        cmd.args(["-S", self.socket.to_str().unwrap()])
            .env_remove("TMUX")
            .env_remove("TMUX_TMPDIR")
            .env_remove("TMPDIR");
        cmd
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

struct TestEnv {
    tmux: TmuxServer,
    runtime_dir: tempfile::TempDir,
    config_dir: tempfile::TempDir,
}

impl TestEnv {
    fn new(executable: Option<&[&str]>) -> Option<Self> {
        let tmux = TmuxServer::start()?;
        let runtime_dir = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        tmux.write_slopd_config(&config_dir, executable);
        Some(TestEnv { tmux, runtime_dir, config_dir })
    }

    fn new_full(
        executable: Option<&[&str]>,
        slopctl: Option<&str>,
        claude_settings: Option<&PathBuf>,
    ) -> Option<Self> {
        let tmux = TmuxServer::start()?;
        let runtime_dir = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        tmux.write_slopd_config_full(&config_dir, executable, slopctl, claude_settings);
        Some(TestEnv { tmux, runtime_dir, config_dir })
    }

    fn spawn_slopd(&self) -> Child {
        Command::new(cargo_bin("slopd"))
            .env("XDG_RUNTIME_DIR", self.runtime_dir.path())
            .env("XDG_CONFIG_HOME", self.config_dir.path())
            .env_remove("TMUX")
            .env_remove("TMUX_TMPDIR")
            .env_remove("TMPDIR")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn slopd")
    }

    fn slopctl(&self, args: &[&str]) -> std::process::Output {
        Command::new(cargo_bin("slopctl"))
            .args(args)
            .env("XDG_RUNTIME_DIR", self.runtime_dir.path())
            .output()
            .expect("failed to run slopctl")
    }
}

#[test]
fn slopd_starts_with_tmux_running() {
    build_bin("slopd");

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let mut slopd = env.spawn_slopd();
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

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let mut slopd = env.spawn_slopd();
    std::thread::sleep(Duration::from_millis(100));

    let session_exists = env.tmux.tmux()
        .args(["has-session", "-t", "slopd"])
        .status()
        .expect("failed to run tmux has-session")
        .success();

    let option_output = env.tmux.tmux()
        .args(["show-options", "-t", "slopd", "-v", "@slopd"])
        .output()
        .expect("failed to run tmux show-options");
    let option_value = String::from_utf8_lossy(&option_output.stdout);

    slopd.kill().unwrap();
    slopd.wait().unwrap();

    assert!(session_exists, "slopd tmux session does not exist");
    assert_eq!(option_value.trim(), "true", "@slopd option not set correctly");
}

#[test]
fn run_spawns_executable_in_new_tmux_window() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let mut slopd = env.spawn_slopd();
    std::thread::sleep(Duration::from_millis(100));

    let output = env.slopctl(&["run"]);

    slopd.kill().unwrap();
    slopd.wait().unwrap();

    assert!(output.status.success(), "slopctl run failed: {:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.trim().starts_with('%'), "expected pane_id in output, got: {}", stdout);
}

#[test]
fn kill_terminates_pane() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let mut slopd = env.spawn_slopd();
    std::thread::sleep(Duration::from_millis(100));

    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();

    let kill_output = env.slopctl(&["kill", &pane_id]);

    slopd.kill().unwrap();
    slopd.wait().unwrap();

    assert!(kill_output.status.success(), "slopctl kill failed: {:?}", kill_output);
    let kill_stdout = String::from_utf8_lossy(&kill_output.stdout);
    assert_eq!(kill_stdout.trim(), pane_id, "kill should print the pane_id");
}

#[test]
fn run_injects_hooks_into_claude_settings() {
    build_bin("slopd");
    build_bin("slopctl");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_settings_path = home_dir.path().join(".claude/settings.json");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(
        Some(&["sleep", "infinity"]),
        Some(&slopctl_path),
        Some(&claude_settings_path),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let mut slopd = env.spawn_slopd();
    std::thread::sleep(Duration::from_millis(100));

    let output = env.slopctl(&["run"]);

    slopd.kill().unwrap();
    slopd.wait().unwrap();

    assert!(output.status.success(), "slopctl run failed: {:?}", output);

    let settings_contents = std::fs::read_to_string(&claude_settings_path)
        .expect("settings.json was not created");
    let settings: serde_json::Value =
        serde_json::from_str(&settings_contents).expect("settings.json is not valid JSON");

    for &event in libslop::HOOK_EVENTS {
        let entries = settings["hooks"][event]
            .as_array()
            .unwrap_or_else(|| panic!("missing hooks.{}", event));
        let has_our_hook = entries.iter().any(|entry| {
            entry["hooks"].as_array().map_or(false, |hooks| {
                hooks.iter().any(|h| {
                    h["type"] == "command"
                        && h["command"]
                            .as_str()
                            .map_or(false, |c| c.contains("slopctl") && c.contains(event))
                })
            })
        });
        assert!(has_our_hook, "missing slopctl hook for event {}", event);
    }
}

#[test]
fn run_hook_injection_is_idempotent() {
    build_bin("slopd");
    build_bin("slopctl");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_settings_path = home_dir.path().join(".claude/settings.json");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(
        Some(&["sleep", "infinity"]),
        Some(&slopctl_path),
        Some(&claude_settings_path),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    // Run twice to verify idempotency
    for _ in 0..2 {
        let mut slopd = env.spawn_slopd();
        std::thread::sleep(Duration::from_millis(100));

        let output = env.slopctl(&["run"]);

        slopd.kill().unwrap();
        slopd.wait().unwrap();

        assert!(output.status.success(), "slopctl run failed: {:?}", output);
        std::thread::sleep(Duration::from_millis(50));
    }

    let settings_contents = std::fs::read_to_string(&claude_settings_path)
        .expect("settings.json was not created");
    let settings: serde_json::Value =
        serde_json::from_str(&settings_contents).expect("settings.json is not valid JSON");

    for &event in libslop::HOOK_EVENTS {
        let entries = settings["hooks"][event]
            .as_array()
            .unwrap_or_else(|| panic!("missing hooks.{}", event));
        let our_hook_count = entries.iter().filter(|entry| {
            entry["hooks"].as_array().map_or(false, |hooks| {
                hooks.iter().any(|h| {
                    h["type"] == "command"
                        && h["command"]
                            .as_str()
                            .map_or(false, |c| c.contains("slopctl") && c.contains(event))
                })
            })
        }).count();
        assert_eq!(our_hook_count, 1, "expected exactly one slopctl hook for event {}, got {}", event, our_hook_count);
    }
}

#[test]
fn session_start_hook_stores_session_id_on_pane() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_settings_path = home_dir.path().join(".claude/settings.json");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        Some(&claude_settings_path),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let mut slopd = env.spawn_slopd();
    std::thread::sleep(Duration::from_millis(100));

    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();

    // Poll until @claude_session_id is set on the pane (mock_claude fires the hook then exits)
    let deadline = Instant::now() + Duration::from_secs(5);
    let session_id = loop {
        let out = env.tmux.tmux()
            .args(["show-options", "-t", &pane_id, "-p", "-v", "@claude_session_id"])
            .output()
            .expect("failed to run tmux show-options");
        let val = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !val.is_empty() {
            break val;
        }
        if Instant::now() > deadline {
            slopd.kill().unwrap();
            slopd.wait().unwrap();
            panic!("timed out waiting for @claude_session_id on pane {}", pane_id);
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    slopd.kill().unwrap();
    slopd.wait().unwrap();

    assert_eq!(session_id, "mock-session-id-1234");
}
