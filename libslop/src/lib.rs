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

pub enum TmuxOption {
    /// Marks the slopd-managed tmux session; value is "true"
    SlopdManaged,
    /// Stores the Claude session ID on a pane
    SlopdClaudeSessionId,
    /// Stores the parent pane ID when a pane was spawned by another pane via slopctl run
    SlopdParentPane,
}

impl TmuxOption {
    pub fn as_str(&self) -> &'static str {
        match self {
            TmuxOption::SlopdManaged => "@slopd_managed",
            TmuxOption::SlopdClaudeSessionId => "@slopd_claude_session_id",
            TmuxOption::SlopdParentPane => "@slopd_parent_pane",
        }
    }
}

/// Validate a user-supplied tag name and return the full tmux option name.
/// Tag names must match `[A-Za-z0-9_-]+` (what tmux accepts in option names).
pub fn tag_option_name(tag: &str) -> Result<String, String> {
    if tag.is_empty() {
        return Err("tag name must not be empty".to_string());
    }
    if !tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(format!(
            "invalid tag {:?}: only ASCII letters, digits, '_', and '-' are allowed",
            tag
        ));
    }
    Ok(format!("@slopd_tag_{}", tag))
}

/// The prefix used for tag options; used to enumerate tags on a pane.
pub const TAG_OPTION_PREFIX: &str = "@slopd_tag_";

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_hooks_into_file_concurrent_no_duplicate_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{}").unwrap();

        const N: usize = 32;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(N));
        let handles: Vec<_> = (0..N)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    inject_hooks_into_file(&path, "slopctl").map_err(|e| e.to_string())
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap().unwrap_or_else(|e| panic!("inject_hooks_into_file failed: {}", e));
        }

        let contents = std::fs::read_to_string(&path).unwrap();
        let settings: serde_json::Value = serde_json::from_str(&contents).unwrap();

        for &event in HOOK_EVENTS {
            let entries = settings["hooks"][event].as_array()
                .unwrap_or_else(|| panic!("missing hooks.{}", event));
            let count = entries.iter().filter(|entry| {
                entry["hooks"].as_array().map_or(false, |hooks| {
                    hooks.iter().any(|h| {
                        h["type"] == "command"
                            && h["command"].as_str()
                                .map_or(false, |c| c.contains("slopctl") && c.contains(event))
                    })
                })
            }).count();
            assert_eq!(count, 1, "event {} has {} entries, want 1", event, count);
        }

        let contents = std::fs::read_to_string(&path).unwrap();
        let settings: serde_json::Value = serde_json::from_str(&contents).unwrap();

        for &event in HOOK_EVENTS {
            let entries = settings["hooks"][event].as_array()
                .unwrap_or_else(|| panic!("missing hooks.{}", event));
            let count = entries.iter().filter(|entry| {
                entry["hooks"].as_array().map_or(false, |hooks| {
                    hooks.iter().any(|h| {
                        h["type"] == "command"
                            && h["command"].as_str()
                                .map_or(false, |c| c.contains("slopctl") && c.contains(event))
                    })
                })
            }).count();
            assert_eq!(count, 1, "event {} has {} entries, want 1", event, count);
        }
    }
}

/// Read, inject, and write hooks to a Claude settings.json file. Idempotent.
///
/// Uses an exclusive advisory lock on a sidecar `.lock` file to prevent lost
/// updates when multiple processes run concurrently, and an atomic rename to
/// prevent torn writes if the process is interrupted mid-write.
pub fn inject_hooks_into_file(
    settings_path: &PathBuf,
    slopctl: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let lock_path = settings_path.with_extension("json.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)?;
    let mut lock = fd_lock::RwLock::new(lock_file);
    let _guard = lock.write()?;

    let mut settings: serde_json::Value = match std::fs::read_to_string(settings_path) {
        Ok(contents) => serde_json::from_str(&contents)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e.into()),
    };

    inject_hooks(&mut settings, slopctl);

    let mut file = atomic_write_file::AtomicWriteFile::options().open(settings_path)?;
    use std::io::Write;
    write!(file, "{}", serde_json::to_string_pretty(&settings)?)?;
    file.commit()?;

    Ok(())
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SlopdConfig {
    #[serde(default)]
    pub tmux: SlopdTmuxConfig,
    #[serde(default)]
    pub run: SlopdRunConfig,
    /// Override Claude's config directory (mirrors CLAUDE_CONFIG_DIR; default: ~/.claude)
    pub claude_config_dir: Option<PathBuf>,
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

    pub fn claude_config_dir(&self) -> PathBuf {
        self.claude_config_dir
            .clone()
            .unwrap_or_else(|| home_dir().join(".claude"))
    }

    pub fn claude_settings_path(&self) -> PathBuf {
        self.claude_config_dir().join("settings.json")
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

/// Describes which events a subscriber wants to receive.
/// All specified fields must match (AND within one filter).
/// Multiple filters in a Subscribe request are OR-ed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventFilter {
    /// Event source: "hook" or "slopd". Omit to match all sources.
    pub source: Option<String>,
    /// Event type, e.g. "UserPromptSubmit". Omit to match all event types.
    pub event_type: Option<String>,
    /// Only receive events from this tmux pane. Omit to match all panes.
    pub pane_id: Option<String>,
    /// Only receive events whose payload contains this Claude session_id. Omit to match all sessions.
    pub session_id: Option<String>,
    /// Additional payload key-value pairs that must all match (shallow equality).
    #[serde(default)]
    pub payload_match: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RequestBody {
    Status,
    Run { parent_pane_id: Option<String> },
    Kill { pane_id: String },
    Hook { event: String, payload: serde_json::Value, pane_id: Option<String> },
    Send { pane_id: String, prompt: String, timeout_secs: u64 },
    /// Send Ctrl+C, Ctrl+D, and Escape to a pane to interrupt a running agent.
    Interrupt { pane_id: String },
    /// Subscribe to a stream of events. An empty filters vec matches all events.
    Subscribe { filters: Vec<EventFilter> },
    /// Set or remove a user-defined tag on a pane.
    Tag { pane_id: String, tag: String, remove: bool },
    /// List all user-defined tags on a pane.
    Tags { pane_id: String },
    /// List all panes in the slopd session.
    Ps,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub body: ResponseBody,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResponseBody {
    Status { state: DaemonState },
    Run { pane_id: String },
    Kill { pane_id: String },
    Sent { pane_id: String },
    Interrupted { pane_id: String },
    Hooked,
    /// Sent once to confirm a Subscribe request was accepted.
    Subscribed,
    /// Streamed to subscribers as events occur.
    Event {
        source: String,
        event_type: String,
        pane_id: Option<String>,
        payload: serde_json::Value,
    },
    Tagged { pane_id: String, tag: String },
    Untagged { pane_id: String, tag: String },
    Tags { pane_id: String, tags: Vec<String> },
    Ps { panes: Vec<PaneInfo> },
    Error { message: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PaneInfo {
    pub pane_id: String,
    /// Unix timestamp of pane creation.
    pub created_at: u64,
    /// Claude session ID stored by the SessionStart hook, if set.
    pub session_id: Option<String>,
    /// User-defined tags on this pane.
    pub tags: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonState {
    pub uptime_secs: u64,
}
