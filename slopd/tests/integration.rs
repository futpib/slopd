use libsloptest::{build_bin, cargo_bin, kill_child, kill_slopd, tempfile, TestEnv};
use std::io::BufRead;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Fire a hook event by calling slopctl hook with the given JSON payload on stdin.
fn fire_hook(env: &TestEnv, event: &str, payload: &str, pane_id: Option<&str>) -> std::process::Output {
    let mut cmd = Command::new(cargo_bin("slopctl"));
    cmd.args(["hook", event])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(pane) = pane_id {
        cmd.env("TMUX_PANE", pane);
    }
    let mut child = cmd.spawn().expect("failed to spawn slopctl hook");
    use std::io::Write;
    child.stdin.as_mut().unwrap().write_all(payload.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn tmux_available() -> bool {
    match Command::new("tmux").arg("-V").status() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => panic!("unexpected error checking for tmux: {}", e),
        Ok(_) => true,
    }
}

/// Hook must never exit 2 — that would block the Claude action.
/// Errors should exit 1 (visible failure), never 2.
#[test]
fn hook_never_exits_2() {
    build_bin("slopctl");

    let runtime_dir = tempfile::tempdir().unwrap();
    let payload = r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"hi"}"#;

    let mut child = Command::new(cargo_bin("slopctl"))
        .args(["hook", "UserPromptSubmit"])
        .env("XDG_RUNTIME_DIR", runtime_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl hook");

    use std::io::Write;
    child.stdin.as_mut().unwrap().write_all(payload.as_bytes()).unwrap();
    let status = child.wait_with_output().unwrap().status;

    assert_ne!(status.code(), Some(2), "hook must never exit 2 (would block Claude action)");
    assert_ne!(status.code(), Some(0), "hook should exit non-zero on error (slopd unreachable)");
}

#[test]
fn slopd_starts_with_tmux_running() {
    build_bin("slopd");

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let mut slopd = env.spawn_slopd();

    let still_running = slopd.try_wait().unwrap().is_none();
    kill_slopd(slopd);

    assert!(still_running, "slopd exited early");
}

#[test]
fn slopd_second_instance_fails_when_first_is_running() {
    build_bin("slopd");

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd1 = env.spawn_slopd();

    let mut slopd2 = Command::new(cargo_bin("slopd"))
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .env("XDG_CONFIG_HOME", env.config_dir.path())
        .env_remove("TMUX")
        .env_remove("TMUX_TMPDIR")
        .env_remove("TMPDIR")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn second slopd");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let exited = loop {
        if let Some(status) = slopd2.try_wait().unwrap() {
            break Some(status);
        }
        if std::time::Instant::now() > deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    if exited.is_none() {
        kill_child(slopd2);
    }
    kill_slopd(slopd1);

    let status2 = exited.expect("second slopd instance should have exited, but it kept running");
    assert!(!status2.success(), "second slopd instance should have failed");
}

#[test]
fn slopd_fails_without_tmux_running() {
    build_bin("slopd");

    if !tmux_available() {
        eprintln!("skipping: tmux not found");
        return;
    }

    // Use a non-existent custom socket path and disable start_server so slopd
    // must connect to an already-running server (which isn't there).
    let runtime_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let slopd_config_dir = config_dir.path().join("slopd");
    std::fs::create_dir_all(&slopd_config_dir).unwrap();
    std::fs::write(
        slopd_config_dir.join("config.toml"),
        "[tmux]\nsocket = \"/nonexistent/tmux.sock\"\nstart_server = false\n",
    ).unwrap();

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

    let slopd = env.spawn_slopd();

    let session_exists = env.tmux.tmux()
        .args(["has-session", "-t", "slopd"])
        .status()
        .expect("failed to run tmux has-session")
        .success();

    let option_output = env.tmux.tmux()
        .args(["show-options", "-t", "slopd", "-v", libslop::TmuxOption::SlopdManaged.as_str()])
        .output()
        .expect("failed to run tmux show-options");
    let option_value = String::from_utf8_lossy(&option_output.stdout);

    kill_slopd(slopd);

    assert!(session_exists, "slopd tmux session does not exist");
    assert_eq!(option_value.trim(), "true", "{} option not set correctly", libslop::TmuxOption::SlopdManaged.as_str());
}

#[test]
fn run_spawns_executable_in_new_tmux_window() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let output = env.slopctl(&["run"]);

    kill_slopd(slopd);

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

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();

    let kill_output = env.slopctl(&["kill", &pane_id]);

    kill_slopd(slopd);

    assert!(kill_output.status.success(), "slopctl kill failed: {:?}", kill_output);
    let kill_stdout = String::from_utf8_lossy(&kill_output.stdout);
    assert_eq!(kill_stdout.trim(), pane_id, "kill should print the pane_id");
}

#[test]
fn run_does_not_inject_hooks_into_host_claude_settings() {
    build_bin("slopd");
    build_bin("slopctl");

    let host_settings = libslop::home_dir().join(".claude/settings.json");
    let mtime_before = host_settings.metadata().ok().map(|m| m.modified().unwrap());

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    let output = env.slopctl(&["run"]);
    kill_slopd(slopd);

    assert!(output.status.success(), "slopctl run failed: {:?}", output);

    let mtime_after = host_settings.metadata().ok().map(|m| m.modified().unwrap());
    assert_eq!(
        mtime_before, mtime_after,
        "~/.claude/settings.json was modified by the test"
    );
}

