//! Daemon-wide quota state. Account aliases share a key by credential directory.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use libslop::{AccountInfo, Backend, ResolvedAccount, SlopdConfig};
use serde_json::Value;

pub const POOL_TAG: &str = "acp-account-pool";

#[derive(Default)]
pub struct AccountHealth {
    loaded: bool,
    unavailable_until: HashMap<String, u64>,
}

pub fn account_home(account: &ResolvedAccount) -> PathBuf {
    account.config_dir.clone().unwrap_or_else(|| {
        let variable = match account.backend {
            Backend::Codex => "CODEX_HOME",
            Backend::Claude => "CLAUDE_CONFIG_DIR",
            _ => unreachable!("pools only support Codex and Claude"),
        };
        std::env::var_os(variable)
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                libslop::home_dir().join(if account.backend == Backend::Codex {
                    ".codex"
                } else {
                    ".claude"
                })
            })
    })
}

fn key(account: &ResolvedAccount) -> String {
    let home = account_home(account);
    let home = std::fs::canonicalize(&home).unwrap_or(home);
    format!("{:?}:{}", account.backend, home.display())
}

fn health_path(config: &SlopdConfig) -> PathBuf {
    let mut path = config.control_socket_path().into_os_string();
    path.push(".account-health.json");
    PathBuf::from(path)
}

impl AccountHealth {
    fn load(&mut self, config: &SlopdConfig, now: u64) {
        if !self.loaded {
            self.loaded = true;
            if let Ok(bytes) = std::fs::read(health_path(config)) {
                match serde_json::from_slice(&bytes) {
                    Ok(state) => self.unavailable_until = state,
                    Err(error) => tracing::warn!("could not load account cooldowns: {error}"),
                }
            }
        }
        self.unavailable_until.retain(|_, until| *until > now);
    }

    pub fn note(
        &mut self,
        config: &SlopdConfig,
        account: &str,
        reset: Option<u64>,
        now: u64,
    ) -> Result<u64, String> {
        self.load(config, now);
        let account = config.resolve_account(Some(account))?;
        let cooldown = config
            .account_pools
            .values()
            .filter(|pool| pool.accounts.contains(&account.name))
            .map(|pool| pool.cooldown_secs.clamp(1, 86400))
            .max()
            .unwrap_or(300);
        let until = reset
            .filter(|reset| *reset > now)
            .unwrap_or(now.saturating_add(cooldown));
        let until = *self
            .unavailable_until
            .entry(key(&account))
            .and_modify(|existing| *existing = (*existing).max(until))
            .or_insert(until);
        let path = health_path(config);
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            let mut file = atomic_write_file::AtomicWriteFile::open(path)?;
            serde_json::to_writer(&mut file, &self.unavailable_until)?;
            file.commit()?;
            Ok(())
        })();
        if let Err(error) = result {
            tracing::warn!("could not persist account cooldowns: {error}");
        }
        Ok(until)
    }

    pub fn select(
        &mut self,
        config: &SlopdConfig,
        name: &str,
        preferred: Option<&str>,
        exclude: &[String],
        now: u64,
    ) -> Result<AccountInfo, String> {
        let mut accounts = config.resolve_account_pool(name)?;
        self.load(config, now);
        let excluded = exclude
            .iter()
            .map(|name| config.resolve_account(Some(name)).map(|a| key(&a)))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(index) = accounts
            .iter()
            .position(|a| Some(a.name.as_str()) == preferred)
        {
            accounts[..=index].rotate_right(1);
        }
        let earliest = accounts
            .iter()
            .filter_map(|a| self.unavailable_until.get(&key(a)))
            .min()
            .copied();
        for account in accounts {
            let key = key(&account);
            if excluded.contains(&key) || self.unavailable_until.contains_key(&key) {
                continue;
            }
            return Ok(AccountInfo {
                is_default: config
                    .resolve_account(None)
                    .is_ok_and(|a| a.name == account.name),
                name: account.name,
                backend: account.backend,
                config_dir: account.config_dir,
                mcp: account.mcp,
            });
        }
        Err(format!(
            "account pool {name:?} exhausted; no untried eligible accounts{}",
            earliest
                .map(|t| format!("; earliest reset at Unix time {t}"))
                .unwrap_or_default()
        ))
    }
}

