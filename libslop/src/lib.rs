pub mod jsonl;

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

/// The XDG runtime directory (`$XDG_RUNTIME_DIR`), where slopd keeps its control
/// socket. When the variable is unset — cron jobs, non-login ssh, containers —
/// the XDG Base Directory spec says to fall back to a replacement directory with
/// similar capabilities and warn, rather than fail. We prefer `/run/user/<uid>`
/// if it already exists (what a login session would use), else a private `0700`
/// dir under the temp dir. slopd and slopctl share this function, so they agree
/// on the location either way.
pub fn runtime_dir() -> PathBuf {
    if let Some(dir) = dirs::runtime_dir() {
        return dir;
    }
    let uid = current_uid();
    let run_user_exists = std::path::Path::new(&format!("/run/user/{uid}")).is_dir();
    let (dir, source) = resolve_runtime_fallback(uid, run_user_exists, &std::env::temp_dir());
    warn_runtime_fallback(&dir);
    if source == RuntimeDirSource::Temp {
        // Our own fallback must satisfy the spec's owner-only (0700) requirement,
        // since it holds the control socket and may live in a shared temp dir.
        if let Err(e) = ensure_private_dir(&dir) {
            eprintln!(
                "warning: failed to create runtime dir {}: {}",
                dir.display(),
                e
            );
        }
    }
    dir
}

/// Which fallback [`runtime_dir`] resolved to, used to decide whether slopd must
/// create the directory itself.
#[derive(Debug, PartialEq, Eq)]
enum RuntimeDirSource {
    /// `/run/user/<uid>` already exists (e.g. a login session whose
    /// `XDG_RUNTIME_DIR` simply wasn't exported into this process).
    RunUser,
    /// A private directory under the temp dir, which slopd creates `0700`.
    Temp,
}

/// Pure fallback decision for [`runtime_dir`] (no I/O), split out so it can be
/// unit-tested deterministically.
fn resolve_runtime_fallback(
    uid: u32,
    run_user_exists: bool,
    temp_dir: &std::path::Path,
) -> (PathBuf, RuntimeDirSource) {
    if run_user_exists {
        (
            PathBuf::from(format!("/run/user/{uid}")),
            RuntimeDirSource::RunUser,
        )
    } else {
        (
            temp_dir.join(format!("slopd-{uid}")),
            RuntimeDirSource::Temp,
        )
    }
}

fn current_uid() -> u32 {
    // getuid() always succeeds and has no preconditions.
    unsafe { libc::getuid() }
}

/// Warn once per process that `$XDG_RUNTIME_DIR` is unset and we fell back.
fn warn_runtime_fallback(dir: &std::path::Path) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        eprintln!(
            "warning: $XDG_RUNTIME_DIR is not set; falling back to {} (per the XDG Base Directory spec)",
            dir.display()
        );
    });
}

/// Create `dir` (and parents) with `0700` perms, enforcing the mode even if it
/// already exists with looser permissions.
fn ensure_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir().expect("could not determine XDG config dir")
}

pub fn home_dir() -> PathBuf {
    dirs::home_dir().expect("could not determine home dir")
}

/// The XDG state directory (`$XDG_STATE_HOME`, default `~/.local/state`), where
/// slopd keeps state that should persist across reboots — unlike the runtime
/// dir (the socket), which is wiped on reboot. Used for the lifecycle journal.
pub fn state_dir() -> PathBuf {
    dirs::state_dir().unwrap_or_else(|| home_dir().join(".local/state"))
}

/// Path to the legacy pane backup manifest (`$XDG_STATE_HOME/slopd/panes.json`).
///
/// Current slopd versions import this old single-file format into the lifecycle
/// journal for the default tmux target.
pub fn panes_manifest_path() -> PathBuf {
    state_dir().join("slopd/panes.json")
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

/// Resolve `program` to an absolute executable path, searching `path` (a
/// PATH-style value) and resolving relative names against `cwd` — mirroring how
/// a spawned pane looks it up. `None` if it can't be found.
///
/// slopd spawns agent panes with this *absolute* path rather than the bare
/// program name, so a pane never depends on its own inherited PATH to locate
/// the executable. That is what made restore silently fail after a reboot:
/// systemd user services start with a minimal PATH that omits `~/.local/bin`
/// (where `claude` lives), so every restored pane's `claude` was not found and
/// the pane died instantly. Resolving up front against slopd's PATH removes the
/// dependency entirely.
pub fn resolve_executable(
    program: &str,
    path: &std::ffi::OsStr,
    cwd: &std::path::Path,
) -> Option<PathBuf> {
    which::which_in(program, Some(path), cwd).ok()
}

/// Whether `program` resolves to an executable (see [`resolve_executable`]).
/// Lets `run` fail fast with a clear message when the configured executable is
/// missing, instead of spawning a pane that just dies.
pub fn executable_exists(program: &str, path: &std::ffi::OsStr, cwd: &std::path::Path) -> bool {
    resolve_executable(program, path, cwd).is_some()
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
    let expanded =
        expand_env_value(value).map_err(|e| format!("invalid --env {:?}: {}", raw, e))?;
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
        let (k, v) =
            item.map_err(|e| format!("failed to parse env file {}: {}", path.display(), e))?;
        out.push((k, v));
    }
    Ok(out)
}

pub enum TmuxOption {
    /// Marks the slopd-managed tmux session; value is "true"
    SlopdManaged,
    /// Stores the backend session ID on a pane.
    SlopdSessionId,
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
    /// Stores the pane's agent backend; unset = claude.
    SlopdBackend,
    /// For opencode panes: the embedded HTTP server port slopd drives the pane over.
    SlopdOpencodePort,
    /// For opencode panes: the per-pane basic-auth token for that server.
    SlopdOpencodeToken,
    /// For Grok panes: the private leader socket shared by the visible TUI and
    /// slopd's ACP sidecar.
    SlopdGrokLeaderSocket,
}

impl TmuxOption {
    pub fn as_str(&self) -> &'static str {
        match self {
            TmuxOption::SlopdManaged => "@slopd_managed",
            TmuxOption::SlopdSessionId => "@slopd_session_id",
            TmuxOption::SlopdAncestorPanes => "@slopd_ancestor_panes",
            TmuxOption::SlopdState => "@slopd_state",
            TmuxOption::SlopdDetailedState => "@slopd_detailed_state",
            TmuxOption::SlopdCreatedAt => "@slopd_created_at",
            TmuxOption::SlopdTranscriptPath => "@slopd_transcript_path",
            TmuxOption::SlopdAccount => "@slopd_account",
            TmuxOption::SlopdBackend => "@slopd_backend",
            TmuxOption::SlopdOpencodePort => "@slopd_opencode_port",
            TmuxOption::SlopdOpencodeToken => "@slopd_opencode_token",
            TmuxOption::SlopdGrokLeaderSocket => "@slopd_grok_leader_socket",
        }
    }
}

/// Validate a user-supplied tag name and return the full tmux option name.
/// Tag names must match `[A-Za-z0-9_-]+` (what tmux accepts in option names).
pub fn tag_option_name(tag: &str) -> Result<String, String> {
    if tag.is_empty() {
        return Err("tag name must not be empty".to_string());
    }
    if !tag
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
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

/// Hook events currently emitted by Codex CLI. Keep this separate from
/// [`HOOK_EVENTS`]: Codex rejects unknown event names in `hooks.json`.
pub const CODEX_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PreCompact",
    "PostCompact",
    "UserPromptSubmit",
    "SubagentStart",
    "SubagentStop",
    "Stop",
];

/// Hook events emitted by Grok Build. Grok accepts Claude-compatible hook JSON,
/// but its event vocabulary differs in several important places: cancellation
/// is distinct from a successful stop, and denied permissions are observable.
pub const GROK_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionDenied",
    "Stop",
    "StopFailure",
    "StopCancelled",
    "Notification",
    "SubagentStart",
    "SubagentStop",
    "PreCompact",
    "PostCompact",
    "SessionEnd",
];

/// Idempotently inject slopctl hook entries into a hook configuration value.
/// Adds our hook command for each event only if not already present.
pub fn inject_hooks(settings: &mut serde_json::Value, slopctl: &str) {
    inject_hook_events(settings, slopctl, HOOK_EVENTS);
}

/// Idempotently inject the hook set supported by `backend`.
pub fn inject_backend_hooks(settings: &mut serde_json::Value, slopctl: &str, backend: Backend) {
    inject_hook_events(settings, slopctl, backend.hook_events());
}