#[test]
fn run_without_claude_config_dir_does_not_inject_hooks_into_host_claude_settings() {
    build_bin("slopd");
    build_bin("slopctl");

    let host_settings = libslop::home_dir().join(".claude/settings.json");
    let mtime_before = host_settings.metadata().ok().map(|m| m.modified().unwrap());

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();

    // new_full with claude_config_dir=None: slopd has no configured claude_config_dir,
    // so it would fall back to ~/.claude if HOME is not isolated.
    let Some(env) = TestEnv::new_full(
        Some(&["sleep", "infinity"]),
        Some(&slopctl_path),
        None,
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    let output = env.slopctl(&["run"]);
    kill_slopd(slopd);

    assert!(output.status.success(), "slopctl run failed: {:?}", output);

    let mtime_after = host_settings.metadata().ok().map(|m| m.modified().unwrap());
    assert_eq!(
        mtime_before, mtime_after,
        "~/.claude/settings.json was modified by the test"
    );
}

#[test]
fn run_injects_hooks_into_claude_settings() {
    build_bin("slopd");
    build_bin("slopctl");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(
        Some(&["sleep", "infinity"]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let output = env.slopctl(&["run"]);

    kill_slopd(slopd);

    assert!(output.status.success(), "slopctl run failed: {:?}", output);

    let settings_contents = std::fs::read_to_string(claude_config_dir.join("settings.json"))
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
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(
        Some(&["sleep", "infinity"]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    for _ in 0..2 {
        let slopd = env.spawn_slopd();

        let output = env.slopctl(&["run"]);

        kill_slopd(slopd);

        assert!(output.status.success(), "slopctl run failed: {:?}", output);
        std::thread::sleep(Duration::from_millis(50));
    }

    let settings_contents = std::fs::read_to_string(claude_config_dir.join("settings.json"))
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
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();
    let session_id = env.wait_for_session_start(listener, &pane_id);

    kill_slopd(slopd);

    assert_eq!(session_id, "mock-session-id-1234");
}

#[test]
fn send_delivers_prompt_to_pane() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();

    let send_output = env.slopctl(&["send", &pane_id, "hello from test"]);

    kill_slopd(slopd);

    assert!(send_output.status.success(), "slopctl send failed: {:?}", send_output);
    assert_eq!(send_output.stdout, format!("{}\n", pane_id).as_bytes());
}

#[test]
fn send_concurrent_all_delivered() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let env = Arc::new(env);

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();

    const N: usize = 5;
    let handles: Vec<_> = (0..N)
        .map(|i| {
            let env = env.clone();
            let pane_id = pane_id.clone();
            std::thread::spawn(move || {
                env.slopctl(&["send", &pane_id, &format!("prompt {}", i)])
            })
        })
        .collect();

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    kill_slopd(slopd);

    // If all slopctl send calls succeeded, all prompts were delivered and acknowledged
    // (slopctl send blocks until UserPromptSubmit fires, and slopd serializes sends per pane).
    for (i, output) in results.iter().enumerate() {
        assert!(output.status.success(), "sender {} failed: {:?}", i, output);
    }
}


#[test]
fn ps_lists_panes_with_session_id_and_tags() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();
    env.wait_for_session_start(listener, &pane_id);

    // Add a tag so we can verify it appears in ps output.
    let tag_out = env.slopctl(&["tag", &pane_id, "mytest"]);
    assert!(tag_out.status.success(), "slopctl tag failed: {:?}", tag_out);

    let ps_out = env.slopctl(&["ps"]);
    let ps_json_out = env.slopctl(&["ps", "--json"]);

    kill_slopd(slopd);

    assert!(ps_out.status.success(), "slopctl ps failed: {:?}", ps_out);
    let stdout = String::from_utf8_lossy(&ps_out.stdout);
    assert!(stdout.contains(&pane_id), "ps output missing pane_id {}: {}", pane_id, stdout);
    assert!(stdout.contains("mock-session-id-1234"), "ps output missing session_id: {}", stdout);
    assert!(stdout.contains("mytest"), "ps output missing tag: {}", stdout);
    assert!(stdout.contains("LAST_ACTIVE"), "ps output missing LAST_ACTIVE column header: {}", stdout);
    assert!(stdout.contains("ago") || stdout.contains("now"), "ps output missing time: {}", stdout);
    assert!(!stdout.contains("56 years ago"), "created_at is 0: {}", stdout);

    // Verify created_at and last_active are plausible recent Unix timestamps.
    assert!(ps_json_out.status.success(), "ps --json failed: {:?}", ps_json_out);
    let panes: serde_json::Value = serde_json::from_slice(&ps_json_out.stdout)
        .expect("ps --json is not valid JSON");
    let pane_entry = panes.as_array().unwrap().iter()
        .find(|p| p["pane_id"] == pane_id)
        .unwrap_or_else(|| panic!("pane {} not in ps --json output", pane_id));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let created_at = pane_entry["created_at"].as_u64().expect("created_at is not a u64");
    assert!(created_at > 0, "created_at is 0");
    assert!(created_at <= now, "created_at is in the future: {}", created_at);
    assert!(now - created_at < 60, "created_at is more than 60s ago: {}", created_at);
    let last_active = pane_entry["last_active"].as_u64().expect("last_active is not a u64");
    assert!(last_active > 0, "last_active is 0");
    assert!(last_active <= now, "last_active is in the future: {}", last_active);
    assert!(created_at <= last_active, "created_at ({}) is after last_active ({})", created_at, last_active);
}

#[test]
fn ps_shows_parent_pane() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Launch the parent pane — mock_claude runs inside a real tmux pane, so TMUX_PANE
    // is set automatically by tmux in the child process environment.
    let listener = env.spawn_session_start_listener();
    let parent_out = env.slopctl(&["run"]);
    assert!(parent_out.status.success());
    let parent_pane = String::from_utf8_lossy(&parent_out.stdout).trim().to_string();
    env.wait_for_session_start(listener, &parent_pane);

    // Switch mock_claude to always-submit mode so single Enters work reliably.
    let mode_out = env.slopctl(&["send", &parent_pane, "/newline-mode always-submit"]);
    assert!(mode_out.status.success(), "slopctl send /newline-mode failed: {:?}", mode_out);

    // Ask mock_claude to spawn a child pane. Because it runs inside a tmux pane,
    // TMUX_PANE is set by tmux automatically — no manual env var wiring needed.
    let send_out = env.slopctl(&["send", &parent_pane, "/run"]);
    assert!(send_out.status.success(), "slopctl send /run failed: {:?}", send_out);

    // mock_claude prints "/run:<child_pane_id>" to the pane; capture it.
    let child_pane = {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let out = env.tmux.tmux()
                .args(["capture-pane", "-t", &parent_pane, "-p"])
                .output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some(line) = text.lines().find(|l| l.starts_with("/run:")) {
                break line.trim_start_matches("/run:").trim().to_string();
            }
            assert!(Instant::now() < deadline, "timed out waiting for /run output in pane");
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    let ps_out = env.slopctl(&["ps"]);
    // Verify no stray quote characters in parent_pane_id via JSON output (issue #5).
    let ps_json_out = env.slopctl(&["ps", "--json"]);

    kill_slopd(slopd);

    assert!(ps_out.status.success(), "ps failed: {:?}", ps_out);
    let stdout = String::from_utf8_lossy(&ps_out.stdout);
    let child_line = stdout.lines()
        .find(|l| l.contains(&child_pane))
        .unwrap_or_else(|| panic!("child pane {} not found in ps output:\n{}", child_pane, stdout));
    assert!(child_line.contains(&parent_pane),
        "child row missing parent pane ID {}:\n{}", parent_pane, child_line);
    let parent_line = stdout.lines()
        .find(|l| l.starts_with(&parent_pane))
        .unwrap_or_else(|| panic!("parent pane {} not found in ps output:\n{}", parent_pane, stdout));
    assert!(parent_line.contains('-'),
        "parent row should have '-' for PARENT:\n{}", parent_line);

    assert!(ps_json_out.status.success(), "ps --json failed: {:?}", ps_json_out);
    let panes: serde_json::Value = serde_json::from_slice(&ps_json_out.stdout)
        .expect("ps --json output is not valid JSON");
    let child_entry = panes.as_array().unwrap().iter()
        .find(|p| p["pane_id"] == child_pane)
        .unwrap_or_else(|| panic!("child pane {} not in ps --json output", child_pane));
    assert_eq!(
        child_entry["parent_pane_id"],
        serde_json::Value::String(parent_pane.clone()),
        "parent_pane_id contains stray quotes or wrong value",
    );
}

#[test]
fn send_to_nonexistent_pane_returns_error() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let output = env.slopctl(&["send", "%999", "hello"]);

    kill_slopd(slopd);

    assert!(!output.status.success(), "slopctl send should have failed for non-existent pane");
}

/// Regression test: send to a pane where UserPromptSubmit will never fire must return an error
/// rather than hanging forever.
#[test]
fn send_to_pane_with_broken_hooks_times_out() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();
    env.wait_for_session_start(listener, &pane_id);

    // Switch mock_claude to always-submit mode. Two Enters needed: the first is
    // literal (alternating mode default), the second submits.
    env.tmux.tmux()
        .args(["send-keys", "-t", &pane_id, "/newline-mode always-submit", "Enter", "Enter"])
        .status()
        .expect("failed to send /newline-mode");
    std::thread::sleep(Duration::from_millis(100));

    // Put mock_claude into break-hooks mode: it drains stdin but fires no hooks.
    // Sent directly via tmux (not slopctl) to avoid going through the Send machinery.
    env.tmux.tmux()
        .args(["send-keys", "-t", &pane_id, "/break-hooks", "Enter"])
        .status()
        .expect("failed to send /break-hooks");

    // This send reaches a live pane (send-keys succeeds) but UserPromptSubmit will never fire.
    // Pass a short --timeout so slopd returns an error quickly rather than the test hanging.
    let output = env.slopctl(&["send", &pane_id, "hello", "--timeout", "2"]);

    kill_slopd(slopd);

    assert!(!output.status.success(), "slopctl send should have timed out: {:?}", output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("timed out"), "expected timeout message in stderr: {:?}", stderr);
}

