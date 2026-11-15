mod support;

use libsloptest::{build_bin, cargo_bin};
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use support::{Sandbox, output, run, wait};

fn ctl(args: &[&str]) -> String {
    run(Command::new("systemctl").arg("--user").args(args))
}

fn property(unit: &str, name: &str) -> String {
    ctl(&["show", unit, "-p", name, "--value"])
}

fn process_identity(pid: &str) -> Option<(String, String)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<_> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    (fields[0] != "Z").then(|| (pid.to_owned(), fields[19].to_owned()))
}

struct Units {
    daemon: String,
    owner: String,
    directory: PathBuf,
}

impl Drop for Units {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let log = output(Command::new("journalctl").args([
                "--user",
                "-u",
                &self.daemon,
                "-u",
                &self.owner,
                "--no-pager",
                "-n",
                "100",
            ]));
            eprintln!("{}", String::from_utf8_lossy(&log.stdout));
        }
        for unit in [&self.daemon, &self.owner] {
            let _ = output(Command::new("systemctl").args(["--user", "stop", unit]));
            let _ = fs::remove_file(self.directory.join(unit));
        }
        let _ = output(Command::new("systemctl").args(["--user", "daemon-reload"]));
        for unit in [&self.daemon, &self.owner] {
            let _ = output(Command::new("systemctl").args(["--user", "reset-failed", unit]));
        }
    }
}

