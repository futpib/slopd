use libsloptest::{build_bin, cargo_bin, kill_child, kill_slopd, TestEnv};
use std::process::{Command, Stdio};
use std::time::Duration;

fn tmux_available() -> bool {
    match Command::new("tmux").arg("-V").status() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => panic!("unexpected error checking for tmux: {}", e),
        Ok(_) => true,
    }
}

/// Helper: start iroh-slopd with a given config dir, pointing at a slopd runtime dir.
/// Returns the child process and the path to the addr file (containing full EndpointAddr JSON).
fn start_iroh_slopd(
    runtime_dir: &std::path::Path,
    iroh_config_dir: &std::path::Path,
) -> (std::process::Child, std::path::PathBuf) {
    let addr_file = iroh_config_dir.join("iroh-slopd-addr.json");

    // Start the server with --addr-file so we can get its full address.
    let child = Command::new(cargo_bin("iroh-slopd"))
        .args(["--addr-file", addr_file.to_str().unwrap()])
        .env("XDG_CONFIG_HOME", iroh_config_dir)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn iroh-slopd");

    // Wait for the addr file to appear and be non-empty.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(contents) = std::fs::read_to_string(&addr_file) {
            if !contents.is_empty() {
                break;
            }
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for iroh-slopd addr file at {}", addr_file.display());
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    (child, addr_file)
}

