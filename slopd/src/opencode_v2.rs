//! OpenCode 2 process ownership and translation into the existing pane protocol.

use crate::opencode::OpencodeClient;
use serde_json::{Value, json};
use std::{collections::HashMap, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};

pub async fn detect(
    program: &Path,
    args: &[String],
    env: &[(String, String)],
    cwd: &Path,
) -> Result<bool, String> {
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new(program)
            .args(args)
            .arg("--version")
            .envs(env.iter().cloned())
            .current_dir(cwd)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "OpenCode version check timed out".to_string())?
    .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "OpenCode version check failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .any(|word| word.trim_start_matches('v').starts_with("2.")))
}

pub fn launch_args(input: &[String]) -> Result<(u16, String, Vec<String>, bool), String> {
    let mut args = Vec::new();
    let mut port = None;
    let mut session = None;
    let mut iter = input.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--port" => port = iter.next().and_then(|s| s.parse().ok()),
            "--hostname" => {
                iter.next();
            }
            "-s" | "--session" => {
                session = Some(
                    iter.next()
                        .ok_or("missing OpenCode session argument")?
                        .clone(),
                )
            }
            "--server" | "--standalone" => {
                return Err(
                    "slopd owns the OpenCode server; --server and --standalone are not supported"
                        .into(),
                );
            }
            _ if arg.starts_with("--server=") => {
                return Err("slopd owns the OpenCode server; --server is not supported".into());
            }
            _ if arg.starts_with("--session=") => session = Some(arg[10..].to_string()),
            _ => args.push(arg.clone()),
        }
    }
    let create = session.is_none();
    Ok((
        port.ok_or("missing OpenCode port")?,
        session.unwrap_or_else(|| format!("ses_{}", uuid::Uuid::now_v7().simple())),
        args,
        create,
    ))
}

/// Keep managed panes on one visible conversation by default, without dropping
/// other CLI overrides or an explicit per-launch choice to enable tabs.
fn cli_config(content: Option<&str>) -> Result<String, String> {
    let mut config: Value = match content.filter(|value| !value.trim().is_empty()) {
        Some(content) => json5::from_str(content)
            .map_err(|_| "OPENCODE_CLI_CONFIG_CONTENT must be a JSON object".to_string())?,
        None => json!({}),
    };
    let config = config
        .as_object_mut()
        .ok_or("OPENCODE_CLI_CONFIG_CONTENT must be a JSON object")?;
    let tabs = config.entry("tabs").or_insert_with(|| json!({}));
    let tabs = tabs
        .as_object_mut()
        .ok_or("OPENCODE_CLI_CONFIG_CONTENT.tabs must be an object")?;
    if !tabs.contains_key("mode") && !tabs.contains_key("enabled") {
        tabs.insert("mode".into(), json!("off"));
    }
    serde_json::to_string(config).map_err(|e| e.to_string())
}

