//! Per-launch MCP policy. Never edit an agent's shared configuration.

use libslop::Backend;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

pub(super) struct DisabledLaunch {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

fn insert_options(args: &mut Vec<String>, options: impl IntoIterator<Item = String>) {
    let end = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    args.splice(end..end, options);
}

fn has_option(args: &[String], option: &str) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == option || arg.starts_with(&format!("{option}=")))
}

/// Only configuration-selection flags belong on the read-only probe; prompts,
/// resume/session flags, and model execution flags must never reach it.
fn codex_config_args(args: &[String], cwd: &Path) -> Result<(Vec<String>, PathBuf), String> {
    let mut result = Vec::new();
    let mut directory = cwd.to_path_buf();
    let mut iter = args.iter().take_while(|arg| arg.as_str() != "--");
    while let Some(arg) = iter.next() {
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(k, v)| (k, Some(v)));
        let is_dir = matches!(flag, "-C" | "--cd");
        if is_dir
            || matches!(
                flag,
                "-c" | "--config" | "-p" | "--profile" | "--enable" | "--disable"
            )
        {
            let value = inline
                .or_else(|| iter.next().map(String::as_str))
                .ok_or_else(|| format!("{flag} requires a value"))?;
            if is_dir {
                directory = cwd.join(value);
            } else {
                result.extend([flag.to_string(), value.to_string()]);
            }
        } else if ["-C", "-c", "-p"]
            .iter()
            .any(|prefix| arg.starts_with(prefix))
            && arg.len() > 2
        {
            let (flag, value) = arg.split_at(2);
            match flag {
                "-C" => directory = cwd.join(value),
                "-c" | "-p" => result.extend([flag.to_string(), value.to_string()]),
                _ => {}
            }
        }
    }
    Ok((result, directory))
}

