#[path = "support/opencode_provider.rs"]
mod provider;
mod support;

use libsloptest::{build_bin, cargo_bin};
use provider::Provider;
use reqwest::Method;
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, atomic::Ordering};
use std::time::Duration;
use support::{Process, Sandbox, run, wait, wait_for};

struct Events {
    process: Option<Process>,
    reader: Option<std::thread::JoinHandle<()>>,
    records: Arc<Mutex<Vec<Value>>>,
}

impl Events {
    fn start(mut command: Command) -> Self {
        let mut child = command.stdout(Stdio::piped()).spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let records: Arc<Mutex<Vec<Value>>> = Arc::default();
        let events = records.clone();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let record = serde_json::from_str(&line).unwrap();
                events.lock().unwrap().push(record);
            }
        });
        Self {
            process: Some(Process(child)),
            reader: Some(reader),
            records,
        }
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        self.process.take();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

struct Live {
    sandbox: Sandbox,
    provider: Provider,
    daemon: Option<Process>,
    listener: Option<Events>,
    pane: String,
    port: u16,
    token: String,
    v2: bool,
}

impl Live {
    fn new(binary: &str, v2: bool) -> Self {
        let mut sandbox = Sandbox::new("slopd-opencode-live-", false);
        let provider = Provider::new(sandbox.root.path().to_owned(), v2);
        for (key, value) in [
            ("OPENCODE_CONFIG", sandbox.path("opencode.json")),
            ("OPENCODE_CONFIG_DIR", sandbox.path("config/opencode")),
            ("OPENCODE_TEST_HOME", sandbox.root.path().to_owned()),
            ("OPENCODE_TEST_MANAGED_CONFIG_DIR", sandbox.path("managed")),
        ] {
            sandbox.env.insert(key.into(), value.into_os_string());
        }
        for (key, value) in [
            ("OPENCODE_DISABLE_PROJECT_CONFIG", "true"),
            ("OPENCODE_DISABLE_DEFAULT_PLUGINS", "true"),
            ("OPENCODE_SERVER_PASSWORD", ""),
            ("OPENCODE_PASSWORD", ""),
            ("OPENCODE_DISABLE_MODELS_FETCH", "true"),
            ("OPENCODE_DISABLE_CLAUDE_CODE", "true"),
        ] {
            sandbox.env.insert(key.into(), value.into());
        }
        fs::write(sandbox.path("opencode.json"), json!({
            "autoupdate":false,"plugin":[],"permission": if v2 { json!({"*":"allow","shell":"ask"}) } else { json!("allow") },
            "enabled_providers":["slopd-test"],"model":"slopd-test/test","small_model":"slopd-test/test",
            "provider":{"slopd-test":{"npm":"@ai-sdk/openai-compatible","name":"Slopd test",
                "options":{"baseURL":format!("http://127.0.0.1:{}/v1", provider.port),"apiKey":"local-test"},
                "models":{"test":{"name":"Test","limit":{"context":32768,"output":1024}}}}}
        }).to_string()).unwrap();
        fs::write(sandbox.path("slopd.toml"), format!(
            "config_dir = {}\ndefault_account = \"oc\"\n[tmux]\nsocket = {}\n[run]\nslopctl = {}\nstart_directory = {}\n[accounts.oc]\nbackend = \"opencode\"\nexecutable = {}\n[backup]\nauto_backup = false\nauto_restore = false\n",
            json!(sandbox.path("claude")), json!(sandbox.path("tmux.sock")), json!(cargo_bin("slopctl")),
            json!(sandbox.root.path()), if v2 { json!(["env", binary]) } else { json!(binary) })).unwrap();
        Self {
            sandbox,
            provider,
            daemon: None,
            listener: None,
            pane: String::new(),
            port: 0,
            token: String::new(),
            v2,
        }
    }