/// Get iroh-slopctl's endpoint ID.
fn iroh_slopctl_info(iroh_slopctl_config_dir: &std::path::Path) -> String {
    let output = Command::new(cargo_bin("iroh-slopctl"))
        .args(["info"])
        .env("XDG_CONFIG_HOME", iroh_slopctl_config_dir)
        .output()
        .expect("failed to run iroh-slopctl info");
    assert!(output.status.success(), "iroh-slopctl info failed: {:?}", output);
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Authorize a client endpoint ID on the iroh-slopd server.
fn iroh_slopd_authorize(iroh_config_dir: &std::path::Path, client_endpoint_id: &str) {
    let output = Command::new(cargo_bin("iroh-slopd"))
        .args(["authorize", client_endpoint_id])
        .env("XDG_CONFIG_HOME", iroh_config_dir)
        .output()
        .expect("failed to run iroh-slopd authorize");
    assert!(output.status.success(), "iroh-slopd authorize failed: {:?}", output);
}

/// Run iroh-slopctl with given args, config, and addr-file.
fn iroh_slopctl(
    iroh_slopctl_config_dir: &std::path::Path,
    addr_file: &std::path::Path,
    args: &[&str],
) -> std::process::Output {
    Command::new(cargo_bin("iroh-slopctl"))
        .args(["--addr-file", addr_file.to_str().unwrap()])
        .args(args)
        .env("XDG_CONFIG_HOME", iroh_slopctl_config_dir)
        .output()
        .expect("failed to run iroh-slopctl")
}

#[test]
fn iroh_e2e_unauthorized_client_is_rejected() {
    if !tmux_available() {
        eprintln!("skipping: tmux not available");
        return;
    }

    build_bin("slopd");
    build_bin("slopctl");
    build_bin("iroh-slopd");
    build_bin("iroh-slopctl");

    let mock_claude = cargo_bin("mock_claude");
    let mock_str = mock_claude.to_str().unwrap();
    let env = TestEnv::new(Some(&[mock_str])).unwrap();
    let slopd = env.spawn_slopd();

    let iroh_server_config_dir = libsloptest::tempfile::tempdir().unwrap();
    let iroh_client_config_dir = libsloptest::tempfile::tempdir().unwrap();

    // Start iroh-slopd WITHOUT authorizing the client.
    let (iroh_slopd, addr_file) = start_iroh_slopd(
        env.runtime_dir.path(),
        iroh_server_config_dir.path(),
    );

    // Ensure the client has a key generated.
    let _client_id = iroh_slopctl_info(iroh_client_config_dir.path());

    // Try to run a command — should fail because we're not authorized.
    let output = iroh_slopctl(iroh_client_config_dir.path(), &addr_file, &["ps", "--json"]);
    assert!(
        !output.status.success(),
        "iroh-slopctl should have failed without authorization, but succeeded: {:?}",
        output,
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("iroh-slopd authorize"),
        "stderr should hint about `iroh-slopd authorize`, got: {}",
        stderr,
    );

    kill_child(iroh_slopd);
    kill_slopd(slopd);
}

#[test]
fn iroh_e2e_authorized_client_can_list_panes() {
    if !tmux_available() {
        eprintln!("skipping: tmux not available");
        return;
    }

    build_bin("slopd");
    build_bin("slopctl");
    build_bin("iroh-slopd");
    build_bin("iroh-slopctl");

    let mock_claude = cargo_bin("mock_claude");
    let mock_str = mock_claude.to_str().unwrap();
    let env = TestEnv::new(Some(&[mock_str])).unwrap();
    let slopd = env.spawn_slopd();

    let iroh_server_config_dir = libsloptest::tempfile::tempdir().unwrap();
    let iroh_client_config_dir = libsloptest::tempfile::tempdir().unwrap();

    // Get client's endpoint ID (this also generates its key).
    let client_endpoint_id = iroh_slopctl_info(iroh_client_config_dir.path());

    // Authorize the client on the server.
    iroh_slopd_authorize(iroh_server_config_dir.path(), &client_endpoint_id);

    // Start iroh-slopd.
    let (iroh_slopd, addr_file) = start_iroh_slopd(
        env.runtime_dir.path(),
        iroh_server_config_dir.path(),
    );

    // Spawn a pane via local slopctl so there's something to list.
    let run_output = env.slopctl(&["run"]);
    assert!(run_output.status.success(), "slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();
    assert!(pane_id.starts_with('%'), "expected pane_id, got: {}", pane_id);

    // Use iroh-slopctl to list panes remotely.
    let output = iroh_slopctl(iroh_client_config_dir.path(), &addr_file, &["ps", "--json"]);

    kill_child(iroh_slopd);
    kill_slopd(slopd);

    assert!(output.status.success(), "iroh-slopctl ps --json failed: {:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let panes: Vec<serde_json::Value> = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("failed to parse ps --json output: {} -- output: {}", e, stdout));
    assert!(
        panes.iter().any(|p| p["pane_id"].as_str() == Some(&pane_id)),
        "expected pane {} in remote ps output: {:?}",
        pane_id, panes,
    );
}

#[test]
fn iroh_e2e_run_and_kill_via_iroh() {
    if !tmux_available() {
        eprintln!("skipping: tmux not available");
        return;
    }

    build_bin("slopd");
    build_bin("slopctl");
    build_bin("iroh-slopd");
    build_bin("iroh-slopctl");

    let mock_claude = cargo_bin("mock_claude");
    let mock_str = mock_claude.to_str().unwrap();
    let env = TestEnv::new(Some(&[mock_str])).unwrap();
    let slopd = env.spawn_slopd();

    let iroh_server_config_dir = libsloptest::tempfile::tempdir().unwrap();
    let iroh_client_config_dir = libsloptest::tempfile::tempdir().unwrap();

    let client_endpoint_id = iroh_slopctl_info(iroh_client_config_dir.path());
    iroh_slopd_authorize(iroh_server_config_dir.path(), &client_endpoint_id);

    let (iroh_slopd, addr_file) = start_iroh_slopd(
        env.runtime_dir.path(),
        iroh_server_config_dir.path(),
    );

    // Run a pane remotely via iroh-slopctl.
    let run_output = iroh_slopctl(iroh_client_config_dir.path(), &addr_file, &["run"]);
    assert!(run_output.status.success(), "iroh-slopctl run failed: {:?}", run_output);
    let pane_id = String::from_utf8_lossy(&run_output.stdout).trim().to_string();
    assert!(pane_id.starts_with('%'), "expected pane_id, got: {}", pane_id);

    // Kill it remotely.
    let kill_output = iroh_slopctl(iroh_client_config_dir.path(), &addr_file, &["kill", &pane_id]);
    assert!(kill_output.status.success(), "iroh-slopctl kill failed: {:?}", kill_output);
    let kill_stdout = String::from_utf8_lossy(&kill_output.stdout);
    assert_eq!(kill_stdout.trim(), pane_id, "kill should print the pane_id");

    // Verify pane is gone.
    let ps_output = iroh_slopctl(iroh_client_config_dir.path(), &addr_file, &["ps", "--json"]);
    assert!(ps_output.status.success());
    let stdout = String::from_utf8_lossy(&ps_output.stdout);
    let panes: Vec<serde_json::Value> = serde_json::from_str(&stdout).unwrap();
    assert!(
        !panes.iter().any(|p| p["pane_id"].as_str() == Some(&pane_id)),
        "pane {} should be gone after kill, but still in ps output",
        pane_id,
    );

    kill_child(iroh_slopd);
    kill_slopd(slopd);
}