/// Regression test for issue #9: send timeout must fire even against a pane that
/// has no hooks at all (no UserPromptSubmit ever fires). Wall time must be close
/// to --timeout, not infinite.
#[test]
fn send_timeout_fires_on_non_hook_pane() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();

    let start = Instant::now();
    let output = env.slopctl(&["send", &pane_id, "hello", "--timeout", "2"]);
    let elapsed = start.elapsed();

    kill_slopd(slopd);

    assert!(!output.status.success(), "send should have timed out");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("timed out"), "expected timeout message in stderr: {:?}", stderr);
    assert!(elapsed < Duration::from_secs(10),
        "send took {:?}, timer appears to have hung (issue #9)", elapsed);
}

#[test]
fn listen_no_filters_receives_all_events() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let mut listen = Command::new(cargo_bin("slopctl"))
        .args(["listen"])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl listen");

    let stdout = listen.stdout.take().unwrap();
    let mut reader = std::io::BufReader::new(stdout);

    // Read and discard the subscription confirmation line.
    let mut subscribed_line = String::new();
    reader.read_line(&mut subscribed_line).expect("failed to read subscribed line");
    assert!(subscribed_line.contains("subscribed"), "unexpected first line: {:?}", subscribed_line);

    // Fire two different event types.
    let stop_payload = r#"{"session_id":"s1","hook_event_name":"Stop"}"#;
    let out = fire_hook(&env, "Stop", stop_payload, None);
    assert!(out.status.success(), "slopctl hook Stop failed: {:?}", out);

    let prompt_payload = r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"hi"}"#;
    let out = fire_hook(&env, "UserPromptSubmit", prompt_payload, None);
    assert!(out.status.success(), "slopctl hook UserPromptSubmit failed: {:?}", out);

    let mut line1 = String::new();
    reader.read_line(&mut line1).expect("failed to read first event");
    let mut line2 = String::new();
    reader.read_line(&mut line2).expect("failed to read second event");

    kill_child(listen);
    kill_slopd(slopd);

    let ev1: serde_json::Value = serde_json::from_str(line1.trim()).expect("first event not valid JSON");
    let ev2: serde_json::Value = serde_json::from_str(line2.trim()).expect("second event not valid JSON");
    assert_eq!(ev1["event_type"], "Stop");
    assert_eq!(ev2["event_type"], "UserPromptSubmit");
}

#[test]
fn listen_receives_hook_event() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let mut listen = Command::new(cargo_bin("slopctl"))
        .args(["listen", "--hook", "UserPromptSubmit"])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl listen");

    let stdout = listen.stdout.take().unwrap();
    let mut reader = std::io::BufReader::new(stdout);

    // Read and discard the subscription confirmation line.
    let mut subscribed_line = String::new();
    reader.read_line(&mut subscribed_line).expect("failed to read subscribed line");
    assert!(subscribed_line.contains("subscribed"), "unexpected first line: {:?}", subscribed_line);

    let payload = r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"hello"}"#;
    let out = fire_hook(&env, "UserPromptSubmit", payload, None);
    assert!(out.status.success(), "slopctl hook failed: {:?}", out);

    let mut line = String::new();
    reader.read_line(&mut line).expect("failed to read event line");

    kill_child(listen);
    kill_slopd(slopd);

    let event: serde_json::Value = serde_json::from_str(line.trim()).expect("event is not valid JSON");
    assert_eq!(event["event_type"], "UserPromptSubmit");
    assert_eq!(event["source"], "hook");
    assert_eq!(event["payload"]["prompt"], "hello");
}

#[test]
fn listen_filters_out_non_matching_events() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let mut listen = Command::new(cargo_bin("slopctl"))
        .args(["listen", "--hook", "UserPromptSubmit"])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl listen");

    let stdout = listen.stdout.take().unwrap();
    let mut reader = std::io::BufReader::new(stdout);

    // Read and discard the subscription confirmation line.
    let mut subscribed_line = String::new();
    reader.read_line(&mut subscribed_line).expect("failed to read subscribed line");
    assert!(subscribed_line.contains("subscribed"), "unexpected first line: {:?}", subscribed_line);

    // Fire a non-matching event first.
    let stop_payload = r#"{"session_id":"s1","hook_event_name":"Stop"}"#;
    let out = fire_hook(&env, "Stop", stop_payload, None);
    assert!(out.status.success(), "slopctl hook Stop failed: {:?}", out);

    // Then fire the matching event.
    let prompt_payload = r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"world"}"#;
    let out = fire_hook(&env, "UserPromptSubmit", prompt_payload, None);
    assert!(out.status.success(), "slopctl hook UserPromptSubmit failed: {:?}", out);

    let mut line = String::new();
    reader.read_line(&mut line).expect("failed to read event line");

    kill_child(listen);
    kill_slopd(slopd);

    let event: serde_json::Value = serde_json::from_str(line.trim()).expect("event is not valid JSON");
    // The first event received must be the UserPromptSubmit, not Stop.
    assert_eq!(event["event_type"], "UserPromptSubmit");
    assert_eq!(event["payload"]["prompt"], "world");
}

#[test]
fn listen_by_pane_id() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Spawn two managed panes so their IDs are known to slopd.
    let out1 = env.slopctl(&["run"]);
    assert!(out1.status.success(), "first run failed");
    let target_pane = String::from_utf8_lossy(&out1.stdout).trim().to_string();

    let out2 = env.slopctl(&["run"]);
    assert!(out2.status.success(), "second run failed");
    let other_pane = String::from_utf8_lossy(&out2.stdout).trim().to_string();

    let mut listen = Command::new(cargo_bin("slopctl"))
        .args(["listen", "--pane-id", &target_pane])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl listen");

    let stdout = listen.stdout.take().unwrap();
    let mut reader = std::io::BufReader::new(stdout);

    // Read and discard the subscription confirmation line.
    let mut subscribed_line = String::new();
    reader.read_line(&mut subscribed_line).expect("failed to read subscribed line");
    assert!(subscribed_line.contains("subscribed"), "unexpected first line: {:?}", subscribed_line);

    // Fire from the wrong pane first.
    let other_payload = r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"wrong pane"}"#;
    let out = fire_hook(&env, "UserPromptSubmit", other_payload, Some(&other_pane));
    assert!(out.status.success());

    // Then fire from the target pane.
    let target_payload = r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"right pane"}"#;
    let out = fire_hook(&env, "UserPromptSubmit", target_payload, Some(&target_pane));
    assert!(out.status.success());

    let event = read_next_hook_event(&mut reader);

    kill_child(listen);
    kill_slopd(slopd);

    assert_eq!(event["pane_id"], target_pane.as_str());
    assert_eq!(event["payload"]["prompt"], "right pane");
}