/// A typed usage-limit outcome, deliberately excluding generic HTTP 429s.
/// Claude's StopFailure does not currently include quotaLimits, so consult only
/// the last assistant error in its bounded transcript tail when necessary.
pub async fn depletion(
    backend: Backend,
    payload: &Value,
    transcript: Option<&str>,
) -> Option<Option<u64>> {
    if backend == Backend::Codex {
        let code = payload.get("error_code").and_then(Value::as_str);
        if code != Some("usage_limit_exceeded") {
            return None;
        }
        let reset = reset_time(&payload["error"]).or_else(|| reset_time(payload));
        if reset.is_some() {
            return Some(reset);
        }
        if let Some(path) = transcript
            && let Some(bytes) = transcript_tail(path).await
        {
            for line in bytes.split(|b| *b == b'\n').rev() {
                let Ok(record) = serde_json::from_slice::<Value>(line) else {
                    continue;
                };
                if record.pointer("/payload/type").and_then(Value::as_str) == Some("token_count")
                    && let Some(limits) = record
                        .pointer("/payload/rate_limits")
                        .filter(|limits| limits.is_object())
                {
                    return Some(
                        ["primary", "secondary"]
                            .iter()
                            .filter_map(|name| {
                                let window = limits.get(name)?;
                                (window
                                    .get("used_percent")
                                    .or_else(|| window.get("usedPercent"))?
                                    .as_f64()?
                                    >= 100.0)
                                    .then(|| reset_time(window))
                                    .flatten()
                            })
                            .max(),
                    );
                }
            }
        }
        return Some(None);
    }
    if backend != Backend::Claude
        || payload.get("error").and_then(Value::as_str) != Some("rate_limit")
    {
        return None;
    }
    if let Some(quota) = quota_reset(payload) {
        return Some(quota);
    }
    if let Some(path) = transcript
        && let Some(bytes) = transcript_tail(path).await
    {
        for line in bytes.split(|b| *b == b'\n').rev() {
            let Ok(record) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            if record["type"] == "user" {
                break;
            }
            if record["type"] == "assistant" {
                if record["error"] == "rate_limit"
                    && let Some(quota) = quota_reset(&record)
                {
                    return Some(quota);
                }
                break;
            }
        }
    }
    // Older Claude versions omit quotaLimits. Match only its usage-limit error
    // banner within a typed rate_limit hook, never arbitrary assistant prose.
    payload
        .get("last_assistant_message")
        .and_then(Value::as_str)
        .filter(|text| text.starts_with("You've hit your ") && text.contains("limit"))
        .map(|_| None)
}

async fn transcript_tail(path: &str) -> Option<Vec<u8>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file = tokio::fs::File::open(path).await.ok()?;
    let len = file.metadata().await.ok()?.len();
    file.seek(std::io::SeekFrom::Start(len.saturating_sub(256 * 1024)))
        .await
        .ok()?;
    let mut bytes = Vec::new();
    file.take(256 * 1024).read_to_end(&mut bytes).await.ok()?;
    Some(bytes)
}

fn quota_reset(value: &Value) -> Option<Option<u64>> {
    let quota = value.get("quotaLimits")?;
    (quota["status"] == "rejected"
        && matches!(
            quota["rateLimitType"].as_str(),
            Some(
                "five_hour"
                    | "seven_day"
                    | "seven_day_opus"
                    | "seven_day_sonnet"
                    | "seven_day_overage_included"
                    | "overage"
            )
        ))
    .then(|| quota.get("resetsAt").and_then(Value::as_u64))
}

fn reset_time(value: &Value) -> Option<u64> {
    ["resets_at", "resetsAt", "reset_at", "retry_at"]
        .iter()
        .find_map(|key| value.get(key).and_then(Value::as_u64))
        .or_else(|| {
            value
                .pointer("/codex_error_info/usage_limit_exceeded")
                .and_then(reset_time)
        })
}

/// Copy Claude's native conversation store only; authentication and settings
/// are deliberately outside these paths. Codex uses thread/fork's path import.
pub fn transfer_claude(
    source: &ResolvedAccount,
    destination: &ResolvedAccount,
    transcript: &Path,
    session_id: &str,
) -> Result<(), String> {
    if session_id.is_empty()
        || session_id.contains(['/', '\\'])
        || session_id == "."
        || session_id == ".."
    {
        return Err("invalid native session ID".into());
    }
    let source_home = std::fs::canonicalize(account_home(source)).map_err(|e| e.to_string())?;
    let destination_home = account_home(destination);
    std::fs::create_dir_all(&destination_home).map_err(|e| e.to_string())?;
    let destination_home = std::fs::canonicalize(destination_home).map_err(|e| e.to_string())?;
    if source_home == destination_home {
        return Ok(());
    }
    let transcript = std::fs::canonicalize(transcript).map_err(|e| e.to_string())?;
    let relative = transcript
        .strip_prefix(source_home.join("projects"))
        .map_err(|_| "Claude transcript is outside the source account's projects directory")?;
    if relative.components().count() != 2
        || transcript.file_stem().and_then(|s| s.to_str()) != Some(session_id)
    {
        return Err("Claude transcript does not match its bound session".into());
    }
    let target = destination_home.join("projects").join(relative);
    copy_history(&transcript, &target, &destination_home, 0)?;
    let sidecar = transcript.with_extension("");
    if sidecar.exists() {
        copy_history(&sidecar, &target.with_extension(""), &destination_home, 0)?;
    }
    Ok(())
}

