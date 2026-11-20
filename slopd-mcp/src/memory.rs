use std::collections::BTreeMap;
use std::fs::{File, OpenOptions, Permissions};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rmcp::ErrorData as McpError;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

const MAX_ENTRIES: usize = 10_000;

#[derive(Clone)]
pub(crate) struct MemoryStore(Arc<Mutex<Inner>>);

struct Inner {
    entries: BTreeMap<(String, String), Entry>,
    revision: u64,
    failed: bool,
    file: File,
    // A separate lifetime lock prevents stale state in a second server process.
    _lock: File,
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    namespace: String,
    key: String,
    content: String,
    tags: Vec<String>,
    revision: u64,
    created_at_unix_ms: u64,
    updated_at_unix_ms: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Event {
    Version {
        version: u32,
    },
    Saved {
        entry: Entry,
    },
    Deleted {
        namespace: String,
        key: String,
        revision: u64,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Save {
    #[serde(default = "default_namespace")]
    namespace: String,
    key: String,
    content: String,
    #[serde(default)]
    tags: Vec<String>,
    expected_revision: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Get {
    #[serde(default = "default_namespace")]
    namespace: String,
    key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Delete {
    #[serde(default = "default_namespace")]
    namespace: String,
    key: String,
    expected_revision: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    #[serde(default)]
    query: String,
    namespace: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}

fn default_namespace() -> String {
    "default".into()
}
fn default_limit() -> usize {
    20
}

impl MemoryStore {
    pub(crate) fn open(path: PathBuf) -> std::io::Result<Self> {
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".lock");
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
            std::fs::set_permissions(parent, Permissions::from_mode(0o700))?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(lock_path)?;
        lock.set_permissions(Permissions::from_mode(0o600))?;
        lock.try_lock()
            .map_err(|_| std::io::Error::other("memory journal is already in use"))?;
        let mut file = libslop::jsonl::open(&path)?;
        let mut entries = BTreeMap::new();
        let mut revision = 0;
        let mut version_seen = false;
        // open() repairs an interrupted tail; reject any remaining corruption,
        // including the last complete line, before permitting another write.
        for line in BufReader::new(File::open(&path)?).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Event>(&line) {
                Ok(Event::Version { version: 1 }) if !version_seen && revision == 0 => {
                    version_seen = true
                }
                Ok(Event::Saved { entry }) if version_seen && entry.revision > revision => {
                    revision = entry.revision;
                    entries.insert((entry.namespace.clone(), entry.key.clone()), entry);
                }
                Ok(Event::Deleted {
                    namespace,
                    key,
                    revision: next,
                }) if version_seen && next > revision => {
                    revision = next;
                    entries.remove(&(namespace, key));
                }
                _ => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "invalid or unsupported memory journal",
                    ));
                }
            }
        }
        if !version_seen && file.metadata()?.len() != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid or unsupported memory journal",
            ));
        }
        if file.metadata()?.len() == 0 {
            libslop::jsonl::append(&mut file, &Event::Version { version: 1 })?;
            file.sync_data()?;
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                File::open(parent)?.sync_all()?;
            }
        }
        Ok(Self(Arc::new(Mutex::new(Inner {
            entries,
            revision,
            failed: false,
            file,
            _lock: lock,
        }))))
    }

    pub(crate) async fn call(
        &self,
        name: &str,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<Value, McpError> {
        match name {
            "save_memory" => {
                let mut args: Save = parse(arguments)?;
                validate_identity(&args.namespace, &args.key)?;
                validate_text("content", &args.content, 16_384)?;
                normalize_tags(&mut args.tags)?;
                let mut inner = self.0.lock().await;
                inner.ensure_healthy()?;
                let identity = (args.namespace.clone(), args.key.clone());
                let previous = inner.entries.get(&identity);
                check_revision(args.expected_revision, previous)?;
                if let Some(entry) = previous
                    && entry.content == args.content
                    && entry.tags == args.tags
                {
                    return Ok(json!({"created": false, "changed": false, "memory": entry}));
                }
                if previous.is_none() && inner.entries.len() >= MAX_ENTRIES {
                    return Err(McpError::invalid_params(
                        "memory limit reached; delete an unused entry first",
                        None,
                    ));
                }
                let created = previous.is_none();
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let entry = Entry {
                    namespace: args.namespace,
                    key: args.key,
                    content: args.content,
                    tags: args.tags,
                    revision: inner.next_revision()?,
                    created_at_unix_ms: previous.map_or(now, |e| e.created_at_unix_ms),
                    updated_at_unix_ms: now,
                };
                inner.append(&Event::Saved {
                    entry: entry.clone(),
                })?;
                inner.revision = entry.revision;
                inner.entries.insert(identity, entry.clone());
                Ok(json!({"created": created, "changed": true, "memory": entry}))
            }
            "get_memory" => {
                let args: Get = parse(arguments)?;
                validate_identity(&args.namespace, &args.key)?;
                let inner = self.0.lock().await;
                inner.ensure_healthy()?;
                let entry = inner.entries.get(&(args.namespace, args.key));
                Ok(json!({"found": entry.is_some(), "memory": entry}))
            }
            "search_memories" => {
                let mut args: Search = parse(arguments)?;
                if args.query.len() > 512 || !(1..=100).contains(&args.limit) {
                    return Err(McpError::invalid_params(
                        "query must be at most 512 bytes and limit must be 1..100",
                        None,
                    ));
                }
                if let Some(namespace) = &args.namespace {
                    validate_text("namespace", namespace, 100)?;
                }
                normalize_tags(&mut args.tags)?;
                let query = args.query.to_lowercase();
                let terms: Vec<_> = query.split_whitespace().collect();
                let inner = self.0.lock().await;
                inner.ensure_healthy()?;
                let mut matches: Vec<_> = inner
                    .entries
                    .values()
                    .filter(|entry| {
                        if args
                            .namespace
                            .as_ref()
                            .is_some_and(|n| n != &entry.namespace)
                            || !args.tags.iter().all(|tag| entry.tags.contains(tag))
                        {
                            return false;
                        }
                        let text = format!(
                            "{} {} {} {}",
                            entry.namespace,
                            entry.key,
                            entry.content,
                            entry.tags.join(" ")
                        )
                        .to_lowercase();
                        terms.iter().all(|term| text.contains(term))
                    })
                    .collect();
                matches.sort_by_key(|entry| std::cmp::Reverse(entry.revision));
                let total = matches.len();
                let page: Vec<_> = matches
                    .into_iter()
                    .skip(args.offset)
                    .take(args.limit)
                    .collect();
                let next = args.offset.saturating_add(page.len());
                Ok(
                    json!({"memories": page, "total": total, "next_offset": (next < total).then_some(next)}),
                )
            }
            "delete_memory" => {
                let args: Delete = parse(arguments)?;
                validate_identity(&args.namespace, &args.key)?;
                let mut inner = self.0.lock().await;
                inner.ensure_healthy()?;
                let identity = (args.namespace.clone(), args.key.clone());
                let previous = inner.entries.get(&identity);
                check_revision(args.expected_revision, previous)?;
                let deleted = previous.is_some();
                if deleted {
                    let revision = inner.next_revision()?;
                    inner.append(&Event::Deleted {
                        namespace: args.namespace,
                        key: args.key,
                        revision,
                    })?;
                    inner.revision = revision;
                    inner.entries.remove(&identity);
                }
                Ok(json!({"deleted": deleted}))
            }
            _ => unreachable!("unknown memory tool"),
        }
    }
}

