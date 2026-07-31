//! Durable pane lifecycle storage.
//!
//! The storage hierarchy is rooted in the configured tmux target (socket plus
//! session name), so independent slopd instances never share recovery state.
//! Inside that target, one append-only JSONL file is used for each tmux session
//! incarnation, identified by the tmux server boot UUID and `#{session_id}`.
//! Pane metadata is appended only when it changes; a compact checkpoint names
//! the pane records that form the current backup. Pane deaths and revivals live
//! in the same journal and are retained indefinitely.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct GenerationKey {
    pub tmux_boot_id: String,
    pub tmux_session_id: String,
}

impl GenerationKey {
    fn file_name(&self) -> String {
        format!(
            "{}-{}.jsonl",
            hex(self.tmux_boot_id.as_bytes()),
            hex(self.tmux_session_id.as_bytes())
        )
    }

    pub(crate) fn display(&self) -> String {
        format!("{}:{}", self.tmux_boot_id, self.tmux_session_id)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum JournalEvent {
    Generation {
        version: u32,
        started_at: u64,
        tmux_boot_id: String,
        tmux_session_id: String,
    },
    Pane {
        at: u64,
        pane: libslop::PaneInfo,
    },
    Checkpoint {
        at: u64,
        pane_ids: Vec<String>,
    },
    Destroyed {
        entry: libslop::GraveEntry,
    },
    Revived {
        at: u64,
        grave_id: String,
        pane_id: String,
        tmux_boot_id: String,
        tmux_session_id: String,
    },
    RestoreResolved {
        at: u64,
        source: GenerationKey,
        action: String,
    },
}

#[derive(Default)]
struct GenerationState {
    key: Option<GenerationKey>,
    started_at: u64,
    pane_versions: HashMap<String, libslop::PaneInfo>,
    checkpoint: Vec<libslop::PaneInfo>,
    has_checkpoint: bool,
    graves: Vec<libslop::GraveEntry>,
    revivals: Vec<(String, u64, String)>,
    resolved_sources: HashSet<GenerationKey>,
}

struct CurrentState {
    generation: GenerationKey,
    file: File,
    pane_versions: HashMap<String, libslop::PaneInfo>,
    checkpoint: Vec<libslop::PaneInfo>,
}

pub(crate) struct LifecycleJournal {
    root: PathBuf,
    current: Mutex<CurrentState>,
    generation_refresh: tokio::sync::Mutex<()>,
}

impl LifecycleJournal {
    pub(crate) fn open(
        config: &libslop::SlopdConfig,
        generation: GenerationKey,
        started_at: u64,
    ) -> Result<Self, String> {
        let root = target_root(config);
        Self::open_at(root, generation, started_at)
    }

    fn open_at(root: PathBuf, generation: GenerationKey, started_at: u64) -> Result<Self, String> {
        let generations = root.join("generations");
        std::fs::create_dir_all(&generations).map_err(|e| {
            format!(
                "failed to create lifecycle journal {}: {e}",
                generations.display()
            )
        })?;
        let (file, state) = open_generation(&generations, &generation, started_at)?;
        Ok(Self {
            root,
            current: Mutex::new(CurrentState {
                generation,
                file,
                pane_versions: state.pane_versions,
                checkpoint: state.checkpoint,
            }),
            generation_refresh: tokio::sync::Mutex::new(()),
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn current_generation(&self) -> GenerationKey {
        self.current.lock().unwrap().generation.clone()
    }

    pub(crate) async fn lock_generation_refresh(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.generation_refresh.lock().await
    }

    /// Switch after slopd recreates its managed tmux session while remaining
    /// alive. A daemon restart against the same session simply re-opens the same
    /// generation file.
    pub(crate) fn switch_generation(
        &self,
        generation: GenerationKey,
        started_at: u64,
    ) -> Result<(), String> {
        let mut current = self.current.lock().unwrap();
        if current.generation == generation {
            return Ok(());
        }
        let (file, state) =
            open_generation(&self.root.join("generations"), &generation, started_at)?;
        *current = CurrentState {
            generation,
            file,
            pane_versions: state.pane_versions,
            checkpoint: state.checkpoint,
        };
        Ok(())
    }

    /// Record a backup checkpoint. Pane bodies are appended only when their
    /// recovery metadata changed; an unchanged periodic backup performs only an
    /// fdatasync and adds no journal growth.
    pub(crate) fn checkpoint(&self, mut panes: Vec<libslop::PaneInfo>) -> Result<usize, String> {
        panes.retain(|pane| pane.session_id.is_some());
        panes.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
        let mut current = self.current.lock().unwrap();
        if current.checkpoint == panes {
            current
                .file
                .sync_data()
                .map_err(|e| format!("failed to sync lifecycle journal: {e}"))?;
            return Ok(panes.len());
        }

        let at = now();
        for pane in &panes {
            if current.pane_versions.get(&pane.pane_id) != Some(pane) {
                append_event(
                    &mut current.file,
                    &JournalEvent::Pane {
                        at,
                        pane: pane.clone(),
                    },
                )?;
                current
                    .pane_versions
                    .insert(pane.pane_id.clone(), pane.clone());
            }
        }
        append_event(
            &mut current.file,
            &JournalEvent::Checkpoint {
                at,
                pane_ids: panes.iter().map(|pane| pane.pane_id.clone()).collect(),
            },
        )?;
        current
            .file
            .sync_data()
            .map_err(|e| format!("failed to sync lifecycle journal: {e}"))?;
        let live_ids: HashSet<&str> = panes.iter().map(|pane| pane.pane_id.as_str()).collect();
        current
            .pane_versions
            .retain(|pane_id, _| live_ids.contains(pane_id.as_str()));
        current.checkpoint = panes;
        Ok(current.checkpoint.len())
    }

    pub(crate) fn checkpoint_pane(&self, pane_id: &str) -> Option<libslop::PaneInfo> {
        self.current
            .lock()
            .unwrap()
            .checkpoint
            .iter()
            .find(|pane| pane.pane_id == pane_id)
            .cloned()
    }

    pub(crate) fn record_destroyed(&self, entry: libslop::GraveEntry) -> Result<(), String> {
        let mut current = self.current.lock().unwrap();
        append_event(&mut current.file, &JournalEvent::Destroyed { entry })?;
        current
            .file
            .sync_data()
            .map_err(|e| format!("failed to sync pane grave record: {e}"))
    }

    pub(crate) fn record_revived(&self, grave_id: &str, pane_id: &str) -> Result<(), String> {
        let mut current = self.current.lock().unwrap();
        let generation = current.generation.clone();
        append_event(
            &mut current.file,
            &JournalEvent::Revived {
                at: now(),
                grave_id: grave_id.to_string(),
                pane_id: pane_id.to_string(),
                tmux_boot_id: generation.tmux_boot_id,
                tmux_session_id: generation.tmux_session_id,
            },
        )?;
        current
            .file
            .sync_data()
            .map_err(|e| format!("failed to sync pane revival record: {e}"))
    }

    pub(crate) fn resolve_restore(
        &self,
        source: &GenerationKey,
        action: &str,
    ) -> Result<(), String> {
        let mut current = self.current.lock().unwrap();
        append_event(
            &mut current.file,
            &JournalEvent::RestoreResolved {
                at: now(),
                source: source.clone(),
                action: action.to_string(),
            },
        )?;
        current
            .file
            .sync_data()
            .map_err(|e| format!("failed to sync restore resolution: {e}"))
    }

    /// Latest checkpoint from an older, not-yet-consumed generation. This is
    /// the reboot restore point and remains discoverable across daemon restarts
    /// without a separate `.pending` marker.
    pub(crate) fn pending_restore(
        &self,
    ) -> Result<Option<(GenerationKey, Vec<libslop::PaneInfo>)>, String> {
        self.latest_checkpoint(false, true)
    }

    /// Restore source for an explicit `slopctl restore`: use a pending older
    /// generation when one exists, otherwise the current generation's latest
    /// checkpoint (preserving the historical backup→kill→restore workflow).
    pub(crate) fn manual_restore_source(
        &self,
    ) -> Result<Option<(GenerationKey, Vec<libslop::PaneInfo>)>, String> {
        if let Some(source) = self.pending_restore()? {
            return Ok(Some(source));
        }
        self.latest_checkpoint(true, false)
    }

    fn latest_checkpoint(
        &self,
        include_current: bool,
        unresolved_only: bool,
    ) -> Result<Option<(GenerationKey, Vec<libslop::PaneInfo>)>, String> {
        self.flush()?;
        let current = self.current_generation();
        let generations = self.read_generations()?;
        let resolved: HashSet<GenerationKey> = generations
            .iter()
            .flat_map(|generation| generation.resolved_sources.iter().cloned())
            .collect();
        for generation in generations.iter().rev() {
            let Some(key) = generation.key.as_ref() else {
                continue;
            };
            if !include_current && *key == current {
                continue;
            }
            if unresolved_only && resolved.contains(key) {
                continue;
            }
            // A freshly recreated session may checkpoint its empty live set
            // before the user resolves an older recovery point. That empty
            // generation is not itself useful to restore and must not hide the
            // older non-empty checkpoint.
            if unresolved_only && generation.checkpoint.is_empty() {
                continue;
            }
            if generation.has_checkpoint {
                return Ok(Some((key.clone(), generation.checkpoint.clone())));
            }
        }
        Ok(None)
    }

    pub(crate) fn graveyard(
        &self,
        boot: Option<i32>,
        limit: usize,
    ) -> Result<Vec<libslop::GraveEntry>, String> {
        if boot.is_some_and(|boot| boot > 0) {
            return Err("--boot must be 0 (current) or a negative generation offset".into());
        }
        self.flush()?;
        let generations = self.read_generations()?;
        let selected = select_generations(&generations, &self.current_generation(), boot)?;
        let mut revived: HashMap<String, (u64, String)> = HashMap::new();
        for generation in &generations {
            for (grave_id, at, pane_id) in &generation.revivals {
                revived.insert(grave_id.clone(), (*at, pane_id.clone()));
            }
        }
        let mut entries: Vec<libslop::GraveEntry> = selected
            .into_iter()
            .flat_map(|generation| generation.graves.iter().cloned())
            .collect();
        for entry in &mut entries {
            if let Some((at, pane_id)) = revived.get(&entry.grave_id) {
                entry.revived_at = Some(*at);
                entry.revived_as = Some(pane_id.clone());
            }
        }
        entries.sort_by(|a, b| {
            b.destroyed_at
                .cmp(&a.destroyed_at)
                .then_with(|| b.grave_id.cmp(&a.grave_id))
        });
        entries.truncate(limit);
        Ok(entries)
    }

    pub(crate) fn select_grave(
        &self,
        target: Option<&str>,
        boot: Option<i32>,
    ) -> Result<libslop::GraveEntry, String> {
        let entries = self.graveyard(boot, usize::MAX)?;
        let mut matches: Vec<libslop::GraveEntry> = match target {
            None => entries
                .into_iter()
                .filter(|entry| entry.revived_at.is_none())
                .take(1)
                .collect(),
            Some(target) if target.starts_with('%') => entries
                .into_iter()
                .filter(|entry| entry.pane.pane_id == target)
                .collect(),
            Some(target) => entries
                .into_iter()
                .filter(|entry| entry.grave_id.starts_with(target))
                .collect(),
        };
        if matches.is_empty() {
            return Err(match target {
                Some(target) => format!("no graveyard entry matches {target:?}"),
                None => "the graveyard has no unrevived panes".to_string(),
            });
        }
        if matches.len() > 1 {
            let candidates = matches
                .iter()
                .take(8)
                .map(|entry| {
                    format!(
                        "{} ({} {}, boot {} session {})",
                        &entry.grave_id[..entry.grave_id.len().min(8)],
                        entry.pane.pane_id,
                        entry.pane.pane_title.as_deref().unwrap_or("untitled"),
                        &entry.tmux_boot_id[..entry.tmux_boot_id.len().min(8)],
                        entry.tmux_session_id,
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "graveyard target is ambiguous; use a grave id or --boot: {candidates}"
            ));
        }
        let entry = matches.remove(0);
        if let Some(pane_id) = &entry.revived_as {
            return Err(format!(
                "grave {} was already revived as {}; kill that pane to create a new grave entry",
                &entry.grave_id[..entry.grave_id.len().min(8)],
                pane_id
            ));
        }
        Ok(entry)
    }

    /// Import the old single-manifest format as a synthetic previous
    /// generation. This is intentionally called only for the default tmux
    /// target, so a secondary daemon cannot accidentally adopt the main
    /// instance's old manifest.
    pub(crate) fn import_legacy(
        &self,
        panes: Vec<libslop::PaneInfo>,
        started_at: u64,
    ) -> Result<bool, String> {
        if panes.is_empty() || self.read_generations()?.len() > 1 {
            return Ok(false);
        }
        let generations_dir = self.root.join("generations");
        let key = GenerationKey {
            tmux_boot_id: "legacy-manifest".to_string(),
            tmux_session_id: "$legacy".to_string(),
        };
        let path = generations_dir.join(key.file_name());
        if path.exists() {
            return Ok(false);
        }
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(|e| format!("failed to create legacy journal {}: {e}", path.display()))?;
        append_event(
            &mut file,
            &JournalEvent::Generation {
                version: 1,
                started_at,
                tmux_boot_id: key.tmux_boot_id,
                tmux_session_id: key.tmux_session_id,
            },
        )?;
        let at = now();
        for pane in &panes {
            append_event(
                &mut file,
                &JournalEvent::Pane {
                    at,
                    pane: pane.clone(),
                },
            )?;
        }
        append_event(
            &mut file,
            &JournalEvent::Checkpoint {
                at,
                pane_ids: panes.into_iter().map(|pane| pane.pane_id).collect(),
            },
        )?;
        file.sync_data()
            .map_err(|e| format!("failed to sync imported legacy manifest: {e}"))?;
        Ok(true)
    }

    fn flush(&self) -> Result<(), String> {
        self.current
            .lock()
            .unwrap()
            .file
            .sync_data()
            .map_err(|e| format!("failed to sync lifecycle journal: {e}"))
    }

    fn read_generations(&self) -> Result<Vec<GenerationState>, String> {
        let dir = self.root.join("generations");
        let mut generations = Vec::new();
        for entry in std::fs::read_dir(&dir)
            .map_err(|e| format!("failed to read journal directory {}: {e}", dir.display()))?
        {
            let entry = entry.map_err(|e| format!("failed to read journal entry: {e}"))?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            generations.push(read_generation(&path)?);
        }
        generations.sort_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then_with(|| match (a.key.as_ref(), b.key.as_ref()) {
                    (Some(a), Some(b)) if a.tmux_boot_id == b.tmux_boot_id => {
                        tmux_session_number(&a.tmux_session_id)
                            .cmp(&tmux_session_number(&b.tmux_session_id))
                            .then_with(|| a.tmux_session_id.cmp(&b.tmux_session_id))
                    }
                    (a, b) => a
                        .map(GenerationKey::display)
                        .cmp(&b.map(GenerationKey::display)),
                })
        });
        Ok(generations)
    }
}

fn target_root(config: &libslop::SlopdConfig) -> PathBuf {
    target_root_at(config, &libslop::state_dir().join("slopd"))
}

fn target_root_at(config: &libslop::SlopdConfig, base: &Path) -> PathBuf {
    let socket = match config.tmux.socket.as_deref() {
        Some(socket) => {
            let expanded = libslop::expand_path(socket);
            if expanded.is_absolute() {
                expanded.to_string_lossy().into_owned()
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(expanded)
                    .to_string_lossy()
                    .into_owned()
            }
        }
        None => "default".to_string(),
    };
    base.join("tmux-targets")
        .join(hex(socket.as_bytes()))
        .join(hex(config.tmux.session().as_bytes()))
}

fn open_generation(
    dir: &Path,
    generation: &GenerationKey,
    started_at: u64,
) -> Result<(File, GenerationState), String> {
    std::fs::create_dir_all(dir).map_err(|e| {
        format!(
            "failed to create generation directory {}: {e}",
            dir.display()
        )
    })?;
    let path = dir.join(generation.file_name());
    repair_final_line(&path)?;
    let exists = path.exists();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(&path)
        .map_err(|e| format!("failed to open lifecycle journal {}: {e}", path.display()))?;
    if !exists || file.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
        append_event(
            &mut file,
            &JournalEvent::Generation {
                version: 1,
                started_at,
                tmux_boot_id: generation.tmux_boot_id.clone(),
                tmux_session_id: generation.tmux_session_id.clone(),
            },
        )?;
        file.sync_data()
            .map_err(|e| format!("failed to sync generation header: {e}"))?;
    }
    let state = read_generation(&path)?;
    Ok((file, state))
}

/// Make an append-only file writable again after a process died partway through
/// its last `write_all`. A complete JSON record missing only its newline is
/// terminated; an incomplete final record is truncated. Interior corruption is
/// deliberately left for `read_generation` to report.
fn repair_final_line(path: &Path) -> Result<(), String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) if !bytes.is_empty() => bytes,
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("failed to inspect {}: {e}", path.display())),
    };
    let mut end = bytes.len();
    while end > 0 && (bytes[end - 1] == b'\n' || bytes[end - 1] == b'\r') {
        end -= 1;
    }
    if end == 0 {
        return Ok(());
    }
    let start = bytes[..end]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let line = &bytes[start..end];
    if serde_json::from_slice::<serde_json::Value>(line).is_ok() {
        if bytes.last() != Some(&b'\n') {
            let mut file = OpenOptions::new()
                .append(true)
                .open(path)
                .map_err(|e| format!("failed to repair {}: {e}", path.display()))?;
            file.write_all(b"\n")
                .map_err(|e| format!("failed to repair {}: {e}", path.display()))?;
        }
    } else {
        OpenOptions::new()
            .write(true)
            .open(path)
            .and_then(|file| file.set_len(start as u64))
            .map_err(|e| format!("failed to truncate incomplete {}: {e}", path.display()))?;
    }
    Ok(())
}

