use libsloptest::{build_bin, cargo_bin, kill_slopd, tempfile, TestEnv};
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

    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();

    let deadline = Instant::now() + Duration::from_secs(5);
    let session_id = loop {
        let out = env.tmux.tmux()
            .args(["show-options", "-t", &pane_id, "-p", "-v", libslop::TmuxOption::SlopdClaudeSessionId.as_str()])
            .output()
            .expect("failed to run tmux show-options");
        let val = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !val.is_empty() {
            break val;
        }
        if Instant::now() > deadline {
            kill_slopd(slopd);
            panic!("timed out waiting for @claude_session_id on pane {}", pane_id);
        }
        std::thread::sleep(Duration::from_millis(50));
    };

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

    // Capture the pane scrollback to verify every prompt was actually delivered.
    let capture = env.tmux.tmux()
        .args(["capture-pane", "-t", &pane_id, "-p"])
        .output()
        .expect("failed to run tmux capture-pane");
    let pane_text = String::from_utf8_lossy(&capture.stdout);

    kill_slopd(slopd);

    for (i, output) in results.iter().enumerate() {
        assert!(output.status.success(), "sender {} failed: {:?}", i, output);
    }
    for i in 0..N {
        let prompt = format!("prompt {}", i);
        assert!(pane_text.contains(&prompt), "pane missing {:?}, pane contents:\n{}", prompt, pane_text);
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

    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();

    // Wait for SessionStart so session_id is set on the pane.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let out = env.tmux.tmux()
            .args(["show-options", "-t", &pane_id, "-p", "-v", libslop::TmuxOption::SlopdClaudeSessionId.as_str()])
            .output().unwrap();
        if !String::from_utf8_lossy(&out.stdout).trim().is_empty() { break; }
        assert!(Instant::now() < deadline, "timed out waiting for SessionStart");
        std::thread::sleep(Duration::from_millis(50));
    }

    // Add a tag so we can verify it appears in ps output.
    let tag_out = env.slopctl(&["tag", &pane_id, "mytest"]);
    assert!(tag_out.status.success(), "slopctl tag failed: {:?}", tag_out);

    let ps_out = env.slopctl(&["ps"]);

    kill_slopd(slopd);

    assert!(ps_out.status.success(), "slopctl ps failed: {:?}", ps_out);
    let stdout = String::from_utf8_lossy(&ps_out.stdout);
    assert!(stdout.contains(&pane_id), "ps output missing pane_id {}: {}", pane_id, stdout);
    assert!(stdout.contains("mock-session-id-1234"), "ps output missing session_id: {}", stdout);
    assert!(stdout.contains("mytest"), "ps output missing tag: {}", stdout);
    assert!(stdout.contains("ago"), "ps output missing created time: {}", stdout);
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

    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();

    // Wait for SessionStart so mock_claude is in its prompt-reading loop.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let out = env.tmux.tmux()
            .args(["show-options", "-t", &pane_id, "-p", "-v", libslop::TmuxOption::SlopdClaudeSessionId.as_str()])
            .output()
            .expect("failed to run tmux show-options");
        if !String::from_utf8_lossy(&out.stdout).trim().is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "timed out waiting for SessionStart");
        std::thread::sleep(Duration::from_millis(50));
    }

    // Put mock_claude into break-hooks mode: it drains stdin but fires no hooks.
    // Sent directly via tmux (not slopctl) to avoid going through the Send machinery.
    env.tmux.tmux()
        .args(["send-keys", "-t", &pane_id, "/break-hooks", "Enter"])
        .status()
        .expect("failed to send /break-hooks");

    // This send reaches a live pane (send-keys succeeds) but UserPromptSubmit will never fire.
    // Pass a short --timeout so slopd returns an error quickly rather than the test hanging.
    let output = env.slopctl(&["send", "--timeout", "2", &pane_id, "hello"]);

    kill_slopd(slopd);

    assert!(!output.status.success(), "slopctl send should have timed out: {:?}", output);
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

    // Give the subscription time to be established.
    std::thread::sleep(Duration::from_millis(100));

    let payload = r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"hello"}"#;
    let out = fire_hook(&env, "UserPromptSubmit", payload, None);
    assert!(out.status.success(), "slopctl hook failed: {:?}", out);

    let stdout = listen.stdout.take().unwrap();
    let mut reader = std::io::BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("failed to read event line");

    listen.kill().unwrap();
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

    std::thread::sleep(Duration::from_millis(100));

    // Fire a non-matching event first.
    let stop_payload = r#"{"session_id":"s1","hook_event_name":"Stop"}"#;
    let out = fire_hook(&env, "Stop", stop_payload, None);
    assert!(out.status.success(), "slopctl hook Stop failed: {:?}", out);

    // Then fire the matching event.
    let prompt_payload = r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"world"}"#;
    let out = fire_hook(&env, "UserPromptSubmit", prompt_payload, None);
    assert!(out.status.success(), "slopctl hook UserPromptSubmit failed: {:?}", out);

    let stdout = listen.stdout.take().unwrap();
    let mut reader = std::io::BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("failed to read event line");

    listen.kill().unwrap();
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

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let target_pane = "%42";
    let other_pane = "%99";

    let mut listen = Command::new(cargo_bin("slopctl"))
        .args(["listen", "--pane-id", target_pane])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl listen");

    std::thread::sleep(Duration::from_millis(100));

    // Fire from the wrong pane first.
    let other_payload = r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"wrong pane"}"#;
    let out = fire_hook(&env, "UserPromptSubmit", other_payload, Some(other_pane));
    assert!(out.status.success());

    // Then fire from the target pane.
    let target_payload = r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"right pane"}"#;
    let out = fire_hook(&env, "UserPromptSubmit", target_payload, Some(target_pane));
    assert!(out.status.success());

    let stdout = listen.stdout.take().unwrap();
    let mut reader = std::io::BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("failed to read event line");

    listen.kill().unwrap();
    kill_slopd(slopd);

    let event: serde_json::Value = serde_json::from_str(line.trim()).expect("event is not valid JSON");
    assert_eq!(event["pane_id"], target_pane);
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

    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();

    // Wait for mock_claude to start (SessionStart hook fires).
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let out = env.tmux.tmux()
            .args(["show-options", "-t", &pane_id, "-p", "-v",
                   libslop::TmuxOption::SlopdClaudeSessionId.as_str()])
            .output().unwrap();
        if !String::from_utf8_lossy(&out.stdout).trim().is_empty() {
            break;
        }
        if Instant::now() > deadline {
            kill_slopd(slopd);
            panic!("timed out waiting for mock_claude to start");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

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
