use libsloptest::{
    TestEnv, build_bin, cargo_bin, kill_child, kill_slopd, sighup_pid, sigint_child, tempfile,
};
use std::io::BufRead;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Fire a hook event by calling slopctl hook with the given JSON payload on stdin.
fn fire_hook(
    env: &TestEnv,
    event: &str,
    payload: &str,
    pane_id: Option<&str>,
) -> std::process::Output {
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
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn tmux_available() -> bool {
    match Command::new("tmux").arg("-V").status() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => panic!("unexpected error checking for tmux: {}", e),
        Ok(_) => true,
    }
}

/// Drive mock_claude's `::mock env KEY` debug command in `pane_id` and return the
/// value mock_claude observed for `key`. Panics if the response does not
/// arrive within 5 seconds.
fn read_pane_env(env: &TestEnv, pane_id: &str, key: &str) -> String {
    // mock_claude starts in alternating newline mode; one Enter is literal and
    // the second submits. Switch to always-submit first.
    env.tmux
        .tmux()
        .args([
            "send-keys",
            "-t",
            pane_id,
            "::mock input-mode always-submit",
            "Enter",
            "Enter",
        ])
        .status()
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));

    env.tmux
        .tmux()
        .args([
            "send-keys",
            "-t",
            pane_id,
            &format!("::mock env {}", key),
            "Enter",
        ])
        .status()
        .unwrap();

    let needle = format!("::mock env {}=", key);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let out = env
            .tmux
            .tmux()
            .args(["capture-pane", "-t", pane_id, "-p"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let joined = text.replace(['\n', '\r'], "");
        if let Some(pos) = joined.find(&needle) {
            let tail = &joined[pos + needle.len()..];
            let value = tail.split_whitespace().next().unwrap_or("").to_string();
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for ::mock env {} response; pane: {:?}",
            key,
            text
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Run slopctl against the test env, optionally forwarding extra env vars to
/// slopctl itself (so `--env KEY=${VAR}` expansion can resolve).
fn slopctl_with_env(
    env: &TestEnv,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> std::process::Output {
    // Keep `run` fire-and-forget for these legacy callers (matches env.slopctl).
    let args = libsloptest::legacy_run_args(args);
    let mut cmd = Command::new(cargo_bin("slopctl"));
    cmd.args(args.as_slice())
        // Don't leak an ambient $TMUX_PANE into a user-initiated run (matches
        // env.slopctl); see TestEnv::slopctl_raw.
        .env_remove("TMUX_PANE")
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.output().expect("failed to run slopctl")
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
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let status = child.wait_with_output().unwrap().status;

    assert_ne!(
        status.code(),
        Some(2),
        "hook must never exit 2 (would block Claude action)"
    );
    assert_ne!(
        status.code(),
        Some(0),
        "hook should exit non-zero on error (slopd unreachable)"
    );
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
    assert!(
        !status2.success(),
        "second slopd instance should have failed"
    );
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
    )
    .unwrap();

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

    let session_exists = env
        .tmux
        .tmux()
        .args(["has-session", "-t", "slopd"])
        .status()
        .expect("failed to run tmux has-session")
        .success();

    let option_output = env
        .tmux
        .tmux()
        .args([
            "show-options",
            "-t",
            "slopd",
            "-v",
            libslop::TmuxOption::SlopdManaged.as_str(),
        ])
        .output()
        .expect("failed to run tmux show-options");
    let option_value = String::from_utf8_lossy(&option_output.stdout);

    kill_slopd(slopd);

    assert!(session_exists, "slopd tmux session does not exist");
    assert_eq!(
        option_value.trim(),
        "true",
        "{} option not set correctly",
        libslop::TmuxOption::SlopdManaged.as_str()
    );
}

#[test]
fn slopd_reuses_existing_slopd_session_without_attaching() {
    build_bin("slopd");

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    // Pre-create the slopd session so it already exists when slopd starts.
    let status = env
        .tmux
        .tmux()
        .args(["new-session", "-d", "-s", "slopd"])
        .status()
        .expect("failed to pre-create slopd session");
    assert!(status.success(), "failed to pre-create slopd session");

    // slopd must start successfully even though the session already exists.
    // Before the fix, `new-session -A` would attach to the terminal, causing
    // slopd to hang instead of running in the background.
    let mut slopd = env.spawn_slopd();

    let still_running = slopd.try_wait().unwrap().is_none();
    kill_slopd(slopd);

    assert!(
        still_running,
        "slopd should keep running when the slopd session already exists"
    );
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
    assert!(
        stdout.trim().starts_with('%'),
        "expected pane_id in output, got: {}",
        stdout
    );
}

#[test]
fn run_uses_start_directory_from_config() {
    build_bin("slopd");
    build_bin("slopctl");

    let work_dir = tempfile::tempdir().unwrap();

    let Some(env) = TestEnv::new_with_start_directory(
        Some(&["sleep", "infinity"]),
        work_dir.path().to_str().unwrap(),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    let output = env.slopctl(&["run"]);
    let pane_id = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // Give the window a moment to start
    std::thread::sleep(Duration::from_millis(200));

    let cwd_output = env
        .tmux
        .tmux()
        .args([
            "display-message",
            "-p",
            "-t",
            &pane_id,
            "#{pane_current_path}",
        ])
        .output()
        .expect("failed to run tmux display-message");

    kill_slopd(slopd);

    assert!(output.status.success(), "slopctl run failed: {:?}", output);
    let cwd = String::from_utf8_lossy(&cwd_output.stdout)
        .trim()
        .to_string();
    assert_eq!(
        std::fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone().into()),
        std::fs::canonicalize(work_dir.path()).unwrap(),
        "pane working directory should match config start_directory"
    );
}

#[test]
fn run_uses_start_directory_from_flag() {
    build_bin("slopd");
    build_bin("slopctl");

    let work_dir = tempfile::tempdir().unwrap();

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    let output = env.slopctl(&["run", "-c", work_dir.path().to_str().unwrap()]);
    let pane_id = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // Give the window a moment to start
    std::thread::sleep(Duration::from_millis(200));

    let cwd_output = env
        .tmux
        .tmux()
        .args([
            "display-message",
            "-p",
            "-t",
            &pane_id,
            "#{pane_current_path}",
        ])
        .output()
        .expect("failed to run tmux display-message");

    kill_slopd(slopd);

    assert!(output.status.success(), "slopctl run failed: {:?}", output);
    let cwd = String::from_utf8_lossy(&cwd_output.stdout)
        .trim()
        .to_string();
    assert_eq!(
        std::fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone().into()),
        std::fs::canonicalize(work_dir.path()).unwrap(),
        "pane working directory should match --start-directory flag"
    );
}

/// A relative `-c` path (e.g. `.`) must resolve against the directory where
/// `slopctl` was invoked, not against slopd's cwd/home. Regression test for
/// `slopctl run -c .` landing the pane in `~` instead of the caller's cwd.
#[test]
fn run_resolves_relative_start_directory_against_slopctl_cwd() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let work_dir = tempfile::tempdir().unwrap();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new(Some(&[&mock_claude_path])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Invoke `slopctl run -c .` with slopctl's working directory set to work_dir,
    // so `.` should resolve to work_dir — not slopd's cwd/home.
    let output = Command::new(cargo_bin("slopctl"))
        .args(["run", "--no-wait", "-c", "."])
        .current_dir(work_dir.path())
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("TMUX_TMPDIR")
        .env_remove("TMPDIR")
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .output()
        .expect("failed to run slopctl");
    assert!(output.status.success(), "slopctl run failed: {:?}", output);
    let pane_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(!pane_id.is_empty(), "slopctl run returned empty pane_id");

    enable_always_submit(&env, &pane_id);
    let cwd = query_pane_cwd(&env, &pane_id);

    kill_slopd(slopd);

    assert_eq!(
        std::fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone().into()),
        std::fs::canonicalize(work_dir.path()).unwrap(),
        "mock_claude cwd should match the relative -c path resolved against slopctl's cwd"
    );
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
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    let kill_output = env.slopctl(&["kill", &pane_id]);

    kill_slopd(slopd);

    assert!(
        kill_output.status.success(),
        "slopctl kill failed: {:?}",
        kill_output
    );
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
    let Some(env) = TestEnv::new_full(Some(&["sleep", "infinity"]), Some(&slopctl_path), None)
    else {
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
    assert!(output.status.success(), "slopctl run failed: {:?}", output);

    // Check hooks while slopd is still running (it removes them on exit).
    let settings_contents = std::fs::read_to_string(claude_config_dir.join("settings.json"))
        .expect("settings.json was not created");
    let settings: serde_json::Value =
        serde_json::from_str(&settings_contents).expect("settings.json is not valid JSON");

    for &event in libslop::HOOK_EVENTS {
        let entries = settings["hooks"][event]
            .as_array()
            .unwrap_or_else(|| panic!("missing hooks.{}", event));
        let has_our_hook = entries.iter().any(|entry| {
            entry["hooks"].as_array().is_some_and(|hooks| {
                hooks.iter().any(|h| {
                    h["type"] == "command"
                        && h["command"]
                            .as_str()
                            .is_some_and(|c| c.contains("slopctl") && c.contains(event))
                })
            })
        });
        assert!(has_our_hook, "missing slopctl hook for event {}", event);
    }

    kill_slopd(slopd);
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

    // First cycle: inject, kill (auto-removes on exit).
    let slopd = env.spawn_slopd();
    let output = env.slopctl(&["run"]);
    assert!(output.status.success(), "slopctl run failed: {:?}", output);
    kill_slopd(slopd);
    std::thread::sleep(Duration::from_millis(50));

    // Second cycle: inject again on top of removed state.
    let slopd = env.spawn_slopd();
    let output = env.slopctl(&["run"]);
    assert!(output.status.success(), "slopctl run failed: {:?}", output);

    // Check while slopd is still running (it removes hooks on exit).
    let settings_contents = std::fs::read_to_string(claude_config_dir.join("settings.json"))
        .expect("settings.json was not created");
    let settings: serde_json::Value =
        serde_json::from_str(&settings_contents).expect("settings.json is not valid JSON");

    for &event in libslop::HOOK_EVENTS {
        let entries = settings["hooks"][event]
            .as_array()
            .unwrap_or_else(|| panic!("missing hooks.{}", event));
        let our_hook_count = entries
            .iter()
            .filter(|entry| {
                entry["hooks"].as_array().is_some_and(|hooks| {
                    hooks.iter().any(|h| {
                        h["type"] == "command"
                            && h["command"]
                                .as_str()
                                .is_some_and(|c| c.contains("slopctl") && c.contains(event))
                    })
                })
            })
            .count();
        assert_eq!(
            our_hook_count, 1,
            "expected exactly one slopctl hook for event {}, got {}",
            event, our_hook_count
        );
    }

    kill_slopd(slopd);
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
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    let session_id = env.wait_for_session_start(listener, &pane_id);

    kill_slopd(slopd);

    assert_eq!(session_id, "mock-session-id-1234");
}

// --- `slopctl run` wait-for-ready (default) -------------------------------
//
// By default `slopctl run` waits for the freshly-spawned pane to become ready
// before returning, so a pane that dies right after spawn (e.g. `claude
// --resume <bad-id>`, which fires SessionStart → SessionEnd → exit) surfaces as
// a non-zero exit instead of a dangling pane id. These tests use `slopctl_raw`
// to bypass the harness's legacy `--no-wait` injection and exercise the default.

/// Failure case: the pane dies before becoming ready. `run` must exit non-zero
/// with a useful message (including the SessionEnd reason) and print no pane id.
#[test]
fn run_fails_when_pane_dies_before_ready() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    // mock_claude --mock-exit=after-session-start fires SessionStart then SessionEnd
    // (reason=prompt_input_exit) and exits, simulating Claude bailing on a bad
    // --resume target after writing only bootstrap metadata.
    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path, "--mock-exit=after-session-start"]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let out = env.slopctl_raw(&["run", "--ready-timeout", "15"]);

    kill_slopd(slopd);

    assert!(
        !out.status.success(),
        "run should fail when the pane dies before becoming ready: {:?}",
        out
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("died before becoming ready"),
        "stderr should explain the pane died; got: {}",
        stderr
    );
    assert!(
        stderr.contains("prompt_input_exit"),
        "stderr should include the SessionEnd reason; got: {}",
        stderr
    );
    // Nothing usable to return, so no pane id on stdout.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.trim().is_empty(),
        "no pane id should be printed for a pane that died; got stdout: {:?}",
        stdout
    );
}

/// Failure case: the pane dies with NO hook ever firing — no SessionStart and
/// no SessionEnd — e.g. the Claude binary crashes on launch or the configured
/// executable isn't found (the common real-world cause: slopd runs the
/// executable inside a tmux window whose PATH may not contain it). slopd only
/// learns the pane is gone via its reconciler and emits PaneDestroyed, so `run`
/// must surface a non-zero exit with the "died before becoming ready" message
/// and NO "(session ended: …)" reason suffix, printing no pane id. This is the
/// bare PaneDestroyed path, distinct from `run_fails_when_pane_dies_before_ready`
/// (which exercises the SessionStart→SessionEnd "(session ended: …)" path).
#[test]
fn run_fails_when_pane_dies_without_any_hook() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    // mock_claude --mock-exit=immediate exits before firing ANY hook, simulating a
    // Claude binary that dies on launch (or an executable tmux can't find).
    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path, "--mock-exit=immediate"]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let out = env.slopctl_raw(&["run", "--ready-timeout", "15"]);

    kill_slopd(slopd);

    assert!(
        !out.status.success(),
        "run should fail when the pane dies before becoming ready: {:?}",
        out
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("died before becoming ready"),
        "stderr should explain the pane died; got: {}",
        stderr
    );
    // No SessionEnd fired, so there is no "(session ended: …)" suffix — this is
    // exactly the reason-less message users hit when claude can't start.
    assert!(
        !stderr.contains("session ended"),
        "no SessionEnd fired, so there should be no reason suffix; got: {}",
        stderr
    );
    // But slopd's dead-pane capture reads the exit code (mock_claude --mock-exit=immediate
    // exits 1) off the lingering pane and surfaces it, so even this no-hook crash
    // is no longer contentless.
    assert!(
        stderr.contains("exit status 1"),
        "stderr should include the captured exit status; got: {}",
        stderr
    );
    // Nothing usable to return, so no pane id on stdout.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.trim().is_empty(),
        "no pane id should be printed for a pane that died; got stdout: {:?}",
        stdout
    );
}

/// The headline win: when claude prints a startup error and dies before any hook
/// fires (the real-world case where a project-local config makes it bail on
/// launch), `slopctl run` must surface that error text AND the exit status — not
/// the contentless "died before becoming ready". slopd sets remain-on-exit on the
/// spawned pane, so the crashed process lingers as a DEAD pane with its final
/// screen frozen; the reconciler captures that screen + exit code into the
/// PaneDestroyed event and `run` prints both.
#[test]
fn run_surfaces_crash_output_and_exit_status() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    // mock_claude --mock-crash-output prints a recognizable line to the terminal and
    // exits 37 before firing any hook — standing in for claude choking on a
    // project-local .claude config and dying with a visible error.
    let marker = "FATAL: project-local config rejected by claude";
    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path, "--mock-crash-output", marker]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let out = env.slopctl_raw(&["run", "--ready-timeout", "15"]);

    kill_slopd(slopd);

    assert!(
        !out.status.success(),
        "run should fail when the pane crashes at startup: {:?}",
        out
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The captured startup error is surfaced verbatim...
    assert!(
        stderr.contains(marker),
        "stderr should include the captured startup error; got: {}",
        stderr
    );
    // ...along with the process exit status...
    assert!(
        stderr.contains("exit status 37"),
        "stderr should include the captured exit status; got: {}",
        stderr
    );
    // ...under the still-present base message.
    assert!(
        stderr.contains("died before becoming ready"),
        "stderr should still carry the base death message; got: {}",
        stderr
    );
    // No SessionEnd hook fired, so no reason suffix.
    assert!(
        !stderr.contains("session ended"),
        "no SessionEnd fired; there should be no reason suffix; got: {}",
        stderr
    );
    // No usable pane id to return.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.trim().is_empty(),
        "no pane id should be printed for a pane that died; got stdout: {:?}",
        stdout
    );
}

/// A crashed pane's captured death screen must survive in slopd's own log, not
/// just in the PaneDestroyed broadcast: the broadcast is ephemeral, so when a
/// pane dies while nobody is running `slopctl listen` (the common case — an
/// agent crashing overnight), the only durable record of WHY is the warn line
/// slopd writes (journald under systemd). Spawn slopd with stderr captured at
/// RUST_LOG=warn, crash a mock_claude pane with a recognizable message, and
/// assert the message and exit status land in the log.
#[test]
fn dead_pane_output_is_logged_for_claude_backend() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let marker = "FATAL: mock claude crashed for the log test";
    let Some(env) = TestEnv::new(Some(&[&mock_claude_path, "--mock-crash-output", marker])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let mut slopd = env.spawn_slopd_with_stderr_captured();

    // Subscribe before spawning so the PaneDestroyed emitted by the reconciler
    // (which fires AFTER the warn line is written) can't be missed.
    let listener = spawn_event_listener(&env, "PaneDestroyed");

    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    wait_for_event(listener, {
        let pane_id = pane_id.clone();
        move |v| v["event_type"] == "PaneDestroyed" && v["pane_id"] == pane_id.as_str()
    });

    // Read the log after shutdown so it is complete (EOF on slopd exit).
    let mut stderr = slopd.stderr.take().expect("slopd stderr is piped");
    kill_slopd(slopd);
    let mut log = String::new();
    {
        use std::io::Read as _;
        stderr.read_to_string(&mut log).expect("read slopd stderr");
    }

    assert!(
        log.contains(marker),
        "slopd log should contain the pane's dying words; got: {}",
        log
    );
    // The enriched death line names the pane, classifies the cause, and carries
    // the exit status — all on one greppable line in the journal.
    assert!(
        log.contains(&format!("pane {} died: cause=self_exit", pane_id)) && log.contains("exit=37"),
        "slopd log should carry the enriched self_exit death line with exit status; got: {}",
        log
    );
}

/// Same guarantee for the opencode backend: an opencode pane that crashes on
/// launch (before ever binding its port) must leave its death screen in slopd's
/// log. The dead-pane capture is backend-agnostic — this pins that it stays so.
#[test]
fn dead_pane_output_is_logged_for_opencode_backend() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_opencode");

    let mock_opencode_path = cargo_bin("mock_opencode").to_str().unwrap().to_string();
    let oc_config_dir = tempfile::tempdir().unwrap();

    let marker = "FATAL: mock opencode crashed for the log test";
    let Some(env) = TestEnv::new_full(None, None, None) else {
        eprintln!("skipping: tmux not found");
        return;
    };
    // An opencode account whose executable is the mock in crash mode: it prints
    // the marker and exits 37 before binding the assigned port.
    env.append_config(&format!(
        "\n[accounts.oc]\nbackend = \"opencode\"\nexecutable = [{:?}, \"--mock-crash-output\", {:?}]\nclaude_config_dir = {:?}\n",
        mock_opencode_path,
        marker,
        oc_config_dir.path().to_str().unwrap(),
    ));

    let mut slopd = env.spawn_slopd_with_stderr_captured();

    let listener = spawn_event_listener(&env, "PaneDestroyed");

    // Spawn `run` without waiting for its reply: the Run handler holds the
    // response for up to 20s retrying ensure_session against the opencode
    // server that never binds (the executable crashed), which outlasts
    // slopctl's request timeout. The reconciler detects and logs the death
    // independently of that reply, and the PaneDestroyed event names the pane.
    let mut run_child = Command::new(cargo_bin("slopctl"))
        .args(["run", "--no-wait", "--account", "oc"])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("TMUX_TMPDIR")
        .env_remove("TMPDIR")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl run");

    let event = wait_for_event(listener, |v| v["event_type"] == "PaneDestroyed");
    let pane_id = event["pane_id"]
        .as_str()
        .expect("PaneDestroyed carries pane_id")
        .to_string();
    let _ = run_child.kill();
    let _ = run_child.wait();

    let mut stderr = slopd.stderr.take().expect("slopd stderr is piped");
    kill_slopd(slopd);
    let mut log = String::new();
    {
        use std::io::Read as _;
        stderr.read_to_string(&mut log).expect("read slopd stderr");
    }

    assert!(
        log.contains(marker),
        "slopd log should contain the opencode pane's dying words; got: {}",
        log
    );
    assert!(
        log.contains(&format!("pane {} died: cause=self_exit", pane_id)) && log.contains("exit=37"),
        "slopd log should carry the enriched self_exit death line with exit status; got: {}",
        log
    );
    assert!(
        log.contains("backend=opencode"),
        "opencode pane's death line should record its backend; got: {}",
        log
    );
}

/// An explicit `slopctl kill` must record an unambiguous, self-explanatory
/// death: cause `deliberate_kill`, detected by the Kill RPC, naming the pane's
/// session and backend. This is the fact a post-mortem needs to say "slopd
/// killed it" instead of guessing — the exact gap the %119 investigation hit.
#[test]
fn kill_records_deliberate_death_with_identity() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    // Give the pane a known session id so the death record proves identity is
    // carried through teardown (a `sleep infinity` mock fires no SessionStart).
    let payload = r#"{"session_id":"sess-kill-abc","hook_event_name":"SessionStart","transcript_path":"/dev/null","cwd":"/tmp"}"#;
    let out = fire_hook(&env, "SessionStart", payload, Some(&pane_id));
    assert!(out.status.success(), "SessionStart hook failed: {:?}", out);

    let listener = spawn_event_listener(&env, "PaneDestroyed");
    let kill_output = env.slopctl(&["kill", &pane_id]);
    assert!(
        kill_output.status.success(),
        "slopctl kill failed: {:?}",
        kill_output
    );

    let event = wait_for_event(listener, {
        let pane_id = pane_id.clone();
        move |v| v["event_type"] == "PaneDestroyed" && v["pane_id"] == pane_id.as_str()
    });
    kill_slopd(slopd);

    let payload = &event["payload"];
    assert_eq!(
        payload["cause"], "deliberate_kill",
        "explicit kill must be recorded as deliberate_kill; got: {}",
        event
    );
    assert_eq!(payload["detected_by"], "kill_rpc", "got: {}", event);
    assert_eq!(
        payload["session_id"], "sess-kill-abc",
        "death record must carry the pane's session id; got: {}",
        event
    );
    assert_eq!(payload["backend"], "claude", "got: {}", event);
}

/// The %119 scenario, reproduced: a pane removed by an *external* `tmux
/// kill-pane` (nothing slopd initiated) must still yield a definitive record —
/// cause `vanished`, and, via correlation with the `after-kill-pane` lifecycle
/// hook tmux fires with no pane id, the fact that it was an external kill rather
/// than a closed window. Before this, such a death logged only a bare
/// `pane=None` and an unattributed `no longer exists`, which is why %119 could
/// not be pinned to a session or a cause. The background reconcile is lengthened
/// so the hook-driven path is what detects the death (deterministic correlation).
#[test]
fn externally_killed_pane_records_vanished_with_hook() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd_with_envs(&[("SLOPD_TEST_RECONCILE_INTERVAL_MS", "60000")]);

    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    let payload = r#"{"session_id":"sess-vanish-xyz","hook_event_name":"SessionStart","transcript_path":"/dev/null","cwd":"/tmp"}"#;
    let out = fire_hook(&env, "SessionStart", payload, Some(&pane_id));
    assert!(out.status.success(), "SessionStart hook failed: {:?}", out);

    let listener = spawn_event_listener(&env, "PaneDestroyed");

    // Kill the pane straight through tmux, bypassing slopd entirely — exactly
    // how %119 went (an external kill-pane, not a `slopctl kill`).
    let killed = env
        .tmux
        .tmux()
        .args(["kill-pane", "-t", &pane_id])
        .status()
        .expect("failed to run tmux kill-pane");
    assert!(killed.success(), "tmux kill-pane failed");

    let event = wait_for_event(listener, {
        let pane_id = pane_id.clone();
        move |v| v["event_type"] == "PaneDestroyed" && v["pane_id"] == pane_id.as_str()
    });
    kill_slopd(slopd);

    let payload = &event["payload"];
    assert_eq!(
        payload["cause"], "vanished",
        "an external kill-pane must be recorded as vanished; got: {}",
        event
    );
    assert_eq!(
        payload["detected_by"], "reconcile_vanished",
        "got: {}",
        event
    );
    assert_eq!(
        payload["session_id"], "sess-vanish-xyz",
        "vanished death must still name the session slopd had bound; got: {}",
        event
    );
    assert_eq!(
        payload["preceding_hook"], "after-kill-pane",
        "the death should be correlated to tmux's after-kill-pane hook; got: {}",
        event
    );
}

/// Failure case: the configured executable doesn't exist. A typo'd or
/// uninstalled `[run] executable` is the most common misconfiguration, and
/// `tmux new-window <missing>` still returns a pane id — so without an explicit
/// check, `run` only fails much later with the generic "died before becoming
/// ready" (or a ready timeout), naming nothing useful. `run` must instead fail
/// fast with a message that NAMES the missing executable and prints no pane id.
#[test]
fn run_fails_clearly_when_executable_does_not_exist() {
    build_bin("slopd");
    build_bin("slopctl");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    // A name that is definitely not on PATH.
    let missing = "slopd-no-such-executable-9f3a";

    let Some(env) = TestEnv::new_full(Some(&[missing]), Some(&slopctl_path), None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    let out = env.slopctl_raw(&["run", "--ready-timeout", "6"]);
    kill_slopd(slopd);

    assert!(
        !out.status.success(),
        "run must fail when the configured executable is missing: {:?}",
        out
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(missing),
        "error should name the missing executable {:?}; got: {}",
        missing,
        stderr
    );
    assert!(
        stderr.to_lowercase().contains("not found"),
        "error should say the executable wasn't found; got: {}",
        stderr
    );
    // Nothing spawned, so no pane id on stdout.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.trim().is_empty(),
        "no pane id for a run that never spawned; got: {:?}",
        stdout
    );
}

/// Success case: a healthy pane reaches ready and survives the settle window.
/// `run` exits 0 and prints the pane id, exactly like the old behaviour.
#[test]
fn run_waits_for_ready_then_prints_pane_id() {
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

    let out = env.slopctl_raw(&["run", "--ready-timeout", "15"]);
    assert!(
        out.status.success(),
        "run should succeed for a healthy pane: {:?}",
        out
    );
    let pane_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        pane_id.starts_with('%'),
        "run should print the pane id; got: {:?}",
        pane_id
    );

    // The pane is genuinely ready by the time run returns.
    let (_state, detailed) = env.pane_state(&pane_id);
    kill_slopd(slopd);
    assert_eq!(
        detailed,
        libslop::PaneDetailedState::Ready,
        "pane should be ready when run returns"
    );
}

/// Timeout case: the pane never becomes ready (a non-Claude `sleep infinity`
/// pane fires no SessionStart). `run` exits non-zero but still prints the pane
/// id so the caller can investigate.
#[test]
fn run_times_out_when_pane_never_becomes_ready() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let out = env.slopctl_raw(&["run", "--ready-timeout", "2"]);

    kill_slopd(slopd);

    assert!(
        !out.status.success(),
        "run should fail on timeout: {:?}",
        out
    );
    // The pane id is still printed so the caller can investigate the stuck pane.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.trim().starts_with('%'),
        "timed-out run should still print the pane id; got stdout: {:?}",
        stdout
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("timed out"),
        "stderr should explain the timeout; got: {}",
        stderr
    );
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

    let listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(listener, &pane_id);

    let send_output = env.slopctl(&["send", &pane_id, "hello from test"]);

    kill_slopd(slopd);

    assert!(
        send_output.status.success(),
        "slopctl send failed: {:?}",
        send_output
    );
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

    let listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(listener, &pane_id);

    const N: usize = 5;
    let handles: Vec<_> = (0..N)
        .map(|i| {
            let env = env.clone();
            let pane_id = pane_id.clone();
            std::thread::spawn(move || env.slopctl(&["send", &pane_id, &format!("prompt {}", i)]))
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
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(listener, &pane_id);

    // Add a tag so we can verify it appears in ps output.
    let tag_out = env.slopctl(&["tag", &pane_id, "mytest"]);
    assert!(
        tag_out.status.success(),
        "slopctl tag failed: {:?}",
        tag_out
    );

    let ps_out = env.slopctl(&["ps"]);
    let ps_json_out = env.slopctl(&["ps", "--json"]);

    kill_slopd(slopd);

    assert!(ps_out.status.success(), "slopctl ps failed: {:?}", ps_out);
    let stdout = String::from_utf8_lossy(&ps_out.stdout);
    assert!(
        stdout.contains(&pane_id),
        "ps output missing pane_id {}: {}",
        pane_id,
        stdout
    );
    assert!(
        stdout.contains("mock-session-id-1234"),
        "ps output missing session_id: {}",
        stdout
    );
    assert!(
        stdout.contains("mytest"),
        "ps output missing tag: {}",
        stdout
    );
    assert!(
        stdout.contains("LAST_ACTIVE"),
        "ps output missing LAST_ACTIVE column header: {}",
        stdout
    );
    assert!(
        stdout.contains("ago") || stdout.contains("now"),
        "ps output missing time: {}",
        stdout
    );
    assert!(
        !stdout.contains("56 years ago"),
        "created_at is 0: {}",
        stdout
    );

    // Verify created_at and last_active are plausible recent Unix timestamps.
    assert!(
        ps_json_out.status.success(),
        "ps --json failed: {:?}",
        ps_json_out
    );
    let panes: serde_json::Value =
        serde_json::from_slice(&ps_json_out.stdout).expect("ps --json is not valid JSON");
    let pane_entry = panes
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["pane_id"] == pane_id)
        .unwrap_or_else(|| panic!("pane {} not in ps --json output", pane_id));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let created_at = pane_entry["created_at"]
        .as_u64()
        .expect("created_at is not a u64");
    assert!(created_at > 0, "created_at is 0");
    assert!(
        created_at <= now,
        "created_at is in the future: {}",
        created_at
    );
    assert!(
        now - created_at < 60,
        "created_at is more than 60s ago: {}",
        created_at
    );
    let last_active = pane_entry["last_active"]
        .as_u64()
        .expect("last_active is not a u64");
    assert!(last_active > 0, "last_active is 0");
    assert!(
        last_active <= now,
        "last_active is in the future: {}",
        last_active
    );
    assert!(
        created_at <= last_active,
        "created_at ({}) is after last_active ({})",
        created_at,
        last_active
    );
}

#[test]
fn ps_lists_panes_in_stable_numeric_order() {
    // Regression: panes are tracked in a DashSet (hash-arbitrary iteration), so
    // `ps` used to list them in a shuffled, instance-dependent order. They must
    // come back sorted by numeric pane id (%N), stably across repeated calls.
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Spawn several panes (sleep-infinity: no session needed for a pure ordering
    // check). Their tmux ids are assigned in increasing numeric order.
    let mut spawned: Vec<String> = Vec::new();
    for _ in 0..4 {
        let out = env.slopctl(&["run"]);
        assert!(out.status.success(), "slopctl run failed: {:?}", out);
        spawned.push(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }

    let pane_num = |id: &str| -> u64 {
        id.strip_prefix('%')
            .and_then(|n| n.parse().ok())
            .expect("pane id is %N")
    };

    // Read ps --json a few times; order must be identical each time AND sorted
    // ascending by numeric pane id.
    let mut prev: Option<Vec<String>> = None;
    for call in 0..3 {
        let out = env.slopctl(&["ps", "--json"]);
        assert!(out.status.success(), "ps --json failed: {:?}", out);
        let panes: Vec<libslop::PaneInfo> =
            serde_json::from_slice(&out.stdout).expect("ps --json is not valid JSON");
        let ids: Vec<String> = panes.iter().map(|p| p.pane_id.clone()).collect();

        let nums: Vec<u64> = ids.iter().map(|id| pane_num(id)).collect();
        let mut sorted = nums.clone();
        sorted.sort_unstable();
        assert_eq!(
            nums, sorted,
            "ps call {call} not sorted by numeric pane id: {ids:?}"
        );

        if let Some(ref p) = prev {
            assert_eq!(
                &ids, p,
                "ps order changed between calls: {p:?} then {ids:?}"
            );
        }
        prev = Some(ids);
    }

    // All spawned panes are present.
    let final_ids = prev.unwrap();
    for s in &spawned {
        assert!(
            final_ids.contains(s),
            "spawned pane {s} missing from ps: {final_ids:?}"
        );
    }

    kill_slopd(slopd);
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
    let parent_pane = String::from_utf8_lossy(&parent_out.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(listener, &parent_pane);

    // Switch mock_claude to always-submit mode so single Enters work reliably.
    let mode_out = env.slopctl(&["send", &parent_pane, "::mock input-mode always-submit"]);
    assert!(
        mode_out.status.success(),
        "slopctl send ::mock input-mode failed: {:?}",
        mode_out
    );

    // Ask mock_claude to spawn a child pane. Because it runs inside a tmux pane,
    // TMUX_PANE is set by tmux automatically — no manual env var wiring needed.
    let send_out = env.slopctl(&["send", &parent_pane, "::mock spawn-pane"]);
    assert!(
        send_out.status.success(),
        "slopctl send ::mock spawn-pane failed: {:?}",
        send_out
    );

    // mock_claude prints "::mock spawned-pane <child_pane_id>" to the pane; capture it.
    let child_pane = {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let out = env
                .tmux
                .tmux()
                .args(["capture-pane", "-t", &parent_pane, "-p"])
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some(line) = text.lines().find(|l| l.starts_with("::mock spawned-pane ")) {
                break line
                    .trim_start_matches("::mock spawned-pane ")
                    .trim()
                    .to_string();
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for ::mock spawn-pane output in pane"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    let ps_out = env.slopctl(&["ps"]);
    // Verify no stray quote characters in parent_pane_id via JSON output (issue #5).
    let ps_json_out = env.slopctl(&["ps", "--json"]);

    kill_slopd(slopd);

    assert!(ps_out.status.success(), "ps failed: {:?}", ps_out);
    let stdout = String::from_utf8_lossy(&ps_out.stdout);
    let child_line = stdout
        .lines()
        .find(|l| l.contains(&child_pane))
        .unwrap_or_else(|| {
            panic!(
                "child pane {} not found in ps output:\n{}",
                child_pane, stdout
            )
        });
    assert!(
        child_line.contains(&parent_pane),
        "child row missing parent pane ID {}:\n{}",
        parent_pane,
        child_line
    );
    let parent_line = stdout
        .lines()
        .find(|l| l.starts_with(&parent_pane))
        .unwrap_or_else(|| {
            panic!(
                "parent pane {} not found in ps output:\n{}",
                parent_pane, stdout
            )
        });
    assert!(
        parent_line.contains('-'),
        "parent row should have '-' for PARENT:\n{}",
        parent_line
    );

    assert!(
        ps_json_out.status.success(),
        "ps --json failed: {:?}",
        ps_json_out
    );
    let panes: serde_json::Value =
        serde_json::from_slice(&ps_json_out.stdout).expect("ps --json output is not valid JSON");
    let child_entry = panes
        .as_array()
        .unwrap()
        .iter()
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

    assert!(
        !output.status.success(),
        "slopctl send should have failed for non-existent pane"
    );
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
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(listener, &pane_id);

    // Switch mock_claude to always-submit mode. Two Enters needed: the first is
    // literal (alternating mode default), the second submits.
    env.tmux
        .tmux()
        .args([
            "send-keys",
            "-t",
            &pane_id,
            "::mock input-mode always-submit",
            "Enter",
            "Enter",
        ])
        .status()
        .expect("failed to send ::mock input-mode");
    std::thread::sleep(Duration::from_millis(100));

    // Put mock_claude into break-hooks mode: it drains stdin but fires no hooks.
    // Sent directly via tmux (not slopctl) to avoid going through the Send machinery.
    env.tmux
        .tmux()
        .args([
            "send-keys",
            "-t",
            &pane_id,
            "::mock transport stall-hooks",
            "Enter",
        ])
        .status()
        .expect("failed to send ::mock transport stall-hooks");

    // This send reaches a live pane (send-keys succeeds) but UserPromptSubmit will never fire.
    // Pass a short --timeout so slopd returns an error quickly rather than the test hanging.
    let output = env.slopctl(&["send", &pane_id, "hello", "--timeout", "2"]);

    kill_slopd(slopd);

    assert!(
        !output.status.success(),
        "slopctl send should have timed out: {:?}",
        output
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("timed out"),
        "expected timeout message in stderr: {:?}",
        stderr
    );
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
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    let start = Instant::now();
    let output = env.slopctl(&["send", &pane_id, "hello", "--timeout", "2"]);
    let elapsed = start.elapsed();

    kill_slopd(slopd);

    assert!(!output.status.success(), "send should have timed out");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("timed out"),
        "expected timeout message in stderr: {:?}",
        stderr
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "send took {:?}, timer appears to have hung (issue #9)",
        elapsed
    );
}

#[test]
fn listen_no_filters_receives_all_events() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Create a managed pane so hooks are not ignored.
    let run_out = env.slopctl(&["run"]);
    assert!(
        run_out.status.success(),
        "slopctl run failed: {:?}",
        run_out
    );
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    let mut listen = Command::new(cargo_bin("slopctl"))
        .args(["listen"])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl listen");

    let stdout = listen.stdout.take().unwrap();

    // Read and discard the subscription confirmation line.
    let timeout = Duration::from_secs(10);
    let (subscribed_line, reader) =
        read_line_timeout(stdout, timeout).expect("timed out reading subscribed line");
    assert!(
        subscribed_line.contains("subscribed"),
        "unexpected first line: {:?}",
        subscribed_line
    );

    // Fire two different event types.
    let stop_payload = r#"{"session_id":"s1","hook_event_name":"Stop"}"#;
    let out = fire_hook(&env, "Stop", stop_payload, Some(&pane_id));
    assert!(out.status.success(), "slopctl hook Stop failed: {:?}", out);

    let prompt_payload =
        r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"hi"}"#;
    let out = fire_hook(&env, "UserPromptSubmit", prompt_payload, Some(&pane_id));
    assert!(
        out.status.success(),
        "slopctl hook UserPromptSubmit failed: {:?}",
        out
    );

    // Skip slopd-internal events (StateChange, etc.) and collect the two hook events.
    let (ev1, reader) = read_next_hook_event(reader);
    let (ev2, _reader) = read_next_hook_event(reader);

    kill_child(listen);
    kill_slopd(slopd);

    assert_eq!(ev1["event_type"], "Stop");
    assert_eq!(ev2["event_type"], "UserPromptSubmit");
}

#[test]
fn listen_receives_hook_event() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Create a managed pane so hooks are not ignored.
    let run_out = env.slopctl(&["run"]);
    assert!(
        run_out.status.success(),
        "slopctl run failed: {:?}",
        run_out
    );
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    let mut listen = Command::new(cargo_bin("slopctl"))
        .args(["listen", "--hook", "UserPromptSubmit"])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl listen");

    let stdout = listen.stdout.take().unwrap();

    // Read and discard the subscription confirmation line.
    let timeout = Duration::from_secs(10);
    let (subscribed_line, reader) =
        read_line_timeout(stdout, timeout).expect("timed out reading subscribed line");
    assert!(
        subscribed_line.contains("subscribed"),
        "unexpected first line: {:?}",
        subscribed_line
    );

    let payload = r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"hello"}"#;
    let out = fire_hook(&env, "UserPromptSubmit", payload, Some(&pane_id));
    assert!(out.status.success(), "slopctl hook failed: {:?}", out);

    let (line, _reader) = read_line_timeout(reader, timeout).expect("timed out reading event line");

    kill_child(listen);
    kill_slopd(slopd);

    let event: serde_json::Value =
        serde_json::from_str(line.trim()).expect("event is not valid JSON");
    assert_eq!(event["event_type"], "UserPromptSubmit");
    assert_eq!(event["source"], "hook");
    assert_eq!(event["payload"]["prompt"], "hello");
}

#[test]
fn listen_filters_out_non_matching_events() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Create a managed pane so hooks are not ignored.
    let run_out = env.slopctl(&["run"]);
    assert!(
        run_out.status.success(),
        "slopctl run failed: {:?}",
        run_out
    );
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    let mut listen = Command::new(cargo_bin("slopctl"))
        .args(["listen", "--hook", "UserPromptSubmit"])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl listen");

    let stdout = listen.stdout.take().unwrap();

    // Read and discard the subscription confirmation line.
    let timeout = Duration::from_secs(10);
    let (subscribed_line, reader) =
        read_line_timeout(stdout, timeout).expect("timed out reading subscribed line");
    assert!(
        subscribed_line.contains("subscribed"),
        "unexpected first line: {:?}",
        subscribed_line
    );

    // Fire a non-matching event first.
    let stop_payload = r#"{"session_id":"s1","hook_event_name":"Stop"}"#;
    let out = fire_hook(&env, "Stop", stop_payload, Some(&pane_id));
    assert!(out.status.success(), "slopctl hook Stop failed: {:?}", out);

    // Then fire the matching event.
    let prompt_payload =
        r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"world"}"#;
    let out = fire_hook(&env, "UserPromptSubmit", prompt_payload, Some(&pane_id));
    assert!(
        out.status.success(),
        "slopctl hook UserPromptSubmit failed: {:?}",
        out
    );

    let (line, _reader) = read_line_timeout(reader, timeout).expect("timed out reading event line");

    kill_child(listen);
    kill_slopd(slopd);

    let event: serde_json::Value =
        serde_json::from_str(line.trim()).expect("event is not valid JSON");
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

    // Read and discard the subscription confirmation line.
    let (subscribed_line, reader) = read_line_timeout(stdout, Duration::from_secs(10))
        .expect("timed out reading subscribed line");
    assert!(
        subscribed_line.contains("subscribed"),
        "unexpected first line: {:?}",
        subscribed_line
    );

    // Fire from the wrong pane first.
    let other_payload =
        r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"wrong pane"}"#;
    let out = fire_hook(&env, "UserPromptSubmit", other_payload, Some(&other_pane));
    assert!(out.status.success());

    // Then fire from the target pane.
    let target_payload =
        r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"right pane"}"#;
    let out = fire_hook(&env, "UserPromptSubmit", target_payload, Some(&target_pane));
    assert!(out.status.success());

    let (event, _reader) = read_next_hook_event(reader);

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
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(listener, &pane_id);

    // Interrupt: sends C-c, C-d, Escape — enough to drop whatever Claude is doing.
    let int_out = env.slopctl(&["interrupt", &pane_id]);
    assert!(int_out.status.success(), "interrupt failed: {:?}", int_out);
    assert_eq!(String::from_utf8_lossy(&int_out.stdout).trim(), pane_id);

    // mock_claude should still be alive — a single interrupt doesn't exit.
    std::thread::sleep(Duration::from_millis(100));
    let pane_alive = env
        .tmux
        .tmux()
        .args(["list-panes", "-t", &pane_id, "-F", "#{pane_id}"])
        .output()
        .unwrap();
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
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    // Tag the pane.
    let tag_out = env.slopctl(&["tag", &pane_id, "my-tag"]);
    assert!(
        tag_out.status.success(),
        "slopctl tag failed: {:?}",
        tag_out
    );

    // List tags — should include our tag.
    let tags_out = env.slopctl(&["tags", &pane_id]);
    assert!(
        tags_out.status.success(),
        "slopctl tags failed: {:?}",
        tags_out
    );
    let tags_stdout = String::from_utf8_lossy(&tags_out.stdout);
    assert!(
        tags_stdout.lines().any(|l| l == "my-tag"),
        "tag not listed: {:?}",
        tags_stdout
    );

    // Verify the tmux option was set on the pane.
    let opt_out = env
        .tmux
        .tmux()
        .args([
            "show-options",
            "-t",
            &pane_id,
            "-p",
            "-v",
            &libslop::tag_option_name("my-tag").unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&opt_out.stdout).trim(), "1");

    // Untag.
    let untag_out = env.slopctl(&["untag", &pane_id, "my-tag"]);
    assert!(
        untag_out.status.success(),
        "slopctl untag failed: {:?}",
        untag_out
    );

    // Tags should now be empty.
    let tags_out2 = env.slopctl(&["tags", &pane_id]);
    assert!(tags_out2.status.success());
    let tags_stdout2 = String::from_utf8_lossy(&tags_out2.stdout);
    assert!(
        !tags_stdout2.lines().any(|l| l == "my-tag"),
        "tag still listed after untag"
    );

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
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    let ps_out = env.slopctl(&["ps", "--json"]);
    assert!(
        ps_out.status.success(),
        "slopctl ps --json failed: {:?}",
        ps_out
    );
    let panes: serde_json::Value =
        serde_json::from_slice(&ps_out.stdout).expect("ps --json is not valid JSON");
    let created_at_before = panes
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["pane_id"] == pane_id)
        .unwrap_or_else(|| panic!("pane {} not in ps --json output", pane_id))["created_at"]
        .as_u64()
        .expect("created_at is not a u64");

    kill_slopd(slopd);
    let slopd2 = env.spawn_slopd();

    let ps_out2 = env.slopctl(&["ps", "--json"]);
    assert!(
        ps_out2.status.success(),
        "slopctl ps --json failed after restart: {:?}",
        ps_out2
    );
    let panes2: serde_json::Value =
        serde_json::from_slice(&ps_out2.stdout).expect("ps --json is not valid JSON after restart");
    let created_at_after = panes2.as_array().unwrap().iter()
        .find(|p| p["pane_id"] == pane_id)
        .unwrap_or_else(|| panic!("pane {} not in ps --json output after restart", pane_id))["created_at"]
        .as_u64()
        .expect("created_at is not a u64 after restart");

    assert_eq!(
        created_at_before, created_at_after,
        "created_at changed after slopd restart"
    );

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
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    let tag_out = env.slopctl(&["tag", &pane_id, "persistent"]);
    assert!(
        tag_out.status.success(),
        "slopctl tag failed: {:?}",
        tag_out
    );

    // Restart slopd — tmux and the pane keep running.
    kill_slopd(slopd);
    let slopd2 = env.spawn_slopd();

    let tags_out = env.slopctl(&["tags", &pane_id]);
    assert!(
        tags_out.status.success(),
        "slopctl tags failed after restart: {:?}",
        tags_out
    );
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
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    let tag_out = env.slopctl(&["tag", &pane_id, "current-pane-tag"]);
    assert!(
        tag_out.status.success(),
        "slopctl tag failed: {:?}",
        tag_out
    );

    // Run `slopctl tags` without an explicit pane ID but with TMUX_PANE set.
    let tags_out = Command::new(cargo_bin("slopctl"))
        .args(["tags"])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .env("TMUX_PANE", &pane_id)
        .output()
        .expect("failed to run slopctl tags");
    assert!(
        tags_out.status.success(),
        "slopctl tags failed: {:?}",
        tags_out
    );
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

    assert!(
        !out.status.success(),
        "slopctl tag should fail for invalid tag name"
    );
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

    assert!(
        !out.status.success(),
        "slopctl tag should fail for empty tag name"
    );
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
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
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

    let pane1 = String::from_utf8_lossy(&env.slopctl(&["run"]).stdout)
        .trim()
        .to_string();
    let pane2 = String::from_utf8_lossy(&env.slopctl(&["run"]).stdout)
        .trim()
        .to_string();

    env.slopctl(&["tag", &pane1, "shared"]);
    env.slopctl(&["tag", &pane2, "shared"]);

    let out = env.slopctl(&["send", "tag=shared", "hello"]);

    kill_slopd(slopd);

    assert!(
        !out.status.success(),
        "send --select one should fail with 2 matches"
    );
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
    let pane1 = String::from_utf8_lossy(&env.slopctl(&["run"]).stdout)
        .trim()
        .to_string();
    let pane2 = String::from_utf8_lossy(&env.slopctl(&["run"]).stdout)
        .trim()
        .to_string();
    env.wait_for_session_starts(listener, &[&pane1, &pane2]);

    env.slopctl(&["tag", &pane1, "broadcast"]);
    env.slopctl(&["tag", &pane2, "broadcast"]);

    let send_out = env.slopctl(&["send", "tag=broadcast", "hello all", "--select", "all"]);

    kill_slopd(slopd);

    assert!(
        send_out.status.success(),
        "send --select all failed: {:?}",
        send_out
    );
    let stdout = String::from_utf8_lossy(&send_out.stdout);
    assert!(
        stdout.contains(&pane1),
        "output missing pane1 {}: {}",
        pane1,
        stdout
    );
    assert!(
        stdout.contains(&pane2),
        "output missing pane2 {}: {}",
        pane2,
        stdout
    );
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
    let pane1 = String::from_utf8_lossy(&env.slopctl(&["run"]).stdout)
        .trim()
        .to_string();
    let pane2 = String::from_utf8_lossy(&env.slopctl(&["run"]).stdout)
        .trim()
        .to_string();
    env.wait_for_session_starts(listener, &[&pane1, &pane2]);

    env.slopctl(&["tag", &pane1, "anytarget"]);
    env.slopctl(&["tag", &pane2, "anytarget"]);

    let send_out = env.slopctl(&["send", "tag=anytarget", "hello any", "--select", "any"]);

    kill_slopd(slopd);

    assert!(
        send_out.status.success(),
        "send --select any failed: {:?}",
        send_out
    );
    let stdout = String::from_utf8_lossy(&send_out.stdout);
    // Exactly one pane ID should appear in the output.
    let count = stdout.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(
        count, 1,
        "expected exactly one pane in output, got: {}",
        stdout
    );
    let chosen = stdout.trim();
    assert!(
        chosen == pane1 || chosen == pane2,
        "chosen pane {} not one of the tagged panes",
        chosen
    );
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

    let pane1 = String::from_utf8_lossy(&env.slopctl(&["run"]).stdout)
        .trim()
        .to_string();
    let pane2 = String::from_utf8_lossy(&env.slopctl(&["run"]).stdout)
        .trim()
        .to_string();

    env.slopctl(&["tag", &pane1, "visible"]);

    let ps_out = env.slopctl(&["ps", "--filter", "tag=visible"]);

    kill_slopd(slopd);

    assert!(ps_out.status.success(), "ps --filter failed: {:?}", ps_out);
    let stdout = String::from_utf8_lossy(&ps_out.stdout);
    assert!(stdout.contains(&pane1), "filtered ps missing tagged pane");
    assert!(
        !stdout.contains(&pane2),
        "filtered ps should not show untagged pane"
    );
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
    env.wait_for_session_starts(
        listener,
        &pane_ids.iter().map(String::as_str).collect::<Vec<_>>(),
    );

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
    let all_out = env.slopctl(&[
        "send",
        "tag=concurrent",
        "hello concurrent",
        "--select",
        "all",
    ]);
    let all_elapsed = all_start.elapsed();

    kill_slopd(slopd);

    assert!(all_out.status.success(), "send failed: {:?}", all_out);

    // All N panes received. Wall time should be well under N * baseline.
    // We allow 3x baseline as headroom for scheduling jitter.
    let limit = baseline * 3 + Duration::from_millis(500);
    assert!(
        all_elapsed < limit,
        "send to {} panes took {:?}, expected < {:?} (baseline {:?}); \
         sends are likely sequential not concurrent",
        N,
        all_elapsed,
        limit,
        baseline,
    );
}

/// Run slopctl with the given args (no daemon needed), assert exit code 2, and
/// assert that stderr contains `expected_hint` so the user knows what went wrong.
fn assert_invalid_usage(args: &[&str], expected_hint: &str) {
    build_bin("slopctl");
    let out = Command::new(cargo_bin("slopctl"))
        .args(args)
        // Don't inherit an ambient $TMUX_PANE (e.g. when running the suite from
        // inside tmux): `slopctl tags` would treat it as the target pane and
        // skip the missing-pane-id validation this asserts on.
        .env_remove("TMUX_PANE")
        .output()
        .expect("failed to run slopctl");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(2),
        "slopctl {:?}: expected exit 2, got {:?}\nstderr: {}",
        args,
        out.status.code(),
        stderr,
    );
    assert!(
        stderr.contains(expected_hint),
        "slopctl {:?}: stderr missing {:?}\nstderr: {}",
        args,
        expected_hint,
        stderr,
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
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1\nstderr: {}",
        stderr
    );
    assert!(
        stderr.contains("foo"),
        "expected filter key in error\nstderr: {}",
        stderr
    );
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
    assert!(
        parent_out.status.success(),
        "first run failed: {:?}",
        parent_out
    );
    let parent_pane = String::from_utf8_lossy(&parent_out.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(listener, &parent_pane);

    // Switch mock_claude to always-submit mode so single Enters work reliably.
    let mode_out = env.slopctl(&["send", &parent_pane, "::mock input-mode always-submit"]);
    assert!(
        mode_out.status.success(),
        "slopctl send ::mock input-mode failed: {:?}",
        mode_out
    );

    // Ask mock_claude to spawn a child. TMUX_PANE is set by tmux in mock_claude's
    // environment, so the child gets @slopd_ancestor_panes set automatically.
    let send_out = env.slopctl(&["send", &parent_pane, "::mock spawn-pane"]);
    assert!(
        send_out.status.success(),
        "slopctl send ::mock spawn-pane failed: {:?}",
        send_out
    );

    // mock_claude prints "::mock spawned-pane <child_pane_id>" to the pane; capture it.
    let child_pane = {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let out = env
                .tmux
                .tmux()
                .args(["capture-pane", "-t", &parent_pane, "-p"])
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some(line) = text.lines().find(|l| l.starts_with("::mock spawned-pane ")) {
                break line
                    .trim_start_matches("::mock spawned-pane ")
                    .trim()
                    .to_string();
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for ::mock spawn-pane output in pane"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    // Verify the child pane has @slopd_ancestor_panes with parent as first entry.
    let opt_out = env
        .tmux
        .tmux()
        .args([
            "show-options",
            "-t",
            &child_pane,
            "-p",
            "-v",
            libslop::TmuxOption::SlopdAncestorPanes.as_str(),
        ])
        .output()
        .unwrap();
    let value = String::from_utf8_lossy(&opt_out.stdout).trim().to_string();
    // The ancestor list should start with the parent pane ID.
    let first_ancestor = value.split(',').next().unwrap_or("").trim();

    kill_slopd(slopd);

    assert_eq!(
        first_ancestor, parent_pane,
        "@slopd_ancestor_panes first entry should equal parent pane ID"
    );
}

#[test]
fn run_does_not_set_claude_config_dir_when_not_configured() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    // No claude_config_dir — slopd should not set CLAUDE_CONFIG_DIR in the pane env.
    let Some(env) = TestEnv::new_full(Some(&[&mock_claude_path]), Some(&slopctl_path), None) else {
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
    env.tmux
        .tmux()
        .args([
            "send-keys",
            "-t",
            &pane_id,
            "::mock input-mode always-submit",
            "Enter",
            "Enter",
        ])
        .status()
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));

    // Send keys directly via tmux (bypasses slopctl send / UserPromptSubmit hook).
    env.tmux
        .tmux()
        .args([
            "send-keys",
            "-t",
            &pane_id,
            "::mock env CLAUDE_CONFIG_DIR",
            "Enter",
        ])
        .status()
        .unwrap();

    // Poll pane output for the ::mock env response.
    let deadline = Instant::now() + Duration::from_secs(5);
    let env_line = loop {
        let out = env
            .tmux
            .tmux()
            .args(["capture-pane", "-t", &pane_id, "-p"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        // tmux may wrap long lines; join the full output before searching.
        let joined = text.replace(['\n', '\r'], "");
        let needle = "::mock env CLAUDE_CONFIG_DIR=";
        if let Some(pos) = joined.find(needle) {
            let value = joined[pos + needle.len()..]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
            break format!("{needle}{value}");
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for ::mock env output"
        );
        std::thread::sleep(Duration::from_millis(50));
    };

    kill_slopd(slopd);

    assert_eq!(
        env_line, "::mock env CLAUDE_CONFIG_DIR=UNSET",
        "CLAUDE_CONFIG_DIR should not be set when no custom dir is configured"
    );
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

    let opt_out = env
        .tmux
        .tmux()
        .args([
            "show-options",
            "-t",
            &pane_id,
            "-p",
            "-v",
            libslop::TmuxOption::SlopdAncestorPanes.as_str(),
        ])
        .output()
        .unwrap();
    let value = String::from_utf8_lossy(&opt_out.stdout).trim().to_string();

    kill_slopd(slopd);

    assert!(
        value.is_empty(),
        "@slopd_ancestor_panes should not be set for user-initiated run, got {:?}",
        value
    );
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

    // Subscribe before spawning so we can't miss the event. If `--print hello` is
    // forwarded, mock_claude runs in print mode and exits immediately, and slopd's
    // reconciler emits PaneDestroyed for the pane. If the args were NOT forwarded,
    // mock_claude would stay in its interactive loop and no PaneDestroyed would
    // fire (the wait below would then time out and fail).
    //
    // (Previously this test set a global remain-on-exit and polled capture-pane
    // for "Pane is dead". slopd now sets remain-on-exit per pane itself and its
    // reconciler kills the dead husk after capturing it, which raced that poll;
    // observing PaneDestroyed is the race-free equivalent.)
    let listener = spawn_event_listener(&env, "PaneDestroyed");

    let run_out = env.slopctl(&["run", "--", "--print", "hello"]);
    assert!(
        run_out.status.success(),
        "slopctl run failed: {:?}",
        run_out
    );
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    let event = wait_for_event(listener, {
        let pane_id = pane_id.clone();
        move |v| v["event_type"] == "PaneDestroyed" && v["pane_id"] == pane_id.as_str()
    });
    assert_eq!(event["payload"]["pane_id"], pane_id.as_str());

    kill_slopd(slopd);
}

/// Verify that ::mock echo command in mock_claude prints the argument back.
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
    assert!(
        run_out.status.success(),
        "slopctl run failed: {:?}",
        run_out
    );
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();
    env.wait_for_session_start(listener, &pane_id);

    let send_out = env.slopctl(&["send", &pane_id, "::mock echo hello-from-echo"]);
    assert!(
        send_out.status.success(),
        "slopctl send failed: {:?}",
        send_out
    );

    // Poll pane output for the echo response.
    let deadline = Instant::now() + Duration::from_secs(5);
    let found = loop {
        let out = env
            .tmux
            .tmux()
            .args(["capture-pane", "-t", &pane_id, "-p"])
            .output()
            .unwrap();
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
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let managed_pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(listener, &managed_pane_id);

    // Now spawn an *unmanaged* mock_claude in the "test" session (not the "slopd" session).
    // It will read the same settings.json with the injected hooks and fire SessionStart
    // on startup, sending hook events to slopd even though it is not managed.
    let unmanaged_out = env
        .tmux
        .tmux()
        .args([
            "new-window",
            "-t",
            "test",
            "-P",
            "-F",
            "#{pane_id}",
            &mock_claude_path,
        ])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .env("CLAUDE_CONFIG_DIR", &claude_config_dir)
        .output()
        .expect("failed to spawn unmanaged mock_claude pane");
    assert!(
        unmanaged_out.status.success(),
        "failed to create unmanaged pane: {:?}",
        unmanaged_out
    );
    let unmanaged_pane_id = String::from_utf8_lossy(&unmanaged_out.stdout)
        .trim()
        .to_string();

    // Start a listener that receives all events (no filters).
    let mut listen = Command::new(cargo_bin("slopctl"))
        .args(["listen"])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl listen");

    let stdout = listen.stdout.take().unwrap();

    // Read and discard the subscription confirmation line.
    let (subscribed_line, reader) = read_line_timeout(stdout, Duration::from_secs(10))
        .expect("timed out reading subscribed line");
    assert!(
        subscribed_line.contains("subscribed"),
        "unexpected first line: {:?}",
        subscribed_line
    );

    // Fire a hook event pretending to come from the unmanaged pane.
    let payload = r#"{"session_id":"unmanaged-session","hook_event_name":"UserPromptSubmit","prompt":"from outside"}"#.to_string();
    let hook_out = fire_hook(&env, "UserPromptSubmit", &payload, Some(&unmanaged_pane_id));
    assert!(
        hook_out.status.success(),
        "hook from unmanaged pane failed: {:?}",
        hook_out
    );

    // Also fire from the managed pane so the listener has something to read
    // (if the unmanaged event is correctly suppressed).
    let managed_payload = r#"{"session_id":"mock-session-id-1234","hook_event_name":"UserPromptSubmit","prompt":"from managed"}"#;
    let hook_out = fire_hook(
        &env,
        "UserPromptSubmit",
        managed_payload,
        Some(&managed_pane_id),
    );
    assert!(
        hook_out.status.success(),
        "hook from managed pane failed: {:?}",
        hook_out
    );

    let (event, _reader) = read_next_hook_event(reader);

    kill_child(listen);
    kill_slopd(slopd);

    // The event from the unmanaged pane should have been silently dropped.
    // The first hook event we receive must be from the managed pane.
    assert_eq!(
        event["pane_id"].as_str().unwrap(),
        managed_pane_id,
        "Expected slopd to ignore the unmanaged pane's event, but got pane_id={:?}",
        event["pane_id"],
    );
    assert_eq!(event["payload"]["prompt"], "from managed");
}

/// A `SessionEnd` hook from a pane slopd does not manage must be answered
/// *immediately*, not after the `PANE_REGISTRATION_WAIT` grace.
///
/// Regression test for "SessionEnd hook [slopctl hook SessionEnd] failed: Hook
/// cancelled" on exit. Every Claude that shares the global settings.json fires
/// `slopctl hook SessionEnd` when it exits — including sessions slopd never
/// spawned. For an unmanaged pane, slopd used to wait the full
/// PANE_REGISTRATION_WAIT (2s) for a registration that can never arrive; but
/// Claude Code cancels a SessionEnd hook that runs past its 1.5s budget, so the
/// wait guaranteed a cancelled hook and the on-screen error. SessionEnd is
/// terminal — an unmanaged pane will never register — so it must be answered at
/// once. A non-terminal hook from the same unmanaged pane still pays the grace,
/// proving the fast path is specific to SessionEnd rather than a blanket removal.
#[test]
fn session_end_from_unmanaged_pane_answers_immediately() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };
    let slopd = env.spawn_slopd();

    // A pane id slopd has never seen. Without the fix this SessionEnd blocks for
    // PANE_REGISTRATION_WAIT (2s) waiting for a registration that never comes.
    let session_end = r#"{"session_id":"outside-session","hook_event_name":"SessionEnd","reason":"prompt_input_exit","transcript_path":"/dev/null","cwd":"/tmp"}"#;
    let start = Instant::now();
    let out = fire_hook(&env, "SessionEnd", session_end, Some("%987654"));
    let elapsed = start.elapsed();

    // Contrast: a non-terminal hook from the same unmanaged pane still waits the
    // full registration grace, so the fast path above is SessionEnd-specific.
    let user_prompt =
        r#"{"session_id":"outside-session","hook_event_name":"UserPromptSubmit","prompt":"x"}"#;
    let up_start = Instant::now();
    let up_out = fire_hook(&env, "UserPromptSubmit", user_prompt, Some("%987654"));
    let up_elapsed = up_start.elapsed();

    kill_slopd(slopd);

    assert!(out.status.success(), "SessionEnd hook failed: {:?}", out);
    assert!(
        elapsed < Duration::from_millis(1000),
        "SessionEnd from an unmanaged pane must return well under Claude's 1.5s \
         SessionEnd budget; took {:?} (regression: PANE_REGISTRATION_WAIT applied)",
        elapsed,
    );
    assert!(
        up_out.status.success(),
        "UserPromptSubmit hook failed: {:?}",
        up_out
    );
    assert!(
        up_elapsed >= Duration::from_millis(1500),
        "a non-terminal hook from an unmanaged pane should still wait for \
         registration (~PANE_REGISTRATION_WAIT); took {:?}",
        up_elapsed,
    );
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
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

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

    // Read and discard the subscription confirmation line.
    let (subscribed_line, reader) = read_line_timeout(stdout, Duration::from_secs(10))
        .expect("timed out reading subscribed line");
    assert!(
        subscribed_line.contains("subscribed"),
        "unexpected first line: {:?}",
        subscribed_line
    );

    // Fire a hook from the pre-existing managed pane.
    let payload =
        r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"after restart"}"#;
    let hook_out = fire_hook(&env, "UserPromptSubmit", payload, Some(&pane_id));
    assert!(hook_out.status.success(), "hook failed: {:?}", hook_out);

    let (event, _reader) = read_next_hook_event(reader);

    kill_child(listen);
    kill_slopd(slopd2);

    assert_eq!(event["pane_id"], pane_id.as_str());
    assert_eq!(event["payload"]["prompt"], "after restart");
}

/// Read a single line from a reader, returning an error if no line arrives within `timeout`.
///
/// Moves reading into a background thread so that a blocking `read_line` cannot hang
/// the test forever.
fn read_line_timeout(
    reader: impl std::io::Read + Send + 'static,
    timeout: Duration,
) -> Result<(String, Box<dyn std::io::Read + Send>), std::sync::mpsc::RecvTimeoutError> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf_reader = std::io::BufReader::new(reader);
        let mut line = String::new();
        let result = buf_reader.read_line(&mut line);
        let _ = tx.send((line, result, buf_reader));
    });
    let (line, result, buf_reader) = rx.recv_timeout(timeout)?;
    result.expect("read_line failed");
    Ok((line, Box::new(buf_reader)))
}

/// Read lines from a reader until a hook event (source == "hook") is found and return it.
/// Skips slopd-internal events (StateChange, DetailedStateChange) which may arrive interleaved.
/// Panics after 10 seconds if no matching event arrives.
fn read_next_hook_event(
    reader: impl std::io::Read + Send + 'static,
) -> (serde_json::Value, Box<dyn std::io::Read + Send>) {
    let timeout = Duration::from_secs(10);
    let mut reader: Box<dyn std::io::Read + Send> = Box::new(reader);
    loop {
        let (line, next_reader) =
            read_line_timeout(reader, timeout).expect("timed out waiting for hook event");
        reader = next_reader;
        let v: serde_json::Value =
            serde_json::from_str(line.trim()).expect("event is not valid JSON");
        if v["source"] == "hook" {
            return (v, reader);
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
    assert_eq!(
        detailed, expected_detailed,
        "detailed_state mismatch after {} hook",
        event
    );
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

    // --mock-session-start=skip prevents mock_claude from firing SessionStart on startup,
    // so we can assert booting_up before any hook fires.
    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path, "--mock-session-start=skip"]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    // mock_claude is running but has not fired SessionStart: state must be booting_up
    let (state, detailed) = env.pane_state(&pane_id);
    assert_eq!(state, libslop::PaneState::BootingUp);
    assert_eq!(detailed, libslop::PaneDetailedState::BootingUp);

    // Fire SessionStart directly via slopctl hook (bypasses Send machinery, so the
    // BootingUp state guard does not block it). SessionStart → Ready.
    let payload = r#"{"session_id":"mock-session-id-1234","hook_event_name":"SessionStart","transcript_path":"/dev/null","cwd":"/tmp","source":"startup","model":"mock"}"#.to_string();
    let hook_out = fire_hook(&env, "SessionStart", &payload, Some(&pane_id));
    assert!(
        hook_out.status.success(),
        "fire SessionStart hook failed: {:?}",
        hook_out
    );
    std::thread::sleep(Duration::from_millis(100));

    let (state, detailed) = env.pane_state(&pane_id);
    assert_eq!(state, libslop::PaneState::Ready);
    assert_eq!(detailed, libslop::PaneDetailedState::Ready);

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
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    let base = |hook: &str| {
        format!(
            r#"{{"session_id":"s1","hook_event_name":"{}","transcript_path":"/dev/null","cwd":"/tmp"}}"#,
            hook
        )
    };

    // SessionStart → ready
    assert_state_after_hook(
        &env,
        &pane_id,
        "SessionStart",
        &base("SessionStart"),
        libslop::PaneState::Ready,
        libslop::PaneDetailedState::Ready,
    );

    // UserPromptSubmit → busy / busy_processing
    assert_state_after_hook(
        &env,
        &pane_id,
        "UserPromptSubmit",
        &base("UserPromptSubmit"),
        libslop::PaneState::Busy,
        libslop::PaneDetailedState::BusyProcessing,
    );

    // PreToolUse → busy / busy_tool_use
    assert_state_after_hook(
        &env,
        &pane_id,
        "PreToolUse",
        &base("PreToolUse"),
        libslop::PaneState::Busy,
        libslop::PaneDetailedState::BusyToolUse,
    );

    // PermissionRequest → awaiting_input / awaiting_input_permission
    assert_state_after_hook(
        &env,
        &pane_id,
        "PermissionRequest",
        &base("PermissionRequest"),
        libslop::PaneState::AwaitingInput,
        libslop::PaneDetailedState::AwaitingInputPermission,
    );

    // PostToolUse → busy / busy_processing
    assert_state_after_hook(
        &env,
        &pane_id,
        "PostToolUse",
        &base("PostToolUse"),
        libslop::PaneState::Busy,
        libslop::PaneDetailedState::BusyProcessing,
    );

    // Elicitation → awaiting_input / awaiting_input_elicitation
    assert_state_after_hook(
        &env,
        &pane_id,
        "Elicitation",
        &base("Elicitation"),
        libslop::PaneState::AwaitingInput,
        libslop::PaneDetailedState::AwaitingInputElicitation,
    );

    // ElicitationResult → busy / busy_processing
    assert_state_after_hook(
        &env,
        &pane_id,
        "ElicitationResult",
        &base("ElicitationResult"),
        libslop::PaneState::Busy,
        libslop::PaneDetailedState::BusyProcessing,
    );

    // SubagentStart → busy / busy_subagent
    assert_state_after_hook(
        &env,
        &pane_id,
        "SubagentStart",
        &base("SubagentStart"),
        libslop::PaneState::Busy,
        libslop::PaneDetailedState::BusySubagent,
    );

    // SubagentStop → busy / busy_processing
    assert_state_after_hook(
        &env,
        &pane_id,
        "SubagentStop",
        &base("SubagentStop"),
        libslop::PaneState::Busy,
        libslop::PaneDetailedState::BusyProcessing,
    );

    // PreCompact → busy / busy_compacting
    assert_state_after_hook(
        &env,
        &pane_id,
        "PreCompact",
        &base("PreCompact"),
        libslop::PaneState::Busy,
        libslop::PaneDetailedState::BusyCompacting,
    );

    // PostCompact → busy / busy_processing
    assert_state_after_hook(
        &env,
        &pane_id,
        "PostCompact",
        &base("PostCompact"),
        libslop::PaneState::Busy,
        libslop::PaneDetailedState::BusyProcessing,
    );

    // Stop → ready
    assert_state_after_hook(
        &env,
        &pane_id,
        "Stop",
        &base("Stop"),
        libslop::PaneState::Ready,
        libslop::PaneDetailedState::Ready,
    );

    // StopFailure → ready
    assert_state_after_hook(
        &env,
        &pane_id,
        "StopFailure",
        &base("StopFailure"),
        libslop::PaneState::Ready,
        libslop::PaneDetailedState::Ready,
    );

    kill_slopd(slopd);
}

/// Regression: a turn that ends without a clean `Stop` (e.g. `SubagentStop`
/// after a `/clear`-over-busy race) leaves the pane busy. Claude then fires a
/// `Notification` hook with `notification_type: "idle_prompt"` ("Claude is
/// waiting for your input"). slopd must treat that as the pane returning to
/// Ready — otherwise it stays stuck busy forever (the %40/%46/%64 freeze
/// reproduced 2026-05-17, where no Stop/turn_duration ever followed).
#[test]
fn notification_idle_prompt_unsticks_busy_pane() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    let base = |hook: &str| {
        format!(
            r#"{{"session_id":"s1","hook_event_name":"{}","transcript_path":"/dev/null","cwd":"/tmp"}}"#,
            hook
        )
    };

    // ready → busy via UserPromptSubmit, then the turn "ends" via SubagentStop
    // (still busy) with no Stop ever arriving — the reproduced stuck shape.
    assert_state_after_hook(
        &env,
        &pane_id,
        "SessionStart",
        &base("SessionStart"),
        libslop::PaneState::Ready,
        libslop::PaneDetailedState::Ready,
    );
    assert_state_after_hook(
        &env,
        &pane_id,
        "UserPromptSubmit",
        &base("UserPromptSubmit"),
        libslop::PaneState::Busy,
        libslop::PaneDetailedState::BusyProcessing,
    );
    assert_state_after_hook(
        &env,
        &pane_id,
        "SubagentStop",
        &base("SubagentStop"),
        libslop::PaneState::Busy,
        libslop::PaneDetailedState::BusyProcessing,
    );

    // Claude announces it is idle and waiting for input → pane must be Ready.
    let idle_payload = r#"{"session_id":"s1","hook_event_name":"Notification","notification_type":"idle_prompt","message":"Claude is waiting for your input","transcript_path":"/dev/null","cwd":"/tmp"}"#;
    assert_state_after_hook(
        &env,
        &pane_id,
        "Notification",
        idle_payload,
        libslop::PaneState::Ready,
        libslop::PaneDetailedState::Ready,
    );

    kill_slopd(slopd);
}

/// Guard: only `idle_prompt` Notifications return the pane to Ready. A
/// Notification of any other type must NOT spuriously clear a busy/awaiting
/// state (keeps the fix from over-broadening).
#[test]
fn notification_non_idle_does_not_unstick() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    let base = |hook: &str| {
        format!(
            r#"{{"session_id":"s1","hook_event_name":"{}","transcript_path":"/dev/null","cwd":"/tmp"}}"#,
            hook
        )
    };

    assert_state_after_hook(
        &env,
        &pane_id,
        "SessionStart",
        &base("SessionStart"),
        libslop::PaneState::Ready,
        libslop::PaneDetailedState::Ready,
    );
    assert_state_after_hook(
        &env,
        &pane_id,
        "UserPromptSubmit",
        &base("UserPromptSubmit"),
        libslop::PaneState::Busy,
        libslop::PaneDetailedState::BusyProcessing,
    );

    // A non-idle Notification must leave the busy state untouched.
    let other_payload = r#"{"session_id":"s1","hook_event_name":"Notification","notification_type":"other","message":"something else","transcript_path":"/dev/null","cwd":"/tmp"}"#;
    assert_state_after_hook(
        &env,
        &pane_id,
        "Notification",
        other_payload,
        libslop::PaneState::Busy,
        libslop::PaneDetailedState::BusyProcessing,
    );

    kill_slopd(slopd);
}

#[test]
fn pane_state_preserves_last_known_state_on_slopd_restart() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    // Advance to ready via SessionStart
    let payload = r#"{"session_id":"s1","hook_event_name":"SessionStart","transcript_path":"/dev/null","cwd":"/tmp"}"#;
    assert_state_after_hook(
        &env,
        &pane_id,
        "SessionStart",
        payload,
        libslop::PaneState::Ready,
        libslop::PaneDetailedState::Ready,
    );

    // Restart slopd
    kill_slopd(slopd);
    let slopd2 = env.spawn_slopd();

    // With no transcript records to replay, recovery falls back to the durable
    // tmux state instead of leaving a live pane stuck in booting_up.
    std::thread::sleep(Duration::from_millis(100));
    let (state, detailed) = env.pane_state(&pane_id);
    assert_eq!(
        state,
        libslop::PaneState::Ready,
        "expected ready after restart"
    );
    assert_eq!(
        detailed,
        libslop::PaneDetailedState::Ready,
        "expected ready after restart"
    );

    // Fire SessionStart again to confirm normal transitions still work after restart
    assert_state_after_hook(
        &env,
        &pane_id,
        "SessionStart",
        payload,
        libslop::PaneState::Ready,
        libslop::PaneDetailedState::Ready,
    );

    kill_slopd(slopd2);
}

/// Regression test for the race between the Run handler and a concurrently-firing
/// SessionStart hook that caused the pane to stay stuck in booting_up.
///
/// The bug: after `tmux new-window` the Run handler inserts the pane into
/// managed_panes (making hooks eligible) and then awaits two tmux set-option
/// calls.  A fast-starting child (mock_claude) can fire its SessionStart hook
/// during those awaits; the hook handler sets state → Ready and broadcasts the
/// event.  When the Run handler resumed it called set_pane_detailed_state(BootingUp)
/// unconditionally, resetting Ready back to BootingUp.  Any subsequent
/// slopctl send then timed out waiting for the pane to become ready.
///
/// The fix guards the set_pane_detailed_state(BootingUp) call so it is skipped
/// when the state has already been advanced by a concurrent hook.
///
/// This test makes the race deterministic by setting SLOPD_TEST_RUN_YIELD_MS,
/// which adds a 2-second async sleep inside the Run handler right before the
/// guard.  mock_claude always fires SessionStart within that window, so the
/// hook is guaranteed to run (and set state → Ready) before the guard runs.
#[test]
fn run_handler_does_not_reset_pane_state_on_concurrent_hook() {
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

    // 2 000 ms is ample time for mock_claude to start and fire SessionStart.
    let slopd = env.spawn_slopd_with_run_yield(2000);

    // Subscribe before running so no SessionStart event is missed.
    let listener = env.spawn_session_start_listener();

    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    // Blocks until the SessionStart broadcast is received.  By the time slopctl
    // run returned, the Run handler (including its guard) has already completed,
    // so both sides of the race have settled.
    env.wait_for_session_start(listener, &pane_id);

    // State must be Ready.  Without the guard the Run handler would have reset
    // it back to BootingUp after the hook set it to Ready.
    let (state, detailed) = env.pane_state(&pane_id);
    assert_eq!(
        state,
        libslop::PaneState::Ready,
        "pane should be Ready after SessionStart but got {:?} / {:?}",
        state,
        detailed
    );

    // Confirm that slopctl send completes without waiting for a ready transition.
    let send_out = env.slopctl(&["send", &pane_id, "hello", "--timeout", "5"]);
    assert!(
        send_out.status.success(),
        "slopctl send should succeed immediately when pane is Ready: {:?}",
        send_out
    );

    kill_slopd(slopd);
}
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
        stdout
            .read_exact(&mut buf)
            .expect("failed to read subscription confirmation");
        if buf[0] == b'\n' {
            break;
        }
        line.push(buf[0]);
    }
    let line = String::from_utf8_lossy(&line);
    assert!(
        line.contains("subscribed"),
        "unexpected first line from slopctl listen: {:?}",
        line
    );
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
                Ok(0) | Err(_) => {
                    let _ = tx.send(None);
                    return;
                }
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
    let event = rx
        .recv_timeout(Duration::from_secs(10))
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
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

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
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

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
        stdout
            .read_exact(&mut buf)
            .expect("failed to read subscription confirmation");
        if buf[0] == b'\n' {
            break;
        }
        line.push(buf[0]);
    }
    let line = String::from_utf8_lossy(&line);
    assert!(
        line.contains("subscribed"),
        "unexpected first line from slopctl listen: {:?}",
        line
    );
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
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

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

/// Verify that slopctl send succeeds when the pane is busy (BusyToolUse).
/// Real Claude queues input during tool use and fires UserPromptSubmit once it
/// returns to the prompt — mock_claude's `::mock busy <duration>` command simulates this.
#[test]
fn send_succeeds_when_pane_is_busy() {
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

    let listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(listener, &pane_id);

    // Send ::mock busy 2s in a background thread — this fires PreToolUse, then blocks
    // waiting for the queued prompt, sleeps 2s, then fires UserPromptSubmit.
    let env2 = env.clone();
    let pane_id2 = pane_id.clone();
    let busy_thread =
        std::thread::spawn(move || env2.slopctl(&["send", &pane_id2, "::mock busy 2s"]));

    // Wait until the pane enters BusyToolUse state.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let (_, detailed) = env.pane_state(&pane_id);
        if detailed == libslop::PaneDetailedState::BusyToolUse {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for BusyToolUse state"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Now send a prompt while the pane is busy. The prompt is queued by mock_claude
    // and UserPromptSubmit fires ~2s later when the tool use finishes.
    let start = Instant::now();
    let send_out = env.slopctl(&["send", &pane_id, "hello while busy", "--timeout", "10"]);
    let elapsed = start.elapsed();

    let _ = busy_thread.join();
    kill_slopd(slopd);

    assert!(
        send_out.status.success(),
        "send while busy failed: {:?}",
        send_out
    );
    // Should have taken roughly 2s (the busy duration), not 10s (the timeout).
    assert!(
        elapsed < Duration::from_secs(8),
        "send while busy took {:?}, should have completed within the busy period",
        elapsed,
    );
}

/// Regression test for issue #15: send to a pane in AwaitingInputPermission state must fail
/// fast rather than waiting the full timeout. Keystrokes go to the permission dialog, not
/// the chat prompt, so UserPromptSubmit will never fire.
#[test]
fn send_fails_fast_when_pane_awaiting_permission() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    // Advance pane to AwaitingInputPermission via PermissionRequest hook.
    let base = |hook: &str| {
        format!(
            r#"{{"session_id":"s1","hook_event_name":"{}","transcript_path":"/dev/null","cwd":"/tmp"}}"#,
            hook
        )
    };
    assert_state_after_hook(
        &env,
        &pane_id,
        "SessionStart",
        &base("SessionStart"),
        libslop::PaneState::Ready,
        libslop::PaneDetailedState::Ready,
    );
    assert_state_after_hook(
        &env,
        &pane_id,
        "PermissionRequest",
        &base("PermissionRequest"),
        libslop::PaneState::AwaitingInput,
        libslop::PaneDetailedState::AwaitingInputPermission,
    );

    // With the pane at a permission dialog, send should fail immediately rather than
    // waiting the full timeout. Keystrokes go to the dialog, not the chat prompt.
    let timeout_secs = 5u64;
    let start = Instant::now();
    let output = env.slopctl(&[
        "send",
        &pane_id,
        "hello",
        "--timeout",
        &timeout_secs.to_string(),
    ]);
    let elapsed = start.elapsed();

    kill_slopd(slopd);

    assert!(
        !output.status.success(),
        "send to pane awaiting permission should have failed: {:?}",
        output
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("timed out"),
        "send should have failed fast (state check), not timed out: {:?}",
        stderr
    );
    assert!(
        elapsed < Duration::from_secs(timeout_secs - 1),
        "send to pane awaiting permission took {:?}, expected fast failure (issue #15)",
        elapsed,
    );
}

/// Issue #17 part 2: send to a BootingUp pane should wait for Ready rather than
/// failing immediately. Once SessionStart fires and the pane becomes Ready, the
/// send should complete successfully.
#[test]
fn send_waits_for_ready_when_pane_is_booting_up() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    // --mock-session-start=skip keeps mock_claude in BootingUp until we explicitly trigger it.
    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path, "--mock-session-start=skip"]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let env = Arc::new(env);
    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    // Confirm pane is BootingUp before proceeding.
    let (_, detailed) = env.pane_state(&pane_id);
    assert_eq!(
        detailed,
        libslop::PaneDetailedState::BootingUp,
        "expected BootingUp before SessionStart"
    );

    // Start send in a background thread — it should block waiting for Ready.
    let env2 = env.clone();
    let pane_id2 = pane_id.clone();
    let send_thread = std::thread::spawn(move || {
        env2.slopctl(&["send", &pane_id2, "hello after boot", "--timeout", "10"])
    });

    // Give send a moment to start blocking, then trigger SessionStart directly
    // via slopctl hook (bypasses send machinery, works regardless of pane state).
    std::thread::sleep(Duration::from_millis(200));

    let payload = r#"{"session_id":"mock-session-id-1234","hook_event_name":"SessionStart","transcript_path":"/dev/null","cwd":"/tmp","source":"startup","model":"mock"}"#.to_string();
    let hook_out = fire_hook(&env, "SessionStart", &payload, Some(&pane_id));
    assert!(
        hook_out.status.success(),
        "fire SessionStart hook failed: {:?}",
        hook_out
    );

    let send_out = send_thread.join().unwrap();

    kill_slopd(slopd);

    assert!(
        send_out.status.success(),
        "send should have succeeded after pane became ready: {:?}",
        send_out
    );
}

/// Issue #17 part 1: send --interrupt should interrupt a busy pane then deliver
/// the prompt, succeeding where a plain send would be stuck waiting.
#[test]
fn send_with_interrupt_preempts_busy_pane() {
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

    let listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(listener, &pane_id);

    // Put mock_claude into a long busy state (30s) in the background.
    let env2 = env.clone();
    let pane_id2 = pane_id.clone();
    let busy_thread =
        std::thread::spawn(move || env2.slopctl(&["send", &pane_id2, "::mock busy 30s"]));

    // Wait until pane is BusyToolUse.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let (_, detailed) = env.pane_state(&pane_id);
        if detailed == libslop::PaneDetailedState::BusyToolUse {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for BusyToolUse state"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Subscribe to UserPromptSubmit now — the `::mock busy` prompt already fired its own
    // (PreToolUse, hence BusyToolUse, comes after it), so the only prompt we
    // capture here is the interrupt-delivered one.
    let mut listen = Command::new(cargo_bin("slopctl"))
        .args(["listen", "--hook", "UserPromptSubmit"])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl listen");
    let listen_stdout = listen.stdout.take().unwrap();
    let (subscribed, reader) = read_line_timeout(listen_stdout, Duration::from_secs(10))
        .expect("timed out reading subscribed line");
    assert!(
        subscribed.contains("subscribed"),
        "unexpected first line: {:?}",
        subscribed
    );

    // send --interrupt should interrupt the busy pane and deliver the prompt.
    let start = Instant::now();
    let send_out = env.slopctl(&[
        "send",
        "--interrupt",
        &pane_id,
        "hello after interrupt",
        "--timeout",
        "10",
    ]);
    let elapsed = start.elapsed();

    let (line, _reader) = read_line_timeout(reader, Duration::from_secs(10))
        .expect("timed out reading UserPromptSubmit event");

    let _ = busy_thread.join();
    kill_child(listen);
    kill_slopd(slopd);

    assert!(
        send_out.status.success(),
        "send --interrupt failed: {:?}",
        send_out
    );
    // Should complete quickly (interrupt fires immediately), not wait the full 30s.
    assert!(
        elapsed < Duration::from_secs(8),
        "send --interrupt took {:?}",
        elapsed
    );
    // And the prompt must arrive verbatim — the interrupt keystrokes must not
    // corrupt it (the old sequence ate the first character).
    let event: serde_json::Value =
        serde_json::from_str(line.trim()).expect("event is not valid JSON");
    let got = event["payload"]["prompt"]
        .as_str()
        .expect("prompt is a string");
    assert_eq!(
        got.trim_end_matches('\n'),
        "hello after interrupt",
        "interrupt-delivered prompt was corrupted"
    );
}

/// Regression (%121): `send --interrupt` on an idle pane must deliver the prompt
/// verbatim. The old interrupt sequence (C-c, C-d, Escape) left the Escape glued
/// to the first typed character, so the terminal read the pair as an escape
/// sequence and swallowed that character — "Reply…" arrived as "eply…" — and
/// against a pane with no in-flight activity the send timed out entirely.
#[test]
fn send_interrupt_delivers_prompt_verbatim_on_idle_pane() {
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

    let listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(listener, &pane_id);

    // Subscribe to UserPromptSubmit so we can read the prompt Claude actually saw.
    let mut listen = Command::new(cargo_bin("slopctl"))
        .args(["listen", "--hook", "UserPromptSubmit"])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl listen");
    let listen_stdout = listen.stdout.take().unwrap();
    let timeout = Duration::from_secs(10);
    let (subscribed, reader) =
        read_line_timeout(listen_stdout, timeout).expect("timed out reading subscribed line");
    assert!(
        subscribed.contains("subscribed"),
        "unexpected first line: {:?}",
        subscribed
    );

    // The leading 'R' is exactly the character the old interrupt sequence ate.
    let prompt = "Reply with exactly the word ZULU.";
    let send_out = env.slopctl(&["send", "--interrupt", &pane_id, prompt, "--timeout", "10"]);
    assert!(
        send_out.status.success(),
        "send --interrupt failed: {:?}",
        send_out
    );

    let (line, _reader) =
        read_line_timeout(reader, timeout).expect("timed out reading UserPromptSubmit event");
    kill_child(listen);
    kill_slopd(slopd);

    let event: serde_json::Value =
        serde_json::from_str(line.trim()).expect("event is not valid JSON");
    assert_eq!(event["event_type"], "UserPromptSubmit");
    // Trailing newline is a mock-only artifact of Alternating newline mode (the
    // first of slopd's retry Enters is modeled as a literal newline); real Claude
    // has none. The bug under test corrupts the START of the prompt, not the end.
    let got = event["payload"]["prompt"]
        .as_str()
        .expect("prompt is a string");
    assert_eq!(
        got.trim_end_matches('\n'),
        prompt,
        "interrupt-delivered prompt was corrupted (first character eaten?)"
    );
}

/// Regression: `send` must clear any residual input before typing, so the prompt
/// is submitted verbatim rather than concatenated onto a stale draft or a ghosted
/// autocomplete suggestion left in the input box.
#[test]
fn send_clears_stale_input_before_typing() {
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

    let listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(listener, &pane_id);

    // Leave a stale draft in the input box: typed straight to the pane, never
    // submitted (no Enter). Byte order in the pty guarantees the mock reads this
    // before slopd's clear + prompt.
    let typed = env
        .tmux
        .tmux()
        .args(["send-keys", "-t", &pane_id, "-l", "STALE_DRAFT_"])
        .output()
        .expect("tmux send-keys failed");
    assert!(typed.status.success(), "tmux send-keys failed: {:?}", typed);

    let mut listen = Command::new(cargo_bin("slopctl"))
        .args(["listen", "--hook", "UserPromptSubmit"])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl listen");
    let listen_stdout = listen.stdout.take().unwrap();
    let timeout = Duration::from_secs(10);
    let (subscribed, reader) =
        read_line_timeout(listen_stdout, timeout).expect("timed out reading subscribed line");
    assert!(
        subscribed.contains("subscribed"),
        "unexpected first line: {:?}",
        subscribed
    );

    let prompt = "clean prompt END.";
    let send_out = env.slopctl(&["send", &pane_id, prompt, "--timeout", "10"]);
    assert!(send_out.status.success(), "send failed: {:?}", send_out);

    let (line, _reader) =
        read_line_timeout(reader, timeout).expect("timed out reading UserPromptSubmit event");
    kill_child(listen);
    kill_slopd(slopd);

    let event: serde_json::Value =
        serde_json::from_str(line.trim()).expect("event is not valid JSON");
    let got = event["payload"]["prompt"]
        .as_str()
        .expect("prompt is a string");
    assert_eq!(
        got.trim_end_matches('\n'),
        prompt,
        "prompt was concatenated onto stale input instead of replacing it"
    );
}

/// Helper: create a raw tmux pane in the "test" session that slopd has never seen.
fn spawn_unmanaged_pane(env: &TestEnv) -> String {
    let out = env
        .tmux
        .tmux()
        .args([
            "new-window",
            "-t",
            "test",
            "-P",
            "-F",
            "#{pane_id}",
            "sleep",
            "infinity",
        ])
        .output()
        .expect("failed to create unmanaged pane");
    assert!(
        out.status.success(),
        "failed to create unmanaged tmux pane: {:?}",
        out
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Helper: create a raw tmux pane directly inside the slopd session, bypassing slopctl run.
/// slopd was already running when the pane was created, so it is not in managed_panes.
fn spawn_unmanaged_pane_in_slopd_session(env: &TestEnv) -> String {
    let out = env
        .tmux
        .tmux()
        .args([
            "new-window",
            "-t",
            "slopd",
            "-P",
            "-F",
            "#{pane_id}",
            "sleep",
            "infinity",
        ])
        .output()
        .expect("failed to create pane in slopd session");
    assert!(
        out.status.success(),
        "failed to create tmux pane in slopd session: {:?}",
        out
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn kill_unmanaged_pane_returns_error() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    let unmanaged = spawn_unmanaged_pane(&env);

    let out = env.slopctl(&["kill", &unmanaged]);

    kill_slopd(slopd);

    assert!(
        !out.status.success(),
        "kill on unmanaged pane should have failed: {:?}",
        out
    );
}

#[test]
fn send_unmanaged_pane_returns_error() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    let unmanaged = spawn_unmanaged_pane(&env);

    let out = env.slopctl(&["send", &unmanaged, "hello"]);

    kill_slopd(slopd);

    assert!(
        !out.status.success(),
        "send to unmanaged pane should have failed: {:?}",
        out
    );
}

#[test]
fn interrupt_unmanaged_pane_returns_error() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    let unmanaged = spawn_unmanaged_pane(&env);

    let out = env.slopctl(&["interrupt", &unmanaged]);

    kill_slopd(slopd);

    assert!(
        !out.status.success(),
        "interrupt on unmanaged pane should have failed: {:?}",
        out
    );
}

#[test]
fn tag_unmanaged_pane_returns_error() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    let unmanaged = spawn_unmanaged_pane(&env);

    let out = env.slopctl(&["tag", &unmanaged, "mylabel"]);

    kill_slopd(slopd);

    assert!(
        !out.status.success(),
        "tag on unmanaged pane should have failed: {:?}",
        out
    );
}

#[test]
fn untag_unmanaged_pane_returns_error() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    let unmanaged = spawn_unmanaged_pane(&env);

    let out = env.slopctl(&["untag", &unmanaged, "mylabel"]);

    kill_slopd(slopd);

    assert!(
        !out.status.success(),
        "untag on unmanaged pane should have failed: {:?}",
        out
    );
}

#[test]
fn kill_pane_in_slopd_session_not_via_run_returns_error() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    let pane = spawn_unmanaged_pane_in_slopd_session(&env);

    let out = env.slopctl(&["kill", &pane]);

    kill_slopd(slopd);

    assert!(
        !out.status.success(),
        "kill on slopd-session pane not registered via run should fail: {:?}",
        out
    );
}

#[test]
fn send_pane_in_slopd_session_not_via_run_returns_error() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    let pane = spawn_unmanaged_pane_in_slopd_session(&env);

    let out = env.slopctl(&["send", &pane, "hello"]);

    kill_slopd(slopd);

    assert!(
        !out.status.success(),
        "send to slopd-session pane not registered via run should fail: {:?}",
        out
    );
}

#[test]
fn interrupt_pane_in_slopd_session_not_via_run_returns_error() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    let pane = spawn_unmanaged_pane_in_slopd_session(&env);

    let out = env.slopctl(&["interrupt", &pane]);

    kill_slopd(slopd);

    assert!(
        !out.status.success(),
        "interrupt on slopd-session pane not registered via run should fail: {:?}",
        out
    );
}

#[test]
fn tag_pane_in_slopd_session_not_via_run_returns_error() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    let pane = spawn_unmanaged_pane_in_slopd_session(&env);

    let out = env.slopctl(&["tag", &pane, "mylabel"]);

    kill_slopd(slopd);

    assert!(
        !out.status.success(),
        "tag on slopd-session pane not registered via run should fail: {:?}",
        out
    );
}

#[test]
fn untag_pane_in_slopd_session_not_via_run_returns_error() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    let pane = spawn_unmanaged_pane_in_slopd_session(&env);

    let out = env.slopctl(&["untag", &pane, "mylabel"]);

    kill_slopd(slopd);

    assert!(
        !out.status.success(),
        "untag on slopd-session pane not registered via run should fail: {:?}",
        out
    );
}

/// Verify that slopd tails transcript files and broadcasts records via the event system.
#[test]
fn transcript_events_received_via_listen() {
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

    // Subscribe to transcript user+assistant events.
    let mut listener = Command::new(cargo_bin("slopctl"))
        .args([
            "listen",
            "--transcript",
            "user",
            "--transcript",
            "assistant",
        ])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn transcript listener");

    // Wait for subscription confirmation.
    {
        let stdout = listener.stdout.as_mut().unwrap();
        let mut line = Vec::new();
        let mut buf = [0u8; 1];
        loop {
            use std::io::Read;
            stdout
                .read_exact(&mut buf)
                .expect("failed to read subscription confirmation");
            if buf[0] == b'\n' {
                break;
            }
            line.push(buf[0]);
        }
        let line = String::from_utf8_lossy(&line);
        assert!(
            line.contains("subscribed"),
            "unexpected first line: {:?}",
            line
        );
    }

    // Spawn the pane and wait for SessionStart.
    let session_listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(session_listener, &pane_id);

    // Send a prompt — mock_claude writes user + assistant transcript records.
    let send_output = env.slopctl(&["send", &pane_id, "hello transcript"]);
    assert!(
        send_output.status.success(),
        "slopctl send failed: {:?}",
        send_output
    );

    // Read transcript events from the listener in a background thread with timeout.
    let stdout = listener.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<Vec<serde_json::Value>>();
    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stdout);
        let mut events = Vec::new();
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
                && v.get("source").and_then(|s| s.as_str()) == Some("transcript")
            {
                events.push(v);
                if events.len() >= 2 {
                    let _ = tx.send(events);
                    return;
                }
            }
        }
        let _ = tx.send(events);
    });

    let events = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("timed out waiting for transcript events");

    kill_child(listener);
    kill_slopd(slopd);

    assert!(
        events.len() >= 2,
        "expected at least 2 transcript events, got {}: {:?}",
        events.len(),
        events
    );

    // Check we got a user and an assistant event.
    let types: Vec<&str> = events
        .iter()
        .filter_map(|e| e.get("event_type").and_then(|t| t.as_str()))
        .collect();
    assert!(
        types.contains(&"user"),
        "missing 'user' transcript event, got: {:?}",
        types
    );
    assert!(
        types.contains(&"assistant"),
        "missing 'assistant' transcript event, got: {:?}",
        types
    );

    // Verify pane_id is set on the events.
    for ev in &events {
        assert_eq!(
            ev.get("pane_id").and_then(|p| p.as_str()),
            Some(pane_id.as_str()),
            "transcript event should have pane_id"
        );
    }

    // Verify the payload contains the original record content.
    let user_event = events.iter().find(|e| e["event_type"] == "user").unwrap();
    let user_content = user_event["payload"]["message"]["content"]
        .as_str()
        .unwrap_or("");
    assert!(
        user_content.contains("hello transcript"),
        "user transcript record should contain the prompt, got: {:?}",
        user_content
    );
}

#[test]
fn ps_does_not_show_pane_not_created_via_run() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Create a managed pane via slopctl run.
    let managed = String::from_utf8_lossy(&env.slopctl(&["run"]).stdout)
        .trim()
        .to_string();
    // Create a pane directly in the slopd session, bypassing slopctl run.
    let unmanaged = spawn_unmanaged_pane_in_slopd_session(&env);

    let ps_out = env.slopctl(&["ps", "--json"]);
    kill_slopd(slopd);

    assert!(
        ps_out.status.success(),
        "slopctl ps --json failed: {:?}",
        ps_out
    );
    let panes: Vec<serde_json::Value> = serde_json::from_slice(&ps_out.stdout)
        .unwrap_or_else(|e| panic!("ps --json output is not valid JSON: {}", e));
    let ids: Vec<&str> = panes.iter().filter_map(|p| p["pane_id"].as_str()).collect();
    assert!(
        ids.contains(&managed.as_str()),
        "managed pane {} missing from ps output",
        managed
    );
    assert!(
        !ids.contains(&unmanaged.as_str()),
        "unmanaged pane {} should not appear in ps output",
        unmanaged
    );
}

/// Helper: read the mock_claude transcript file from a test's claude_config_dir.
/// mock_claude writes to <claude_config_dir>/projects/mock/mock-session-id-1234.jsonl.
fn read_transcript(claude_config_dir: &std::path::Path) -> Vec<serde_json::Value> {
    let path = claude_config_dir.join("projects/mock/mock-session-id-1234.jsonl");
    let contents = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read transcript at {}: {}", path.display(), e));
    contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str(l)
                .unwrap_or_else(|e| panic!("bad JSON in transcript: {}: {}", e, l))
        })
        .collect()
}

/// Helper: filter transcript records by type.
fn transcript_records_of_type<'a>(
    records: &'a [serde_json::Value],
    record_type: &str,
) -> Vec<&'a serde_json::Value> {
    records
        .iter()
        .filter(|r| r.get("type").and_then(|t| t.as_str()) == Some(record_type))
        .collect()
}

/// Verify that mock_claude writes user and assistant transcript records for a normal prompt.
#[test]
fn mock_claude_transcript_normal_prompt() {
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
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(listener, &pane_id);

    let send_output = env.slopctl(&["send", &pane_id, "hello world"]);
    assert!(send_output.status.success());

    // Give transcript a moment to be flushed.
    std::thread::sleep(Duration::from_millis(200));

    let records = read_transcript(&claude_config_dir);

    kill_slopd(slopd);

    let user_records = transcript_records_of_type(&records, "user");
    let assistant_records = transcript_records_of_type(&records, "assistant");
    let queue_records = transcript_records_of_type(&records, "queue-operation");

    assert_eq!(
        user_records.len(),
        1,
        "expected 1 user record, got {}",
        user_records.len()
    );
    assert_eq!(
        assistant_records.len(),
        1,
        "expected 1 assistant record, got {}",
        assistant_records.len()
    );
    assert!(
        queue_records.is_empty(),
        "normal prompt should not produce queue-operation records"
    );

    let content = user_records[0]["message"]["content"].as_str().unwrap();
    assert!(
        content.trim() == "hello world",
        "expected 'hello world', got {:?}",
        content
    );
}

/// Verify that mock_claude writes queue-operation enqueue/remove records when a
/// prompt is queued during `::mock busy` and processed afterwards.
#[test]
fn mock_claude_transcript_busy_queue_records() {
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

    let listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success());
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(listener, &pane_id);

    // Send ::mock busy 3s in a background thread — fires PreToolUse, then collects queued
    // input for up to 3 seconds before processing. slopctl send for the queued prompt
    // unblocks immediately when the enqueue transcript record appears.
    let env2 = env.clone();
    let pane_id2 = pane_id.clone();
    let busy_thread =
        std::thread::spawn(move || env2.slopctl(&["send", &pane_id2, "::mock busy 3s"]));

    // Wait until BusyToolUse.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (_, detailed) = env.pane_state(&pane_id);
        if detailed == libslop::PaneDetailedState::BusyToolUse {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for BusyToolUse"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Send a prompt while busy — it gets queued.
    let send_output = env.slopctl(&["send", &pane_id, "queued prompt", "--timeout", "10"]);
    assert!(
        send_output.status.success(),
        "send while busy failed: {:?}",
        send_output
    );

    let _ = busy_thread.join();

    // Wait for the pane to return to Ready (Stop fires after the busy turn completes).
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, detailed) = env.pane_state(&pane_id);
        if detailed == libslop::PaneDetailedState::Ready {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for pane to return to Ready"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Give transcript a moment to be flushed.
    std::thread::sleep(Duration::from_millis(200));

    let records = read_transcript(&claude_config_dir);

    kill_slopd(slopd);

    let queue_records = transcript_records_of_type(&records, "queue-operation");
    assert!(
        queue_records.len() >= 2,
        "expected at least 2 queue-operation records, got {}: {:?}",
        queue_records.len(),
        queue_records
    );

    // First queue-operation should be enqueue with the queued prompt content.
    let enqueue = queue_records
        .iter()
        .find(|r| r["operation"] == "enqueue")
        .expect("missing enqueue queue-operation");
    assert!(
        enqueue["content"].as_str().unwrap().trim() == "queued prompt",
        "enqueue content mismatch: {:?}",
        enqueue["content"]
    );

    // Second should be dequeue (queued item consumed and processed).
    let dequeue = queue_records
        .iter()
        .find(|r| r["operation"] == "dequeue")
        .expect("missing dequeue queue-operation");
    assert!(
        dequeue.get("content").is_none() || dequeue["content"].is_null(),
        "dequeue should not have content"
    );

    // The queued prompt should also produce user + assistant records.
    let user_records = transcript_records_of_type(&records, "user");
    let queued_user = user_records
        .iter()
        .find(|r| {
            r["message"]["content"]
                .as_str()
                .is_some_and(|c| c.trim() == "queued prompt")
        })
        .expect("missing user record for the queued prompt");
    assert!(queued_user["sessionId"].as_str().is_some());

    let assistant_records = transcript_records_of_type(&records, "assistant");
    let queued_assistant = assistant_records
        .iter()
        .find(|r| {
            r["message"]["content"]
                .as_str()
                .is_some_and(|c| c.contains("queued prompt"))
        })
        .expect("missing assistant record for the queued prompt");
    assert!(queued_assistant["sessionId"].as_str().is_some());

    // Verify ordering: enqueue comes before dequeue.
    let enqueue_idx = records
        .iter()
        .position(|r| r.get("operation").and_then(|o| o.as_str()) == Some("enqueue"))
        .unwrap();
    let dequeue_idx = records
        .iter()
        .position(|r| r.get("operation").and_then(|o| o.as_str()) == Some("dequeue"))
        .unwrap();
    assert!(
        enqueue_idx < dequeue_idx,
        "enqueue (idx {}) should come before dequeue (idx {})",
        enqueue_idx,
        dequeue_idx
    );

    // Verify ordering: dequeue comes before the user record for the queued prompt.
    let user_idx = records
        .iter()
        .position(|r| {
            r.get("type").and_then(|t| t.as_str()) == Some("user")
                && r["message"]["content"]
                    .as_str()
                    .is_some_and(|c| c.trim() == "queued prompt")
        })
        .unwrap();
    assert!(
        dequeue_idx < user_idx,
        "dequeue (idx {}) should come before user record (idx {})",
        dequeue_idx,
        user_idx
    );
}

/// Verify that SubscribeTranscript replays the last N records then streams live records.
#[test]
fn subscribe_transcript_replays_then_streams_live() {
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

    // Spawn pane and wait for SessionStart.
    let session_listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success());
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(session_listener, &pane_id);

    // Send two prompts so there is transcript history.
    let send1 = env.slopctl(&["send", &pane_id, "first prompt"]);
    assert!(send1.status.success());
    let send2 = env.slopctl(&["send", &pane_id, "second prompt"]);
    assert!(send2.status.success());

    // Give transcript time to flush.
    std::thread::sleep(Duration::from_millis(500));

    // Subscribe with --replay 100 to get all history plus live.
    let mut listener = Command::new(cargo_bin("slopctl"))
        .args(["listen", "--pane-id", &pane_id, "--replay", "100"])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn replay listener");

    // Wait for subscription confirmation.
    {
        let stdout = listener.stdout.as_mut().unwrap();
        let mut line = Vec::new();
        let mut buf = [0u8; 1];
        loop {
            use std::io::Read;
            stdout
                .read_exact(&mut buf)
                .expect("failed to read subscription confirmation");
            if buf[0] == b'\n' {
                break;
            }
            line.push(buf[0]);
        }
        let line = String::from_utf8_lossy(&line);
        assert!(
            line.contains("subscribed"),
            "unexpected first line: {:?}",
            line
        );
    }

    // Send a third prompt (this should arrive as a live record after replay).
    let send3 = env.slopctl(&["send", &pane_id, "third prompt"]);
    assert!(send3.status.success());

    // Read records from the listener.
    let stdout = listener.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<Vec<serde_json::Value>>();
    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stdout);
        let mut records = Vec::new();
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                records.push(v);
                // We expect: system+user+assistant for "first prompt",
                // user+assistant for "second prompt", ReplayEnd,
                // user+assistant for "third prompt" (live).
                // Wait for at least a user record containing "third prompt".
                let has_third = records.iter().any(|r| {
                    r["event_type"] == "user"
                        && r["payload"]["message"]["content"]
                            .as_str()
                            .is_some_and(|c| c.contains("third prompt"))
                });
                if has_third {
                    let _ = tx.send(records);
                    return;
                }
            }
        }
        let _ = tx.send(records);
    });

    let records = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("timed out waiting for replay + live records");

    kill_child(listener);
    kill_slopd(slopd);

    // Verify we got replayed records for "first prompt" and "second prompt".
    let first_user = records.iter().any(|r| {
        r["event_type"] == "user"
            && r["payload"]["message"]["content"]
                .as_str()
                .is_some_and(|c| c.contains("first prompt"))
    });
    assert!(first_user, "missing replayed 'first prompt' record");

    let second_user = records.iter().any(|r| {
        r["event_type"] == "user"
            && r["payload"]["message"]["content"]
                .as_str()
                .is_some_and(|c| c.contains("second prompt"))
    });
    assert!(second_user, "missing replayed 'second prompt' record");

    // Verify ReplayEnd marker exists.
    let replay_end = records.iter().any(|r| r["event_type"] == "ReplayEnd");
    assert!(replay_end, "missing ReplayEnd marker in stream");

    // Verify live "third prompt" exists.
    let third_user = records.iter().any(|r| {
        r["event_type"] == "user"
            && r["payload"]["message"]["content"]
                .as_str()
                .is_some_and(|c| c.contains("third prompt"))
    });
    assert!(third_user, "missing live 'third prompt' record");

    // Verify ReplayEnd comes before the third prompt.
    let replay_end_idx = records
        .iter()
        .position(|r| r["event_type"] == "ReplayEnd")
        .unwrap();
    let third_idx = records
        .iter()
        .position(|r| {
            r["event_type"] == "user"
                && r["payload"]["message"]["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("third prompt"))
        })
        .unwrap();
    assert!(
        replay_end_idx < third_idx,
        "ReplayEnd (idx {}) should come before live third prompt (idx {})",
        replay_end_idx,
        third_idx
    );

    // Verify all transcript records have cursor set.
    for r in &records {
        if r["source"] == "transcript" {
            assert!(
                r["cursor"].is_number(),
                "transcript record should have numeric cursor, got: {:?}",
                r
            );
        }
    }
}

/// Verify that ReadTranscript returns paginated history with cursors.
#[test]
fn read_transcript_returns_paginated_records() {
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

    // Spawn pane and send prompts to create transcript history.
    let session_listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success());
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(session_listener, &pane_id);

    let send1 = env.slopctl(&["send", &pane_id, "alpha"]);
    assert!(send1.status.success());
    let send2 = env.slopctl(&["send", &pane_id, "beta"]);
    assert!(send2.status.success());

    std::thread::sleep(Duration::from_millis(500));

    // Read all transcript records (no --before cursor).
    let out = env.slopctl(&["transcript", &pane_id, "--limit", "100"]);
    assert!(out.status.success(), "slopctl transcript failed: {:?}", out);
    let page: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("transcript output not valid JSON");
    let records = page["records"].as_array().expect("records should be array");

    // Should have records (system + user + assistant for each prompt).
    assert!(
        records.len() >= 4,
        "expected at least 4 records, got {}",
        records.len()
    );

    // Every record should have a cursor.
    for r in records {
        assert!(
            r["cursor"].is_number(),
            "record should have numeric cursor: {:?}",
            r
        );
    }

    // Cursors should be monotonically increasing.
    let cursors: Vec<u64> = records
        .iter()
        .map(|r| r["cursor"].as_u64().unwrap())
        .collect();
    for i in 1..cursors.len() {
        assert!(
            cursors[i] > cursors[i - 1],
            "cursors should be monotonically increasing: {:?}",
            cursors
        );
    }

    // Now paginate: read records before the cursor of the last record.
    let mid_cursor = cursors[cursors.len() / 2];
    let out2 = env.slopctl(&[
        "transcript",
        &pane_id,
        "--before",
        &mid_cursor.to_string(),
        "--limit",
        "100",
    ]);
    assert!(out2.status.success());
    let page2: serde_json::Value =
        serde_json::from_slice(&out2.stdout).expect("transcript page 2 not valid JSON");
    let records2 = page2["records"]
        .as_array()
        .expect("records should be array");

    // All records in page 2 should have cursors strictly less than mid_cursor.
    for r in records2 {
        let c = r["cursor"].as_u64().unwrap();
        assert!(
            c < mid_cursor,
            "paginated record cursor {} should be < before_cursor {}",
            c,
            mid_cursor
        );
    }

    kill_slopd(slopd);
}

/// Verify that ReadTranscript returns empty page for pane without transcript.
#[test]
fn read_transcript_empty_for_pane_without_transcript() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Spawn a pane (sleep infinity — no mock_claude, no transcript).
    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success());
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    // ReadTranscript should return empty page.
    let out = env.slopctl(&["transcript", &pane_id]);
    assert!(out.status.success(), "slopctl transcript failed: {:?}", out);
    let page: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("transcript output not valid JSON");
    let records = page["records"].as_array().expect("records should be array");
    assert!(
        records.is_empty(),
        "expected empty records for pane without transcript, got {}",
        records.len()
    );

    kill_slopd(slopd);
}

/// Verify that slopd resumes tailing transcript files for preexisting panes after a restart.
#[test]
fn transcript_tailing_resumes_after_slopd_restart() {
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

    // Spawn a pane and wait for SessionStart (which starts transcript tailing).
    let session_listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(session_listener, &pane_id);

    // Send a prompt before restart to confirm transcript tailing works.
    let send_output = env.slopctl(&["send", &pane_id, "before restart"]);
    assert!(
        send_output.status.success(),
        "slopctl send (before restart) failed: {:?}",
        send_output
    );

    // Give the transcript record time to be written and tailed.
    std::thread::sleep(Duration::from_millis(300));

    // --- Restart slopd ---
    kill_slopd(slopd);
    let slopd2 = env.spawn_slopd();

    // After restart the pane is in booting_up. Fire any hook that carries
    // transcript_path so slopd picks up the tailer again, then fire
    // SessionStart to transition to ready (so slopctl send won't block).
    let transcript_path = claude_config_dir.join("projects/mock/mock-session-id-1234.jsonl");
    let session_start_payload = format!(
        r#"{{"session_id":"mock-session-id-1234","hook_event_name":"SessionStart","transcript_path":"{}","cwd":"/tmp","source":"startup","model":"mock"}}"#,
        transcript_path.display(),
    );
    let hook_out = fire_hook(&env, "SessionStart", &session_start_payload, Some(&pane_id));
    assert!(
        hook_out.status.success(),
        "SessionStart hook after restart failed: {:?}",
        hook_out
    );
    std::thread::sleep(Duration::from_millis(200));

    // Subscribe to transcript events after restart.
    let mut listener = Command::new(cargo_bin("slopctl"))
        .args([
            "listen",
            "--transcript",
            "user",
            "--transcript",
            "assistant",
        ])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn transcript listener");

    // Wait for subscription confirmation.
    {
        let stdout = listener.stdout.as_mut().unwrap();
        let mut line = Vec::new();
        let mut buf = [0u8; 1];
        loop {
            use std::io::Read;
            stdout
                .read_exact(&mut buf)
                .expect("failed to read subscription confirmation");
            if buf[0] == b'\n' {
                break;
            }
            line.push(buf[0]);
        }
        let line = String::from_utf8_lossy(&line);
        assert!(
            line.contains("subscribed"),
            "unexpected first line: {:?}",
            line
        );
    }

    // Send a prompt after restart — mock_claude writes transcript records to the same file.
    let send_output = env.slopctl(&["send", &pane_id, "after restart"]);
    assert!(
        send_output.status.success(),
        "slopctl send (after restart) failed: {:?}",
        send_output
    );

    // Read transcript events from the listener in a background thread with timeout.
    let stdout = listener.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<Vec<serde_json::Value>>();
    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stdout);
        let mut events = Vec::new();
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
                && v.get("source").and_then(|s| s.as_str()) == Some("transcript")
            {
                events.push(v);
                if events.len() >= 2 {
                    let _ = tx.send(events);
                    return;
                }
            }
        }
        let _ = tx.send(events);
    });

    let events = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("timed out waiting for transcript events after slopd restart");

    kill_child(listener);
    kill_slopd(slopd2);

    assert!(
        events.len() >= 2,
        "expected at least 2 transcript events after restart, got {}: {:?}",
        events.len(),
        events
    );

    // Check we got user and assistant events.
    let types: Vec<&str> = events
        .iter()
        .filter_map(|e| e.get("event_type").and_then(|t| t.as_str()))
        .collect();
    assert!(
        types.contains(&"user"),
        "missing 'user' transcript event after restart, got: {:?}",
        types
    );
    assert!(
        types.contains(&"assistant"),
        "missing 'assistant' transcript event after restart, got: {:?}",
        types
    );

    // Verify the events came from the post-restart prompt.
    let user_event = events.iter().find(|e| e["event_type"] == "user").unwrap();
    let user_content = user_event["payload"]["message"]["content"]
        .as_str()
        .unwrap_or("");
    assert!(
        user_content.contains("after restart"),
        "user transcript record should contain post-restart prompt, got: {:?}",
        user_content
    );

    // Verify pane_id is set on the events.
    for ev in &events {
        assert_eq!(
            ev.get("pane_id").and_then(|p| p.as_str()),
            Some(pane_id.as_str()),
            "transcript event should have pane_id after restart"
        );
    }
}

/// Helper: send `::mock spawn-pane` to a pane via mock_claude and capture the child pane ID
/// from the `::mock spawned-pane <child_pane_id>` line printed to the pane.
fn spawn_child_via_mock_claude(env: &TestEnv, parent_pane: &str) -> String {
    // Count existing ::mock spawned-pane  lines so we can detect the new one.
    let before_count = {
        let out = env
            .tmux
            .tmux()
            .args(["capture-pane", "-t", parent_pane, "-p"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        text.lines()
            .filter(|l| l.starts_with("::mock spawned-pane "))
            .count()
    };

    let send_out = env.slopctl(&["send", parent_pane, "::mock spawn-pane"]);
    assert!(
        send_out.status.success(),
        "slopctl send ::mock spawn-pane failed: {:?}",
        send_out
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let out = env
            .tmux
            .tmux()
            .args(["capture-pane", "-t", parent_pane, "-p"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let run_lines: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with("::mock spawned-pane "))
            .collect();
        if run_lines.len() > before_count {
            return run_lines
                .last()
                .unwrap()
                .trim_start_matches("::mock spawned-pane ")
                .trim()
                .to_string();
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for ::mock spawn-pane output in pane {}",
            parent_pane
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Helper: set up a 3-pane A→B→C hierarchy. Returns (slopd child, pane_a, pane_b, pane_c).
fn setup_abc_hierarchy(env: &TestEnv) -> (String, String, String) {
    // Spawn pane A (grandparent).
    let listener = env.spawn_session_start_listener();
    let a_out = env.slopctl(&["run"]);
    assert!(a_out.status.success());
    let pane_a = String::from_utf8_lossy(&a_out.stdout).trim().to_string();
    env.wait_for_session_start(listener, &pane_a);

    let mode_out = env.slopctl(&["send", &pane_a, "::mock input-mode always-submit"]);
    assert!(mode_out.status.success());

    // A spawns B.
    let pane_b = spawn_child_via_mock_claude(env, &pane_a);

    // Wait for B to be ready.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (state, _) = env.pane_state(&pane_b);
        if state == libslop::PaneState::Ready {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for pane B to become Ready"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let mode_out = env.slopctl(&["send", &pane_b, "::mock input-mode always-submit"]);
    assert!(mode_out.status.success());

    // B spawns C.
    let pane_c = spawn_child_via_mock_claude(env, &pane_b);

    // Verify initial hierarchy: C→B→A.
    let ps_json_out = env.slopctl(&["ps", "--json"]);
    assert!(ps_json_out.status.success());
    let panes: serde_json::Value =
        serde_json::from_slice(&ps_json_out.stdout).expect("ps --json output is not valid JSON");
    let c_entry = panes
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["pane_id"] == pane_c.as_str())
        .expect("pane C not found in ps output");
    assert_eq!(
        c_entry["parent_pane_id"],
        pane_b.as_str(),
        "setup: C's parent should be B"
    );
    let b_entry = panes
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["pane_id"] == pane_b.as_str())
        .expect("pane B not found in ps output");
    assert_eq!(
        b_entry["parent_pane_id"],
        pane_a.as_str(),
        "setup: B's parent should be A"
    );

    (pane_a, pane_b, pane_c)
}

/// Helper: set up a 2-pane A→B hierarchy. Returns (pane_a, pane_b).
fn setup_ab_hierarchy(env: &TestEnv) -> (String, String) {
    let listener = env.spawn_session_start_listener();
    let a_out = env.slopctl(&["run"]);
    assert!(a_out.status.success());
    let pane_a = String::from_utf8_lossy(&a_out.stdout).trim().to_string();
    env.wait_for_session_start(listener, &pane_a);

    let mode_out = env.slopctl(&["send", &pane_a, "::mock input-mode always-submit"]);
    assert!(mode_out.status.success());

    // A spawns B.
    let pane_b = spawn_child_via_mock_claude(env, &pane_a);

    // Verify initial hierarchy: B→A.
    let ps_json_out = env.slopctl(&["ps", "--json"]);
    assert!(ps_json_out.status.success());
    let panes: serde_json::Value =
        serde_json::from_slice(&ps_json_out.stdout).expect("ps --json output is not valid JSON");
    let b_entry = panes
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["pane_id"] == pane_b.as_str())
        .expect("pane B not found in ps output");
    assert_eq!(
        b_entry["parent_pane_id"],
        pane_a.as_str(),
        "setup: B's parent should be A"
    );

    (pane_a, pane_b)
}

fn new_reparent_test_env() -> Option<(TestEnv, tempfile::TempDir)> {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let env = TestEnv::new_full(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    )?;

    Some((env, home_dir))
}

/// Assert that pane `pane_id` has `expected_parent` in `slopctl ps --json` output.
/// Pass `None` to assert parent_pane_id is null.
fn assert_parent_pane(env: &TestEnv, pane_id: &str, expected_parent: Option<&str>) {
    let ps_json_out = env.slopctl(&["ps", "--json"]);
    assert!(ps_json_out.status.success());
    let panes: serde_json::Value =
        serde_json::from_slice(&ps_json_out.stdout).expect("ps --json output is not valid JSON");
    let entry = panes
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["pane_id"] == pane_id)
        .unwrap_or_else(|| panic!("pane {} not found in ps output", pane_id));
    let expected = match expected_parent {
        Some(id) => serde_json::Value::String(id.to_string()),
        None => serde_json::Value::Null,
    };
    assert_eq!(
        entry["parent_pane_id"], expected,
        "pane {} parent_pane_id: expected {:?}, got {:?}",
        pane_id, expected, entry["parent_pane_id"],
    );
}

/// Assert that pane `pane_id` does not appear in `slopctl ps --json` output.
fn assert_pane_gone(env: &TestEnv, pane_id: &str) {
    let ps_json_out = env.slopctl(&["ps", "--json"]);
    assert!(ps_json_out.status.success());
    let panes: serde_json::Value =
        serde_json::from_slice(&ps_json_out.stdout).expect("ps --json output is not valid JSON");
    assert!(
        panes
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["pane_id"] != pane_id),
        "pane {} should not appear in ps output after being killed",
        pane_id,
    );
}

/// Wait for a pane to disappear from `slopctl ps --json` output.
fn wait_for_pane_gone(env: &TestEnv, pane_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ps_json_out = env.slopctl(&["ps", "--json"]);
        assert!(ps_json_out.status.success());
        let panes: serde_json::Value = serde_json::from_slice(&ps_json_out.stdout)
            .expect("ps --json output is not valid JSON");
        if panes
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["pane_id"] != pane_id)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for pane {} to disappear",
            pane_id
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ── A→B→C: kill middle pane B, C should be reparented to A ──

#[test]
fn reparent_middle_via_slopctl_kill() {
    let Some((env, _home)) = new_reparent_test_env() else {
        eprintln!("skipping: tmux not found");
        return;
    };
    let slopd = env.spawn_slopd();
    let (pane_a, pane_b, pane_c) = setup_abc_hierarchy(&env);

    // Kill B via slopctl.
    let kill_out = env.slopctl(&["kill", &pane_b]);
    assert!(
        kill_out.status.success(),
        "slopctl kill failed: {:?}",
        kill_out
    );

    assert_parent_pane(&env, &pane_c, Some(&pane_a));
    assert_pane_gone(&env, &pane_b);

    kill_slopd(slopd);
}

#[test]
fn reparent_middle_via_tmux_kill() {
    let Some((env, _home)) = new_reparent_test_env() else {
        eprintln!("skipping: tmux not found");
        return;
    };
    let slopd = env.spawn_slopd();
    let (pane_a, pane_b, pane_c) = setup_abc_hierarchy(&env);

    // Kill B directly via tmux (bypassing slopd).
    let out = env
        .tmux
        .tmux()
        .args(["kill-pane", "-t", &pane_b])
        .output()
        .unwrap();
    assert!(out.status.success(), "tmux kill-pane failed: {:?}", out);

    // slopd doesn't know about the kill until list_panes is called.
    wait_for_pane_gone(&env, &pane_b);
    assert_parent_pane(&env, &pane_c, Some(&pane_a));

    kill_slopd(slopd);
}

#[test]
fn reparent_middle_via_process_exit() {
    let Some((env, _home)) = new_reparent_test_env() else {
        eprintln!("skipping: tmux not found");
        return;
    };
    let slopd = env.spawn_slopd();
    let (pane_a, pane_b, pane_c) = setup_abc_hierarchy(&env);

    // Make B's mock_claude exit by sending two C-c in a row (mock_claude exits on
    // consecutive C-c). We use tmux send-keys directly because slopctl send would
    // wait for a UserPromptSubmit hook that never fires when the process exits.
    env.tmux
        .tmux()
        .args(["send-keys", "-t", &pane_b, "C-c"])
        .status()
        .unwrap();
    std::thread::sleep(Duration::from_millis(50));
    env.tmux
        .tmux()
        .args(["send-keys", "-t", &pane_b, "C-c"])
        .status()
        .unwrap();

    wait_for_pane_gone(&env, &pane_b);
    assert_parent_pane(&env, &pane_c, Some(&pane_a));

    kill_slopd(slopd);
}

// ── A→B: kill root pane A, B's parent should become null ──

#[test]
fn reparent_root_via_slopctl_kill() {
    let Some((env, _home)) = new_reparent_test_env() else {
        eprintln!("skipping: tmux not found");
        return;
    };
    let slopd = env.spawn_slopd();
    let (pane_a, pane_b) = setup_ab_hierarchy(&env);

    // Kill A via slopctl.
    let kill_out = env.slopctl(&["kill", &pane_a]);
    assert!(
        kill_out.status.success(),
        "slopctl kill failed: {:?}",
        kill_out
    );

    assert_parent_pane(&env, &pane_b, None);
    assert_pane_gone(&env, &pane_a);

    kill_slopd(slopd);
}

#[test]
fn reparent_root_via_tmux_kill() {
    let Some((env, _home)) = new_reparent_test_env() else {
        eprintln!("skipping: tmux not found");
        return;
    };
    let slopd = env.spawn_slopd();
    let (pane_a, pane_b) = setup_ab_hierarchy(&env);

    // Kill A directly via tmux.
    let out = env
        .tmux
        .tmux()
        .args(["kill-pane", "-t", &pane_a])
        .output()
        .unwrap();
    assert!(out.status.success(), "tmux kill-pane failed: {:?}", out);

    wait_for_pane_gone(&env, &pane_a);
    assert_parent_pane(&env, &pane_b, None);

    kill_slopd(slopd);
}

#[test]
fn reparent_root_via_process_exit() {
    let Some((env, _home)) = new_reparent_test_env() else {
        eprintln!("skipping: tmux not found");
        return;
    };
    let slopd = env.spawn_slopd();
    let (pane_a, pane_b) = setup_ab_hierarchy(&env);

    // Make A's mock_claude exit by sending two C-c in a row (mock_claude exits on
    // consecutive C-c). We use tmux send-keys directly because slopctl send would
    // wait for a UserPromptSubmit hook that never fires when the process exits.
    env.tmux
        .tmux()
        .args(["send-keys", "-t", &pane_a, "C-c"])
        .status()
        .unwrap();
    std::thread::sleep(Duration::from_millis(50));
    env.tmux
        .tmux()
        .args(["send-keys", "-t", &pane_a, "C-c"])
        .status()
        .unwrap();

    wait_for_pane_gone(&env, &pane_a);
    assert_parent_pane(&env, &pane_b, None);

    kill_slopd(slopd);
}

// ── Pane killed while slopd is offline, then slopd restarts ──

#[test]
fn reparent_middle_during_slopd_restart() {
    let Some((env, _home)) = new_reparent_test_env() else {
        eprintln!("skipping: tmux not found");
        return;
    };
    let slopd = env.spawn_slopd();
    let (pane_a, pane_b, pane_c) = setup_abc_hierarchy(&env);

    // Stop slopd.
    kill_slopd(slopd);

    // Kill B via tmux while slopd is offline (slopctl kill won't work without slopd).
    let out = env
        .tmux
        .tmux()
        .args(["kill-pane", "-t", &pane_b])
        .output()
        .unwrap();
    assert!(out.status.success(), "tmux kill-pane failed: {:?}", out);

    // Restart slopd — it should detect B is gone and reparent C to A.
    let slopd = env.spawn_slopd();

    assert_pane_gone(&env, &pane_b);
    assert_parent_pane(&env, &pane_c, Some(&pane_a));

    kill_slopd(slopd);
}

#[test]
fn reparent_root_during_slopd_restart() {
    let Some((env, _home)) = new_reparent_test_env() else {
        eprintln!("skipping: tmux not found");
        return;
    };
    let slopd = env.spawn_slopd();
    let (pane_a, pane_b) = setup_ab_hierarchy(&env);

    // Stop slopd.
    kill_slopd(slopd);

    // Kill A via tmux while slopd is offline.
    let out = env
        .tmux
        .tmux()
        .args(["kill-pane", "-t", &pane_a])
        .output()
        .unwrap();
    assert!(out.status.success(), "tmux kill-pane failed: {:?}", out);

    // Restart slopd — it should detect A is gone and clear B's parent.
    let slopd = env.spawn_slopd();

    assert_pane_gone(&env, &pane_a);
    assert_parent_pane(&env, &pane_b, None);

    kill_slopd(slopd);
}

/// Helper: spawn a chain of `depth` panes where each spawns the next via `::mock spawn-pane`.
/// Returns the pane IDs in order from root to leaf: [P0, P1, ..., P(depth-1)].
fn setup_pane_chain(env: &TestEnv, depth: usize) -> Vec<String> {
    assert!(depth >= 1);

    let listener = env.spawn_session_start_listener();
    let out = env.slopctl(&["run"]);
    assert!(out.status.success());
    let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
    env.wait_for_session_start(listener, &root);

    let mode_out = env.slopctl(&["send", &root, "::mock input-mode always-submit"]);
    assert!(mode_out.status.success());

    let mut chain = vec![root];

    for _ in 1..depth {
        let parent = chain.last().unwrap();
        let child = spawn_child_via_mock_claude(env, parent);

        // Wait for child to be ready.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (state, _) = env.pane_state(&child);
            if state == libslop::PaneState::Ready {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for pane to become Ready"
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        let mode_out = env.slopctl(&["send", &child, "::mock input-mode always-submit"]);
        assert!(mode_out.status.success());

        chain.push(child);
    }

    chain
}

/// 6-level chain: P0→P1→P2→P3→P4→P5. Kill P1,P2,P3,P4 while slopd is offline.
/// After restart, P5's parent should be P0 (the only surviving ancestor).
#[test]
fn reparent_deep_chain_during_slopd_restart() {
    let Some((env, _home)) = new_reparent_test_env() else {
        eprintln!("skipping: tmux not found");
        return;
    };
    let slopd = env.spawn_slopd();
    let chain = setup_pane_chain(&env, 6);

    // Verify initial parent chain.
    for i in 1..6 {
        assert_parent_pane(&env, &chain[i], Some(&chain[i - 1]));
    }

    // Stop slopd.
    kill_slopd(slopd);

    // Kill P1, P2, P3, P4 via tmux while slopd is offline.
    for pane in &chain[1..5] {
        let out = env
            .tmux
            .tmux()
            .args(["kill-pane", "-t", pane])
            .output()
            .unwrap();
        assert!(out.status.success(), "tmux kill-pane {} failed", pane);
    }

    // Restart slopd.
    let slopd = env.spawn_slopd();

    // P1-P4 should be gone.
    for pane in &chain[1..5] {
        assert_pane_gone(&env, pane);
    }

    // P5's parent should be P0 (skipping all dead intermediaries).
    assert_parent_pane(&env, &chain[5], Some(&chain[0]));

    // P0 should still have no parent.
    assert_parent_pane(&env, &chain[0], None);

    kill_slopd(slopd);
}

/// Regression: interrupting Claude while it's in AwaitingInputPermission state causes
/// it to write transcript `user` events (tool rejection + interrupt message) but NOT
/// fire any hooks. slopd detects this via the transcript tailer and transitions the
/// state back to Ready.
#[test]
fn interrupt_in_awaiting_permission_transitions_to_ready() {
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

    let listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(listener, &pane_id);

    // Send ::mock permission 1s to put mock_claude into AwaitingInputPermission state.
    // This fires UserPromptSubmit, PreToolUse hooks, waits 1s (busy period),
    // then fires PermissionRequest and blocks on the permission dialog.
    let env2 = env.clone();
    let pane_id2 = pane_id.clone();
    let permission_thread =
        std::thread::spawn(move || env2.slopctl(&["send", &pane_id2, "::mock permission 1s"]));

    // Wait until pane reaches AwaitingInputPermission.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (_, detailed) = env.pane_state(&pane_id);
        if detailed == libslop::PaneDetailedState::AwaitingInputPermission {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for AwaitingInputPermission state"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Interrupt the pane. Mock claude writes transcript user events (no hooks).
    // slopd should detect these transcript events and transition to Ready.
    let int_out = env.slopctl(&["interrupt", &pane_id]);
    assert!(int_out.status.success(), "interrupt failed: {:?}", int_out);

    // Wait for slopd to detect the transcript events and transition to Ready.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (_, detailed) = env.pane_state(&pane_id);
        if detailed == libslop::PaneDetailedState::Ready {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for Ready state after interrupt; still {:?}",
            detailed,
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let (simple, detailed) = env.pane_state(&pane_id);
    assert_eq!(detailed, libslop::PaneDetailedState::Ready);
    assert_eq!(simple, libslop::PaneState::Ready);

    let _ = permission_thread.join();
    kill_slopd(slopd);
}

/// After slopd restarts, a pane whose Claude is idle in ready state should recover
/// its state from the transcript rather than staying stuck at BootingUp.
#[test]
fn ready_pane_recovers_state_after_slopd_restart() {
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
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(listener, &pane_id);

    // Confirm pane is in Ready state.
    let (simple, detailed) = env.pane_state(&pane_id);
    assert_eq!(simple, libslop::PaneState::Ready);
    assert_eq!(detailed, libslop::PaneDetailedState::Ready);

    // Send a prompt so there's a Stop hook in the transcript (Claude returns to ready).
    let send_out = env.slopctl(&["send", &pane_id, "::mock echo hello"]);
    assert!(send_out.status.success(), "send failed: {:?}", send_out);

    // Wait for pane to return to Ready after processing the prompt.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (_, detailed) = env.pane_state(&pane_id);
        if detailed == libslop::PaneDetailedState::Ready {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for Ready after send"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Restart slopd. The tmux session and mock_claude survive.
    kill_slopd(slopd);
    let slopd2 = env.spawn_slopd();

    // Give slopd time to load managed panes and process any transcript events.
    std::thread::sleep(Duration::from_millis(1000));

    // slopd should recover the real state by replaying transcript records.
    let (simple, detailed) = env.pane_state(&pane_id);
    assert_eq!(
        detailed,
        libslop::PaneDetailedState::Ready,
        "expected Ready after slopd restart (recovered from transcript), got {:?}",
        detailed,
    );
    assert_eq!(
        simple,
        libslop::PaneState::Ready,
        "expected Ready after slopd restart, got {:?}",
        simple,
    );

    kill_slopd(slopd2);
}

/// PaneCreated event fires when a pane is spawned via slopctl run.
#[test]
fn listen_event_pane_created_fires_on_run() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let listener = spawn_event_listener(&env, "PaneCreated");

    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    let event = wait_for_event(listener, {
        let pane_id = pane_id.clone();
        move |v| v["event_type"] == "PaneCreated" && v["pane_id"] == pane_id.as_str()
    });

    assert_eq!(event["source"], "slopd");
    assert_eq!(event["payload"]["pane_id"], pane_id.as_str());

    kill_slopd(slopd);
}

/// PaneDestroyed event fires when a pane is killed via slopctl kill.
#[test]
fn listen_event_pane_destroyed_fires_on_kill() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    let listener = spawn_event_listener(&env, "PaneDestroyed");

    let kill_output = env.slopctl(&["kill", &pane_id]);
    assert!(
        kill_output.status.success(),
        "slopctl kill failed: {:?}",
        kill_output
    );

    let event = wait_for_event(listener, {
        let pane_id = pane_id.clone();
        move |v| v["event_type"] == "PaneDestroyed" && v["pane_id"] == pane_id.as_str()
    });

    assert_eq!(event["source"], "slopd");
    assert_eq!(event["payload"]["pane_id"], pane_id.as_str());

    kill_slopd(slopd);
}

/// User-defined tmux hooks on the slopd session survive a slopd restart.
#[test]
fn user_tmux_hooks_survive_slopd_restart() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Add a user-defined hook on the slopd session (session-scope hook).
    // tmux normalizes single quotes to double quotes in show-hooks output.
    let user_hook_cmd = "display-message 'user hook fired'";
    let user_hook_normalized = "display-message \"user hook fired\"";
    let status = env
        .tmux
        .tmux()
        .args([
            "set-hook",
            "-a",
            "-t",
            "slopd",
            "after-kill-pane",
            user_hook_cmd,
        ])
        .status()
        .expect("failed to set user tmux hook");
    assert!(status.success(), "failed to set user tmux hook");

    // Record all hooks before restart.
    let before = env
        .tmux
        .tmux()
        .args(["show-hooks", "-t", "slopd"])
        .output()
        .expect("failed to show hooks");
    let before_hooks = String::from_utf8_lossy(&before.stdout).to_string();
    assert!(
        before_hooks.contains(user_hook_normalized),
        "user hook not found before restart: {}",
        before_hooks,
    );

    // Restart slopd — it should re-register its hooks without removing the user's.
    kill_slopd(slopd);
    let slopd2 = env.spawn_slopd();

    let after = env
        .tmux
        .tmux()
        .args(["show-hooks", "-t", "slopd"])
        .output()
        .expect("failed to show hooks after restart");
    let after_hooks = String::from_utf8_lossy(&after.stdout).to_string();

    assert!(
        after_hooks.contains(user_hook_normalized),
        "user hook was removed by slopd restart: {}",
        after_hooks,
    );

    // Also verify slopd's own hooks are still present.
    assert!(
        after_hooks.contains("slopctl tmux-hook after-kill-pane"),
        "slopd's after-kill-pane hook missing after restart: {}",
        after_hooks,
    );

    kill_slopd(slopd2);
}

/// slopd registers its tmux hooks idempotently — no duplicates after restart.
#[test]
fn slopd_tmux_hooks_not_duplicated_on_restart() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    kill_slopd(slopd);
    let slopd2 = env.spawn_slopd();
    kill_slopd(slopd2);
    let slopd3 = env.spawn_slopd();

    let output = env
        .tmux
        .tmux()
        .args(["show-hooks", "-t", "slopd"])
        .output()
        .expect("failed to show hooks");
    let hooks = String::from_utf8_lossy(&output.stdout).to_string();

    // Count how many times our after-kill-pane hook appears — should be exactly 1.
    let count = hooks
        .lines()
        .filter(|l| l.contains("slopctl tmux-hook after-kill-pane"))
        .count();
    assert_eq!(
        count, 1,
        "expected exactly 1 slopd after-kill-pane hook, found {}: {}",
        count, hooks,
    );

    kill_slopd(slopd3);
}

/// slopctl kill succeeds even if the pane's process already exited (pane is dead
/// in tmux but still tracked by slopd). This covers the race where the process
/// exits between slopd's managed_panes check and the tmux kill-pane call.
#[test]
fn kill_succeeds_when_pane_already_dead() {
    build_bin("slopd");
    build_bin("slopctl");

    // Use "true" as the executable so the pane exits immediately.
    let Some(env) = TestEnv::new(Some(&["true"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    // Wait for the process to exit and the pane to disappear from tmux.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let out = env
            .tmux
            .tmux()
            .args(["list-panes", "-s", "-t", "slopd", "-F", "#{pane_id}"])
            .output()
            .expect("failed to list panes");
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !stdout.lines().any(|l| l.trim() == pane_id) {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for pane {} to exit", pane_id);
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Kill should either succeed (slopd still had the pane in managed_panes)
    // or report "not managed" (the reconciler already cleaned it up).
    let kill_output = env.slopctl(&["kill", &pane_id]);
    if kill_output.status.success() {
        let kill_stdout = String::from_utf8_lossy(&kill_output.stdout);
        assert_eq!(kill_stdout.trim(), pane_id);
    } else {
        let stderr = String::from_utf8_lossy(&kill_output.stderr);
        assert!(
            stderr.contains("not managed"),
            "unexpected kill error: {:?}",
            kill_output,
        );
    }

    kill_slopd(slopd);
}

/// PaneDestroyed event fires when a pane's process exits (detected by the
/// background reconciler, not by slopctl kill).
#[test]
fn pane_destroyed_fires_on_process_exit() {
    build_bin("slopd");
    build_bin("slopctl");

    // Use "sleep 0.2" so the pane lives long enough to subscribe, then exits.
    let Some(env) = TestEnv::new(Some(&["sleep", "0.2"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let listener = spawn_event_listener(&env, "PaneDestroyed");

    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    // The process will exit on its own; the reconciler should detect it and
    // emit PaneDestroyed within a few seconds.
    let event = wait_for_event(listener, {
        let pane_id = pane_id.clone();
        move |v| v["event_type"] == "PaneDestroyed" && v["pane_id"] == pane_id.as_str()
    });

    assert_eq!(event["source"], "slopd");
    assert_eq!(event["payload"]["pane_id"], pane_id.as_str());

    kill_slopd(slopd);
}

/// When a managed pane's process crashes without firing any hooks or writing a
/// transcript, slopd's reconciler must still detect the pane is gone, emit
/// PaneDestroyed, and remove it from ps output.
#[test]
fn pane_destroyed_on_crash_without_hooks() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    // --mock-hooks=disabled suppresses all hook calls; --print '::mock process exit 1' makes
    // mock_claude exit immediately with code 1 (no interactive loop, no
    // SessionStart, no transcript).
    let Some(env) = TestEnv::new(Some(&[
        &mock_claude_path,
        "--mock-hooks=disabled",
        "--print",
        "::mock process exit 1",
    ])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Subscribe to PaneDestroyed before spawning the pane.
    let listener = spawn_event_listener(&env, "PaneDestroyed");

    // Spawn mock_claude — it will exit immediately without firing any hooks.
    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    // The reconciler should detect the pane is gone and emit PaneDestroyed.
    let event = wait_for_event(listener, {
        let pane_id = pane_id.clone();
        move |v| v["event_type"] == "PaneDestroyed" && v["pane_id"] == pane_id.as_str()
    });

    assert_eq!(event["source"], "slopd");
    assert_eq!(event["payload"]["pane_id"], pane_id.as_str());
    // The dead pane lingered (remain-on-exit) long enough for the reconciler to
    // read its exit code off pane_dead_status and enrich the event. mock_claude
    // here exits 1 via `--print '::mock process exit 1'`.
    assert_eq!(
        event["payload"]["exit_status"], 1,
        "PaneDestroyed should carry the captured exit status; got: {}",
        event
    );

    // The pane should no longer appear in ps output.
    let ps_output = env.slopctl(&["ps", "--json"]);
    assert!(ps_output.status.success());
    let panes_after: Vec<serde_json::Value> = serde_json::from_slice(&ps_output.stdout).unwrap();
    assert!(
        !panes_after.iter().any(|p| p["pane_id"] == pane_id.as_str()),
        "pane should not be listed after crash",
    );

    kill_slopd(slopd);
}

/// slopd removes stale tmux hook entries from a previous slopctl path when
/// re-registering hooks with the current path.
#[test]
fn slopd_removes_stale_tmux_hook_entries() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    // Create the slopd session and plant a stale hook before starting slopd.
    // We need the session to exist first.
    let status = env
        .tmux
        .tmux()
        .args(["new-session", "-d", "-s", "slopd"])
        .status()
        .expect("failed to create slopd session");
    // Session may already exist from prior test env setup; ignore failure.
    let _ = status;

    let stale_hook = "run-shell \"XDG_RUNTIME_DIR=/old/runtime /old/path/slopctl tmux-hook after-kill-pane || true\"";
    let status = env
        .tmux
        .tmux()
        .args([
            "set-hook",
            "-a",
            "-t",
            "slopd",
            "after-kill-pane",
            stale_hook,
        ])
        .status()
        .expect("failed to set stale hook");
    assert!(status.success(), "failed to plant stale hook");

    // Verify stale hook is present.
    let before = env
        .tmux
        .tmux()
        .args(["show-hooks", "-t", "slopd"])
        .output()
        .expect("failed to show hooks");
    let before_hooks = String::from_utf8_lossy(&before.stdout).to_string();
    assert!(
        before_hooks.contains("/old/path/slopctl"),
        "stale hook not planted: {}",
        before_hooks
    );

    // Start slopd — it should remove the stale entry and add its own.
    let slopd = env.spawn_slopd();

    let after = env
        .tmux
        .tmux()
        .args(["show-hooks", "-t", "slopd"])
        .output()
        .expect("failed to show hooks after slopd start");
    let after_hooks = String::from_utf8_lossy(&after.stdout).to_string();

    // Stale entry must be gone.
    assert!(
        !after_hooks.contains("/old/path/slopctl"),
        "stale hook entry was not removed: {}",
        after_hooks,
    );

    // Current slopctl entry must be present.
    assert!(
        after_hooks.contains("slopctl tmux-hook after-kill-pane"),
        "current slopctl hook missing after stale cleanup: {}",
        after_hooks,
    );

    kill_slopd(slopd);
}

/// After subscribing, the client can still issue request-response calls (e.g. ps)
/// on the same connection.
#[test]
fn multiplexed_subscribe_then_request() {
    build_bin("slopd");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Spawn a pane so ps() returns something.
    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    let socket_path = env.socket_path();

    // Use the library client directly via tokio.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        let (reader, writer) = stream.into_split();
        let mut client = libslopctl::Client::new(reader, writer);

        // Subscribe to all events.
        let mut subscription = client.subscribe(vec![]).await.unwrap();

        // While subscribed, issue a ps() request on the same client.
        let panes = client.ps().await.unwrap();
        assert!(!panes.is_empty(), "ps() should return at least one pane");
        assert!(
            panes.iter().any(|p| p.pane_id == pane_id),
            "pane {} not in ps output",
            pane_id
        );

        // Also verify subscription still works: fire a hook and check we get an event.
        // Use a separate connection to fire the hook.
        let stream2 = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        let (r2, w2) = stream2.into_split();
        let mut hook_client = libslopctl::Client::new(r2, w2);

        let payload = serde_json::json!({
            "session_id": "s1",
            "hook_event_name": "UserPromptSubmit",
            "transcript_path": "/dev/null",
            "cwd": "/tmp",
            "prompt": "multiplex-test"
        });
        hook_client
            .hook(
                "UserPromptSubmit".to_string(),
                payload,
                Some(pane_id.clone()),
            )
            .await
            .unwrap();

        // Read from the subscription until we get the hook event or timeout.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut found = false;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                result = subscription.next() => {
                    match result {
                        Ok(Some(libslopctl::SubscriptionItem::Record(record))) => {
                            if record.event_type == "UserPromptSubmit" {
                                found = true;
                                break;
                            }
                        }
                        Ok(Some(libslopctl::SubscriptionItem::Subscribed)) => {}
                        Ok(None) => break,
                        Err(e) => panic!("subscription error: {}", e),
                    }
                }
            }
        }
        assert!(
            found,
            "expected to receive UserPromptSubmit event via subscription"
        );
    });

    kill_slopd(slopd);
}

/// Unsubscribe stops the subscription stream.
#[test]
fn multiplexed_unsubscribe_stops_stream() {
    build_bin("slopd");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    let socket_path = env.socket_path();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        let (reader, writer) = stream.into_split();
        let mut client = libslopctl::Client::new(reader, writer);

        let subscription = client.subscribe(vec![]).await.unwrap();

        // Unsubscribe.
        client.unsubscribe(&subscription).await.unwrap();

        // After unsubscribe, the client should still be usable for requests.
        let _state = client.status().await.unwrap();
    });

    kill_slopd(slopd);
}

/// unsubscribe_by_id cancels a subscription using only the request ID.
#[test]
fn multiplexed_unsubscribe_by_id_stops_stream() {
    build_bin("slopd");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    let socket_path = env.socket_path();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        let (reader, writer) = stream.into_split();
        let mut client = libslopctl::Client::new(reader, writer);

        let subscription = client.subscribe(vec![]).await.unwrap();
        let sub_id = subscription.id();

        // Unsubscribe using only the ID.
        client.unsubscribe_by_id(sub_id).await.unwrap();

        // Client should still be usable for requests after unsubscribe_by_id.
        let _state = client.status().await.unwrap();
    });

    kill_slopd(slopd);
}

/// Multiple subscriptions can coexist on the same connection.
#[test]
fn multiplexed_multiple_subscriptions() {
    build_bin("slopd");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    let socket_path = env.socket_path();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        let (reader, writer) = stream.into_split();
        let mut client = libslopctl::Client::new(reader, writer);

        // Create two subscriptions: one for hook events, one for slopd events.
        let mut hook_sub = client.subscribe(vec![libslop::EventFilter {
            source: Some("hook".to_string()),
            ..Default::default()
        }]).await.unwrap();

        let mut slopd_sub = client.subscribe(vec![libslop::EventFilter {
            source: Some("slopd".to_string()),
            ..Default::default()
        }]).await.unwrap();

        // Fire a hook event — should appear on hook_sub but not on slopd_sub
        // (filter is source:hook vs source:slopd).
        let stream2 = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        let (r2, w2) = stream2.into_split();
        let mut hook_client = libslopctl::Client::new(r2, w2);

        let payload = serde_json::json!({
            "session_id": "s1",
            "hook_event_name": "SessionStart",
            "transcript_path": "/dev/null",
            "cwd": "/tmp"
        });
        hook_client.hook("SessionStart".to_string(), payload, Some(pane_id.clone())).await.unwrap();

        // hook_sub should receive the hook event.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut hook_found = false;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                result = hook_sub.next() => {
                    if let Ok(Some(libslopctl::SubscriptionItem::Record(record))) = result
                        && record.source == "hook" {
                            hook_found = true;
                            break;
                        }
                }
            }
        }
        assert!(hook_found, "hook subscription should receive hook event");

        // slopd_sub should receive the StateChange event (fired by slopd when
        // it processes the SessionStart hook).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut state_change_found = false;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                result = slopd_sub.next() => {
                    if let Ok(Some(libslopctl::SubscriptionItem::Record(record))) = result
                        && (record.event_type == "StateChange" || record.event_type == "DetailedStateChange") {
                            state_change_found = true;
                            break;
                        }
                }
            }
        }
        assert!(state_change_found, "slopd subscription should receive state change event");

        // Client should still work for requests.
        let panes = client.ps().await.unwrap();
        assert!(!panes.is_empty());
    });

    kill_slopd(slopd);
}

#[test]
fn executable_cli_flag_overrides_config() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    // Config uses the default executable ("claude") — the --executable CLI flag
    // should override it to mock_claude.
    let Some(env) = TestEnv::new_full(None, Some(&slopctl_path), Some(&claude_config_dir)) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd_with_args(&["--executable", &mock_claude_path]);

    let listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    let session_id = env.wait_for_session_start(listener, &pane_id);

    // mock_claude always uses this session ID.
    assert_eq!(session_id, "mock-session-id-1234");

    kill_slopd(slopd);
}

#[test]
fn executable_cli_flag_with_args() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    // Config uses the default executable — the --executable flag passes
    // mock_claude with --mock-session-start=skip, so no SessionStart hook fires.
    let Some(env) = TestEnv::new_full(None, Some(&slopctl_path), Some(&claude_config_dir)) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd_with_args(&[
        "--executable",
        &mock_claude_path,
        "--mock-session-start=skip",
    ]);

    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    // With --mock-session-start=skip, mock_claude skips the SessionStart hook,
    // so the pane should stay in BootingUp state rather than transitioning to Ready.
    std::thread::sleep(Duration::from_millis(500));
    let (state, detailed_state) = env.pane_state(&pane_id);
    assert_eq!(state, libslop::PaneState::BootingUp);
    assert_eq!(detailed_state, libslop::PaneDetailedState::BootingUp);

    kill_slopd(slopd);
}

#[test]
fn uninject_hooks_removes_slopctl_entries() {
    build_bin("slopd");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    std::fs::create_dir_all(&claude_config_dir).unwrap();

    // Inject hooks directly (not via slopd, to avoid auto-cleanup on exit).
    let settings_path = claude_config_dir.join("settings.json");
    libslop::inject_hooks_into_file(&settings_path, "slopctl").unwrap();

    // Verify hooks were injected.
    let settings_contents =
        std::fs::read_to_string(&settings_path).expect("settings.json was not created");
    let settings: serde_json::Value =
        serde_json::from_str(&settings_contents).expect("settings.json is not valid JSON");
    for &event in libslop::HOOK_EVENTS {
        let entries = settings["hooks"][event]
            .as_array()
            .unwrap_or_else(|| panic!("missing hooks.{}", event));
        assert!(
            !entries.is_empty(),
            "hooks.{} should have entries after injection",
            event
        );
    }

    // Write a minimal slopd config pointing to our claude_config_dir.
    let config_dir = tempfile::tempdir().unwrap();
    let slopd_config_dir = config_dir.path().join("slopd");
    std::fs::create_dir_all(&slopd_config_dir).unwrap();
    std::fs::write(
        slopd_config_dir.join("config.toml"),
        format!(
            "claude_config_dir = {:?}\n",
            claude_config_dir.to_str().unwrap()
        ),
    )
    .unwrap();

    // Run slopd uninject-hooks to remove them.
    let uninject_output = Command::new(cargo_bin("slopd"))
        .args(["uninject-hooks"])
        .env("XDG_CONFIG_HOME", config_dir.path())
        .env("HOME", config_dir.path())
        .output()
        .expect("failed to run slopd uninject-hooks");
    assert!(
        uninject_output.status.success(),
        "slopd uninject-hooks failed: {:?}",
        uninject_output
    );

    // Verify hooks were removed.
    let settings_contents = std::fs::read_to_string(claude_config_dir.join("settings.json"))
        .expect("settings.json missing after uninject");
    let settings: serde_json::Value =
        serde_json::from_str(&settings_contents).expect("settings.json is not valid JSON");
    for &event in libslop::HOOK_EVENTS {
        let entries = settings["hooks"][event]
            .as_array()
            .unwrap_or_else(|| panic!("missing hooks.{}", event));
        let slopctl_count = entries
            .iter()
            .filter(|entry| {
                entry["hooks"].as_array().is_some_and(|hooks| {
                    hooks.iter().any(|h| {
                        h["type"] == "command"
                            && h["command"]
                                .as_str()
                                .is_some_and(|c| c.contains("slopctl") && c.contains(event))
                    })
                })
            })
            .count();
        assert_eq!(
            slopctl_count, 0,
            "event {} still has slopctl entries after uninject",
            event
        );
    }
}

#[test]
fn uninject_hooks_preserves_other_hook_entries() {
    build_bin("slopd");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    std::fs::create_dir_all(&claude_config_dir).unwrap();

    // Write settings.json with a foreign hook entry, then inject slopctl hooks.
    let settings_path = claude_config_dir.join("settings.json");
    let initial_settings = serde_json::json!({
        "hooks": {
            "Stop": [
                {
                    "matcher": "",
                    "hooks": [{"type": "command", "command": "my-tool notify stop"}]
                }
            ]
        }
    });
    std::fs::write(
        &settings_path,
        serde_json::to_string_pretty(&initial_settings).unwrap(),
    )
    .unwrap();
    libslop::inject_hooks_into_file(&settings_path, "slopctl").unwrap();

    // Write a minimal slopd config pointing to our claude_config_dir.
    let config_dir = tempfile::tempdir().unwrap();
    let slopd_config_dir = config_dir.path().join("slopd");
    std::fs::create_dir_all(&slopd_config_dir).unwrap();
    std::fs::write(
        slopd_config_dir.join("config.toml"),
        format!(
            "claude_config_dir = {:?}\n",
            claude_config_dir.to_str().unwrap()
        ),
    )
    .unwrap();

    // Run slopd uninject-hooks.
    let uninject_output = Command::new(cargo_bin("slopd"))
        .args(["uninject-hooks"])
        .env("XDG_CONFIG_HOME", config_dir.path())
        .env("HOME", config_dir.path())
        .output()
        .expect("failed to run slopd uninject-hooks");
    assert!(
        uninject_output.status.success(),
        "slopd uninject-hooks failed: {:?}",
        uninject_output
    );

    // The foreign hook entry must still be present.
    let settings_contents =
        std::fs::read_to_string(&settings_path).expect("settings.json missing after uninject");
    let settings: serde_json::Value =
        serde_json::from_str(&settings_contents).expect("settings.json is not valid JSON");
    let stop_entries = settings["hooks"]["Stop"].as_array().unwrap();
    let foreign_count = stop_entries
        .iter()
        .filter(|entry| {
            entry["hooks"].as_array().is_some_and(|hooks| {
                hooks
                    .iter()
                    .any(|h| h["command"].as_str() == Some("my-tool notify stop"))
            })
        })
        .count();
    assert_eq!(
        foreign_count, 1,
        "foreign hook entry was incorrectly removed"
    );
}

#[test]
fn uninject_hooks_without_settings_file_succeeds() {
    build_bin("slopd");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");

    let config_dir = tempfile::tempdir().unwrap();
    // Write a minimal slopd config pointing claude_config_dir to a nonexistent path.
    let slopd_config_dir = config_dir.path().join("slopd");
    std::fs::create_dir_all(&slopd_config_dir).unwrap();
    std::fs::write(
        slopd_config_dir.join("config.toml"),
        format!(
            "claude_config_dir = {:?}\n",
            claude_config_dir.to_str().unwrap()
        ),
    )
    .unwrap();

    // Should succeed even when there's no settings.json to modify.
    let output = Command::new(cargo_bin("slopd"))
        .args(["uninject-hooks"])
        .env("XDG_CONFIG_HOME", config_dir.path())
        .env("HOME", config_dir.path())
        .output()
        .expect("failed to run slopd uninject-hooks");
    assert!(
        output.status.success(),
        "slopd uninject-hooks should succeed even without settings.json: {:?}",
        output
    );
}

#[test]
fn slopd_removes_hooks_on_normal_exit() {
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

    // Start slopd and run a pane to inject hooks.
    let slopd = env.spawn_slopd();
    let output = env.slopctl(&["run"]);
    assert!(output.status.success(), "slopctl run failed: {:?}", output);

    // Verify hooks were injected while slopd is running.
    let settings_contents = std::fs::read_to_string(claude_config_dir.join("settings.json"))
        .expect("settings.json was not created");
    let settings: serde_json::Value =
        serde_json::from_str(&settings_contents).expect("settings.json is not valid JSON");
    for &event in libslop::HOOK_EVENTS {
        let entries = settings["hooks"][event]
            .as_array()
            .unwrap_or_else(|| panic!("missing hooks.{}", event));
        assert!(
            !entries.is_empty(),
            "hooks.{} should have entries while slopd is running",
            event
        );
    }

    // Stop slopd (SIGTERM — normal exit).
    kill_slopd(slopd);

    // After exit, hooks should be removed from settings.json.
    let settings_contents = std::fs::read_to_string(claude_config_dir.join("settings.json"))
        .expect("settings.json missing after slopd exit");
    let settings: serde_json::Value =
        serde_json::from_str(&settings_contents).expect("settings.json is not valid JSON");
    for &event in libslop::HOOK_EVENTS {
        let entries = settings["hooks"][event]
            .as_array()
            .unwrap_or_else(|| panic!("missing hooks.{}", event));
        let slopctl_count = entries
            .iter()
            .filter(|entry| {
                entry["hooks"].as_array().is_some_and(|hooks| {
                    hooks.iter().any(|h| {
                        h["type"] == "command"
                            && h["command"]
                                .as_str()
                                .is_some_and(|c| c.contains("slopctl") && c.contains(event))
                    })
                })
            })
            .count();
        assert_eq!(
            slopctl_count, 0,
            "event {} still has slopctl entries after slopd exit",
            event
        );
    }
}

#[test]
fn slopd_removes_hooks_on_sigint() {
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
    assert!(output.status.success(), "slopctl run failed: {:?}", output);

    // Verify hooks were injected while slopd is running.
    let settings_contents = std::fs::read_to_string(claude_config_dir.join("settings.json"))
        .expect("settings.json was not created");
    let settings: serde_json::Value =
        serde_json::from_str(&settings_contents).expect("settings.json is not valid JSON");
    for &event in libslop::HOOK_EVENTS {
        let entries = settings["hooks"][event]
            .as_array()
            .unwrap_or_else(|| panic!("missing hooks.{}", event));
        assert!(
            !entries.is_empty(),
            "hooks.{} should have entries while slopd is running",
            event
        );
    }

    // Send SIGINT (simulates Ctrl+C from cargo run).
    sigint_child(slopd);

    // After SIGINT exit, hooks should be removed from settings.json.
    let settings_contents = std::fs::read_to_string(claude_config_dir.join("settings.json"))
        .expect("settings.json missing after slopd SIGINT exit");
    let settings: serde_json::Value =
        serde_json::from_str(&settings_contents).expect("settings.json is not valid JSON");
    for &event in libslop::HOOK_EVENTS {
        let entries = settings["hooks"][event]
            .as_array()
            .unwrap_or_else(|| panic!("missing hooks.{}", event));
        let slopctl_count = entries
            .iter()
            .filter(|entry| {
                entry["hooks"].as_array().is_some_and(|hooks| {
                    hooks.iter().any(|h| {
                        h["type"] == "command"
                            && h["command"]
                                .as_str()
                                .is_some_and(|c| c.contains("slopctl") && c.contains(event))
                    })
                })
            })
            .count();
        assert_eq!(
            slopctl_count, 0,
            "event {} still has slopctl entries after slopd SIGINT",
            event
        );
    }
}

/// An absolute-path `[run] executable` is honored even when its directory is
/// NOT on the pane's PATH — the pre-spawn existence check resolves absolute
/// paths directly (the libslop unit test covers the resolver; this covers the
/// Run-handler wiring end-to-end). Without absolute handling, `run` would
/// wrongly reject a perfectly valid `executable = "/abs/path/..."`.
#[test]
fn run_accepts_absolute_path_executable_not_on_path() {
    build_bin("slopd");
    build_bin("slopctl");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();

    // Absolute path to a real binary (`sleep`); the pane stays alive on it.
    let sleep_abs = std::env::var_os("PATH")
        .and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("sleep"))
                .find(|p| p.exists())
        })
        .expect("sleep must be installed for tests");
    let sleep_abs = sleep_abs.to_str().unwrap().to_string();

    // Sandbox PATH with only tmux — crucially NOT sleep's directory, so `sleep`
    // resolves *only* via the absolute path we pass as the executable.
    let path_dir = tempfile::tempdir().unwrap();
    let tmux_path = std::env::var_os("PATH")
        .and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("tmux"))
                .find(|p| p.exists())
        })
        .expect("tmux must be installed for tests");
    std::os::unix::fs::symlink(&tmux_path, path_dir.path().join("tmux"))
        .expect("failed to symlink tmux into sandbox PATH");

    let Some(env) = TestEnv::new_full(Some(&[&sleep_abs, "infinity"]), Some(&slopctl_path), None)
    else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd_with_envs(&[("PATH", path_dir.path().to_str().unwrap())]);
    // Legacy `--no-wait`: we only need it to spawn — the absolute executable must
    // pass the existence check despite its directory not being on PATH.
    let out = env.slopctl(&["run"]);
    kill_slopd(slopd);

    assert!(
        out.status.success(),
        "run with an absolute-path executable (dir off PATH) should succeed: {:?}",
        out
    );
    let pane_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        pane_id.starts_with('%'),
        "should print a pane id; got stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn run_injects_hooks_with_absolute_slopctl_path_when_not_on_path() {
    build_bin("slopd");
    build_bin("slopctl");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");

    // Do NOT pass an explicit slopctl path — let slopd resolve it.
    // The test's premise is that "slopctl" is NOT on PATH, so slopd must fall
    // back to the sibling-of-current-exe resolution and produce an absolute
    // path.  But `slopd-git` is commonly installed system-wide as
    // `/usr/bin/slopctl`, and on most distros there is no PATH entry that
    // contains tmux without also containing slopctl.  Build a sandbox
    // directory that contains only the binaries slopd actually needs (tmux),
    // and pass it as PATH — `which::which("slopctl")` then fails, exercising
    // the sibling-resolution branch this test was written to cover.
    let path_dir = tempfile::tempdir().unwrap();
    let tmux_path = std::env::var_os("PATH")
        .and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("tmux"))
                .find(|p| p.exists())
        })
        .expect("tmux must be installed for tests");
    std::os::unix::fs::symlink(&tmux_path, path_dir.path().join("tmux"))
        .expect("failed to symlink tmux into sandbox PATH");
    // The executable ("sleep") must be resolvable too, now that `run` pre-checks
    // it exists before spawning; only slopctl is intentionally kept off this PATH
    // so the sibling-resolution branch is still exercised.
    let sleep_path = std::env::var_os("PATH")
        .and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("sleep"))
                .find(|p| p.exists())
        })
        .expect("sleep must be installed for tests");
    std::os::unix::fs::symlink(&sleep_path, path_dir.path().join("sleep"))
        .expect("failed to symlink sleep into sandbox PATH");

    let Some(env) = TestEnv::new_full(Some(&["sleep", "infinity"]), None, Some(&claude_config_dir))
    else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd_with_envs(&[("PATH", path_dir.path().to_str().unwrap())]);
    let output = env.slopctl(&["run"]);
    assert!(output.status.success(), "slopctl run failed: {:?}", output);

    let settings_contents = std::fs::read_to_string(claude_config_dir.join("settings.json"))
        .expect("settings.json was not created");
    let settings: serde_json::Value =
        serde_json::from_str(&settings_contents).expect("settings.json is not valid JSON");

    // Hooks should use an absolute path to slopctl, not the bare "slopctl".
    for &event in libslop::HOOK_EVENTS {
        let entries = settings["hooks"][event]
            .as_array()
            .unwrap_or_else(|| panic!("missing hooks.{}", event));
        let has_absolute_path_hook = entries.iter().any(|entry| {
            entry["hooks"].as_array().is_some_and(|hooks| {
                hooks.iter().any(|h| {
                    h["type"] == "command"
                        && h["command"]
                            .as_str()
                            .is_some_and(|c| c.contains("/slopctl hook") && c.starts_with('/'))
                })
            })
        });
        assert!(
            has_absolute_path_hook,
            "event {} should have an absolute-path slopctl hook, got: {:?}",
            event, entries
        );
    }

    kill_slopd(slopd);
}

#[test]
fn uninject_hooks_removes_absolute_path_slopctl_entries() {
    build_bin("slopd");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    std::fs::create_dir_all(&claude_config_dir).unwrap();

    // Inject hooks with an absolute path (simulates slopd that resolved slopctl).
    let settings_path = claude_config_dir.join("settings.json");
    libslop::inject_hooks_into_file(&settings_path, "/opt/custom/bin/slopctl").unwrap();

    // Verify hooks were injected with the absolute path.
    let settings_contents =
        std::fs::read_to_string(&settings_path).expect("settings.json was not created");
    let settings: serde_json::Value =
        serde_json::from_str(&settings_contents).expect("settings.json is not valid JSON");
    for &event in libslop::HOOK_EVENTS {
        let entries = settings["hooks"][event]
            .as_array()
            .unwrap_or_else(|| panic!("missing hooks.{}", event));
        assert!(
            !entries.is_empty(),
            "hooks.{} should have entries after injection",
            event
        );
    }

    // Write a minimal slopd config pointing to our claude_config_dir.
    let config_dir = tempfile::tempdir().unwrap();
    let slopd_config_dir = config_dir.path().join("slopd");
    std::fs::create_dir_all(&slopd_config_dir).unwrap();
    std::fs::write(
        slopd_config_dir.join("config.toml"),
        format!(
            "claude_config_dir = {:?}\n",
            claude_config_dir.to_str().unwrap()
        ),
    )
    .unwrap();

    // Run slopd uninject-hooks — should remove absolute-path entries too.
    let uninject_output = Command::new(cargo_bin("slopd"))
        .args(["uninject-hooks"])
        .env("XDG_CONFIG_HOME", config_dir.path())
        .env("HOME", config_dir.path())
        .output()
        .expect("failed to run slopd uninject-hooks");
    assert!(
        uninject_output.status.success(),
        "slopd uninject-hooks failed: {:?}",
        uninject_output
    );

    // Verify all slopctl entries were removed.
    let settings_contents =
        std::fs::read_to_string(&settings_path).expect("settings.json missing after uninject");
    let settings: serde_json::Value =
        serde_json::from_str(&settings_contents).expect("settings.json is not valid JSON");
    for &event in libslop::HOOK_EVENTS {
        let entries = settings["hooks"][event]
            .as_array()
            .unwrap_or_else(|| panic!("missing hooks.{}", event));
        let slopctl_count = entries
            .iter()
            .filter(|entry| {
                entry["hooks"].as_array().is_some_and(|hooks| {
                    hooks
                        .iter()
                        .any(|h| h["command"].as_str().is_some_and(|c| c.contains("slopctl")))
                })
            })
            .count();
        assert_eq!(
            slopctl_count, 0,
            "event {} still has slopctl entries after uninject",
            event
        );
    }
}

#[test]
fn slopd_reinjects_hooks_on_restart_with_existing_panes() {
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

    // First cycle: start slopd, spawn a pane, then stop slopd (removes hooks).
    let slopd = env.spawn_slopd();
    let output = env.slopctl(&["run"]);
    assert!(output.status.success(), "slopctl run failed: {:?}", output);
    kill_slopd(slopd);

    // Hooks should be gone after slopd exits.
    let settings_contents = std::fs::read_to_string(claude_config_dir.join("settings.json"))
        .expect("settings.json missing after first slopd exit");
    let settings: serde_json::Value =
        serde_json::from_str(&settings_contents).expect("settings.json is not valid JSON");
    for &event in libslop::HOOK_EVENTS {
        let entries = settings["hooks"][event]
            .as_array()
            .unwrap_or_else(|| panic!("missing hooks.{}", event));
        let slopctl_count = entries
            .iter()
            .filter(|entry| {
                entry["hooks"].as_array().is_some_and(|hooks| {
                    hooks.iter().any(|h| {
                        h["type"] == "command"
                            && h["command"].as_str().is_some_and(|c| c.contains("slopctl"))
                    })
                })
            })
            .count();
        assert_eq!(
            slopctl_count, 0,
            "event {} still has slopctl entries after first slopd exit",
            event
        );
    }

    // Second cycle: restart slopd WITHOUT running a new pane.
    // The existing pane from the first cycle is still alive in tmux.
    // slopd should detect the recovered pane and re-inject hooks.
    let slopd = env.spawn_slopd();

    // Give slopd a moment to recover panes and inject hooks.
    std::thread::sleep(Duration::from_millis(200));

    let settings_contents = std::fs::read_to_string(claude_config_dir.join("settings.json"))
        .expect("settings.json missing after slopd restart");
    let settings: serde_json::Value =
        serde_json::from_str(&settings_contents).expect("settings.json is not valid JSON");
    for &event in libslop::HOOK_EVENTS {
        let entries = settings["hooks"][event]
            .as_array()
            .unwrap_or_else(|| panic!("missing hooks.{}", event));
        let has_our_hook = entries.iter().any(|entry| {
            entry["hooks"].as_array().is_some_and(|hooks| {
                hooks.iter().any(|h| {
                    h["type"] == "command"
                        && h["command"]
                            .as_str()
                            .is_some_and(|c| c.contains("slopctl") && c.contains(event))
                })
            })
        });
        assert!(
            has_our_hook,
            "event {} should have slopctl hook after restart with existing panes",
            event
        );
    }

    kill_slopd(slopd);
}

/// slopd continues working normally when its tmux exits on last pane closed.
#[test]
fn slopd_survives_tmux_exit_on_last_pane_closed() {
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

    // Assert tmux is running.
    let status = env
        .tmux
        .tmux()
        .args(["list-sessions"])
        .status()
        .expect("failed to list tmux sessions");
    assert!(status.success(), "tmux should be running");

    let slopd = env.spawn_slopd();

    // slopctl run mock_claude.
    let listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(listener, &pane_id);

    // Instruct mock_claude to exit 0.
    let send_output = env.slopctl(&["send", &pane_id, "::mock process exit 0"]);
    assert!(
        send_output.status.success(),
        "slopctl send ::mock process exit 0 failed: {:?}",
        send_output
    );

    // Wait for the reconciler to detect the pane is gone.
    std::thread::sleep(Duration::from_secs(4));

    // Assert slopctl ps works.
    let ps_output = env.slopctl(&["ps", "--json"]);
    assert!(
        ps_output.status.success(),
        "slopctl ps should work: {:?}",
        ps_output
    );

    // Assert slopctl run works.
    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run should work: {:?}",
        run_output
    );

    kill_slopd(slopd);
}

/// slopd continues working normally when the initial shell pane in the slopd
/// tmux session receives Ctrl+D and exits (no slopctl run involved).
#[test]
fn slopd_survives_initial_shell_pane_exit() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    // Assert tmux is running.
    let status = env
        .tmux
        .tmux()
        .args(["list-sessions"])
        .status()
        .expect("failed to list tmux sessions");
    assert!(status.success(), "tmux should be running");

    let slopd = env.spawn_slopd();

    // Find the initial shell pane in the slopd session.
    let list_output = env
        .tmux
        .tmux()
        .args(["list-panes", "-s", "-t", "slopd", "-F", "#{pane_id}"])
        .output()
        .expect("failed to list panes");
    let pane_id = String::from_utf8_lossy(&list_output.stdout)
        .trim()
        .to_string();
    assert!(!pane_id.is_empty(), "slopd session should have a pane");

    // Send Ctrl+D to the shell pane to make it exit.
    let status = env
        .tmux
        .tmux()
        .args(["send-keys", "-t", &pane_id, "", "C-d"])
        .status()
        .expect("tmux send-keys failed");
    assert!(status.success(), "tmux send-keys C-d failed");

    // Wait for the pane to exit.
    std::thread::sleep(Duration::from_secs(4));

    // Assert slopctl ps works.
    let ps_output = env.slopctl(&["ps", "--json"]);
    assert!(
        ps_output.status.success(),
        "slopctl ps should work: {:?}",
        ps_output
    );

    // Assert slopctl run works.
    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run should work: {:?}",
        run_output
    );

    kill_slopd(slopd);
}

#[test]
fn run_forwards_env_from_cli_flag() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();
    let Some(env) = TestEnv::new(Some(&[&mock_claude_path])) else {
        eprintln!("skipping: tmux not found");
        return;
    };
    let slopd = env.spawn_slopd();

    let out = env.slopctl(&["run", "--env", "SLOPD_TEST_FOO=hello"]);
    assert!(out.status.success(), "run failed: {:?}", out);
    let pane_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    std::thread::sleep(Duration::from_millis(200));

    let value = read_pane_env(&env, &pane_id, "SLOPD_TEST_FOO");
    kill_slopd(slopd);
    assert_eq!(value, "hello", "--env should forward KEY=VALUE to the pane");
}

#[test]
fn run_expands_cli_env_value_from_slopctl_env() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();
    let Some(env) = TestEnv::new(Some(&[&mock_claude_path])) else {
        eprintln!("skipping: tmux not found");
        return;
    };
    let slopd = env.spawn_slopd();

    let out = slopctl_with_env(
        &env,
        &["run", "--env", "SLOPD_TEST_BAR=${SLOPD_TEST_SOURCE}"],
        &[("SLOPD_TEST_SOURCE", "resolved")],
    );
    assert!(out.status.success(), "run failed: {:?}", out);
    let pane_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    std::thread::sleep(Duration::from_millis(200));

    let value = read_pane_env(&env, &pane_id, "SLOPD_TEST_BAR");
    kill_slopd(slopd);
    assert_eq!(
        value, "resolved",
        "${{VAR}} in --env should be expanded from slopctl's environment"
    );
}

#[test]
fn run_cli_env_missing_var_is_error() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };
    let slopd = env.spawn_slopd();

    let mut cmd = Command::new(cargo_bin("slopctl"));
    cmd.args([
        "run",
        "--env",
        "SLOPD_TEST_X=${SLOPD_TEST_DEFINITELY_UNSET}",
    ])
    .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
    .env_remove("SLOPD_TEST_DEFINITELY_UNSET");
    let out = cmd.output().expect("failed to run slopctl");
    kill_slopd(slopd);

    assert!(!out.status.success(), "missing var should fail: {:?}", out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("SLOPD_TEST_DEFINITELY_UNSET"),
        "error should mention the missing variable; got: {}",
        stderr
    );
}

#[test]
fn run_forwards_env_from_env_file() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();
    let Some(env) = TestEnv::new(Some(&[&mock_claude_path])) else {
        eprintln!("skipping: tmux not found");
        return;
    };
    let slopd = env.spawn_slopd();

    let env_file = env.config_dir.path().join("test.env");
    std::fs::write(
        &env_file,
        "# a comment\nSLOPD_TEST_FILE_A=aaa\nSLOPD_TEST_FILE_B=bbb\n",
    )
    .unwrap();

    let out = env.slopctl(&["run", "--env-file", env_file.to_str().unwrap()]);
    assert!(out.status.success(), "run failed: {:?}", out);
    let pane_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    std::thread::sleep(Duration::from_millis(200));

    let a = read_pane_env(&env, &pane_id, "SLOPD_TEST_FILE_A");
    let b = read_pane_env(&env, &pane_id, "SLOPD_TEST_FILE_B");
    kill_slopd(slopd);
    assert_eq!(a, "aaa");
    assert_eq!(b, "bbb");
}

#[test]
fn run_cli_flag_overrides_env_file() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();
    let Some(env) = TestEnv::new(Some(&[&mock_claude_path])) else {
        eprintln!("skipping: tmux not found");
        return;
    };
    let slopd = env.spawn_slopd();

    let env_file = env.config_dir.path().join("test.env");
    std::fs::write(&env_file, "SLOPD_TEST_PREC=from-file\n").unwrap();

    let out = env.slopctl(&[
        "run",
        "--env-file",
        env_file.to_str().unwrap(),
        "--env",
        "SLOPD_TEST_PREC=from-flag",
    ]);
    assert!(out.status.success(), "run failed: {:?}", out);
    let pane_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    std::thread::sleep(Duration::from_millis(200));

    let value = read_pane_env(&env, &pane_id, "SLOPD_TEST_PREC");
    kill_slopd(slopd);
    assert_eq!(value, "from-flag", "--env should override --env-file");
}

#[test]
fn run_forwards_env_from_config_run_env() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();
    let socket = env.tmux.socket.to_str().unwrap().to_string();
    let config = format!(
        "[tmux]\nsocket = {:?}\n\n[run]\nexecutable = [{:?}]\n\n[run.env]\nSLOPD_TEST_CFG = \"cfg-value\"\n",
        socket, mock_claude_path,
    );
    let slopd_config_dir = env.config_dir.path().join("slopd");
    std::fs::create_dir_all(&slopd_config_dir).unwrap();
    std::fs::write(slopd_config_dir.join("config.toml"), config).unwrap();

    let slopd = env.spawn_slopd();

    let out = env.slopctl(&["run"]);
    assert!(out.status.success(), "run failed: {:?}", out);
    let pane_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    std::thread::sleep(Duration::from_millis(200));

    let value = read_pane_env(&env, &pane_id, "SLOPD_TEST_CFG");
    kill_slopd(slopd);
    assert_eq!(
        value, "cfg-value",
        "[run.env] in config should reach the pane"
    );
}

#[test]
fn run_forwards_env_from_config_env_files() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let env_file = env.config_dir.path().join("cfg.env");
    std::fs::write(&env_file, "SLOPD_TEST_CFG_FILE=from-cfg-file\n").unwrap();

    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();
    let socket = env.tmux.socket.to_str().unwrap().to_string();
    let config = format!(
        "[tmux]\nsocket = {:?}\n\n[run]\nexecutable = [{:?}]\nenv_files = [{:?}]\n",
        socket,
        mock_claude_path,
        env_file.to_str().unwrap(),
    );
    let slopd_config_dir = env.config_dir.path().join("slopd");
    std::fs::create_dir_all(&slopd_config_dir).unwrap();
    std::fs::write(slopd_config_dir.join("config.toml"), config).unwrap();

    let slopd = env.spawn_slopd();

    let out = env.slopctl(&["run"]);
    assert!(out.status.success(), "run failed: {:?}", out);
    let pane_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    std::thread::sleep(Duration::from_millis(200));

    let value = read_pane_env(&env, &pane_id, "SLOPD_TEST_CFG_FILE");
    kill_slopd(slopd);
    assert_eq!(
        value, "from-cfg-file",
        "[run] env_files should reach the pane"
    );
}

#[test]
fn run_cli_env_overrides_config_env() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();
    let socket = env.tmux.socket.to_str().unwrap().to_string();
    let config = format!(
        "[tmux]\nsocket = {:?}\n\n[run]\nexecutable = [{:?}]\n\n[run.env]\nSLOPD_TEST_PREC = \"from-config\"\n",
        socket, mock_claude_path,
    );
    let slopd_config_dir = env.config_dir.path().join("slopd");
    std::fs::create_dir_all(&slopd_config_dir).unwrap();
    std::fs::write(slopd_config_dir.join("config.toml"), config).unwrap();

    let slopd = env.spawn_slopd();

    let out = env.slopctl(&["run", "--env", "SLOPD_TEST_PREC=from-cli"]);
    assert!(out.status.success(), "run failed: {:?}", out);
    let pane_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    std::thread::sleep(Duration::from_millis(200));

    let value = read_pane_env(&env, &pane_id, "SLOPD_TEST_PREC");
    kill_slopd(slopd);
    assert_eq!(
        value, "from-cli",
        "CLI --env should override [run.env] in config"
    );
}

/// Reproducer for the bug observed in production: when `tmux list-panes -s -t slopd`
/// inside `reconcile_panes` transiently returns no panes (success+empty stdout, or
/// "can't find session:" stderr — happens when the tmux session is briefly missing
/// or when a tmux command interleave returns nothing), reconcile concluded that
/// every managed pane was destroyed and removed it from the in-memory
/// `managed_panes` set.  Once disowned, every subsequent `Send`/`Interrupt`/`Tag`
/// returned "pane is not managed by slopd" — even though the pane was alive and
/// continued running Claude.
///
/// Slopd journal at the time the production bug bit (Apr 26 20:46:25):
///   "pane %1 no longer exists, emitting PaneDestroyed"
///   followed by repeated "ignoring hook from unmanaged pane %1"
///   followed by every dm-relay delivery failing with "is not managed by slopd"
///
/// The fix is per-pane verification: before declaring a managed pane destroyed,
/// query its options directly (`show-options -t %X -p`).  If the pane is alive
/// and still has `@slopd_managed=true` set, keep it.
///
/// This test injects the failure mode via `SLOPD_TEST_RECONCILE_FORCE_EMPTY=1`,
/// which forces every reconcile tick to behave as if tmux returned an empty pane
/// list.  Without the fix, reconcile removes the pane within one tick.  With the
/// fix, per-pane verification preserves it.
#[test]
fn reconcile_does_not_disown_alive_pane_when_list_panes_returns_empty() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();
    let Some(env) = TestEnv::new(Some(&[&mock_claude_path])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd_with_envs(&[("SLOPD_TEST_RECONCILE_FORCE_EMPTY", "1")]);

    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    // Reconcile interval is 2 s.  Wait long enough that several ticks have fired
    // and any false-positive removal would have happened.
    std::thread::sleep(Duration::from_secs(5));

    // The pane is still alive, and managed_panes should still contain it.
    // `slopctl tag` only checks managed_panes, so it is a clean probe of whether
    // the pane was wrongly disowned.  Before the fix this returns
    // "pane %X is not managed by slopd"; after the fix it succeeds.
    let tag_out = env.slopctl(&["tag", &pane_id, "verify"]);
    let stderr = String::from_utf8_lossy(&tag_out.stderr);
    assert!(
        tag_out.status.success(),
        "slopctl tag should succeed because pane is still alive, but got: status={:?} stderr={}",
        tag_out.status,
        stderr,
    );

    kill_slopd(slopd);
}

/// `slopctl ps` must reflect slopd's in-memory `managed_panes` set rather than
/// arbitrary tmux pane options.  A pane that has `@slopd_managed=true` set on
/// it but that is not in `managed_panes` (e.g. the option is stale, or a pane
/// was created outside `slopctl run`) must NOT appear in `ps` output, because
/// `Send`/`Interrupt`/`Tag` would all reject it as "pane is not managed".
///
/// Without this guarantee, dm-relay (and other clients) discover a pane via
/// `slopctl ps`, then fail to operate on it because slopd's authoritative set
/// disagrees — exactly the inconsistency that surfaced in the production
/// disowning bug, where `slopctl ps` kept showing %1 long after slopd's
/// `Send`/`Interrupt` started rejecting it.
#[test]
fn ps_reflects_managed_panes_not_tmux_options() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "60"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Spawn a pane the normal way — this one IS in managed_panes.
    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let managed_pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    // Create an "impostor" pane: a fresh window in slopd's tmux session, with
    // `@slopd_managed=true` set on it manually.  This pane was never inserted
    // into managed_panes — it represents a stale option, a manual intervention,
    // or a pane that was reconciled away while still alive in tmux.
    let new_window_out = env
        .tmux
        .tmux()
        .args(["new-window", "-d", "-t", "slopd", "-P", "-F", "#{pane_id}"])
        .output()
        .expect("tmux new-window failed");
    assert!(
        new_window_out.status.success(),
        "tmux new-window failed: stderr={}",
        String::from_utf8_lossy(&new_window_out.stderr)
    );
    let impostor_pane_id = String::from_utf8_lossy(&new_window_out.stdout)
        .trim()
        .to_string();
    assert!(
        impostor_pane_id.starts_with('%'),
        "expected pane id like %42, got {:?}",
        impostor_pane_id
    );

    let set_status = env
        .tmux
        .tmux()
        .args([
            "set-option",
            "-t",
            &impostor_pane_id,
            "-p",
            "@slopd_managed",
            "true",
        ])
        .status()
        .expect("tmux set-option failed");
    assert!(set_status.success());

    let ps_output = env.slopctl(&["ps", "--json"]);
    assert!(
        ps_output.status.success(),
        "slopctl ps failed: {:?}",
        String::from_utf8_lossy(&ps_output.stderr)
    );
    let panes: Vec<serde_json::Value> =
        serde_json::from_slice(&ps_output.stdout).expect("ps --json output should be JSON");
    let pane_ids: Vec<&str> = panes
        .iter()
        .map(|p| p["pane_id"].as_str().unwrap())
        .collect();

    assert!(
        pane_ids.iter().any(|id| *id == managed_pane_id),
        "managed pane {} should be in ps output: {:?}",
        managed_pane_id,
        pane_ids,
    );
    assert!(
        !pane_ids.iter().any(|id| *id == impostor_pane_id),
        "impostor pane {} (not in managed_panes) must NOT be in ps output: {:?}",
        impostor_pane_id,
        pane_ids,
    );

    kill_slopd(slopd);
}

/// Read `slopctl status` output and return the value after `subscribers: `.
fn read_subscriber_count(env: &TestEnv) -> u64 {
    let out = env.slopctl(&["status"]);
    assert!(out.status.success(), "slopctl status failed: {:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("subscribers: ") {
            return rest.trim().parse().unwrap_or_else(|e| {
                panic!("could not parse subscriber count from {:?}: {}", line, e)
            });
        }
    }
    panic!("subscribers: line missing from status output: {:?}", stdout);
}

/// Block until `slopctl status` reports at least `min` subscribers, or timeout.
fn wait_for_subscriber_count_at_least(env: &TestEnv, min: u64, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let count = read_subscriber_count(env);
        if count >= min {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for subscriber_count >= {} (last seen: {})",
                min, count
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Block until `slopctl status` reports at most `max` subscribers, or timeout.
fn wait_for_subscriber_count_at_most(env: &TestEnv, max: u64, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let count = read_subscriber_count(env);
        if count <= max {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for subscriber_count <= {} (last seen: {})",
                max, count
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Read `slopctl status` output and return the value after `config_generation: `.
fn read_config_generation(env: &TestEnv) -> u64 {
    let out = env.slopctl(&["status"]);
    assert!(out.status.success(), "slopctl status failed: {:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("config_generation: ") {
            return rest.trim().parse().unwrap_or_else(|e| {
                panic!("could not parse config_generation from {:?}: {}", line, e)
            });
        }
    }
    panic!(
        "config_generation: line missing from status output: {:?}",
        stdout
    );
}

/// Block until `slopctl status` reports `config_generation >= min`. Used after
/// `sighup_pid` to wait deterministically for the reload to take effect.
fn wait_for_config_generation_at_least(env: &TestEnv, min: u64, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let observed = read_config_generation(env);
        if observed >= min {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for config_generation >= {} (last seen: {})",
                min, observed
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn wait_exits_zero_on_matching_hook_event() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Need a managed pane so hook events aren't dropped.
    let run_out = env.slopctl(&["run"]);
    assert!(
        run_out.status.success(),
        "slopctl run failed: {:?}",
        run_out
    );
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    let wait_child = Command::new(cargo_bin("slopctl"))
        .args(["wait", "--hook", "UserPromptSubmit", "--timeout", "10"])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl wait");

    // Wait for the subscription to land before firing, otherwise we'd race.
    wait_for_subscriber_count_at_least(&env, 1, Duration::from_secs(5));

    let prompt_payload =
        r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"hi"}"#;
    let out = fire_hook(&env, "UserPromptSubmit", prompt_payload, Some(&pane_id));
    assert!(
        out.status.success(),
        "slopctl hook UserPromptSubmit failed: {:?}",
        out
    );

    let out = wait_child
        .wait_with_output()
        .expect("failed to wait on slopctl wait");
    kill_slopd(slopd);

    assert!(
        out.status.success(),
        "slopctl wait should exit 0 on matching event, got {:?}",
        out.status
    );

    // Output parity with `listen`: a {"subscribed":true} line first, then the
    // matching record as a JSON line.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut lines = stdout.lines();
    let first = lines.next().expect("missing subscribed line");
    assert!(
        first.contains("\"subscribed\":true"),
        "first line should be subscribed confirmation: {:?}",
        first
    );

    // Find the UserPromptSubmit record (slopd-internal events may interleave).
    let hook_event = lines
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|v| v["source"] == "hook" && v["event_type"] == "UserPromptSubmit")
        .unwrap_or_else(|| {
            panic!(
                "no UserPromptSubmit hook record in wait stdout: {:?}",
                stdout
            )
        });
    assert_eq!(hook_event["pane_id"], pane_id);
}

#[test]
fn wait_exits_nonzero_on_timeout() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let out = Command::new(cargo_bin("slopctl"))
        .args(["wait", "--hook", "NeverFires", "--timeout", "1"])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .expect("failed to run slopctl wait");

    kill_slopd(slopd);

    assert!(
        !out.status.success(),
        "slopctl wait should exit non-zero on timeout, got {:?}",
        out
    );
}

#[test]
fn wait_until_payload_predicate_matches() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Need a managed pane so hooks transition slopd's state machine.
    let run_out = env.slopctl(&["run"]);
    assert!(
        run_out.status.success(),
        "slopctl run failed: {:?}",
        run_out
    );
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    // Subscribe to DetailedStateChange filtered by detailed_state=ready.
    let mut wait_child = Command::new(cargo_bin("slopctl"))
        .args([
            "wait",
            "--event",
            "DetailedStateChange",
            "--pane-id",
            &pane_id,
            "--until",
            "detailed_state=ready",
            "--timeout",
            "10",
        ])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl wait");

    wait_for_subscriber_count_at_least(&env, 1, Duration::from_secs(5));

    // Fire a hook that transitions the pane to a non-ready state first, so the
    // subsequent ready event is the one that satisfies the predicate.
    let stop_payload = r#"{"session_id":"s1","hook_event_name":"Stop"}"#;
    let out = fire_hook(&env, "Stop", stop_payload, Some(&pane_id));
    assert!(out.status.success(), "slopctl hook Stop failed: {:?}", out);

    let status = wait_child.wait().expect("failed to wait on slopctl wait");
    kill_slopd(slopd);

    assert!(
        status.success(),
        "slopctl wait --until detailed_state=ready should exit 0, got {:?}",
        status
    );
}

/// When slopctl disconnects (clean exit, crash, or kill), the spawned subscriber
/// task on the slopd side must be reaped. Otherwise the broadcast::Receiver
/// stays alive, eats channel slots, and silently leaks across reconnects.
#[test]
fn wait_subscription_reaped_on_disconnect() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let baseline = read_subscriber_count(&env);

    // Spawn a wait that will block effectively forever (filter never matches).
    let wait_child = Command::new(cargo_bin("slopctl"))
        .args(["wait", "--hook", "NeverFires", "--timeout", "300"])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl wait");

    wait_for_subscriber_count_at_least(&env, baseline + 1, Duration::from_secs(5));

    // SIGKILL: simulates a crash/abrupt termination — no graceful cleanup
    // from slopctl's side, so slopd must notice the closed socket and reap.
    // std::process::Child::kill sends SIGKILL on Unix.
    let mut wait_child = wait_child;
    wait_child.kill().unwrap();
    wait_child.wait().unwrap();

    // The subscriber should drop back to baseline within a short window.
    wait_for_subscriber_count_at_most(&env, baseline, Duration::from_secs(5));

    kill_slopd(slopd);
}

/// `wait` and `listen` must produce identical stdout (subscribed line + every
/// record as a JSON line) for the same event sequence. wait stops after the
/// matching record; listen would continue, so we compare wait's full output
/// against the prefix of listen's output.
#[test]
fn wait_and_listen_produce_identical_output_until_match() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_out = env.slopctl(&["run"]);
    assert!(
        run_out.status.success(),
        "slopctl run failed: {:?}",
        run_out
    );
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    // Spawn both wait and listen with the same filters BEFORE firing any
    // event, so both subscriptions see the same broadcast sequence.
    let wait_child = Command::new(cargo_bin("slopctl"))
        .args([
            "wait",
            "--hook",
            "UserPromptSubmit",
            "--pane-id",
            &pane_id,
            "--timeout",
            "10",
        ])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl wait");
    let listen_child = Command::new(cargo_bin("slopctl"))
        .args([
            "listen",
            "--hook",
            "UserPromptSubmit",
            "--pane-id",
            &pane_id,
        ])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl listen");

    wait_for_subscriber_count_at_least(&env, 2, Duration::from_secs(5));

    let prompt_payload =
        r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"hi"}"#;
    let out = fire_hook(&env, "UserPromptSubmit", prompt_payload, Some(&pane_id));
    assert!(
        out.status.success(),
        "slopctl hook UserPromptSubmit failed: {:?}",
        out
    );

    let wait_out = wait_child
        .wait_with_output()
        .expect("failed to wait on wait child");
    assert!(
        wait_out.status.success(),
        "wait should exit 0, got {:?}",
        wait_out.status
    );

    // Read listen's first len(wait_stdout) bytes, then kill it.
    let mut listen_child = listen_child;
    let listen_stdout = listen_child.stdout.take().expect("listen has no stdout");
    let wait_stdout = String::from_utf8_lossy(&wait_out.stdout).to_string();
    let target_len = wait_stdout.len();

    use std::io::Read;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = listen_stdout;
        let mut buf = vec![0u8; target_len];
        let mut filled = 0;
        while filled < target_len {
            match reader.read(&mut buf[filled..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => filled += n,
            }
        }
        buf.truncate(filled);
        let _ = tx.send(buf);
    });
    let listen_prefix = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("timed out reading listen stdout prefix");
    kill_child(listen_child);
    kill_slopd(slopd);

    let listen_prefix = String::from_utf8_lossy(&listen_prefix).to_string();
    assert_eq!(
        wait_stdout, listen_prefix,
        "wait stdout must equal the equivalent prefix of listen stdout\nwait:   {:?}\nlisten: {:?}",
        wait_stdout, listen_prefix,
    );
}

/// SIGHUP must cause slopd to re-read its config file: a `slopctl run` issued
/// after the signal should use the new executable from the file.
#[test]
fn sighup_reloads_executable_from_config() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    // Initial config: mock_claude with --mock-session-start=skip, so panes stay BootingUp.
    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path, "--mock-session-start=skip"]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_out = env.slopctl(&["run"]);
    assert!(
        run_out.status.success(),
        "first slopctl run failed: {:?}",
        run_out
    );
    let pane1 = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    // Confirm the pre-reload behavior: --mock-session-start=skip keeps the pane BootingUp.
    std::thread::sleep(Duration::from_millis(500));
    let (_, detailed) = env.pane_state(&pane1);
    assert_eq!(
        detailed,
        libslop::PaneDetailedState::BootingUp,
        "pre-reload pane should be BootingUp under --mock-session-start=skip"
    );

    // Rewrite the config to drop --mock-session-start=skip, then SIGHUP, then wait
    // until the reload counter advances so the next `slopctl run` is guaranteed
    // to use the new config.
    env.tmux.write_slopd_config_full(
        &env.config_dir,
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
        None,
    );
    sighup_pid(slopd.id());
    wait_for_config_generation_at_least(&env, 1, Duration::from_secs(5));

    // Subscribe BEFORE the post-reload run so we don't miss the SessionStart.
    let listener = env.spawn_session_start_listener();

    let run_out = env.slopctl(&["run"]);
    assert!(
        run_out.status.success(),
        "post-reload slopctl run failed: {:?}",
        run_out
    );
    let pane2 = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    // The new executable does NOT have --mock-session-start=skip, so SessionStart fires.
    env.wait_for_session_start(listener, &pane2);

    kill_slopd(slopd);
}

/// A SIGHUP that hits a malformed config file must not crash slopd, must not
/// silently drop back to defaults, and must keep the pre-reload behavior.
#[test]
fn sighup_with_invalid_config_keeps_old_config() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new(Some(&[&mock_claude_path, "--mock-session-start=skip"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Corrupt the config file. SIGHUP should log a warning but keep the
    // previously-loaded config — and importantly NOT bump config_generation.
    let config_path = env.config_dir.path().join("slopd/config.toml");
    std::fs::write(&config_path, "this isn't [valid toml = ").unwrap();
    let gen_before = read_config_generation(&env);
    sighup_pid(slopd.id());

    // Give slopd a moment to process the signal. There's no positive signal
    // to wait for (a failed reload doesn't bump generation), so a small sleep
    // is the best we can do without instrumenting failure cases.
    std::thread::sleep(Duration::from_millis(200));

    // Daemon must still respond and generation must NOT have advanced.
    let gen_after = read_config_generation(&env);
    assert_eq!(
        gen_after, gen_before,
        "config_generation must not advance on a failed reload"
    );

    // The original config (with --mock-session-start=skip) must still be in effect:
    // a fresh `slopctl run` should still produce a BootingUp pane.
    let run_out = env.slopctl(&["run"]);
    assert!(
        run_out.status.success(),
        "slopctl run failed: {:?}",
        run_out
    );
    let pane = String::from_utf8_lossy(&run_out.stdout).trim().to_string();
    std::thread::sleep(Duration::from_millis(500));
    let (_, detailed) = env.pane_state(&pane);
    assert_eq!(
        detailed,
        libslop::PaneDetailedState::BootingUp,
        "pane should still be BootingUp; the invalid SIGHUP should not have changed config"
    );

    kill_slopd(slopd);
}

/// SIGHUP with no config file present must be a no-op (no crash, daemon still
/// healthy). Mirrors the startup behavior where a missing config falls back
/// to defaults.
#[test]
fn sighup_with_missing_config_does_not_crash() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new(Some(&[&mock_claude_path, "--mock-session-start=skip"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Removing the file then SIGHUP-ing reloads to defaults (mirrors the
    // startup behavior). We just want to confirm the daemon survives.
    let config_path = env.config_dir.path().join("slopd/config.toml");
    std::fs::remove_file(&config_path).unwrap();
    sighup_pid(slopd.id());
    wait_for_config_generation_at_least(&env, 1, Duration::from_secs(5));

    let status_out = env.slopctl(&["status"]);
    assert!(
        status_out.status.success(),
        "slopctl status failed after missing-config SIGHUP: {:?}",
        status_out
    );

    kill_slopd(slopd);
}

/// SIGHUP must not disturb existing panes — they keep running with whatever
/// executable spawned them. Reload only affects subsequent operations.
#[test]
fn sighup_does_not_disturb_existing_panes() {
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
    let run_out = env.slopctl(&["run"]);
    assert!(
        run_out.status.success(),
        "slopctl run failed: {:?}",
        run_out
    );
    let pane = String::from_utf8_lossy(&run_out.stdout).trim().to_string();
    env.wait_for_session_start(listener, &pane);

    // Rewrite to a different executable, then SIGHUP. The existing pane should
    // be unaffected.
    env.tmux.write_slopd_config_full(
        &env.config_dir,
        Some(&[&mock_claude_path, "--mock-session-start=skip"]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
        None,
    );
    sighup_pid(slopd.id());
    wait_for_config_generation_at_least(&env, 1, Duration::from_secs(5));

    // Pane should still be visible in ps and reachable via the daemon.
    let ps_out = env.slopctl(&["ps", "--json"]);
    assert!(ps_out.status.success(), "slopctl ps failed: {:?}", ps_out);
    let panes: Vec<libslop::PaneInfo> = serde_json::from_slice(&ps_out.stdout).unwrap();
    assert!(
        panes.iter().any(|p| p.pane_id == pane),
        "existing pane {} should still be in ps after SIGHUP: {:?}",
        pane,
        panes
    );

    kill_slopd(slopd);
}

/// `wait --until` with a jq-style path containing `[]` should match when ANY
/// element of the array satisfies the rest of the path. Verifies that the
/// real motivating case (an assistant message whose content[] has a "text"
/// block sandwiched among "thinking" blocks) now works.
#[test]
fn wait_until_jq_array_path_matches_any_element() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Need a managed pane so hook events aren't dropped.
    let run_out = env.slopctl(&["run"]);
    assert!(
        run_out.status.success(),
        "slopctl run failed: {:?}",
        run_out
    );
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    let wait_child = Command::new(cargo_bin("slopctl"))
        .args([
            "wait",
            "--hook",
            "JqArrayCase",
            "--until",
            "items[].type=text",
            "--timeout",
            "10",
        ])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl wait");

    wait_for_subscriber_count_at_least(&env, 1, Duration::from_secs(5));

    // Payload mimics the real "thinking then text" assistant content shape.
    let payload = r#"{"items":[{"type":"thinking","text":"…"},{"type":"text","text":"hi"}]}"#;
    let out = fire_hook(&env, "JqArrayCase", payload, Some(&pane_id));
    assert!(out.status.success(), "fire_hook failed: {:?}", out);

    let out = wait_child.wait_with_output().expect("wait failed");
    kill_slopd(slopd);

    assert!(
        out.status.success(),
        "wait --until items[].type=text should exit 0, got {:?}",
        out.status
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"event_type\":\"JqArrayCase\""),
        "matching record should be printed; got: {:?}",
        stdout
    );
}

/// Server-side `--where` should drop non-matching events at the daemon —
/// they never reach the listener. Fires two events with different payloads
/// against `listen --where` and asserts only the matching one is delivered.
#[test]
fn listen_where_filters_at_server() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_out = env.slopctl(&["run"]);
    assert!(
        run_out.status.success(),
        "slopctl run failed: {:?}",
        run_out
    );
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    let mut listen = Command::new(cargo_bin("slopctl"))
        .args([
            "listen",
            "--hook",
            "JqArrayCase",
            "--where",
            "items[].type=text",
        ])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl listen");

    // Read the {"subscribed":true} confirmation so we know the subscription is live.
    let stdout = listen.stdout.as_mut().unwrap();
    let mut subscribed = Vec::new();
    let mut buf = [0u8; 1];
    loop {
        use std::io::Read;
        stdout.read_exact(&mut buf).expect("subscribed read failed");
        if buf[0] == b'\n' {
            break;
        }
        subscribed.push(buf[0]);
    }
    let subscribed = String::from_utf8_lossy(&subscribed);
    assert!(
        subscribed.contains("subscribed"),
        "first line: {:?}",
        subscribed
    );

    // Event #1: should be filtered OUT (no items.[].type == "text").
    let payload_no = r#"{"items":[{"type":"thinking"},{"type":"tool_use"}]}"#;
    let out = fire_hook(&env, "JqArrayCase", payload_no, Some(&pane_id));
    assert!(out.status.success(), "fire_hook (no) failed: {:?}", out);

    // Event #2: should be DELIVERED.
    let payload_yes = r#"{"items":[{"type":"thinking"},{"type":"text"}],"marker":"yes"}"#;
    let out = fire_hook(&env, "JqArrayCase", payload_yes, Some(&pane_id));
    assert!(out.status.success(), "fire_hook (yes) failed: {:?}", out);

    // Read up to one record (with timeout). The first event-line we see must
    // be the "yes" payload — if --where wasn't enforced, the "no" payload
    // would arrive first.
    let stdout = listen.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<serde_json::Value>();
    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { return };
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
                && v.get("source").and_then(|s| s.as_str()) == Some("hook")
            {
                let _ = tx.send(v);
                return;
            }
        }
    });
    let received = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("timed out — --where may not be enforced server-side");

    kill_child(listen);
    kill_slopd(slopd);

    assert_eq!(
        received["payload"]["marker"], "yes",
        "first delivered event must be the matching one (--where enforced server-side); got: {}",
        received
    );
}

/// `wait --until` with a malformed path should fail fast with a clear error,
/// not silently never-match-and-time-out.
#[test]
fn wait_until_rejects_malformed_path() {
    build_bin("slopctl");

    let runtime_dir = tempfile::tempdir().unwrap();
    let out = Command::new(cargo_bin("slopctl"))
        .args(["wait", "--hook", "Whatever", "--until", "foo[abc]=x"])
        .env("XDG_RUNTIME_DIR", runtime_dir.path())
        .output()
        .expect("failed to run slopctl wait");

    assert!(!out.status.success(), "wait should reject malformed path");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("invalid predicate") || stderr.contains("non-negative integer"),
        "error should mention the bad path; got stderr: {:?}",
        stderr
    );
}

// ---------------------------------------------------------------------------
// Transcript-signal confirmation for client-local slash commands.
//
// /model, /effort, /compact, /clear fire NO UserPromptSubmit hook in real
// Claude (empirically confirmed against real Claude 2026-05-17). slopd's
// `slopctl send` confirmation currently waits only on the UserPromptSubmit
// hook (or a queue-operation enqueue transcript record), so sending any of
// these slash commands times out even though the command was accepted.
//
// The existing hook/state-based confirmation is kept; a transcript-signal
// confirmation is added beside it — the generic `<command-name>/X` user
// record notifies pending senders. Either signal confirms.
// ---------------------------------------------------------------------------

/// Spawn a managed mock_claude pane and return (env, slopd child, pane_id).
fn spawn_pane_for_slash_test() -> Option<(
    Arc<TestEnv>,
    std::process::Child,
    String,
    std::path::PathBuf,
)> {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let env = TestEnv::new_full(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    )?;
    // Leak the tempdir so the transcript survives for the duration of the test.
    std::mem::forget(home_dir);

    let env = Arc::new(env);
    let slopd = env.spawn_slopd();
    let listener = env.spawn_session_start_listener();
    let run_out = env.slopctl(&["run"]);
    assert!(
        run_out.status.success(),
        "slopctl run failed: {:?}",
        run_out
    );
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();
    env.wait_for_session_start(listener, &pane_id);
    Some((env, slopd, pane_id, claude_config_dir))
}

/// `slopctl send <pane> "/model <id>"` must be confirmed via the transcript
/// `<command-name>` signal even though /model fires no UserPromptSubmit hook.
#[test]
fn send_model_slash_confirmed_via_transcript() {
    let Some((env, slopd, pane_id, _cfg)) = spawn_pane_for_slash_test() else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let out = env.slopctl(&["send", &pane_id, "/model claude-opus-4-7", "--timeout", "8"]);

    kill_slopd(slopd);

    assert!(
        out.status.success(),
        "slopctl send of /model should be confirmed via the transcript \
         <command-name> signal (no UserPromptSubmit fires); got: {:?} stderr={:?}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
}

/// `/effort <level>` is the same class as /model — confirmed via transcript.
#[test]
fn send_effort_slash_confirmed_via_transcript() {
    let Some((env, slopd, pane_id, _cfg)) = spawn_pane_for_slash_test() else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let out = env.slopctl(&["send", &pane_id, "/effort high", "--timeout", "8"]);

    kill_slopd(slopd);

    assert!(
        out.status.success(),
        "slopctl send of /effort should be confirmed via transcript; got: {:?} stderr={:?}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
}

/// `/compact` and `/clear` also fire no UserPromptSubmit — both must confirm
/// via the generic `<command-name>` transcript signal.
#[test]
fn send_compact_and_clear_confirmed_via_transcript() {
    let Some((env, slopd, pane_id, _cfg)) = spawn_pane_for_slash_test() else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let compact = env.slopctl(&["send", &pane_id, "/compact", "--timeout", "8"]);
    assert!(
        compact.status.success(),
        "slopctl send of /compact should be confirmed via transcript; got: {:?} stderr={:?}",
        compact.status,
        String::from_utf8_lossy(&compact.stderr),
    );

    let clear = env.slopctl(&["send", &pane_id, "/clear", "--timeout", "8"]);

    kill_slopd(slopd);

    assert!(
        clear.status.success(),
        "slopctl send of /clear should be confirmed via transcript; got: {:?} stderr={:?}",
        clear.status,
        String::from_utf8_lossy(&clear.stderr),
    );
}

/// Regression guard: the existing UserPromptSubmit-hook confirmation path must
/// keep working for normal prompts ("either one signal confirms"). This passes
/// today and must keep passing after the transcript signal is added.
#[test]
fn normal_prompt_still_confirmed_via_hook() {
    let Some((env, slopd, pane_id, _cfg)) = spawn_pane_for_slash_test() else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let out = env.slopctl(&["send", &pane_id, "hello world", "--timeout", "10"]);

    kill_slopd(slopd);

    assert!(
        out.status.success(),
        "normal prompt must still confirm via the UserPromptSubmit hook; got: {:?} stderr={:?}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
}

/// `slopctl wait --pane-id <garbage>` must error eagerly with a message that
/// points at the right shape, rather than silently subscribing to nothing.
#[test]
fn wait_pane_id_rejects_garbage_with_helpful_error() {
    build_bin("slopctl");
    let runtime_dir = tempfile::tempdir().unwrap();
    let out = Command::new(cargo_bin("slopctl"))
        .args(["wait", "--pane-id", "not-a-real-pane", "--timeout", "1"])
        .env("XDG_RUNTIME_DIR", runtime_dir.path())
        .output()
        .expect("failed to run slopctl wait");

    assert!(
        !out.status.success(),
        "expected non-zero exit for garbage --pane-id"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--pane-id"),
        "stderr should mention --pane-id: {:?}",
        stderr
    );
    assert!(
        stderr.contains("tmux pane id"),
        "stderr should explain shape: {:?}",
        stderr
    );
    assert!(
        stderr.contains("UUID"),
        "stderr should mention UUID option: {:?}",
        stderr
    );
}

/// `slopctl listen --pane-id <garbage>` must error the same way `wait` does.
/// (Both flow through the same `resolve_pane_id_or_session` helper.)
#[test]
fn listen_pane_id_rejects_garbage_with_helpful_error() {
    build_bin("slopctl");
    let runtime_dir = tempfile::tempdir().unwrap();
    let out = Command::new(cargo_bin("slopctl"))
        .args(["listen", "--pane-id", "garbage"])
        .env("XDG_RUNTIME_DIR", runtime_dir.path())
        .output()
        .expect("failed to run slopctl listen");

    assert!(
        !out.status.success(),
        "expected non-zero exit for garbage --pane-id"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--pane-id"),
        "stderr should mention --pane-id: {:?}",
        stderr
    );
}

/// `slopctl wait --pane-id <UUID>` must fail loudly, pointing the caller at
/// `--session-id` instead of silently routing across flags. Filter semantics
/// stay explicit: if you want session filtering, use `--session-id`.
#[test]
fn wait_pane_id_rejects_uuid_with_session_id_hint() {
    build_bin("slopctl");
    let runtime_dir = tempfile::tempdir().unwrap();
    let session_uuid = "31a02dee-3e6d-42f0-b7c4-4382305b7e10";

    let out = Command::new(cargo_bin("slopctl"))
        .args(["wait", "--pane-id", session_uuid, "--timeout", "1"])
        .env("XDG_RUNTIME_DIR", runtime_dir.path())
        .output()
        .expect("failed to run slopctl wait");

    assert!(
        !out.status.success(),
        "expected non-zero exit for UUID --pane-id"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--pane-id"),
        "stderr should mention --pane-id: {:?}",
        stderr
    );
    assert!(
        stderr.contains("UUID"),
        "stderr should explain the UUID detection: {:?}",
        stderr
    );
    assert!(
        stderr.contains("--session-id"),
        "stderr should point at --session-id for UUIDs: {:?}",
        stderr
    );
}

/// Same loud-failure behavior for `slopctl listen --pane-id <UUID>`.
#[test]
fn listen_pane_id_rejects_uuid_with_session_id_hint() {
    build_bin("slopctl");
    let runtime_dir = tempfile::tempdir().unwrap();
    let session_uuid = "31a02dee-3e6d-42f0-b7c4-4382305b7e10";

    let out = Command::new(cargo_bin("slopctl"))
        .args(["listen", "--pane-id", session_uuid])
        .env("XDG_RUNTIME_DIR", runtime_dir.path())
        .output()
        .expect("failed to run slopctl listen");

    assert!(
        !out.status.success(),
        "expected non-zero exit for UUID --pane-id"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--session-id"),
        "stderr should point at --session-id for UUIDs: {:?}",
        stderr
    );
}

/// `slopctl wait` against a pane already in the target state must exit
/// immediately with a synthetic `CurrentState` record. The snapshot-then-wait
/// is the default behavior: no flag needed.
#[test]
fn wait_short_circuits_when_pane_state_already_matches() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };
    let slopd = env.spawn_slopd();

    let run_out = env.slopctl(&["run"]);
    assert!(
        run_out.status.success(),
        "slopctl run failed: {:?}",
        run_out
    );
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    // Drive the pane into the Ready state up-front via SessionStart hook.
    assert_state_after_hook(
        &env,
        &pane_id,
        "SessionStart",
        &format!(
            r#"{{"session_id":"s1","hook_event_name":"SessionStart","transcript_path":"/dev/null","cwd":"/tmp","pane_id":"{}"}}"#,
            pane_id
        ),
        libslop::PaneState::Ready,
        libslop::PaneDetailedState::Ready,
    );

    // wait with --pane-id and --where: the pane is already Ready, so the seed
    // must match and return without consuming any real event. Without the
    // automatic seed this would block on a state change that never happens
    // (the pane is already in the target state).
    let start = Instant::now();
    let out = Command::new(cargo_bin("slopctl"))
        .args([
            "wait",
            "--pane-id",
            &pane_id,
            "--where",
            "state=ready",
            "--timeout",
            "30",
        ])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .output()
        .expect("failed to run slopctl wait");
    let elapsed = start.elapsed();

    kill_slopd(slopd);

    assert!(
        out.status.success(),
        "slopctl wait should exit 0 when state already matches; got {:?} stderr={:?}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "wait should return immediately on snapshot match, took {:?}",
        elapsed
    );

    // Output: {"subscribed":true} line, then a synthetic record with
    // event_type=CurrentState and seeded_current=true in payload.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut lines = stdout.lines();
    let first = lines.next().expect("missing subscribed line");
    assert!(
        first.contains("\"subscribed\":true"),
        "first line should confirm subscribe: {:?}",
        first
    );

    let seeded: serde_json::Value = lines
        .filter_map(|line| serde_json::from_str(line).ok())
        .find(|v: &serde_json::Value| v["event_type"] == "CurrentState")
        .unwrap_or_else(|| {
            panic!(
                "missing CurrentState seeded record in wait stdout: {:?}",
                stdout
            )
        });
    assert_eq!(seeded["source"], "slopd");
    assert_eq!(seeded["pane_id"], pane_id);
    assert_eq!(seeded["payload"]["state"], "ready");
    assert_eq!(seeded["payload"]["detailed_state"], "ready");
    assert_eq!(seeded["payload"]["seeded_current"], true);
}

/// `slopctl wait` without `--pane-id` or `--session-id` skips the snapshot
/// (no target to query) and falls through to normal live-event waiting.
/// This is silent fall-through, not an error.
#[test]
fn wait_without_target_falls_through_to_live_wait() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };
    let slopd = env.spawn_slopd();

    // No --pane-id / --session-id. With no target, snapshot can't run, so
    // the wait falls through to live events and times out (no hook fires).
    let out = Command::new(cargo_bin("slopctl"))
        .args(["wait", "--hook", "NeverFires", "--timeout", "1"])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .output()
        .expect("failed to run slopctl wait");

    kill_slopd(slopd);

    assert!(
        !out.status.success(),
        "wait should time out (non-zero exit), got {:?}",
        out.status
    );
    // No spurious "requires --pane-id" error — just a normal timeout.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("timed out"),
        "expected normal timeout error, not a target-required error; got: {:?}",
        stderr
    );
}

/// `slopctl wait --no-snapshot` skips the pre-wait pane-state snapshot and
/// waits for a real live event instead, even when the pane already satisfies
/// the predicates. Use case: the caller explicitly wants the *next* transition.
#[test]
fn wait_no_snapshot_ignores_current_state_and_waits_for_event() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };
    let slopd = env.spawn_slopd();

    let run_out = env.slopctl(&["run"]);
    assert!(
        run_out.status.success(),
        "slopctl run failed: {:?}",
        run_out
    );
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    // Drive the pane to Ready. Without --no-snapshot the wait below would
    // exit immediately via the seeded CurrentState record.
    assert_state_after_hook(
        &env,
        &pane_id,
        "SessionStart",
        &format!(
            r#"{{"session_id":"s1","hook_event_name":"SessionStart","transcript_path":"/dev/null","cwd":"/tmp","pane_id":"{}"}}"#,
            pane_id
        ),
        libslop::PaneState::Ready,
        libslop::PaneDetailedState::Ready,
    );

    // With --no-snapshot: the wait skips the snapshot and times out because
    // no further state-change event ever fires (the pane is already Ready).
    let start = Instant::now();
    let out = Command::new(cargo_bin("slopctl"))
        .args([
            "wait",
            "--pane-id",
            &pane_id,
            "--where",
            "state=ready",
            "--no-snapshot",
            "--timeout",
            "1",
        ])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .output()
        .expect("failed to run slopctl wait");
    let elapsed = start.elapsed();

    kill_slopd(slopd);

    assert!(
        !out.status.success(),
        "wait --no-snapshot should time out (the pane is already in target state, no next transition); got {:?}",
        out.status
    );
    // Should actually wait until the timeout, not exit instantly.
    assert!(
        elapsed >= Duration::from_millis(800),
        "wait --no-snapshot should not short-circuit on the snapshot; got elapsed {:?}",
        elapsed
    );

    // Confirm the subscribed line went out but no CurrentState seeded record.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let has_seeded = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .any(|v| v["event_type"] == "CurrentState");
    assert!(
        !has_seeded,
        "wait --no-snapshot must not emit a CurrentState seeded record; stdout={:?}",
        stdout
    );
}

/// `--where` accepts both `state=ready` and `.state=ready` — the leading dot
/// is optional, matching jq syntax. This regression-guards the `--help` text.
#[test]
fn wait_where_leading_dot_optional_end_to_end() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };
    let slopd = env.spawn_slopd();

    let run_out = env.slopctl(&["run"]);
    assert!(
        run_out.status.success(),
        "slopctl run failed: {:?}",
        run_out
    );
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    // Drive the pane to Ready so the snapshot can match.
    assert_state_after_hook(
        &env,
        &pane_id,
        "SessionStart",
        &format!(
            r#"{{"session_id":"s1","hook_event_name":"SessionStart","transcript_path":"/dev/null","cwd":"/tmp","pane_id":"{}"}}"#,
            pane_id
        ),
        libslop::PaneState::Ready,
        libslop::PaneDetailedState::Ready,
    );

    // With leading dot: should match and exit immediately.
    let out_dot = Command::new(cargo_bin("slopctl"))
        .args([
            "wait",
            "--pane-id",
            &pane_id,
            "--where",
            ".state=ready",
            "--timeout",
            "5",
        ])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .output()
        .expect("failed to run slopctl wait");

    // Without leading dot: must also match.
    let out_no_dot = Command::new(cargo_bin("slopctl"))
        .args([
            "wait",
            "--pane-id",
            &pane_id,
            "--where",
            "state=ready",
            "--timeout",
            "5",
        ])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .output()
        .expect("failed to run slopctl wait");

    kill_slopd(slopd);

    assert!(
        out_dot.status.success(),
        "wait --where .state=ready should succeed, got {:?}",
        out_dot.status
    );
    assert!(
        out_no_dot.status.success(),
        "wait --where state=ready should succeed, got {:?}",
        out_no_dot.status
    );
}

// ---------------------------------------------------------------------------
// Multi-account support
// ---------------------------------------------------------------------------

/// Switch a freshly-spawned mock_claude pane into always-submit mode so a plain
/// tmux `send-keys` + Enter submits a prompt (no `slopctl send` needed). Mirrors
/// the setup in `run_does_not_set_claude_config_dir_when_not_configured`.
fn enable_always_submit(env: &TestEnv, pane_id: &str) {
    // Give mock_claude a moment to enter raw mode before sending keys.
    std::thread::sleep(Duration::from_millis(200));
    // Two Enters: the first is literal (alternating-mode default), the second submits.
    env.tmux
        .tmux()
        .args([
            "send-keys",
            "-t",
            pane_id,
            "::mock input-mode always-submit",
            "Enter",
            "Enter",
        ])
        .status()
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
}

/// Ask a mock_claude pane (already in always-submit mode) to print one of its
/// environment variables, returning the full `::mock env VAR=value` line it emits
/// ("UNSET" when the variable is absent). Polls the pane until the line appears.
fn query_pane_env(env: &TestEnv, pane_id: &str, var: &str) -> String {
    env.tmux
        .tmux()
        .args([
            "send-keys",
            "-t",
            pane_id,
            &format!("::mock env {}", var),
            "Enter",
        ])
        .status()
        .unwrap();
    let needle = format!("::mock env {}=", var);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let out = env
            .tmux
            .tmux()
            .args(["capture-pane", "-t", pane_id, "-p"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        // tmux may wrap long lines; join the full output before searching.
        let joined = text.replace(['\n', '\r'], "");
        if let Some(pos) = joined.find(&needle) {
            let value = joined[pos + needle.len()..]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
            return format!("{needle}{value}");
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for ::mock env {} output",
            var
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Ask a mock_claude pane (already in always-submit mode) to print its current
/// working directory, returning the path from the `::mock cwd <path>` line it emits.
/// Polls the pane until the line appears.
fn query_pane_cwd(env: &TestEnv, pane_id: &str) -> String {
    env.tmux
        .tmux()
        .args(["send-keys", "-t", pane_id, "::mock cwd", "Enter"])
        .status()
        .unwrap();
    let needle = "::mock cwd ";
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let out = env
            .tmux
            .tmux()
            .args(["capture-pane", "-t", pane_id, "-p"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        // tmux may wrap long paths; join the full output before searching.
        let joined = text.replace(['\n', '\r'], "");
        if let Some(pos) = joined.find(needle) {
            return joined[pos + needle.len()..]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for ::mock cwd output; pane: {:?}",
            text
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Return the account `slopctl ps` reports for `pane_id` (requires slopd alive).
fn pane_account(env: &TestEnv, pane_id: &str) -> String {
    let out = env.slopctl(&["ps", "--json"]);
    assert!(out.status.success(), "ps --json failed: {:?}", out);
    let panes: Vec<libslop::PaneInfo> =
        serde_json::from_slice(&out.stdout).expect("ps --json should parse");
    panes
        .into_iter()
        .find(|p| p.pane_id == pane_id)
        .unwrap_or_else(|| panic!("pane {} not found in ps", pane_id))
        .account
}

#[test]
fn run_account_sets_config_dir_and_shows_in_ps() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let accounts_root = tempfile::tempdir().unwrap();
    let work_dir = accounts_root.path().join("work");

    let Some(env) = TestEnv::new_with_accounts(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        &[("work", work_dir.as_path())],
        None,
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_out = env.slopctl(&["run", "--account", "work"]);
    assert!(
        run_out.status.success(),
        "run --account work failed: {:?}",
        run_out
    );
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    let account = pane_account(&env, &pane_id);
    enable_always_submit(&env, &pane_id);
    let config_dir_line = query_pane_env(&env, &pane_id, "CLAUDE_CONFIG_DIR");

    kill_slopd(slopd);

    assert_eq!(
        config_dir_line,
        format!(
            "::mock env CLAUDE_CONFIG_DIR={}",
            work_dir.to_str().unwrap()
        ),
        "CLAUDE_CONFIG_DIR should point at the selected account's dir",
    );
    assert_eq!(account, "work", "ps should report the pane's account");
}

#[test]
fn run_uses_default_account_without_flag() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let accounts_root = tempfile::tempdir().unwrap();
    let work_dir = accounts_root.path().join("work");

    let Some(env) = TestEnv::new_with_accounts(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        &[("work", work_dir.as_path())],
        Some("work"),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // No --account flag: slopd's default_account should apply.
    let run_out = env.slopctl(&["run"]);
    assert!(run_out.status.success(), "run failed: {:?}", run_out);
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    let account = pane_account(&env, &pane_id);
    enable_always_submit(&env, &pane_id);
    let config_dir_line = query_pane_env(&env, &pane_id, "CLAUDE_CONFIG_DIR");

    kill_slopd(slopd);

    assert_eq!(
        config_dir_line,
        format!(
            "::mock env CLAUDE_CONFIG_DIR={}",
            work_dir.to_str().unwrap()
        ),
        "default_account should set CLAUDE_CONFIG_DIR",
    );
    assert_eq!(
        account, "work",
        "default_account should select the work account"
    );
}

#[test]
fn run_inherits_account_from_parent_pane() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let accounts_root = tempfile::tempdir().unwrap();
    let work_dir = accounts_root.path().join("work");

    let Some(env) = TestEnv::new_with_accounts(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        &[("work", work_dir.as_path())],
        None,
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // A parent pane on the "work" account.
    let parent_out = env.slopctl(&["run", "--account", "work"]);
    assert!(
        parent_out.status.success(),
        "parent run failed: {:?}",
        parent_out
    );
    let parent_id = String::from_utf8_lossy(&parent_out.stdout)
        .trim()
        .to_string();

    // Spawn a child as if from inside the parent: TMUX_PANE points at the
    // parent and no --account is given, so the daemon inherits the parent's
    // account from its @slopd_account option.
    let child_out =
        env.slopctl_raw_envs(&["run", "--no-wait"], &[("TMUX_PANE", parent_id.as_str())]);
    assert!(
        child_out.status.success(),
        "child run failed: {:?}",
        child_out
    );
    let child_id = String::from_utf8_lossy(&child_out.stdout)
        .trim()
        .to_string();

    let child_account = pane_account(&env, &child_id);
    enable_always_submit(&env, &child_id);
    let config_dir_line = query_pane_env(&env, &child_id, "CLAUDE_CONFIG_DIR");

    kill_slopd(slopd);

    assert_eq!(
        child_account, "work",
        "child should inherit the parent pane's account"
    );
    assert_eq!(
        config_dir_line,
        format!(
            "::mock env CLAUDE_CONFIG_DIR={}",
            work_dir.to_str().unwrap()
        ),
        "inherited account should resolve to the parent's config dir",
    );
}

#[test]
fn run_account_flag_overrides_inherited_account() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let accounts_root = tempfile::tempdir().unwrap();
    let work_dir = accounts_root.path().join("work");
    let personal_dir = accounts_root.path().join("personal");

    let Some(env) = TestEnv::new_with_accounts(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        &[
            ("work", work_dir.as_path()),
            ("personal", personal_dir.as_path()),
        ],
        None,
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // A parent pane on "work".
    let parent_out = env.slopctl(&["run", "--account", "work"]);
    assert!(
        parent_out.status.success(),
        "parent run failed: {:?}",
        parent_out
    );
    let parent_id = String::from_utf8_lossy(&parent_out.stdout)
        .trim()
        .to_string();

    // From inside the "work" parent, but with an explicit --account personal:
    // the flag must win over the inherited account.
    let child_out = env.slopctl_raw_envs(
        &["run", "--no-wait", "--account", "personal"],
        &[("TMUX_PANE", parent_id.as_str())],
    );
    assert!(
        child_out.status.success(),
        "run --account personal failed: {:?}",
        child_out
    );
    let child_id = String::from_utf8_lossy(&child_out.stdout)
        .trim()
        .to_string();

    let child_account = pane_account(&env, &child_id);
    enable_always_submit(&env, &child_id);
    let config_dir_line = query_pane_env(&env, &child_id, "CLAUDE_CONFIG_DIR");

    kill_slopd(slopd);

    assert_eq!(
        child_account, "personal",
        "--account should override the inherited account"
    );
    assert_eq!(
        config_dir_line,
        format!(
            "::mock env CLAUDE_CONFIG_DIR={}",
            personal_dir.to_str().unwrap()
        ),
        "--account should override the inherited account's dir",
    );
}

#[test]
fn run_unknown_account_fails_without_spawning() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let accounts_root = tempfile::tempdir().unwrap();
    let work_dir = accounts_root.path().join("work");

    let Some(env) = TestEnv::new_with_accounts(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        &[("work", work_dir.as_path())],
        None,
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let pane_count = |env: &TestEnv| -> usize {
        let ps_out = env.slopctl(&["ps", "--json"]);
        let panes: Vec<libslop::PaneInfo> =
            serde_json::from_slice(&ps_out.stdout).expect("ps --json should parse");
        panes.len()
    };

    let before = pane_count(&env);
    let run_out = env.slopctl_raw(&["run", "--no-wait", "--account", "ghost"]);
    // The account is resolved before any tmux window is created, so a failed
    // resolution must not change the pane count.
    let after = pane_count(&env);

    kill_slopd(slopd);

    assert!(
        !run_out.status.success(),
        "run with an unknown account should fail: {:?}",
        run_out
    );
    let stderr = String::from_utf8_lossy(&run_out.stderr);
    assert!(
        stderr.contains("unknown account"),
        "stderr should explain the failure: {}",
        stderr
    );
    assert!(
        stderr.contains("ghost"),
        "stderr should name the bad account: {}",
        stderr
    );
    assert!(
        stderr.contains("work"),
        "stderr should list configured accounts: {}",
        stderr
    );
    assert_eq!(
        before, after,
        "no pane should be spawned for an unknown account"
    );
}

#[test]
fn run_account_injects_hooks_into_account_settings_only() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let accounts_root = tempfile::tempdir().unwrap();
    let work_dir = accounts_root.path().join("work");
    let personal_dir = accounts_root.path().join("personal");

    let Some(env) = TestEnv::new_with_accounts(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        &[
            ("work", work_dir.as_path()),
            ("personal", personal_dir.as_path()),
        ],
        None,
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_out = env.slopctl(&["run", "--account", "work"]);
    assert!(
        run_out.status.success(),
        "run --account work failed: {:?}",
        run_out
    );

    // Read BEFORE shutting slopd down — shutdown removes the hooks again.
    let work_settings = std::fs::read_to_string(work_dir.join("settings.json"))
        .expect("work account settings.json should exist after run");
    // The other account, never launched, must be left untouched.
    let personal_exists = personal_dir.join("settings.json").exists();

    kill_slopd(slopd);

    assert!(
        work_settings.contains("hook SessionStart"),
        "hooks should be injected into the launched account's settings.json: {}",
        work_settings,
    );
    assert!(
        !personal_exists,
        "an account that was never launched should not have its settings.json touched"
    );
}

#[test]
fn run_default_account_maps_to_top_level_claude_config_dir() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    // Top-level claude_config_dir, no [accounts]: it backs the "default" account.
    let cc_root = tempfile::tempdir().unwrap();
    let claude_config_dir = cc_root.path().join("claude-default");

    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Explicit `--account default` resolves to the top-level claude_config_dir.
    let run_out = env.slopctl(&["run", "--account", "default"]);
    assert!(
        run_out.status.success(),
        "run --account default failed: {:?}",
        run_out
    );
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    let account = pane_account(&env, &pane_id);
    enable_always_submit(&env, &pane_id);
    let config_dir_line = query_pane_env(&env, &pane_id, "CLAUDE_CONFIG_DIR");

    kill_slopd(slopd);

    assert_eq!(
        account, "default",
        "the pane should be on the default account"
    );
    assert_eq!(
        config_dir_line,
        format!(
            "::mock env CLAUDE_CONFIG_DIR={}",
            claude_config_dir.to_str().unwrap()
        ),
        "the default account should use the top-level claude_config_dir",
    );
}

#[test]
fn ps_shows_account_column() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let accounts_root = tempfile::tempdir().unwrap();
    let work_dir = accounts_root.path().join("work");

    let Some(env) = TestEnv::new_with_accounts(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        &[("work", work_dir.as_path())],
        None,
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_out = env.slopctl(&["run", "--account", "work"]);
    assert!(
        run_out.status.success(),
        "run --account work failed: {:?}",
        run_out
    );

    // The default table output (not --json) should carry an ACCOUNT column.
    let ps_out = env.slopctl(&["ps"]);

    kill_slopd(slopd);

    assert!(ps_out.status.success(), "ps failed: {:?}", ps_out);
    let stdout = String::from_utf8_lossy(&ps_out.stdout);
    assert!(
        stdout.contains("ACCOUNT"),
        "ps header should include ACCOUNT:\n{}",
        stdout
    );
    assert!(
        stdout.contains("work"),
        "ps should show the pane's account:\n{}",
        stdout
    );
}

#[test]
fn transcript_discovered_and_tailed_for_non_default_account() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let accounts_root = tempfile::tempdir().unwrap();
    let work_dir = accounts_root.path().join("work");

    let Some(env) = TestEnv::new_with_accounts(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        &[("work", work_dir.as_path())],
        None,
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Subscribe to transcript user+assistant events BEFORE spawning the pane.
    let mut listener = Command::new(cargo_bin("slopctl"))
        .args([
            "listen",
            "--transcript",
            "user",
            "--transcript",
            "assistant",
        ])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn transcript listener");
    {
        let stdout = listener.stdout.as_mut().unwrap();
        let mut line = Vec::new();
        let mut buf = [0u8; 1];
        loop {
            use std::io::Read;
            stdout
                .read_exact(&mut buf)
                .expect("failed to read subscription confirmation");
            if buf[0] == b'\n' {
                break;
            }
            line.push(buf[0]);
        }
        assert!(String::from_utf8_lossy(&line).contains("subscribed"));
    }

    // Spawn a pane on the "work" account and wait for SessionStart, whose hook
    // payload carries the transcript_path (under work_dir) that triggers tailing.
    let session_listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run", "--account", "work"]);
    assert!(
        run_output.status.success(),
        "run --account work failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(session_listener, &pane_id);

    // A prompt makes mock_claude write user + assistant records under
    // CLAUDE_CONFIG_DIR (= work_dir).
    let send_output = env.slopctl(&["send", &pane_id, "hello work account"]);
    assert!(
        send_output.status.success(),
        "slopctl send failed: {:?}",
        send_output
    );

    // Collect streamed transcript events — this proves slopd discovered and
    // tailed the account's relocated JSONL, not a default-dir path.
    let stdout = listener.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<Vec<serde_json::Value>>();
    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stdout);
        let mut events = Vec::new();
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
                && v.get("source").and_then(|s| s.as_str()) == Some("transcript")
            {
                events.push(v);
                if events.len() >= 2 {
                    let _ = tx.send(events);
                    return;
                }
            }
        }
        let _ = tx.send(events);
    });
    let events = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("timed out waiting for transcript events from the account pane");

    // ReadTranscript path too (slopctl transcript reads the stored path).
    let transcript_out = env.slopctl(&["transcript", &pane_id]);
    assert!(
        transcript_out.status.success(),
        "slopctl transcript failed: {:?}",
        transcript_out
    );
    let transcript_json: serde_json::Value =
        serde_json::from_slice(&transcript_out.stdout).expect("transcript output should be JSON");

    kill_child(listener);
    kill_slopd(slopd);

    // Streamed events: a user + assistant record for this pane, carrying the prompt.
    let types: Vec<&str> = events
        .iter()
        .filter_map(|e| e.get("event_type").and_then(|t| t.as_str()))
        .collect();
    assert!(
        types.contains(&"user"),
        "missing 'user' transcript event, got: {:?}",
        types
    );
    assert!(
        types.contains(&"assistant"),
        "missing 'assistant' transcript event, got: {:?}",
        types
    );
    let user_event = events.iter().find(|e| e["event_type"] == "user").unwrap();
    assert_eq!(
        user_event.get("pane_id").and_then(|p| p.as_str()),
        Some(pane_id.as_str())
    );
    let user_content = user_event["payload"]["message"]["content"]
        .as_str()
        .unwrap_or("");
    assert!(
        user_content.contains("hello work account"),
        "streamed user record should contain the prompt, got: {:?}",
        user_content
    );

    // The JSONL must physically live under the account dir, and not the default.
    let records = read_transcript(&work_dir);
    assert!(
        records.iter().any(|r| r["type"] == "user"
            && r["message"]["content"]
                .as_str()
                .is_some_and(|c| c.contains("hello work account"))),
        "transcript under the account dir should contain the user record: {:?}",
        records,
    );
    let default_transcript = env
        .config_dir
        .path()
        .join(".claude/projects/mock/mock-session-id-1234.jsonl");
    assert!(
        !default_transcript.exists(),
        "no transcript should be written under the default dir for an account pane"
    );

    // slopctl transcript (ReadTranscript) returned the records from the account dir too.
    let read_records = transcript_json["records"]
        .as_array()
        .expect("records array");
    assert!(
        read_records.iter().any(|r| r["event_type"] == "user"),
        "slopctl transcript should return the account pane's user record: {:?}",
        read_records
    );
}

#[test]
fn transcript_tailing_resumes_after_restart_for_non_default_account() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let accounts_root = tempfile::tempdir().unwrap();
    let work_dir = accounts_root.path().join("work");

    let Some(env) = TestEnv::new_with_accounts(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        &[("work", work_dir.as_path())],
        None,
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let session_listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run", "--account", "work"]);
    assert!(
        run_output.status.success(),
        "run --account work failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(session_listener, &pane_id);

    let send_output = env.slopctl(&["send", &pane_id, "before restart"]);
    assert!(
        send_output.status.success(),
        "send before restart failed: {:?}",
        send_output
    );
    std::thread::sleep(Duration::from_millis(300));

    // --- Restart slopd: recovery must re-establish tailing on the account's transcript. ---
    kill_slopd(slopd);
    let slopd2 = env.spawn_slopd();

    // The transcript lives under the account dir. Fire SessionStart with that
    // path so the recovered pane returns to ready (recovery resumes tailing from
    // the pane's stored @slopd_transcript_path, which points under work_dir).
    let transcript_path = work_dir.join("projects/mock/mock-session-id-1234.jsonl");
    let session_start_payload = format!(
        r#"{{"session_id":"mock-session-id-1234","hook_event_name":"SessionStart","transcript_path":"{}","cwd":"/tmp","source":"startup","model":"mock"}}"#,
        transcript_path.display(),
    );
    let hook_out = fire_hook(&env, "SessionStart", &session_start_payload, Some(&pane_id));
    assert!(
        hook_out.status.success(),
        "SessionStart after restart failed: {:?}",
        hook_out
    );
    std::thread::sleep(Duration::from_millis(200));

    let mut listener = Command::new(cargo_bin("slopctl"))
        .args([
            "listen",
            "--transcript",
            "user",
            "--transcript",
            "assistant",
        ])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn transcript listener");
    {
        let stdout = listener.stdout.as_mut().unwrap();
        let mut line = Vec::new();
        let mut buf = [0u8; 1];
        loop {
            use std::io::Read;
            stdout
                .read_exact(&mut buf)
                .expect("failed to read subscription confirmation");
            if buf[0] == b'\n' {
                break;
            }
            line.push(buf[0]);
        }
        assert!(String::from_utf8_lossy(&line).contains("subscribed"));
    }

    let send_output = env.slopctl(&["send", &pane_id, "after restart"]);
    assert!(
        send_output.status.success(),
        "send after restart failed: {:?}",
        send_output
    );

    let stdout = listener.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<Vec<serde_json::Value>>();
    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stdout);
        let mut events = Vec::new();
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
                && v.get("source").and_then(|s| s.as_str()) == Some("transcript")
            {
                events.push(v);
                if events.len() >= 2 {
                    let _ = tx.send(events);
                    return;
                }
            }
        }
        let _ = tx.send(events);
    });
    let events = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("timed out waiting for transcript events after restart (account pane)");

    kill_child(listener);
    kill_slopd(slopd2);

    assert!(
        events
            .iter()
            .filter(|e| e["event_type"] == "user")
            .any(|e| {
                e["payload"]["message"]["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("after restart"))
            }),
        "tailing should resume on the account's transcript after restart, got: {:?}",
        events,
    );
}

/// True iff `path` exists and still contains slopctl hook entries.
fn settings_has_slopctl_hooks(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|c| c.contains("hook SessionStart"))
}

// #1 — two panes on different accounts at once: events route per pane, ps shows
// the right account for each, and each transcript lands under its own dir.
#[test]
fn concurrent_panes_on_different_accounts_are_isolated() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let accounts_root = tempfile::tempdir().unwrap();
    let work_dir = accounts_root.path().join("work");
    let personal_dir = accounts_root.path().join("personal");

    let Some(env) = TestEnv::new_with_accounts(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        &[
            ("work", work_dir.as_path()),
            ("personal", personal_dir.as_path()),
        ],
        None,
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let session_listener = env.spawn_session_start_listener();
    let work_pane = String::from_utf8_lossy(&env.slopctl(&["run", "--account", "work"]).stdout)
        .trim()
        .to_string();
    let personal_pane =
        String::from_utf8_lossy(&env.slopctl(&["run", "--account", "personal"]).stdout)
            .trim()
            .to_string();
    assert!(
        !work_pane.is_empty() && !personal_pane.is_empty(),
        "both panes should spawn"
    );
    env.wait_for_session_starts(session_listener, &[&work_pane, &personal_pane]);

    // A successful send waits for the UserPromptSubmit hook to round-trip, so it
    // proves each account's settings.json hooks reach slopd, attributed per pane.
    let send_work = env.slopctl(&["send", &work_pane, "from work"]);
    assert!(
        send_work.status.success(),
        "send to work pane failed: {:?}",
        send_work
    );
    let send_personal = env.slopctl(&["send", &personal_pane, "from personal"]);
    assert!(
        send_personal.status.success(),
        "send to personal pane failed: {:?}",
        send_personal
    );

    let work_account = pane_account(&env, &work_pane);
    let personal_account = pane_account(&env, &personal_pane);

    std::thread::sleep(Duration::from_millis(300));
    let work_records = read_transcript(&work_dir);
    let personal_records = read_transcript(&personal_dir);

    kill_slopd(slopd);

    assert_eq!(
        work_account, "work",
        "ps should attribute the work pane to 'work'"
    );
    assert_eq!(
        personal_account, "personal",
        "ps should attribute the personal pane to 'personal'"
    );

    let work_has = |needle: &str| {
        work_records.iter().any(|r| {
            r["type"] == "user"
                && r["message"]["content"]
                    .as_str()
                    .is_some_and(|c| c.contains(needle))
        })
    };
    let personal_has = |needle: &str| {
        personal_records.iter().any(|r| {
            r["type"] == "user"
                && r["message"]["content"]
                    .as_str()
                    .is_some_and(|c| c.contains(needle))
        })
    };
    assert!(
        work_has("from work"),
        "work transcript should hold its own prompt: {:?}",
        work_records
    );
    assert!(
        personal_has("from personal"),
        "personal transcript should hold its own prompt: {:?}",
        personal_records
    );
    // Cross-contamination check: neither account's transcript holds the other's prompt.
    assert!(
        !work_has("from personal"),
        "work transcript leaked the personal prompt"
    );
    assert!(
        !personal_has("from work"),
        "personal transcript leaked the work prompt"
    );
}

// #2 — after a restart, recovery re-injects hooks into the recovered pane's
// account dir (and leaves a never-used account untouched).
#[test]
fn restart_reinjects_hooks_into_recovered_pane_account_dir() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let accounts_root = tempfile::tempdir().unwrap();
    let work_dir = accounts_root.path().join("work");
    let personal_dir = accounts_root.path().join("personal");

    let Some(env) = TestEnv::new_with_accounts(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        &[
            ("work", work_dir.as_path()),
            ("personal", personal_dir.as_path()),
        ],
        None,
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    let session_listener = env.spawn_session_start_listener();
    let work_pane = String::from_utf8_lossy(&env.slopctl(&["run", "--account", "work"]).stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(session_listener, &work_pane);

    assert!(
        settings_has_slopctl_hooks(&work_dir.join("settings.json")),
        "work account should have hooks after run"
    );

    // Shutdown removes hooks from the account dir...
    kill_slopd(slopd);
    assert!(
        !settings_has_slopctl_hooks(&work_dir.join("settings.json")),
        "shutdown should remove hooks from the account dir"
    );

    // ...and a restart's recovery re-injects them into the *account* dir, because
    // the surviving pane records @slopd_account=work.
    let slopd2 = env.spawn_slopd();

    assert!(
        settings_has_slopctl_hooks(&work_dir.join("settings.json")),
        "recovery should re-inject hooks into the recovered pane's account dir"
    );
    assert!(
        !personal_dir.join("settings.json").exists(),
        "recovery should not touch an account with no recovered pane"
    );

    kill_slopd(slopd2);
}

// #3 — shutdown removes hooks from every account that had a pane, not just one.
#[test]
fn shutdown_removes_hooks_from_all_used_account_dirs() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let accounts_root = tempfile::tempdir().unwrap();
    let work_dir = accounts_root.path().join("work");
    let personal_dir = accounts_root.path().join("personal");

    let Some(env) = TestEnv::new_with_accounts(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        &[
            ("work", work_dir.as_path()),
            ("personal", personal_dir.as_path()),
        ],
        None,
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    let session_listener = env.spawn_session_start_listener();
    let work_pane = String::from_utf8_lossy(&env.slopctl(&["run", "--account", "work"]).stdout)
        .trim()
        .to_string();
    let personal_pane =
        String::from_utf8_lossy(&env.slopctl(&["run", "--account", "personal"]).stdout)
            .trim()
            .to_string();
    env.wait_for_session_starts(session_listener, &[&work_pane, &personal_pane]);

    assert!(
        settings_has_slopctl_hooks(&work_dir.join("settings.json")),
        "work should have hooks"
    );
    assert!(
        settings_has_slopctl_hooks(&personal_dir.join("settings.json")),
        "personal should have hooks"
    );

    kill_slopd(slopd);

    assert!(
        !settings_has_slopctl_hooks(&work_dir.join("settings.json")),
        "shutdown should clean the work account's settings.json"
    );
    assert!(
        !settings_has_slopctl_hooks(&personal_dir.join("settings.json")),
        "shutdown should clean the personal account's settings.json"
    );
}

// #4 — SIGHUP reload picks up a newly-added account.
#[test]
fn sighup_reload_picks_up_new_account() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let accounts_root = tempfile::tempdir().unwrap();
    let work_dir = accounts_root.path().join("work");
    let personal_dir = accounts_root.path().join("personal");

    // Start with only "work" configured.
    let Some(env) = TestEnv::new_with_accounts(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        &[("work", work_dir.as_path())],
        None,
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Before reload, "personal" is unknown.
    let before = env.slopctl_raw(&["run", "--no-wait", "--account", "personal"]);
    assert!(
        !before.status.success(),
        "personal should be unknown before reload"
    );

    // Add "personal" to the config, then SIGHUP and wait for the reload to land.
    env.tmux.write_slopd_config_accounts(
        &env.config_dir,
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        &[
            ("work", work_dir.as_path()),
            ("personal", personal_dir.as_path()),
        ],
        None,
    );
    sighup_pid(slopd.id());
    wait_for_config_generation_at_least(&env, 1, Duration::from_secs(5));

    // After reload, "personal" resolves.
    let after = env.slopctl(&["run", "--account", "personal"]);
    assert!(
        after.status.success(),
        "personal should resolve after reload: {:?}",
        after
    );
    let personal_pane = String::from_utf8_lossy(&after.stdout).trim().to_string();
    let account = pane_account(&env, &personal_pane);

    kill_slopd(slopd);

    assert_eq!(
        account, "personal",
        "the reloaded account should be in effect"
    );
}

// #5 — a default_account pointing at an unconfigured account fails the run clearly.
#[test]
fn run_fails_when_default_account_is_unknown() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let accounts_root = tempfile::tempdir().unwrap();
    let work_dir = accounts_root.path().join("work");

    // default_account names "ghost", which is not configured under [accounts].
    let Some(env) = TestEnv::new_with_accounts(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        &[("work", work_dir.as_path())],
        Some("ghost"),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // A plain run resolves to default_account ("ghost") and must fail.
    let run_out = env.slopctl_raw(&["run", "--no-wait"]);

    kill_slopd(slopd);

    assert!(
        !run_out.status.success(),
        "run should fail when default_account is unknown: {:?}",
        run_out
    );
    let stderr = String::from_utf8_lossy(&run_out.stderr);
    assert!(
        stderr.contains("unknown account"),
        "stderr should explain the failure: {}",
        stderr
    );
    assert!(
        stderr.contains("ghost"),
        "stderr should name the missing default account: {}",
        stderr
    );
}

// #6 — `slopd uninject-hooks` cleans every configured account dir.
#[test]
fn uninject_hooks_cleans_all_account_dirs() {
    build_bin("slopd");

    let accounts_root = tempfile::tempdir().unwrap();
    let work_dir = accounts_root.path().join("work");
    let personal_dir = accounts_root.path().join("personal");

    // Inject hooks directly into both account dirs (not via slopd, to avoid
    // auto-cleanup on daemon exit).
    libslop::inject_hooks_into_file(&work_dir.join("settings.json"), "slopctl").unwrap();
    libslop::inject_hooks_into_file(&personal_dir.join("settings.json"), "slopctl").unwrap();
    assert!(settings_has_slopctl_hooks(&work_dir.join("settings.json")));
    assert!(settings_has_slopctl_hooks(
        &personal_dir.join("settings.json")
    ));

    // A config with both accounts (no tmux needed for uninject-hooks).
    let config_dir = tempfile::tempdir().unwrap();
    let slopd_config_dir = config_dir.path().join("slopd");
    std::fs::create_dir_all(&slopd_config_dir).unwrap();
    std::fs::write(
        slopd_config_dir.join("config.toml"),
        format!(
            "[accounts]\nwork = {:?}\npersonal = {:?}\n",
            work_dir.to_str().unwrap(),
            personal_dir.to_str().unwrap(),
        ),
    )
    .unwrap();

    let out = Command::new(cargo_bin("slopd"))
        .args(["uninject-hooks"])
        .env("XDG_CONFIG_HOME", config_dir.path())
        .env("HOME", config_dir.path())
        .output()
        .expect("failed to run slopd uninject-hooks");
    assert!(
        out.status.success(),
        "slopd uninject-hooks failed: {:?}",
        out
    );

    assert!(
        !settings_has_slopctl_hooks(&work_dir.join("settings.json")),
        "uninject-hooks should clean the work account dir"
    );
    assert!(
        !settings_has_slopctl_hooks(&personal_dir.join("settings.json")),
        "uninject-hooks should clean the personal account dir"
    );
}

// ~ in the top-level claude_config_dir must reach the pane expanded (the README
// shows `claude_config_dir = "~/.claude"`, so a literal ~ would be a bug).
#[test]
fn run_expands_tilde_in_top_level_claude_config_dir() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    // spawn_slopd sets HOME to the test's config_dir, so ~ expands to there.
    let claude_config_dir = std::path::PathBuf::from("~/claude-home");
    let Some(env) = TestEnv::new_full(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_out = env.slopctl(&["run"]);
    assert!(run_out.status.success(), "run failed: {:?}", run_out);
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    enable_always_submit(&env, &pane_id);
    let config_dir_line = query_pane_env(&env, &pane_id, "CLAUDE_CONFIG_DIR");

    kill_slopd(slopd);

    let expected = env.config_dir.path().join("claude-home");
    assert_eq!(
        config_dir_line,
        format!(
            "::mock env CLAUDE_CONFIG_DIR={}",
            expected.to_str().unwrap()
        ),
        "~ in claude_config_dir should be expanded before reaching the pane",
    );
}

// ---------------------------------------------------------------------------
// run --interactive
// ---------------------------------------------------------------------------

/// Write a slopctl config under `config_dir/slopctl/config.toml` selecting an
/// interactive command + type. `sh_cmd` is the `sh -c` script (use `{}` for the
/// pane id). Returns nothing; point XDG_CONFIG_HOME at config_dir to load it.
fn write_slopctl_interactive_config(config_dir: &std::path::Path, sh_cmd: &str, run_type: &str) {
    let dir = config_dir.join("slopctl");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "[run]\ninteractive_command = [\"sh\", \"-c\", {:?}]\ninteractive_type = {:?}\n",
            sh_cmd, run_type,
        ),
    )
    .unwrap();
}

#[test]
fn run_interactive_exec_runs_viewer_with_substituted_pane_id() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(Some(&[&mock_claude_path]), Some(&slopctl_path), None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    // Benign exec viewer: record the substituted pane id to a marker file
    // (instead of `tmux attach`, which would block).
    let marker_dir = tempfile::tempdir().unwrap();
    let marker = marker_dir.path().join("exec-marker");
    write_slopctl_interactive_config(
        env.config_dir.path(),
        &format!("echo {} > {}", "{{pane_id}}", marker.display()),
        "exec",
    );

    let slopd = env.spawn_slopd();

    let xdg_config = env.config_dir.path().to_str().unwrap();
    let run_out = env.slopctl_raw_envs(
        &["run", "--interactive"],
        &[("XDG_CONFIG_HOME", xdg_config)],
    );

    // exec replaced slopctl with the viewer (`sh`), which exits 0.
    assert!(
        run_out.status.success(),
        "interactive exec run failed: {:?}",
        run_out
    );

    let recorded = std::fs::read_to_string(&marker)
        .expect("viewer should have written the marker file")
        .trim()
        .to_string();
    assert!(
        recorded.starts_with('%'),
        "marker should hold a tmux pane id, got {:?}",
        recorded
    );

    // The recorded pane id must be a real, created pane.
    let ps_out = env.slopctl(&["ps", "--json"]);
    let panes: Vec<libslop::PaneInfo> = serde_json::from_slice(&ps_out.stdout).expect("ps --json");
    kill_slopd(slopd);

    assert!(
        panes.iter().any(|p| p.pane_id == recorded),
        "the substituted pane id {:?} should be a live pane: {:?}",
        recorded,
        panes
    );
}

#[test]
fn run_interactive_forking_spawns_viewer_and_prints_pane_id() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(Some(&[&mock_claude_path]), Some(&slopctl_path), None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let marker_dir = tempfile::tempdir().unwrap();
    let marker = marker_dir.path().join("fork-marker");
    write_slopctl_interactive_config(
        env.config_dir.path(),
        &format!("echo {} > {}", "{{pane_id}}", marker.display()),
        "forking",
    );

    let slopd = env.spawn_slopd();

    let xdg_config = env.config_dir.path().to_str().unwrap();
    let run_out = env.slopctl_raw_envs(
        &["run", "--interactive"],
        &[("XDG_CONFIG_HOME", xdg_config)],
    );

    // Forking mode: slopctl spawns the viewer in the background and returns,
    // printing the pane id (like --no-wait).
    assert!(
        run_out.status.success(),
        "interactive forking run failed: {:?}",
        run_out
    );
    let printed = String::from_utf8_lossy(&run_out.stdout).trim().to_string();
    assert!(
        printed.starts_with('%'),
        "slopctl should print the pane id, got {:?}",
        printed
    );

    // The detached viewer writes the same (substituted) pane id; poll for it.
    let deadline = Instant::now() + Duration::from_secs(5);
    let recorded = loop {
        if let Ok(s) = std::fs::read_to_string(&marker) {
            let s = s.trim().to_string();
            if !s.is_empty() {
                break s;
            }
        }
        assert!(
            Instant::now() < deadline,
            "background viewer never wrote the marker"
        );
        std::thread::sleep(Duration::from_millis(50));
    };

    kill_slopd(slopd);

    assert_eq!(
        recorded, printed,
        "the backgrounded viewer should see the same pane id slopctl printed"
    );
}

// slopd honors a configured [tmux] session name: panes land in it, and the
// default "slopd" session is never created.
#[test]
fn slopd_uses_configured_tmux_session_name() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_with_tmux_session(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        "custom-slopd",
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_out = env.slopctl(&["run"]);
    assert!(run_out.status.success(), "run failed: {:?}", run_out);
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    // The configured session exists; the default "slopd" session does not.
    let has_custom = env
        .tmux
        .tmux()
        .args(["has-session", "-t", "custom-slopd"])
        .status()
        .unwrap();
    let has_default = env
        .tmux
        .tmux()
        .args(["has-session", "-t", "slopd"])
        .status()
        .unwrap();

    // The pane is tracked normally (ps works regardless of session name).
    let ps_out = env.slopctl(&["ps", "--json"]);
    let panes: Vec<libslop::PaneInfo> = serde_json::from_slice(&ps_out.stdout).expect("ps --json");

    kill_slopd(slopd);

    assert!(
        has_custom.success(),
        "slopd should create the configured 'custom-slopd' session"
    );
    assert!(
        !has_default.success(),
        "slopd should not create the default 'slopd' session when a custom one is configured"
    );
    assert!(
        panes.iter().any(|p| p.pane_id == pane_id),
        "the pane should be tracked: {:?}",
        panes
    );
}

// `--config <path>` makes slopd read its config from an arbitrary location
// instead of `$XDG_CONFIG_HOME/slopd/config.toml`, and that override wins over a
// config present at the default location. This is what lets a second slopd
// instance run from its own config file (with its own tmux socket/session)
// without touching the primary one.
#[test]
fn config_flag_overrides_config_file_location() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    // The harness writes a default-location config naming session "default-loc".
    // slopd must ignore it once we point --config at a different file.
    let Some(env) = TestEnv::new_with_tmux_session(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        "default-loc",
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    // An alternate config at a non-default path, with a distinct session name
    // and control socket (so the override is observable) but the same test tmux
    // socket (so the instance is actually functional).
    let alt_config = env.config_dir.path().join("alt-config.toml");
    let alt_socket = env.config_dir.path().join("from-flag.sock");
    std::fs::write(
        &alt_config,
        format!(
            "[control]\nsocket = {:?}\n\n[tmux]\nsocket = {:?}\nsession = \"from-flag\"\n\n[run]\nexecutable = [{:?}]\nslopctl = {:?}\n",
            alt_socket.to_str().unwrap(),
            env.tmux.socket.to_str().unwrap(),
            mock_claude_path,
            slopctl_path,
        ),
    )
    .unwrap();

    let slopd = env.spawn_slopd_at_socket(&["--config", alt_config.to_str().unwrap()], &alt_socket);

    let run_out = env.slopctl_raw(&["--config", alt_config.to_str().unwrap(), "run", "--no-wait"]);
    assert!(run_out.status.success(), "run failed: {:?}", run_out);
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    // The session from the --config file exists; the default-location one does not.
    let has_flag = env
        .tmux
        .tmux()
        .args(["has-session", "-t", "from-flag"])
        .status()
        .unwrap();
    let has_default = env
        .tmux
        .tmux()
        .args(["has-session", "-t", "default-loc"])
        .status()
        .unwrap();

    let ps_out = env.slopctl_raw(&["--config", alt_config.to_str().unwrap(), "ps", "--json"]);
    let panes: Vec<libslop::PaneInfo> = serde_json::from_slice(&ps_out.stdout).expect("ps --json");

    kill_slopd(slopd);

    assert!(
        has_flag.success(),
        "slopd should use the session from the --config file ('from-flag')"
    );
    assert!(
        !has_default.success(),
        "slopd should ignore the default-location config when --config is given"
    );
    assert!(
        panes.iter().any(|p| p.pane_id == pane_id),
        "the pane should be tracked: {:?}",
        panes
    );
}

// `[control].socket` selects the local instance for both slopd and slopctl.
// slopd also bakes the effective path into injected hooks so agent lifecycle
// events return to that same non-default instance.
#[test]
fn configured_control_socket_is_used_by_daemon_client_and_hooks() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(Some(&[&mock_claude_path]), Some(&slopctl_path), None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let configured_socket = env.config_dir.path().join("configured-slopd.sock");
    env.append_config(&format!(
        "[control]\nsocket = {:?}",
        configured_socket.to_str().unwrap()
    ));
    let slopd = env.spawn_slopd_at_socket(&[], &configured_socket);

    // No --socket is needed: slopctl reads the same selected slopd config.
    let run_out = env.slopctl_raw(&["run", "--no-wait"]);
    assert!(
        run_out.status.success(),
        "run via configured socket failed: {:?}",
        run_out
    );
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();
    let ps_out = env.slopctl_raw(&["ps", "--json"]);
    let panes: Vec<libslop::PaneInfo> = serde_json::from_slice(&ps_out.stdout).expect("ps --json");

    let settings_path = env.config_dir.path().join(".claude/settings.json");
    let settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&settings_path).expect("settings.json should exist"),
    )
    .unwrap();
    let stop_cmd = settings["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap_or("")
        .to_string();

    kill_slopd(slopd);

    assert!(
        !env.socket_path().exists(),
        "slopd must not also bind the default socket"
    );
    assert!(
        panes.iter().any(|p| p.pane_id == pane_id),
        "pane should be tracked: {:?}",
        panes
    );
    assert!(
        stop_cmd.contains(&format!("--socket {}", configured_socket.display())),
        "injected hook should carry configured socket; got {:?}",
        stop_cmd
    );
}

#[test]
fn configured_control_socket_change_on_sighup_requires_restart() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(Some(&[&mock_claude_path]), Some(&slopctl_path), None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let startup_socket = env.config_dir.path().join("startup-slopd.sock");
    let reloaded_socket = env.config_dir.path().join("reloaded-slopd.sock");
    env.append_config(&format!(
        "[control]\nsocket = {:?}",
        startup_socket.to_str().unwrap()
    ));
    let slopd = env.spawn_slopd_at_socket(&[], &startup_socket);

    let config_path = env.config_path();
    let config = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(
        &config_path,
        config.replace(
            startup_socket.to_str().unwrap(),
            reloaded_socket.to_str().unwrap(),
        ),
    )
    .unwrap();
    sighup_pid(slopd.id());

    // Poll the startup socket explicitly because the on-disk config now names
    // the path that will take effect after restart.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = env.slopctl_raw(&["--socket", startup_socket.to_str().unwrap(), "status"]);
        let generation = String::from_utf8_lossy(&status.stdout)
            .lines()
            .find_map(|line| {
                line.strip_prefix("config_generation: ")
                    .and_then(|value| value.parse::<u64>().ok())
            })
            .unwrap_or(0);
        if status.status.success() && generation >= 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for SIGHUP reload on startup socket"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let run_out = env.slopctl_raw(&[
        "--socket",
        startup_socket.to_str().unwrap(),
        "run",
        "--no-wait",
    ]);
    assert!(
        run_out.status.success(),
        "daemon should remain reachable at startup socket: {:?}",
        run_out
    );

    let settings_path = env.config_dir.path().join(".claude/settings.json");
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(settings_path).unwrap()).unwrap();
    let stop_cmd = settings["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap_or("");

    kill_slopd(slopd);

    assert!(
        !reloaded_socket.exists(),
        "SIGHUP must not bind the changed control socket"
    );
    assert!(
        stop_cmd.contains(&format!("--socket {}", startup_socket.display())),
        "post-reload hooks must keep the bound startup socket; got {stop_cmd:?}"
    );
    assert!(
        !stop_cmd.contains(&reloaded_socket.display().to_string()),
        "post-reload hooks must not point at the unbound config socket; got {stop_cmd:?}"
    );
}

// `--socket <path>` takes precedence over `[control].socket` and is baked into
// injected hooks, so an explicit one-off target cannot be redirected by config.
#[test]
fn socket_flag_overrides_control_socket_and_is_carried_into_hooks() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(Some(&[&mock_claude_path]), Some(&slopctl_path), None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let configured_socket = env.config_dir.path().join("from-config.sock");
    env.append_config(&format!(
        "[control]\nsocket = {:?}",
        configured_socket.to_str().unwrap()
    ));

    // A different control socket explicitly selected on the command line.
    let custom_socket = env.config_dir.path().join("custom-slopd.sock");
    let slopd = env.spawn_slopd_with_socket(&custom_socket);
    let custom = custom_socket.to_str().unwrap();

    // slopd did not bind either lower-precedence location. slopctl without
    // --socket follows config and therefore cannot reach this CLI-selected
    // instance.
    let default_exists = env.socket_path().exists();
    let configured_exists = configured_socket.exists();
    let ps_configured = env.slopctl_raw(&["ps", "--json"]);

    // With --socket, ps works and run spawns a tracked pane.
    let run_out = env.slopctl_raw(&["--socket", custom, "run", "--no-wait"]);
    assert!(
        run_out.status.success(),
        "run --socket failed: {:?}",
        run_out
    );
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    let ps_out = env.slopctl_raw(&["--socket", custom, "ps", "--json"]);
    let panes: Vec<libslop::PaneInfo> = serde_json::from_slice(&ps_out.stdout).expect("ps --json");

    // The hooks injected for the spawned pane carry `--socket <custom>` so the
    // pane's `slopctl hook` calls report back to this instance.
    let settings_path = env.config_dir.path().join(".claude/settings.json");
    let settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&settings_path).expect("settings.json should exist"),
    )
    .unwrap();
    let stop_cmd = settings["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap_or("")
        .to_string();

    kill_slopd(slopd);

    assert!(
        !default_exists,
        "slopd must not bind the default socket when --socket is given"
    );
    assert!(
        !configured_exists,
        "CLI --socket must override the configured socket"
    );
    assert!(
        !ps_configured.status.success(),
        "slopctl following config must not reach the CLI-selected instance"
    );
    assert!(
        panes.iter().any(|p| p.pane_id == pane_id),
        "pane should be tracked: {:?}",
        panes
    );
    assert!(
        stop_cmd.contains(&format!("--socket {}", custom)),
        "injected hook command should carry --socket; got {:?}",
        stop_cmd
    );
    assert!(
        stop_cmd.ends_with(" hook Stop"),
        "injected hook command malformed: {:?}",
        stop_cmd
    );
}

// `slopctl run` creates the pane's window in the background (`new-window -d`),
// so it doesn't steal focus from clients already watching the session.
#[test]
fn run_creates_background_window_without_stealing_focus() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(Some(&[&mock_claude_path]), Some(&slopctl_path), None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_out = env.slopctl(&["run"]);
    assert!(run_out.status.success(), "run failed: {:?}", run_out);
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    let window_of = |target: &str| -> String {
        let out = env
            .tmux
            .tmux()
            .args(["display-message", "-t", target, "-p", "#{window_index}"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let pane_window = window_of(&pane_id);
    let session_current = window_of("slopd");

    kill_slopd(slopd);

    assert!(
        !pane_window.is_empty(),
        "should resolve the new pane's window"
    );
    assert_ne!(
        session_current, pane_window,
        "a backgrounded run should not make the new pane's window the session's current window",
    );
}

// The grouped-session mechanism the default `--interactive` command relies on:
// focusing a pane in a grouped view doesn't move the shared session, and a
// `destroy-unattached` view disposes itself without killing the shared panes.
#[test]
fn grouped_interactive_view_is_isolated_and_self_cleaning() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(Some(&[&mock_claude_path]), Some(&slopctl_path), None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    let run_out = env.slopctl(&["run"]);
    assert!(run_out.status.success(), "run failed: {:?}", run_out);
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();

    let tmux_status = |args: &[&str]| env.tmux.tmux().args(args).status().unwrap();
    let window_index = |target: &str| -> String {
        let o = env
            .tmux
            .tmux()
            .args(["display-message", "-t", target, "-p", "#{window_index}"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    };

    let pane_window = window_index(&pane_id);
    let slopd_current_before = window_index("slopd");

    // Build a grouped view of the slopd session (what `--interactive` attaches to).
    assert!(tmux_status(&["new-session", "-d", "-s", "view", "-t", "slopd"]).success());

    // Isolation: focusing the pane's window in the VIEW must not move the shared
    // session's current window (so other clients aren't disturbed).
    assert!(tmux_status(&["select-window", "-t", &format!("view:{}", pane_window)]).success());
    let view_current = window_index("view");
    let slopd_current_after = window_index("slopd");

    // Self-cleaning: an unattached view with destroy-unattached disposes itself…
    assert!(tmux_status(&["set-option", "-t", "view", "destroy-unattached", "on"]).success());
    std::thread::sleep(Duration::from_millis(300));
    let view_gone = !env
        .tmux
        .tmux()
        .args(["has-session", "-t", "view"])
        .status()
        .unwrap()
        .success();

    // …without taking the shared Claude pane down with it.
    let ps_out = env.slopctl(&["ps", "--json"]);
    let panes: Vec<libslop::PaneInfo> = serde_json::from_slice(&ps_out.stdout).expect("ps --json");

    kill_slopd(slopd);

    assert_eq!(
        view_current, pane_window,
        "the grouped view should focus the new pane's window"
    );
    assert_eq!(
        slopd_current_after, slopd_current_before,
        "focusing the pane in the grouped view must not move the shared session's current window"
    );
    assert!(
        view_gone,
        "an unattached view with destroy-unattached should self-destruct"
    );
    assert!(
        panes.iter().any(|p| p.pane_id == pane_id),
        "destroying the throwaway view must not kill the shared Claude pane: {:?}",
        panes
    );
}

// --- backup / restore across reboot ---------------------------------------
//
// slopd persists the managed-pane set as a lifecycle-journal checkpoint and,
// on a fresh start into a brand-new tmux session (the post-reboot signal),
// re-spawns each recorded pane with `claude --resume <session_id>`. This drives
// the full round trip: run a pane, snapshot it on clean shutdown, wipe the tmux
// session to simulate a reboot, restart slopd, and assert the pane comes back
// with its session id and tags intact under a fresh tmux pane id.
#[test]
fn backup_restore_round_trip_across_reboot() {
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
    // Restore is opt-in; enable it for this test.
    env.append_config("[backup]\nauto_restore = true");

    // --- first boot: run a pane, tag it, snapshot on clean shutdown ---
    let slopd1 = env.spawn_slopd();
    let listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    let session_id = env.wait_for_session_start(listener, &pane_id);
    assert_eq!(session_id, "mock-session-id-1234");

    let tag_output = env.slopctl(&["tag", &pane_id, "mytag"]);
    assert!(
        tag_output.status.success(),
        "slopctl tag failed: {:?}",
        tag_output
    );

    // SIGINT triggers a clean shutdown, which writes the final snapshot.
    sigint_child(slopd1);

    // The latest generation checkpoint should record our pane.
    let checkpoint = latest_lifecycle_checkpoint(&env);
    let entry = checkpoint
        .iter()
        .find(|p| p.session_id.as_deref() == Some("mock-session-id-1234"))
        .expect("snapshot should contain the running pane");
    assert!(
        entry.tags.contains(&"mytag".to_string()),
        "snapshot should preserve tags; got {:?}",
        entry.tags
    );
    assert_eq!(entry.account, "default");

    // --- simulate a reboot: destroy the slopd tmux session (and its panes).
    // The harness tmux server keeps running via its own "test" session, so the
    // next slopd start sees no "slopd" session and treats it as a fresh boot.
    let kill = env
        .tmux
        .tmux()
        .args(["kill-session", "-t", "slopd"])
        .status()
        .unwrap();
    assert!(kill.success(), "failed to kill slopd tmux session");

    // --- second boot: slopd restores the pane from the checkpoint ---
    let slopd2 = env.spawn_slopd();

    // Restore runs before the socket binds, so the pane should already be
    // present; poll briefly to be robust against scheduling.
    let deadline = Instant::now() + Duration::from_secs(15);
    let restored = loop {
        let out = env.slopctl(&["ps", "--json"]);
        let panes: Vec<libslop::PaneInfo> = serde_json::from_slice(&out.stdout).unwrap_or_default();
        if let Some(p) = panes
            .into_iter()
            .find(|p| p.session_id.as_deref() == Some("mock-session-id-1234"))
        {
            break p;
        }
        if Instant::now() > deadline {
            kill_slopd(slopd2);
            panic!("restored pane did not appear within timeout");
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    assert!(
        restored.tags.contains(&"mytag".to_string()),
        "restored pane should keep its tags; got {:?}",
        restored.tags
    );
    assert_eq!(restored.account, "default");
    // The restored pane gets a fresh tmux id (the old one died with the session).
    assert_ne!(
        restored.pane_id, pane_id,
        "restored pane should have a new tmux id"
    );

    kill_slopd(slopd2);
}

// A daemon restart against a *surviving* tmux session must NOT restore from the
// manifest — load_managed_panes already recovers the live panes, so restoring
// would duplicate them. This guards the session-existed gate.
#[test]
fn restart_with_surviving_session_does_not_duplicate_panes() {
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
    // Restore enabled — the point of this test is that the session-existed gate
    // still prevents duplication even with restore turned on.
    env.append_config("[backup]\nauto_restore = true");

    let slopd1 = env.spawn_slopd();
    let listener = env.spawn_session_start_listener();
    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();
    env.wait_for_session_start(listener, &pane_id);

    // Clean shutdown writes a manifest, but the tmux session (and its pane)
    // survive — this is a daemon restart, not a reboot.
    sigint_child(slopd1);

    let slopd2 = env.spawn_slopd();

    // The single original pane should be recovered from tmux, not duplicated by
    // a manifest restore. Give any erroneous restore time to spawn a second pane.
    std::thread::sleep(Duration::from_millis(500));
    let out = env.slopctl(&["ps", "--json"]);
    let panes: Vec<libslop::PaneInfo> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let claude_panes: Vec<_> = panes
        .iter()
        .filter(|p| p.session_id.as_deref() == Some("mock-session-id-1234"))
        .collect();
    assert_eq!(
        claude_panes.len(),
        1,
        "a surviving-session restart must not duplicate panes; got {:?}",
        panes
    );
    assert_eq!(
        claude_panes[0].pane_id, pane_id,
        "the recovered pane should keep its original tmux id"
    );

    kill_slopd(slopd2);
}

// mock_claude always reports this fixed session id, so any number of mock panes
// share one session — handy for exercising dedup.
const MOCK_SID: &str = "mock-session-id-1234";

/// Build the bins and a TestEnv wired to mock_claude, with `backup_toml` appended
/// to the slopd config. Returns the env plus the home tempdir it borrows (kept
/// alive by the caller). `None` if tmux is unavailable.
fn backup_env(backup_toml: &str) -> Option<(TestEnv, tempfile::TempDir)> {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");
    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    let mock_claude_path = cargo_bin("mock_claude").to_str().unwrap().to_string();
    let env = TestEnv::new_full(
        Some(&[&mock_claude_path]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    )?;
    env.append_config(backup_toml);
    Some((env, home_dir))
}

/// Journal files for this test's tmux target. The production layout includes
/// hex-encoded socket/session directories; walking below `tmux-targets` keeps
/// tests independent of that encoding detail.
fn lifecycle_journal_files(env: &TestEnv) -> Vec<std::path::PathBuf> {
    fn visit(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, files);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                files.push(path);
            }
        }
    }

    let mut files = Vec::new();
    visit(
        &env.config_dir
            .path()
            .join(".local/state/slopd/tmux-targets"),
        &mut files,
    );
    files
}

fn journal_generation_order(path: &std::path::Path) -> (u64, i64) {
    let file = std::fs::File::open(path).expect("open lifecycle journal");
    let header = std::io::BufReader::new(file)
        .lines()
        .find_map(|line| serde_json::from_str::<serde_json::Value>(&line.ok()?).ok())
        .expect("lifecycle generation header");
    let started = header["started_at"].as_u64().unwrap_or_default();
    let session = header["tmux_session_id"]
        .as_str()
        .and_then(|id| id.strip_prefix('$'))
        .and_then(|id| id.parse().ok())
        .unwrap_or(-1);
    (started, session)
}

fn latest_lifecycle_journal(env: &TestEnv) -> std::path::PathBuf {
    lifecycle_journal_files(env)
        .into_iter()
        .max_by_key(|path| journal_generation_order(path))
        .expect("lifecycle journal was not written")
}

fn checkpoint_from_journal(path: &std::path::Path) -> Vec<libslop::PaneInfo> {
    let file = std::fs::File::open(path).expect("open lifecycle journal");
    let mut panes = std::collections::HashMap::<String, libslop::PaneInfo>::new();
    let mut checkpoint = Vec::new();
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match value["event"].as_str() {
            Some("pane") => {
                let pane: libslop::PaneInfo =
                    serde_json::from_value(value["pane"].clone()).expect("valid pane event");
                panes.insert(pane.pane_id.clone(), pane);
            }
            Some("checkpoint") => {
                checkpoint = value["pane_ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|id| id.as_str())
                    .filter_map(|id| panes.get(id).cloned())
                    .collect();
            }
            _ => {}
        }
    }
    checkpoint
}

fn latest_lifecycle_checkpoint(env: &TestEnv) -> Vec<libslop::PaneInfo> {
    checkpoint_from_journal(&latest_lifecycle_journal(env))
}

/// Replace the latest checkpoint by appending pane versions plus a checkpoint,
/// matching the journal's public on-disk format. Used only to plant malformed or
/// historical recovery states that cannot be produced through the CLI.
fn append_lifecycle_checkpoint(env: &TestEnv, panes: &[libslop::PaneInfo]) {
    use std::io::Write;
    let path = latest_lifecycle_journal(env);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("append lifecycle journal");
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    for pane in panes {
        writeln!(
            file,
            "{}",
            serde_json::json!({"event": "pane", "at": at, "pane": pane})
        )
        .unwrap();
    }
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "event": "checkpoint",
            "at": at,
            "pane_ids": panes.iter().map(|pane| &pane.pane_id).collect::<Vec<_>>(),
        })
    )
    .unwrap();
}

/// Count managed panes whose session id is `sid`.
fn count_panes_with_session(env: &TestEnv, sid: &str) -> usize {
    let out = env.slopctl(&["ps", "--json"]);
    let panes: Vec<libslop::PaneInfo> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    panes
        .iter()
        .filter(|p| p.session_id.as_deref() == Some(sid))
        .count()
}

/// Simulate a reboot: destroy the slopd tmux session (and its panes). The
/// harness tmux server stays up via its own "test" session, so the next slopd
/// start sees a fresh session.
fn reboot_tmux(env: &TestEnv) {
    let ok = env
        .tmux
        .tmux()
        .args(["kill-session", "-t", "slopd"])
        .status()
        .unwrap();
    assert!(ok.success(), "failed to kill slopd tmux session");
}

/// Run a pane and wait for its SessionStart so the session id is recorded.
fn run_and_wait(env: &TestEnv) -> String {
    let listener = env.spawn_session_start_listener();
    let out = env.slopctl(&["run"]);
    assert!(out.status.success(), "slopctl run failed: {:?}", out);
    let pane_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    env.wait_for_session_start(listener, &pane_id);
    pane_id
}

// `slopctl backup` / `slopctl restore` work on demand even with both automatic
// behaviours off: a manual backup persists the manifest, and after a reboot a
// manual restore brings the pane back.
#[test]
fn manual_backup_and_restore_commands() {
    let Some((env, _home)) = backup_env("[backup]\nauto_backup = false\nauto_restore = false")
    else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd1 = env.spawn_slopd();
    run_and_wait(&env);

    // Nothing is written automatically; a manual backup persists the pane.
    let out = env.slopctl(&["backup"]);
    assert!(out.status.success(), "slopctl backup failed: {:?}", out);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("backed up 1"),
        "expected 'backed up 1'; got {:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    sigint_child(slopd1);
    reboot_tmux(&env);

    // auto_restore is off, so the restart leaves the manifest alone...
    let slopd2 = env.spawn_slopd();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        count_panes_with_session(&env, MOCK_SID),
        0,
        "auto_restore=false must not restore on startup"
    );

    // ...until we ask for it manually.
    let out = env.slopctl(&["restore"]);
    assert!(out.status.success(), "slopctl restore failed: {:?}", out);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("restored 1"),
        "expected 'restored 1'; got {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        count_panes_with_session(&env, MOCK_SID),
        1,
        "manual restore should bring the pane back"
    );

    kill_slopd(slopd2);
}

#[test]
fn graveyard_records_and_revives_a_killed_pane() {
    let Some((env, _home)) = backup_env("[backup]\nauto_backup = false") else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    let pane_id = run_and_wait(&env);
    assert!(
        env.slopctl(&["tag", &pane_id, "recover-me"])
            .status
            .success()
    );
    assert!(env.slopctl(&["kill", &pane_id]).status.success());

    let listed = env.slopctl(&["graveyard", "--json"]);
    assert!(listed.status.success(), "graveyard failed: {listed:?}");
    let entries: Vec<libslop::GraveEntry> =
        serde_json::from_slice(&listed.stdout).expect("graveyard JSON");
    let grave = entries
        .iter()
        .find(|entry| entry.pane.pane_id == pane_id)
        .expect("killed pane should be in graveyard");
    assert_eq!(
        uuid::Uuid::parse_str(&grave.grave_id)
            .unwrap()
            .get_version(),
        Some(uuid::Version::SortRand),
        "grave IDs should be stock UUID v7 values"
    );
    assert_eq!(grave.cause, "deliberate_kill");
    assert!(grave.pane.tags.contains(&"recover-me".to_string()));
    assert!(grave.revived_at.is_none());
    let human = String::from_utf8(env.slopctl(&["graveyard"]).stdout).unwrap();
    let mut lines = human.lines();
    let header = lines.next().expect("graveyard header");
    let row = lines.next().expect("graveyard row");
    assert_eq!(header.find("GRAVE"), row.find(&grave.grave_id[..8]));
    assert_eq!(header.find("PANE"), row.find(&pane_id));
    assert_eq!(header.find("STATUS"), row.find("deliberate_kill"));
    assert!(!human.contains('\t'));
    assert!(human.contains("ago") || human.contains("now"));

    let prefix = &grave.grave_id[..grave.grave_id.len().min(8)];
    let revived = env.slopctl(&["revive", prefix]);
    assert!(revived.status.success(), "revive failed: {revived:?}");
    let revived_id = String::from_utf8_lossy(&revived.stdout).trim().to_string();
    assert_ne!(revived_id, pane_id);
    let panes: Vec<libslop::PaneInfo> =
        serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap();
    let pane = panes
        .iter()
        .find(|pane| pane.pane_id == revived_id)
        .expect("revived pane should be managed");
    assert_eq!(pane.session_id.as_deref(), Some(MOCK_SID));
    assert!(pane.tags.contains(&"recover-me".to_string()));

    let entries: Vec<libslop::GraveEntry> =
        serde_json::from_slice(&env.slopctl(&["graveyard", "--json"]).stdout).unwrap();
    let grave = entries
        .iter()
        .find(|entry| entry.grave_id.starts_with(prefix))
        .unwrap();
    assert_eq!(grave.revived_as.as_deref(), Some(revived_id.as_str()));
    assert!(grave.revived_at.is_some());

    kill_slopd(slopd);
}

#[test]
fn graveyard_boot_disambiguates_reused_tmux_pane_ids() {
    let Some((env, _home)) = backup_env("[backup]\nauto_backup = false") else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    let first_pane = run_and_wait(&env);
    assert!(env.slopctl(&["kill", &first_pane]).status.success());

    // Keep slopd itself alive while replacing the tmux server. The next `run`
    // must both recreate its managed session and switch the open journal to the
    // new server/session generation.
    assert!(
        env.tmux
            .tmux()
            .arg("kill-server")
            .status()
            .unwrap()
            .success()
    );
    // `kill-server` replies just before the server has completely released its
    // socket. Wait for that teardown before starting a new server at the same
    // path, otherwise tmux can report "server exited unexpectedly".
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        std::thread::sleep(Duration::from_millis(50));
        if env
            .tmux
            .tmux()
            .args(["new-session", "-d", "-s", "test"])
            .status()
            .unwrap()
            .success()
        {
            break;
        }
        assert!(Instant::now() < deadline, "failed to restart tmux server");
    }

    let second_pane = run_and_wait(&env);
    assert_eq!(
        first_pane, second_pane,
        "precondition: a fresh tmux server should reuse its pane-id sequence"
    );
    assert!(env.slopctl(&["kill", &second_pane]).status.success());

    let entries: Vec<libslop::GraveEntry> =
        serde_json::from_slice(&env.slopctl(&["graveyard", "--json"]).stdout).unwrap();
    let reused: Vec<_> = entries
        .iter()
        .filter(|entry| entry.pane.pane_id == first_pane)
        .collect();
    assert_eq!(reused.len(), 2, "both incarnations must be retained");
    assert_ne!(reused[0].tmux_boot_id, reused[1].tmux_boot_id);

    let ambiguous = env.slopctl(&["revive", &first_pane]);
    assert!(
        !ambiguous.status.success(),
        "a reused pane id without --boot must be rejected"
    );
    let current: Vec<libslop::GraveEntry> =
        serde_json::from_slice(&env.slopctl(&["graveyard", "--boot", "0", "--json"]).stdout)
            .unwrap();
    let previous: Vec<libslop::GraveEntry> =
        serde_json::from_slice(&env.slopctl(&["graveyard", "--boot", "-1", "--json"]).stdout)
            .unwrap();
    assert_eq!(current.len(), 1);
    assert_eq!(previous.len(), 1);
    assert_ne!(current[0].tmux_boot_id, previous[0].tmux_boot_id);

    kill_slopd(slopd);
}

// A manual restore against a live daemon must not double a session that is
// already running — the dedup set is seeded with the running panes' session ids.
#[test]
fn manual_restore_skips_already_running_session() {
    let Some((env, _home)) = backup_env("[backup]\nauto_backup = false") else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    run_and_wait(&env);

    // Capture the running pane into the manifest, then restore with it still alive.
    let out = env.slopctl(&["backup"]);
    assert!(out.status.success(), "slopctl backup failed: {:?}", out);

    let out = env.slopctl(&["restore"]);
    assert!(out.status.success(), "slopctl restore failed: {:?}", out);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("restored 0"),
        "restore must skip the already-running session; got {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        count_panes_with_session(&env, MOCK_SID),
        1,
        "the already-running session must not be duplicated"
    );

    kill_slopd(slopd);
}

// Repeated reboots must not accumulate duplicates: a session restored once stays
// a single pane across a second reboot+restore.
#[test]
fn repeated_restore_does_not_duplicate() {
    let Some((env, _home)) = backup_env("[backup]\nauto_restore = true") else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd1 = env.spawn_slopd();
    run_and_wait(&env);
    sigint_child(slopd1);

    // Reboot #1 → restore.
    reboot_tmux(&env);
    let slopd2 = env.spawn_slopd();
    let deadline = Instant::now() + Duration::from_secs(15);
    while count_panes_with_session(&env, MOCK_SID) == 0 {
        if Instant::now() > deadline {
            kill_slopd(slopd2);
            panic!("first restore did not bring the pane back");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    sigint_child(slopd2);

    // Reboot #2 → restore again. The already-restored session must not double.
    reboot_tmux(&env);
    let slopd3 = env.spawn_slopd();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        count_panes_with_session(&env, MOCK_SID),
        1,
        "a repeated reboot+restore must not duplicate an already-restored session"
    );

    kill_slopd(slopd3);
}

// A manifest with two entries that share a session id (possible when
// @slopd_claude_session_id is overwritten by an in-pane resume) must restore to
// a single pane, not two Claudes on one transcript. Two mock panes share the
// fixed mock session id, so a backup of both produces exactly such a manifest.
#[test]
fn restore_dedups_duplicate_sessions_in_manifest() {
    let Some((env, _home)) = backup_env("[backup]\nauto_restore = true") else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd1 = env.spawn_slopd();
    let listener = env.spawn_session_start_listener();
    let p1 = String::from_utf8_lossy(&env.slopctl(&["run"]).stdout)
        .trim()
        .to_string();
    let p2 = String::from_utf8_lossy(&env.slopctl(&["run"]).stdout)
        .trim()
        .to_string();
    env.wait_for_session_starts(listener, &[&p1, &p2]);

    // Both panes report the same mock session id, so the shutdown backup writes a
    // manifest with two entries sharing it.
    sigint_child(slopd1);
    let checkpoint = latest_lifecycle_checkpoint(&env);
    let dup = checkpoint
        .iter()
        .filter(|p| p.session_id.as_deref() == Some(MOCK_SID))
        .count();
    assert_eq!(
        dup, 2,
        "precondition: checkpoint should hold two entries with the shared session id; got {:?}",
        checkpoint
    );

    reboot_tmux(&env);
    let slopd2 = env.spawn_slopd();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        count_panes_with_session(&env, MOCK_SID),
        1,
        "duplicate session ids in the manifest must restore to a single pane"
    );

    kill_slopd(slopd2);
}

// Regression: slopd marks its tmux session with an @slopd_managed option (at the
// SESSION level) so the session can be identified. A `#{@slopd_managed}` format
// resolves user options hierarchically, so the session's idle shell pane — which
// has no pane-level value — inherits the session marker's "true". load_managed_panes
// must read the option pane-scoped (`show-options -p`, no inheritance) instead,
// or it adopts the idle shell as a phantom managed pane. A fresh session must
// therefore show no managed panes.
#[test]
fn idle_shell_not_adopted_on_fresh_start() {
    build_bin("slopd");
    build_bin("slopctl");
    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();
    // No `run`: the session's only pane is the idle shell created by
    // `tmux new-session`. It is not pane-level managed and must not be adopted.
    let out = env.slopctl(&["ps", "--json"]);
    assert!(out.status.success(), "slopctl ps failed: {:?}", out);
    let panes: Vec<libslop::PaneInfo> = serde_json::from_slice(&out.stdout).expect("valid ps json");
    assert!(
        panes.is_empty(),
        "a fresh slopd session must adopt no panes; the idle shell inherits the \
         session-level @slopd_managed marker but is not pane-managed. got: {:?}",
        panes
    );

    kill_slopd(slopd);
}

// With auto_restore off, a reboot must NOT silently lose the restore point.
// The older checkpoint becomes a "pending restore": auto-backup is suspended so it is
// preserved through post-reboot activity (the "reboot, start working, THEN
// remember to restore" case), instead of being clobbered by the new live set.
#[test]
fn pending_restore_preserves_manifest_across_activity() {
    let Some((env, _home)) = backup_env("[backup]\nauto_restore = false\ninterval_secs = 1") else {
        eprintln!("skipping: tmux not found");
        return;
    };

    // First boot: a pane, captured into the journal on clean shutdown.
    let slopd1 = env.spawn_slopd();
    run_and_wait(&env);
    sigint_child(slopd1);
    let source_path = latest_lifecycle_journal(&env);
    let before = std::fs::read(&source_path).expect("journal written on shutdown");
    assert!(
        !before.is_empty() && before != b"[]",
        "precondition: journal holds the pane"
    );

    // Reboot: fresh session, auto_restore off → pending, nothing restored.
    reboot_tmux(&env);
    let slopd2 = env.spawn_slopd();
    let status = String::from_utf8_lossy(&env.slopctl(&["status"]).stdout).into_owned();
    assert!(
        status.contains("pending_restore: 1 pane"),
        "status should surface the pending restore; got {:?}",
        status
    );
    assert_eq!(
        count_panes_with_session(&env, MOCK_SID),
        0,
        "nothing restored yet"
    );

    // The edge case: create a pane before restoring. auto-backup must stay
    // suspended so the restore point is preserved, not overwritten by the new
    // (smaller) live set. Wait past several periodic backup ticks (interval=1s).
    let out = env.slopctl(&["run"]);
    assert!(out.status.success(), "run failed: {:?}", out);
    std::thread::sleep(Duration::from_millis(2500));

    let after = std::fs::read(&source_path).expect("source journal still present");
    assert_eq!(
        before, after,
        "while a restore is pending, auto-backup must not overwrite the source checkpoint, \
         even after new panes are created post-reboot"
    );

    kill_slopd(slopd2);
}

// `slopctl restore` consumes the pending restore: it brings the panes back and
// clears the pending state (resuming normal auto-backup).
#[test]
fn pending_restore_resolved_by_slopctl_restore() {
    let Some((env, _home)) = backup_env("[backup]\nauto_restore = false") else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd1 = env.spawn_slopd();
    run_and_wait(&env);
    sigint_child(slopd1);

    reboot_tmux(&env);
    let slopd2 = env.spawn_slopd();
    assert!(
        String::from_utf8_lossy(&env.slopctl(&["status"]).stdout).contains("pending_restore: 1"),
        "should be pending after a fresh reboot with auto_restore off"
    );
    assert_eq!(count_panes_with_session(&env, MOCK_SID), 0);

    // Resolve it on demand.
    let out = env.slopctl(&["restore"]);
    assert!(out.status.success(), "restore failed: {:?}", out);

    let deadline = Instant::now() + Duration::from_secs(15);
    while count_panes_with_session(&env, MOCK_SID) == 0 {
        if Instant::now() > deadline {
            kill_slopd(slopd2);
            panic!("restore did not bring the pane back");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let status = String::from_utf8_lossy(&env.slopctl(&["status"]).stdout).into_owned();
    assert!(
        !status.contains("pending_restore"),
        "pending must be cleared once restored; got {:?}",
        status
    );

    kill_slopd(slopd2);
}

// The pending state must survive a *daemon* restart (not just a reboot), or a
// crash/restart in the pending window would resume auto-backup and clobber the
// preserved checkpoint. A journal resolution record persists it: the restarted
// daemon re-enters pending even though the tmux session survived.
#[test]
fn pending_restore_survives_daemon_restart() {
    let Some((env, _home)) = backup_env("[backup]\nauto_restore = false\ninterval_secs = 1") else {
        eprintln!("skipping: tmux not found");
        return;
    };

    // Boot 1: a pane captured into the journal.
    let slopd1 = env.spawn_slopd();
    run_and_wait(&env);
    sigint_child(slopd1);
    let source_path = latest_lifecycle_journal(&env);
    let before = std::fs::read(&source_path).expect("journal written");
    assert!(
        before != b"[]" && !before.is_empty(),
        "precondition: journal holds the pane"
    );

    // Reboot → pending.
    reboot_tmux(&env);
    let slopd2 = env.spawn_slopd();
    assert!(
        String::from_utf8_lossy(&env.slopctl(&["status"]).stdout).contains("pending_restore: 1"),
        "should be pending after reboot"
    );

    // Daemon restart (tmux session survives) before resolving: kill slopd, then
    // start a new one against the same surviving session.
    sigint_child(slopd2);
    let slopd3 = env.spawn_slopd();
    std::thread::sleep(Duration::from_millis(2500)); // past several backup ticks

    // The unresolved source generation makes the new daemon re-enter pending,
    // so it is preserved and status still shows it.
    let after = std::fs::read(&source_path).expect("source journal still present");
    assert_eq!(
        before, after,
        "a daemon restart during a pending restore must not clobber its source checkpoint"
    );
    assert!(
        String::from_utf8_lossy(&env.slopctl(&["status"]).stdout).contains("pending_restore: 1"),
        "pending must persist across a daemon restart"
    );

    kill_slopd(slopd3);
}

// The architectural guard against the post-reboot PATH failure. slopd launches
// every Claude pane through one chokepoint that resolves the executable to an
// absolute path. If it can't be resolved when a reboot would auto-restore (the
// real incident: systemd's minimal PATH omitted ~/.local/bin, so bare `claude`
// wasn't found and every restored pane died instantly, letting the empty live
// set clobber the manifest), slopd must NOT spawn doomed panes. It keeps the
// manifest as a pending restore so the restore point survives until the
// executable is back, then `slopctl restore` brings the panes up.
#[test]
fn missing_executable_preserves_manifest_instead_of_clobbering() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let home_dir = tempfile::tempdir().unwrap();
    let claude_config_dir = home_dir.path().join(".claude");
    let slopctl_path = cargo_bin("slopctl").to_str().unwrap().to_string();
    // A private copy of mock_claude we can delete to simulate the executable
    // becoming unresolvable on the second boot (the minimal-PATH / uninstall case).
    let shim = home_dir.path().join("claude-shim");
    std::fs::copy(cargo_bin("mock_claude"), &shim).unwrap();
    let shim_path = shim.to_str().unwrap().to_string();

    let Some(env) = TestEnv::new_full(
        Some(&[&shim_path]),
        Some(&slopctl_path),
        Some(&claude_config_dir),
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };
    env.append_config("[backup]\nauto_restore = true\ninterval_secs = 1");

    // Boot 1: a pane, captured into the journal on clean shutdown.
    let slopd1 = env.spawn_slopd();
    run_and_wait(&env);
    sigint_child(slopd1);
    let source_path = latest_lifecycle_journal(&env);
    let before = std::fs::read(&source_path).expect("journal written on shutdown");
    assert!(
        before != b"[]" && !before.is_empty(),
        "precondition: journal holds the pane"
    );

    // The executable disappears.
    std::fs::remove_file(&shim).unwrap();

    // Boot 2 (reboot): auto_restore is on, but the executable can't be resolved.
    // Must enter pending and preserve the manifest, not spawn doomed panes.
    reboot_tmux(&env);
    let slopd2 = env.spawn_slopd();
    let status = String::from_utf8_lossy(&env.slopctl(&["status"]).stdout).into_owned();
    assert!(
        status.contains("pending_restore: 1 pane"),
        "a missing executable must enter pending, not silently fail; got {:?}",
        status
    );
    assert_eq!(
        count_panes_with_session(&env, MOCK_SID),
        0,
        "nothing should have been spawned"
    );
    // Past several periodic backup ticks: the source checkpoint must survive.
    std::thread::sleep(Duration::from_millis(2500));
    let after = std::fs::read(&source_path).expect("source journal still present");
    assert_eq!(
        before, after,
        "a failed restore must not clobber the source checkpoint"
    );

    // Recovery: with the executable back, `slopctl restore` brings the pane up,
    // proving the spawn resolves it through the shared chokepoint.
    std::fs::copy(cargo_bin("mock_claude"), &shim).unwrap();
    let out = env.slopctl(&["restore"]);
    assert!(out.status.success(), "restore failed: {:?}", out);
    let deadline = Instant::now() + Duration::from_secs(15);
    while count_panes_with_session(&env, MOCK_SID) == 0 {
        if Instant::now() > deadline {
            kill_slopd(slopd2);
            panic!("pane did not come back after the executable was restored");
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    kill_slopd(slopd2);
}

// Issue #3 regression: a pane's working_dir (#{pane_current_path}) drifts when
// the agent `cd`s, but `claude --resume <id>` finds the session via the project
// dir of its LAUNCH cwd (the dir Claude was started in, recorded in the
// transcript). Restore must launch from the transcript's recorded cwd, not the
// drifted working_dir — otherwise claude searches the wrong project, can't find
// the session, and the pane dies. Here working_dir points to an unrelated dir
// while the transcript records the real launch cwd; the restored pane must come
// up in the launch cwd.
#[test]
fn restore_uses_transcript_launch_cwd_over_drifted_working_dir() {
    let Some((env, home)) = backup_env("[backup]\nauto_restore = true") else {
        eprintln!("skipping: tmux not found");
        return;
    };
    // Where Claude was really launched (its transcript records this cwd)...
    let launch_dir = home.path().join("launch");
    std::fs::create_dir_all(&launch_dir).unwrap();
    // ...vs an unrelated dir we plant in working_dir to simulate the agent
    // having cd'd away. Restore must NOT launch here.
    let drifted_dir = home.path().join("drifted");
    std::fs::create_dir_all(&drifted_dir).unwrap();
    // A transcript recording the launch cwd, like real Claude. The first record
    // has no cwd (mirroring the real leading "mode" record) to exercise the scan.
    let transcript = home.path().join("session-transcript.jsonl");
    std::fs::write(
        &transcript,
        format!(
            "{{\"type\":\"mode\"}}\n{{\"type\":\"user\",\"cwd\":{:?}}}\n",
            launch_dir.to_str().unwrap(),
        ),
    )
    .unwrap();

    // Boot 1: a pane so a checkpoint is written, then clean shutdown.
    let slopd1 = env.spawn_slopd();
    run_and_wait(&env);
    sigint_child(slopd1);

    // Plant the drift: working_dir → the unrelated dir, transcript_path → our
    // transcript recording the real launch cwd.
    let mut checkpoint = latest_lifecycle_checkpoint(&env);
    for p in &mut checkpoint {
        if p.session_id.as_deref() == Some(MOCK_SID) {
            p.working_dir = Some(drifted_dir.to_str().unwrap().to_string());
            p.transcript_path = Some(transcript.to_str().unwrap().to_string());
        }
    }
    append_lifecycle_checkpoint(&env, &checkpoint);

    // Reboot → restore. The restored pane must be launched in launch_dir.
    reboot_tmux(&env);
    let slopd2 = env.spawn_slopd();
    let deadline = Instant::now() + Duration::from_secs(15);
    let restored = loop {
        let panes: Vec<libslop::PaneInfo> =
            serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap_or_default();
        if let Some(p) = panes
            .into_iter()
            .find(|p| p.session_id.as_deref() == Some(MOCK_SID))
        {
            break p;
        }
        if Instant::now() > deadline {
            kill_slopd(slopd2);
            panic!("pane was not restored");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(
        restored.working_dir.as_deref(),
        launch_dir.to_str(),
        "restore must launch from the transcript's recorded launch cwd, not the drifted working_dir"
    );

    kill_slopd(slopd2);
}

/// When a turn ends with StopFailure (e.g. an API 500), slopd auto-continues it
/// by sending "continue" after a backoff — without the user having to. We assert
/// this end-to-end by watching the durable `slopctl listen` event stream for the
/// auto-injected `continue` UserPromptSubmit, rather than racing the pane's
/// transient busy_processing state (the mock's continue-turn ends in a few ms,
/// far faster than any ps poll could observe).
#[test]
fn auto_continue_on_stop_failure() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    // Fast retry settings for the test: 100ms initial backoff, max 200ms, 2 attempts.
    let Some(env) = TestEnv::new_with_auto_continue(
        Some(&[cargo_bin("mock_claude").to_str().unwrap()]),
        None,
        2,   // max_attempts
        100, // initial_backoff_ms
        200, // max_backoff_ms
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Spawn a pane.
    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    // Wait for the pane to be ready (SessionStart fired).
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (_, detailed) = env.pane_state(&pane_id);
        if detailed == libslop::PaneDetailedState::Ready {
            break;
        }
        if Instant::now() > deadline {
            kill_slopd(slopd);
            panic!("pane did not become ready; detailed_state: {:?}", detailed);
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Subscribe to the event stream BEFORE triggering the failure so we can't
    // miss the auto-injected "continue" prompt (the stream has no replay).
    let mut listen = Command::new(cargo_bin("slopctl"))
        .args(["listen", "--hook", "UserPromptSubmit"])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl listen");
    let stdout = listen.stdout.take().unwrap();
    let (subscribed_line, reader) = read_line_timeout(stdout, Duration::from_secs(10))
        .expect("timed out reading subscribed line");
    assert!(
        subscribed_line.contains("subscribed"),
        "unexpected first line: {:?}",
        subscribed_line
    );

    // Switch to always-submit mode so a single Enter submits the next line.
    env.tmux
        .tmux()
        .args([
            "send-keys",
            "-t",
            &pane_id,
            "::mock input-mode always-submit",
            "Enter",
            "Enter",
        ])
        .status()
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));

    // Trigger a failed turn: mock_claude fires UserPromptSubmit → StopFailure.
    env.tmux
        .tmux()
        .args(["send-keys", "-t", &pane_id, "::mock fail once", "Enter"])
        .status()
        .unwrap();

    // The first UserPromptSubmit we see is for "::mock fail once" itself; the second
    // must be the auto-injected "continue" — proof that slopd recovered the
    // failed turn on its own.
    let mut reader: Box<dyn std::io::Read + Send> = reader;
    let mut saw_continue = false;
    let overall_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < overall_deadline {
        let (ev, next_reader) = read_next_hook_event(reader);
        reader = next_reader;
        let prompt = ev["payload"]["prompt"].as_str().unwrap_or("");
        if prompt == "continue" {
            saw_continue = true;
            break;
        }
    }

    kill_child(listen);
    kill_slopd(slopd);

    assert!(
        saw_continue,
        "slopd did not auto-continue the failed turn with a 'continue' prompt"
    );
}

/// A turn that keeps failing must NOT be auto-continued forever: slopd gives up
/// after `max_retry_attempts`. Regression guard for the bug where the injected
/// "continue"'s own UserPromptSubmit reset the retry counter, so the cap was
/// never reached. With max_attempts=2 we expect exactly two injected "continue"
/// prompts, then silence.
#[test]
fn auto_continue_gives_up_after_max_attempts() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let Some(env) = TestEnv::new_with_auto_continue(
        Some(&[cargo_bin("mock_claude").to_str().unwrap()]),
        None,
        2,   // max_attempts
        50,  // initial_backoff_ms
        100, // max_backoff_ms
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    // Wait for the pane to be ready.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (_, detailed) = env.pane_state(&pane_id);
        if detailed == libslop::PaneDetailedState::Ready {
            break;
        }
        if Instant::now() > deadline {
            kill_slopd(slopd);
            panic!("pane did not become ready; detailed_state: {:?}", detailed);
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Subscribe before triggering so we catch every injected "continue".
    let mut listen = Command::new(cargo_bin("slopctl"))
        .args(["listen", "--hook", "UserPromptSubmit"])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl listen");
    let stdout = listen.stdout.take().unwrap();
    let (subscribed_line, reader) = read_line_timeout(stdout, Duration::from_secs(10))
        .expect("timed out reading subscribed line");
    assert!(
        subscribed_line.contains("subscribed"),
        "unexpected first line: {:?}",
        subscribed_line
    );

    env.tmux
        .tmux()
        .args([
            "send-keys",
            "-t",
            &pane_id,
            "::mock input-mode always-submit",
            "Enter",
            "Enter",
        ])
        .status()
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));

    // Turn on persistent-failure mode, then trigger the first failing turn.
    env.tmux
        .tmux()
        .args(["send-keys", "-t", &pane_id, "::mock fail always", "Enter"])
        .status()
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    env.tmux
        .tmux()
        .args(["send-keys", "-t", &pane_id, "trigger", "Enter"])
        .status()
        .unwrap();

    // Count injected "continue" prompts. With max_attempts=2 and fast backoff
    // (50/100ms), all retries finish well within the collection window. We read
    // for a fixed budget and assert the count stops at exactly 2 — proof the cap
    // holds and it isn't retrying forever.
    let mut reader: Box<dyn std::io::Read + Send> = reader;
    let mut continue_count = 0;
    let collect_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < collect_deadline {
        // Bounded per-line wait so we stop reading once retries cease.
        match read_line_timeout(reader, Duration::from_millis(500)) {
            Ok((line, next_reader)) => {
                reader = next_reader;
                let v: serde_json::Value = match serde_json::from_str(line.trim()) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if v["source"] == "hook" && v["payload"]["prompt"].as_str() == Some("continue") {
                    continue_count += 1;
                }
            }
            // No event for 500ms — retries have stopped.
            Err(_) => break,
        }
    }

    kill_child(listen);
    kill_slopd(slopd);

    assert_eq!(
        continue_count, 2,
        "expected exactly max_retry_attempts (2) auto-continue prompts, got {}",
        continue_count
    );
}

/// A turn that runs LONGER than the retry backoff must not provoke a second
/// "continue": the resend is edge-triggered by StopFailure (end of turn), not a
/// periodic timer, so while the auto-continued turn is still running no new retry
/// is scheduled. Guards against a regression to naive periodic resending, which
/// would spam "continue" into the busy turn. Backoff is 100ms but the continued
/// turn runs 1000ms — 10× the delay — so any periodic resender would fire
/// several times in that window.
#[test]
fn auto_continue_does_not_resend_during_long_turn() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_claude");

    let Some(env) = TestEnv::new_with_auto_continue(
        Some(&[cargo_bin("mock_claude").to_str().unwrap()]),
        None,
        5,   // max_attempts (room for spurious resends to show up if the bug exists)
        100, // initial_backoff_ms
        200, // max_backoff_ms
    ) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let run_output = env.slopctl(&["run"]);
    assert!(
        run_output.status.success(),
        "slopctl run failed: {:?}",
        run_output
    );
    let pane_id = String::from_utf8_lossy(&run_output.stdout)
        .trim()
        .to_string();

    // Wait for the pane to be ready.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (_, detailed) = env.pane_state(&pane_id);
        if detailed == libslop::PaneDetailedState::Ready {
            break;
        }
        if Instant::now() > deadline {
            kill_slopd(slopd);
            panic!("pane did not become ready; detailed_state: {:?}", detailed);
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Subscribe before triggering so we catch every injected "continue".
    let mut listen = Command::new(cargo_bin("slopctl"))
        .args(["listen", "--hook", "UserPromptSubmit"])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn slopctl listen");
    let stdout = listen.stdout.take().unwrap();
    let (subscribed_line, reader) = read_line_timeout(stdout, Duration::from_secs(10))
        .expect("timed out reading subscribed line");
    assert!(
        subscribed_line.contains("subscribed"),
        "unexpected first line: {:?}",
        subscribed_line
    );

    env.tmux
        .tmux()
        .args([
            "send-keys",
            "-t",
            &pane_id,
            "::mock input-mode always-submit",
            "Enter",
            "Enter",
        ])
        .status()
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));

    // Fail once, then the injected "continue" runs a 1000ms busy turn (10× the
    // 100ms backoff) before a clean Stop.
    env.tmux
        .tmux()
        .args([
            "send-keys",
            "-t",
            &pane_id,
            "::mock fail-then-busy 1000ms",
            "Enter",
        ])
        .status()
        .unwrap();

    // Count "continue" prompts across a fixed wall-clock window that spans the
    // whole busy turn. Drain on a background thread so a GAP in the stream (the
    // busy turn emits PreToolUse/PostToolUse/Stop — none of them UserPromptSubmit
    // — so this --hook-filtered stream is silent for ~1s) does NOT end
    // collection early: a periodic resender would fire its spurious "continue"
    // precisely during that silent gap, so we must keep listening through it.
    // 2.5s comfortably exceeds the 1000ms turn plus scheduling slack.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = std::io::BufReader::new(reader);
        loop {
            let mut line = String::new();
            match buf.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    let mut continue_count = 0;
    let collect_deadline = Instant::now() + Duration::from_millis(2500);
    while Instant::now() < collect_deadline {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                let v: serde_json::Value = match serde_json::from_str(line.trim()) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if v["source"] == "hook" && v["payload"]["prompt"].as_str() == Some("continue") {
                    continue_count += 1;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // The continued turn ended cleanly, so the pane is back to Ready.
    let (_, detailed) = env.pane_state(&pane_id);

    kill_child(listen);
    kill_slopd(slopd);

    assert_eq!(
        continue_count, 1,
        "expected exactly one auto-continue (no resend during the long turn), got {}",
        continue_count
    );
    assert_eq!(
        detailed,
        libslop::PaneDetailedState::Ready,
        "pane should be Ready after the continued turn completed cleanly"
    );
}

#[test]
fn opencode_delayed_start_is_backed_up_and_restored() {
    // Real OpenCode accepts TCP connections before its instance API finishes
    // bootstrapping. In that window GET /session fails quickly, while an eager
    // POST /session can hang. slopd must wait for readiness, record the durable
    // session, include the pane in backup, and resume it after a reboot.
    build_bin("slopctl");
    build_bin("mock_opencode");
    let mock_opencode = cargo_bin("mock_opencode");
    let oc_config_dir = tempfile::tempdir().unwrap();

    let env = TestEnv::new_full(None, None, None).expect("tmux required");
    env.append_config(&format!(
        "\n[accounts.oc]\nbackend = \"opencode\"\nexecutable = {:?}\nclaude_config_dir = {:?}\n\
         [backup]\nauto_restore = true\n",
        mock_opencode.to_str().unwrap(),
        oc_config_dir.path().to_str().unwrap(),
    ));

    let slopd1 = env.spawn_slopd();
    let run_out = env.slopctl_raw(&[
        "run",
        "--no-wait",
        "--account",
        "oc",
        "--",
        "--mock-startup-delay",
        "800ms",
    ]);
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();
    assert!(
        run_out.status.success() && !pane_id.is_empty(),
        "delayed OpenCode run failed: {:?} stdout={:?} stderr={:?}",
        run_out.status,
        String::from_utf8_lossy(&run_out.stdout),
        String::from_utf8_lossy(&run_out.stderr),
    );

    let panes: Vec<libslop::PaneInfo> =
        serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap_or_default();
    let pane = panes
        .iter()
        .find(|pane| pane.pane_id == pane_id)
        .expect("delayed OpenCode pane in ps");
    assert_eq!(
        pane.session_id.as_deref(),
        Some("ses_mock"),
        "slopd should wait for the instance API and record its durable session",
    );

    let backup = env.slopctl(&["backup"]);
    assert!(
        backup.status.success() && String::from_utf8_lossy(&backup.stdout).contains("backed up 1"),
        "durable OpenCode session should be backed up: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&backup.stdout),
        String::from_utf8_lossy(&backup.stderr),
    );

    sigint_child(slopd1);
    reboot_tmux(&env);
    let slopd2 = env.spawn_slopd();

    let deadline = Instant::now() + Duration::from_secs(15);
    let restored = loop {
        let panes: Vec<libslop::PaneInfo> =
            serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap_or_default();
        if let Some(pane) = panes
            .into_iter()
            .find(|pane| pane.session_id.as_deref() == Some("ses_mock"))
        {
            break pane;
        }
        if Instant::now() >= deadline {
            kill_slopd(slopd2);
            panic!("OpenCode pane was not restored from the backup manifest");
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    assert_eq!(restored.backend, libslop::Backend::Opencode);
    assert_eq!(restored.account, "oc");
    assert_ne!(
        restored.pane_id, pane_id,
        "restore should create a fresh tmux pane"
    );

    kill_slopd(slopd2);
}

#[test]
fn opencode_pane_is_tracked_sendable_through_tui_and_interruptible_over_http() {
    // End-to-end for the opencode backend: slopd spawns mock_opencode (which
    // binds the assigned --port and serves the API subset OpencodeClient uses),
    // tracks it via the status-poll driver, submits through the visible TUI, and
    // drives interrupt/transcript over HTTP.
    build_bin("slopctl");
    build_bin("mock_opencode");
    let mock_opencode = cargo_bin("mock_opencode");
    let oc_config_dir = tempfile::tempdir().unwrap();

    let env = TestEnv::new_full(None, None, None).expect("tmux required");
    // An opencode account whose executable is the mock. backend is explicit
    // (inference only recognizes the canonical "opencode"/"claude" names, and
    // this binary is named mock_opencode).
    env.append_config(&format!(
        "\n[accounts.oc]\nbackend = \"opencode\"\nexecutable = {:?}\nclaude_config_dir = {:?}\n",
        mock_opencode.to_str().unwrap(),
        oc_config_dir.path().to_str().unwrap(),
    ));

    let slopd = env.spawn_slopd();

    // Wait-for-ready run: the driver polls mock_opencode's /session/status=idle
    // and flips the pane to ready. Returns the pane id on stdout.
    let run_out = env.slopctl_raw(&["run", "--account", "oc"]);
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();
    assert!(
        run_out.status.success() && !pane_id.is_empty(),
        "slopctl run --account oc failed: {:?} stdout={:?}",
        run_out.status,
        String::from_utf8_lossy(&run_out.stdout)
    );

    let (_, detailed) = env.pane_state(&pane_id);
    assert_eq!(
        detailed,
        libslop::PaneDetailedState::Ready,
        "opencode pane should be ready right after wait-for-ready run, got {:?}",
        detailed
    );

    // ensure_session() must have POSTed /session (a fresh mock lists no session
    // until one is created) and recorded its id — ps shows it.
    let ps_panes: Vec<libslop::PaneInfo> =
        serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap_or_default();
    let p = ps_panes
        .iter()
        .find(|p| p.pane_id == pane_id)
        .expect("pane in ps");
    assert_eq!(
        p.session_id.as_deref(),
        Some("ses_mock"),
        "ensure_session should have created/reused session ses_mock; got {:?}",
        p.session_id
    );
    assert_eq!(p.backend, libslop::Backend::Opencode);

    // Send through OpenCode's composer. mock_opencode consumes the physical
    // Enter from tmux, then simulates a busy→idle turn.
    let send = env.slopctl(&["send", &pane_id, "hello from slopd"]);
    assert!(
        send.status.success(),
        "slopctl send failed: {:?} stderr={:?}",
        send.status,
        String::from_utf8_lossy(&send.stderr)
    );

    // Wait for the mock's turn to complete (busy → ready).
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut back_to_ready = false;
    while Instant::now() < deadline {
        let (_, d) = env.pane_state(&pane_id);
        if d == libslop::PaneDetailedState::Ready {
            back_to_ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        back_to_ready,
        "opencode pane did not return to ready after send"
    );

    // Transcript was pulled over HTTP (GET /message) and contains the prompt.
    let transcript = env.slopctl(&["transcript", &pane_id]);
    let out = String::from_utf8_lossy(&transcript.stdout);
    assert!(
        out.contains("hello from slopd"),
        "opencode transcript should contain the sent text, got: {}",
        out
    );

    // Interrupt over HTTP (POST /abort).
    let interrupt = env.slopctl(&["interrupt", &pane_id]);
    assert!(
        interrupt.status.success(),
        "slopctl interrupt failed: {:?}",
        interrupt.status
    );

    kill_slopd(slopd);
}

/// Every user-supplied OpenCode input must cross one generic composer boundary.
/// In particular `/compact` must not be translated to `/session/:id/command`,
/// and multiline/key-like/Unicode text must survive unchanged.
#[test]
fn opencode_send_routes_arbitrary_input_through_tui_composer() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_opencode");
    let mock_opencode = cargo_bin("mock_opencode");
    let oc_config_dir = tempfile::tempdir().unwrap();
    let logs = tempfile::tempdir().unwrap();
    let input_log = logs.path().join("tui-input.jsonl");
    let connection_log = logs.path().join("http.log");

    let env = TestEnv::new_full(None, None, None).expect("tmux required");
    env.append_config(&format!(
        "\n[accounts.oc]\nbackend = \"opencode\"\nexecutable = {:?}\nclaude_config_dir = {:?}\n",
        mock_opencode.to_str().unwrap(),
        oc_config_dir.path().to_str().unwrap(),
    ));

    let slopd = env.spawn_slopd();
    let input_env = format!("MOCK_OPENCODE_INPUT_LOG={}", input_log.display());
    let connection_env = format!("MOCK_OPENCODE_CONN_LOG={}", connection_log.display());
    let run = env.slopctl_raw(&[
        "run",
        "--account",
        "oc",
        "--env",
        &input_env,
        "--env",
        &connection_env,
    ]);
    assert!(
        run.status.success(),
        "opencode run failed: {:?} stderr={:?}",
        run.status,
        String::from_utf8_lossy(&run.stderr),
    );
    let pane_id = String::from_utf8_lossy(&run.stdout).trim().to_string();

    let compact = env.slopctl(&["send", &pane_id, "/compact", "--timeout", "5"]);
    assert!(
        compact.status.success(),
        "generic /compact send failed: {:?} stderr={:?}",
        compact.status,
        String::from_utf8_lossy(&compact.stderr),
    );

    let compact_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (_, state) = env.pane_state(&pane_id);
        if state == libslop::PaneDetailedState::BusyCompacting {
            break;
        }
        assert!(
            Instant::now() < compact_deadline,
            "mock OpenCode never executed /compact; final state was {:?}",
            state,
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    wait_until_ready(&env, &pane_id, Duration::from_secs(5));

    let arbitrary = "Enter C-u /not-a-command\nsecond line 🦀";
    let send = env.slopctl(&["send", &pane_id, arbitrary, "--timeout", "5"]);
    assert!(
        send.status.success(),
        "arbitrary composer send failed: {:?} stderr={:?}",
        send.status,
        String::from_utf8_lossy(&send.stderr),
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let accepted = loop {
        let values: Vec<String> = std::fs::read_to_string(&input_log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        if values.len() >= 2 {
            break values;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for mock OpenCode TUI submissions; got {:?}",
            values,
        );
        std::thread::sleep(Duration::from_millis(50));
    };

    assert_eq!(accepted[0], "/compact");
    assert_eq!(accepted[1], arbitrary);

    let requests = std::fs::read_to_string(&connection_log).unwrap_or_default();
    assert!(
        requests.matches("POST /tui/clear-prompt").count() >= 2,
        "each send should clear the TUI composer; requests:\n{}",
        requests,
    );
    assert!(
        requests.matches("POST /tui/append-prompt").count() >= 2,
        "each send should append exact text to the TUI composer; requests:\n{}",
        requests,
    );
    assert!(
        !requests.contains(&format!("POST /session/{}/command", "ses_mock")),
        "slopd must not special-case slash commands; requests:\n{}",
        requests,
    );
    assert!(
        !requests.contains(&format!("POST /session/{}/prompt_async", "ses_mock")),
        "ordinary user input must use the same TUI path; requests:\n{}",
        requests,
    );

    kill_slopd(slopd);
}

/// Resuming an opencode session via `slopctl run` must bind slopd's tracking to
/// the REQUESTED session id, not POST a fresh empty one. Regression test for the
/// bug where the run handler always called ensure_session (POST /session),
/// stranding the resumed conversation on a new session: `slopctl run --backend
/// opencode -- -s <id>` spawned a pane but `ps` showed a different, empty
/// session. With the fix, the pane tracks exactly the id passed to `-s`.
#[test]
fn opencode_run_resume_binds_to_requested_session() {
    build_bin("slopctl");
    build_bin("mock_opencode");
    let mock_opencode = cargo_bin("mock_opencode");
    let oc_config_dir = tempfile::tempdir().unwrap();

    let env = TestEnv::new_full(None, None, None).expect("tmux required");
    env.append_config(&format!(
        "\n[accounts.oc]\nbackend = \"opencode\"\nexecutable = {:?}\nclaude_config_dir = {:?}\n",
        mock_opencode.to_str().unwrap(),
        oc_config_dir.path().to_str().unwrap(),
    ));

    let slopd = env.spawn_slopd();

    // Resume a specific session id via the passthrough `-s <id>`. mock_opencode
    // lists it in GET /session (as real opencode does for `opencode -s <id>`), so
    // slopd's resume path finds and binds it.
    let target = "ses_resume_target_1234";
    let run_out = env.slopctl_raw(&[
        "run",
        "--account",
        "oc",
        "--ready-timeout",
        "30",
        "--",
        "-s",
        target,
    ]);
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();
    assert!(
        run_out.status.success() && !pane_id.is_empty(),
        "resume run failed: {:?} stderr={:?}",
        run_out.status,
        String::from_utf8_lossy(&run_out.stderr)
    );

    // The tracked session must be exactly the resumed id — NOT the fresh
    // "ses_mock" that ensure_session's POST would have produced.
    let ps_panes: Vec<libslop::PaneInfo> =
        serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap_or_default();
    let p = ps_panes
        .iter()
        .find(|p| p.pane_id == pane_id)
        .expect("pane in ps");
    assert_eq!(
        p.session_id.as_deref(),
        Some(target),
        "resumed pane must track the requested session id, got {:?}",
        p.session_id
    );
    assert_eq!(p.backend, libslop::Backend::Opencode);

    kill_slopd(slopd);
}

#[test]
fn fork_opencode_pane_binds_new_pane_to_forked_session() {
    // Forking an opencode pane must: call the SOURCE server's POST /session/:id/fork
    // (which mints a fresh top-level session id and returns it), spawn a NEW pane
    // bound to that id via the resume path, link it to the source as parent, and
    // leave the source pane running untouched. The mock's fork endpoint returns
    // FORK_SID ("ses_mock_fork"); the new pane, spawned with `-s ses_mock_fork`,
    // lists and binds to it (the analogue of the real shared session store).
    build_bin("slopctl");
    build_bin("mock_opencode");
    let mock_opencode = cargo_bin("mock_opencode");
    let oc_config_dir = tempfile::tempdir().unwrap();

    let env = TestEnv::new_full(None, None, None).expect("tmux required");
    env.append_config(&format!(
        "\n[accounts.oc]\nbackend = \"opencode\"\nexecutable = {:?}\nclaude_config_dir = {:?}\n",
        mock_opencode.to_str().unwrap(),
        oc_config_dir.path().to_str().unwrap(),
    ));

    let slopd = env.spawn_slopd();

    // Source opencode pane on its own POSTed session (ses_mock).
    let run_out = env.slopctl_raw(&["run", "--account", "oc", "--ready-timeout", "30"]);
    let src_pane = String::from_utf8_lossy(&run_out.stdout).trim().to_string();
    assert!(
        run_out.status.success() && !src_pane.is_empty(),
        "opencode run failed: {:?} stderr={:?}",
        run_out.status,
        String::from_utf8_lossy(&run_out.stderr)
    );

    // Fork it: the new pane must become ready and have its id printed.
    let fork_out = env.slopctl_raw(&["fork", &src_pane, "--ready-timeout", "30"]);
    let fork_pane = String::from_utf8_lossy(&fork_out.stdout).trim().to_string();
    assert!(
        fork_out.status.success() && !fork_pane.is_empty(),
        "fork failed: {:?} stderr={:?}",
        fork_out.status,
        String::from_utf8_lossy(&fork_out.stderr)
    );
    assert_ne!(fork_pane, src_pane, "fork must create a distinct pane");

    let ps: Vec<libslop::PaneInfo> =
        serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap_or_default();
    let fp = ps
        .iter()
        .find(|p| p.pane_id == fork_pane)
        .expect("fork pane in ps");
    assert_eq!(
        fp.session_id.as_deref(),
        Some("ses_mock_fork"),
        "fork pane must bind to the forked session id, got {:?}",
        fp.session_id
    );
    assert_eq!(fp.backend, libslop::Backend::Opencode);
    assert_eq!(
        fp.parent_pane_id.as_deref(),
        Some(src_pane.as_str()),
        "fork pane must record the source pane as its parent"
    );

    // The source pane is untouched: still present, still on its own session.
    let sp = ps
        .iter()
        .find(|p| p.pane_id == src_pane)
        .expect("source pane still present");
    assert_eq!(
        sp.session_id.as_deref(),
        Some("ses_mock"),
        "source pane session must be untouched by the fork, got {:?}",
        sp.session_id
    );

    kill_slopd(slopd);
}

#[test]
fn codex_mock_run_send_approval_transcript_fork_and_restart() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_codex");
    let mock_codex = cargo_bin("mock_codex");
    let codex_home = tempfile::tempdir().unwrap();
    let env = TestEnv::new(None).expect("tmux required");
    env.append_config(&format!(
        "\n[accounts.codex]\nbackend = \"codex\"\nexecutable = {:?}\nconfig_dir = {:?}\n",
        mock_codex.to_str().unwrap(),
        codex_home.path().to_str().unwrap(),
    ));

    let slopd = env.spawn_slopd();
    let run = env.slopctl_raw(&["run", "--account", "codex", "--ready-timeout", "20"]);
    assert!(
        run.status.success(),
        "Codex run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let source = String::from_utf8_lossy(&run.stdout).trim().to_string();
    assert!(
        !codex_home.path().join("app-server-control").exists(),
        "standalone Codex integration must not create a shared app-server"
    );
    let hooks: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(codex_home.path().join("hooks.json")).unwrap(),
    )
    .unwrap();
    assert!(hooks.pointer("/hooks/SessionStart").is_some());
    assert!(
        hooks.pointer("/hooks/Notification").is_none(),
        "Claude-only hook names must not be injected into Codex hooks.json"
    );
    let hup = Command::new("kill")
        .args(["-HUP", &slopd.id().to_string()])
        .status()
        .unwrap();
    assert!(hup.success(), "failed to send SIGHUP to slopd");
    std::thread::sleep(Duration::from_millis(250));
    assert!(
        env.slopctl(&["ps", "--json"]).status.success(),
        "slopd or its standalone Codex pane did not survive SIGHUP"
    );

    let (run_a, run_b) = std::thread::scope(|scope| {
        let a = scope
            .spawn(|| env.slopctl_raw(&["run", "--account", "codex", "--ready-timeout", "20"]));
        let b = scope
            .spawn(|| env.slopctl_raw(&["run", "--account", "codex", "--ready-timeout", "20"]));
        (a.join().unwrap(), b.join().unwrap())
    });
    assert!(
        run_a.status.success() && run_b.status.success(),
        "concurrent Codex runs failed"
    );
    let concurrent_a = String::from_utf8_lossy(&run_a.stdout).trim().to_string();
    let concurrent_b = String::from_utf8_lossy(&run_b.stdout).trim().to_string();
    let concurrent_ps: Vec<libslop::PaneInfo> =
        serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap();
    let session_a = concurrent_ps
        .iter()
        .find(|p| p.pane_id == concurrent_a)
        .and_then(|p| p.session_id.clone());
    let session_b = concurrent_ps
        .iter()
        .find(|p| p.pane_id == concurrent_b)
        .and_then(|p| p.session_id.clone());
    assert!(
        session_a.is_some() && session_b.is_some() && session_a != session_b,
        "concurrent Codex runs must bind distinct threads: {session_a:?} {session_b:?}"
    );

    let send = env.slopctl(&["send", &source, "hello mock codex"]);
    assert!(send.status.success(), "Codex send failed: {:?}", send);
    let active = env.slopctl(&["send", &source, "::mock active"]);
    assert!(active.status.success());
    let deadline = Instant::now() + Duration::from_secs(5);
    while env.pane_state(&source).1 != libslop::PaneDetailedState::BusyProcessing {
        assert!(Instant::now() < deadline, "mock turn never became active");
        std::thread::sleep(Duration::from_millis(25));
    }
    let steer = env.slopctl(&["send", &source, "steered input"]);
    assert!(
        steer.status.success(),
        "Codex steer failed: {}",
        String::from_utf8_lossy(&steer.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while env.pane_state(&source).1 != libslop::PaneDetailedState::Ready {
        assert!(
            Instant::now() < deadline,
            "steered mock turn did not finish"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let transcript = env.slopctl(&["transcript", &source, "--limit", "1"]);
    let transcript: serde_json::Value = serde_json::from_slice(&transcript.stdout).unwrap();
    let records = transcript["records"].as_array().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["event_type"], "agentMessage");

    let approval = env.slopctl(&["send", &source, "::mock permission"]);
    assert!(approval.status.success());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (_, detailed) = env.pane_state(&source);
        if detailed == libslop::PaneDetailedState::AwaitingInputPermission {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "mock approval request was not surfaced: {detailed:?}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    // Cross at least one 2s status-poll interval. `thread/resume` replays a
    // pending approval; the non-mutating `thread/read` backstop must not.
    std::thread::sleep(Duration::from_millis(2500));
    // Approval belongs to the TUI. Answer it as a terminal user would; there is
    // intentionally no slopctl response RPC.
    let response = env
        .tmux
        .tmux()
        .args(["send-keys", "-t", &source, "y", "Enter"])
        .status()
        .unwrap();
    assert!(response.success(), "typing Codex approval into TUI failed");
    let deadline = Instant::now() + Duration::from_secs(5);
    while env.pane_state(&source).1 != libslop::PaneDetailedState::Ready {
        assert!(
            Instant::now() < deadline,
            "approval replay left a second request pending"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    let fork = env.slopctl_raw(&["fork", &source, "--ready-timeout", "20"]);
    assert!(
        fork.status.success(),
        "Codex fork failed: {}",
        String::from_utf8_lossy(&fork.stderr)
    );
    let fork_pane = String::from_utf8_lossy(&fork.stdout).trim().to_string();
    let ps: Vec<libslop::PaneInfo> =
        serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap();
    let source_info = ps.iter().find(|p| p.pane_id == source).unwrap();
    let fork_info = ps.iter().find(|p| p.pane_id == fork_pane).unwrap();
    assert_eq!(fork_info.backend, libslop::Backend::Codex);
    assert_ne!(fork_info.session_id, source_info.session_id);
    assert_eq!(fork_info.parent_pane_id.as_deref(), Some(source.as_str()));

    kill_slopd(slopd);
    let slopd = env.spawn_slopd();
    let recovered = env.slopctl(&["send", &source, "after restart"]);
    assert!(
        recovered.status.success(),
        "recovered Codex send failed: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );

    // Manual backup/restore must recover hook-driven Codex state.
    let before_restore: Vec<libslop::PaneInfo> =
        serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap();
    let source_session = before_restore
        .iter()
        .find(|p| p.pane_id == source)
        .and_then(|p| p.session_id.clone())
        .unwrap();
    let fork_session = before_restore
        .iter()
        .find(|p| p.pane_id == fork_pane)
        .and_then(|p| p.session_id.clone())
        .unwrap();
    assert!(env.slopctl(&["backup"]).status.success());
    assert!(env.slopctl(&["kill", &source]).status.success());
    assert!(env.slopctl(&["kill", &fork_pane]).status.success());
    assert!(env.slopctl(&["restore"]).status.success());
    let deadline = Instant::now() + Duration::from_secs(10);
    let restored = loop {
        let ps: Vec<libslop::PaneInfo> =
            serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap();
        let source = ps
            .iter()
            .find(|p| p.session_id.as_deref() == Some(source_session.as_str()));
        let fork = ps
            .iter()
            .find(|p| p.session_id.as_deref() == Some(fork_session.as_str()));
        if let (Some(source), Some(fork)) = (source, fork)
            && source.detailed_state == libslop::PaneDetailedState::Ready
            && fork.detailed_state == libslop::PaneDetailedState::Ready
        {
            break source.pane_id.clone();
        }
        assert!(
            Instant::now() < deadline,
            "restored Codex panes did not become ready: {ps:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        env.slopctl(&["send", &restored, "after backup restore"])
            .status
            .success()
    );
    kill_slopd(slopd);
}

#[test]
fn fresh_codex_is_ready_before_lazy_session_start_and_interrupt_does_not_exit() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_codex");
    let mock_codex = cargo_bin("mock_codex");
    let codex_home = tempfile::tempdir().unwrap();
    let env = TestEnv::new(None).expect("tmux required");
    env.append_config(&format!(
        "\n[accounts.codex-lazy]\nbackend = \"codex\"\nexecutable = [{:?}, \"--mock-session-start=lazy\"]\nconfig_dir = {:?}\n",
        mock_codex.to_str().unwrap(),
        codex_home.path().to_str().unwrap(),
    ));

    let slopd = env.spawn_slopd();
    let run = env.slopctl_raw(&["run", "--account", "codex-lazy", "--ready-timeout", "10"]);
    assert!(
        run.status.success(),
        "fresh lazy Codex run should become ready before SessionStart: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let pane_id = String::from_utf8_lossy(&run.stdout).trim().to_string();
    let panes: Vec<libslop::PaneInfo> =
        serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap();
    let pane = panes.iter().find(|pane| pane.pane_id == pane_id).unwrap();
    assert_eq!(pane.detailed_state, libslop::PaneDetailedState::Ready);
    assert_eq!(
        pane.session_id, None,
        "fresh Codex must be usable even though it has not created a session yet"
    );

    assert!(
        env.slopctl(&["send", &pane_id, "::mock active"])
            .status
            .success(),
        "first prompt should be accepted by a session-less fresh Codex pane"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let source_session = loop {
        let panes: Vec<libslop::PaneInfo> =
            serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap();
        let pane = panes.iter().find(|pane| pane.pane_id == pane_id).unwrap();
        if pane.session_id.is_some()
            && pane.detailed_state == libslop::PaneDetailedState::BusyProcessing
        {
            break pane.session_id.clone().unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "first prompt did not bind and activate the lazy Codex session"
        );
        std::thread::sleep(Duration::from_millis(25));
    };

    let interrupt = env.slopctl(&["interrupt", &pane_id]);
    assert!(
        interrupt.status.success(),
        "Codex interrupt failed: {}",
        String::from_utf8_lossy(&interrupt.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let panes: Vec<libslop::PaneInfo> =
            serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap();
        if let Some(pane) = panes.iter().find(|pane| pane.pane_id == pane_id)
            && pane.detailed_state == libslop::PaneDetailedState::Ready
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Codex interrupt exited the pane or failed to return it to ready"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    let resume = env.slopctl_raw(&[
        "run",
        "--account",
        "codex-lazy",
        "--ready-timeout",
        "10",
        "--",
        "--resume",
        &source_session,
    ]);
    assert!(
        resume.status.success(),
        "lazy Codex resume should be sendable before SessionStart: {}",
        String::from_utf8_lossy(&resume.stderr)
    );
    let resume_pane = String::from_utf8_lossy(&resume.stdout).trim().to_string();
    let panes: Vec<libslop::PaneInfo> =
        serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap();
    let resume_info = panes
        .iter()
        .find(|pane| pane.pane_id == resume_pane)
        .unwrap();
    assert_eq!(
        resume_info.detailed_state,
        libslop::PaneDetailedState::Ready
    );
    assert_eq!(resume_info.session_id, None);
    assert!(env.slopctl(&["kill", &resume_pane]).status.success());

    let fork = env.slopctl_raw(&["fork", &pane_id, "--ready-timeout", "10"]);
    assert!(
        fork.status.success(),
        "lazy Codex fork should return before its first prompt creates a session: {}",
        String::from_utf8_lossy(&fork.stderr)
    );
    let fork_pane = String::from_utf8_lossy(&fork.stdout).trim().to_string();
    let panes: Vec<libslop::PaneInfo> =
        serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap();
    let fork_info = panes.iter().find(|pane| pane.pane_id == fork_pane).unwrap();
    assert_eq!(fork_info.session_id, None);
    assert_eq!(fork_info.parent_pane_id.as_deref(), Some(pane_id.as_str()));

    assert!(
        env.slopctl(&["send", &fork_pane, "forked first prompt"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let fork_session = loop {
        let panes: Vec<libslop::PaneInfo> =
            serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap();
        let fork_info = panes.iter().find(|pane| pane.pane_id == fork_pane).unwrap();
        if fork_info.session_id.is_some()
            && fork_info.detailed_state == libslop::PaneDetailedState::Ready
        {
            break fork_info.session_id.clone().unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "first fork prompt did not bind its lazy Codex session"
        );
        std::thread::sleep(Duration::from_millis(25));
    };

    assert!(env.slopctl(&["backup"]).status.success());
    assert!(env.slopctl(&["kill", &fork_pane]).status.success());
    assert!(env.slopctl(&["restore"]).status.success());
    let deadline = Instant::now() + Duration::from_secs(5);
    let restored = loop {
        let panes: Vec<libslop::PaneInfo> =
            serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap();
        if let Some(restored) = panes.iter().find(|pane| {
            pane.session_id.as_deref() == Some(fork_session.as_str())
                && pane.detailed_state == libslop::PaneDetailedState::Ready
        }) {
            break restored.clone();
        }
        assert!(
            Instant::now() < deadline,
            "resumed lazy Codex pane did not become ready before SessionStart"
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(
        restored.parent_pane_id.as_deref(),
        Some(pane_id.as_str()),
        "partial restore must preserve a parent that is already live"
    );
    assert!(
        env.slopctl(&["send", &restored.pane_id, "restored first prompt"])
            .status
            .success(),
        "restored lazy Codex pane should accept its first post-resume prompt"
    );

    kill_slopd(slopd);
}

#[test]
fn codex_send_uses_bracketed_paste_and_waits_until_prompt_is_submitted() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_codex");
    let mock_codex = cargo_bin("mock_codex");
    let codex_home = tempfile::tempdir().unwrap();
    let env = TestEnv::new(None).expect("tmux required");
    env.append_config(&format!(
        "\n[accounts.codex-enter-retry]\nbackend = \"codex\"\nexecutable = [{:?}, \"--mock-require-bracketed-paste\", \"--mock-submit-after=3\"]\nconfig_dir = {:?}\n",
        mock_codex.to_str().unwrap(),
        codex_home.path().to_str().unwrap(),
    ));

    let slopd = env.spawn_slopd();
    let run = env.slopctl_raw(&[
        "run",
        "--account",
        "codex-enter-retry",
        "--ready-timeout",
        "10",
    ]);
    assert!(
        run.status.success(),
        "Codex run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let pane_id = String::from_utf8_lossy(&run.stdout).trim().to_string();
    let prompt = format!(
        "BRACKETED_PASTE_CANARY\n{}",
        vec!["multiline Buzz payload"; 80].join("\n")
    );

    let send = env.slopctl(&["send", &pane_id, &prompt]);
    assert!(
        send.status.success(),
        "Codex send failed: {}",
        String::from_utf8_lossy(&send.stderr)
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let transcript = env.slopctl(&["transcript", &pane_id, "--limit", "20"]);
        let transcript: serde_json::Value = serde_json::from_slice(&transcript.stdout).unwrap();
        let submitted = transcript["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| {
                record["event_type"] == "userMessage"
                    && record
                        .pointer("/payload/text")
                        .and_then(serde_json::Value::as_str)
                        == Some(prompt.as_str())
            });
        if submitted {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Codex prompt was left as a draft after ignored Enter keys: {transcript}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    kill_slopd(slopd);
}

#[test]
fn codex_send_does_not_report_an_unsubmitted_draft_as_sent() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_codex");
    let mock_codex = cargo_bin("mock_codex");
    let codex_home = tempfile::tempdir().unwrap();
    let env = TestEnv::new(None).expect("tmux required");
    env.append_config(&format!(
        "\n[accounts.codex-never-submit]\nbackend = \"codex\"\nexecutable = [{:?}, \"--mock-require-bracketed-paste\", \"--mock-submit-after=255\"]\nconfig_dir = {:?}\n",
        mock_codex.to_str().unwrap(),
        codex_home.path().to_str().unwrap(),
    ));

    let slopd = env.spawn_slopd();
    let run = env.slopctl_raw(&[
        "run",
        "--account",
        "codex-never-submit",
        "--ready-timeout",
        "10",
    ]);
    assert!(
        run.status.success(),
        "Codex run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let pane_id = String::from_utf8_lossy(&run.stdout).trim().to_string();

    let send = env.slopctl(&[
        "send",
        &pane_id,
        "UNSUBMITTED_DRAFT_CANARY",
        "--timeout",
        "1",
    ]);
    let transcript = env.slopctl(&["transcript", &pane_id, "--limit", "20"]);
    kill_slopd(slopd);

    assert!(
        !send.status.success(),
        "slopctl send falsely reported an unsubmitted Codex draft as sent"
    );
    assert!(
        String::from_utf8_lossy(&send.stderr).contains("timed out"),
        "send failure did not explain the missing submission: {}",
        String::from_utf8_lossy(&send.stderr)
    );
    let transcript: serde_json::Value = serde_json::from_slice(&transcript.stdout).unwrap();
    assert!(
        transcript["records"]
            .as_array()
            .is_none_or(|records| records.is_empty()),
        "unsubmitted draft leaked into the transcript: {transcript}"
    );
}

fn mock_codex_policy(env: &TestEnv, pane_id: &str) -> serde_json::Value {
    let mut listener = Command::new(cargo_bin("slopctl"))
        .args(["listen", "--pane-id", pane_id])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn Codex policy listener");
    let stdout = listener.stdout.as_mut().expect("listener has no stdout");
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        use std::io::Read;
        stdout
            .read_exact(&mut byte)
            .expect("failed to read subscription confirmation");
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    assert!(
        String::from_utf8_lossy(&line).contains("subscribed"),
        "unexpected policy listener confirmation: {:?}",
        line
    );

    let send = env.slopctl(&["send", pane_id, "::mock policy show"]);
    assert!(
        send.status.success(),
        "Codex policy probe failed: {}",
        String::from_utf8_lossy(&send.stderr)
    );
    let pane_id = pane_id.to_string();
    let event = wait_for_event(listener, move |value| {
        value["source"] == "transcript"
            && value["event_type"] == "agentMessage"
            && value["pane_id"] == pane_id
            && value
                .pointer("/payload/text")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| serde_json::from_str::<serde_json::Value>(text).is_ok())
    });
    let text = event
        .pointer("/payload/text")
        .and_then(serde_json::Value::as_str)
        .expect("mock Codex policy event lacks text");
    serde_json::from_str(text).expect("mock Codex returned invalid policy JSON")
}

fn assert_mock_codex_yolo(env: &TestEnv, pane_id: &str) {
    let policy = mock_codex_policy(env, pane_id);
    assert_eq!(
        policy["approvalPolicy"], "never",
        "Codex approval policy was not reapplied for pane {pane_id}: {policy}"
    );
    assert_eq!(
        policy["sandbox"], "danger-full-access",
        "Codex sandbox policy was not reapplied for pane {pane_id}: {policy}"
    );
}

#[test]
fn codex_standalone_args_survive_fork_recovery_and_restore() {
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_codex");
    let mock_codex = cargo_bin("mock_codex");
    let codex_home = tempfile::tempdir().unwrap();
    let env = TestEnv::new(None).expect("tmux required");
    env.append_config(&format!(
        "\n[accounts.codex-yolo]\nbackend = \"codex\"\nexecutable = [{:?}, \"--dangerously-bypass-approvals-and-sandbox\"]\nconfig_dir = {:?}\n",
        mock_codex.to_str().unwrap(),
        codex_home.path().to_str().unwrap(),
    ));

    let slopd = env.spawn_slopd();
    let run = env.slopctl_raw(&["run", "--account", "codex-yolo", "--ready-timeout", "20"]);
    assert!(
        run.status.success(),
        "Codex YOLO run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let source = String::from_utf8_lossy(&run.stdout).trim().to_string();
    assert_mock_codex_yolo(&env, &source);

    // A fork is another standalone CLI process and receives the configured
    // executable arguments independently.
    let fork = env.slopctl_raw(&["fork", &source, "--ready-timeout", "20"]);
    assert!(
        fork.status.success(),
        "Codex YOLO fork failed: {}",
        String::from_utf8_lossy(&fork.stderr)
    );
    let fork_pane = String::from_utf8_lossy(&fork.stdout).trim().to_string();
    assert_mock_codex_yolo(&env, &fork_pane);

    // A daemon restart recovers hooks/transcript state; it does not reconnect
    // either pane to a shared service or mutate the live process policy.
    kill_slopd(slopd);
    let slopd = env.spawn_slopd();
    wait_until_ready(&env, &fork_pane, Duration::from_secs(10));
    assert_mock_codex_yolo(&env, &fork_pane);

    // Manual backup/restore spawns standalone `codex resume` processes and
    // reapplies the configured executable arguments.
    let panes: Vec<libslop::PaneInfo> =
        serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap();
    let fork_session = panes
        .iter()
        .find(|pane| pane.pane_id == fork_pane)
        .and_then(|pane| pane.session_id.clone())
        .expect("fork pane lacks a Codex thread id");
    assert!(env.slopctl(&["backup"]).status.success());
    assert!(env.slopctl(&["kill", &source]).status.success());
    assert!(env.slopctl(&["kill", &fork_pane]).status.success());
    assert!(env.slopctl(&["restore"]).status.success());
    let deadline = Instant::now() + Duration::from_secs(10);
    let restored_fork = loop {
        let panes: Vec<libslop::PaneInfo> =
            serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap_or_default();
        if let Some(pane) = panes.into_iter().find(|pane| {
            pane.session_id.as_deref() == Some(fork_session.as_str())
                && pane.detailed_state == libslop::PaneDetailedState::Ready
        }) {
            break pane.pane_id;
        }
        assert!(
            Instant::now() < deadline,
            "restored Codex YOLO fork did not become ready"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_mock_codex_yolo(&env, &restored_fork);

    kill_slopd(slopd);
}

#[test]
fn fork_claude_pane_mints_new_forked_session() {
    // Forking a Claude pane mints a fresh session id and spawns a pane with
    // `--resume <src> --fork-session --session-id <new>`. The new pane must track
    // the minted id (uuid-shaped, distinct from the source's), record the source as
    // its parent, and leave the source running. mock_claude honors --session-id, so
    // its SessionStart hook reports exactly the id slopd minted.
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
    let run_out = env.slopctl(&["run"]);
    assert!(run_out.status.success(), "run failed: {:?}", run_out);
    let src_pane = String::from_utf8_lossy(&run_out.stdout).trim().to_string();
    env.wait_for_session_start(listener, &src_pane);

    // Fork the claude pane; catch the forked pane's SessionStart so slopd has
    // finished binding by the time we inspect ps.
    let fork_listener = env.spawn_session_start_listener();
    let fork_out = env.slopctl_raw(&["fork", &src_pane, "--ready-timeout", "30"]);
    let fork_pane = String::from_utf8_lossy(&fork_out.stdout).trim().to_string();
    assert!(
        fork_out.status.success() && !fork_pane.is_empty(),
        "fork failed: {:?} stderr={:?}",
        fork_out.status,
        String::from_utf8_lossy(&fork_out.stderr)
    );
    assert_ne!(fork_pane, src_pane, "fork must create a distinct pane");

    // Faithful to real Claude (verified live): a forked pane's SessionStart hook
    // fires with the RESUMED SOURCE id, not the minted fork id. slopd must NOT
    // bind that id — it pins the fork id it minted. The listener returns the hook
    // id, which is therefore the source id.
    let hook_sid = env.wait_for_session_start(fork_listener, &fork_pane);
    assert_eq!(
        hook_sid, "mock-session-id-1234",
        "mock must reproduce real Claude: the fork's SessionStart reports the source id"
    );

    let ps: Vec<libslop::PaneInfo> =
        serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap_or_default();
    let fp = ps
        .iter()
        .find(|p| p.pane_id == fork_pane)
        .expect("fork pane in ps");
    assert_eq!(fp.backend, libslop::Backend::Claude);
    // Regression: despite the SessionStart hook reporting the source id, slopd
    // tracks the MINTED fork id — a fresh uuid, distinct from the source's.
    let fork_sid = fp.session_id.clone().expect("fork pane has a session id");
    assert_ne!(
        fork_sid, "mock-session-id-1234",
        "slopd must pin the minted fork id, not the source id the SessionStart hook reports"
    );
    assert_eq!(
        fork_sid.len(),
        36,
        "fork session id should be a uuid, got {:?}",
        fork_sid
    );
    assert_eq!(
        fork_sid.matches('-').count(),
        4,
        "fork session id should be a uuid, got {:?}",
        fork_sid
    );
    // The tracked transcript is the fork's OWN file (named for the fork id), not
    // the source's — so `transcript`/tailing read the copy, not the original.
    let tp = fp
        .transcript_path
        .clone()
        .expect("fork pane has a transcript path");
    assert!(
        tp.ends_with(&format!("{}.jsonl", fork_sid)),
        "fork transcript must be the fork session's file, got {:?}",
        tp
    );
    assert!(
        !tp.contains("mock-session-id-1234"),
        "fork transcript must not be the source session's file, got {:?}",
        tp
    );
    assert_eq!(
        fp.parent_pane_id.as_deref(),
        Some(src_pane.as_str()),
        "fork pane must record the source pane as its parent"
    );

    // The source pane keeps its original session.
    let sp = ps
        .iter()
        .find(|p| p.pane_id == src_pane)
        .expect("source pane present");
    assert_eq!(
        sp.session_id.as_deref(),
        Some("mock-session-id-1234"),
        "source pane session must be untouched by the fork"
    );

    kill_slopd(slopd);
}

#[test]
fn opencode_follows_tui_session_switch() {
    // When the human navigates the TUI to a different session, opencode emits a
    // `tui.session.select` SSE event. slopd must follow it: re-point the session it
    // drives so `ps`/`send`/`transcript` describe the conversation the pane now
    // shows, instead of staying bound to the spawn-time session. Without the fix,
    // slopd kept driving the old session and the two views diverged.
    build_bin("slopctl");
    build_bin("mock_opencode");
    let mock_opencode = cargo_bin("mock_opencode");
    let oc_config_dir = tempfile::tempdir().unwrap();

    let env = TestEnv::new_full(None, None, None).expect("tmux required");
    env.append_config(&format!(
        "\n[accounts.oc]\nbackend = \"opencode\"\nexecutable = {:?}\nclaude_config_dir = {:?}\n",
        mock_opencode.to_str().unwrap(),
        oc_config_dir.path().to_str().unwrap(),
    ));

    let slopd = env.spawn_slopd();

    let run_out = env.slopctl_raw(&["run", "--account", "oc"]);
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();
    assert!(
        run_out.status.success() && !pane_id.is_empty(),
        "slopctl run --account oc failed: {:?}",
        run_out.status
    );

    // slopd starts on the spawn-time session.
    let session_of = |env: &TestEnv| -> Option<String> {
        let ps: Vec<libslop::PaneInfo> =
            serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap_or_default();
        ps.into_iter()
            .find(|p| p.pane_id == pane_id)
            .and_then(|p| p.session_id)
    };
    assert_eq!(
        session_of(&env).as_deref(),
        Some("ses_mock"),
        "slopd should start bound to the spawn-time session"
    );

    // Drive the TUI to switch sessions: the mock's "::mock switch-session" prompt creates a second
    // top-level session and emits tui.session.select for it.
    let send = env.slopctl(&["send", &pane_id, "::mock switch-session"]);
    assert!(
        send.status.success(),
        "slopctl send 'switch' failed: {:?}",
        send.status
    );

    // slopd must re-point onto the followed session (ses_mock2).
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut followed = false;
    while Instant::now() < deadline {
        if session_of(&env).as_deref() == Some("ses_mock2") {
            followed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    assert!(
        followed,
        "slopd did not follow tui.session.select onto ses_mock2; ps session = {:?}",
        session_of(&env)
    );

    // And the command paths follow too: `transcript` now reads the second session's
    // conversation over HTTP (GET /session/ses_mock2/message), not the original's.
    let transcript = env.slopctl(&["transcript", &pane_id]);
    let out = String::from_utf8_lossy(&transcript.stdout);
    assert!(
        out.contains("second session message"),
        "transcript should follow onto the second session, got: {}",
        out
    );

    kill_slopd(slopd);
}

#[test]
fn opencode_creates_own_session_not_ephemeral_boot_session() {
    // Regression: a freshly-booted opencode TUI opens an empty session that the
    // server garbage-collects. slopd used to ADOPT the latest existing session
    // (GET /session) at spawn — so it latched onto that ephemeral boot session,
    // which then 404'd, and `slopctl send` failed with "Session not found"
    // (observed live). slopd must instead POST its own (persistent) session.
    //
    // The mock's --mock-ghost-session lists GHOST_SID as the newest session in
    // GET /session but 404s on any use of it — exactly the trap. The fix must
    // ignore it and drive a POSTed, sendable session.
    build_bin("slopctl");
    build_bin("mock_opencode");
    let mock_opencode = cargo_bin("mock_opencode");
    let oc_config_dir = tempfile::tempdir().unwrap();

    let env = TestEnv::new_full(None, None, None).expect("tmux required");
    // executable is an array so the --mock-ghost-session test flag reaches the mock.
    env.append_config(&format!(
        "\n[accounts.oc]\nbackend = \"opencode\"\nexecutable = [{:?}, \"--mock-ghost-session\"]\nclaude_config_dir = {:?}\n",
        mock_opencode.to_str().unwrap(),
        oc_config_dir.path().to_str().unwrap(),
    ));

    let slopd = env.spawn_slopd();

    let run_out = env.slopctl_raw(&["run", "--account", "oc"]);
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();
    assert!(
        run_out.status.success() && !pane_id.is_empty(),
        "slopctl run --account oc failed: {:?}",
        run_out.status
    );

    // slopd must NOT have adopted the ghost; it POSTed its own session (ses_mock).
    let ps: Vec<libslop::PaneInfo> =
        serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap_or_default();
    let p = ps
        .iter()
        .find(|p| p.pane_id == pane_id)
        .expect("pane in ps");
    assert_ne!(
        p.session_id.as_deref(),
        Some("ses_ghost"),
        "slopd adopted the ephemeral ghost session instead of creating its own"
    );
    assert_eq!(
        p.session_id.as_deref(),
        Some("ses_mock"),
        "slopd should drive the session it POSTed; got {:?}",
        p.session_id
    );

    // The decisive check now crosses the TUI composer, then reads the resulting
    // transcript from the tracked session. A ghost-bound driver would 404 that
    // transcript request even if Enter itself reached the visible TUI.
    let send = env.slopctl(&["send", &pane_id, "hello"]);
    assert!(
        send.status.success(),
        "send failed — slopd is driving an unusable session: {:?} stderr={:?}",
        send.status,
        String::from_utf8_lossy(&send.stderr)
    );
    wait_until_ready(&env, &pane_id, Duration::from_secs(5));
    let transcript = env.slopctl(&["transcript", &pane_id]);
    assert!(
        transcript.status.success()
            && String::from_utf8_lossy(&transcript.stdout).contains("hello"),
        "tracked OpenCode session did not contain the TUI-submitted prompt: {:?}",
        transcript,
    );

    kill_slopd(slopd);
}

#[test]
fn opencode_fresh_pane_becomes_ready_not_stuck_booting() {
    // A freshly-spawned opencode pane must transition booting_up -> ready on its
    // own: the driver POSTs slopd's session and the status-poll confirms it exists
    // and is idle, flipping the pane to ready. This is the no-wait path — we
    // assert the transition happens (not just that `run` blocked until ready), so
    // a regression that left the pane stuck booting_up (e.g. readiness keyed on a
    // session that never appears in GET /session) is caught.
    build_bin("slopctl");
    build_bin("mock_opencode");
    let mock_opencode = cargo_bin("mock_opencode");
    let oc_config_dir = tempfile::tempdir().unwrap();

    let env = TestEnv::new_full(None, None, None).expect("tmux required");
    env.append_config(&format!(
        "\n[accounts.oc]\nbackend = \"opencode\"\nexecutable = {:?}\nclaude_config_dir = {:?}\n",
        mock_opencode.to_str().unwrap(),
        oc_config_dir.path().to_str().unwrap(),
    ));

    let slopd = env.spawn_slopd();

    // env.slopctl injects --no-wait, so run returns while the pane is still
    // booting_up; the driver must drive it to ready without the run blocking.
    let run_out = env.slopctl(&["run", "--account", "oc"]);
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();
    assert!(
        run_out.status.success() && !pane_id.is_empty(),
        "slopctl run --no-wait --account oc failed: {:?}",
        run_out.status
    );

    // Poll until ready (driver reconciles every ~3s; allow margin). The pane must
    // NOT remain stuck in booting_up.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut final_state = libslop::PaneDetailedState::BootingUp;
    while Instant::now() < deadline {
        let (_, d) = env.pane_state(&pane_id);
        final_state = d.clone();
        if d == libslop::PaneDetailedState::Ready {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    kill_slopd(slopd);

    assert_eq!(
        final_state,
        libslop::PaneDetailedState::Ready,
        "fresh opencode pane stuck in {:?}, expected it to reach ready",
        final_state
    );
}

#[test]
fn opencode_driver_stops_reconnecting_after_pane_death() {
    // Regression: tearing down an opencode pane must cancel its HTTP driver (the
    // SSE reader + backstop poll). Before the fix, every teardown path cancelled
    // only the transcript tailer, so the opencode driver kept reconnecting to its
    // now-dead server forever — slopd accumulated a "graveyard" of killed panes it
    // polled indefinitely.
    //
    // We can't observe the leak against the pane's own server (killing the pane
    // kills that server too). Instead we kill the pane, stand a FRESH mock on the
    // same freed port that logs every request it receives, and assert the
    // (should-be-cancelled) driver never connects to it. A leaked driver would
    // reconnect within a couple of backoff cycles and leave log lines behind.
    build_bin("slopctl");
    build_bin("mock_opencode");
    let mock_opencode = cargo_bin("mock_opencode");
    let oc_config_dir = tempfile::tempdir().unwrap();
    let conn_log_dir = tempfile::tempdir().unwrap();
    let conn_log = conn_log_dir.path().join("conns.log");

    let env = TestEnv::new_full(None, None, None).expect("tmux required");
    env.append_config(&format!(
        "\n[accounts.oc]\nbackend = \"opencode\"\nexecutable = {:?}\nclaude_config_dir = {:?}\n",
        mock_opencode.to_str().unwrap(),
        oc_config_dir.path().to_str().unwrap(),
    ));

    let slopd = env.spawn_slopd();

    let run_out = env.slopctl(&["run", "--account", "oc"]);
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();
    assert!(
        run_out.status.success() && !pane_id.is_empty(),
        "slopctl run --account oc failed: {:?}",
        run_out.status
    );
    wait_until_ready(&env, &pane_id, Duration::from_secs(15));

    // Read the port slopd allocated for this pane's embedded server.
    let opt = env
        .tmux
        .tmux()
        .args(["show-options", "-p", "-t", &pane_id, "@slopd_opencode_port"])
        .output()
        .expect("show-options");
    let port: u16 = String::from_utf8_lossy(&opt.stdout)
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no @slopd_opencode_port on pane {}", pane_id));

    // Kill the pane. This kills the in-pane mock (freeing the port) and, with the
    // fix, cancels the opencode driver.
    let kill_out = env.slopctl(&["kill", &pane_id]);
    assert!(
        kill_out.status.success(),
        "slopctl kill failed: {:?}",
        kill_out.status
    );

    // Wait for the old server to be gone (connection refused) so the fresh mock
    // can bind the same port.
    let addr = format!("127.0.0.1:{}", port);
    let free_deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_millis(200))
        .is_ok()
    {
        if Instant::now() > free_deadline {
            panic!("in-pane mock on port {} never exited after kill", port);
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Stand a fresh mock on the same port, logging every request it receives.
    let mut standalone = Command::new(&mock_opencode)
        .args(["--port", &port.to_string(), "--hostname", "127.0.0.1"])
        .env("MOCK_OPENCODE_CONN_LOG", &conn_log)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn standalone mock_opencode");

    // Wait until it's listening.
    let up_deadline = Instant::now() + Duration::from_secs(5);
    while std::net::TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_millis(200))
        .is_err()
    {
        if Instant::now() > up_deadline {
            let _ = standalone.kill();
            kill_slopd(slopd);
            panic!("standalone mock failed to bind port {}", port);
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Give a leaked driver ample time to reconnect. Its SSE reader retries with
    // backoff (starting at 1s) and the backstop poll runs every 3s, so several
    // seconds is plenty for at least one connection in the unfixed case.
    std::thread::sleep(Duration::from_secs(8));

    let _ = standalone.kill();
    let _ = standalone.wait();
    kill_slopd(slopd);

    let connections = std::fs::read_to_string(&conn_log).unwrap_or_default();
    let count = connections.lines().count();
    assert_eq!(
        count, 0,
        "cancelled opencode driver must not reconnect to a dead pane's port; \
         saw {} request(s) to the replacement server:\n{}",
        count, connections,
    );
}

#[test]
fn run_backend_flag_overrides_to_opencode_without_an_account() {
    // `slopctl run --backend opencode` with no opencode account declared: the
    // flag flips the default account's backend to opencode and, because the
    // configured executable (mock_opencode) is an unrecognized name, keeps it as
    // the spawn binary — the documented override behaviour for custom paths.
    build_bin("slopctl");
    build_bin("mock_opencode");
    let mock_path = cargo_bin("mock_opencode").to_str().unwrap().to_string();
    let env = TestEnv::new_full(Some(&[mock_path.as_str()]), None, None).expect("tmux required");

    let slopd = env.spawn_slopd();

    let run_out = env.slopctl_raw(&["run", "--backend", "opencode"]);
    let pane_id = String::from_utf8_lossy(&run_out.stdout).trim().to_string();
    assert!(
        run_out.status.success() && !pane_id.is_empty(),
        "slopctl run --backend opencode failed: {:?} stderr={:?}",
        run_out.status,
        String::from_utf8_lossy(&run_out.stderr)
    );

    let (_, detailed) = env.pane_state(&pane_id);
    assert_eq!(
        detailed,
        libslop::PaneDetailedState::Ready,
        "opencode pane created via --backend should reach ready, got {:?}",
        detailed
    );

    kill_slopd(slopd);
}

#[test]
fn opencode_pane_restores_across_reboot() {
    // A pane created via `--backend opencode` is backed up with backend=opencode,
    // and after a simulated reboot slopd restores it with `opencode -s <id>` over a
    // fresh HTTP port + reattaches the driver — exercising the opencode restore path.
    build_bin("slopd");
    build_bin("slopctl");
    build_bin("mock_opencode");
    let mock_path = cargo_bin("mock_opencode").to_str().unwrap().to_string();
    let env = TestEnv::new_full(Some(&[mock_path.as_str()]), None, None).expect("tmux required");
    env.append_config("[backup]\nauto_restore = true");

    // --- first boot: run an opencode pane, let it reach ready, snapshot ---
    let slopd1 = env.spawn_slopd();
    let pane_id =
        String::from_utf8_lossy(&env.slopctl_raw(&["run", "--backend", "opencode"]).stdout)
            .trim()
            .to_string();
    assert!(!pane_id.is_empty(), "slopctl run --backend opencode failed");
    wait_until_ready(&env, &pane_id, Duration::from_secs(15));

    sigint_child(slopd1); // clean shutdown → writes the checkpoint

    let checkpoint = latest_lifecycle_checkpoint(&env);
    let entry = checkpoint
        .iter()
        .find(|p| p.session_id.as_deref() == Some("ses_mock"))
        .expect("opencode pane should be in the checkpoint");
    assert_eq!(
        entry.backend,
        libslop::Backend::Opencode,
        "checkpoint must record the opencode backend so restore dispatches correctly"
    );

    // --- simulate reboot: destroy the slopd tmux session (and its panes) ---
    let kill = env
        .tmux
        .tmux()
        .args(["kill-session", "-t", "slopd"])
        .status()
        .unwrap();
    assert!(kill.success(), "failed to kill slopd tmux session");

    // --- second boot: auto-restore re-spawns opencode -s + reattaches driver ---
    let slopd2 = env.spawn_slopd();
    let deadline = Instant::now() + Duration::from_secs(20);
    let restored = loop {
        let panes: Vec<libslop::PaneInfo> =
            serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap_or_default();
        if let Some(p) = panes
            .into_iter()
            .find(|p| p.session_id.as_deref() == Some("ses_mock"))
        {
            break p;
        }
        if Instant::now() > deadline {
            kill_slopd(slopd2);
            panic!("opencode pane was not restored within timeout");
        }
        std::thread::sleep(Duration::from_millis(150));
    };
    assert_eq!(
        restored.backend,
        libslop::Backend::Opencode,
        "restored pane should be opencode"
    );
    assert_ne!(
        restored.pane_id, pane_id,
        "restored pane should have a new tmux id"
    );
    // The reattached driver should advance it to ready again.
    wait_until_ready(&env, &restored.pane_id, Duration::from_secs(15));

    kill_slopd(slopd2);
}

/// Poll `pane_state` until the pane is Ready, panicking after `timeout`.
fn wait_until_ready(env: &TestEnv, pane_id: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if env.pane_state(pane_id).1 == libslop::PaneDetailedState::Ready {
            return;
        }
        if Instant::now() > deadline {
            panic!("pane {} did not reach ready within {:?}", pane_id, timeout);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn opencode_listen_transcript_streams_live_over_sse() {
    // Live transcript streaming: the SSE driver broadcasts `source:transcript`
    // records as the opencode turn produces them, so `slopctl listen` sees the
    // turn content in real time (not just via pull `slopctl transcript`).
    build_bin("slopctl");
    build_bin("mock_opencode");
    let mock_path = cargo_bin("mock_opencode").to_str().unwrap().to_string();
    let env = TestEnv::new_full(Some(&[mock_path.as_str()]), None, None).expect("tmux required");

    let slopd = env.spawn_slopd();
    let pane_id =
        String::from_utf8_lossy(&env.slopctl_raw(&["run", "--backend", "opencode"]).stdout)
            .trim()
            .to_string();
    assert!(!pane_id.is_empty(), "slopctl run --backend opencode failed");
    wait_until_ready(&env, &pane_id, Duration::from_secs(15));

    // Subscribe to the pane's live event stream BEFORE sending.
    let mut listen = Command::new(cargo_bin("slopctl"))
        .args(["listen", "--pane-id", &pane_id])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn slopctl listen");
    // Consume the {"subscribed":true} confirmation line.
    {
        use std::io::Read;
        let stdout = listen.stdout.as_mut().expect("listener stdout");
        let mut buf = [0u8; 1];
        let mut line = Vec::new();
        loop {
            stdout
                .read_exact(&mut buf)
                .expect("read subscription confirmation");
            if buf[0] == b'\n' {
                break;
            }
            line.push(buf[0]);
        }
        assert!(
            String::from_utf8_lossy(&line).contains("subscribed"),
            "unexpected first line: {:?}",
            line
        );
    }

    assert!(
        env.slopctl(&["send", &pane_id, "ping"]).status.success(),
        "slopctl send failed"
    );

    // Read streamed lines until the assistant's "echo: ping" transcript record arrives.
    let stdout = listen.stdout.take().expect("listener stdout gone");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut found = false;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) if line.contains("echo: ping") => {
                found = true;
                break;
            }
            Ok(_) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }
    kill_child(listen);
    assert!(
        found,
        "live transcript stream did not deliver the opencode turn record"
    );
    kill_slopd(slopd);
}

#[test]
fn opencode_auto_continues_after_session_error() {
    // A "::mock fail once" prompt fails the first time (session.error); slopd's auto-continue
    // retries it, the retry succeeds, and the assistant reply lands — exercising
    // the opencode equivalent of Claude's StopFailure retry.
    build_bin("slopctl");
    build_bin("mock_opencode");
    let mock_path = cargo_bin("mock_opencode").to_str().unwrap().to_string();
    // Fast backoff so the retry lands within the test window.
    let env = TestEnv::new_with_auto_continue(Some(&[mock_path.as_str()]), None, 3, 50, 200)
        .expect("tmux required");
    let slopd = env.spawn_slopd();
    let pane_id =
        String::from_utf8_lossy(&env.slopctl_raw(&["run", "--backend", "opencode"]).stdout)
            .trim()
            .to_string();
    assert!(!pane_id.is_empty(), "slopctl run --backend opencode failed");
    wait_until_ready(&env, &pane_id, Duration::from_secs(15));

    assert!(
        env.slopctl(&["send", &pane_id, "::mock fail once"])
            .status
            .success(),
        "slopctl send boom failed"
    );

    // The retry's successful turn appends an assistant "echo: ::mock fail once".
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let out_bytes = env.slopctl(&["transcript", &pane_id]).stdout;
        let out = String::from_utf8_lossy(&out_bytes);
        if out.contains("echo: ::mock fail once") {
            break;
        }
        if Instant::now() > deadline {
            kill_slopd(slopd);
            panic!(
                "auto-continue retry did not produce echo: ::mock fail once; transcript: {}",
                out
            );
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    // The retried turn's session.idle lands shortly after the message; wait for it.
    wait_until_ready(&env, &pane_id, Duration::from_secs(10));
    kill_slopd(slopd);
}

#[test]
fn opencode_tool_use_tracks_busy_tool_use_state() {
    // A tool-using turn streams `message.part.updated` with part.type=tool
    // (state pending/running), which maps to busy_tool_use — closing the
    // state-fidelity gap (verified shape from real opencode).
    build_bin("slopctl");
    build_bin("mock_opencode");
    let mock_path = cargo_bin("mock_opencode").to_str().unwrap().to_string();
    let env = TestEnv::new_full(Some(&[mock_path.as_str()]), None, None).expect("tmux required");
    let slopd = env.spawn_slopd();
    let pane_id =
        String::from_utf8_lossy(&env.slopctl_raw(&["run", "--backend", "opencode"]).stdout)
            .trim()
            .to_string();
    assert!(!pane_id.is_empty());
    wait_until_ready(&env, &pane_id, Duration::from_secs(15));

    assert!(
        env.slopctl(&["send", &pane_id, "::mock tool"])
            .status
            .success()
    );
    // The mock holds the tool in pending/running (~300ms) → busy_tool_use.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen_tool_use = false;
    while Instant::now() < deadline {
        if env.pane_state(&pane_id).1 == libslop::PaneDetailedState::BusyToolUse {
            seen_tool_use = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        seen_tool_use,
        "opencode tool turn should pass through busy_tool_use"
    );
    wait_until_ready(&env, &pane_id, Duration::from_secs(10));
    kill_slopd(slopd);
}

#[test]
fn opencode_listen_hook_fires_synthesized_events() {
    // Option A: opencode has no native hooks, but slopd synthesizes hook-NAMED
    // events from its SSE bus, so `slopctl listen --hook` works uniformly. A turn
    // end emits a `Stop` hook (and a tool turn emits `PreToolUse`).
    build_bin("slopctl");
    build_bin("mock_opencode");
    let mock_path = cargo_bin("mock_opencode").to_str().unwrap().to_string();
    let env = TestEnv::new_full(Some(&[mock_path.as_str()]), None, None).expect("tmux required");
    let slopd = env.spawn_slopd();
    let pane_id =
        String::from_utf8_lossy(&env.slopctl_raw(&["run", "--backend", "opencode"]).stdout)
            .trim()
            .to_string();
    assert!(!pane_id.is_empty());
    wait_until_ready(&env, &pane_id, Duration::from_secs(15));

    // Subscribe to Stop + PreToolUse hooks on this pane before sending.
    let mut listen = Command::new(cargo_bin("slopctl"))
        .args([
            "listen",
            "--hook",
            "Stop",
            "--hook",
            "PreToolUse",
            "--pane-id",
            &pane_id,
        ])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn slopctl listen");
    {
        use std::io::Read;
        let stdout = listen.stdout.as_mut().expect("listener stdout");
        let mut buf = [0u8; 1];
        let mut line = Vec::new();
        loop {
            stdout
                .read_exact(&mut buf)
                .expect("read subscription confirmation");
            if buf[0] == b'\n' {
                break;
            }
            line.push(buf[0]);
        }
        assert!(
            String::from_utf8_lossy(&line).contains("subscribed"),
            "unexpected first line: {:?}",
            line
        );
    }

    // A tool turn should produce both PreToolUse and (at turn end) Stop hooks.
    assert!(
        env.slopctl(&["send", &pane_id, "::mock tool"])
            .status
            .success()
    );

    let stdout = listen.stdout.take().expect("listener stdout gone");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut saw_pretool = false;
    let mut saw_stop = false;
    while Instant::now() < deadline && !(saw_pretool && saw_stop) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                if line.contains(r#""source":"hook""#) {
                    if line.contains(r#""event_type":"PreToolUse""#) {
                        saw_pretool = true;
                    }
                    if line.contains(r#""event_type":"Stop""#) {
                        saw_stop = true;
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }
    kill_child(listen);
    assert!(saw_pretool, "expected a synthesized PreToolUse hook event");
    assert!(saw_stop, "expected a synthesized Stop hook event");
    kill_slopd(slopd);
}

#[test]
fn opencode_subagent_turn_tracks_busy_subagent() {
    // opencode runs subagents as child sessions (session.created with parentID ==
    // the pane's session). slopd detects that → busy_subagent. (Verified the
    // child-session shape against real opencode 1.17.x.)
    build_bin("slopctl");
    build_bin("mock_opencode");
    let mock_path = cargo_bin("mock_opencode").to_str().unwrap().to_string();
    let env = TestEnv::new_full(Some(&[mock_path.as_str()]), None, None).expect("tmux required");
    let slopd = env.spawn_slopd();
    let pane_id =
        String::from_utf8_lossy(&env.slopctl_raw(&["run", "--backend", "opencode"]).stdout)
            .trim()
            .to_string();
    assert!(!pane_id.is_empty());
    wait_until_ready(&env, &pane_id, Duration::from_secs(15));

    assert!(
        env.slopctl(&["send", &pane_id, "::mock subagent normal"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen_subagent = false;
    while Instant::now() < deadline {
        if env.pane_state(&pane_id).1 == libslop::PaneDetailedState::BusySubagent {
            seen_subagent = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        seen_subagent,
        "opencode subagent turn should pass through busy_subagent"
    );
    wait_until_ready(&env, &pane_id, Duration::from_secs(10));
    kill_slopd(slopd);
}

#[test]
fn opencode_retrying_subagent_does_not_stick_busy_subagent() {
    // Regression (reproduces a live incident): a subagent (child session) that
    // wedges in `retry` (rate-limit backoff) never emits a terminal SSE event, so
    // the SSE reader's subagent entry would leak forever and pin the pane to
    // busy_subagent — poisoning every main-session event even though nothing is
    // actually running. The backstop reconciles the subagent set against
    // /session/status: a child that is no longer "working" there (retry ≠ working)
    // is pruned, and the main session's own `retry` status surfaces as
    // busy_processing instead.
    build_bin("slopctl");
    build_bin("mock_opencode");
    let mock_path = cargo_bin("mock_opencode").to_str().unwrap().to_string();
    let env = TestEnv::new_full(Some(&[mock_path.as_str()]), None, None).expect("tmux required");
    let slopd = env.spawn_slopd();
    let pane_id =
        String::from_utf8_lossy(&env.slopctl_raw(&["run", "--backend", "opencode"]).stdout)
            .trim()
            .to_string();
    assert!(!pane_id.is_empty());
    wait_until_ready(&env, &pane_id, Duration::from_secs(15));

    // "subagent" + "retry" → the mock spawns a child, then wedges BOTH the child and
    // the main session in retry with NO terminal event on the SSE stream.
    assert!(
        env.slopctl(&["send", &pane_id, "::mock subagent retry"])
            .status
            .success()
    );

    // The SSE reader sees the child spawn → busy_subagent.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen_subagent = false;
    while Instant::now() < deadline {
        if env.pane_state(&pane_id).1 == libslop::PaneDetailedState::BusySubagent {
            seen_subagent = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        seen_subagent,
        "expected busy_subagent once the child session spawns"
    );

    // It must NOT stay stuck: the backstop prunes the retry-wedged child and the
    // main session's retry surfaces as busy_processing. (Before the fix it stayed
    // busy_subagent indefinitely.)
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut recovered = false;
    while Instant::now() < deadline {
        if env.pane_state(&pane_id).1 == libslop::PaneDetailedState::BusyProcessing {
            recovered = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        recovered,
        "retry-wedged subagent must not pin the pane to busy_subagent; expected recovery to busy_processing, got {:?}",
        env.pane_state(&pane_id).1
    );
    kill_slopd(slopd);
}

#[test]
fn opencode_leaked_subagent_pruned_when_idle_event_missed() {
    // Regression: if the child's terminal SSE event is missed (e.g. dropped on an
    // SSE reconnect), the SSE reader never removes it and the pane would stay
    // busy_subagent. The backstop must notice the child is gone from
    // /session/status and prune it, synthesizing the SubagentStop the SSE stream
    // never delivered. Asserting SubagentStop fires has teeth: with the child's
    // session.idle dropped, ONLY the backstop prune can produce it.
    build_bin("slopctl");
    build_bin("mock_opencode");
    let mock_path = cargo_bin("mock_opencode").to_str().unwrap().to_string();
    let env = TestEnv::new_full(Some(&[mock_path.as_str()]), None, None).expect("tmux required");
    let slopd = env.spawn_slopd();
    let pane_id =
        String::from_utf8_lossy(&env.slopctl_raw(&["run", "--backend", "opencode"]).stdout)
            .trim()
            .to_string();
    assert!(!pane_id.is_empty());
    wait_until_ready(&env, &pane_id, Duration::from_secs(15));

    // Subscribe to SubagentStop before triggering, so we can prove the prune emits
    // it even though the child's session.idle is never sent.
    let mut listen = Command::new(cargo_bin("slopctl"))
        .args(["listen", "--hook", "SubagentStop", "--pane-id", &pane_id])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn slopctl listen");
    {
        use std::io::Read;
        let stdout = listen.stdout.as_mut().expect("listener stdout");
        let mut buf = [0u8; 1];
        let mut line = Vec::new();
        loop {
            stdout
                .read_exact(&mut buf)
                .expect("read subscription confirmation");
            if buf[0] == b'\n' {
                break;
            }
            line.push(buf[0]);
        }
        assert!(
            String::from_utf8_lossy(&line).contains("subscribed"),
            "unexpected first line"
        );
    }

    // "subagent" + "leak" → the child spawns and finishes (gone from
    // /session/status) but its session.idle SSE event is DROPPED.
    assert!(
        env.slopctl(&["send", &pane_id, "::mock subagent leak"])
            .status
            .success()
    );
    let stdout = listen.stdout.take().expect("listener stdout gone");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut saw_stop = false;
    while Instant::now() < deadline && !saw_stop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) if line.contains(r#""event_type":"SubagentStop""#) => saw_stop = true,
            Ok(_) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }
    kill_child(listen);
    assert!(
        saw_stop,
        "backstop must synthesize SubagentStop for a child whose session.idle was missed"
    );
    // And the pane recovers to ready (the leaked subagent no longer pins it).
    wait_until_ready(&env, &pane_id, Duration::from_secs(10));
    kill_slopd(slopd);
}

#[test]
fn opencode_question_tool_tracks_awaiting_elicitation() {
    // opencode's `question` tool is its elicitation equivalent (agent asking the
    // user a clarifying question) → awaiting_input_elicitation, plus a
    // synthesized Elicitation hook.
    build_bin("slopctl");
    build_bin("mock_opencode");
    let mock_path = cargo_bin("mock_opencode").to_str().unwrap().to_string();
    let env = TestEnv::new_full(Some(&[mock_path.as_str()]), None, None).expect("tmux required");
    let slopd = env.spawn_slopd();
    let pane_id =
        String::from_utf8_lossy(&env.slopctl_raw(&["run", "--backend", "opencode"]).stdout)
            .trim()
            .to_string();
    assert!(!pane_id.is_empty());
    wait_until_ready(&env, &pane_id, Duration::from_secs(15));

    assert!(
        env.slopctl(&["send", &pane_id, "::mock question"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen_elicitation = false;
    while Instant::now() < deadline {
        if env.pane_state(&pane_id).1 == libslop::PaneDetailedState::AwaitingInputElicitation {
            seen_elicitation = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        seen_elicitation,
        "opencode question tool should set awaiting_input_elicitation"
    );
    kill_slopd(slopd);
}

#[test]
fn opencode_daemon_restart_reattaches_runtime() {
    // A daemon restart (tmux + the opencode pane survive) must re-adopt the pane
    // and reattach its HTTP runtime + driver from the stored @slopd_opencode_port
    // option (load_managed_panes recovery for opencode). Verified by: the pane is
    // still tracked with backend=opencode, reaches ready again, and a send works.
    build_bin("slopctl");
    build_bin("mock_opencode");
    let mock_path = cargo_bin("mock_opencode").to_str().unwrap().to_string();
    let env = TestEnv::new_full(Some(&[mock_path.as_str()]), None, None).expect("tmux required");
    let slopd1 = env.spawn_slopd();
    let pane_id =
        String::from_utf8_lossy(&env.slopctl_raw(&["run", "--backend", "opencode"]).stdout)
            .trim()
            .to_string();
    assert!(!pane_id.is_empty());
    wait_until_ready(&env, &pane_id, Duration::from_secs(15));

    // Clean shutdown: tmux session + the opencode pane survive.
    kill_slopd(slopd1);
    let slopd2 = env.spawn_slopd();

    // The pane is re-adopted from tmux, runtime reattached, driver resumed → ready.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let panes: Vec<libslop::PaneInfo> =
            serde_json::from_slice(&env.slopctl(&["ps", "--json"]).stdout).unwrap_or_default();
        if let Some(p) = panes.iter().find(|p| p.pane_id == pane_id) {
            assert_eq!(
                p.backend,
                libslop::Backend::Opencode,
                "recovered pane should stay opencode"
            );
            if p.detailed_state == libslop::PaneDetailedState::Ready {
                break;
            }
        }
        if Instant::now() > deadline {
            kill_slopd(slopd2);
            panic!("opencode pane not re-tracked after daemon restart");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // A send succeeding post-restart proves the HTTP client was reattached.
    assert!(
        env.slopctl(&["send", &pane_id, "still alive"])
            .status
            .success(),
        "send after restart failed"
    );
    kill_slopd(slopd2);
}

#[test]
fn opencode_subagent_emits_subagent_hooks() {
    // A child session (subagent) synthesizes SubagentStart (on spawn) and
    // SubagentStop (on child idle) hooks, so `listen --hook SubagentStart` works.
    build_bin("slopctl");
    build_bin("mock_opencode");
    let mock_path = cargo_bin("mock_opencode").to_str().unwrap().to_string();
    let env = TestEnv::new_full(Some(&[mock_path.as_str()]), None, None).expect("tmux required");
    let slopd = env.spawn_slopd();
    let pane_id =
        String::from_utf8_lossy(&env.slopctl_raw(&["run", "--backend", "opencode"]).stdout)
            .trim()
            .to_string();
    assert!(!pane_id.is_empty());
    wait_until_ready(&env, &pane_id, Duration::from_secs(15));

    let mut listen = Command::new(cargo_bin("slopctl"))
        .args([
            "listen",
            "--hook",
            "SubagentStart",
            "--hook",
            "SubagentStop",
            "--pane-id",
            &pane_id,
        ])
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn slopctl listen");
    {
        use std::io::Read;
        let stdout = listen.stdout.as_mut().expect("listener stdout");
        let mut buf = [0u8; 1];
        let mut line = Vec::new();
        loop {
            stdout
                .read_exact(&mut buf)
                .expect("read subscription confirmation");
            if buf[0] == b'\n' {
                break;
            }
            line.push(buf[0]);
        }
        assert!(
            String::from_utf8_lossy(&line).contains("subscribed"),
            "unexpected first line: {:?}",
            line
        );
    }

    assert!(
        env.slopctl(&["send", &pane_id, "::mock subagent normal"])
            .status
            .success()
    );
    let stdout = listen.stdout.take().expect("listener stdout gone");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut saw_start = false;
    let mut saw_stop = false;
    while Instant::now() < deadline && !(saw_start && saw_stop) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) if line.contains(r#""source":"hook""#) => {
                if line.contains(r#""event_type":"SubagentStart""#) {
                    saw_start = true;
                }
                if line.contains(r#""event_type":"SubagentStop""#) {
                    saw_stop = true;
                }
            }
            Ok(_) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }
    kill_child(listen);
    assert!(saw_start, "expected a synthesized SubagentStart hook");
    assert!(saw_stop, "expected a synthesized SubagentStop hook");
    kill_slopd(slopd);
}