fn copy_history(source: &Path, target: &Path, root: &Path, depth: usize) -> Result<(), String> {
    use std::io::Write;
    if depth > 8 {
        return Err("session history nesting exceeds limit".into());
    }
    let meta = std::fs::symlink_metadata(source).map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink() {
        return Err("refusing symlink in session history".into());
    }
    let parent = target.parent().ok_or("history path has no parent")?;
    // Check each existing destination ancestor before creating or writing it.
    let mut path = root.to_path_buf();
    for component in parent
        .strip_prefix(root)
        .map_err(|e| e.to_string())?
        .components()
    {
        path.push(component);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if !meta.is_dir() || meta.file_type().is_symlink() => {
                return Err("unsafe destination history directory".into());
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&path).map_err(|e| e.to_string())?
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    if meta.is_dir() {
        if let Ok(meta) = std::fs::symlink_metadata(target) {
            if !meta.is_dir() || meta.file_type().is_symlink() {
                return Err("unsafe destination history directory".into());
            }
        } else {
            std::fs::create_dir(target).map_err(|e| e.to_string())?;
        }
        for entry in std::fs::read_dir(source).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            copy_history(
                &entry.path(),
                &target.join(entry.file_name()),
                root,
                depth + 1,
            )?;
        }
    } else if meta.is_file() {
        if let Ok(existing) = target.symlink_metadata() {
            // A cancelled/failed fork can leave a previous import. Reuse it
            // only if it is an unchanged prefix of this same conversation.
            // Divergent destination history must never be overwritten.
            if !existing.is_file()
                || existing.file_type().is_symlink()
                || !is_prefix(target, source)?
            {
                return Err(format!(
                    "destination history diverges: {}",
                    target.display()
                ));
            }
            if existing.len() == meta.len() {
                return Ok(());
            }
        }
        let mut input = std::fs::File::open(source).map_err(|e| e.to_string())?;
        let mut output =
            atomic_write_file::AtomicWriteFile::open(target).map_err(|e| e.to_string())?;
        std::io::copy(&mut input, &mut output).map_err(|e| e.to_string())?;
        output.flush().map_err(|e| e.to_string())?;
        output.commit().map_err(|e| e.to_string())?;
    } else {
        return Err("unexpected file type in session history".into());
    }
    Ok(())
}