impl Inner {
    fn ensure_healthy(&self) -> Result<(), McpError> {
        if self.failed {
            return Err(McpError::internal_error(
                "memory persistence is uncertain; restart the MCP adapter and read the entry before retrying",
                None,
            ));
        }
        Ok(())
    }

    fn next_revision(&self) -> Result<u64, McpError> {
        self.revision
            .checked_add(1)
            .ok_or_else(|| McpError::internal_error("memory revision exhausted", None))
    }

    fn append(&mut self, event: &Event) -> Result<(), McpError> {
        libslop::jsonl::append(&mut self.file, event)
            .and_then(|()| self.file.sync_data())
            .map_err(|error| {
                self.failed = true;
                tracing::error!(%error, "failed to persist memory");
                McpError::internal_error(
                    "memory persistence failed; restart the MCP adapter and read the entry before retrying",
                    None,
                )
            })
    }
}

fn parse<T: DeserializeOwned>(arguments: Option<&Map<String, Value>>) -> Result<T, McpError> {
    serde_json::from_value(Value::Object(arguments.cloned().unwrap_or_default())).map_err(|_| {
        McpError::invalid_params(
            "invalid memory arguments; follow this tool's input schema",
            None,
        )
    })
}

fn validate_identity(namespace: &str, key: &str) -> Result<(), McpError> {
    validate_text("namespace", namespace, 100)?;
    validate_text("key", key, 200)
}

