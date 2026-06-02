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

/// Expand `$VAR` / `${VAR}` references in a string against the current process
/// environment. Missing variables are an error (unlike `expand_path`, which
/// leaves them as-is for path-like values).
pub fn expand_env_value(value: &str) -> Result<String, String> {
    shellexpand::env_with_context(value, |var| {
        std::env::var(var)
            .map(Some)
            .map_err(|_| format!("environment variable ${} is not set", var))
    })
    .map(|cow| cow.into_owned())
    .map_err(|e| e.to_string())
}

/// Parse a `KEY=VALUE` string into a pair, expanding `$VAR` / `${VAR}` in the
/// value against the current process environment. Rejects empty keys and
/// inputs missing the `=` separator.
pub fn parse_env_kv(raw: &str) -> Result<(String, String), String> {
    let (key, value) = raw
        .split_once('=')
        .ok_or_else(|| format!("invalid --env {:?}: expected KEY=VALUE", raw))?;
    if key.is_empty() {
        return Err(format!("invalid --env {:?}: empty key", raw));
    }
    let expanded = expand_env_value(value)
        .map_err(|e| format!("invalid --env {:?}: {}", raw, e))?;
    Ok((key.to_string(), expanded))
}

/// Load environment pairs from a dotenv-style file. Returns pairs in the
/// order they appear in the file. Values are expanded by dotenvy's own
/// substitution rules (it supports `${VAR}` against the process env).
pub fn load_env_file(path: &std::path::Path) -> Result<Vec<(String, String)>, String> {
    let iter = dotenvy::from_path_iter(path)
        .map_err(|e| format!("failed to open env file {}: {}", path.display(), e))?;
    let mut out = Vec::new();
    for item in iter {
        let (k, v) = item
            .map_err(|e| format!("failed to parse env file {}: {}", path.display(), e))?;
        out.push((k, v));
    }
    Ok(out)
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
    /// Stores the account name the pane was launched under (empty/unset for the
    /// unnamed default account). Used to re-inject the right hooks on recovery.
    SlopdAccount,
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
            TmuxOption::SlopdAccount => "@slopd_account",
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
            let is_stale = entry.get("hooks").and_then(|h| h.as_array()).is_some_and(|hooks_arr| {
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
            entry.get("hooks").and_then(|h| h.as_array()).is_some_and(|hooks_arr| {
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

/// Remove all slopctl hook entries from a Claude settings.json value.
/// Entries from other tools are preserved.
pub fn remove_hooks(settings: &mut serde_json::Value) {
    let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return;
    };

    for &event in HOOK_EVENTS {
        let Some(entries) = hooks.get_mut(event).and_then(|e| e.as_array_mut()) else {
            continue;
        };
        let suffix = format!(" hook {}", event);
        entries.retain(|entry| {
            let is_ours = entry.get("hooks").and_then(|h| h.as_array()).is_some_and(|hooks_arr| {
                hooks_arr.iter().any(|h| {
                    if h.get("type").and_then(|t| t.as_str()) != Some("command") {
                        return false;
                    }
                    let cmd = h.get("command").and_then(|c| c.as_str()).unwrap_or("");
                    if !cmd.ends_with(&suffix) {
                        return false;
                    }
                    let prefix = &cmd[..cmd.len() - suffix.len()];
                    prefix == "slopctl" || prefix.ends_with("/slopctl")
                })
            });
            !is_ours
        });
    }
}

/// Read, remove slopctl hooks, and write a Claude settings.json file.
pub fn remove_hooks_from_file(
    settings_path: &PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    // If the settings file doesn't exist, there's nothing to remove.
    if !settings_path.exists() {
        return Ok(());
    }

    let lock_path = settings_path.with_extension("json.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        // Advisory lock file: flock'd, never written, so never truncated.
        .truncate(false)
        .write(true)
        .open(&lock_path)?;
    let mut lock = fd_lock::RwLock::new(lock_file);
    let _guard = lock.write()?;

    let mut settings: serde_json::Value = match std::fs::read_to_string(settings_path) {
        Ok(contents) => serde_json::from_str(&contents)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    remove_hooks(&mut settings);

    let mut file = atomic_write_file::AtomicWriteFile::options().open(settings_path)?;
    use std::io::Write;
    write!(file, "{}", serde_json::to_string_pretty(&settings)?)?;
    file.commit()?;

    Ok(())
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
                entry["hooks"].as_array().is_some_and(|hooks| {
                    hooks.iter().any(|h| {
                        h["type"] == "command"
                            && h["command"].as_str()
                                .is_some_and(|c| c.contains("slopctl") && c.contains(event))
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
                entry["hooks"].as_array().is_some_and(|hooks| {
                    hooks.iter().any(|h| {
                        h["type"] == "command"
                            && h["command"].as_str()
                                .is_some_and(|c| c.contains("slopctl") && c.contains(event))
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
            entry["hooks"].as_array().is_some_and(|hooks| {
                hooks.iter().any(|h| h["command"].as_str() == Some("foobar hook Stop"))
            })
        }).count();
        assert_eq!(foobar_count, 1, "foobar hook Stop entry was incorrectly removed");

        // The slopctl entry must also be present.
        let slopctl_count = stop_entries.iter().filter(|entry| {
            entry["hooks"].as_array().is_some_and(|hooks| {
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
                entry["hooks"].as_array().is_some_and(|hooks| {
                    hooks.iter().any(|h| {
                        h["command"].as_str()
                            .is_some_and(|c| c.contains("/home/claude/.local/bin/slopctl"))
                    })
                })
            }).count();
            assert_eq!(old_count, 0, "event {} still has stale absolute-path entry", event);

            // New entry must be present exactly once.
            let new_count = entries.iter().filter(|entry| {
                entry["hooks"].as_array().is_some_and(|hooks| {
                    hooks.iter().any(|h| {
                        h["command"].as_str()
                            .is_some_and(|c| c == format!("slopctl hook {}", event))
                    })
                })
            }).count();
            assert_eq!(new_count, 1, "event {} has {} new-path entries, want 1", event, new_count);
        }
    }

    #[test]
    fn remove_hooks_removes_all_slopctl_entries() {
        let mut settings = serde_json::json!({});
        inject_hooks(&mut settings, "slopctl");

        // Verify hooks were injected.
        for &event in HOOK_EVENTS {
            assert!(!settings["hooks"][event].as_array().unwrap().is_empty());
        }

        remove_hooks(&mut settings);

        // All slopctl entries must be gone.
        for &event in HOOK_EVENTS {
            let entries = settings["hooks"][event].as_array()
                .unwrap_or_else(|| panic!("missing hooks.{}", event));
            let slopctl_count = entries.iter().filter(|entry| {
                entry["hooks"].as_array().is_some_and(|hooks| {
                    hooks.iter().any(|h| {
                        h["type"] == "command"
                            && h["command"].as_str()
                                .is_some_and(|c| c.contains("slopctl") && c.contains(event))
                    })
                })
            }).count();
            assert_eq!(slopctl_count, 0, "event {} still has {} slopctl entries", event, slopctl_count);
        }
    }

    #[test]
    fn remove_hooks_preserves_other_tool_entries() {
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
        remove_hooks(&mut settings);

        let stop_entries = settings["hooks"]["Stop"].as_array().unwrap();
        let foobar_count = stop_entries.iter().filter(|entry| {
            entry["hooks"].as_array().is_some_and(|hooks| {
                hooks.iter().any(|h| h["command"].as_str() == Some("foobar hook Stop"))
            })
        }).count();
        assert_eq!(foobar_count, 1, "foobar hook Stop entry was incorrectly removed");
    }

    #[test]
    fn remove_hooks_handles_absolute_path_slopctl() {
        let mut settings = serde_json::json!({});
        inject_hooks(&mut settings, "/usr/local/bin/slopctl");

        remove_hooks(&mut settings);

        for &event in HOOK_EVENTS {
            let entries = settings["hooks"][event].as_array()
                .unwrap_or_else(|| panic!("missing hooks.{}", event));
            let slopctl_count = entries.iter().filter(|entry| {
                entry["hooks"].as_array().is_some_and(|hooks| {
                    hooks.iter().any(|h| {
                        h["command"].as_str()
                            .is_some_and(|c| c.contains("slopctl"))
                    })
                })
            }).count();
            assert_eq!(slopctl_count, 0, "event {} still has slopctl entries after removal", event);
        }
    }

    #[test]
    fn remove_hooks_preserves_non_hook_settings() {
        let mut settings = serde_json::json!({
            "permissions": {"allow": ["Read"]},
            "hooks": {}
        });

        inject_hooks(&mut settings, "slopctl");
        remove_hooks(&mut settings);

        assert_eq!(settings["permissions"]["allow"][0], "Read");
    }

    #[test]
    fn remove_hooks_from_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{}").unwrap();

        inject_hooks_into_file(&path, "slopctl").unwrap();

        // Verify hooks exist.
        let contents = std::fs::read_to_string(&path).unwrap();
        let settings: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert!(!settings["hooks"]["SessionStart"].as_array().unwrap().is_empty());

        remove_hooks_from_file(&path).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let settings: serde_json::Value = serde_json::from_str(&contents).unwrap();
        for &event in HOOK_EVENTS {
            let entries = settings["hooks"][event].as_array()
                .unwrap_or_else(|| panic!("missing hooks.{}", event));
            assert_eq!(entries.len(), 0, "event {} still has entries after removal", event);
        }
    }

    #[test]
    fn remove_hooks_from_file_noop_when_no_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"permissions": {"allow": ["Read"]}}"#).unwrap();

        remove_hooks_from_file(&path).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let settings: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(settings["permissions"]["allow"][0], "Read");
    }

    #[test]
    fn remove_hooks_cleans_up_empty_hook_events() {
        let mut settings = serde_json::json!({});
        inject_hooks(&mut settings, "slopctl");
        remove_hooks(&mut settings);

        // After removing all slopctl hooks, each event array should be empty
        // but the hooks object should still exist.
        assert!(settings["hooks"].is_object());
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

    #[test]
    fn resolve_slopctl_absolute_path_returned_as_is() {
        assert_eq!(resolve_slopctl("/usr/local/bin/slopctl"), "/usr/local/bin/slopctl");
    }

    #[test]
    fn resolve_slopctl_nonexistent_bare_name_falls_back_to_original() {
        // A binary that is definitely not on PATH and not a sibling of the test binary.
        let result = resolve_slopctl("__slopctl_nonexistent_test_binary__");
        assert_eq!(result, "__slopctl_nonexistent_test_binary__");
    }

    #[test]
    fn resolve_slopctl_finds_sibling_binary() {
        // Create a temporary "slopctl" next to the current test executable so
        // resolve_slopctl can discover it as a sibling.
        let exe = std::env::current_exe().unwrap();
        let sibling = exe.with_file_name("__test_slopctl_sibling__");
        std::fs::write(&sibling, "").unwrap();
        let result = resolve_slopctl("__test_slopctl_sibling__");
        std::fs::remove_file(&sibling).unwrap();
        assert_eq!(result, sibling.to_string_lossy());
    }

    #[test]
    fn resolve_slopctl_prefers_path_over_sibling() {
        // "sh" is on PATH — resolve_slopctl should return the bare name, not a sibling.
        assert_eq!(resolve_slopctl("sh"), "sh");
    }

    #[test]
    fn remove_hooks_removes_both_bare_and_absolute_slopctl() {
        let mut settings = serde_json::json!({});
        // Inject with bare name.
        inject_hooks(&mut settings, "slopctl");
        // Also inject with an absolute path (simulates a second slopd with different config).
        inject_hooks(&mut settings, "/opt/bin/slopctl");

        remove_hooks(&mut settings);

        for &event in HOOK_EVENTS {
            let entries = settings["hooks"][event].as_array()
                .unwrap_or_else(|| panic!("missing hooks.{}", event));
            let slopctl_count = entries.iter().filter(|entry| {
                entry["hooks"].as_array().is_some_and(|hooks| {
                    hooks.iter().any(|h| {
                        h["command"].as_str()
                            .is_some_and(|c| c.contains("slopctl"))
                    })
                })
            }).count();
            assert_eq!(slopctl_count, 0, "event {} still has {} slopctl entries after removal", event, slopctl_count);
        }
    }

    // --- jq-style payload path tests ---

    fn p(s: &str) -> PayloadPath {
        parse_payload_path(s).unwrap_or_else(|e| panic!("parse_payload_path({:?}) failed: {}", s, e))
    }

    #[test]
    fn parse_path_simple_keys() {
        assert_eq!(p("foo"), vec![PathSegment::Key("foo".into())]);
        assert_eq!(p("foo.bar"), vec![
            PathSegment::Key("foo".into()),
            PathSegment::Key("bar".into()),
        ]);
        // Leading dot is optional and equivalent.
        assert_eq!(p(".foo.bar"), p("foo.bar"));
    }

    #[test]
    fn parse_path_array_segments() {
        assert_eq!(p("foo[]"), vec![
            PathSegment::Key("foo".into()),
            PathSegment::AnyElement,
        ]);
        assert_eq!(p("foo[0]"), vec![
            PathSegment::Key("foo".into()),
            PathSegment::Index(0),
        ]);
        assert_eq!(p("foo[].bar"), vec![
            PathSegment::Key("foo".into()),
            PathSegment::AnyElement,
            PathSegment::Key("bar".into()),
        ]);
        assert_eq!(p("foo[0][1].bar"), vec![
            PathSegment::Key("foo".into()),
            PathSegment::Index(0),
            PathSegment::Index(1),
            PathSegment::Key("bar".into()),
        ]);
    }

    #[test]
    fn parse_path_empty_path() {
        assert_eq!(parse_payload_path("").unwrap(), Vec::<PathSegment>::new());
        assert_eq!(parse_payload_path(".").unwrap(), Vec::<PathSegment>::new());
    }

    #[test]
    fn parse_path_rejects_malformed() {
        assert!(parse_payload_path("foo..bar").is_err(), "double dot should fail");
        assert!(parse_payload_path("[0]").is_err(), "leading bracket should fail");
        assert!(parse_payload_path("foo[").is_err(), "unclosed bracket should fail");
        assert!(parse_payload_path("foo[abc]").is_err(), "non-int index should fail");
        assert!(parse_payload_path("foo[-1]").is_err(), "negative index not yet supported");
    }

    #[test]
    fn path_matches_object_key() {
        let v = serde_json::json!({"detailed_state": "ready"});
        assert!(path_matches(&v, &p("detailed_state"), "ready"));
        assert!(!path_matches(&v, &p("detailed_state"), "busy"));
        assert!(!path_matches(&v, &p("missing"), "ready"));
    }

    #[test]
    fn path_matches_nested() {
        let v = serde_json::json!({"tool_input": {"command": "ls"}});
        assert!(path_matches(&v, &p("tool_input.command"), "ls"));
        assert!(!path_matches(&v, &p("tool_input.command"), "rm"));
    }

    #[test]
    fn path_matches_any_element() {
        // The key case: an assistant message whose content[] contains a text block.
        let v = serde_json::json!({
            "message": {
                "content": [
                    {"type": "thinking", "thinking": "…"},
                    {"type": "text", "text": "hello"},
                ],
            },
        });
        assert!(path_matches(&v, &p("message.content[].type"), "text"));
        assert!(path_matches(&v, &p("message.content[].type"), "thinking"));
        assert!(!path_matches(&v, &p("message.content[].type"), "tool_use"));
    }

    #[test]
    fn path_matches_index() {
        let v = serde_json::json!({"items": ["a", "b", "c"]});
        assert!(path_matches(&v, &p("items[0]"), "a"));
        assert!(path_matches(&v, &p("items[2]"), "c"));
        assert!(!path_matches(&v, &p("items[2]"), "a"));
        // Out-of-bounds → no match, no panic.
        assert!(!path_matches(&v, &p("items[99]"), "a"));
    }

    #[test]
    fn path_matches_scalar_types() {
        let v = serde_json::json!({"n": 42, "b": true, "s": "x", "z": null});
        assert!(path_matches(&v, &p("n"), "42"));
        assert!(path_matches(&v, &p("b"), "true"));
        assert!(path_matches(&v, &p("s"), "x"));
        assert!(path_matches(&v, &p("z"), "null"));
    }

    #[test]
    fn path_does_not_match_compound_against_string() {
        let v = serde_json::json!({"obj": {"a": 1}, "arr": [1, 2]});
        // jq-equivalent: `.obj == "anything"` is false; same here.
        assert!(!path_matches(&v, &p("obj"), "{\"a\":1}"));
        assert!(!path_matches(&v, &p("arr"), "[1,2]"));
    }

    #[test]
    fn path_any_element_short_circuits_on_non_array() {
        // `.foo[]` against `foo: "string"` should not match anything.
        let v = serde_json::json!({"foo": "bar"});
        assert!(!path_matches(&v, &p("foo[].x"), "bar"));
    }

    // --- account config + resolution tests ---

    fn config_from_toml(s: &str) -> SlopdConfig {
        toml::from_str(s).unwrap_or_else(|e| panic!("parse config {:?}: {}", s, e))
    }

    #[test]
    fn account_config_accepts_bare_string() {
        let cfg = config_from_toml("[accounts]\nwork = \"/srv/claude-work\"\n");
        let acct = cfg.accounts.get("work").expect("work account missing");
        assert_eq!(acct.claude_config_dir(), &PathBuf::from("/srv/claude-work"));
    }

    #[test]
    fn account_config_accepts_table_form() {
        let cfg = config_from_toml(
            "[accounts.personal]\nclaude_config_dir = \"/srv/claude-personal\"\n",
        );
        let acct = cfg.accounts.get("personal").expect("personal account missing");
        assert_eq!(acct.claude_config_dir(), &PathBuf::from("/srv/claude-personal"));
    }

    #[test]
    fn resolve_account_named_returns_name_and_dir() {
        let cfg = config_from_toml("[accounts]\nwork = \"/srv/work\"\n");
        let resolved = cfg.resolve_account(Some("work")).unwrap();
        assert_eq!(resolved.name, "work");
        assert_eq!(resolved.config_dir, Some(PathBuf::from("/srv/work")));
    }

    #[test]
    fn resolve_account_unknown_errors_and_lists_configured() {
        let cfg = config_from_toml("[accounts]\nwork = \"/srv/work\"\n");
        let err = cfg.resolve_account(Some("nope")).unwrap_err();
        assert!(err.contains("nope"), "err should name the bad account: {}", err);
        assert!(err.contains("work"), "err should list configured accounts: {}", err);
        assert!(err.contains(DEFAULT_ACCOUNT), "err should list the default account: {}", err);
    }

    #[test]
    fn resolve_account_none_uses_default_account() {
        let cfg = config_from_toml("default_account = \"work\"\n[accounts]\nwork = \"/srv/work\"\n");
        let resolved = cfg.resolve_account(None).unwrap();
        assert_eq!(resolved.name, "work");
        assert_eq!(resolved.config_dir, Some(PathBuf::from("/srv/work")));
    }

    #[test]
    fn resolve_account_explicit_overrides_default_account() {
        let cfg = config_from_toml(
            "default_account = \"work\"\n[accounts]\nwork = \"/srv/work\"\npersonal = \"/srv/personal\"\n",
        );
        let resolved = cfg.resolve_account(Some("personal")).unwrap();
        assert_eq!(resolved.name, "personal");
        assert_eq!(resolved.config_dir, Some(PathBuf::from("/srv/personal")));
    }

    #[test]
    fn resolve_account_default_uses_top_level_claude_config_dir() {
        // Top-level claude_config_dir backs the reserved "default" account.
        let cfg = config_from_toml("claude_config_dir = \"/srv/legacy\"\n");
        for requested in [None, Some(DEFAULT_ACCOUNT)] {
            let resolved = cfg.resolve_account(requested).unwrap();
            assert_eq!(resolved.name, DEFAULT_ACCOUNT);
            assert_eq!(resolved.config_dir, Some(PathBuf::from("/srv/legacy")));
        }
    }

    #[test]
    fn resolve_account_explicit_default_table_overrides_top_level() {
        // [accounts.default] wins over the top-level claude_config_dir shorthand.
        let cfg = config_from_toml(
            "claude_config_dir = \"/srv/legacy\"\n[accounts]\ndefault = \"/srv/explicit\"\n",
        );
        let resolved = cfg.resolve_account(Some(DEFAULT_ACCOUNT)).unwrap();
        assert_eq!(resolved.name, DEFAULT_ACCOUNT);
        assert_eq!(resolved.config_dir, Some(PathBuf::from("/srv/explicit")));
    }

    #[test]
    fn resolve_account_default_with_nothing_configured_has_no_dir() {
        // Nothing configured: the default account resolves but exports no dir
        // (Claude falls back to ~/.claude).
        let cfg = SlopdConfig::default();
        let resolved = cfg.resolve_account(None).unwrap();
        assert_eq!(resolved.name, DEFAULT_ACCOUNT);
        assert_eq!(resolved.config_dir, None);
    }

    #[test]
    fn resolve_account_reserved_default_succeeds_even_with_bad_default_account() {
        // A misconfigured default_account makes resolve_account(None) error, but
        // the reserved DEFAULT_ACCOUNT must still resolve — startup recovery
        // (load_managed_panes) relies on this to avoid crashing the daemon.
        let cfg = config_from_toml("default_account = \"ghost\"\n[accounts]\nwork = \"/srv/work\"\n");
        assert!(cfg.resolve_account(None).is_err(), "None resolves to the bad default_account and errors");
        let resolved = cfg.resolve_account(Some(DEFAULT_ACCOUNT)).unwrap();
        assert_eq!(resolved.name, DEFAULT_ACCOUNT);
        assert_eq!(resolved.config_dir, None);
    }

    #[test]
    fn resolve_account_expands_tilde_in_account_dir() {
        let cfg = config_from_toml("[accounts]\nwork = \"~/claude-work\"\n");
        let resolved = cfg.resolve_account(Some("work")).unwrap();
        assert_eq!(resolved.config_dir, Some(home_dir().join("claude-work")));
    }

    #[test]
    fn resolve_account_expands_tilde_in_top_level_claude_config_dir() {
        // The default account's top-level claude_config_dir is `~`-expanded too.
        let cfg = config_from_toml("claude_config_dir = \"~/claude-default\"\n");
        let resolved = cfg.resolve_account(None).unwrap();
        assert_eq!(resolved.config_dir, Some(home_dir().join("claude-default")));
    }

    #[test]
    fn claude_config_dir_method_expands_tilde_and_var() {
        let cfg = config_from_toml("claude_config_dir = \"~/claude-default\"\n");
        assert_eq!(cfg.claude_config_dir(), home_dir().join("claude-default"));
        // SAFETY: single-threaded test; no other thread reads this var concurrently.
        unsafe { std::env::set_var("SLOPD_TEST_CC_DIR", "/tmp/cc") };
        let cfg = config_from_toml("claude_config_dir = \"$SLOPD_TEST_CC_DIR/sub\"\n");
        assert_eq!(cfg.claude_config_dir(), PathBuf::from("/tmp/cc/sub"));
    }

    #[test]
    fn resolved_settings_path_uses_account_dir() {
        let cfg = config_from_toml("[accounts]\nwork = \"/srv/work\"\n");
        let resolved = cfg.resolve_account(Some("work")).unwrap();
        assert_eq!(
            cfg.resolved_settings_path(&resolved),
            PathBuf::from("/srv/work/settings.json"),
        );
    }

    #[test]
    fn resolved_settings_path_default_matches_claude_settings_path() {
        // For the unnamed default, resolved_settings_path must equal the legacy
        // claude_settings_path so startup/shutdown hook management stays consistent.
        let cfg = config_from_toml("claude_config_dir = \"/srv/legacy\"\n");
        let resolved = cfg.resolve_account(None).unwrap();
        assert_eq!(cfg.resolved_settings_path(&resolved), cfg.claude_settings_path());
    }

    #[test]
    fn all_settings_paths_includes_default_and_accounts_deduped() {
        let cfg = config_from_toml(
            "claude_config_dir = \"/srv/legacy\"\n\
             [accounts]\nwork = \"/srv/work\"\npersonal = \"/srv/legacy\"\n",
        );
        let paths = cfg.all_settings_paths();
        assert!(paths.contains(&PathBuf::from("/srv/legacy/settings.json")));
        assert!(paths.contains(&PathBuf::from("/srv/work/settings.json")));
        // /srv/legacy is both the default dir and the "personal" account dir, but
        // must appear only once.
        let legacy_count = paths
            .iter()
            .filter(|p| *p == &PathBuf::from("/srv/legacy/settings.json"))
            .count();
        assert_eq!(legacy_count, 1, "duplicate dirs must be collapsed: {:?}", paths);
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
        // Advisory lock file: flock'd, never written, so never truncated.
        .truncate(false)
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
    /// Verbosity level: 0 = warn, 1 = info, 2 = debug, 3 = trace (default: 0).
    /// Overridden by CLI `-v` flags or `RUST_LOG`.
    #[serde(default)]
    pub verbose: u8,
    #[serde(default)]
    pub tmux: SlopdTmuxConfig,
    #[serde(default)]
    pub run: SlopdRunConfig,
    /// Claude config dir (mirrors CLAUDE_CONFIG_DIR; default: ~/.claude) for the
    /// reserved [`DEFAULT_ACCOUNT`] account — the one used when no account is
    /// selected. Shorthand for `[accounts.default] claude_config_dir = ...`.
    /// Supports `~` and `$VAR` / `${VAR}` expansion.
    pub claude_config_dir: Option<PathBuf>,
    /// Named Claude accounts. Each maps an account name to its own configuration
    /// (at minimum a Claude config dir, the per-account equivalent of
    /// `claude_config_dir`). Select one for a pane with
    /// `slopctl run --account <name>`; child panes inherit their parent's
    /// account unless overridden. The name `default` is reserved (see
    /// [`DEFAULT_ACCOUNT`]).
    #[serde(default)]
    pub accounts: std::collections::BTreeMap<String, AccountConfig>,
    /// Account used by `slopctl run` when no `--account` is given and none is
    /// inherited from the parent pane. When unset, the [`DEFAULT_ACCOUNT`]
    /// account is used.
    pub default_account: Option<String>,
}

/// The reserved account name used when nothing else selects one. Its config dir
/// comes from `[accounts.default]` if present, otherwise the top-level
/// `claude_config_dir`, otherwise Claude's own `~/.claude`.
pub const DEFAULT_ACCOUNT: &str = "default";

/// Configuration for a single named account. Accepts either a bare string (the
/// Claude config dir, the common case) or a table for richer per-account
/// settings, so both of these are valid:
///
/// ```toml
/// [accounts]
/// work = "~/.config/claude-work"          # shorthand: just the dir
///
/// [accounts.personal]
/// claude_config_dir = "~/.config/claude-personal"   # table form (extensible)
/// ```
///
/// The table form is where future per-account options live (see
/// [`AccountSettings`]); the bare-string form is sugar for a table with only
/// `claude_config_dir` set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AccountConfig {
    /// Shorthand: the account is just its Claude config directory.
    Dir(PathBuf),
    /// Full table form, extensible with further per-account keys over time.
    Settings(AccountSettings),
}

/// The table form of a per-account configuration. New per-account options are
/// added here as fields (give each a `#[serde(default)]` so the table stays
/// backward-compatible), plus a matching accessor on [`AccountConfig`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountSettings {
    /// The account's Claude config directory.
    pub claude_config_dir: PathBuf,
    // Future per-account options go here, e.g.:
    //   #[serde(default)] pub executable: Option<Executable>,
    //   #[serde(default)] pub env: std::collections::BTreeMap<String, String>,
}

impl AccountConfig {
    /// The account's Claude config directory, as written in config (unexpanded).
    pub fn claude_config_dir(&self) -> &PathBuf {
        match self {
            AccountConfig::Dir(p) => p,
            AccountConfig::Settings(s) => &s.claude_config_dir,
        }
    }
}

/// The outcome of resolving a requested account name against the config: the
/// account that is in effect and the Claude config dir to hand the new pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAccount {
    /// The account name in effect (always set; [`DEFAULT_ACCOUNT`] for the
    /// default). Recorded on the pane as `@slopd_account` so it shows in `ps`
    /// and child panes can inherit it.
    pub name: String,
    /// The Claude config dir to export as `CLAUDE_CONFIG_DIR`. `None` means
    /// leave `CLAUDE_CONFIG_DIR` unset so Claude falls back to `~/.claude`.
    pub config_dir: Option<PathBuf>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SlopdTmuxConfig {
    /// Path to a custom tmux socket (`tmux -S`). Supports `~` and `$VAR` /
    /// `${VAR}` expansion.
    pub socket: Option<PathBuf>,
    /// Run `tmux start-server` on startup (default: true when socket is not set).
    pub start_server: Option<bool>,
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
    #[serde(default)]
    pub executable: Executable,
    /// Path to slopctl binary used for hook injection (default: "slopctl")
    #[serde(default = "default_slopctl")]
    pub slopctl: String,
    /// Default working directory for new Claude panes. Supports `~` and
    /// `$VAR` / `${VAR}` expansion. Overridden per-session by
    /// `slopctl run --start-directory`.
    pub start_directory: Option<PathBuf>,
    /// Extra environment variables for every new Claude pane. Values support
    /// `$VAR` / `${VAR}` expansion against slopd's environment at spawn time.
    /// Merged with (and overridden by) `slopctl run --env` / `--env-file`.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Paths to env-files loaded for every new Claude pane. Paths support
    /// `~` / `$VAR` expansion. Files are loaded in order; later files and
    /// [run.env] entries override earlier ones. CLI `--env-file` / `--env`
    /// override all of these.
    #[serde(default)]
    pub env_files: Vec<PathBuf>,
}

fn default_slopctl() -> String {
    "slopctl".to_string()
}

/// Resolve the configured slopctl path to an absolute path if the bare name
/// is not found on PATH. Falls back to a sibling of the current executable
/// (e.g. when running via `cargo run`).
pub fn resolve_slopctl(configured: &str) -> String {
    // If it's already an absolute path, keep it.
    if configured.starts_with('/') {
        return configured.to_string();
    }
    // If found on PATH, use the bare name.
    if which::which(configured).is_ok() {
        return configured.to_string();
    }
    // Try sibling of the current executable.
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.with_file_name(configured);
        if sibling.exists() {
            return sibling.to_string_lossy().into_owned();
        }
    }
    // Give up — return the original and let it fail at hook time.
    configured.to_string()
}

impl Default for SlopdRunConfig {
    fn default() -> Self {
        Self {
            executable: Executable::default(),
            slopctl: default_slopctl(),
            start_directory: None,
            env: std::collections::BTreeMap::new(),
            env_files: Vec::new(),
        }
    }
}

impl SlopdConfig {
    pub fn load() -> Self {
        let path = Self::config_path();
        load_config(path)
    }

    /// Path to the slopd config file (`$XDG_CONFIG_HOME/slopd/config.toml`).
    pub fn config_path() -> PathBuf {
        config_dir().join("slopd/config.toml")
    }

    /// Load and parse the config from `path`, propagating I/O and parse errors
    /// instead of warning-and-defaulting like `load()`. A missing file returns
    /// `Ok(default)` because that's the documented "no config" behavior. Used
    /// by SIGHUP reload, where a parse error must keep the previous config
    /// rather than silently dropping back to defaults.
    pub fn try_load_from(path: &std::path::Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(contents) => toml::from_str(&contents)
                .map_err(|e| format!("failed to parse {}: {}", path.display(), e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("failed to read {}: {}", path.display(), e)),
        }
    }

    pub fn claude_config_dir(&self) -> PathBuf {
        self.claude_config_dir
            .as_deref()
            .map(expand_path)
            .unwrap_or_else(|| home_dir().join(".claude"))
    }

    pub fn claude_settings_path(&self) -> PathBuf {
        self.claude_config_dir().join("settings.json")
    }

    /// Resolve a requested account name into the account in effect and the
    /// Claude config dir to export for a new pane.
    ///
    /// The account name is `requested`, else `default_account`, else
    /// [`DEFAULT_ACCOUNT`]. The dir is then:
    /// - for [`DEFAULT_ACCOUNT`]: `[accounts.default]` if present, else the
    ///   top-level `claude_config_dir`, else `None` (Claude's `~/.claude`);
    /// - for any other name: `[accounts.<name>]`, or an error (listing the
    ///   configured accounts) if it is not configured.
    ///
    /// All config dirs — named accounts and the top-level `claude_config_dir` —
    /// are `~` / `$VAR`-expanded.
    pub fn resolve_account(&self, requested: Option<&str>) -> Result<ResolvedAccount, String> {
        let name = requested
            .map(str::to_string)
            .or_else(|| self.default_account.clone())
            .unwrap_or_else(|| DEFAULT_ACCOUNT.to_string());

        // The default account is backed by [accounts.default], then the
        // top-level claude_config_dir, then ~/.claude (left unset).
        if name == DEFAULT_ACCOUNT {
            let config_dir = self
                .accounts
                .get(DEFAULT_ACCOUNT)
                .map(|a| expand_path(a.claude_config_dir()))
                .or_else(|| self.claude_config_dir.as_deref().map(expand_path));
            return Ok(ResolvedAccount { name, config_dir });
        }

        let account = self.accounts.get(&name).ok_or_else(|| {
            let mut configured: Vec<&str> = self.accounts.keys().map(String::as_str).collect();
            configured.push(DEFAULT_ACCOUNT);
            format!(
                "unknown account {:?} (configured accounts: {})",
                name,
                configured.join(", "),
            )
        })?;
        Ok(ResolvedAccount {
            name,
            config_dir: Some(expand_path(account.claude_config_dir())),
        })
    }

    /// The `settings.json` path where hooks are injected for a resolved account.
    /// Falls back to `~/.claude/settings.json` when no dir is in effect, so it
    /// always names a concrete file.
    pub fn resolved_settings_path(&self, resolved: &ResolvedAccount) -> PathBuf {
        resolved
            .config_dir
            .clone()
            .unwrap_or_else(|| home_dir().join(".claude"))
            .join("settings.json")
    }

    /// Every distinct `settings.json` slopd may manage hooks in: the default
    /// account plus every configured account, deduplicated. Used at startup
    /// recovery, shutdown, and `uninject-hooks`, where the account of each
    /// (possibly recovered) pane is not individually known.
    pub fn all_settings_paths(&self) -> Vec<PathBuf> {
        let mut names: std::collections::BTreeSet<&str> =
            self.accounts.keys().map(String::as_str).collect();
        names.insert(DEFAULT_ACCOUNT);

        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for name in names {
            // resolve_account only errors for unknown named accounts; every name
            // here comes from the config (or is DEFAULT_ACCOUNT), so this holds.
            if let Ok(resolved) = self.resolve_account(Some(name)) {
                let path = self.resolved_settings_path(&resolved);
                if seen.insert(path.clone()) {
                    out.push(path);
                }
            }
        }
        out
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

/// One step in a jq-style payload path. Segments are separated by `.` in the
/// surface syntax; `[]` and `[N]` may follow any key segment any number of
/// times.
///
/// Example parse: `message.content[].type` →
/// `[Key("message"), Key("content"), AnyElement, Key("type")]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PathSegment {
    /// Object key access (`.foo`).
    Key(String),
    /// Array index access (`[3]`).
    Index(usize),
    /// "Any element" of an array (`[]`). Matches if the rest of the path
    /// reaches an equal scalar via at least one element.
    AnyElement,
}

/// A parsed jq-style payload path. Constructed via `parse_payload_path`.
pub type PayloadPath = Vec<PathSegment>;

/// Parse a jq-style path. Accepts an optional leading `.`. Each segment is
/// either a non-empty identifier-like key or `[]` / `[N]` immediately after a
/// key. Empty path (just `""` or `"."`) is allowed and means "the value
/// itself."
///
/// Returns Err with a human-readable message on malformed input.
pub fn parse_payload_path(raw: &str) -> Result<PayloadPath, String> {
    let trimmed = raw.strip_prefix('.').unwrap_or(raw);
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut out: PayloadPath = Vec::new();
    for piece in trimmed.split('.') {
        if piece.is_empty() {
            return Err(format!("invalid path {:?}: empty segment between dots", raw));
        }
        // A piece is `name`, `name[]`, `name[3]`, `name[][3]`, etc.
        // Find the first `[` (if any); everything before it is the key, the
        // rest is a sequence of `[…]` brackets.
        let (key, brackets) = match piece.find('[') {
            Some(i) => (&piece[..i], &piece[i..]),
            None => (piece, ""),
        };
        if key.is_empty() {
            return Err(format!(
                "invalid path {:?}: bracket without preceding key in segment {:?}",
                raw, piece,
            ));
        }
        out.push(PathSegment::Key(key.to_string()));
        let mut rest = brackets;
        while !rest.is_empty() {
            let close = rest.find(']').ok_or_else(|| {
                format!("invalid path {:?}: unclosed `[` in segment {:?}", raw, piece)
            })?;
            let inside = &rest[1..close];
            if inside.is_empty() {
                out.push(PathSegment::AnyElement);
            } else {
                let n: usize = inside.parse().map_err(|_| {
                    format!(
                        "invalid path {:?}: array index {:?} is not a non-negative integer",
                        raw, inside,
                    )
                })?;
                out.push(PathSegment::Index(n));
            }
            rest = &rest[close + 1..];
        }
    }
    Ok(out)
}

/// Walk a JSON value following the path; return true if any reachable scalar
/// at the end of the path equals `expected` (string-equal after JSON
/// stringification for numbers/bools/null). Arrays and objects never match a
/// scalar `expected`.
pub fn path_matches(value: &serde_json::Value, path: &[PathSegment], expected: &str) -> bool {
    fn walk(v: &serde_json::Value, path: &[PathSegment], expected: &str) -> bool {
        let Some((head, rest)) = path.split_first() else {
            return scalar_eq(v, expected);
        };
        match head {
            PathSegment::Key(k) => match v.get(k) {
                Some(child) => walk(child, rest, expected),
                None => false,
            },
            PathSegment::Index(i) => match v.get(*i) {
                Some(child) => walk(child, rest, expected),
                None => false,
            },
            PathSegment::AnyElement => match v.as_array() {
                Some(arr) => arr.iter().any(|child| walk(child, rest, expected)),
                None => false,
            },
        }
    }
    walk(value, path, expected)
}

fn scalar_eq(v: &serde_json::Value, expected: &str) -> bool {
    match v {
        serde_json::Value::String(s) => s == expected,
        serde_json::Value::Null => expected == "null",
        serde_json::Value::Bool(b) => b.to_string() == expected,
        serde_json::Value::Number(n) => n.to_string() == expected,
        // Arrays and objects intentionally don't match scalar string values.
        _ => false,
    }
}

/// A parsed predicate against an event's `payload`: a jq-style path plus the
/// expected scalar (string-equal comparison). Used both client-side
/// (`wait --until`) and on the wire (`EventFilter::payload_path_match` for
/// `listen --where`). Construct via `parse_payload_predicate`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PayloadPredicate {
    pub path: PayloadPath,
    pub expected: String,
}

/// Parse a single `KEY=VALUE` predicate where KEY is a jq-style path. Returns
/// a human-readable error on malformed input.
pub fn parse_payload_predicate(raw: &str) -> Result<PayloadPredicate, String> {
    let (key, value) = raw.split_once('=').ok_or_else(|| {
        format!("invalid predicate {:?}: expected KEY=VALUE", raw)
    })?;
    let path = parse_payload_path(key)
        .map_err(|e| format!("invalid predicate {:?}: {}", raw, e))?;
    Ok(PayloadPredicate {
        path,
        expected: value.to_string(),
    })
}

/// Parse many `KEY=VALUE` predicates in flag order. Used by both `--until`
/// and `--where`.
pub fn parse_payload_predicates(raw: Vec<String>) -> Result<Vec<PayloadPredicate>, String> {
    raw.into_iter().map(|p| parse_payload_predicate(&p)).collect()
}

/// True iff every predicate matches the value (AND).
pub fn predicates_match(value: &serde_json::Value, predicates: &[PayloadPredicate]) -> bool {
    predicates.iter().all(|p| path_matches(value, &p.path, &p.expected))
}

/// Describes which events a subscriber wants to receive.
/// All specified fields must match (AND within one filter).
/// Multiple filters in a Subscribe request are OR-ed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// jq-style path predicates that must all match. See `PayloadPredicate`.
    #[serde(default)]
    pub payload_path_match: Vec<PayloadPredicate>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RequestBody {
    Status,
    Run {
        parent_pane_id: Option<String>,
        extra_args: Vec<String>,
        start_directory: Option<PathBuf>,
        /// Extra environment variables for the new pane (client-side-resolved).
        /// Merged after the daemon's `[run.env]` config; later pairs win.
        #[serde(default)]
        env: Vec<(String, String)>,
        /// Named account to launch the pane under. The daemon resolves it to a
        /// Claude config dir via `[accounts]`. `None` means the daemon default
        /// (`default_account`, else the unnamed `claude_config_dir`).
        #[serde(default)]
        account: Option<String>,
    },
    Kill { pane_id: String },
    Hook { event: String, payload: serde_json::Value, pane_id: Option<String> },
    /// Notification from a tmux hook (called by slopctl tmux-hook).
    TmuxHook { event: String, pane_id: Option<String> },
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
    /// Cancel a subscription previously created by Subscribe or SubscribeTranscript.
    /// The `id` field in the outer Request identifies the Unsubscribe request itself;
    /// `subscription_id` is the `id` of the original Subscribe/SubscribeTranscript request.
    Unsubscribe { subscription_id: u64 },
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
    TmuxHooked,
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
    /// Confirms that a subscription has been cancelled.
    Unsubscribed { subscription_id: u64 },
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

    // Option-returning parser paired with `as_str`; deliberately not the std
    // `FromStr` trait (which returns `Result`).
    #[allow(clippy::should_implement_trait)]
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

    // Option-returning parser paired with `as_str`; deliberately not the std
    // `FromStr` trait (which returns `Result`).
    #[allow(clippy::should_implement_trait)]
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
    /// Current working directory of the pane (#{pane_current_path}).
    #[serde(default)]
    pub working_dir: Option<String>,
    /// The account the pane was launched under (from @slopd_account). Defaults
    /// to [`DEFAULT_ACCOUNT`] for panes with no recorded account.
    #[serde(default = "default_account_name")]
    pub account: String,
}

/// Serde default for [`PaneInfo::account`]: the reserved default account name.
fn default_account_name() -> String {
    DEFAULT_ACCOUNT.to_string()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonState {
    pub uptime_secs: u64,
    /// Number of broadcast::Receiver instances currently held by event-streaming
    /// subscriber tasks. Useful for verifying that subscriptions are reaped when
    /// their owning connection closes.
    #[serde(default)]
    pub subscriber_count: u64,
    /// Generation counter incremented on every successful SIGHUP reload.
    /// 0 = initial config; 1 = after the first successful reload; etc. Failed
    /// reloads (parse errors, missing files report as the previous generation)
    /// do not advance this counter.
    #[serde(default)]
    pub config_generation: u64,
}