fn read_generation(path: &Path) -> Result<GenerationState, String> {
    let file = File::open(path)
        .map_err(|e| format!("failed to open lifecycle journal {}: {e}", path.display()))?;
    let mut state = GenerationState::default();
    let mut lines = BufReader::new(file).lines().peekable();
    while let Some(line) = lines.next() {
        let line = line.map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        let event: JournalEvent = match serde_json::from_str(&line) {
            Ok(event) => event,
            // A crash may leave only the final append incomplete. Ignore that
            // tail; malformed records in the middle indicate real corruption.
            Err(_) if lines.peek().is_none() => break,
            Err(e) => {
                return Err(format!(
                    "malformed lifecycle journal {}: {e}",
                    path.display()
                ));
            }
        };
        match event {
            JournalEvent::Generation {
                started_at,
                tmux_boot_id,
                tmux_session_id,
                ..
            } => {
                state.started_at = started_at;
                state.key = Some(GenerationKey {
                    tmux_boot_id,
                    tmux_session_id,
                });
            }
            JournalEvent::Pane { pane, .. } => {
                state.pane_versions.insert(pane.pane_id.clone(), pane);
            }
            JournalEvent::Checkpoint { pane_ids, .. } => {
                state.checkpoint = pane_ids
                    .iter()
                    .filter_map(|pane_id| state.pane_versions.get(pane_id).cloned())
                    .collect();
                let live_ids: HashSet<&str> = pane_ids.iter().map(String::as_str).collect();
                state
                    .pane_versions
                    .retain(|pane_id, _| live_ids.contains(pane_id.as_str()));
                state.has_checkpoint = true;
            }
            JournalEvent::Destroyed { entry } => state.graves.push(entry),
            JournalEvent::Revived {
                at,
                grave_id,
                pane_id,
                ..
            } => state.revivals.push((grave_id, at, pane_id)),
            JournalEvent::RestoreResolved { source, .. } => {
                state.resolved_sources.insert(source);
            }
        }
    }
    Ok(state)
}