#[test]
fn interrupt_exits_mock_claude() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();
    env.wait_for_session_start(listener, &pane_id);

    // Interrupt: sends C-c, C-d, Escape — enough to drop whatever Claude is doing.
    let int_out = env.slopctl(&["interrupt", &pane_id]);
    assert!(int_out.status.success(), "interrupt failed: {:?}", int_out);
    assert_eq!(String::from_utf8_lossy(&int_out.stdout).trim(), pane_id);

    // mock_claude should still be alive — a single interrupt doesn't exit.
    std::thread::sleep(Duration::from_millis(100));
    let pane_alive = env.tmux.tmux()
        .args(["list-panes", "-t", &pane_id, "-F", "#{pane_id}"])
        .output().unwrap();
    assert!(
        String::from_utf8_lossy(&pane_alive.stdout).contains(&pane_id),
        "pane should still be alive after interrupt"
    );

    kill_slopd(slopd);
}

#[test]
fn tag_and_untag_pane() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();

    // Tag the pane.
    let tag_out = env.slopctl(&["tag", &pane_id, "my-tag"]);
    assert!(tag_out.status.success(), "slopctl tag failed: {:?}", tag_out);

    // List tags — should include our tag.
    let tags_out = env.slopctl(&["tags", &pane_id]);
    assert!(tags_out.status.success(), "slopctl tags failed: {:?}", tags_out);
    let tags_stdout = String::from_utf8_lossy(&tags_out.stdout);
    assert!(tags_stdout.lines().any(|l| l == "my-tag"), "tag not listed: {:?}", tags_stdout);

    // Verify the tmux option was set on the pane.
    let opt_out = env.tmux.tmux()
        .args(["show-options", "-t", &pane_id, "-p", "-v",
               &libslop::tag_option_name("my-tag").unwrap()])
        .output().unwrap();
    assert_eq!(String::from_utf8_lossy(&opt_out.stdout).trim(), "1");

    // Untag.
    let untag_out = env.slopctl(&["untag", &pane_id, "my-tag"]);
    assert!(untag_out.status.success(), "slopctl untag failed: {:?}", untag_out);

    // Tags should now be empty.
    let tags_out2 = env.slopctl(&["tags", &pane_id]);
    assert!(tags_out2.status.success());
    let tags_stdout2 = String::from_utf8_lossy(&tags_out2.stdout);
    assert!(!tags_stdout2.lines().any(|l| l == "my-tag"), "tag still listed after untag");

    kill_slopd(slopd);
}

#[test]
fn created_at_survives_slopd_restart() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();

    let ps_out = env.slopctl(&["ps", "--json"]);
    assert!(ps_out.status.success(), "slopctl ps --json failed: {:?}", ps_out);
    let panes: serde_json::Value = serde_json::from_slice(&ps_out.stdout).expect("ps --json is not valid JSON");
    let created_at_before = panes.as_array().unwrap().iter()
        .find(|p| p["pane_id"] == pane_id)
        .unwrap_or_else(|| panic!("pane {} not in ps --json output", pane_id))["created_at"]
        .as_u64()
        .expect("created_at is not a u64");

    kill_slopd(slopd);
    let slopd2 = env.spawn_slopd();

    let ps_out2 = env.slopctl(&["ps", "--json"]);
    assert!(ps_out2.status.success(), "slopctl ps --json failed after restart: {:?}", ps_out2);
    let panes2: serde_json::Value = serde_json::from_slice(&ps_out2.stdout).expect("ps --json is not valid JSON after restart");
    let created_at_after = panes2.as_array().unwrap().iter()
        .find(|p| p["pane_id"] == pane_id)
        .unwrap_or_else(|| panic!("pane {} not in ps --json output after restart", pane_id))["created_at"]
        .as_u64()
        .expect("created_at is not a u64 after restart");

    assert_eq!(created_at_before, created_at_after, "created_at changed after slopd restart");

    kill_slopd(slopd2);
}

#[test]
fn tags_survive_slopd_restart() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();

    let tag_out = env.slopctl(&["tag", &pane_id, "persistent"]);
    assert!(tag_out.status.success(), "slopctl tag failed: {:?}", tag_out);

    // Restart slopd — tmux and the pane keep running.
    kill_slopd(slopd);
    let slopd2 = env.spawn_slopd();

    let tags_out = env.slopctl(&["tags", &pane_id]);
    assert!(tags_out.status.success(), "slopctl tags failed after restart: {:?}", tags_out);
    let tags_stdout = String::from_utf8_lossy(&tags_out.stdout);
    assert!(
        tags_stdout.lines().any(|l| l == "persistent"),
        "tag lost after slopd restart: {:?}",
        tags_stdout,
    );

    kill_slopd(slopd2);
}

#[test]
fn tags_without_pane_id_uses_tmux_pane_env() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();

    let tag_out = env.slopctl(&["tag", &pane_id, "current-pane-tag"]);
    assert!(tag_out.status.success(), "slopctl tag failed: {:?}", tag_out);

    // Run `slopctl tags` without an explicit pane ID but with TMUX_PANE set.
    let tags_out = Command::new(cargo_bin("slopctl"))
        .args(["tags"])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .env("TMUX_PANE", &pane_id)
        .output()
        .expect("failed to run slopctl tags");
    assert!(tags_out.status.success(), "slopctl tags failed: {:?}", tags_out);
    let stdout = String::from_utf8_lossy(&tags_out.stdout);
    assert!(
        stdout.lines().any(|l| l == "current-pane-tag"),
        "expected tag in output: {:?}",
        stdout,
    );

    kill_slopd(slopd);
}

#[test]
fn tags_without_pane_id_and_without_tmux_pane_errors() {
    build_bin("slopctl");

    // Run `slopctl tags` with no pane ID and no TMUX_PANE — should fail.
    let out = Command::new(cargo_bin("slopctl"))
        .args(["tags"])
        .env_remove("TMUX_PANE")
        // XDG_RUNTIME_DIR does not need to point at a live daemon; clap should
        // reject the invocation before any socket connection is attempted.
        .env("XDG_RUNTIME_DIR", "/tmp")
        .output()
        .expect("failed to run slopctl tags");
    assert!(
        !out.status.success(),
        "slopctl tags should fail when PANE_ID is omitted and TMUX_PANE is unset",
    );
}

#[test]
fn tag_invalid_name_returns_error() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let out = env.slopctl(&["tag", "%0", "bad tag!"]);

    kill_slopd(slopd);

    assert!(!out.status.success(), "slopctl tag should fail for invalid tag name");
}

#[test]
fn tag_empty_name_returns_error() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let out = env.slopctl(&["tag", "%0", ""]);

    kill_slopd(slopd);

    assert!(!out.status.success(), "slopctl tag should fail for empty tag name");
}

#[test]
fn send_filtered_one_match() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success());
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();
    env.wait_for_session_start(listener, &pane_id);

    let tag_out = env.slopctl(&["tag", &pane_id, "mytarget"]);
    assert!(tag_out.status.success());

    let send_out = env.slopctl(&["send", "tag=mytarget", "hello from filter"]);

    kill_slopd(slopd);

    assert!(send_out.status.success(), "send failed: {:?}", send_out);
    assert_eq!(String::from_utf8_lossy(&send_out.stdout).trim(), pane_id);
}

#[test]
fn send_filtered_one_errors_on_zero_matches() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let out = env.slopctl(&["send", "tag=nonexistent", "hello"]);

    kill_slopd(slopd);

    assert!(!out.status.success(), "send should fail with no matches");
}