    fn cli(&self, args: &[&str]) -> String {
        run(self.sandbox.cli().args(args))
    }
    fn tmux(&self, args: &[&str]) -> String {
        self.sandbox.tmux(args)
    }
    fn rows(&self) -> Vec<Value> {
        serde_json::from_str(&self.cli(&["ps", "--json"])).unwrap()
    }
    fn info(&self, pane: &str) -> Value {
        self.rows()
            .into_iter()
            .find(|p| p["pane_id"] == pane)
            .expect("managed pane")
    }
    fn session(&self, pane: &str) -> String {
        self.info(pane)["session_id"].as_str().unwrap().to_owned()
    }
    fn screen(&self, pane: &str) -> String {
        self.tmux(&["capture-pane", "-p", "-t", pane])
    }
    fn transcript(&self, pane: &str) -> String {
        self.cli(&["transcript", pane])
    }
    fn api(&self, path: &str, body: Option<Value>, method: Method) -> Value {
        self.provider
            .api(self.port, &self.token, self.v2, path, body, method)
    }
    fn option(&self, pane: &str, key: &str) -> String {
        self.tmux(&["show-options", "-pqv", "-t", pane, key])
    }
    fn bind_api(&mut self) {
        self.port = self
            .option(&self.pane, "@slopd_opencode_port")
            .parse()
            .unwrap();
        self.token = self.option(&self.pane, "@slopd_opencode_token");
    }
    fn log_file(&self) -> File {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.sandbox.path("slopd.log"))
            .unwrap()
    }

    fn start_daemon(&mut self) {
        self.daemon.take();
        let process = self
            .sandbox
            .command(cargo_bin("slopd"))
            .arg("--config")
            .arg(self.sandbox.path("slopd.toml"))
            .arg("--socket")
            .arg(self.sandbox.path("slopd.sock"))
            .arg("-vv")
            .stdout(self.log_file())
            .stderr(self.log_file())
            .spawn()
            .unwrap();
        self.daemon = Some(Process(process));
        wait("slopd socket", 30, || {
            assert!(
                self.daemon
                    .as_mut()
                    .unwrap()
                    .0
                    .try_wait()
                    .unwrap()
                    .is_none(),
                "slopd exited"
            );
            UnixStream::connect(self.sandbox.path("slopd.sock")).is_ok()
        });
    }
    fn listen(&mut self) {
        self.listener.take();
        let mut command = self.sandbox.cli();
        command.arg("listen").stderr(self.log_file());
        self.listener = Some(Events::start(command));
        wait("event subscription", 10, || {
            self.event(|event| event["subscribed"] == true)
        });
    }
    fn event(&self, check: impl FnMut(&Value) -> bool) -> bool {
        self.listener
            .as_ref()
            .unwrap()
            .records
            .lock()
            .unwrap()
            .iter()
            .any(check)
    }
    fn restored(&self, label: &str) -> Vec<Value> {
        wait_for(label, 60, || {
            let rows = self.rows();
            (rows.len() == 2 && rows.iter().all(|row| row["state"] == "ready")).then_some(rows)
        })
    }
    fn check_histories(&self, rows: &[Value], active: &str, fork: &str, check_tui: bool) {
        let mut ids: Vec<_> = rows
            .iter()
            .map(|row| row["session_id"].as_str().unwrap())
            .collect();
        ids.sort();
        let mut expected = [active, fork];
        expected.sort();
        assert_eq!(ids, expected);
        for row in rows {
            let pane = row["pane_id"].as_str().unwrap();
            let transcript = self.transcript(pane);
            assert!(transcript.contains("SLOPD_LIVE_DONE"));
            assert_eq!(
                transcript.contains("SLOPD_MULTILINE"),
                row["session_id"] == fork
            );
            if check_tui {
                wait("restored TUI", 60, || self.screen(pane).contains("Test"));
                assert!(!self.screen(pane).contains("SLOPD_SAVED_TAB"));
            }
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let screen = support::output(
                self.sandbox
                    .command("tmux")
                    .arg("-S")
                    .arg(self.sandbox.path("tmux.sock"))
                    .args(["capture-pane", "-p", "-t", &self.pane]),
            );
            eprintln!("TUI:\n{}", String::from_utf8_lossy(&screen.stdout));
            eprintln!(
                "Daemon log:\n{}",
                fs::read_to_string(self.sandbox.path("slopd.log")).unwrap_or_default()
            );
            eprintln!(
                "Provider requests: {}",
                self.provider.requests.lock().unwrap().len()
            );
            if let Some(events) = &self.listener {
                eprintln!("Events: {:?}", events.records.lock().unwrap());
            }
        }
        let _ = fs::write(self.sandbox.path("release-tool"), "");
        let _ = fs::write(self.sandbox.path("interrupt-tool"), "");
        self.listener.take();
        self.daemon.take();
    }
}

fn find_tabs(directory: &Path, session: &str) -> Option<PathBuf> {
    for entry in fs::read_dir(directory).ok()? {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if let Some(found) = find_tabs(&path, session) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|name| name == "tabs.json")
            && fs::read_to_string(&path).unwrap().contains(session)
        {
            return Some(path);
        }
    }
    None
}