fn validate_text(field: &str, text: &str, max: usize) -> Result<(), McpError> {
    if text.trim().is_empty() || text.len() > max || text.contains('\0') {
        return Err(McpError::invalid_params(
            format!(
                "{field} must contain non-whitespace text, at most {max} UTF-8 bytes, and no NUL"
            ),
            None,
        ));
    }
    Ok(())
}

fn normalize_tags(tags: &mut Vec<String>) -> Result<(), McpError> {
    if tags.len() > 20 {
        return Err(McpError::invalid_params(
            "at most 20 tags are allowed",
            None,
        ));
    }
    for tag in tags.iter() {
        validate_text("tag", tag, 64)?;
    }
    tags.sort();
    tags.dedup();
    Ok(())
}

fn check_revision(expected: Option<u64>, entry: Option<&Entry>) -> Result<(), McpError> {
    let current = entry.map_or(0, |e| e.revision);
    if expected.is_some_and(|e| e != current) {
        return Err(McpError::invalid_params(
            "memory changed; get_memory before retrying",
            Some(json!({"current_revision": current})),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    async fn call(store: &MemoryStore, name: &str, arguments: Value) -> Result<Value, McpError> {
        store.call(name, arguments.as_object()).await
    }

    #[tokio::test]
    async fn journal_repairs_partial_tail_and_rejects_a_second_owner() {
        let dir = libsloptest::tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.jsonl");
        let store = MemoryStore::open(path.clone()).unwrap();
        assert!(MemoryStore::open(path.clone()).is_err());
        let saved = call(
            &store,
            "save_memory",
            json!({"key":"language","content":"ქართული"}),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        drop(store);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"event\":\"saved\"")
            .unwrap();
        let store = MemoryStore::open(path.clone()).unwrap();
        let found = call(&store, "search_memories", json!({"query":"ქართული"}))
            .await
            .unwrap();
        assert_eq!(found["memories"][0], saved["memory"]);
        call(
            &store,
            "save_memory",
            json!({"key":"other","content":"survives repair"}),
        )
        .await
        .unwrap();
        drop(store);
        let store = MemoryStore::open(path).unwrap();
        assert_eq!(
            call(&store, "search_memories", json!({})).await.unwrap()["total"],
            2
        );
    }

    #[test]
    fn unsupported_journal_fails_without_replacing_existing_records() {
        let dir = libsloptest::tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.jsonl");
        let original = b"{\"event\":\"version\",\"version\":99}\n";
        std::fs::write(&path, original).unwrap();
        assert!(MemoryStore::open(path.clone()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[test]
    fn corrupt_complete_tail_is_rejected_without_changing_the_journal() {
        let dir = libsloptest::tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.jsonl");
        let original = b"{\"event\":\"version\",\"version\":1}\n{not-json}\n";
        std::fs::write(&path, original).unwrap();
        assert!(MemoryStore::open(path.clone()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[tokio::test]
    async fn concurrent_revision_guards_allow_only_one_update() {
        let dir = libsloptest::tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path().join("memory.jsonl")).unwrap();
        let initial = call(
            &store,
            "save_memory",
            json!({"key":"k","content":"original"}),
        )
        .await
        .unwrap();
        let revision = initial["memory"]["revision"].as_u64().unwrap();
        let (one, two) = tokio::join!(
            call(
                &store,
                "save_memory",
                json!({"key":"k","content":"one","expected_revision":revision})
            ),
            call(
                &store,
                "save_memory",
                json!({"key":"k","content":"two","expected_revision":revision})
            ),
        );
        assert_ne!(one.is_ok(), two.is_ok());
    }

    #[tokio::test]
    async fn failed_write_never_reports_success_and_blocks_stale_reads() {
        let dir = libsloptest::tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.jsonl");
        let store = MemoryStore::open(path.clone()).unwrap();
        let initial = call(
            &store,
            "save_memory",
            json!({"key":"k","content":"original"}),
        )
        .await
        .unwrap();
        store.0.lock().await.file = File::open(&path).unwrap();
        assert!(
            call(
                &store,
                "save_memory",
                json!({"key":"k","content":"cannot persist"})
            )
            .await
            .is_err()
        );
        assert!(
            call(&store, "get_memory", json!({"key":"k"}))
                .await
                .is_err()
        );
        drop(store);
        let store = MemoryStore::open(path).unwrap();
        let restored = call(&store, "get_memory", json!({"key":"k"}))
            .await
            .unwrap();
        assert_eq!(restored["memory"], initial["memory"]);
    }
}