#[test]
fn send_filtered_one_errors_on_multiple_matches() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let pane1 = String::from_utf8_lossy(&env.slopctl(&["run"]).stdout).trim().to_string();
    let pane2 = String::from_utf8_lossy(&env.slopctl(&["run"]).stdout).trim().to_string();

    env.slopctl(&["tag", &pane1, "shared"]);
    env.slopctl(&["tag", &pane2, "shared"]);

    let out = env.slopctl(&["send", "tag=shared", "hello"]);

    kill_slopd(slopd);

    assert!(!out.status.success(), "send --select one should fail with 2 matches");
}

#[test]
fn send_filtered_all_sends_to_all_matching() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let listener = env.spawn_session_start_listener();
    let pane1 = String::from_utf8_lossy(&env.slopctl(&["run"]).stdout).trim().to_string();
    let pane2 = String::from_utf8_lossy(&env.slopctl(&["run"]).stdout).trim().to_string();
    env.wait_for_session_starts(listener, &[&pane1, &pane2]);

    env.slopctl(&["tag", &pane1, "broadcast"]);
    env.slopctl(&["tag", &pane2, "broadcast"]);

    let send_out = env.slopctl(&["send", "tag=broadcast", "hello all", "--select", "all"]);

    kill_slopd(slopd);

    assert!(send_out.status.success(), "send --select all failed: {:?}", send_out);
    let stdout = String::from_utf8_lossy(&send_out.stdout);
    assert!(stdout.contains(&pane1), "output missing pane1 {}: {}", pane1, stdout);
    assert!(stdout.contains(&pane2), "output missing pane2 {}: {}", pane2, stdout);
}

#[test]
fn send_filtered_any_sends_to_exactly_one_pane() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let listener = env.spawn_session_start_listener();
    let pane1 = String::from_utf8_lossy(&env.slopctl(&["run"]).stdout).trim().to_string();
    let pane2 = String::from_utf8_lossy(&env.slopctl(&["run"]).stdout).trim().to_string();
    env.wait_for_session_starts(listener, &[&pane1, &pane2]);

    env.slopctl(&["tag", &pane1, "anytarget"]);
    env.slopctl(&["tag", &pane2, "anytarget"]);

    let send_out = env.slopctl(&["send", "tag=anytarget", "hello any", "--select", "any"]);

    kill_slopd(slopd);

    assert!(send_out.status.success(), "send --select any failed: {:?}", send_out);
    let stdout = String::from_utf8_lossy(&send_out.stdout);
    // Exactly one pane ID should appear in the output.
    let count = stdout.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(count, 1, "expected exactly one pane in output, got: {}", stdout);
    let chosen = stdout.trim();
    assert!(chosen == pane1 || chosen == pane2, "chosen pane {} not one of the tagged panes", chosen);
}

#[test]
fn ps_filter_shows_only_matching_panes() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let pane1 = String::from_utf8_lossy(&env.slopctl(&["run"]).stdout).trim().to_string();
    let pane2 = String::from_utf8_lossy(&env.slopctl(&["run"]).stdout).trim().to_string();

    env.slopctl(&["tag", &pane1, "visible"]);

    let ps_out = env.slopctl(&["ps", "--filter", "tag=visible"]);

    kill_slopd(slopd);

    assert!(ps_out.status.success(), "ps --filter failed: {:?}", ps_out);
    let stdout = String::from_utf8_lossy(&ps_out.stdout);
    assert!(stdout.contains(&pane1), "filtered ps missing tagged pane");
    assert!(!stdout.contains(&pane2), "filtered ps should not show untagged pane");
}

/// Verify that send with --select all delivers to N panes concurrently: total wall time
/// must be less than 2x the single-pane round-trip, not N times it.
#[test]
fn send_filtered_all_is_concurrent() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    const N: usize = 4;
    let listener = env.spawn_session_start_listener();
    let mut pane_ids = Vec::new();
    for _ in 0..N {
        let out = env.slopctl(&["run"]);
        assert!(out.status.success());
        pane_ids.push(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    env.wait_for_session_starts(listener, &pane_ids.iter().map(String::as_str).collect::<Vec<_>>());

    for pane_id in &pane_ids {
        env.slopctl(&["tag", pane_id, "concurrent"]);
    }

    // Measure a single send to one pane to establish a baseline.
    let baseline_start = Instant::now();
    let single = env.slopctl(&["send", &pane_ids[0], "baseline"]);
    assert!(single.status.success());
    let baseline = baseline_start.elapsed();

    // Now send with filters to all N panes and measure wall time.
    let all_start = Instant::now();
    let all_out = env.slopctl(&["send", "tag=concurrent", "hello concurrent",
                                "--select", "all"]);
    let all_elapsed = all_start.elapsed();

    kill_slopd(slopd);

    assert!(all_out.status.success(), "send failed: {:?}", all_out);

    // All N panes received. Wall time should be well under N * baseline.
    // We allow 2x baseline as headroom for scheduling jitter.
    let limit = baseline * 2 + Duration::from_millis(500);
    assert!(
        all_elapsed < limit,
        "send to {} panes took {:?}, expected < {:?} (baseline {:?}); \
         sends are likely sequential not concurrent",
        N, all_elapsed, limit, baseline,
    );
}

/// Run slopctl with the given args (no daemon needed), assert exit code 2, and
/// assert that stderr contains `expected_hint` so the user knows what went wrong.
fn assert_invalid_usage(args: &[&str], expected_hint: &str) {
    build_bin("slopctl");
    let out = Command::new(cargo_bin("slopctl"))
        .args(args)
        .output()
        .expect("failed to run slopctl");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(2),
        "slopctl {:?}: expected exit 2, got {:?}\nstderr: {}",
        args, out.status.code(), stderr,
    );
    assert!(
        stderr.contains(expected_hint),
        "slopctl {:?}: stderr missing {:?}\nstderr: {}",
        args, expected_hint, stderr,
    );
}

#[test]
fn help_no_subcommand() {
    assert_invalid_usage(&[], "Usage:");
}

#[test]
fn help_unknown_subcommand() {
    assert_invalid_usage(&["frobnicate"], "Usage:");
}

#[test]
fn help_kill_missing_pane_id() {
    assert_invalid_usage(&["kill"], "<PANE_ID>");
}

#[test]
fn help_hook_missing_event() {
    assert_invalid_usage(&["hook"], "<EVENT>");
}

#[test]
fn help_send_missing_args() {
    assert_invalid_usage(&["send"], "<PANE_ID>");
}

#[test]
fn help_send_missing_prompt() {
    assert_invalid_usage(&["send", "%1"], "<PROMPT>");
}

#[test]
fn help_interrupt_missing_pane_id() {
    assert_invalid_usage(&["interrupt"], "<PANE_ID>");
}

#[test]
fn help_tag_missing_args() {
    assert_invalid_usage(&["tag"], "<PANE_ID>");
}

#[test]
fn help_tag_missing_tag() {
    assert_invalid_usage(&["tag", "%1"], "<TAG>");
}

#[test]
fn help_untag_missing_args() {
    assert_invalid_usage(&["untag"], "<PANE_ID>");
}

#[test]
fn help_untag_missing_tag() {
    assert_invalid_usage(&["untag", "%1"], "<TAG>");
}

#[test]
fn help_tags_missing_pane_id() {
    assert_invalid_usage(&["tags"], "<PANE_ID>");
}