fn exercise(with_companion: bool) {
    let sandbox = Sandbox::new("slopd-restart-test-", true);
    let prefix = sandbox.root.path().file_name().unwrap().to_str().unwrap();
    // Units drop before the private server and temporary directory.
    let units = Units {
        daemon: format!("{prefix}.service"),
        owner: format!("{prefix}-tmux.service"),
        directory: PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap()).join("systemd/user"),
    };
    fs::create_dir_all(&units.directory).unwrap();
    fs::write(sandbox.path("slopd.toml"), format!(
        "config_dir = {}\n[tmux]\nsocket = {}\nstart_server = true\n[run]\nslopctl = {}\nexecutable = {}\nstart_directory = {}\n[backup]\nauto_backup = false\nauto_restore = false\n",
        json!(sandbox.path("claude")), json!(sandbox.path("tmux.sock")), json!(cargo_bin("slopctl")),
        json!(cargo_bin("mock_claude")), json!(sandbox.root.path()))).unwrap();
    let mut service =
        include_str!("../../slopd.service").replace("slopd-tmux.service", &units.owner);
    if !with_companion {
        // Keep KillMode=process but reproduce the old daemon-owned server.
        service = service
            .replace(&format!("Requires={}\n", units.owner), "")
            .replace(&format!("After={}\n", units.owner), "");
    }
    service = service.replace(
        "ExecStart=%h/.cargo/bin/slopd",
        &format!(
            "ExecStart={} --config {} --socket {} -vv",
            json!(cargo_bin("slopd")),
            json!(sandbox.path("slopd.toml")),
            json!(sandbox.path("slopd.sock"))
        ),
    );
    service.push_str(&format!(
        "\n[Service]\nWorkingDirectory={}\nUnsetEnvironment=TMUX TMUX_PANE\n",
        sandbox.root.path().display()
    ));
    // Keep the real runtime directory so tmux can reach the user systemd bus.
    for key in [
        "PATH",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
    ] {
        service.push_str(&format!(
            "Environment={}\n",
            json!(format!(
                "{key}={}",
                sandbox.env[std::ffi::OsStr::new(key)].to_str().unwrap()
            ))
        ));
    }
    let companion = include_str!("../../slopd-tmux.service").replace(
        "/usr/bin/tmux start-server",
        &format!(
            "{} -S {} -f /dev/null start-server",
            json!(sandbox.real_tmux),
            json!(sandbox.path("tmux.sock"))
        ),
    );
    fs::write(units.directory.join(&units.daemon), service).unwrap();
    fs::write(units.directory.join(&units.owner), companion).unwrap();
    ctl(&["daemon-reload"]);
    ctl(&["start", &units.daemon]);
    let ready = || {
        assert_eq!(
            property(&units.daemon, "ActiveState"),
            "active",
            "daemon exited"
        );
        output(sandbox.cli().arg("status")).status.success()
    };
    wait("control socket", 20, ready);
    let pane = run(sandbox.cli().args(["run", "--no-wait"]));
    let info = || -> Option<Value> {
        let panes: Vec<Value> =
            serde_json::from_str(&run(sandbox.cli().args(["ps", "--json"]))).unwrap();
        panes.into_iter().find(|p| p["pane_id"] == pane)
    };
    wait("agent session binding", 20, || {
        info().is_some_and(|p| p["session_id"].is_string() && p["state"] == "ready")
    });
    let identity = || {
        let values = sandbox.tmux(&[
            "display-message",
            "-p",
            "-t",
            &pane,
            "#{pid} #{pane_pid} #{pane_dead}",
        ]);
        let values: Vec<_> = values.split_whitespace().collect();
        assert_eq!(values[2], "0", "agent pane died");
        [
            process_identity(values[0]).expect("server exited"),
            process_identity(values[1]).expect("agent exited"),
        ]
    };
    let binding = || {
        let row = info().unwrap();
        ["session_id", "transcript_path", "backend", "account"].map(|key| row[key].clone())
    };
    let exchange_prompt = |label: &str| {
        let prompt = format!("restart-regression-{label}");
        run(sandbox
            .cli()
            .args(["send", "--timeout", "10", &pane, &prompt]));
        let path = PathBuf::from(info().unwrap()["transcript_path"].as_str().unwrap());
        wait(&format!("response after {label}"), 20, || {
            fs::read_to_string(&path)
                .unwrap()
                .contains(&format!("mock response to: {prompt}"))
        });
        wait("ready after response", 20, || {
            info().unwrap()["state"] == "ready"
        });
    };
    let original = identity();
    let saved = binding();
    let scope = ctl(&["whoami", &original[1].0]);
    assert!(
        scope.starts_with("tmux-spawn-"),
        "tmux must support systemd pane scopes"
    );
    assert_eq!(
        property(&scope, "PartOf"),
        if with_companion {
            &units.owner
        } else {
            &units.daemon
        }
        .as_str()
    );
    exchange_prompt("startup");
    if !with_companion {
        ctl(&["stop", &units.daemon]);
        wait("original pane death", 20, || {
            process_identity(&original[1].0).is_none()
        });
        assert_eq!(
            process_identity(&original[0].0).as_ref(),
            Some(&original[0]),
            "tmux itself should survive"
        );
        println!("Reproduced: KillMode=process kept tmux alive but its agent scope died.");
        return;
    }
    assert_eq!(property(&units.owner, "MainPID"), original[0].0);
    assert_eq!(property(&units.daemon, "ConsistsOf"), "");
    let mut previous = process_identity(&property(&units.daemon, "MainPID"));
    for action in ["restart", "stop/start"] {
        if action == "restart" {
            ctl(&["restart", &units.daemon]);
        } else {
            ctl(&["stop", &units.daemon]);
            assert_eq!(identity(), original);
            assert_eq!(property(&units.owner, "ActiveState"), "active");
            ctl(&["start", &units.daemon]);
        }
        wait("restarted control socket", 20, ready);
        let current = process_identity(&property(&units.daemon, "MainPID"));
        assert!(current.is_some() && current != previous);
        previous = current;
        assert_eq!(identity(), original, "server or agent was replaced");
        wait("re-adopted pane", 20, || info().is_some());
        assert_eq!(binding(), saved);
        assert_eq!(property(&scope, "PartOf"), units.owner);
        exchange_prompt(action);
        println!("PASS {action}: same server, agent PID, session, and working prompt exchange.");
    }
}

/// Exercise the shipped units through the real user manager and tmux scopes.
#[test]
#[ignore = "requires a systemd user manager and systemd-enabled tmux"]
fn systemd_restart_preserves_agent_processes() {
    for name in ["slopd", "slopctl", "mock_claude"] {
        build_bin(name);
    }
    exercise(false);
    exercise(true);
}