/// A pipe leases the server to this process. Even SIGKILL or a tmux-server crash
/// closes the pipe, so the private server cannot survive its pane owner.
pub async fn run(
    port: u16,
    session: &str,
    create_session: bool,
    server_args: &[String],
    command: &[String],
) -> Result<i32, String> {
    let token = std::env::var("SLOPD_OPENCODE_TOKEN").map_err(|e| e.to_string())?;
    let program = command.first().ok_or("missing OpenCode executable")?;
    let cli_config = cli_config(std::env::var("OPENCODE_CLI_CONFIG_CONTENT").ok().as_deref())?;
    let mut server = Command::new(program)
        .args(server_args)
        .args([
            "serve",
            "--stdio",
            "--port",
            &port.to_string(),
            "--hostname",
            "127.0.0.1",
        ])
        .env_remove("SLOPD_OPENCODE_TOKEN")
        .env("OPENCODE_PASSWORD", &token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    let lease = server.stdin.take();
    let mut lines = BufReader::new(server.stdout.take().ok_or("missing server stdout")?).lines();
    let ready = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
        .await
        .map_err(|_| "private server startup timed out")?
        .map_err(|e| e.to_string())?
        .ok_or("private server exited before readiness")?;
    let ready: Value = serde_json::from_str(&ready)
        .map_err(|e| format!("invalid server readiness: {e}: {ready}"))?;
    let url = format!("http://127.0.0.1:{port}");
    if ready["url"].as_str() != Some(&url) {
        return Err(format!("unexpected private server address: {ready}"));
    }
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    let mut client = OpencodeClient::new(port, Some(token.clone()));
    client.v2 = true;
    if !create_session {
        client.wait_v2_session(session).await?;
    }
    client
        .create_v2_session(
            session,
            &std::env::current_dir()
                .map_err(|e| e.to_string())?
                .to_string_lossy(),
        )
        .await?;
    let mut tui = Command::new(program)
        .args(&command[1..])
        .args(["--server", &url, "--session", session])
        .env("OPENCODE_CLI_CONFIG_CONTENT", cli_config)
        .env_remove("SLOPD_OPENCODE_TOKEN")
        .env("OPENCODE_PASSWORD", token)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .map_err(|e| e.to_string())?;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| e.to_string())?;
    let result = tokio::select! {
        status = tui.wait() => status.map(|s| s.code().unwrap_or(1)).map_err(|e| e.to_string()),
        status = server.wait() => Err(format!("private server exited: {status:?}")),
        _ = hup.recv() => Ok(129),
        _ = term.recv() => Ok(143),
    };
    let _ = tui.kill().await;
    drop(lease);
    if tokio::time::timeout(Duration::from_secs(5), server.wait())
        .await
        .is_err()
    {
        let _ = server.kill().await;
    }
    result
}

pub fn message(value: &Value) -> Value {
    let role = value["type"].as_str().unwrap_or("assistant");
    let parts = match role {
        "assistant" => value["content"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|content| {
                let mut part = content.clone();
                if part["type"] == "tool" {
                    part["tool"] = part["name"].clone();
                    part["state"]["output"] = json!(output(&part["state"]["content"]));
                }
                part
            })
            .collect::<Vec<_>>(),
        _ => vec![
            json!({"type": "text", "text": value.get("text").or_else(|| value.get("summary")).cloned().unwrap_or(Value::Null)}),
        ],
    };
    let mut info = value.clone();
    info["role"] = json!(role);
    json!({"info": info, "parts": parts})
}

fn output(content: &Value) -> String {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Default)]
pub struct Events {
    tools: HashMap<(String, String), (String, Value)>,
    text: HashMap<(String, String, u64), String>,
}

impl Events {
    pub fn missing_tool(&self, event: &Value) -> bool {
        matches!(
            event["type"].as_str(),
            Some("session.tool.called" | "session.tool.success" | "session.tool.failed")
        ) && !self.tools.contains_key(&(
            event["data"]["sessionID"].as_str().unwrap_or("").into(),
            event["data"]["id"].as_str().unwrap_or("").into(),
        ))
    }

    pub fn seed_tools(&mut self, sid: &str, messages: &[Value]) {
        for part in messages
            .iter()
            .flat_map(|m| m["parts"].as_array().into_iter().flatten())
        {
            if part["type"] == "tool"
                && let (Some(id), Some(name)) = (part["id"].as_str(), part["tool"].as_str())
            {
                self.tools.insert(
                    (sid.into(), id.into()),
                    (name.into(), part["state"]["input"].clone()),
                );
            }
        }
    }

