use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub fn verbosity_to_level(verbosity: u8) -> tracing::Level {
    match verbosity {
        0 => tracing::Level::WARN,
        1 => tracing::Level::INFO,
        2 => tracing::Level::DEBUG,
        _ => tracing::Level::TRACE,
    }
}

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

/// Expand `~` and `$VAR` / `${VAR}` references in a path.
///
/// - A leading `~` (alone or followed by `/`) is replaced with the current
///   user's home directory.
/// - `$NAME` and `${NAME}` are replaced with the value of the environment
///   variable `NAME`; unknown variables are left as-is.
///
/// This is intended for paths read from config files, where the shell does
/// not perform expansion automatically.
pub fn expand_path(path: &std::path::Path) -> PathBuf {
    let s = path.to_string_lossy();
    let expanded = shellexpand::full_with_context_no_errors(
        s.as_ref(),
        // Use dirs::home_dir() directly (returns Option) rather than the local
        // home_dir() wrapper (which panics) — shellexpand needs an Option.
        || dirs::home_dir().and_then(|p| p.into_os_string().into_string().ok()),
        |var| std::env::var(var).ok(),
    );
    PathBuf::from(expanded.as_ref())
}

pub enum TmuxOption {
    /// Marks the slopd-managed tmux session; value is "true"
    SlopdManaged,
    /// Stores the Claude session ID on a pane
    SlopdClaudeSessionId,
    /// Comma-separated ancestor pane IDs (immediate parent first, then grandparent, etc.)
    SlopdAncestorPanes,
    /// Stores the simplified pane state
    SlopdState,
    /// Stores the detailed pane state
    SlopdDetailedState,
    /// Stores the pane creation unix timestamp
    SlopdCreatedAt,
    /// Stores the transcript file path reported by SessionStart
    SlopdTranscriptPath,
}

impl TmuxOption {
    pub fn as_str(&self) -> &'static str {
        match self {
            TmuxOption::SlopdManaged => "@slopd_managed",
            TmuxOption::SlopdClaudeSessionId => "@slopd_claude_session_id",
            TmuxOption::SlopdAncestorPanes => "@slopd_ancestor_panes",
            TmuxOption::SlopdState => "@slopd_state",
            TmuxOption::SlopdDetailedState => "@slopd_detailed_state",
            TmuxOption::SlopdCreatedAt => "@slopd_created_at",
            TmuxOption::SlopdTranscriptPath => "@slopd_transcript_path",
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

        // Remove stale entries from a previous slopctl path (e.g. hardcoded absolute path
        // after switching to a plain "slopctl" command).  A stale entry is one whose sole
        // hook command is "slopctl hook {event}" (or an absolute path ending in "/slopctl
        // hook {event}") but is not our current command.  Commands from other tools
        // (e.g. "foobar hook {event}") are never considered stale.
        let stale_suffix = format!(" hook {}", event);
        entries.retain(|entry| {
            let is_stale = entry.get("hooks").and_then(|h| h.as_array()).map_or(false, |hooks_arr| {
                hooks_arr.iter().any(|h| {
                    if h.get("type").and_then(|t| t.as_str()) != Some("command") {
                        return false;
                    }
                    let cmd = h.get("command").and_then(|c| c.as_str()).unwrap_or("");
                    if !cmd.ends_with(&stale_suffix) || cmd == command {
                        return false;
                    }
                    // Only remove entries whose executable is slopctl (plain or absolute path).
                    let prefix = &cmd[..cmd.len() - stale_suffix.len()];
                    prefix == "slopctl" || prefix.ends_with("/slopctl")
                })
            });
            !is_stale
        });

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

    #[test]
    fn inject_hooks_preserves_other_tool_entries() {
        // Build a settings.json that already contains hook entries from a different tool
        // (e.g. "foobar hook Stop").  inject_hooks must leave those entries alone.
        let mut settings = serde_json::json!({
            "hooks": {
                "Stop": [
                    {
                        "matcher": "",
                        "hooks": [{"type": "command", "command": "foobar hook Stop"}]
                    }
                ]
            }
        });

        inject_hooks(&mut settings, "slopctl");

        let stop_entries = settings["hooks"]["Stop"].as_array().unwrap();

        // The foobar entry must still be present.
        let foobar_count = stop_entries.iter().filter(|entry| {
            entry["hooks"].as_array().map_or(false, |hooks| {
                hooks.iter().any(|h| h["command"].as_str() == Some("foobar hook Stop"))
            })
        }).count();
        assert_eq!(foobar_count, 1, "foobar hook Stop entry was incorrectly removed");

        // The slopctl entry must also be present.
        let slopctl_count = stop_entries.iter().filter(|entry| {
            entry["hooks"].as_array().map_or(false, |hooks| {
                hooks.iter().any(|h| h["command"].as_str() == Some("slopctl hook Stop"))
            })
        }).count();
        assert_eq!(slopctl_count, 1, "slopctl hook Stop entry is missing");
    }

    #[test]
    fn inject_hooks_removes_stale_path_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{}").unwrap();

        // Inject with an old absolute path (simulates previous slopd config).
        inject_hooks_into_file(&path, "/home/claude/.local/bin/slopctl").unwrap();

        // Then inject with the new plain command.
        inject_hooks_into_file(&path, "slopctl").unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let settings: serde_json::Value = serde_json::from_str(&contents).unwrap();

        for &event in HOOK_EVENTS {
            let entries = settings["hooks"][event].as_array()
                .unwrap_or_else(|| panic!("missing hooks.{}", event));

            // Old path entry must be gone.
            let old_count = entries.iter().filter(|entry| {
                entry["hooks"].as_array().map_or(false, |hooks| {
                    hooks.iter().any(|h| {
                        h["command"].as_str()
                            .map_or(false, |c| c.contains("/home/claude/.local/bin/slopctl"))
                    })
                })
            }).count();
            assert_eq!(old_count, 0, "event {} still has stale absolute-path entry", event);

            // New entry must be present exactly once.
            let new_count = entries.iter().filter(|entry| {
                entry["hooks"].as_array().map_or(false, |hooks| {
                    hooks.iter().any(|h| {
                        h["command"].as_str()
                            .map_or(false, |c| c == &format!("slopctl hook {}", event))
                    })
                })
            }).count();
            assert_eq!(new_count, 1, "event {} has {} new-path entries, want 1", event, new_count);
        }
    }

