use libsloptest::{build_bin, cargo_bin, kill_slopd, tempfile, TestEnv};
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn slopctl_version_contains_commit_hash() {
    build_bin("slopctl");

    let output = Command::new(cargo_bin("slopctl"))
        .arg("--version")
        .output()
        .expect("failed to run slopctl --version");

    assert!(output.status.success(), "slopctl --version failed: {:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let has_hash = stdout.split(|c: char| !c.is_ascii_hexdigit()).any(|tok| tok.len() >= 7);
    assert!(has_hash, "no commit hash found in slopctl --version output: {:?}", stdout.trim());
}

#[test]
fn slopd_version_contains_commit_hash() {
    build_bin("slopd");

    let output = Command::new(cargo_bin("slopd"))
        .arg("--version")
        .output()
        .expect("failed to run slopd --version");

    assert!(output.status.success(), "slopd --version failed: {:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let has_hash = stdout.split(|c: char| !c.is_ascii_hexdigit()).any(|tok| tok.len() >= 7);
    assert!(has_hash, "no commit hash found in slopd --version output: {:?}", stdout.trim());
}

#[test]
fn ps_json_returns_valid_json_array() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Spawn a pane so ps has something to return
    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();

    let output = env.slopctl(&["ps", "--json"]);

    kill_slopd(slopd);

    assert!(output.status.success(), "slopctl ps --json failed: {:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let panes: Vec<serde_json::Value> = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("output is not valid JSON: {}\noutput was: {}", e, stdout));

    let pane = panes.iter().find(|p| p["pane_id"] == pane_id)
        .unwrap_or_else(|| panic!("spawned pane {} not found in ps --json output: {}", pane_id, stdout));

    assert!(pane["pane_id"].is_string(), "pane_id must be a string");
    assert!(pane["created_at"].is_number(), "created_at must be a number");
    assert!(pane["last_active"].is_number(), "last_active must be a number");
    assert!(pane["tags"].is_array(), "tags must be an array");
}

#[test]
fn ps_table_contains_expected_columns() {
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

    let output = env.slopctl(&["ps"]);

    kill_slopd(slopd);

    assert!(output.status.success(), "slopctl ps failed: {:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("PANE"), "missing PANE column: {}", stdout);
    assert!(stdout.contains("CREATED"), "missing CREATED column: {}", stdout);
    assert!(stdout.contains("LAST_ACTIVE"), "missing LAST_ACTIVE column: {}", stdout);
    assert!(stdout.contains("SESSION"), "missing SESSION column: {}", stdout);
    assert!(stdout.contains("STATE"), "missing STATE column: {}", stdout);
    assert!(stdout.contains(&pane_id), "missing pane_id in output: {}", stdout);
    assert!(stdout.contains("ago") || stdout.contains("now"), "missing time in output: {}", stdout);
}

#[test]
fn ps_json_filter_by_tag() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(Some(&["sleep", "infinity"])) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    // Spawn a pane and tag it
    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();
    assert!(!pane_id.is_empty(), "slopctl run returned empty pane_id");

    let tag_output = env.slopctl(&["tag", &pane_id, "testlabel"]);
    assert!(tag_output.status.success(), "slopctl tag failed: {:?}", tag_output);

    // ps --json --filter tag=testlabel should include our pane
    let filtered = env.slopctl(&["ps", "--json", "--filter", "tag=testlabel"]);
    assert!(filtered.status.success(), "slopctl ps --json --filter failed: {:?}", filtered);
    let stdout = String::from_utf8_lossy(&filtered.stdout);
    let panes: Vec<serde_json::Value> = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("not valid JSON: {}\noutput: {}", e, stdout));
    assert_eq!(panes.len(), 1, "expected exactly one pane with tag=testlabel, got {}", panes.len());
    assert_eq!(panes[0]["pane_id"], pane_id);
    assert!(panes[0]["created_at"].is_number(), "created_at must be a number");
    let tags = panes[0]["tags"].as_array().expect("tags must be an array");
    assert!(tags.iter().any(|t| t == "testlabel"), "tags must contain 'testlabel', got {:?}", tags);

    // ps --json --filter tag=other should return empty array
    let none = env.slopctl(&["ps", "--json", "--filter", "tag=other"]);
    assert!(none.status.success(), "slopctl ps --json --filter tag=other failed: {:?}", none);
    let stdout2 = String::from_utf8_lossy(&none.stdout);
    let panes2: Vec<serde_json::Value> = serde_json::from_str(stdout2.trim())
        .unwrap_or_else(|e| panic!("not valid JSON: {}", e));
    assert!(panes2.is_empty(), "expected empty array for unmatched filter");

    kill_slopd(slopd);
}

#[test]
fn status_with_slopd_running() {
    build_bin("slopd");

    let Some(env) = TestEnv::new(None) else {
        eprintln!("skipping: tmux not found");
        return;
    };

    let slopd = env.spawn_slopd();

    let output = env.slopctl(&["status"]);

    kill_slopd(slopd);

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

/// Test that `slopctl send --interrupt` interrupts a pane and then delivers the prompt.
/// The --interrupt flag sends Ctrl+C/D/Esc before typing the prompt; this verifies the
/// combined flow succeeds and returns the pane ID.
#[test]
fn send_interrupt_delivers_prompt_after_interrupt() {
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
    assert!(!pane_id.is_empty(), "slopctl run returned empty pane_id");

    // Wait for SessionStart so mock_claude is in its prompt-reading loop and
    // slopd has marked the pane as session-ready.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let out = env.tmux.tmux()
            .args(["show-options", "-t", &pane_id, "-p", "-v",
                   libslop::TmuxOption::SlopdClaudeSessionId.as_str()])
            .output()
            .expect("failed to run tmux show-options");
        if !String::from_utf8_lossy(&out.stdout).trim().is_empty() {
            break;
        }
        if Instant::now() > deadline {
            kill_slopd(slopd);
            panic!("timed out waiting for SessionStart on pane {}", pane_id);
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // send --interrupt should: send Ctrl+C/D/Esc to interrupt, then deliver the prompt.
    let send_output = env.slopctl(&["send", "--interrupt", &pane_id, "hello with interrupt"]);

    kill_slopd(slopd);

    assert!(send_output.status.success(), "slopctl send --interrupt failed: {:?}", send_output);
    assert_eq!(
        send_output.stdout,
        format!("{}\n", pane_id).as_bytes(),
        "expected pane_id in stdout"
    );
}