fn select_generations<'a>(
    generations: &'a [GenerationState],
    current: &GenerationKey,
    boot: Option<i32>,
) -> Result<Vec<&'a GenerationState>, String> {
    let Some(offset) = boot else {
        return Ok(generations.iter().collect());
    };
    let current_index = generations
        .iter()
        .position(|generation| generation.key.as_ref() == Some(current))
        .ok_or_else(|| "current tmux generation is missing from the journal".to_string())?;
    let index = current_index as i64 + offset as i64;
    if index < 0 || index >= generations.len() as i64 {
        return Err(format!("no tmux generation exists at --boot {offset}"));
    }
    Ok(vec![&generations[index as usize]])
}

fn append_event(file: &mut File, event: &JournalEvent) -> Result<(), String> {
    let mut line = serde_json::to_vec(event)
        .map_err(|e| format!("failed to serialize lifecycle event: {e}"))?;
    line.push(b'\n');
    file.write_all(&line)
        .map_err(|e| format!("failed to append lifecycle event: {e}"))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn tmux_session_number(id: &str) -> Option<u64> {
    id.strip_prefix('$').and_then(|number| number.parse().ok())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0xf) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: &str, session: &str) -> libslop::PaneInfo {
        libslop::PaneInfo {
            pane_id: id.to_string(),
            created_at: 1,
            last_active: 2,
            session_id: Some(session.to_string()),
            parent_pane_id: None,
            tags: vec![],
            state: libslop::PaneState::Ready,
            detailed_state: libslop::PaneDetailedState::Ready,
            working_dir: Some("/tmp".to_string()),
            transcript_path: None,
            account: libslop::DEFAULT_ACCOUNT.to_string(),
            backend: libslop::Backend::Claude,
            pane_title: Some("test".to_string()),
        }
    }

    fn config(dir: &Path, session: &str) -> libslop::SlopdConfig {
        let path = dir.join(format!("{session}.toml"));
        std::fs::write(
            &path,
            format!(
                "[tmux]\nsocket = {:?}\nsession = {:?}\n",
                dir.join("tmux.sock"),
                session
            ),
        )
        .unwrap();
        libslop::SlopdConfig::load_from(&path)
    }

    fn open(
        dir: &Path,
        config: &libslop::SlopdConfig,
        generation: GenerationKey,
        started_at: u64,
    ) -> LifecycleJournal {
        LifecycleJournal::open_at(
            target_root_at(config, &dir.join("state")),
            generation,
            started_at,
        )
        .unwrap()
    }

    fn grave(journal: &LifecycleJournal, id: &str, pane_id: &str, session: &str, at: u64) {
        let generation = journal.current_generation();
        journal
            .record_destroyed(libslop::GraveEntry {
                grave_id: id.to_string(),
                tmux_boot_id: generation.tmux_boot_id,
                tmux_session_id: generation.tmux_session_id,
                destroyed_at: at,
                cause: "deliberate_kill".to_string(),
                detected_by: "kill_rpc".to_string(),
                pane: pane(pane_id, session),
                revived_at: None,
                revived_as: None,
            })
            .unwrap();
    }

    #[test]
    fn checkpoint_appends_only_when_state_changes() {
        let dir = libsloptest::tempfile::tempdir().unwrap();
        let config = config(dir.path(), "slopd");
        let key = GenerationKey {
            tmux_boot_id: "boot".into(),
            tmux_session_id: "$1".into(),
        };
        let journal = open(dir.path(), &config, key, 1);
        journal.checkpoint(vec![pane("%1", "s1")]).unwrap();
        let path = std::fs::read_dir(journal.root().join("generations"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let before = std::fs::metadata(&path).unwrap().len();
        journal.checkpoint(vec![pane("%1", "s1")]).unwrap();
        assert_eq!(before, std::fs::metadata(path).unwrap().len());
    }

    #[test]
    fn tmux_target_namespaces_are_distinct() {
        let dir = libsloptest::tempfile::tempdir().unwrap();
        let a = config(dir.path(), "one");
        let b = config(dir.path(), "two");
        let state = dir.path().join("state");
        assert_ne!(target_root_at(&a, &state), target_root_at(&b, &state));

        let mut other_socket = config(dir.path(), "one");
        other_socket.tmux.socket = Some(dir.path().join("other.sock"));
        assert_ne!(
            target_root_at(&a, &state),
            target_root_at(&other_socket, &state)
        );
    }

    #[test]
    fn reused_pane_ids_are_disambiguated_by_generation() {
        let dir = libsloptest::tempfile::tempdir().unwrap();
        let config = config(dir.path(), "slopd");
        let first = GenerationKey {
            tmux_boot_id: "boot-a".into(),
            tmux_session_id: "$1".into(),
        };
        let journal = open(dir.path(), &config, first, 1);
        grave(&journal, "grave-a", "%7", "session-a", 10);

        let second = GenerationKey {
            tmux_boot_id: "boot-b".into(),
            tmux_session_id: "$1".into(),
        };
        journal.switch_generation(second, 2).unwrap();
        grave(&journal, "grave-b", "%7", "session-b", 20);

        let all = journal.graveyard(None, 10).unwrap();
        assert_eq!(all.len(), 2);
        assert_ne!(all[0].tmux_boot_id, all[1].tmux_boot_id);
        assert!(journal.select_grave(Some("%7"), None).is_err());
        assert_eq!(
            journal.select_grave(Some("%7"), Some(0)).unwrap().grave_id,
            "grave-b"
        );
        assert_eq!(
            journal.select_grave(Some("%7"), Some(-1)).unwrap().grave_id,
            "grave-a"
        );
    }

    #[test]
    fn restore_resolution_survives_daemon_reopen() {
        let dir = libsloptest::tempfile::tempdir().unwrap();
        let config = config(dir.path(), "slopd");
        let first = GenerationKey {
            tmux_boot_id: "boot".into(),
            tmux_session_id: "$9".into(),
        };
        let journal = open(dir.path(), &config, first.clone(), 1);
        journal.checkpoint(vec![pane("%4", "session")]).unwrap();
        let second = GenerationKey {
            tmux_boot_id: "boot".into(),
            tmux_session_id: "$10".into(),
        };
        journal.switch_generation(second.clone(), 2).unwrap();
        assert_eq!(journal.pending_restore().unwrap().unwrap().0, first);
        journal.resolve_restore(&first, "manual_restore").unwrap();
        drop(journal);

        let reopened = open(dir.path(), &config, second, 2);
        assert!(reopened.pending_restore().unwrap().is_none());
    }

    #[test]
    fn empty_new_generation_does_not_hide_older_restore_point() {
        let dir = libsloptest::tempfile::tempdir().unwrap();
        let config = config(dir.path(), "slopd");
        let first = GenerationKey {
            tmux_boot_id: "boot".into(),
            tmux_session_id: "$1".into(),
        };
        let journal = open(dir.path(), &config, first.clone(), 1);
        journal.checkpoint(vec![pane("%4", "session")]).unwrap();
        journal
            .switch_generation(
                GenerationKey {
                    tmux_boot_id: "boot".into(),
                    tmux_session_id: "$2".into(),
                },
                2,
            )
            .unwrap();
        journal.checkpoint(Vec::new()).unwrap();
        journal
            .switch_generation(
                GenerationKey {
                    tmux_boot_id: "boot".into(),
                    tmux_session_id: "$3".into(),
                },
                3,
            )
            .unwrap();

        assert_eq!(journal.pending_restore().unwrap().unwrap().0, first);
    }

    #[test]
    fn legacy_manifest_import_becomes_a_pending_checkpoint_once() {
        let dir = libsloptest::tempfile::tempdir().unwrap();
        let config = config(dir.path(), "slopd");
        let current = GenerationKey {
            tmux_boot_id: "boot".into(),
            tmux_session_id: "$1".into(),
        };
        let journal = open(dir.path(), &config, current, 2);
        assert!(
            journal
                .import_legacy(vec![pane("%4", "session")], 1)
                .unwrap()
        );
        assert!(
            !journal
                .import_legacy(vec![pane("%4", "session")], 1)
                .unwrap()
        );

        let (source, panes) = journal.pending_restore().unwrap().unwrap();
        assert_eq!(source.tmux_boot_id, "legacy-manifest");
        assert_eq!(panes, vec![pane("%4", "session")]);
    }

    #[test]
    fn incomplete_final_json_line_is_ignored() {
        use std::io::Write;
        let dir = libsloptest::tempfile::tempdir().unwrap();
        let config = config(dir.path(), "slopd");
        let key = GenerationKey {
            tmux_boot_id: "boot".into(),
            tmux_session_id: "$1".into(),
        };
        let journal = open(dir.path(), &config, key.clone(), 1);
        journal.checkpoint(vec![pane("%4", "session")]).unwrap();
        let path = std::fs::read_dir(journal.root().join("generations"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        drop(journal);
        write!(
            std::fs::OpenOptions::new().append(true).open(path).unwrap(),
            "{{\"event\":\"pane\""
        )
        .unwrap();

        let reopened = open(dir.path(), &config, key.clone(), 1);
        assert_eq!(
            reopened.manual_restore_source().unwrap().unwrap().1,
            vec![pane("%4", "session")]
        );
        reopened
            .checkpoint(vec![pane("%4", "session"), pane("%5", "other")])
            .unwrap();
        drop(reopened);
        let reopened = open(dir.path(), &config, key, 1);
        assert_eq!(reopened.current.lock().unwrap().checkpoint.len(), 2);
    }
}
