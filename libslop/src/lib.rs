use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub fn socket_path() -> PathBuf {
    runtime_dir().join("slopd/slopd.sock")
}

pub fn runtime_dir() -> PathBuf {
    dirs::runtime_dir().expect("could not determine XDG runtime dir")
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir().expect("could not determine XDG config dir")
}

pub fn home_dir() -> PathBuf {
    dirs::home_dir().expect("could not determine home dir")
}

pub const HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PostToolUseFailure",
    "Notification",
    "SubagentStart",
    "SubagentStop",
    "Stop",
    "StopFailure",
    "TeammateIdle",
    "TaskCompleted",
    "InstructionsLoaded",
    "ConfigChange",
    "WorktreeCreate",
    "WorktreeRemove",
    "PreCompact",
    "PostCompact",
    "Elicitation",
    "ElicitationResult",
    "SessionEnd",
];

/// Idempotently inject slopctl hook entries into a Claude settings.json value.
/// Adds our hook command for each event only if not already present.
pub fn inject_hooks(settings: &mut serde_json::Value, slopctl: &str) {
    let hooks = settings
        .as_object_mut()
        .expect("settings.json must be an object")
        .entry("hooks")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .expect("hooks must be an object");

    for &event in HOOK_EVENTS {
        let command = format!("{} hook {}", slopctl, event);
        let our_hook = serde_json::json!({
            "type": "command",
            "command": command
        });
        let our_matcher = serde_json::json!({
            "matcher": "",
            "hooks": [our_hook]
        });

        let entries = hooks
            .entry(event)
            .or_insert_with(|| serde_json::Value::Array(vec![]))
            .as_array_mut()
            .expect("hook event entry must be an array");

        let already_present = entries.iter().any(|entry| {
            entry.get("hooks").and_then(|h| h.as_array()).map_or(false, |hooks_arr| {
                hooks_arr.iter().any(|h| {
                    h.get("type").and_then(|t| t.as_str()) == Some("command")
                        && h.get("command").and_then(|c| c.as_str()) == Some(&command)
                })
            })
        });

        if !already_present {
            entries.push(our_matcher);
        }
    }
}

/// Read, inject, and write hooks to a Claude settings.json file. Idempotent.
pub fn inject_hooks_into_file(
    settings_path: &PathBuf,
    slopctl: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut settings: serde_json::Value = match std::fs::read_to_string(settings_path) {
        Ok(contents) => serde_json::from_str(&contents)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e.into()),
    };

    inject_hooks(&mut settings, slopctl);

    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(settings_path, serde_json::to_string_pretty(&settings)?)?;
    Ok(())
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SlopdConfig {
    #[serde(default)]
    pub tmux: SlopdTmuxConfig,
    #[serde(default)]
    pub run: SlopdRunConfig,
    /// Override path to Claude's settings.json (default: ~/.claude/settings.json)
    pub claude_settings: Option<PathBuf>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SlopdTmuxConfig {
    pub socket: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Executable {
    String(String),
    Array(Vec<String>),
}

impl Executable {
    pub fn program(&self) -> &str {
        match self {
            Executable::String(s) => s.as_str(),
            Executable::Array(v) => v[0].as_str(),
        }
    }

    pub fn args(&self) -> &[String] {
        match self {
            Executable::String(_) => &[],
            Executable::Array(v) => &v[1..],
        }
    }
}

impl Default for Executable {
    fn default() -> Self {
        Executable::String("claude".to_string())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SlopdRunConfig {
    pub executable: Executable,
    /// Path to slopctl binary used for hook injection (default: "slopctl")
    #[serde(default = "default_slopctl")]
    pub slopctl: String,
}

fn default_slopctl() -> String {
    "slopctl".to_string()
}

impl Default for SlopdRunConfig {
    fn default() -> Self {
        Self {
            executable: Executable::default(),
            slopctl: default_slopctl(),
        }
    }
}

impl SlopdConfig {
    pub fn load() -> Self {
        let path = config_dir().join("slopd/config.toml");
        load_config(path)
    }

    pub fn claude_settings_path(&self) -> PathBuf {
        self.claude_settings
            .clone()
            .unwrap_or_else(|| home_dir().join(".claude/settings.json"))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SlopctlConfig {}

impl SlopctlConfig {
    pub fn load() -> Self {
        let path = config_dir().join("slopctl/config.toml");
        load_config(path)
    }
}

fn load_config<T: Default + for<'de> Deserialize<'de>>(path: PathBuf) -> T {
    match std::fs::read_to_string(&path) {
        Ok(contents) => toml::from_str(&contents).unwrap_or_else(|e| {
            eprintln!("warning: failed to parse {}: {}", path.display(), e);
            T::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => T::default(),
        Err(e) => {
            eprintln!("warning: failed to read {}: {}", path.display(), e);
            T::default()
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub body: RequestBody,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RequestBody {
    Ping,
    Status,
    Run,
    Kill { pane_id: String },
    Hook { event: String, payload: serde_json::Value },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub body: ResponseBody,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResponseBody {
    Pong,
    Status { state: DaemonState },
    Run { pane_id: String },
    Kill { pane_id: String },
    Hooked,
    Error { message: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonState {
    pub uptime_secs: u64,
}