#[test]
fn help_send_unknown_filter_key() {
    build_bin("slopctl");
    let out = Command::new(cargo_bin("slopctl"))
        .args(["send", "foo=bar", "hello"])
        .output()
        .expect("failed to run slopctl");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "expected exit 1\nstderr: {}", stderr);
    assert!(stderr.contains("foo"), "expected filter key in error\nstderr: {}", stderr);
}

#[test]
fn run_from_pane_sets_parent_pane_attribute() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Spawn the parent pane — mock_claude runs inside a real tmux pane.
    let listener = env.spawn_session_start_listener();
    let parent_out = env.slopctl(&["run"]);
    assert!(parent_out.status.success(), "first run failed: {:?}", parent_out);
    let parent_pane = String::from_utf8_lossy(&parent_out.stdout).trim().to_string();
    env.wait_for_session_start(listener, &parent_pane);

    // Switch mock_claude to always-submit mode so single Enters work reliably.
    let mode_out = env.slopctl(&["send", &parent_pane, "/newline-mode always-submit"]);
    assert!(mode_out.status.success(), "slopctl send /newline-mode failed: {:?}", mode_out);

    // Ask mock_claude to spawn a child. TMUX_PANE is set by tmux in mock_claude's
    // environment, so the child gets @slopd_parent_pane set automatically.
    let send_out = env.slopctl(&["send", &parent_pane, "/run"]);
    assert!(send_out.status.success(), "slopctl send /run failed: {:?}", send_out);

    // mock_claude prints "/run:<child_pane_id>" to the pane; capture it.
    let child_pane = {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let out = env.tmux.tmux()
                .args(["capture-pane", "-t", &parent_pane, "-p"])
                .output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some(line) = text.lines().find(|l| l.starts_with("/run:")) {
                break line.trim_start_matches("/run:").trim().to_string();
            }
            assert!(Instant::now() < deadline, "timed out waiting for /run output in pane");
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    // Verify the child pane has @slopd_parent_pane set to the parent.
    let opt_out = env.tmux.tmux()
        .args(["show-options", "-t", &child_pane, "-p", "-v",
               libslop::TmuxOption::SlopdParentPane.as_str()])
        .output().unwrap();
    let value = String::from_utf8_lossy(&opt_out.stdout).trim().to_string();

    kill_slopd(slopd);

    assert_eq!(value, parent_pane,
        "@slopd_parent_pane on child pane should equal parent pane ID");
}

#[test]
fn run_does_not_set_claude_config_dir_when_not_configured() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    // No claude_config_dir — slopd should not set CLAUDE_CONFIG_DIR in the pane env.
    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        None,
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_out = env.slopctl(&["run"]);
    assert!(run_out.status.success(), "run failed: {:?}", run_out);
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    // mock_claude starts immediately (no hook injection needed — we bypass slopctl send).
    // Give it a moment to enter raw mode before sending keys.
    std::thread::sleep(Duration::from_millis(200));

    // Switch mock_claude to always-submit mode. Two Enters needed: the first is
    // literal (alternating mode default), the second submits.
    env.tmux.tmux()
        .args(["send-keys", "-t", &pane_id, "/newline-mode always-submit", "Enter", "Enter"])
        .status().unwrap();
    std::thread::sleep(Duration::from_millis(100));

    // Send keys directly via tmux (bypasses slopctl send / UserPromptSubmit hook).
    env.tmux.tmux()
        .args(["send-keys", "-t", &pane_id, "/env CLAUDE_CONFIG_DIR", "Enter"])
        .status().unwrap();

    // Poll pane output for the /env response.
    let deadline = Instant::now() + Duration::from_secs(5);
    let env_line = loop {
        let out = env.tmux.tmux()
            .args(["capture-pane", "-t", &pane_id, "-p"])
            .output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        // tmux may wrap long lines; join the full output before searching.
        let joined = text.replace('\n', "").replace('\r', "");
        if let Some(pos) = joined.find("/env:CLAUDE_CONFIG_DIR=") {
            break joined[pos..].split_whitespace().next().unwrap_or("").to_string();
        }
        assert!(Instant::now() < deadline, "timed out waiting for /env output");
        std::thread::sleep(Duration::from_millis(50));
    };

    kill_slopd(slopd);

    assert_eq!(env_line, "/env:CLAUDE_CONFIG_DIR=UNSET",
        "CLAUDE_CONFIG_DIR should not be set when no custom dir is configured");
}

#[test]
fn run_without_tmux_pane_has_no_parent_attribute() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // env.slopctl does not set TMUX_PANE, simulating a user-initiated run.
    let out = env.slopctl(&["run"]);
    assert!(out.status.success(), "run failed: {:?}", out);
    let pane_id = String::from_utf8_lossy(&out.stdout).trim().to_string();

    let opt_out = env.tmux.tmux()
        .args(["show-options", "-t", &pane_id, "-p", "-v",
               libslop::TmuxOption::SlopdParentPane.as_str()])
        .output().unwrap();
    let value = String::from_utf8_lossy(&opt_out.stdout).trim().to_string();

    kill_slopd(slopd);

    assert!(value.is_empty(), "@slopd_parent_pane should not be set for user-initiated run, got {:?}", value);
}

/// Verify that extra args passed via `slopctl run -- <args>` are forwarded to the executable.
/// mock_claude --print exits immediately without entering the interactive loop.
#[test]
fn run_extra_args_print_exits_immediately() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();
    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");

    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Set remain-on-exit so we can inspect the pane after mock_claude exits.
    env.tmux.tmux()
        .args(["set-option", "-t", "slopd", "-g", "remain-on-exit", "on"])
        .status().unwrap();

    let run_out = env.slopctl(&["run", "--", "--print", "hello"]);
    assert!(run_out.status.success(), "slopctl run failed: {:?}", run_out);
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    // mock_claude --print should exit quickly. Poll until the pane is dead.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let out = env.tmux.tmux()
            .args(["capture-pane", "-t", &pane_id, "-p"])
            .output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        if text.contains("Pane is dead") {
            break;
        }
        assert!(Instant::now() < deadline, "timed out waiting for pane to exit");
        std::thread::sleep(Duration::from_millis(50));
    }

    kill_slopd(slopd);
}

/// Verify that /echo command in mock_claude prints the argument back.
#[test]
fn echo_command_prints_output() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();
    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");

    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let listener = env.spawn_session_start_listener();
    let run_out = env.slopctl(&["run"]);
    assert!(run_out.status.success(), "slopctl run failed: {:?}", run_out);
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();
    env.wait_for_session_start(listener, &pane_id);

    let send_out = env.slopctl(&["send", &pane_id, "/echo hello-from-echo"]);
    assert!(send_out.status.success(), "slopctl send failed: {:?}", send_out);

    // Poll pane output for the echo response.
    let deadline = Instant::now() + Duration::from_secs(5);
    let found = loop {
        let out = env.tmux.tmux()
            .args(["capture-pane", "-t", &pane_id, "-p"])
            .output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        if text.lines().any(|l| l.contains("hello-from-echo")) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    kill_slopd(slopd);

    assert!(found, "expected 'hello-from-echo' in pane output");
}

