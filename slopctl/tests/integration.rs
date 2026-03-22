use libsloptest::{build_bin, cargo_bin, kill_slopd, tempfile, TestEnv};
use std::process::Command;

#[test]
fn ps_json_returns_valid_json_array() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(None) else {
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
    assert!(pane["tags"].is_array(), "tags must be an array");
}

#[test]
fn ps_json_filter_by_tag() {
    build_bin("slopd");
    build_bin("slopctl");

    let Some(env) = TestEnv::new(None) else {
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