    /// Keep the original V2 event for diagnostics while presenting the same
    /// hook/transcript shapes as V1 to existing slopctl and Android clients.
    pub fn normalize(&mut self, event: Value) -> Option<Value> {
        let kind = event["type"].as_str()?;
        let data = &event["data"];
        let sid = data["sessionID"].as_str().unwrap_or("");
        let mut props = data.clone();
        let mapped = match kind {
            "session.tool.input.started" => {
                self.tools.insert(
                    (sid.into(), data["id"].as_str()?.into()),
                    (data["name"].as_str()?.into(), Value::Null),
                );
                return None;
            }
            "session.tool.called" | "session.tool.success" | "session.tool.failed" => {
                let key = (sid.to_string(), data["id"].as_str()?.to_string());
                let (name, input) = self
                    .tools
                    .entry(key.clone())
                    .or_insert_with(|| (String::new(), Value::Null));
                let status = match kind {
                    "session.tool.called" => {
                        *input = data["input"].clone();
                        "running"
                    }
                    "session.tool.success" => "completed",
                    _ => "error",
                };
                props["part"] = json!({"id": data["id"], "messageID": data["assistantMessageID"], "sessionID": sid,
                    "type": "tool", "tool": name, "state": {"status": status, "input": input,
                    "output": output(&data["content"]), "error": data["error"]}});
                if status != "running" {
                    self.tools.remove(&key);
                }
                "message.part.updated"
            }
            "session.inbox.enqueued" if data["item"]["type"] == "user" => {
                props["info"] = json!({"id": data["inboxID"], "sessionID": sid, "role": "user"});
                props["parts"] = json!([{"type": "text", "text": data["item"]["payload"]["text"]}]);
                "message.updated"
            }
            "session.text.delta"
            | "session.reasoning.delta"
            | "session.text.ended"
            | "session.reasoning.ended" => {
                let key = (
                    sid.to_string(),
                    data["assistantMessageID"].as_str()?.to_string(),
                    data["ordinal"].as_u64()?,
                );
                let text = if kind.ends_with(".delta") {
                    let text = self.text.entry(key).or_default();
                    text.push_str(data["delta"].as_str().unwrap_or(""));
                    text.clone()
                } else {
                    self.text.remove(&key);
                    data["text"].as_str().unwrap_or("").to_string()
                };
                props["part"] = json!({"type": if kind.starts_with("session.text.") {"text"} else {"reasoning"},
                    "text": text, "messageID": data["assistantMessageID"], "sessionID": sid});
                "message.part.updated"
            }
            "session.execution.started" | "session.step.started" => {
                props["status"] = json!({"type": "busy"});
                "session.status"
            }
            "session.execution.succeeded" | "session.execution.interrupted" => {
                self.text.retain(|(session, _, _), _| session != sid);
                self.tools.retain(|(session, _), _| session != sid);
                "session.idle"
            }
            "session.execution.failed" => {
                self.text.retain(|(session, _, _), _| session != sid);
                self.tools.retain(|(session, _), _| session != sid);
                "session.error"
            }
            "session.compaction.started" => "session.compacted",
            "session.created" => {
                props["info"] = data.clone();
                "session.created"
            }
            "form.created" => {
                props["sessionID"] = data["form"]["sessionID"].clone();
                props["part"] = json!({"type":"tool", "tool":"question", "state":{"status":"running", "input":data["form"]}});
                "message.part.updated"
            }
            "form.replied" | "form.cancelled" => {
                props["part"] =
                    json!({"type":"tool", "tool":"question", "state":{"status":"completed"}});
                "message.part.updated"
            }
            // The durable execution event above is the one Stop boundary.
            "session.idle" => return None,
            _ => kind,
        };
        Some(json!({"type": mapped, "properties": props, "v2_event": event}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_cli_defaults_preserve_overrides() {
        for content in [None, Some(""), Some("  "), Some("{}")] {
            assert_eq!(cli_config(content).unwrap(), r#"{"tabs":{"mode":"off"}}"#);
        }
        let config: Value = serde_json::from_str(
            &cli_config(Some(
                r#"{
            // OpenCode accepts comments and trailing commas in CLI overrides.
            "tabs": {"layout": "vertical",},
            "plugins": ["monitor"], "session": {"thinking": "hide"},
        }"#,
            ))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            config,
            json!({"tabs":{"layout":"vertical","mode":"off"},
            "plugins":["monitor"],"session":{"thinking":"hide"}})
        );
        for tabs in [
            json!({"mode":"on"}),
            json!({"mode":"auto"}),
            json!({"mode":"off"}),
            json!({"enabled":true}),
            json!({"enabled":false}),
        ] {
            let original = json!({"tabs":tabs});
            let actual: Value =
                serde_json::from_str(&cli_config(Some(&original.to_string())).unwrap()).unwrap();
            assert_eq!(actual, original);
        }
    }

    #[test]
    fn invalid_cli_overrides_fail_without_echoing_contents() {
        for content in [
            "null",
            "[]",
            "false",
            r#"{"tabs":null}"#,
            r#"{"tabs":true}"#,
            r#"{"secret":"do-not-print", broken"#,
        ] {
            let error = cli_config(Some(content)).unwrap_err();
            assert!(error.contains("OPENCODE_CLI_CONFIG_CONTENT"));
            assert!(!error.contains("do-not-print"));
        }
    }

    #[test]
    fn tool_completion_recovers_identity_from_history() {
        let mut events = Events::default();
        let event = json!({"type":"session.tool.success", "data":{"sessionID":"ses_test", "id":"call_1", "assistantMessageID":"msg_1", "content":[{"type":"text","text":"done"}]}});
        assert!(events.missing_tool(&event));
        events.seed_tools("ses_test", &[message(&json!({"id":"msg_1", "type":"assistant", "content":[{"type":"tool", "id":"call_1", "name":"shell", "state":{"status":"running", "input":{"command":"test"}}}]}))]);
        assert!(!events.missing_tool(&event));
        let normalized = events.normalize(event).unwrap();
        let part = &normalized["properties"]["part"];
        assert_eq!(part["tool"], "shell");
        assert_eq!(part["state"]["input"]["command"], "test");
        assert_eq!(part["state"]["output"], "done");
        assert_eq!(
            crate::opencode::event_to_detailed(&normalized),
            Some(libslop::PaneDetailedState::BusyProcessing)
        );
    }

    #[test]
    fn text_accumulates_and_terminal_events_clear_state() {
        let mut events = Events::default();
        for (delta, expected) in [("one", "one"), ("two", "onetwo")] {
            let event = events.normalize(json!({"type":"session.text.delta", "data":{"sessionID":"ses_test", "assistantMessageID":"msg_1", "ordinal":0,"delta":delta}})).unwrap();
            assert_eq!(event["properties"]["part"]["text"], expected);
        }
        let stop = events
            .normalize(
                json!({"type":"session.execution.succeeded","data":{"sessionID":"ses_test"}}),
            )
            .unwrap();
        assert_eq!(
            crate::opencode::event_to_detailed(&stop),
            Some(libslop::PaneDetailedState::Ready)
        );
        assert!(events.text.is_empty());
        assert!(
            events
                .normalize(json!({"type":"session.idle","data":{"sessionID":"ses_test"}}))
                .is_none()
        );
    }

    #[test]
    fn launch_preserves_tui_arguments_and_rejects_external_servers() {
        let args = [
            "--port",
            "1234",
            "--hostname",
            "127.0.0.1",
            "--session=ses_saved",
            "--model",
            "test/model",
        ]
        .map(str::to_string);
        let (port, session, remaining, create) = launch_args(&args).unwrap();
        assert_eq!(port, 1234);
        assert_eq!(session, "ses_saved");
        assert_eq!(remaining, ["--model", "test/model"]);
        assert!(!create);
        assert!(launch_args(&["--server=http://example.invalid".into()]).is_err());
        assert!(launch_args(&["--port".into(), "1234".into(), "-s".into()]).is_err());
    }
}