/// When a Claude instance outside of slopd's managed session has `slopctl hook` configured
/// (e.g. because it shares the same settings.json), its hook events should NOT be dispatched
/// to subscribers as if they came from a managed pane.
#[test]
fn hook_from_unmanaged_pane_is_not_dispatched() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Spawn a managed pane so that hooks get injected into settings.json.
    let listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let managed_pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();
    env.wait_for_session_start(listener, &managed_pane_id);

    // Now spawn an *unmanaged* mock_claude in the "test" session (not the "slopd" session).
    // It will read the same settings.json with the injected hooks and fire SessionStart
    // on startup, sending hook events to slopd even though it is not managed.
    let unmanaged_out = env.tmux.tmux()
        .args([
            "new-window", "-t", "test", "-P", "-F", "#{pane_id}",
            &mock_claude_path,
        ])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .env("CLAUDE_CONFIG_DIR", &claude_config_dir)
        .output()
        .expect("failed to spawn unmanaged mock_claude pane");
    assert!(unmanaged_out.status.success(), "failed to create unmanaged pane: {:?}", unmanaged_out);
    let unmanaged_pane_id = String::from_utf8_lossy(&unmanaged_out.stdout).trim().to_string();

    // Start a listener that receives all events (no filters).
    let mut listen = Command::new(cargo_bin("slopctl"))
        .args(["listen"])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl listen");

    let stdout = listen.stdout.take().unwrap();
    let mut reader = std::io::BufReader::new(stdout);

    // Read and discard the subscription confirmation line.
    let mut subscribed_line = String::new();
    reader.read_line(&mut subscribed_line).expect("failed to read subscribed line");
    assert!(subscribed_line.contains("subscribed"), "unexpected first line: {:?}", subscribed_line);

    // Fire a hook event pretending to come from the unmanaged pane.
    let payload = format!(
        r#"{{"session_id":"unmanaged-session","hook_event_name":"UserPromptSubmit","prompt":"from outside"}}"#
    );
    let hook_out = fire_hook(&env, "UserPromptSubmit", &payload, Some(&unmanaged_pane_id));
    assert!(hook_out.status.success(), "hook from unmanaged pane failed: {:?}", hook_out);

    // Also fire from the managed pane so the listener has something to read
    // (if the unmanaged event is correctly suppressed).
    let managed_payload = r#"{"session_id":"mock-session-id-1234","hook_event_name":"UserPromptSubmit","prompt":"from managed"}"#;
    let hook_out = fire_hook(&env, "UserPromptSubmit", managed_payload, Some(&managed_pane_id));
    assert!(hook_out.status.success(), "hook from managed pane failed: {:?}", hook_out);

    let event = read_next_hook_event(&mut reader);

    kill_child(listen);
    kill_slopd(slopd);

    // The event from the unmanaged pane should have been silently dropped.
    // The first hook event we receive must be from the managed pane.
    assert_eq!(
        event["pane_id"].as_str().unwrap(), managed_pane_id,
        "Expected slopd to ignore the unmanaged pane's event, but got pane_id={:?}",
        event["pane_id"],
    );
    assert_eq!(event["payload"]["prompt"], "from managed");
}

/// Panes created before a slopd restart must still be recognized as managed.
/// Hooks fired from those panes should still be dispatched to subscribers.
#[test]
fn hooks_from_managed_pane_work_after_slopd_restart() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();

    // Restart slopd — the tmux session and pane survive.
    kill_slopd(slopd);
    let slopd2 = env.spawn_slopd();

    // Start a listener.
    let mut listen = Command::new(cargo_bin("slopctl"))
        .args(["listen"])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl listen");

    let stdout = listen.stdout.take().unwrap();
    let mut reader = std::io::BufReader::new(stdout);

    // Read and discard the subscription confirmation line.
    let mut subscribed_line = String::new();
    reader.read_line(&mut subscribed_line).expect("failed to read subscribed line");
    assert!(subscribed_line.contains("subscribed"), "unexpected first line: {:?}", subscribed_line);

    // Fire a hook from the pre-existing managed pane.
    let payload = r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"after restart"}"#;
    let hook_out = fire_hook(&env, "UserPromptSubmit", payload, Some(&pane_id));
    assert!(hook_out.status.success(), "hook failed: {:?}", hook_out);

    let event = read_next_hook_event(&mut reader);

    kill_child(listen);
    kill_slopd(slopd2);

    assert_eq!(event["pane_id"], pane_id.as_str());
    assert_eq!(event["payload"]["prompt"], "after restart");
}

/// Read lines from a BufReader until a hook event (source == "hook") is found and return it.
/// Skips slopd-internal events (StateChange, DetailedStateChange) which may arrive interleaved.
fn read_next_hook_event(reader: &mut std::io::BufReader<impl std::io::Read>) -> serde_json::Value {
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("failed to read event line");
        let v: serde_json::Value = serde_json::from_str(line.trim()).expect("event is not valid JSON");
        if v["source"] == "hook" {
            return v;
        }
    }
}

/// Helper: fire a hook for a pane and assert the resulting (state, detailed_state).
fn assert_state_after_hook(
    env: &libsloptest::TestEnv,
    pane_id: &str,
    event: &str,
    payload: &str,
    expected_state: libslop::PaneState,
    expected_detailed: libslop::PaneDetailedState,
) {
    let out = fire_hook(env, event, payload, Some(pane_id));
    assert!(out.status.success(), "hook {} failed: {:?}", event, out);
    // Give slopd a moment to write the tmux option.
    std::thread::sleep(Duration::from_millis(100));
    let (state, detailed) = env.pane_state(pane_id);
    assert_eq!(state, expected_state, "state mismatch after {} hook", event);
    assert_eq!(detailed, expected_detailed, "detailed_state mismatch after {} hook", event);
}

#[test]
fn pane_state_booting_up_on_run_then_transitions_on_hooks() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    // --no-session-start prevents mock_claude from firing SessionStart on startup,
    // so we can assert booting_up before any hook fires.
    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path, "--no-session-start"]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();

    // mock_claude is running but has not fired SessionStart: state must be booting_up
    let (state, detailed) = env.pane_state(&pane_id);
    assert_eq!(state, libslop::PaneState::BootingUp);
    assert_eq!(detailed, libslop::PaneDetailedState::BootingUp);

    // Ask mock_claude to fire SessionStart via its /hook command.
    // mock_claude fires SessionStart (synchronously, wait=true) then falls through to
    // UserPromptSubmit. slopctl send blocks until UserPromptSubmit is confirmed, so
    // when it returns slopd has processed both hooks.
    let send_out = env.slopctl(&["send", &pane_id, "/hook SessionStart"]);
    assert!(send_out.status.success(), "slopctl send /hook SessionStart failed: {:?}", send_out);

    // SessionStart → Ready, then UserPromptSubmit → BusyProcessing.
    // BusyProcessing confirms SessionStart was processed (state passed through Ready).
    let (state, detailed) = env.pane_state(&pane_id);
    assert_eq!(state, libslop::PaneState::Busy);
    assert_eq!(detailed, libslop::PaneDetailedState::BusyProcessing);

    kill_slopd(slopd);
}