async fn probe_json(
    backend: Backend,
    program: &Path,
    args: &[String],
    env: &[(String, String)],
    config_dir: Option<&Path>,
    cwd: &Path,
) -> Result<Value, String> {
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd).kill_on_drop(true);
    if let Some(dir) = config_dir {
        command.env(backend.config_dir_env_var(), dir);
    }
    // Match spawn_pane's last-wins environment precedence.
    command.envs(env.iter().cloned());
    for key in ["TMUX", "TMUX_PANE", "CODEX_THREAD_ID", "CODEX_SESSION_ID"] {
        command.env_remove(key);
    }
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .map_err(|_| {
            "MCP configuration probe timed out; refusing to launch with mcp=false".to_string()
        })?
        .map_err(|e| format!("MCP configuration probe failed: {e}"))?;
    // Config dumps can contain credentials. Never put their contents in errors.
    if !output.status.success() {
        return Err(format!(
            "{} MCP configuration probe failed ({}); refusing to launch with mcp=false",
            backend.canonical_executable(),
            output.status
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|_| {
        "MCP configuration probe did not return JSON; refusing to launch with mcp=false".to_string()
    })
}

fn codex_override(servers: &Value) -> Result<String, String> {
    let servers = servers
        .as_array()
        .ok_or("unexpected Codex MCP configuration")?;
    let entries = servers
        .iter()
        .map(|server| {
            let name = server
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or("invalid Codex MCP server name")?;
            // A root inline table preserves quoted/dotted server names. Codex's
            // dotted -c key parser does not understand TOML quoting in key paths.
            // A root override also replaces prior CLI-provided tables. Keep a
            // valid transport so a server introduced with -c still validates.
            let transport = &server["transport"];
            let (key, value) = match transport["type"].as_str() {
                Some("stdio") => ("command", transport["command"].as_str()),
                Some("streamable_http") => ("url", transport["url"].as_str()),
                _ => return Err("unsupported Codex MCP transport"),
            };
            let value = value.ok_or("invalid Codex MCP transport")?;
            Ok(format!(
                "{} = {{ enabled = false, {key} = {} }}",
                json!(name),
                json!(value)
            ))
        })
        .collect::<Result<Vec<_>, &str>>()?;
    Ok(format!("mcp_servers = {{ {} }}", entries.join(", ")))
}

pub(super) async fn disabled_launch(
    backend: Backend,
    program: &Path,
    args: &[String],
    env: &[(String, String)],
    config_dir: Option<&Path>,
    cwd: &Path,
    opencode_v2: bool,
) -> Result<DisabledLaunch, String> {
    let mut launch = DisabledLaunch {
        args: args.to_vec(),
        env: Vec::new(),
    };
    match backend {
        Backend::Claude => {
            for option in ["--mcp-config", "--chrome"] {
                if has_option(args, option) {
                    return Err(format!("mcp=false conflicts with {option}"));
                }
            }
            let mut options = vec!["--mcp-config".into(), "{\"mcpServers\":{}}".into()];
            if !has_option(args, "--strict-mcp-config") {
                options.push("--strict-mcp-config".into());
            }
            if !has_option(args, "--no-chrome") {
                options.push("--no-chrome".into());
            }
            insert_options(&mut launch.args, options);
            launch
                .env
                .push(("ENABLE_CLAUDEAI_MCP_SERVERS".into(), "false".into()));
        }
        Backend::Codex => {
            if has_option(args, "--remote") {
                return Err("mcp=false cannot configure a remote Codex app server".into());
            }
            let (config_args, directory) = codex_config_args(args, cwd)?;
            let mut options = vec![
                "--disable".into(),
                "apps".into(),
                "--disable".into(),
                "plugins".into(),
            ];
            let mut probe_args = config_args.clone();
            probe_args.extend(options.clone());
            probe_args.extend(["mcp".into(), "list".into(), "--json".into()]);
            let servers =
                probe_json(backend, program, &probe_args, env, config_dir, &directory).await?;
            options.extend(["-c".into(), codex_override(&servers)?]);
            let mut verify_args = config_args;
            verify_args.extend(options.clone());
            verify_args.extend(["mcp".into(), "list".into(), "--json".into()]);
            let verified =
                probe_json(backend, program, &verify_args, env, config_dir, &directory).await?;
            if !verified
                .as_array()
                .is_some_and(|servers| servers.iter().all(|server| server["enabled"] == false))
            {
                return Err("Codex configuration still enables MCP servers; refusing to launch with mcp=false".into());
            }
            insert_options(&mut launch.args, options);
            if !has_option(args, "--no-daemon") {
                launch.args.insert(0, "--no-daemon".into());
            }
        }
        Backend::Opencode if !opencode_v2 => {
            let probe_args = vec!["debug".into(), "config".into()];
            let effective = probe_json(backend, program, &probe_args, env, config_dir, cwd).await?;
            let existing = env
                .iter()
                .rev()
                .find(|(key, _)| key == "OPENCODE_CONFIG_CONTENT")
                .map(|(_, value)| value.clone())
                .or_else(|| std::env::var("OPENCODE_CONFIG_CONTENT").ok());
            let mut overlay: Value = match existing {
                Some(value) => {
                    json5::from_str(&value).map_err(|_| "invalid OPENCODE_CONFIG_CONTENT")?
                }
                None => json!({}),
            };
            let overlay = overlay
                .as_object_mut()
                .ok_or("OPENCODE_CONFIG_CONTENT must be an object")?;
            let mut servers = effective.get("mcp").cloned().unwrap_or_else(|| json!({}));
            for server in servers
                .as_object_mut()
                .ok_or("unexpected OpenCode MCP configuration")?
                .values_mut()
            {
                server
                    .as_object_mut()
                    .ok_or("unexpected OpenCode MCP server configuration")?
                    .insert("enabled".into(), false.into());
            }
            overlay.insert("mcp".into(), servers);
            launch.env.push((
                "OPENCODE_CONFIG_CONTENT".into(),
                serde_json::to_string(overlay).map_err(|e| e.to_string())?,
            ));
            let mut verify_env = env.to_vec();
            verify_env.extend(launch.env.clone());
            let verified =
                probe_json(backend, program, &probe_args, &verify_env, config_dir, cwd).await?;
            if !verified
                .get("mcp")
                .and_then(Value::as_object)
                .is_some_and(|servers| servers.values().all(|server| server["enabled"] == false))
            {
                return Err("OpenCode configuration still enables MCP servers; refusing to launch with mcp=false".into());
            }
        }
        Backend::Opencode => return Err("mcp=false is not supported for OpenCode 2 yet".into()),
        Backend::Grok => return Err("mcp=false is not supported for this Grok backend yet".into()),
    }
    Ok(launch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn codex_probe_uses_launch_config_and_directory_without_running_prompt() {
        let args = strings(&[
            "--model",
            "test",
            "-C",
            "work",
            "--profile=lean",
            "-c",
            "mcp_servers.a.enabled=true",
            "resume",
            "session",
            "--enable",
            "apps",
            "--",
            "--cd",
            "/ignored",
        ]);
        let (selected, cwd) = codex_config_args(&args, Path::new("/base")).unwrap();
        assert_eq!(cwd, Path::new("/base/work"));
        assert_eq!(
            selected,
            strings(&[
                "--profile",
                "lean",
                "-c",
                "mcp_servers.a.enabled=true",
                "--enable",
                "apps"
            ])
        );
        assert!(codex_config_args(&strings(&["-c"]), Path::new("/base")).is_err());
    }

    #[tokio::test]
    async fn claude_disabled_policy_preserves_prompt_and_rejects_conflicting_servers() {
        let args = strings(&["--model", "test", "--", "prompt"]);
        let policy = disabled_launch(
            Backend::Claude,
            Path::new("unused"),
            &args,
            &[],
            None,
            Path::new("/tmp"),
            false,
        )
        .await
        .unwrap();
        assert_eq!(&policy.args[policy.args.len() - 2..], ["--", "prompt"]);
        assert!(has_option(&policy.args, "--strict-mcp-config"));
        assert!(
            policy
                .env
                .contains(&("ENABLE_CLAUDEAI_MCP_SERVERS".into(), "false".into()))
        );
        for option in ["--mcp-config=file.json", "--chrome"] {
            assert!(
                disabled_launch(
                    Backend::Claude,
                    Path::new("unused"),
                    &strings(&[option]),
                    &[],
                    None,
                    Path::new("/tmp"),
                    false
                )
                .await
                .is_err()
            );
        }
    }

    #[tokio::test]
    async fn probe_failure_does_not_launch_or_leak_config_output() {
        let dir = libsloptest::tempfile::tempdir().unwrap();
        let script = dir.path().join("agent");
        std::fs::write(&script, "#!/bin/sh\necho SECRET >&2\nexit 1\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let error = disabled_launch(Backend::Codex, &script, &[], &[], None, dir.path(), false)
            .await
            .err()
            .unwrap();
        assert!(error.contains("refusing to launch"));
        assert!(!error.contains("SECRET"));
    }

    #[tokio::test]
    #[ignore = "requires the real Codex CLI; config inspection only, no model calls"]
    async fn codex_mcp_disabled_with_real_cli() {
        let dir = libsloptest::tempfile::tempdir().unwrap();
        let home = dir.path().join("codex-home");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(".codex")).unwrap();
        let config = format!(
            "[features]\napps=true\nplugins=true\n[projects.{}]\ntrust_level=\"trusted\"\n[mcp_servers.global]\ncommand=\"/bin/false\"\n",
            json!(project)
        );
        std::fs::write(home.join("config.toml"), &config).unwrap();
        std::fs::write(
            home.join("lean.config.toml"),
            "[mcp_servers.profile]\ncommand=\"/bin/false\"\n[mcp_servers.http]\nurl=\"http://127.0.0.1:9/mcp\"\n",
        )
        .unwrap();
        std::fs::write(
            project.join(".codex/config.toml"),
            "[mcp_servers.\"project.with.dots\"]\ncommand=\"/bin/false\"\n",
        )
        .unwrap();
        let args = strings(&[
            "-C",
            project.to_str().unwrap(),
            "-p",
            "lean",
            "-c",
            "mcp_servers.cli.command=\"/bin/false\"",
            "--enable",
            "apps",
            "--enable",
            "plugins",
        ]);
        let policy = disabled_launch(
            Backend::Codex,
            Path::new("codex"),
            &args,
            &[],
            Some(&home),
            dir.path(),
            false,
        )
        .await
        .unwrap();
        let mut query = policy.args.clone();
        query.extend(strings(&["mcp", "list", "--json"]));
        let result = probe_json(
            Backend::Codex,
            Path::new("codex"),
            &query,
            &[],
            Some(&home),
            &project,
        )
        .await
        .unwrap();
        let servers = result.as_array().unwrap();
        assert_eq!(servers.len(), 5);
        assert!(servers.iter().all(|server| server["enabled"] == false));
        // features list does not accept runtime profiles; the base config
        // deliberately enables both features for this separate assertion.
        let mut feature_args = policy.args.clone();
        let profile = feature_args.iter().position(|arg| arg == "-p").unwrap();
        feature_args.drain(profile..profile + 2);
        let features = Command::new("codex")
            .args(&feature_args)
            .args(["features", "list"])
            .env("CODEX_HOME", &home)
            .current_dir(&project)
            .output()
            .await
            .unwrap();
        assert!(
            features.status.success(),
            "{}",
            String::from_utf8_lossy(&features.stderr)
        );
        let text = String::from_utf8(features.stdout).unwrap();
        for name in ["apps", "plugins"] {
            let line = text
                .lines()
                .find(|line| line.split_whitespace().next() == Some(name))
                .unwrap();
            assert_eq!(line.split_whitespace().last(), Some("false"));
        }
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).unwrap(),
            config
        );
    }

    #[tokio::test]
    #[ignore = "requires OpenCode 1; set SLOPD_TEST_OPENCODE to its executable"]
    async fn opencode_mcp_disabled_with_real_cli() {
        let program = std::env::var("SLOPD_TEST_OPENCODE").unwrap_or_else(|_| "opencode".into());
        let dir = libsloptest::tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let config = dir.path().join("config");
        std::fs::create_dir_all(config.join("opencode")).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let content = r#"{"$schema":"https://opencode.ai/config.json","mcp":{"project":{"type":"local","command":["/bin/false"]}}}"#;
        std::fs::write(project.join("opencode.json"), content).unwrap();
        std::fs::write(
            config.join("opencode/opencode.json"),
            r#"{"mcp":{"global":{"type":"local","command":["/bin/false"]}}}"#,
        )
        .unwrap();
        let env = vec![
            ("HOME".into(), dir.path().display().to_string()),
            ("XDG_CONFIG_HOME".into(), config.display().to_string()),
            ("XDG_CACHE_HOME".into(), dir.path().join("cache").display().to_string()),
            ("XDG_DATA_HOME".into(), dir.path().join("data").display().to_string()),
            ("XDG_STATE_HOME".into(), dir.path().join("state").display().to_string()),
            ("OPENCODE_DISABLE_AUTOUPDATE".into(), "true".into()),
            ("OPENCODE_CONFIG_CONTENT".into(), r#"{"model":"test/model","mcp":{"inline":{"type":"local","command":["/bin/false"]}}}"#.into()),
        ];
        let policy = disabled_launch(
            Backend::Opencode,
            Path::new(&program),
            &[],
            &env,
            None,
            &project,
            false,
        )
        .await
        .unwrap();
        let overlay: Value = serde_json::from_str(&policy.env[0].1).unwrap();
        assert_eq!(overlay["model"], "test/model");
        let servers = overlay["mcp"].as_object().unwrap();
        assert_eq!(servers.len(), 3);
        assert!(servers.values().all(|server| server["enabled"] == false));
        assert_eq!(
            std::fs::read_to_string(project.join("opencode.json")).unwrap(),
            content
        );
    }
}