    #[test]
    fn expand_path_tilde_alone() {
        let home = home_dir();
        assert_eq!(expand_path(std::path::Path::new("~")), home);
    }

    #[test]
    fn expand_path_tilde_slash() {
        let home = home_dir();
        let result = expand_path(std::path::Path::new("~/code/project"));
        assert_eq!(result, home.join("code/project"));
    }

    #[test]
    fn expand_path_dollar_var() {
        // SAFETY: single-threaded test; no other thread reads this variable concurrently.
        unsafe { std::env::set_var("SLOPD_TEST_DIR", "/tmp/test-project") };
        let result = expand_path(std::path::Path::new("$SLOPD_TEST_DIR/sub"));
        assert_eq!(result, std::path::PathBuf::from("/tmp/test-project/sub"));
    }

    #[test]
    fn expand_path_dollar_brace_var() {
        // SAFETY: single-threaded test; no other thread reads this variable concurrently.
        unsafe { std::env::set_var("SLOPD_TEST_DIR2", "/tmp/braced") };
        let result = expand_path(std::path::Path::new("${SLOPD_TEST_DIR2}/sub"));
        assert_eq!(result, std::path::PathBuf::from("/tmp/braced/sub"));
    }

    #[test]
    fn expand_path_no_expansion_needed() {
        let result = expand_path(std::path::Path::new("/absolute/path"));
        assert_eq!(result, std::path::PathBuf::from("/absolute/path"));
    }