#[test]
fn pane_state_transitions_through_all_hooks() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();

    let base = |hook: &str| format!(
        r#"{{"session_id":"s1","hook_event_name":"{}","transcript_path":"/dev/null","cwd":"/tmp"}}"#,
        hook
    );

    // SessionStart → ready
    assert_state_after_hook(&env, &pane_id, "SessionStart", &base("SessionStart"),
        libslop::PaneState::Ready, libslop::PaneDetailedState::Ready);

    // UserPromptSubmit → busy / busy_processing
    assert_state_after_hook(&env, &pane_id, "UserPromptSubmit", &base("UserPromptSubmit"),
        libslop::PaneState::Busy, libslop::PaneDetailedState::BusyProcessing);

    // PreToolUse → busy / busy_tool_use
    assert_state_after_hook(&env, &pane_id, "PreToolUse", &base("PreToolUse"),
        libslop::PaneState::Busy, libslop::PaneDetailedState::BusyToolUse);

    // PermissionRequest → awaiting_input / awaiting_input_permission
    assert_state_after_hook(&env, &pane_id, "PermissionRequest", &base("PermissionRequest"),
        libslop::PaneState::AwaitingInput, libslop::PaneDetailedState::AwaitingInputPermission);

    // PostToolUse → busy / busy_processing
    assert_state_after_hook(&env, &pane_id, "PostToolUse", &base("PostToolUse"),
        libslop::PaneState::Busy, libslop::PaneDetailedState::BusyProcessing);

    // Elicitation → awaiting_input / awaiting_input_elicitation
    assert_state_after_hook(&env, &pane_id, "Elicitation", &base("Elicitation"),
        libslop::PaneState::AwaitingInput, libslop::PaneDetailedState::AwaitingInputElicitation);

    // ElicitationResult → busy / busy_processing
    assert_state_after_hook(&env, &pane_id, "ElicitationResult", &base("ElicitationResult"),
        libslop::PaneState::Busy, libslop::PaneDetailedState::BusyProcessing);

    // SubagentStart → busy / busy_subagent
    assert_state_after_hook(&env, &pane_id, "SubagentStart", &base("SubagentStart"),
        libslop::PaneState::Busy, libslop::PaneDetailedState::BusySubagent);

    // SubagentStop → busy / busy_processing
    assert_state_after_hook(&env, &pane_id, "SubagentStop", &base("SubagentStop"),
        libslop::PaneState::Busy, libslop::PaneDetailedState::BusyProcessing);

    // PreCompact → busy / busy_compacting
    assert_state_after_hook(&env, &pane_id, "PreCompact", &base("PreCompact"),
        libslop::PaneState::Busy, libslop::PaneDetailedState::BusyCompacting);

    // PostCompact → busy / busy_processing
    assert_state_after_hook(&env, &pane_id, "PostCompact", &base("PostCompact"),
        libslop::PaneState::Busy, libslop::PaneDetailedState::BusyProcessing);

    // Stop → ready
    assert_state_after_hook(&env, &pane_id, "Stop", &base("Stop"),
        libslop::PaneState::Ready, libslop::PaneDetailedState::Ready);

    // StopFailure → ready
    assert_state_after_hook(&env, &pane_id, "StopFailure", &base("StopFailure"),
        libslop::PaneState::Ready, libslop::PaneDetailedState::Ready);

    kill_slopd(slopd);
}

#[test]
fn pane_state_resets_to_booting_up_on_slopd_restart() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();

    // Advance to ready via SessionStart
    let payload = r#"{"session_id":"s1","hook_event_name":"SessionStart","transcript_path":"/dev/null","cwd":"/tmp"}"#;
    assert_state_after_hook(&env, &pane_id, "SessionStart", payload,
        libslop::PaneState::Ready, libslop::PaneDetailedState::Ready);

    // Restart slopd
    kill_slopd(slopd);
    let slopd2 = env.spawn_slopd();

    // State must be reset to booting_up
    std::thread::sleep(Duration::from_millis(100));
    let (state, detailed) = env.pane_state(&pane_id);
    assert_eq!(state, libslop::PaneState::BootingUp, "expected booting_up after restart");
    assert_eq!(detailed, libslop::PaneDetailedState::BootingUp, "expected booting_up after restart");

    // Fire SessionStart again to confirm normal transitions still work after restart
    assert_state_after_hook(&env, &pane_id, "SessionStart", payload,
        libslop::PaneState::Ready, libslop::PaneDetailedState::Ready);

    kill_slopd(slopd2);
}

/// Spawn `slopctl listen --event <event_type>` and wait for the subscription confirmation.
/// Returns the child process with stdout piped.
fn spawn_event_listener(env: &TestEnv, event_type: &str) -> std::process::Child {
    let mut child = Command::new(cargo_bin("slopctl"))
        .args(["listen", "--event", event_type])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl listen --event");
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

/// Read lines from a listener child until a line whose parsed JSON satisfies `pred`, or panic after 10s.
fn wait_for_event<F>(mut listener: std::process::Child, pred: F) -> serde_json::Value
where
    F: Fn(&serde_json::Value) -> bool + Send + 'static,
{
    use std::io::BufRead;
    let stdout = listener.stdout.take().expect("listener has no stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => { let _ = tx.send(None); return; }
                Ok(_) => {}
            }
            let v: serde_json::Value = match serde_json::from_str(line.trim()) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if pred(&v) {
                let _ = tx.send(Some(v));
                return;
            }
        }
    });
    let event = rx.recv_timeout(Duration::from_secs(10))
        .expect("timed out waiting for event")
        .expect("listener closed before matching event");
    kill_child(listener);
    event
}

#[test]
fn listen_event_state_change_fires_on_hook() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();

    let listener = spawn_event_listener(&env, "StateChange");

    let payload = r#"{"session_id":"s1","hook_event_name":"SessionStart","transcript_path":"/dev/null","cwd":"/tmp"}"#;
    let out = fire_hook(&env, "SessionStart", payload, Some(&pane_id));
    assert!(out.status.success(), "hook failed: {:?}", out);

    let event = wait_for_event(listener, move |v| {
        v["event_type"] == "StateChange" && v["pane_id"] == pane_id.as_str()
    });

    assert_eq!(event["source"], "slopd");
    assert_eq!(event["event_type"], "StateChange");
    assert_eq!(event["payload"]["state"], "ready");

    kill_slopd(slopd);
}

#[test]
fn listen_event_detailed_state_change_fires_on_hook() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();

    let listener = spawn_event_listener(&env, "DetailedStateChange");

    let payload = r#"{"session_id":"s1","hook_event_name":"PreToolUse","transcript_path":"/dev/null","cwd":"/tmp"}"#;
    let out = fire_hook(&env, "PreToolUse", payload, Some(&pane_id));
    assert!(out.status.success(), "hook failed: {:?}", out);

    let event = wait_for_event(listener, move |v| {
        v["event_type"] == "DetailedStateChange" && v["pane_id"] == pane_id.as_str()
    });

    assert_eq!(event["source"], "slopd");
    assert_eq!(event["event_type"], "DetailedStateChange");
    assert_eq!(event["payload"]["detailed_state"], "busy_tool_use");

    kill_slopd(slopd);
}

/// Spawn `slopctl listen --hook <event_type>` and wait for the subscription confirmation.
fn spawn_hook_listener(env: &TestEnv, event_type: &str) -> std::process::Child {
    let mut child = Command::new(cargo_bin("slopctl"))
        .args(["listen", "--hook", event_type])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl listen --hook");
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

#[test]
fn listen_hook_delivers_hook_event() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();

    let listener = spawn_hook_listener(&env, "UserPromptSubmit");

    let payload = r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","transcript_path":"/dev/null","cwd":"/tmp","prompt":"hello"}"#;
    let out = fire_hook(&env, "UserPromptSubmit", payload, Some(&pane_id));
    assert!(out.status.success(), "hook failed: {:?}", out);

    let event = wait_for_event(listener, move |v| {
        v["event_type"] == "UserPromptSubmit" && v["pane_id"] == pane_id.as_str()
    });

    assert_eq!(event["source"], "hook");
    assert_eq!(event["event_type"], "UserPromptSubmit");
    assert_eq!(event["payload"]["prompt"], "hello");

    kill_slopd(slopd);
}