fn inject_hook_events(settings: &mut serde_json::Value, slopctl: &str, hook_events: &[&str]) {
    let hooks = settings
        .as_object_mut()
        .expect("hook configuration must be an object")
        .entry("hooks")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .expect("hooks must be an object");

    for &event in hook_events {
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
            let is_stale = entry
                .get("hooks")
                .and_then(|h| h.as_array())
                .is_some_and(|hooks_arr| {
                    hooks_arr.iter().any(|h| {
                        if h.get("type").and_then(|t| t.as_str()) != Some("command") {
                            return false;
                        }
                        let cmd = h.get("command").and_then(|c| c.as_str()).unwrap_or("");
                        if !cmd.ends_with(&stale_suffix) || cmd == command {
                            return false;
                        }
                        // Only remove entries whose executable is slopctl (plain or
                        // absolute path). Match the first token so a command that
                        // carries args (e.g. `slopctl --socket <path> hook ...`) is
                        // still recognized as ours.
                        let prefix = &cmd[..cmd.len() - stale_suffix.len()];
                        let exe = prefix.split_whitespace().next().unwrap_or("");
                        exe == "slopctl" || exe.ends_with("/slopctl")
                    })
                });
            !is_stale
        });

        let already_present = entries.iter().any(|entry| {
            entry
                .get("hooks")
                .and_then(|h| h.as_array())
                .is_some_and(|hooks_arr| {
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

/// Remove all slopctl hook entries from a hook configuration value.
/// Entries from other tools are preserved.
pub fn remove_hooks(settings: &mut serde_json::Value) {
    remove_hook_events(settings, HOOK_EVENTS);
}

/// Remove slopctl hooks supported by `backend`, preserving foreign hooks.
pub fn remove_backend_hooks(settings: &mut serde_json::Value, backend: Backend) {
    remove_hook_events(settings, backend.hook_events());
}

fn remove_hook_events(settings: &mut serde_json::Value, hook_events: &[&str]) {
    let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return;
    };

    for &event in hook_events {
        let Some(entries) = hooks.get_mut(event).and_then(|e| e.as_array_mut()) else {
            continue;
        };
        let suffix = format!(" hook {}", event);
        entries.retain(|entry| {
            let is_ours = entry
                .get("hooks")
                .and_then(|h| h.as_array())
                .is_some_and(|hooks_arr| {
                    hooks_arr.iter().any(|h| {
                        if h.get("type").and_then(|t| t.as_str()) != Some("command") {
                            return false;
                        }
                        let cmd = h.get("command").and_then(|c| c.as_str()).unwrap_or("");
                        if !cmd.ends_with(&suffix) {
                            return false;
                        }
                        // Match the first token so `slopctl --socket <path> hook ...`
                        // entries are removed too, not just the bare form.
                        let prefix = &cmd[..cmd.len() - suffix.len()];
                        let exe = prefix.split_whitespace().next().unwrap_or("");
                        exe == "slopctl" || exe.ends_with("/slopctl")
                    })
                });
            !is_ours
        });
    }
}

/// Read, remove slopctl hooks, and write a Claude settings.json file.
pub fn remove_hooks_from_file(settings_path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    remove_hook_events_from_file(settings_path, HOOK_EVENTS)
}

/// Read, remove slopctl hooks for `backend`, and atomically write the file.
pub fn remove_backend_hooks_from_file(
    settings_path: &PathBuf,
    backend: Backend,
) -> Result<(), Box<dyn std::error::Error>> {
    remove_hook_events_from_file(settings_path, backend.hook_events())
}

fn remove_hook_events_from_file(
    settings_path: &PathBuf,
    hook_events: &[&str],
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

    remove_hook_events(&mut settings, hook_events);

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
    fn normalize_pane_title_strips_status_glyphs_and_oc_prefix() {
        // Real samples (verified against live panes).
        assert_eq!(
            normalize_pane_title("OC | tg-dm-responder takeover").as_deref(),
            Some("tg-dm-responder takeover")
        );
        assert_eq!(
            normalize_pane_title("✳ orchestrator").as_deref(),
            Some("orchestrator")
        );
        assert_eq!(normalize_pane_title("⠂ slopd").as_deref(), Some("slopd"));
        assert_eq!(
            normalize_pane_title("⠐ Review z.ai models and usage billing").as_deref(),
            Some("Review z.ai models and usage billing")
        );
        // Spinner glyph in front of the opencode prefix (both stripped, either order).
        assert_eq!(
            normalize_pane_title("⠐ OC | building").as_deref(),
            Some("building")
        );
        // A plain title is preserved; a title starting with a digit is kept intact.
        assert_eq!(
            normalize_pane_title("plain title").as_deref(),
            Some("plain title")
        );
        assert_eq!(
            normalize_pane_title("42 fixes").as_deref(),
            Some("42 fixes")
        );
        // Nothing informative → None.
        assert_eq!(normalize_pane_title("✳"), None);
        assert_eq!(normalize_pane_title("   "), None);
    }

    #[test]
    fn runtime_fallback_prefers_run_user_then_private_temp() {
        let temp = std::path::Path::new("/tmp");
        // /run/user/<uid> present -> use it (what a login session would).
        let (dir, src) = resolve_runtime_fallback(1000, true, temp);
        assert_eq!(dir, PathBuf::from("/run/user/1000"));
        assert_eq!(src, RuntimeDirSource::RunUser);
        // No /run/user/<uid> -> a per-uid private dir under the temp dir, so two
        // users on one host don't collide on the control socket.
        let (dir, src) = resolve_runtime_fallback(4242, false, temp);
        assert_eq!(dir, PathBuf::from("/tmp/slopd-4242"));
        assert_eq!(src, RuntimeDirSource::Temp);
    }

    #[test]
    fn executable_exists_finds_path_binaries_not_bogus_names() {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let cwd = std::env::current_dir().unwrap();
        // `sh` is on PATH on any unix host the tests run on.
        assert!(
            executable_exists("sh", &path, &cwd),
            "sh should resolve on PATH"
        );
        // A bogus name resolves nowhere.
        assert!(
            !executable_exists("slopd-no-such-binary-zzz", &path, &cwd),
            "a name that isn't on PATH must not resolve"
        );
        // An absolute path to a real binary resolves regardless of PATH.
        assert!(
            executable_exists("/bin/sh", std::ffi::OsStr::new(""), &cwd),
            "an absolute path to a real binary should resolve"
        );
    }

    #[test]
    fn resolve_executable_returns_an_absolute_path() {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let cwd = std::env::current_dir().unwrap();
        // A bare name on PATH resolves to its absolute location — so slopd can
        // spawn that path and the pane never needs the program on its own PATH
        // (the architectural fix for the post-reboot restore failure).
        let resolved = resolve_executable("sh", &path, &cwd).expect("sh should resolve on PATH");
        assert!(
            resolved.is_absolute(),
            "resolved executable must be absolute; got {:?}",
            resolved
        );
        // A bogus name resolves nowhere.
        assert!(resolve_executable("slopd-no-such-binary-zzz", &path, &cwd).is_none());
    }

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
            h.join()
                .unwrap()
                .unwrap_or_else(|e| panic!("inject_hooks_into_file failed: {}", e));
        }

        let contents = std::fs::read_to_string(&path).unwrap();
        let settings: serde_json::Value = serde_json::from_str(&contents).unwrap();

        for &event in HOOK_EVENTS {
            let entries = settings["hooks"][event]
                .as_array()
                .unwrap_or_else(|| panic!("missing hooks.{}", event));
            let count = entries
                .iter()
                .filter(|entry| {
                    entry["hooks"].as_array().is_some_and(|hooks| {
                        hooks.iter().any(|h| {
                            h["type"] == "command"
                                && h["command"]
                                    .as_str()
                                    .is_some_and(|c| c.contains("slopctl") && c.contains(event))
                        })
                    })
                })
                .count();
            assert_eq!(count, 1, "event {} has {} entries, want 1", event, count);
        }

        let contents = std::fs::read_to_string(&path).unwrap();
        let settings: serde_json::Value = serde_json::from_str(&contents).unwrap();

        for &event in HOOK_EVENTS {
            let entries = settings["hooks"][event]
                .as_array()
                .unwrap_or_else(|| panic!("missing hooks.{}", event));
            let count = entries
                .iter()
                .filter(|entry| {
                    entry["hooks"].as_array().is_some_and(|hooks| {
                        hooks.iter().any(|h| {
                            h["type"] == "command"
                                && h["command"]
                                    .as_str()
                                    .is_some_and(|c| c.contains("slopctl") && c.contains(event))
                        })
                    })
                })
                .count();
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
        let foobar_count = stop_entries
            .iter()
            .filter(|entry| {
                entry["hooks"].as_array().is_some_and(|hooks| {
                    hooks
                        .iter()
                        .any(|h| h["command"].as_str() == Some("foobar hook Stop"))
                })
            })
            .count();
        assert_eq!(
            foobar_count, 1,
            "foobar hook Stop entry was incorrectly removed"
        );

        // The slopctl entry must also be present.
        let slopctl_count = stop_entries
            .iter()
            .filter(|entry| {
                entry["hooks"].as_array().is_some_and(|hooks| {
                    hooks
                        .iter()
                        .any(|h| h["command"].as_str() == Some("slopctl hook Stop"))
                })
            })
            .count();
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
            let entries = settings["hooks"][event]
                .as_array()
                .unwrap_or_else(|| panic!("missing hooks.{}", event));

            // Old path entry must be gone.
            let old_count = entries
                .iter()
                .filter(|entry| {
                    entry["hooks"].as_array().is_some_and(|hooks| {
                        hooks.iter().any(|h| {
                            h["command"]
                                .as_str()
                                .is_some_and(|c| c.contains("/home/claude/.local/bin/slopctl"))
                        })
                    })
                })
                .count();
            assert_eq!(
                old_count, 0,
                "event {} still has stale absolute-path entry",
                event
            );

            // New entry must be present exactly once.
            let new_count = entries
                .iter()
                .filter(|entry| {
                    entry["hooks"].as_array().is_some_and(|hooks| {
                        hooks.iter().any(|h| {
                            h["command"]
                                .as_str()
                                .is_some_and(|c| c == format!("slopctl hook {}", event))
                        })
                    })
                })
                .count();
            assert_eq!(
                new_count, 1,
                "event {} has {} new-path entries, want 1",
                event, new_count
            );
        }
    }

    #[test]
    fn inject_hooks_with_socket_prefix_is_idempotent_and_swaps_stale() {
        // A `slopctl --socket <path>` command prefix (what SlopdConfig::hook_slopctl
        // produces under `--socket`) is written verbatim and is idempotent.
        let mut settings = serde_json::json!({});
        let with_sock = "slopctl --socket /run/x/slopd.sock";
        inject_hooks(&mut settings, with_sock);
        inject_hooks(&mut settings, with_sock);
        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(
            stop.len(),
            1,
            "re-injecting the same --socket command must not duplicate"
        );
        assert_eq!(
            stop[0]["hooks"][0]["command"],
            "slopctl --socket /run/x/slopd.sock hook Stop"
        );

        // Switching back to the bare command removes the stale --socket entry
        // (first-token match), leaving exactly the bare one.
        inject_hooks(&mut settings, "slopctl");
        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(
            stop.len(),
            1,
            "stale --socket entry should be replaced, not kept alongside"
        );
        assert_eq!(stop[0]["hooks"][0]["command"], "slopctl hook Stop");

        // A foreign tool's entry is never touched by either transition.
        if let Some(arr) = settings["hooks"]["Stop"].as_array_mut() {
            arr.push(serde_json::json!({"matcher": "", "hooks": [{"type": "command", "command": "claudex hook Stop"}]}));
        }
        inject_hooks(&mut settings, "slopctl --socket /run/y.sock");
        let has_foreign = settings["hooks"]["Stop"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| {
                e["hooks"]
                    .as_array()
                    .is_some_and(|h| h.iter().any(|x| x["command"] == "claudex hook Stop"))
            });
        assert!(
            has_foreign,
            "foreign (claudex) hook entry must be preserved across --socket re-injection"
        );

        // remove_hooks strips a --socket entry too (but leaves the foreign one).
        remove_hooks(&mut settings);
        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1, "only the foreign entry should remain");
        assert_eq!(stop[0]["hooks"][0]["command"], "claudex hook Stop");
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
            let entries = settings["hooks"][event]
                .as_array()
                .unwrap_or_else(|| panic!("missing hooks.{}", event));
            let slopctl_count = entries
                .iter()
                .filter(|entry| {
                    entry["hooks"].as_array().is_some_and(|hooks| {
                        hooks.iter().any(|h| {
                            h["type"] == "command"
                                && h["command"]
                                    .as_str()
                                    .is_some_and(|c| c.contains("slopctl") && c.contains(event))
                        })
                    })
                })
                .count();
            assert_eq!(
                slopctl_count, 0,
                "event {} still has {} slopctl entries",
                event, slopctl_count
            );
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
        let foobar_count = stop_entries
            .iter()
            .filter(|entry| {
                entry["hooks"].as_array().is_some_and(|hooks| {
                    hooks
                        .iter()
                        .any(|h| h["command"].as_str() == Some("foobar hook Stop"))
                })
            })
            .count();
        assert_eq!(
            foobar_count, 1,
            "foobar hook Stop entry was incorrectly removed"
        );
    }

    #[test]
    fn remove_hooks_handles_absolute_path_slopctl() {
        let mut settings = serde_json::json!({});
        inject_hooks(&mut settings, "/usr/local/bin/slopctl");

        remove_hooks(&mut settings);

        for &event in HOOK_EVENTS {
            let entries = settings["hooks"][event]
                .as_array()
                .unwrap_or_else(|| panic!("missing hooks.{}", event));
            let slopctl_count = entries
                .iter()
                .filter(|entry| {
                    entry["hooks"].as_array().is_some_and(|hooks| {
                        hooks
                            .iter()
                            .any(|h| h["command"].as_str().is_some_and(|c| c.contains("slopctl")))
                    })
                })
                .count();
            assert_eq!(
                slopctl_count, 0,
                "event {} still has slopctl entries after removal",
                event
            );
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
        assert!(
            !settings["hooks"]["SessionStart"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        remove_hooks_from_file(&path).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let settings: serde_json::Value = serde_json::from_str(&contents).unwrap();
        for &event in HOOK_EVENTS {
            let entries = settings["hooks"][event]
                .as_array()
                .unwrap_or_else(|| panic!("missing hooks.{}", event));
            assert_eq!(
                entries.len(),
                0,
                "event {} still has entries after removal",
                event
            );
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
        assert_eq!(
            result,
            std::path::PathBuf::from("/base/$__SLOPD_NONEXISTENT_VAR__/end")
        );
    }

    #[test]
    fn resolve_slopctl_absolute_path_returned_as_is() {
        assert_eq!(
            resolve_slopctl("/usr/local/bin/slopctl"),
            "/usr/local/bin/slopctl"
        );
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
            let entries = settings["hooks"][event]
                .as_array()
                .unwrap_or_else(|| panic!("missing hooks.{}", event));
            let slopctl_count = entries
                .iter()
                .filter(|entry| {
                    entry["hooks"].as_array().is_some_and(|hooks| {
                        hooks
                            .iter()
                            .any(|h| h["command"].as_str().is_some_and(|c| c.contains("slopctl")))
                    })
                })
                .count();
            assert_eq!(
                slopctl_count, 0,
                "event {} still has {} slopctl entries after removal",
                event, slopctl_count
            );
        }
    }

    // --- jq-style payload path tests ---

    fn p(s: &str) -> PayloadPath {
        parse_payload_path(s)
            .unwrap_or_else(|e| panic!("parse_payload_path({:?}) failed: {}", s, e))
    }

    #[test]
    fn parse_path_simple_keys() {
        assert_eq!(p("foo"), vec![PathSegment::Key("foo".into())]);
        assert_eq!(
            p("foo.bar"),
            vec![
                PathSegment::Key("foo".into()),
                PathSegment::Key("bar".into()),
            ]
        );
        // Leading dot is optional and equivalent.
        assert_eq!(p(".foo.bar"), p("foo.bar"));
    }

    #[test]
    fn parse_path_array_segments() {
        assert_eq!(
            p("foo[]"),
            vec![PathSegment::Key("foo".into()), PathSegment::AnyElement,]
        );
        assert_eq!(
            p("foo[0]"),
            vec![PathSegment::Key("foo".into()), PathSegment::Index(0),]
        );
        assert_eq!(
            p("foo[].bar"),
            vec![
                PathSegment::Key("foo".into()),
                PathSegment::AnyElement,
                PathSegment::Key("bar".into()),
            ]
        );
        assert_eq!(
            p("foo[0][1].bar"),
            vec![
                PathSegment::Key("foo".into()),
                PathSegment::Index(0),
                PathSegment::Index(1),
                PathSegment::Key("bar".into()),
            ]
        );
    }

    #[test]
    fn parse_path_empty_path() {
        assert_eq!(parse_payload_path("").unwrap(), Vec::<PathSegment>::new());
        assert_eq!(parse_payload_path(".").unwrap(), Vec::<PathSegment>::new());
    }

    #[test]
    fn parse_path_rejects_malformed() {
        assert!(
            parse_payload_path("foo..bar").is_err(),
            "double dot should fail"
        );
        assert!(
            parse_payload_path("[0]").is_err(),
            "leading bracket should fail"
        );
        assert!(
            parse_payload_path("foo[").is_err(),
            "unclosed bracket should fail"
        );
        assert!(
            parse_payload_path("foo[abc]").is_err(),
            "non-int index should fail"
        );
        assert!(
            parse_payload_path("foo[-1]").is_err(),
            "negative index not yet supported"
        );
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
    fn backup_path_is_rejected() {
        let error = toml::from_str::<SlopdConfig>("[backup]\npath = \"/tmp/panes.json\"\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown field `path`"), "{error}");
    }

    #[test]
    fn account_config_accepts_bare_string() {
        let cfg = config_from_toml("[accounts]\nwork = \"/srv/claude-work\"\n");
        let acct = cfg.accounts.get("work").expect("work account missing");
        assert_eq!(acct.config_dir(), Some(&PathBuf::from("/srv/claude-work")));
    }

    #[test]
    fn account_config_accepts_table_form() {
        let cfg = config_from_toml("[accounts.personal]\nconfig_dir = \"/srv/claude-personal\"\n");
        let acct = cfg
            .accounts
            .get("personal")
            .expect("personal account missing");
        assert_eq!(
            acct.config_dir(),
            Some(&PathBuf::from("/srv/claude-personal"))
        );
    }

    #[test]
    fn account_config_accepts_table_without_config_dir() {
        let cfg = config_from_toml("[accounts.codex]\nbackend = \"codex\"\n");
        let acct = cfg.accounts.get("codex").expect("codex account missing");
        assert_eq!(acct.config_dir(), None);
    }

    #[test]
    fn resolve_account_named_returns_name_and_dir() {
        let cfg = config_from_toml("[accounts]\nwork = \"/srv/work\"\n");
        let resolved = cfg.resolve_account(Some("work")).unwrap();
        assert_eq!(resolved.name, "work");
        assert_eq!(resolved.config_dir, Some(PathBuf::from("/srv/work")));
    }

    #[test]
    fn resolve_account_named_without_dir_uses_backend_default() {
        let cfg = config_from_toml("[accounts.codex]\nbackend = \"codex\"\n");
        let resolved = cfg.resolve_account(Some("codex")).unwrap();
        assert_eq!(resolved.name, "codex");
        assert_eq!(resolved.backend, Backend::Codex);
        assert_eq!(resolved.config_dir, None);
        assert_eq!(resolved.executable.program(), "codex");
    }

    #[test]
    fn resolve_account_unknown_errors_and_lists_configured() {
        let cfg = config_from_toml("[accounts]\nwork = \"/srv/work\"\n");
        let err = cfg.resolve_account(Some("nope")).unwrap_err();
        assert!(
            err.contains("nope"),
            "err should name the bad account: {}",
            err
        );
        assert!(
            err.contains("work"),
            "err should list configured accounts: {}",
            err
        );
        assert!(
            err.contains(DEFAULT_ACCOUNT),
            "err should list the default account: {}",
            err
        );
    }

    #[test]
    fn resolve_account_none_uses_default_account() {
        let cfg =
            config_from_toml("default_account = \"work\"\n[accounts]\nwork = \"/srv/work\"\n");
        let resolved = cfg.resolve_account(None).unwrap();
        assert_eq!(resolved.name, "work");
        assert_eq!(resolved.config_dir, Some(PathBuf::from("/srv/work")));
    }

    #[test]
    fn default_account_pointer_switches_between_named_backends() {
        let config = r#"
            default_account = "codex"

            [accounts.claude]

            [accounts.codex]
            backend = "codex"

            [accounts.opencode]
            backend = "opencode"

            [accounts.grok]
            backend = "grok"
        "#;
        let mut cfg = config_from_toml(config);

        for (name, backend) in [
            ("codex", Backend::Codex),
            ("claude", Backend::Claude),
            ("opencode", Backend::Opencode),
            ("grok", Backend::Grok),
        ] {
            cfg.default_account = Some(name.to_string());
            let resolved = cfg.resolve_account(None).unwrap();
            assert_eq!(resolved.name, name);
            assert_eq!(resolved.backend, backend);
            assert_eq!(resolved.config_dir, None);
        }
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
    fn resolve_account_default_uses_top_level_config_dir() {
        // Top-level config_dir backs the reserved "default" account.
        let cfg = config_from_toml("config_dir = \"/srv/legacy\"\n");
        for requested in [None, Some(DEFAULT_ACCOUNT)] {
            let resolved = cfg.resolve_account(requested).unwrap();
            assert_eq!(resolved.name, DEFAULT_ACCOUNT);
            assert_eq!(resolved.config_dir, Some(PathBuf::from("/srv/legacy")));
        }
    }

    #[test]
    fn resolve_account_explicit_default_table_overrides_top_level() {
        // [accounts.default] wins over the top-level config_dir shorthand.
        let cfg = config_from_toml(
            "config_dir = \"/srv/legacy\"\n[accounts]\ndefault = \"/srv/explicit\"\n",
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
        let cfg =
            config_from_toml("default_account = \"ghost\"\n[accounts]\nwork = \"/srv/work\"\n");
        assert!(
            cfg.resolve_account(None).is_err(),
            "None resolves to the bad default_account and errors"
        );
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
    fn resolve_account_expands_tilde_in_top_level_config_dir() {
        // The default account's top-level config_dir is `~`-expanded too.
        let cfg = config_from_toml("config_dir = \"~/claude-default\"\n");
        let resolved = cfg.resolve_account(None).unwrap();
        assert_eq!(resolved.config_dir, Some(home_dir().join("claude-default")));
    }

    #[test]
    fn config_dir_method_expands_tilde_and_var() {
        let cfg = config_from_toml("config_dir = \"~/claude-default\"\n");
        assert_eq!(cfg.config_dir(), home_dir().join("claude-default"));
        // SAFETY: single-threaded test; no other thread reads this var concurrently.
        unsafe { std::env::set_var("SLOPD_TEST_CC_DIR", "/tmp/cc") };
        let cfg = config_from_toml("config_dir = \"$SLOPD_TEST_CC_DIR/sub\"\n");
        assert_eq!(cfg.config_dir(), PathBuf::from("/tmp/cc/sub"));
    }

    // --- backend + executable resolution (model C) tests ---

    #[test]
    fn backend_default_is_claude() {
        assert_eq!(Backend::default(), Backend::Claude);
        let cfg = SlopdConfig::default();
        let resolved = cfg.resolve_account(None).unwrap();
        assert_eq!(resolved.backend, Backend::Claude);
        assert_eq!(
            resolved.executable,
            Executable::String("claude".to_string())
        );
    }

    #[test]
    fn backend_explicit_opencode_defaults_executable() {
        // `backend = "opencode"` alone → spawn opencode (vice-versa).
        let cfg = config_from_toml(
            "[accounts.oc]\nconfig_dir = \"~/.config/opencode\"\nbackend = \"opencode\"\n",
        );
        let resolved = cfg.resolve_account(Some("oc")).unwrap();
        assert_eq!(resolved.backend, Backend::Opencode);
        assert_eq!(
            resolved.executable,
            Executable::String("opencode".to_string())
        );
        assert_eq!(resolved.backend.config_dir_env_var(), "OPENCODE_CONFIG_DIR");
    }

    #[test]
    fn backend_inferred_from_executable() {
        // `executable = "opencode"` alone → infer opencode.
        let cfg = config_from_toml(
            "[run]\nexecutable = \"opencode\"\n[accounts.oc]\nconfig_dir = \"~/.config/opencode\"\n",
        );
        let resolved = cfg.resolve_account(Some("oc")).unwrap();
        assert_eq!(resolved.backend, Backend::Opencode);
        assert_eq!(
            resolved.executable,
            Executable::String("opencode".to_string())
        );
    }

    #[test]
    fn backend_inferred_from_default_account_global_executable() {
        // A bare `[run] executable = "opencode"` flips the default account too.
        let cfg = config_from_toml("[run]\nexecutable = \"opencode\"\n");
        let resolved = cfg.resolve_account(None).unwrap();
        assert_eq!(resolved.backend, Backend::Opencode);
    }

    #[test]
    fn backend_conflict_between_explicit_and_executable_errors() {
        // backend = "claude" + executable = "opencode" → contradiction.
        let cfg = config_from_toml(
            "[accounts.bad]\nconfig_dir = \"x\"\nbackend = \"claude\"\nexecutable = \"opencode\"\n",
        );
        let err = cfg.resolve_account(Some("bad")).unwrap_err();
        assert!(err.contains("conflict"), "expected conflict error: {}", err);
    }

    #[test]
    fn backend_custom_executable_is_override_not_inferred() {
        // Unrecognized executable + explicit backend → override, no conflict.
        let cfg = config_from_toml(
            "[accounts.oc]\nconfig_dir = \"x\"\nbackend = \"opencode\"\nexecutable = \"/opt/my-oc-fork\"\n",
        );
        let resolved = cfg.resolve_account(Some("oc")).unwrap();
        assert_eq!(resolved.backend, Backend::Opencode);
        assert_eq!(
            resolved.executable,
            Executable::String("/opt/my-oc-fork".to_string())
        );
    }

    #[test]
    fn backend_custom_executable_without_explicit_backend_defaults_claude() {
        // Unrecognized executable alone can't be inferred → Claude (inference is
        // recognized-names only; custom paths need an explicit backend).
        let cfg = config_from_toml(
            "[accounts.oc]\nconfig_dir = \"x\"\nexecutable = \"/opt/my-oc-fork\"\n",
        );
        let resolved = cfg.resolve_account(Some("oc")).unwrap();
        assert_eq!(resolved.backend, Backend::Claude);
        assert_eq!(
            resolved.executable,
            Executable::String("/opt/my-oc-fork".to_string())
        );
    }

    #[test]
    fn backend_per_account_does_not_inherit_top_level() {
        // Top-level `backend` backs only the default account, like config_dir.
        let cfg = config_from_toml("backend = \"opencode\"\n[accounts.work]\nconfig_dir = \"x\"\n");
        assert_eq!(
            cfg.resolve_account(None).unwrap().backend,
            Backend::Opencode
        );
        assert_eq!(
            cfg.resolve_account(Some("work")).unwrap().backend,
            Backend::Claude
        );
    }

    #[test]
    fn backend_account_table_overrides_global_executable() {
        // Per-account executable wins over the global `[run] executable`.
        let cfg = config_from_toml(
            "[run]\nexecutable = \"claude\"\n[accounts.oc]\nconfig_dir = \"x\"\nexecutable = \"opencode\"\n",
        );
        let resolved = cfg.resolve_account(Some("oc")).unwrap();
        assert_eq!(resolved.backend, Backend::Opencode);
        assert_eq!(
            resolved.executable,
            Executable::String("opencode".to_string())
        );
    }

    #[test]
    fn backend_shorthand_dir_account_derives_from_global_executable() {
        // Bare-string account has no backend/executable → derive from global.
        let cfg = config_from_toml(
            "[run]\nexecutable = \"opencode\"\n[accounts]\noc = \"~/.config/opencode\"\n",
        );
        let resolved = cfg.resolve_account(Some("oc")).unwrap();
        assert_eq!(resolved.backend, Backend::Opencode);
    }

    #[test]
    fn backend_executable_array_form_program_is_inferred() {
        // Array executable: inference looks at argv[0].
        let cfg = config_from_toml(
            "[run]\nexecutable = [\"opencode\", \"--dangerously-skip-permissions\"]\n",
        );
        let resolved = cfg.resolve_account(None).unwrap();
        assert_eq!(resolved.backend, Backend::Opencode);
        assert_eq!(
            resolved.executable.args(),
            &["--dangerously-skip-permissions".to_string()]
        );
    }

    #[test]
    fn backend_all_settings_paths_skips_non_claude() {
        // Hook injection targets are Claude-only.
        let cfg = config_from_toml(
            "[accounts.oc]\nconfig_dir = \"~/.config/opencode\"\nbackend = \"opencode\"\n\
             [accounts.work]\nconfig_dir = \"~/.config/claude-work\"\n",
        );
        let paths = cfg.all_settings_paths();
        // Only the default (claude) + work (claude) accounts; oc (opencode) skipped.
        assert_eq!(
            paths.len(),
            2,
            "opencode account must be skipped: {:?}",
            paths
        );
    }

    #[test]
    fn backend_infer_from_program_strips_path_and_exe() {
        assert_eq!(Backend::infer_from_program("claude"), Some(Backend::Claude));
        assert_eq!(
            Backend::infer_from_program("/usr/bin/opencode"),
            Some(Backend::Opencode)
        );
        assert_eq!(
            Backend::infer_from_program("/usr/local/bin/codex"),
            Some(Backend::Codex)
        );
        assert_eq!(
            Backend::infer_from_program("/usr/bin/grok"),
            Some(Backend::Grok)
        );
        assert_eq!(
            Backend::infer_from_program("opencode.exe"),
            Some(Backend::Opencode)
        );
        assert_eq!(Backend::infer_from_program("/opt/my-fork"), None);
        assert_eq!(
            Backend::infer_from_program("opencode-ai"),
            None,
            "only exact canonical names match"
        );
    }

    #[test]
    fn backend_explicit_codex_defaults_and_uses_codex_home() {
        let cfg: SlopdConfig = toml::from_str(
            r#"
            [accounts.work]
            backend = "codex"
            config_dir = "/tmp/codex-work"
        "#,
        )
        .unwrap();
        let resolved = cfg.resolve_account(Some("work")).unwrap();
        assert_eq!(resolved.backend, Backend::Codex);
        assert_eq!(resolved.executable.program(), "codex");
        assert_eq!(resolved.backend.config_dir_env_var(), "CODEX_HOME");
        assert!(resolved.backend.uses_injected_hooks());
        assert_eq!(
            cfg.resolved_hook_path(&resolved),
            PathBuf::from("/tmp/codex-work/hooks.json")
        );
    }

    #[test]
    fn backend_explicit_grok_defaults_and_uses_grok_home() {
        let cfg: SlopdConfig = toml::from_str(
            r#"
            [accounts.work]
            backend = "grok"
            config_dir = "/tmp/grok-work"
        "#,
        )
        .unwrap();
        let resolved = cfg.resolve_account(Some("work")).unwrap();
        assert_eq!(resolved.backend, Backend::Grok);
        assert_eq!(resolved.executable.program(), "grok");
        assert_eq!(resolved.backend.config_dir_env_var(), "GROK_HOME");
        assert!(resolved.backend.uses_injected_hooks());
        assert_eq!(
            cfg.resolved_hook_path(&resolved),
            PathBuf::from("/tmp/grok-work/hooks/slopd.json")
        );
    }

    #[test]
    fn grok_hook_injection_uses_native_event_vocabulary() {
        let mut hooks = serde_json::json!({});
        inject_backend_hooks(&mut hooks, "slopctl", Backend::Grok);
        for event in GROK_HOOK_EVENTS {
            assert_eq!(
                hooks["hooks"][event].as_array().map(Vec::len),
                Some(1),
                "missing Grok hook {event}"
            );
        }
        assert!(hooks["hooks"].get("PermissionRequest").is_none());
        assert!(hooks["hooks"].get("Elicitation").is_none());
    }

    #[test]
    fn codex_hook_injection_uses_only_supported_events() {
        let mut hooks = serde_json::json!({});
        inject_backend_hooks(&mut hooks, "slopctl", Backend::Codex);
        for event in CODEX_HOOK_EVENTS {
            assert_eq!(
                hooks["hooks"][event].as_array().map(Vec::len),
                Some(1),
                "missing Codex hook {event}"
            );
        }
        assert!(
            hooks["hooks"].get("Notification").is_none(),
            "Claude-only hook names must not be written to Codex hooks.json"
        );
    }

    // --- slopctl interactive-run config ---

    #[test]
    fn slopctl_config_defaults_to_grouped_exec_viewer() {
        let cfg = SlopctlConfig::default();
        assert_eq!(cfg.run.interactive_type, RunType::Exec);
        // Default socket → no -S; isolated grouped view focused on the new pane.
        assert_eq!(
            cfg.interactive_command(None, SLOPD_TMUX_SESSION),
            vec![
                "tmux",
                "new-session",
                "-t",
                "slopd",
                ";",
                "set-option",
                "destroy-unattached",
                "on",
                ";",
                "select-window",
                "-t",
                "{{pane_id}}"
            ],
        );
    }

    #[test]
    fn default_interactive_command_honors_socket() {
        assert_eq!(
            default_interactive_command(Some("/run/x.sock"), SLOPD_TMUX_SESSION),
            vec![
                "tmux",
                "-S",
                "/run/x.sock",
                "new-session",
                "-t",
                "slopd",
                ";",
                "set-option",
                "destroy-unattached",
                "on",
                ";",
                "select-window",
                "-t",
                "{{pane_id}}"
            ],
        );
        // No socket → no -S prefix.
        assert_eq!(
            default_interactive_command(None, SLOPD_TMUX_SESSION)
                .first()
                .map(String::as_str),
            Some("tmux"),
        );
        assert!(
            !default_interactive_command(None, SLOPD_TMUX_SESSION)
                .iter()
                .any(|a| a == "-S")
        );
    }

    #[test]
    fn slopctl_config_parses_interactive_command_and_type() {
        let cfg: SlopctlConfig = toml::from_str(
            "[run]\ninteractive_command = [\"kitty\", \"--\", \"tmux\", \"attach\", \"-t\", \"slopd\"]\ninteractive_type = \"forking\"\n",
        ).unwrap();
        assert_eq!(cfg.run.interactive_type, RunType::Forking);
        // A configured command is returned as-is, ignoring socket/session.
        assert_eq!(
            cfg.interactive_command(Some("/ignored.sock"), "ignored"),
            vec!["kitty", "--", "tmux", "attach", "-t", "slopd"],
        );
    }

    #[test]
    fn substitute_replaces_all_named_placeholders() {
        let cmd: Vec<String> = ["sh", "-c", "echo {{pane_id}} > /tmp/{{pane_id}}.log"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = SlopctlConfig::substitute(&cmd, &[("pane_id", "%42")]);
        assert_eq!(out, vec!["sh", "-c", "echo %42 > /tmp/%42.log"]);
    }

    #[test]
    fn substitute_supports_multiple_variables() {
        // Future-proofing: more than one named variable.
        let cmd: Vec<String> = vec!["{{account}}:{{pane_id}}".to_string()];
        let out = SlopctlConfig::substitute(&cmd, &[("pane_id", "%7"), ("account", "work")]);
        assert_eq!(out, vec!["work:%7"]);
    }

    #[test]
    fn substitute_fills_socket_and_session() {
        let cmd: Vec<String> = ["tmux", "-S", "{{socket}}", "attach", "-t", "{{session}}"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = SlopctlConfig::substitute(
            &cmd,
            &[
                ("pane_id", "%9"),
                ("socket", "/run/x.sock"),
                ("session", "slopd"),
            ],
        );
        assert_eq!(
            out,
            vec!["tmux", "-S", "/run/x.sock", "attach", "-t", "slopd"]
        );
    }

    #[test]
    fn substitute_does_not_touch_tmux_format_strings() {
        // `#{pane_id}` is a tmux format; double-brace placeholders must leave it intact.
        let cmd: Vec<String> = ["tmux", "display", "-p", "#{pane_id}"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = SlopctlConfig::substitute(&cmd, &[("pane_id", "%42")]);
        assert_eq!(out, vec!["tmux", "display", "-p", "#{pane_id}"]);
    }

    #[test]
    fn run_type_default_is_exec() {
        assert_eq!(RunType::default(), RunType::Exec);
    }

    #[test]
    fn tmux_session_defaults_and_is_configurable() {
        assert_eq!(SlopdTmuxConfig::default().session(), SLOPD_TMUX_SESSION);
        let cfg = config_from_toml("[tmux]\nsession = \"work-slopd\"\n");
        assert_eq!(cfg.tmux.session(), "work-slopd");
    }

    #[test]
    fn control_socket_config_is_used_and_cli_runtime_override_wins() {
        let mut cfg = config_from_toml(
            "[control]\nsocket = \"/run/user/1000/slopd-config.sock\"\n\n[run]\nslopctl = \"slopctl\"\n",
        );
        assert_eq!(
            cfg.control_socket_path(),
            PathBuf::from("/run/user/1000/slopd-config.sock")
        );
        assert_eq!(
            cfg.hook_slopctl(),
            "slopctl --socket /run/user/1000/slopd-config.sock"
        );

        // slopd stores the startup-selected CLI value in the runtime-only field.
        cfg.control_socket = Some(PathBuf::from("/run/user/1000/slopd-cli.sock"));
        assert_eq!(
            cfg.control_socket_path(),
            PathBuf::from("/run/user/1000/slopd-cli.sock")
        );
        assert_eq!(
            cfg.hook_slopctl(),
            "slopctl --socket /run/user/1000/slopd-cli.sock"
        );
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
        let cfg = config_from_toml("config_dir = \"/srv/legacy\"\n");
        let resolved = cfg.resolve_account(None).unwrap();
        assert_eq!(
            cfg.resolved_settings_path(&resolved),
            cfg.claude_settings_path()
        );
    }

    #[test]
    fn all_settings_paths_includes_default_and_accounts_deduped() {
        let cfg = config_from_toml(
            "config_dir = \"/srv/legacy\"\n\
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
        assert_eq!(
            legacy_count, 1,
            "duplicate dirs must be collapsed: {:?}",
            paths
        );
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
    inject_hook_events_into_file(settings_path, slopctl, HOOK_EVENTS)
}

/// Read, inject, and atomically write the hook configuration for `backend`.
pub fn inject_backend_hooks_into_file(
    settings_path: &PathBuf,
    slopctl: &str,
    backend: Backend,
) -> Result<(), Box<dyn std::error::Error>> {
    inject_hook_events_into_file(settings_path, slopctl, backend.hook_events())
}

fn inject_hook_events_into_file(
    settings_path: &PathBuf,
    slopctl: &str,
    hook_events: &[&str],
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

    inject_hook_events(&mut settings, slopctl, hook_events);

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
    /// Local control endpoint configuration. The CLI `--socket` option takes
    /// precedence over this section.
    #[serde(default)]
    pub control: SlopdControlConfig,
    #[serde(default)]
    pub run: SlopdRunConfig,
    #[serde(default)]
    pub backup: SlopdBackupConfig,
    /// Agent config dir for the reserved [`DEFAULT_ACCOUNT`] (the account used
    /// when no account is selected). Exported as `CLAUDE_CONFIG_DIR` (Claude) or
    /// `OPENCODE_CONFIG_DIR` (OpenCode), `CODEX_HOME` (Codex), or `GROK_HOME`
    /// (Grok). Supports `~`
    /// and `$VAR` / `${VAR}` expansion.
    #[serde(alias = "claude_config_dir")]
    pub config_dir: Option<PathBuf>,
    /// Backend for the reserved [`DEFAULT_ACCOUNT`] (the account used when no
    /// account is selected). Shorthand for `[accounts.default] backend = ...`.
    /// Named accounts do **not** inherit this — set `backend` on each
    /// `[accounts.<name>]`. When unset, the default account's backend is derived
    /// — see [`Backend::resolve`].
    #[serde(default)]
    pub backend: Option<Backend>,
    /// Named agent accounts. Each maps an account name to its own configuration.
    /// An omitted config dir lets the selected backend use its standard location.
    /// Select one for a pane with
    /// `slopctl run --account <name>`; child panes inherit their parent's
    /// account unless overridden. The name `default` is reserved (see
    /// [`DEFAULT_ACCOUNT`]).
    #[serde(default)]
    pub accounts: std::collections::BTreeMap<String, AccountConfig>,
    /// Account used by `slopctl run` when no `--account` is given and none is
    /// inherited from the parent pane. When unset, the [`DEFAULT_ACCOUNT`]
    /// account is used.
    pub default_account: Option<String>,
    /// Effective non-default control socket selected at process startup, after
    /// applying CLI-over-config precedence. Runtime-only so a SIGHUP reload
    /// cannot move hooks away from the socket the process actually bound.
    #[serde(skip)]
    pub control_socket: Option<PathBuf>,
}

/// The reserved account name used when nothing else selects one. Its config dir
/// comes from `[accounts.default]` if present, otherwise the top-level
/// `config_dir`, otherwise Claude's own `~/.claude`.
pub const DEFAULT_ACCOUNT: &str = "default";

/// Configuration for a single named account. Accepts either a bare string (the
/// agent config dir) or a table for richer per-account
/// settings, so both of these are valid:
///
/// ```toml
/// [accounts]
/// work = "~/.config/claude-work"          # shorthand: just the dir
///
/// [accounts.personal]
/// config_dir = "~/.config/claude-personal"   # table form (extensible)
/// ```
///
/// The table form is where future per-account options live (see
/// [`AccountSettings`]); the bare-string form is sugar for a table with only
/// `config_dir` set. In the table form, `config_dir` may be omitted to use the
/// backend's standard config location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AccountConfig {
    /// Shorthand: the account is just its agent config directory.
    Dir(PathBuf),
    /// Full table form, extensible with further per-account keys over time.
    Settings(AccountSettings),
}

/// The table form of a per-account configuration. New per-account options are
/// added here as fields (give each a `#[serde(default)]` so the table stays
/// backward-compatible), plus a matching accessor on [`AccountConfig`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountSettings {
    /// The account's agent config directory (exported as `CLAUDE_CONFIG_DIR`
    /// for [`Backend::Claude`], `OPENCODE_CONFIG_DIR` for [`Backend::Opencode`],
    /// `CODEX_HOME` for [`Backend::Codex`], or `GROK_HOME` for
    /// [`Backend::Grok`]). When omitted, that environment variable is left
    /// unset and the backend uses its standard location.
    #[serde(alias = "claude_config_dir")]
    #[serde(default)]
    pub config_dir: Option<PathBuf>,
    /// Agent backend for this account. When unset, the backend is derived — see
    /// [`Backend::resolve`].
    #[serde(default)]
    pub backend: Option<Backend>,
    /// Per-account executable override. When unset, the global `[run] executable`
    /// is used (or the backend's canonical binary — see [`Backend::resolve`]).
    #[serde(default)]
    pub executable: Option<Executable>,
}

impl AccountConfig {
    /// The account's agent config directory, as written in config (unexpanded).
    pub fn config_dir(&self) -> Option<&PathBuf> {
        match self {
            AccountConfig::Dir(p) => Some(p),
            AccountConfig::Settings(s) => s.config_dir.as_ref(),
        }
    }

    /// Per-account backend override, if set (table form only).
    pub fn backend(&self) -> Option<Backend> {
        match self {
            AccountConfig::Settings(s) => s.backend,
            AccountConfig::Dir(_) => None,
        }
    }

    /// Per-account executable override, if set (table form only).
    pub fn executable(&self) -> Option<&Executable> {
        match self {
            AccountConfig::Settings(s) => s.executable.as_ref(),
            AccountConfig::Dir(_) => None,
        }
    }
}

/// The outcome of resolving a requested account name against the config: the
/// account in effect, the agent config dir, and the resolved backend +
/// executable to spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAccount {
    /// The account name in effect (always set; [`DEFAULT_ACCOUNT`] for the
    /// default). Recorded on the pane as `@slopd_account` so it shows in `ps`
    /// and child panes can inherit it.
    pub name: String,
    /// The agent config dir to export (as `CLAUDE_CONFIG_DIR` /`
    /// `OPENCODE_CONFIG_DIR`, selected by [`Self::backend`]). `None` means leave
    /// it unset so the agent falls back to its default.
    pub config_dir: Option<PathBuf>,
    /// The agent backend in effect (drives spawn behavior + the config-dir env
    /// var + whether hooks are injected).
    pub backend: Backend,
    /// The executable to spawn for this account (already resolved against the
    /// backend per [`Backend::resolve`]).
    pub executable: Executable,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SlopdControlConfig {
    /// Unix socket used by slopd and local clients. Supports `~` and `$VAR` /
    /// `${VAR}` expansion. `slopd --socket` / `slopctl --socket` override it.
    pub socket: Option<PathBuf>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SlopdTmuxConfig {
    /// Path to a custom tmux socket (`tmux -S`). Supports `~` and `$VAR` /
    /// `${VAR}` expansion.
    pub socket: Option<PathBuf>,
    /// Name of the tmux session slopd manages its panes in (default:
    /// [`SLOPD_TMUX_SESSION`]). Usually only worth changing to run more than one
    /// slopd instance against the same tmux server/socket.
    pub session: Option<String>,
    /// Run `tmux start-server` on startup (default: true when socket is not set).
    pub start_server: Option<bool>,
}

impl SlopdTmuxConfig {
    /// Whether slopd should run `tmux start-server` on startup.
    pub fn should_start_server(&self) -> bool {
        self.start_server.unwrap_or(self.socket.is_none())
    }

    /// The tmux session name slopd manages (configured, else [`SLOPD_TMUX_SESSION`]).
    pub fn session(&self) -> String {
        self.session
            .clone()
            .unwrap_or_else(|| SLOPD_TMUX_SESSION.to_string())
    }
}

/// Which agent CLI a pane runs. Selects the spawn backend, the config-dir env
/// var exported into the pane, and whether slopd injects Claude-style hooks.
///
/// Resolution against `executable` is bidirectional ("each implies the other"):
/// see [`Backend::resolve`]. Inference recognizes only the canonical binary
/// names (`claude`, `opencode`, `codex`, `grok`); a custom path/wrapper needs an explicit
/// `backend` and is treated as an executable override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// Anthropic's Claude Code CLI (default). Uses `~/.claude`, injects
    /// `slopctl hook` entries into `settings.json`, and tails the jsonl
    /// transcript.
    #[default]
    Claude,
    /// OpenCode (`opencode`). Runs the TUI with an embedded HTTP server; slopd
    /// drives it over HTTP/SSE — no hooks, no jsonl tailing.
    Opencode,
    /// OpenAI Codex CLI. Runs as an independent local process; slopd observes it
    /// through Codex hooks and the session rollout transcript.
    Codex,
    /// xAI Grok Build. Runs a visible TUI against a pane-private leader while
    /// slopd attaches as an ACP subscriber; hooks and updates.jsonl remain the
    /// durable lifecycle and recovery authority.
    Grok,
}

impl Backend {
    /// The canonical bare binary name for this backend.
    pub fn canonical_executable(self) -> &'static str {
        match self {
            Backend::Claude => "claude",
            Backend::Opencode => "opencode",
            Backend::Codex => "codex",
            Backend::Grok => "grok",
        }
    }

    /// Infer a backend from a binary name, recognizing only the canonical names
    /// (a directory prefix and `.exe` suffix are tolerated). Returns `None` for
    /// custom paths/wrappers — those need an explicit `backend` and are treated
    /// as an executable override, never inferred or conflicted.
    pub fn infer_from_program(program: &str) -> Option<Backend> {
        let base = program
            .rsplit('/')
            .next()
            .unwrap_or(program)
            .trim_end_matches(".exe");
        match base {
            "claude" => Some(Backend::Claude),
            "opencode" => Some(Backend::Opencode),
            "codex" => Some(Backend::Codex),
            "grok" => Some(Backend::Grok),
            _ => None,
        }
    }

    /// The env var slopd exports to point the agent at its config dir.
    pub fn config_dir_env_var(self) -> &'static str {
        match self {
            Backend::Claude => "CLAUDE_CONFIG_DIR",
            Backend::Opencode => "OPENCODE_CONFIG_DIR",
            Backend::Codex => "CODEX_HOME",
            Backend::Grok => "GROK_HOME",
        }
    }

    /// Whether slopd injects `slopctl hook` entries for this backend.
    pub fn uses_injected_hooks(self) -> bool {
        matches!(self, Backend::Claude | Backend::Codex | Backend::Grok)
    }

    /// Hook event names accepted by this backend's configuration file.
    pub fn hook_events(self) -> &'static [&'static str] {
        match self {
            Backend::Claude => HOOK_EVENTS,
            Backend::Codex => CODEX_HOOK_EVENTS,
            Backend::Grok => GROK_HOOK_EVENTS,
            Backend::Opencode => &[],
        }
    }

    /// Resolve `(explicit backend, explicit executable)` into the backend in
    /// effect and the executable to spawn, under the "each implies the other"
    /// rule (model C):
    ///
    /// - `backend` set → authoritative; `executable` defaults to its canonical
    ///   binary when unset.
    /// - `backend` unset → inferred from `executable` when it is a recognized
    ///   name, else [`Backend::Claude`].
    /// - An explicit `backend` that contradicts a recognized `executable` is an
    ///   error (e.g. `backend = "claude"` + `executable = "opencode"`).
    /// - An unrecognized `executable` (custom path/wrapper) is always treated as
    ///   an override and never infers or conflicts.
    pub fn resolve(
        explicit_backend: Option<Backend>,
        explicit_executable: Option<&Executable>,
    ) -> Result<(Backend, Executable), String> {
        let inferred = explicit_executable.and_then(|e| Backend::infer_from_program(e.program()));
        if let (Some(asked), Some(inferred)) = (explicit_backend, inferred)
            && asked != inferred
        {
            return Err(format!(
                "backend {:?} conflicts with executable {:?} (which implies backend {:?})",
                asked,
                explicit_executable.unwrap().program(),
                inferred,
            ));
        }
        let backend = explicit_backend.or(inferred).unwrap_or_default();
        let executable = explicit_executable
            .cloned()
            .unwrap_or_else(|| Executable::String(backend.canonical_executable().to_string()));
        Ok((backend, executable))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Agent executable for panes that don't override it per-account. When
    /// unset, the effective executable is derived via [`Backend::resolve`] (the
    /// resolved backend's canonical binary). A recognized name here
    /// (`claude`/`opencode`/`codex`/`grok`) also implies the backend for accounts that don't
    /// set one.
    #[serde(default)]
    pub executable: Option<Executable>,
    /// Path to slopctl binary used for hook injection (default: "slopctl")
    #[serde(default = "default_slopctl")]
    pub slopctl: String,
    /// Default working directory for new agent panes. Supports `~` and
    /// `$VAR` / `${VAR}` expansion. Overridden per-session by
    /// `slopctl run --start-directory`.
    pub start_directory: Option<PathBuf>,
    /// Extra environment variables for every new agent pane. Values support
    /// `$VAR` / `${VAR}` expansion against slopd's environment at spawn time.
    /// Merged with (and overridden by) `slopctl run --env` / `--env-file`.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Paths to env-files loaded for every new agent pane. Paths support
    /// `~` / `$VAR` expansion. Files are loaded in order; later files and
    /// [run.env] entries override earlier ones. CLI `--env-file` / `--env`
    /// override all of these.
    #[serde(default)]
    pub env_files: Vec<PathBuf>,
    /// Whether to automatically send "continue" when a turn ends with StopFailure
    /// (e.g., API errors like 500). Uses exponential backoff.
    #[serde(default = "default_auto_continue_on_failure")]
    pub auto_continue_on_failure: bool,
    /// Maximum number of auto-continue attempts before giving up.
    #[serde(default = "default_max_retry_attempts")]
    pub max_retry_attempts: u32,
    /// Initial backoff in milliseconds before the first auto-continue retry.
    #[serde(default = "default_initial_backoff_ms")]
    pub initial_backoff_ms: u64,
    /// Optional ceiling (milliseconds) on the exponential backoff delay. When
    /// unset the delay keeps doubling every retry uncapped; set it to flatten the
    /// schedule into steady polling once the delay reaches this value.
    #[serde(default)]
    pub max_backoff_ms: Option<u64>,
}

fn default_auto_continue_on_failure() -> bool {
    true
}

fn default_max_retry_attempts() -> u32 {
    // Uncapped exponential backoff from initial=1s: the delays run
    // 1,2,4,8,16,32,64,128s, so the cumulative wait after N attempts is
    // (2^N - 1)s. 8 attempts sum to 255s (~4m15s) of retrying — long enough to
    // ride out a transient outage unattended, with each successive retry backing
    // off further rather than hammering a down API.
    8
}

fn default_initial_backoff_ms() -> u64 {
    1000
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
            executable: None,
            slopctl: default_slopctl(),
            start_directory: None,
            env: std::collections::BTreeMap::new(),
            env_files: Vec::new(),
            auto_continue_on_failure: default_auto_continue_on_failure(),
            max_retry_attempts: default_max_retry_attempts(),
            initial_backoff_ms: default_initial_backoff_ms(),
            max_backoff_ms: None,
        }
    }
}

/// Backup and restore of the managed-pane set across reboots (the `[backup]`
/// config section).
///
/// slopd normally keeps each pane's identity (backend session id, account, tags,
/// ancestry) in tmux pane options, which it re-reads on a daemon restart. A
/// *reboot* destroys the tmux server along with those options and processes, so
/// slopd checkpoints the pane set in its durable lifecycle journal and resumes
/// each backend session afterwards.
///
/// The two automatic behaviours are independent toggles, and all four
/// combinations are valid. Manual `slopctl backup` and `slopctl restore` are
/// always available regardless of these — the toggles only control what slopd
/// does on its own.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlopdBackupConfig {
    /// Automatically checkpoint the pane set on a timer and clean shutdown
    /// (default: true). `slopctl backup` triggers a write on demand regardless.
    #[serde(default = "default_auto_backup")]
    pub auto_backup: bool,
    /// Automatically re-spawn the recorded panes through their backend on the
    /// next startup into a freshly-created tmux session, i.e. after a reboot
    /// (default: false, so a reboot doesn't resurrect panes unless asked).
    /// `slopctl restore` triggers a restore on demand regardless.
    #[serde(default)]
    pub auto_restore: bool,
    /// How often (seconds) to auto-back-up while running (default: 30). A backup
    /// is also taken on clean shutdown regardless of this interval.
    #[serde(default = "default_backup_interval_secs")]
    pub interval_secs: u64,
}

fn default_auto_backup() -> bool {
    true
}

fn default_backup_interval_secs() -> u64 {
    30
}

impl Default for SlopdBackupConfig {
    fn default() -> Self {
        Self {
            auto_backup: default_auto_backup(),
            auto_restore: false,
            interval_secs: default_backup_interval_secs(),
        }
    }
}

impl SlopdConfig {
    pub fn load() -> Self {
        let path = Self::config_path();
        load_config(path)
    }

    /// Like [`SlopdConfig::load`], but read from `path` instead of the default
    /// [`config_path`]. Backs the `--config` CLI override; warns and defaults on
    /// a missing or unparseable file, just like `load`.
    pub fn load_from(path: &std::path::Path) -> Self {
        load_config(path.to_path_buf())
    }

    /// Path to the slopd config file (`$XDG_CONFIG_HOME/slopd/config.toml`).
    pub fn config_path() -> PathBuf {
        config_dir().join("slopd/config.toml")
    }

    /// Configured control-socket override, with path expansion applied.
    pub fn configured_control_socket(&self) -> Option<PathBuf> {
        self.control.socket.as_deref().map(expand_path)
    }

    /// The effective non-default control socket: the startup-frozen CLI/config
    /// selection when running inside slopd, otherwise the value read directly
    /// from `[control].socket`.
    pub fn control_socket_override(&self) -> Option<PathBuf> {
        self.control_socket
            .clone()
            .or_else(|| self.configured_control_socket())
    }

    /// The control socket this instance listens on: CLI override, then
    /// `[control].socket`, then the XDG default [`socket_path`].
    pub fn control_socket_path(&self) -> PathBuf {
        self.control_socket_override().unwrap_or_else(socket_path)
    }

    /// The `slopctl` command prefix baked into injected hook commands. With a
    /// `--socket` override it becomes `<slopctl> --socket <path>` so panes (and
    /// tmux hooks) report back to *this* instance rather than whichever socket
    /// `$XDG_RUNTIME_DIR` happens to point at; otherwise just the plain
    /// `run.slopctl` value, keeping the default instance's hooks byte-identical.
    pub fn hook_slopctl(&self) -> String {
        match self.control_socket_override() {
            Some(sock) => format!("{} --socket {}", self.run.slopctl, sock.display()),
            None => self.run.slopctl.clone(),
        }
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

    pub fn config_dir(&self) -> PathBuf {
        self.config_dir
            .as_deref()
            .map(expand_path)
            .unwrap_or_else(|| home_dir().join(".claude"))
    }

    pub fn claude_settings_path(&self) -> PathBuf {
        self.config_dir().join("settings.json")
    }

    /// Resolve a requested account name into the account, backend, executable,
    /// and optional config dir in effect for a new pane.
    ///
    /// The account name is `requested`, else `default_account`, else
    /// [`DEFAULT_ACCOUNT`]. The dir is then:
    /// - for [`DEFAULT_ACCOUNT`]: `[accounts.default]` if present, else the
    ///   top-level `config_dir`, else `None` (Claude's `~/.claude`);
    /// - for any other name: `[accounts.<name>]`, or an error (listing the
    ///   configured accounts) if it is not configured.
    ///
    /// All config dirs — named accounts and the top-level `config_dir` —
    /// are `~` / `$VAR`-expanded.
    pub fn resolve_account(&self, requested: Option<&str>) -> Result<ResolvedAccount, String> {
        let name = requested
            .map(str::to_string)
            .or_else(|| self.default_account.clone())
            .unwrap_or_else(|| DEFAULT_ACCOUNT.to_string());

        // The default account is backed by [accounts.default], then the
        // top-level config_dir, then ~/.claude (left unset).
        if name == DEFAULT_ACCOUNT {
            let acct = self.accounts.get(DEFAULT_ACCOUNT);
            let config_dir = acct
                .and_then(|a| a.config_dir().map(|path| expand_path(path)))
                .or_else(|| self.config_dir.as_deref().map(expand_path));
            // Backend/executable: [accounts.default] wins over the top-level
            // `backend` / `[run] executable`; the top-level values back ONLY the
            // default account (named accounts don't inherit them).
            let explicit_backend = acct.and_then(|a| a.backend()).or(self.backend);
            let explicit_executable = acct
                .and_then(|a| a.executable())
                .or(self.run.executable.as_ref());
            let (backend, executable) = Backend::resolve(explicit_backend, explicit_executable)?;
            return Ok(ResolvedAccount {
                name,
                config_dir,
                backend,
                executable,
            });
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
        // Named accounts: per-account backend/executable, falling back to the
        // global `[run] executable` (but NOT the top-level `backend`, which is
        // default-account-only, matching `config_dir`).
        let explicit_backend = account.backend();
        let explicit_executable = account.executable().or(self.run.executable.as_ref());
        let (backend, executable) = Backend::resolve(explicit_backend, explicit_executable)?;
        Ok(ResolvedAccount {
            name,
            config_dir: account.config_dir().map(|path| expand_path(path)),
            backend,
            executable,
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

    /// The backend-specific hook configuration file for a resolved account.
    pub fn resolved_hook_path(&self, resolved: &ResolvedAccount) -> PathBuf {
        match resolved.backend {
            Backend::Claude => self.resolved_settings_path(resolved),
            Backend::Codex => resolved
                .config_dir
                .clone()
                .unwrap_or_else(|| home_dir().join(".codex"))
                .join("hooks.json"),
            Backend::Grok => resolved
                .config_dir
                .clone()
                .unwrap_or_else(|| home_dir().join(".grok"))
                .join("hooks")
                .join("slopd.json"),
            Backend::Opencode => unreachable!("opencode does not use injected hooks"),
        }
    }

    /// Every distinct `settings.json` slopd may manage hooks in: the default
    /// account plus every configured account, deduplicated. Used at startup
    /// recovery, shutdown, and `uninject-hooks`, where the account of each
    /// (possibly recovered) pane is not individually known.
    pub fn all_settings_paths(&self) -> Vec<PathBuf> {
        self.all_hook_paths()
            .into_iter()
            .filter_map(|(path, backend)| (backend == Backend::Claude).then_some(path))
            .collect()
    }

    /// Every distinct backend hook file slopd may manage, paired with the
    /// backend whose event vocabulary applies to that file.
    pub fn all_hook_paths(&self) -> Vec<(PathBuf, Backend)> {
        let mut names: std::collections::BTreeSet<&str> =
            self.accounts.keys().map(String::as_str).collect();
        names.insert(DEFAULT_ACCOUNT);

        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for name in names {
            // resolve_account only errors for unknown named accounts; every name
            // here comes from the config (or is DEFAULT_ACCOUNT), so this holds.
            if let Ok(resolved) = self.resolve_account(Some(name)) {
                if !resolved.backend.uses_injected_hooks() {
                    continue;
                }
                let path = self.resolved_hook_path(&resolved);
                if seen.insert((path.clone(), resolved.backend)) {
                    out.push((path, resolved.backend));
                }
            }
        }
        out
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SlopctlConfig {
    #[serde(default)]
    pub run: SlopctlRunConfig,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SlopctlRunConfig {
    /// Command for `slopctl run --interactive`, run once the new pane exists.
    /// `{{pane_id}}`, `{{socket}}` (slopd tmux socket, empty if default), and
    /// `{{session}}` placeholders in each argument are substituted. When unset,
    /// defaults to [`default_interactive_command`] (attach an isolated grouped
    /// view of the slopd session focused on the new pane).
    pub interactive_command: Option<Vec<String>>,
    /// How to run the interactive command (a subset of systemd's `Type=`):
    /// `exec` (default) replaces the slopctl process with it; `forking` runs it
    /// detached in the background and slopctl prints the pane id and exits.
    #[serde(default)]
    pub interactive_type: RunType,
}

/// How `slopctl run --interactive` runs its command. Named after the relevant
/// subset of systemd's service `Type=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunType {
    /// Replace the slopctl process with the command (e.g. `tmux attach` takes
    /// over the terminal). slopctl does not return.
    #[default]
    Exec,
    /// Run the command detached in the background; slopctl prints the pane id
    /// and exits.
    Forking,
}

/// The tmux session name slopd manages all of its panes in.
pub const SLOPD_TMUX_SESSION: &str = "slopd";

/// Default `slopctl run --interactive` command. Attaches to an isolated,
/// *grouped* view of the slopd session and focuses the new pane:
///
/// ```text
/// tmux [-S <socket>] new-session -t <session> ; set destroy-unattached on ; select-window -t {{pane_id}}
/// ```
///
/// A grouped session shares the slopd session's windows but keeps its own
/// current window, so focusing the new pane here does not move other clients
/// watching the session. `destroy-unattached on` makes the throwaway view
/// session clean itself up when you detach. Honors slopd's `[tmux] socket`.
pub fn default_interactive_command(socket: Option<&str>, session: &str) -> Vec<String> {
    let mut cmd = vec!["tmux".to_string()];
    if let Some(socket) = socket {
        cmd.push("-S".to_string());
        cmd.push(socket.to_string());
    }
    for arg in [
        "new-session",
        "-t",
        session,
        ";",
        "set-option",
        "destroy-unattached",
        "on",
        ";",
        "select-window",
        "-t",
        "{{pane_id}}",
    ] {
        cmd.push(arg.to_string());
    }
    cmd
}

impl SlopctlConfig {
    pub fn load() -> Self {
        let path = config_dir().join("slopctl/config.toml");
        load_config(path)
    }

    /// Like [`SlopctlConfig::load`], but read from `path` instead of the default
    /// `$XDG_CONFIG_HOME/slopctl/config.toml`. Backs the `--config` CLI
    /// override: a single file can configure both slopctl and slopd, since each
    /// struct ignores fields it does not recognize.
    pub fn load_from(path: &std::path::Path) -> Self {
        load_config(path.to_path_buf())
    }

    /// The effective interactive command: the configured one, else the default
    /// built from the slopd tmux `socket`/`session`.
    pub fn interactive_command(&self, socket: Option<&str>, session: &str) -> Vec<String> {
        self.run
            .interactive_command
            .clone()
            .unwrap_or_else(|| default_interactive_command(socket, session))
    }

    /// Substitute `{{name}}` placeholders in an interactive command template.
    /// `vars` maps placeholder names to values; every `{{name}}` occurrence in
    /// each argument is replaced. Double braces (handlebars-style) are used so
    /// the tokens never collide with tmux `#{...}` format strings. Variables
    /// today are `pane_id`, `socket` (the slopd tmux socket, empty if default),
    /// and `session`; the slice form leaves room for more.
    pub fn substitute(command: &[String], vars: &[(&str, &str)]) -> Vec<String> {
        command
            .iter()
            .map(|arg| {
                let mut out = arg.clone();
                for (name, value) in vars {
                    out = out.replace(&["{{", name, "}}"].concat(), value);
                }
                out
            })
            .collect()
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
            return Err(format!(
                "invalid path {:?}: empty segment between dots",
                raw
            ));
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
                format!(
                    "invalid path {:?}: unclosed `[` in segment {:?}",
                    raw, piece
                )
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
    let (key, value) = raw
        .split_once('=')
        .ok_or_else(|| format!("invalid predicate {:?}: expected KEY=VALUE", raw))?;
    let path =
        parse_payload_path(key).map_err(|e| format!("invalid predicate {:?}: {}", raw, e))?;
    Ok(PayloadPredicate {
        path,
        expected: value.to_string(),
    })
}

/// Parse many `KEY=VALUE` predicates in flag order. Used by both `--until`
/// and `--where`.
pub fn parse_payload_predicates(raw: Vec<String>) -> Result<Vec<PayloadPredicate>, String> {
    raw.into_iter()
        .map(|p| parse_payload_predicate(&p))
        .collect()
}

/// True iff every predicate matches the value (AND).
pub fn predicates_match(value: &serde_json::Value, predicates: &[PayloadPredicate]) -> bool {
    predicates
        .iter()
        .all(|p| path_matches(value, &p.path, &p.expected))
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
        /// Named account to launch the pane under. The daemon resolves it via
        /// `[accounts]`. `None` means the daemon default (`default_account`,
        /// else the reserved `default` account).
        #[serde(default)]
        account: Option<String>,
        /// Override the pane's agent backend (`--backend`). `None` = use the
        /// account's resolved backend. An explicit override wins; see the run
        /// handler for the executable recomputation.
        #[serde(default)]
        backend: Option<Backend>,
        /// Internal (daemon-set) only: pin a preallocated backend session id on
        /// the new pane before it is registered, so a fork's `SessionStart`
        /// hook cannot overwrite it with the source session. Set only by the
        /// `Fork` handler's synthetic Run; always `None` from the CLI. Ignored
        /// for opencode, which tracks its id via the resume path.
        #[serde(default)]
        pin_session_id: Option<String>,
    },
    /// Fork a running pane into a new pane whose backend session starts as a
    /// copy of the source pane's session history, then diverges independently.
    /// The source pane is untouched. Backend-specific: for Claude the daemon
    /// mints a fresh session id and spawns `--resume <src> --fork-session
    /// --session-id <new>`; for opencode it calls the source server's
    /// `POST /session/:id/fork` and spawns a pane bound to the returned id.
    Fork {
        /// The pane to fork from.
        pane_id: String,
        /// Working directory for the new pane. Defaults to the source pane's
        /// cwd (Claude resolves its transcript by cwd, so this must match).
        #[serde(default)]
        start_directory: Option<PathBuf>,
        /// Extra environment variables for the forked pane (like `Run`).
        #[serde(default)]
        env: Vec<(String, String)>,
        /// Extra args appended to the forked backend invocation (like `Run`).
        #[serde(default)]
        extra_args: Vec<String>,
    },
    Kill {
        pane_id: String,
    },
    Hook {
        event: String,
        payload: serde_json::Value,
        pane_id: Option<String>,
    },
    /// Notification from a tmux hook (called by slopctl tmux-hook).
    TmuxHook {
        event: String,
        pane_id: Option<String>,
    },
    Send {
        pane_id: String,
        prompt: String,
        timeout_secs: u64,
        interrupt: bool,
    },
    /// Send Ctrl+C, Ctrl+D, and Escape to a pane to interrupt a running agent.
    Interrupt {
        pane_id: String,
    },
    /// Subscribe to a stream of lifecycle events (hook + slopd). An empty filters vec matches all.
    Subscribe {
        filters: Vec<EventFilter>,
    },
    /// Subscribe to a pane's transcript: replay the last `last_n` records from
    /// disk, then stream new records live. All delivered as `Record`s.
    SubscribeTranscript {
        pane_id: String,
        last_n: u64,
    },
    /// Read a page of historical transcript records before a given cursor.
    ReadTranscript {
        pane_id: String,
        before_cursor: Option<u64>,
        limit: u64,
    },
    /// Set or remove a user-defined tag on a pane.
    Tag {
        pane_id: String,
        tag: String,
        remove: bool,
    },
    /// List all user-defined tags on a pane.
    Tags {
        pane_id: String,
    },
    /// List all panes in the slopd session.
    Ps,
    /// Write a lifecycle-journal checkpoint now (manual `slopctl backup`),
    /// independent of the `auto_backup` setting.
    Backup,
    /// Re-spawn panes from the pending/latest checkpoint (manual `slopctl restore`),
    /// independent of `auto_restore`. Sessions already running are skipped, so
    /// this is safe to run against a live daemon.
    Restore,
    /// List panes recorded in the durable lifecycle graveyard. `boot` uses
    /// journal-style relative generation numbers: 0 is the current tmux
    /// session generation, -1 the previous one, and so on. Omit it to search
    /// every retained generation, newest first.
    Graveyard {
        #[serde(default)]
        boot: Option<i32>,
        #[serde(default = "default_graveyard_limit")]
        limit: usize,
    },
    /// Resume a pane from the lifecycle graveyard. `target` is a grave id
    /// (full or unique prefix) or an old tmux pane id such as `%21`; omitted
    /// means the newest not-yet-revived entry. `boot` disambiguates reused pane
    /// ids across tmux session generations. `env` is applied only if a new pane
    /// must be spawned.
    Revive {
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        boot: Option<i32>,
        #[serde(default)]
        env: Vec<(String, String)>,
    },
    /// Cancel a subscription previously created by Subscribe or SubscribeTranscript.
    /// The `id` field in the outer Request identifies the Unsubscribe request itself;
    /// `subscription_id` is the `id` of the original Subscribe/SubscribeTranscript request.
    Unsubscribe {
        subscription_id: u64,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub body: ResponseBody,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResponseBody {
    Status {
        state: DaemonState,
    },
    Run {
        pane_id: String,
    },
    /// Response to Fork: the new pane and the backend session id it was bound to
    /// (preallocated by the daemon or returned by the backend fork API).
    Forked {
        pane_id: String,
        session_id: String,
    },
    Kill {
        pane_id: String,
    },
    Sent {
        pane_id: String,
    },
    Interrupted {
        pane_id: String,
    },
    Hooked,
    TmuxHooked,
    /// Sent once to confirm a Subscribe or SubscribeTranscript request was accepted.
    Subscribed,
    /// Streamed to subscribers (both Subscribe and SubscribeTranscript).
    Record(Record),
    /// Response to ReadTranscript.
    TranscriptPage {
        records: Vec<Record>,
    },
    Tagged {
        pane_id: String,
        tag: String,
    },
    Untagged {
        pane_id: String,
        tag: String,
    },
    Tags {
        pane_id: String,
        tags: Vec<String>,
    },
    Ps {
        panes: Vec<PaneInfo>,
    },
    /// Response to Backup: number of panes written to the checkpoint.
    BackedUp {
        count: usize,
    },
    /// Response to Restore: number of panes re-spawned (sessions already running
    /// are skipped and not counted).
    Restored {
        restored: usize,
    },
    /// Durable pane-death records, newest first.
    Graveyard {
        entries: Vec<GraveEntry>,
    },
    /// The newly spawned pane (or an already-running pane bound to the same
    /// backend session) produced by `revive`.
    Revived {
        pane_id: String,
        grave_id: String,
    },
    /// Confirms that a subscription has been cancelled.
    Unsubscribed {
        subscription_id: u64,
    },
    Error {
        message: String,
    },
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub pane_id: String,
    /// Unix timestamp when slopd spawned this pane (from @slopd_created_at).
    pub created_at: u64,
    /// Unix timestamp of last tmux window activity (#{window_activity}).
    pub last_active: u64,
    /// Backend session ID learned from lifecycle discovery, if set.
    pub session_id: Option<String>,
    /// Parent pane ID if this pane was spawned by another pane via slopctl run.
    pub parent_pane_id: Option<String>,
    /// User-defined tags on this pane.
    pub tags: Vec<String>,
    /// Simplified pane state.
    pub state: PaneState,
    /// Detailed pane state.
    pub detailed_state: PaneDetailedState,
    /// Current working directory of the pane (#{pane_current_path}). Note this
    /// drifts as the agent `cd`s and is NOT necessarily the launch cwd; restore
    /// uses [`Self::transcript_path`] to recover the launch cwd instead.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Path to the pane's transcript (@slopd_transcript_path), if known.
    /// Restore reads the session's launch cwd from it: `claude --resume`
    /// resolves the session from the project dir of its launch cwd, which is the
    /// dir Claude was *started* in — not the drift-prone `working_dir`.
    #[serde(default)]
    pub transcript_path: Option<String>,
    /// The account the pane was launched under (from @slopd_account). Defaults
    /// to [`DEFAULT_ACCOUNT`] for panes with no recorded account.
    #[serde(default = "default_account_name")]
    pub account: String,
    /// The agent backend this pane runs (from @slopd_backend). Defaults to
    /// [`Backend::Claude`] for panes with no recorded backend (e.g. created
    /// before multi-backend support).
    #[serde(default)]
    pub backend: Backend,
    /// Human-readable title from the pane's tmux title (`#{pane_title}`), which
    /// both backends set to the agent's self-assigned name/summary. Already
    /// normalized (status glyph / spinner frame and the opencode `OC | ` prefix
    /// stripped — see [`normalize_pane_title`]); `None` if the pane has no
    /// informative title. Purely descriptive: slopd never keys off it.
    #[serde(default)]
    pub pane_title: Option<String>,
}

/// A durable pane-death record from slopd's lifecycle journal.
///
/// `tmux_boot_id` identifies one tmux server lifetime and `tmux_session_id`
/// identifies the managed session incarnation inside that server. Together
/// with `pane.pane_id` they remain unambiguous even when tmux reuses `%N` after
/// a server or session restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraveEntry {
    pub grave_id: String,
    pub tmux_boot_id: String,
    pub tmux_session_id: String,
    pub destroyed_at: u64,
    pub cause: String,
    pub detected_by: String,
    pub pane: PaneInfo,
    #[serde(default)]
    pub revived_at: Option<u64>,
    #[serde(default)]
    pub revived_as: Option<String>,
}

fn default_graveyard_limit() -> usize {
    50
}

/// Clean an agent's tmux pane title (`#{pane_title}`) into a stable label.
///
/// Agents prefix their pane title with a live status indicator — a spinner frame
/// (braille `⠂`/`⠐`/… that changes every animation tick) or a ready glyph (`✳`) —
/// and opencode further prefixes `OC | `. Left raw these flicker and add noise in
/// a table, so strip the leading non-alphanumeric glyph run and an `OC | ` prefix,
/// in either order. Returns `None` if nothing informative remains.
pub fn normalize_pane_title(raw: &str) -> Option<String> {
    let mut s = raw.trim();
    // Iterate: a spinner glyph may precede the `OC | ` prefix, or vice versa.
    loop {
        let before = s;
        // Drop a leading status/spinner glyph run: leading non-alphanumeric chars
        // (stops at the first letter/digit, so real titles are preserved).
        s = s
            .trim_start_matches(|c: char| !c.is_alphanumeric())
            .trim_start();
        if let Some(rest) = s.strip_prefix("OC | ") {
            s = rest.trim_start();
        }
        if s == before {
            break;
        }
    }
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
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
    /// Set when an older generation has panes that have not been restored yet:
    /// the number awaiting `slopctl restore`. While pending, auto-backup is
    /// suspended so the recovery checkpoint remains an explicit user choice.
    #[serde(default)]
    pub pending_restore: Option<usize>,
}
