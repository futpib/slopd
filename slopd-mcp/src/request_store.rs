//! Durable copy of the agent-request mailbox.
//!
//! The mailbox is what lets `get_agent_result` answer for work started on an
//! earlier turn or by an earlier MCP session. Keeping it only in memory meant a
//! slopd-mcp restart (an upgrade, a crash) forgot every request, so clients
//! were told the request_id was unknown even when the agent finished normally.
//! The mailbox is small (bounded by `MAILBOX_LIMIT`), so each change rewrites
//! one JSON snapshot atomically instead of keeping a journal.

use std::fs::{File, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct StoredRequest {
    pub request_id: String,
    #[serde(default)]
    pub pane_id: Option<String>,
    pub prompt: String,
    #[serde(default)]
    pub selector_key: String,
    #[serde(default)]
    pub dedupe_key: String,
    pub created_at_unix_ms: u64,
    /// Set once slopd confirmed the prompt was submitted to `pane_id`. Only a
    /// delivered request can be resumed after a restart; anything else may or
    /// may not have reached the agent.
    #[serde(default)]
    pub delivered: bool,
    /// Newest transcript cursor that existed before the prompt was sent; the
    /// reply is searched for strictly after it. None means the transcript was
    /// empty, so every record is a candidate.
    #[serde(default)]
    pub after_cursor: Option<u64>,
    pub status: StoredStatus,
    #[serde(default)]
    pub reply: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StoredStatus {
    Pending,
    Completed,
    Failed,
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    requests: Vec<StoredRequest>,
}

/// Owns the snapshot file. The lock file keeps a second slopd-mcp from
/// overwriting the same mailbox.
pub(crate) struct RequestStore {
    path: PathBuf,
    _lock: File,
}

impl RequestStore {
    pub(crate) fn open(path: PathBuf) -> std::io::Result<(Self, Vec<StoredRequest>)> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
            std::fs::set_permissions(parent, Permissions::from_mode(0o700))?;
        }
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(lock_path)?;
        lock.try_lock()
            .map_err(|_| std::io::Error::other("request mailbox is already in use"))?;
        let requests = match std::fs::read(&path) {
            Ok(bytes) => {
                let snapshot: Snapshot = serde_json::from_slice(&bytes).map_err(|error| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("invalid request mailbox {}: {error}", path.display()),
                    )
                })?;
                if snapshot.version != VERSION {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "unsupported request mailbox version {} in {}",
                            snapshot.version,
                            path.display()
                        ),
                    ));
                }
                snapshot.requests
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        Ok((Self { path, _lock: lock }, requests))
    }

    /// Replace the snapshot with `requests`, oldest first.
    pub(crate) fn save(&self, requests: Vec<StoredRequest>) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(&Snapshot {
            version: VERSION,
            requests,
        })
        .map_err(std::io::Error::other)?;
        write_atomically(&self.path, &bytes)
    }
}

fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut temp = path.as_os_str().to_os_string();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(bytes)?;
    file.sync_data()?;
    drop(file);
    std::fs::rename(&temp, path)?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: &str, status: StoredStatus) -> StoredRequest {
        StoredRequest {
            request_id: id.into(),
            pane_id: Some("%7".into()),
            prompt: "hello".into(),
            selector_key: "selector".into(),
            dedupe_key: "dedupe".into(),
            created_at_unix_ms: 1,
            delivered: true,
            after_cursor: Some(42),
            status,
            reply: (status == StoredStatus::Completed).then(|| "hi".into()),
            error: None,
        }
    }

    #[test]
    fn round_trips_and_rejects_a_second_owner() {
        let dir = libsloptest::tempfile::tempdir().unwrap();
        let path = dir.path().join("state/requests.json");
        let (store, loaded) = RequestStore::open(path.clone()).unwrap();
        assert!(loaded.is_empty());
        assert!(RequestStore::open(path.clone()).is_err());
        let saved = vec![
            request("a", StoredStatus::Completed),
            request("b", StoredStatus::Pending),
        ];
        store.save(saved.clone()).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(store);
        let (_store, loaded) = RequestStore::open(path).unwrap();
        assert_eq!(loaded, saved);
    }

    #[test]
    fn unsupported_snapshot_is_rejected_without_being_replaced() {
        let dir = libsloptest::tempfile::tempdir().unwrap();
        let path = dir.path().join("requests.json");
        let original = br#"{"version":99,"requests":[]}"#;
        std::fs::write(&path, original).unwrap();
        assert!(RequestStore::open(path.clone()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), original);
    }
}