    #[test]
    fn expand_path_unknown_var_left_as_is() {
        let result = expand_path(std::path::Path::new("/base/$__SLOPD_NONEXISTENT_VAR__/end"));
        assert_eq!(result, std::path::PathBuf::from("/base/$__SLOPD_NONEXISTENT_VAR__/end"));
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

#[derive(Debug, Serialize, Deserialize)]
pub struct SlopdTmuxConfig {
    pub socket: Option<PathBuf>,
    /// Run `tmux start-server` on startup (default: true when socket is not set).
    pub start_server: Option<bool>,
}

impl Default for SlopdTmuxConfig {
    fn default() -> Self {
        Self { socket: None, start_server: None }
    }
}

impl SlopdTmuxConfig {
    /// Whether slopd should run `tmux start-server` on startup.
    pub fn should_start_server(&self) -> bool {
        self.start_server.unwrap_or(self.socket.is_none())
    }
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
    /// Default working directory for new Claude panes. Supports `~` and
    /// `$VAR` / `${VAR}` expansion. Overridden per-session by
    /// `slopctl run --start-directory`.
    pub start_directory: Option<PathBuf>,
}

fn default_slopctl() -> String {
    "slopctl".to_string()
}

impl Default for SlopdRunConfig {
    fn default() -> Self {
        Self {
            executable: Executable::default(),
            slopctl: default_slopctl(),
            start_directory: None,
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

/// Unified envelope for all events and transcript records across all endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    /// Byte offset in the JSONL file. Set for transcript records, None for lifecycle events.
    pub cursor: Option<u64>,
    /// Origin: "transcript", "hook", or "slopd".
    pub source: String,
    /// Record/event type: "user", "assistant", "StateChange", "ReplayEnd", etc.
    pub event_type: String,
    /// Tmux pane this record belongs to, if applicable.
    pub pane_id: Option<String>,
    /// The full payload (parsed JSON for transcript, structured data for events).
    pub payload: serde_json::Value,
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
    Run { parent_pane_id: Option<String>, extra_args: Vec<String>, start_directory: Option<PathBuf> },
    Kill { pane_id: String },
    Hook { event: String, payload: serde_json::Value, pane_id: Option<String> },
    Send { pane_id: String, prompt: String, timeout_secs: u64, interrupt: bool },
    /// Send Ctrl+C, Ctrl+D, and Escape to a pane to interrupt a running agent.
    Interrupt { pane_id: String },
    /// Subscribe to a stream of lifecycle events (hook + slopd). An empty filters vec matches all.
    Subscribe { filters: Vec<EventFilter> },
    /// Subscribe to a pane's transcript: replay the last `last_n` records from
    /// disk, then stream new records live. All delivered as `Record`s.
    SubscribeTranscript { pane_id: String, last_n: u64 },
    /// Read a page of historical transcript records before a given cursor.
    ReadTranscript { pane_id: String, before_cursor: Option<u64>, limit: u64 },
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
    /// Sent once to confirm a Subscribe or SubscribeTranscript request was accepted.
    Subscribed,
    /// Streamed to subscribers (both Subscribe and SubscribeTranscript).
    Record(Record),
    /// Response to ReadTranscript.
    TranscriptPage { records: Vec<Record> },
    Tagged { pane_id: String, tag: String },
    Untagged { pane_id: String, tag: String },
    Tags { pane_id: String, tags: Vec<String> },
    Ps { panes: Vec<PaneInfo> },
    Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneState {
    BootingUp,
    Ready,
    Busy,
    AwaitingInput,
}

impl PaneState {
    pub fn as_str(&self) -> &'static str {
        match self {
            PaneState::BootingUp => "booting_up",
            PaneState::Ready => "ready",
            PaneState::Busy => "busy",
            PaneState::AwaitingInput => "awaiting_input",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "booting_up" => Some(PaneState::BootingUp),
            "ready" => Some(PaneState::Ready),
            "busy" => Some(PaneState::Busy),
            "awaiting_input" => Some(PaneState::AwaitingInput),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneDetailedState {
    BootingUp,
    Ready,
    BusyProcessing,
    BusyToolUse,
    BusySubagent,
    BusyCompacting,
    AwaitingInputPermission,
    AwaitingInputElicitation,
}

impl PaneDetailedState {
    pub fn as_str(&self) -> &'static str {
        match self {
            PaneDetailedState::BootingUp => "booting_up",
            PaneDetailedState::Ready => "ready",
            PaneDetailedState::BusyProcessing => "busy_processing",
            PaneDetailedState::BusyToolUse => "busy_tool_use",
            PaneDetailedState::BusySubagent => "busy_subagent",
            PaneDetailedState::BusyCompacting => "busy_compacting",
            PaneDetailedState::AwaitingInputPermission => "awaiting_input_permission",
            PaneDetailedState::AwaitingInputElicitation => "awaiting_input_elicitation",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "booting_up" => Some(PaneDetailedState::BootingUp),
            "ready" => Some(PaneDetailedState::Ready),
            "busy_processing" => Some(PaneDetailedState::BusyProcessing),
            "busy_tool_use" => Some(PaneDetailedState::BusyToolUse),
            "busy_subagent" => Some(PaneDetailedState::BusySubagent),
            "busy_compacting" => Some(PaneDetailedState::BusyCompacting),
            "awaiting_input_permission" => Some(PaneDetailedState::AwaitingInputPermission),
            "awaiting_input_elicitation" => Some(PaneDetailedState::AwaitingInputElicitation),
            _ => None,
        }
    }

    pub fn to_simple(&self) -> PaneState {
        match self {
            PaneDetailedState::BootingUp => PaneState::BootingUp,
            PaneDetailedState::Ready => PaneState::Ready,
            PaneDetailedState::BusyProcessing
            | PaneDetailedState::BusyToolUse
            | PaneDetailedState::BusySubagent
            | PaneDetailedState::BusyCompacting => PaneState::Busy,
            PaneDetailedState::AwaitingInputPermission
            | PaneDetailedState::AwaitingInputElicitation => PaneState::AwaitingInput,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PaneInfo {
    pub pane_id: String,
    /// Unix timestamp when slopd spawned this pane (from @slopd_created_at).
    pub created_at: u64,
    /// Unix timestamp of last tmux window activity (#{window_activity}).
    pub last_active: u64,
    /// Claude session ID stored by the SessionStart hook, if set.
    pub session_id: Option<String>,
    /// Parent pane ID if this pane was spawned by another pane via slopctl run.
    pub parent_pane_id: Option<String>,
    /// User-defined tags on this pane.
    pub tags: Vec<String>,
    /// Simplified pane state.
    pub state: PaneState,
    /// Detailed pane state.
    pub detailed_state: PaneDetailedState,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonState {
    pub uptime_secs: u64,
}