fn is_prefix(prefix: &Path, full: &Path) -> Result<bool, String> {
    use std::io::Read;
    let mut prefix = std::fs::File::open(prefix).map_err(|e| e.to_string())?;
    let mut full = std::fs::File::open(full).map_err(|e| e.to_string())?;
    let mut left = [0; 8192];
    let mut right = [0; 8192];
    loop {
        let count = prefix.read(&mut left).map_err(|e| e.to_string())?;
        if count == 0 {
            return Ok(true);
        }
        if full.read_exact(&mut right[..count]).is_err() || left[..count] != right[..count] {
            return Ok(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cooldown_is_shared_by_aliases_persisted_and_expires() {
        let dir = libsloptest::tempfile::tempdir().unwrap();
        let mut config = SlopdConfig {
            control_socket: Some(dir.path().join("slopd.sock")),
            ..Default::default()
        };
        for (name, home) in [("a", "one"), ("alias", "one"), ("b", "two")] {
            config.accounts.insert(
                name.into(),
                libslop::AccountConfig::Settings(libslop::AccountSettings {
                    backend: Some(Backend::Codex),
                    config_dir: Some(dir.path().join(home)),
                    executable: None,
                    mcp: true,
                }),
            );
        }
        config.account_pools.insert(
            "pool".into(),
            libslop::AccountPoolConfig {
                accounts: vec!["a".into(), "alias".into(), "b".into()],
                cooldown_secs: 60,
            },
        );
        let mut health = AccountHealth::default();
        assert_eq!(
            health.select(&config, "pool", None, &[], 100).unwrap().name,
            "a"
        );
        assert_eq!(health.note(&config, "a", Some(200), 100).unwrap(), 200);
        assert_eq!(
            health
                .select(&config, "pool", Some("alias"), &[], 101)
                .unwrap()
                .name,
            "b"
        );
        let mut restarted = AccountHealth::default();
        assert_eq!(
            restarted
                .select(&config, "pool", None, &[], 101)
                .unwrap()
                .name,
            "b"
        );
        restarted.note(&config, "b", None, 101).unwrap();
        assert!(
            restarted
                .select(&config, "pool", None, &[], 102)
                .unwrap_err()
                .contains("161")
        );
        assert_eq!(
            restarted
                .select(&config, "pool", None, &[], 161)
                .unwrap()
                .name,
            "b"
        );
        assert_eq!(
            restarted
                .select(&config, "pool", None, &[], 201)
                .unwrap()
                .name,
            "a"
        );
        assert_eq!(
            restarted
                .select(&config, "pool", None, &["a".into()], 201)
                .unwrap()
                .name,
            "b"
        );
    }

    #[tokio::test]
    async fn depletion_excludes_temporary_throttling_auth_and_arbitrary_text() {
        for code in [
            "rate_limit_exceeded",
            "server_overloaded",
            "authentication_error",
        ] {
            assert_eq!(
                depletion(
                    Backend::Codex,
                    &json!({"error_code":code,"error":{"message":"You've hit your usage limit."}}),
                    None
                )
                .await,
                None
            );
        }
        assert_eq!(
            depletion(
                Backend::Codex,
                &json!({"error_code":"usage_limit_exceeded","error":{"resets_at":1234}}),
                None
            )
            .await,
            Some(Some(1234))
        );
        assert_eq!(
            depletion(
                Backend::Claude,
                &json!({"error":"rate_limit","error_details":"429 Too Many Requests"}),
                None
            )
            .await,
            None
        );
        assert_eq!(depletion(Backend::Claude, &json!({"error":"authentication_failed","last_assistant_message":"You've hit your usage limit."}), None).await, None);
        assert_eq!(depletion(Backend::Claude, &json!({"error":"rate_limit","quotaLimits":{"status":"rejected","rateLimitType":"five_hour","resetsAt":1234}}), None).await, Some(Some(1234)));
        let dir = libsloptest::tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        std::fs::write(&path, json!({"type":"event_msg","payload":{"type":"token_count","rate_limits":{
            "primary":{"used_percent":100,"resets_at":1234}, "secondary":{"used_percent":100,"resets_at":5678}
        }}}).to_string()).unwrap();
        assert_eq!(
            depletion(
                Backend::Codex,
                &json!({"error_code":"usage_limit_exceeded"}),
                path.to_str()
            )
            .await,
            Some(Some(5678))
        );
        std::fs::write(&path, format!("{}\n{}\n", json!({"type":"assistant","error":"rate_limit","quotaLimits":{"status":"rejected","rateLimitType":"seven_day","resetsAt":4567}}), json!({"type":"user"}))).unwrap();
        assert_eq!(
            depletion(
                Backend::Claude,
                &json!({"error":"rate_limit"}),
                path.to_str()
            )
            .await,
            None,
            "old errors cannot deplete a new turn"
        );
    }

    #[test]
    fn claude_transfer_copies_only_conversation_and_rejects_symlink_targets() {
        let dir = libsloptest::tempfile::tempdir().unwrap();
        let mut config = SlopdConfig::default();
        for name in ["a", "b"] {
            config.accounts.insert(
                name.into(),
                libslop::AccountConfig::Dir(dir.path().join(name)),
            );
        }
        let a = config.resolve_account(Some("a")).unwrap();
        let b = config.resolve_account(Some("b")).unwrap();
        let transcript = account_home(&a).join("projects/project/session.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "history").unwrap();
        std::fs::write(account_home(&a).join("credentials.json"), "secret").unwrap();
        transfer_claude(&a, &b, &transcript, "session").unwrap();
        assert_eq!(
            std::fs::read_to_string(account_home(&b).join("projects/project/session.jsonl"))
                .unwrap(),
            "history"
        );
        assert!(!account_home(&b).join("credentials.json").exists());
        std::fs::write(&transcript, "history extended after cancellation").unwrap();
        transfer_claude(&a, &b, &transcript, "session").unwrap();
        std::fs::write(
            account_home(&b).join("projects/project/session.jsonl"),
            "divergent",
        )
        .unwrap();
        assert!(
            transfer_claude(&a, &b, &transcript, "session")
                .unwrap_err()
                .contains("diverges")
        );
        std::fs::remove_dir_all(account_home(&b).join("projects")).unwrap();
        std::os::unix::fs::symlink(dir.path(), account_home(&b).join("projects")).unwrap();
        assert!(
            transfer_claude(&a, &b, &transcript, "session")
                .unwrap_err()
                .contains("unsafe")
        );
    }
}