/// Real TUI + embedded API + shell tool, with a deterministic local model.
/// Run explicitly with OPENCODE_TEST_BINARY=/path/to/opencode and --ignored.
#[test]
#[ignore = "requires a real OpenCode binary"]
fn opencode_live_native_session_switch() {
    build_bin("slopd");
    build_bin("slopctl");
    let binary = std::env::var("OPENCODE_TEST_BINARY").expect("OPENCODE_TEST_BINARY required");
    let version = run(Command::new(&binary).arg("--version"));
    let v2 = version
        .split_whitespace()
        .any(|word| word.trim_start_matches('v').starts_with("2."));
    println!("Real OpenCode: {version}");
    let mut live = Live::new(&binary, v2);
    live.tmux(&[
        "-f",
        "/dev/null",
        "new-session",
        "-d",
        "-s",
        "test",
        "-x",
        "160",
        "-y",
        "50",
    ]);
    live.start_daemon();
    let cli_config = live.sandbox.path("config/opencode/cli.json");
    let saved_config = "{\"animations\":false,\"session\":{\"sidebar\":\"show\"}}\n";
    let tabs_on = "OPENCODE_CLI_CONFIG_CONTENT={\"tabs\":{\"mode\":\"on\"}}";
    let saved_tabs = if v2 {
        fs::create_dir_all(cli_config.parent().unwrap()).unwrap();
        fs::write(&cli_config, saved_config).unwrap();
        live.pane = live.cli(&["run", "--ready-timeout", "60", "--env", tabs_on]);
        live.bind_api();
        let seed = live.session(&live.pane);
        live.api(
            &format!("/session/{seed}"),
            Some(json!({"title":"SLOPD_SAVED_TAB"})),
            Method::PATCH,
        );
        wait("saved tab in seed TUI", 60, || {
            live.screen(&live.pane).contains("SLOPD_SAVED_TAB")
        });
        let path = wait_for("persisted seed tab", 30, || {
            find_tabs(&live.sandbox.path("state"), &seed)
        });
        live.cli(&["kill", &live.pane]);
        let contents = fs::read(&path).unwrap();
        Some((path, contents))
    } else {
        None
    };

    live.pane = if v2 {
        live.cli(&["run", "--no-wait", "--env", "OPENCODE_CLI_CONFIG_CONTENT={\"session\":{\"sidebar\":\"hide\"},\"tabs\":{\"layout\":\"horizontal\"}}"])
    } else {
        live.cli(&["run", "--ready-timeout", "60"])
    };
    // Send immediately: API readiness can precede the terminal composer.
    live.cli(&[
        "send",
        &live.pane,
        "SLOPD_COLD_START: reply once without tools.",
    ]);
    wait("cold-start prompt reaching provider", 30, || {
        live.provider
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.to_string().contains("SLOPD_COLD_START"))
    });
    wait("cold-start completion", 30, || {
        live.info(&live.pane)["state"] == "ready"
    });
    let history: Value = serde_json::from_str(&live.transcript(&live.pane)).unwrap();
    assert_eq!(
        history["records"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["event_type"] == "user" && r.to_string().contains("SLOPD_COLD_START"))
            .count(),
        1
    );
    assert!(
        !json!(*live.provider.requests.lock().unwrap())
            .to_string()
            .contains("slopd-ready-"),
        "readiness probe was submitted"
    );
    println!("Immediate cold-start prompt delivered once without readiness-probe leakage");
    live.bind_api();
    let initial = live.session(&live.pane);
    live.listen();
    assert_eq!(
        live.listener.as_ref().unwrap().records.lock().unwrap()[0]["subscribed"],
        true,
        "first listener record must acknowledge subscription"
    );
    wait("TUI model display", 60, || {
        live.screen(&live.pane).contains("Test")
    });
    std::thread::sleep(Duration::from_secs(3));
    if let Some((path, contents)) = saved_tabs {
        let screen = live.screen(&live.pane);
        assert!(!screen.contains("SLOPD_SAVED_TAB"), "{screen}");
        assert!(
            !screen.contains("Context"),
            "existing sidebar override was discarded"
        );
        assert_eq!(
            fs::read(path).unwrap(),
            contents,
            "tabs-off launch changed saved tabs"
        );
        assert_eq!(fs::read_to_string(cli_config).unwrap(), saved_config);
        let probe = live.cli(&["run", "--ready-timeout", "60", "--env", tabs_on]);
        wait("saved tab restored by explicit opt-in", 60, || {
            live.screen(&probe).contains("SLOPD_SAVED_TAB")
        });
        live.cli(&["kill", &probe]);
        println!(
            "Saved tabs hidden by default; explicit opt-in restores them; CLI overrides and saved files preserved"
        );
    }
    // Native navigation creates a session without a tui.session.select event.
    live.tmux(&["send-keys", "-t", &live.pane, "C-x", "n"]);
    std::thread::sleep(Duration::from_millis(500));
    live.cli(&[
        "send",
        &live.pane,
        "SLOPD_LIVE_PROMPT: run the controlled bash tool.",
    ]);
    wait("model tool request", 45, || {
        live.provider.tool_started.load(Ordering::SeqCst)
    });
    let active = wait_for("active OpenCode session", 30, || {
        live.api("/session/status", None, Method::GET)
            .as_object()
            .unwrap()
            .iter()
            .find(|(_, status)| status["type"] == "busy" || status["type"] == "running")
            .map(|(id, _)| id.clone())
    });
    assert_ne!(
        active, initial,
        "native navigation did not create a different session"
    );
    wait("binding to native TUI session", 10, || {
        live.session(&live.pane) == active
    });
    if v2 {
        let permission = wait_for("permission request", 30, || {
            live.api(&format!("/session/{active}/permission"), None, Method::GET)
                .as_array()
                .unwrap()
                .first()
                .cloned()
        });
        wait("permission state", 30, || {
            live.info(&live.pane)["detailed_state"] == "awaiting_input_permission"
        });
        std::thread::sleep(Duration::from_secs(4));
        assert_eq!(
            live.info(&live.pane)["detailed_state"],
            "awaiting_input_permission"
        );
        live.api(
            &format!(
                "/session/{active}/permission/{}/reply",
                permission["id"].as_str().unwrap()
            ),
            Some(json!({"decision":"always"})),
            Method::POST,
        );
        println!("Permission prompt remains visible across status polling");
    }
    wait("live tool hook", 30, || {
        live.event(|e| e["event_type"] == "PreToolUse")
    });
    wait("busy state event", 30, || {
        live.event(|e| e["event_type"] == "StateChange" && e["payload"]["state"] == "busy")
    });
    wait("tool state event", 30, || {
        live.event(|e| {
            e["event_type"] == "DetailedStateChange"
                && e["payload"]["detailed_state"] == "busy_tool_use"
        })
    });
    wait("live user transcript", 30, || {
        live.event(|e| e["source"] == "transcript" && e.to_string().contains("SLOPD_LIVE_PROMPT"))
    });
    wait("running tool after permission reply", 30, || {
        live.info(&live.pane)["state"] == "busy"
    });
    assert!(live.transcript(&live.pane).contains("SLOPD_LIVE_PROMPT"));
    println!("Native session switch recovered; busy/tool/transcript events observed");

    // Recover a stale durable binding while the original TUI tool remains busy.
    live.listener.take();
    live.daemon.take();
    live.tmux(&[
        "set-option",
        "-p",
        "-t",
        &live.pane,
        "@slopd_session_id",
        &initial,
    ]);
    live.start_daemon();
    wait("recovery from stale persisted binding", 15, || {
        live.session(&live.pane) == active && live.info(&live.pane)["state"] == "busy"
    });
    live.listen();
    let hold: f64 = std::env::var("OPENCODE_TEST_TOOL_HOLD_SECONDS")
        .unwrap_or_else(|_| "31".into())
        .parse()
        .unwrap();
    std::thread::sleep(Duration::from_secs_f64(hold));
    assert_eq!(live.info(&live.pane)["state"], "busy");
    if v2 {
        assert_eq!(live.info(&live.pane)["detailed_state"], "busy_tool_use");
    }
    fs::write(live.sandbox.path("release-tool"), "").unwrap();
    wait("idle after real shell tool", 30, || {
        live.info(&live.pane)["state"] == "ready"
    });
    let transcript = live.transcript(&live.pane);
    assert!(
        transcript.contains("SLOPD_TOOL_DONE") && transcript.contains("SLOPD_LIVE_DONE"),
        "{transcript}"
    );
    wait("live transcript after recovered binding", 30, || {
        live.event(|e| e["source"] == "transcript" && e.to_string().contains("SLOPD_LIVE_DONE"))
    });
    assert!(live.event(|e| e["event_type"] == "PostToolUse"));
    assert!(live.event(|e| e["event_type"] == "Stop"));
    wait("idle state event", 30, || {
        live.event(|e| e["event_type"] == "StateChange" && e["payload"]["state"] == "ready")
    });
    assert_eq!(live.option(&live.pane, "@slopd_session_id"), active);
    assert!(
        fs::read_to_string(live.sandbox.path("slopd.log"))
            .unwrap()
            .contains("recovered stale session binding")
    );
    let tool = if v2 { "shell" } else { "bash" };
    assert!(
        live.event(
            |e| e["event_type"] == "PostToolUse" && e["payload"]["extra"]["tool_name"] == tool
        )
    );
    println!("Stale-binding restart recovered; tool held {hold}s and state returned to ready");

    let fork = live.cli(&["fork", &live.pane, "--ready-timeout", "60"]);
    let fork_session = live.session(&fork);
    assert_ne!(fork_session, active);
    assert!(live.transcript(&fork).contains("SLOPD_LIVE_DONE"));
    wait("fork TUI", 30, || live.screen(&fork).contains("Test"));
    if v2 {
        assert!(!live.screen(&fork).contains("SLOPD_SAVED_TAB"));
    }
    std::thread::sleep(Duration::from_secs(1));
    live.tmux(&["send-keys", "-t", &fork, "-l", "DISCARD_STALE_COMPOSER"]);
    let prompt = format!(
        "SLOPD_MULTILINE\n{}\nEND_MULTILINE",
        "quoted `literal` $value 'text' ".repeat(800)
    );
    live.cli(&["send", &fork, &prompt]);
    wait("large multiline prompt", 30, || {
        live.transcript(&fork).contains("END_MULTILINE")
    });
    wait("fork completion", 30, || {
        live.info(&fork)["state"] == "ready"
    });
    assert!(!live.transcript(&fork).contains("DISCARD_STALE_COMPOSER"));
    assert!(
        !live.transcript(&live.pane).contains("SLOPD_MULTILINE"),
        "fork changed source conversation"
    );
    live.provider.tool_started.store(false, Ordering::SeqCst);
    live.cli(&[
        "send",
        &fork,
        "SLOPD_INTERRUPT: run the controlled tool until interrupted.",
    ]);
    wait("interrupt tool request", 30, || {
        live.provider.tool_started.load(Ordering::SeqCst)
    });
    wait("busy before interrupt", 30, || {
        live.info(&fork)["state"] == "busy"
    });
    live.cli(&["interrupt", &fork]);
    wait("ready after interrupt", 30, || {
        live.info(&fork)["state"] == "ready"
    });
    println!("Fork history, independent multiline input, and API interrupt passed");

    // A full server loss must recover both histories in new TUI processes.
    live.cli(&["backup"]);
    live.listener.take();
    let old_ports: Vec<u16> = [&live.pane, &fork]
        .into_iter()
        .map(|pane| live.option(pane, "@slopd_opencode_port").parse().unwrap())
        .collect();
    live.tmux(&["kill-server"]);
    wait("private servers stopped with tmux", 15, || {
        old_ports.iter().all(|port| {
            let address = SocketAddr::from(([127, 0, 0, 1], *port));
            TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_err()
        })
    });
    wait("old panes gone", 30, || live.rows().is_empty());
    live.cli(&["restore"]);
    let rows = live.restored("restored sessions");
    live.check_histories(&rows, &active, &fork_session, v2);
    println!("Full tmux-server loss restored both exact session histories");
    if let Ok(upgrade) = std::env::var("OPENCODE_TEST_UPGRADE_BINARY")
        && !v2
    {
        live.cli(&["backup"]);
        live.tmux(&["kill-server"]);
        wait("v1 panes gone before upgrade", 30, || {
            live.rows().is_empty()
        });
        live.daemon.take();
        let path = live.sandbox.path("slopd.toml");
        let config = fs::read_to_string(&path).unwrap().replace(
            &format!("executable = {}", json!(binary)),
            &format!("executable = {}", json!(upgrade)),
        );
        fs::write(path, config).unwrap();
        live.tmux(&[
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "test",
            "-x",
            "160",
            "-y",
            "50",
        ]);
        live.start_daemon();
        live.cli(&["restore"]);
        let rows = live.restored("v1 histories restored through v2 migration");
        live.check_histories(&rows, &active, &fork_session, false);
        println!("Upgrade to OpenCode 2 preserved both v1 session identities and histories");
    }
}
