use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::transport::Transport;
use crate::wire::{self, Sender};

const PROTOCOL_VERSION: u32 = 2;
const NATIVE_STEER_METHOD: &str = "_goose/unstable/session/steer";
const ACP_TAG: &str = "acp";
const ACP_RESUMABLE_TAG: &str = "acp-resumable";
const ACP_DELETED_TAG: &str = "acp-deleted";
const ACP_SESSION_TAG_PREFIX: &str = "acp-session-";
const ACP_CWD_TAG_PREFIX: &str = "acp-cwd-";
const ACP_OWNER_TAG_PREFIX: &str = "acp-owner-";
const GRAVEYARD_RECOVERY_LIMIT: usize = 4096;

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum SystemPromptMode {
    /// Frame the ACP system prompt above the first user message. This preserves
    /// the text but cannot preserve system-role authority.
    Prepend,
    /// Reject sessions that carry a system prompt.
    Reject,
    /// Discard the ACP system prompt.
    Ignore,
}

#[derive(Clone)]
pub struct Config {
    pub transport: Transport,
    pub account: Option<String>,
    pub backend: Option<libslop::Backend>,
    pub extra_args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub working_directory: Option<PathBuf>,
    pub system_prompt_mode: SystemPromptMode,
    pub ready_timeout: Duration,
    pub send_timeout_secs: u64,
    pub turn_timeout: Duration,
    pub max_sessions: usize,
}

pub struct Adapter {
    config: Config,
    owner_tag: String,
    sessions: Mutex<HashMap<String, Session>>,
    session_creation: Mutex<()>,
    next_activity_id: AtomicU64,
    next_turn_id: AtomicU64,
}

struct Session {
    pane_id: Option<String>,
    grave_id: Option<String>,
    backend: Option<libslop::Backend>,
    native_session_id: Option<String>,
    cwd: PathBuf,
    start_directory: PathBuf,
    system_prompt: Option<String>,
    system_prompt_delivered: bool,
    /// The next Codex turn must discard transcript history replayed while a
    /// backend-native session binds its transcript path.
    pending_resume_history: bool,
    active_turn: Option<ActiveTurn>,
    last_used: u64,
    title: Option<String>,
}

struct StartedPane {
    pane_id: String,
    backend: libslop::Backend,
    native_session_id: Option<String>,
}

struct ActiveTurn {
    id: u64,
    run_id: String,
    cancel: CancellationToken,
}

struct TurnLease {
    turn_id: u64,
    run_id: String,
    pane_id: String,
    backend: libslop::Backend,
    prompt: String,
    system_prompt_included: bool,
    discard_replayed_history: bool,
    cancel: CancellationToken,
}

struct TurnResult {
    accepted: bool,
    stop_reason: &'static str,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitializeParams {
    protocol_version: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionNewParams {
    cwd: String,
    #[serde(default)]
    mcp_servers: Vec<Value>,
    #[serde(default)]
    system_prompt: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionPromptParams {
    session_id: String,
    prompt: Vec<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionCancelParams {
    session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionResumeParams {
    session_id: String,
    cwd: String,
    #[serde(default)]
    mcp_servers: Vec<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionListParams {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeSteerParams {
    session_id: String,
    expected_run_id: String,
    prompt: Vec<Value>,
}

impl Adapter {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self {
            config,
            owner_tag: format!("{ACP_OWNER_TAG_PREFIX}{}", uuid::Uuid::new_v4()),
            sessions: Mutex::new(HashMap::new()),
            session_creation: Mutex::new(()),
            next_activity_id: AtomicU64::new(1),
            next_turn_id: AtomicU64::new(1),
        })
    }

    /// Rebuild the logical ACP session catalog from live pane tags and slopd's
    /// durable graveyard before accepting protocol requests. Live panes from
    /// the pre-persistence adapter are migrated when they already expose a
    /// backend-native session; an unprompted pane cannot be recovered safely
    /// because its pending ACP system prompt lived only in the old process.
    pub async fn recover_sessions(&self) -> Result<(), String> {
        let mut client = self.config.transport.connect().await?;
        let live_panes = client.ps().await.map_err(|error| error.to_string())?;
        let graves = client
            .graveyard(None, GRAVEYARD_RECOVERY_LIMIT)
            .await
            .map_err(|error| error.to_string())?;

        let mut recovered = HashMap::new();
        let mut seen_grave_sessions = HashSet::new();
        for grave in graves {
            if !self.pane_is_in_scope(&grave.pane) {
                continue;
            }
            let Some(session_id) = tagged_session_id(&grave.pane.tags) else {
                continue;
            };
            if !seen_grave_sessions.insert(session_id.clone()) {
                continue;
            }
            if grave.pane.tags.iter().any(|tag| tag == ACP_DELETED_TAG)
                || grave.revived_at.is_some()
            {
                continue;
            }
            if !grave.pane.tags.iter().any(|tag| tag == ACP_RESUMABLE_TAG)
                || grave.pane.session_id.is_none()
            {
                continue;
            }
            let cwd = tagged_cwd(&grave.pane.tags)
                .or_else(|| grave.pane.working_dir.as_deref().map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from("/"));
            let start_directory = self
                .config
                .working_directory
                .clone()
                .unwrap_or_else(|| cwd.clone());
            recovered.entry(session_id).or_insert_with(|| Session {
                pane_id: None,
                grave_id: Some(grave.grave_id),
                backend: None,
                native_session_id: grave.pane.session_id,
                cwd,
                start_directory,
                system_prompt: None,
                system_prompt_delivered: true,
                pending_resume_history: false,
                active_turn: None,
                last_used: grave.destroyed_at.max(grave.pane.last_active),
                title: grave.pane.pane_title,
            });
        }

        let mut live_metadata = Vec::new();
        let mut incomplete = Vec::new();
        for pane in live_panes {
            if !self.pane_is_in_scope(&pane) {
                continue;
            }
            if pane.tags.iter().any(|tag| tag == ACP_DELETED_TAG) {
                incomplete.push((pane.pane_id, pane.tags, true));
                continue;
            }
            let tagged_id = tagged_session_id(&pane.tags);
            let legacy = tagged_id.is_none();
            let session_id = tagged_id.unwrap_or_else(|| format!("slopd:{}", pane.pane_id));
            let explicitly_resumable = pane.tags.iter().any(|tag| tag == ACP_RESUMABLE_TAG);
            if (!legacy && !explicitly_resumable) || (legacy && pane.session_id.is_none()) {
                incomplete.push((pane.pane_id, pane.tags, false));
                continue;
            }
            let cwd = tagged_cwd(&pane.tags)
                .or_else(|| pane.working_dir.as_deref().map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from("/"));
            let start_directory = self
                .config
                .working_directory
                .clone()
                .unwrap_or_else(|| cwd.clone());
            live_metadata.push((
                pane.pane_id.clone(),
                session_id.clone(),
                cwd.to_string_lossy().into_owned(),
            ));
            recovered.insert(
                session_id,
                Session {
                    pane_id: Some(pane.pane_id),
                    grave_id: None,
                    backend: Some(pane.backend),
                    native_session_id: pane.session_id,
                    cwd,
                    start_directory,
                    system_prompt: None,
                    system_prompt_delivered: true,
                    pending_resume_history: false,
                    active_turn: None,
                    last_used: pane.last_active.max(pane.created_at),
                    title: pane.pane_title,
                },
            );
        }

        let next_activity = recovered
            .values()
            .map(|session| session.last_used)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        *self.sessions.lock().await = recovered;
        self.next_activity_id
            .store(next_activity.max(1), Ordering::Relaxed);

        for (pane_id, session_id, cwd) in live_metadata {
            self.tag_session_pane(&pane_id, &session_id, &cwd, true)
                .await?;
        }
        for (pane_id, tags, preserve_tombstone) in incomplete {
            tracing::warn!(
                pane_id,
                "discarding incomplete ACP pane after adapter restart"
            );
            if !preserve_tombstone {
                self.remove_session_tags(&pane_id, &tags).await;
            }
            if let Err(error) = self.kill_pane(&pane_id).await {
                tracing::warn!(pane_id, "failed to remove incomplete ACP pane: {error}");
            }
        }

        self.trim_recovered_panes().await?;
        let sessions = self.sessions.lock().await;
        let live = sessions
            .values()
            .filter(|session| session.pane_id.is_some())
            .count();
        tracing::info!(
            sessions = sessions.len(),
            live,
            "recovered durable ACP sessions"
        );
        Ok(())
    }

    fn pane_is_in_scope(&self, pane: &libslop::PaneInfo) -> bool {
        let account = self
            .config
            .account
            .as_deref()
            .unwrap_or(libslop::DEFAULT_ACCOUNT);
        pane.account == account
            && self
                .config
                .backend
                .is_none_or(|backend| pane.backend == backend)
            && pane.tags.iter().any(|tag| tag == ACP_TAG)
    }

    pub async fn dispatch(self: &Arc<Self>, message: Value, sender: &Sender) {
        match wire::classify(&message) {
            wire::Inbound::Request { id, method, params } => {
                self.handle_request(id, method, params, sender).await
            }
            wire::Inbound::Notification { method, params } => {
                if method == "session/cancel" {
                    self.cancel(params).await;
                }
            }
            wire::Inbound::Ignored => {}
            wire::Inbound::Invalid { id, code, message } => {
                wire::send(sender, wire::error(id, code, message)).await;
            }
        }
    }

    async fn handle_request(
        self: &Arc<Self>,
        id: Value,
        method: String,
        params: Value,
        sender: &Sender,
    ) {
        match method.as_str() {
            "initialize" => self.initialize(id, params, sender).await,
            "session/new" => {
                let adapter = Arc::clone(self);
                let sender = sender.clone();
                tokio::spawn(async move {
                    adapter.session_new(id, params, &sender).await;
                });
            }
            "session/resume" => {
                let adapter = Arc::clone(self);
                let sender = sender.clone();
                tokio::spawn(async move {
                    adapter.session_resume(id, params, &sender).await;
                });
            }
            "session/list" => self.session_list(id, params, sender).await,
            "session/delete" => {
                let adapter = Arc::clone(self);
                let sender = sender.clone();
                tokio::spawn(async move {
                    adapter.session_delete(id, params, &sender).await;
                });
            }
            "session/close" => {
                let adapter = Arc::clone(self);
                let sender = sender.clone();
                tokio::spawn(async move {
                    adapter.session_close(id, params, &sender).await;
                });
            }
            "session/prompt" => {
                let adapter = Arc::clone(self);
                let sender = sender.clone();
                tokio::spawn(async move {
                    adapter.session_prompt(id, params, &sender).await;
                });
            }
            NATIVE_STEER_METHOD => {
                let adapter = Arc::clone(self);
                let sender = sender.clone();
                tokio::spawn(async move {
                    adapter.native_steer(id, params, &sender).await;
                });
            }
            "session/cancel" => {
                self.cancel(params).await;
                wire::send(sender, wire::ok(id, Value::Null)).await;
            }
            _ => {
                wire::send(
                    sender,
                    wire::error(
                        id,
                        wire::METHOD_NOT_FOUND,
                        format!("jsonrpc: method not found: {method}"),
                    ),
                )
                .await;
            }
        }
    }

    async fn initialize(&self, id: Value, params: Value, sender: &Sender) {
        let params: InitializeParams = match serde_json::from_value(params) {
            Ok(params) => params,
            Err(error) => {
                wire::send(
                    sender,
                    wire::error(
                        id,
                        wire::INVALID_PARAMS,
                        format!("initialize: invalid params: {error}"),
                    ),
                )
                .await;
                return;
            }
        };
        wire::send(
            sender,
            wire::ok(
                id,
                json!({
                    "protocolVersion": params.protocol_version.min(PROTOCOL_VERSION),
                    "agentCapabilities": {
                        "loadSession": false,
                        "sessionCapabilities": {
                            "resume": {},
                            "list": {},
                            "delete": {},
                            "close": {},
                        },
                        "promptCapabilities": {
                            "image": false,
                            "audio": false,
                            "embeddedContext": false,
                        },
                        "mcpCapabilities": {
                            "http": false,
                            "sse": false,
                        },
                    },
                    "agentInfo": {
                        "name": "slopd-acp",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            ),
        )
        .await;
    }

    async fn session_new(&self, id: Value, params: Value, sender: &Sender) {
        let params: SessionNewParams = match serde_json::from_value(params) {
            Ok(params) => params,
            Err(error) => {
                return reject(sender, id, format!("session/new: invalid params: {error}")).await;
            }
        };
        if params.cwd.is_empty() || !Path::new(&params.cwd).is_absolute() {
            return reject(
                sender,
                id,
                "session/new: cwd must be an absolute path".into(),
            )
            .await;
        }
        if !params.mcp_servers.is_empty() {
            return reject(
                sender,
                id,
                "session/new: MCP servers are not supported by the slopd control protocol".into(),
            )
            .await;
        }

        let system_prompt = params
            .system_prompt
            .filter(|prompt| !prompt.trim().is_empty());
        let system_prompt = match (self.config.system_prompt_mode, system_prompt) {
            (SystemPromptMode::Reject, Some(_)) => {
                return reject(
                    sender,
                    id,
                    "session/new: systemPrompt has no backend-neutral slopd mapping; \
                     use --system-prompt-mode prepend or ignore"
                        .into(),
                )
                .await;
            }
            (SystemPromptMode::Ignore, Some(_)) => {
                tracing::warn!("discarding ACP systemPrompt because mode=ignore");
                None
            }
            (SystemPromptMode::Prepend, Some(prompt)) => {
                tracing::warn!(
                    "ACP systemPrompt will be framed into the first user prompt; \
                     the underlying CLI does not receive it with system-role authority"
                );
                Some(prompt)
            }
            (_, None) => None,
        };

        let start_directory = self
            .config
            .working_directory
            .clone()
            .unwrap_or_else(|| PathBuf::from(&params.cwd));
        let _creation = self.session_creation.lock().await;
        if let Err(error) = self.make_live_pane_room().await {
            return server_error(sender, id, format!("session/new: {error}")).await;
        }
        let started = match self.start_pane(start_directory.clone(), None).await {
            Ok(started) => started,
            Err(error) => return server_error(sender, id, error).await,
        };
        let session_id = format!("slopd:{}", uuid::Uuid::new_v4());
        if let Err(error) = self
            .tag_session_pane(&started.pane_id, &session_id, &params.cwd, false)
            .await
        {
            let _ = self.kill_pane(&started.pane_id).await;
            return server_error(
                sender,
                id,
                format!("session/new: could not persist session metadata: {error}"),
            )
            .await;
        }
        let mut sessions = self.sessions.lock().await;
        sessions.insert(
            session_id.clone(),
            Session {
                pane_id: Some(started.pane_id),
                grave_id: None,
                backend: Some(started.backend),
                native_session_id: started.native_session_id,
                cwd: PathBuf::from(&params.cwd),
                start_directory,
                system_prompt,
                system_prompt_delivered: false,
                pending_resume_history: false,
                active_turn: None,
                last_used: self.next_activity_id.fetch_add(1, Ordering::Relaxed),
                title: None,
            },
        );
        drop(sessions);

        wire::send(sender, wire::ok(id, json!({ "sessionId": session_id }))).await;
    }

    async fn session_resume(&self, id: Value, params: Value, sender: &Sender) {
        let params: SessionResumeParams = match serde_json::from_value(params) {
            Ok(params) => params,
            Err(error) => {
                return reject(
                    sender,
                    id,
                    format!("session/resume: invalid params: {error}"),
                )
                .await;
            }
        };
        if let Err(error) =
            validate_session_cwd_and_mcp("session/resume", &params.cwd, &params.mcp_servers)
        {
            return reject(sender, id, error).await;
        }

        let _creation = self.session_creation.lock().await;
        {
            let sessions = self.sessions.lock().await;
            let Some(session) = sessions.get(&params.session_id) else {
                return reject(sender, id, "session/resume: unknown session".into()).await;
            };
            if session.cwd != std::path::Path::new(&params.cwd) {
                return reject(
                    sender,
                    id,
                    format!(
                        "session/resume: cwd does not match the session's original cwd {}",
                        session.cwd.display()
                    ),
                )
                .await;
            }
        }
        if let Err(error) = self.ensure_session_resident(&params.session_id).await {
            return server_error(sender, id, format!("session/resume: {error}")).await;
        }
        if let Some(session) = self.sessions.lock().await.get_mut(&params.session_id) {
            session.last_used = self.next_activity_id.fetch_add(1, Ordering::Relaxed);
        }
        wire::send(sender, wire::ok(id, json!({}))).await;
    }

    async fn session_list(&self, id: Value, params: Value, sender: &Sender) {
        let params: SessionListParams = match serde_json::from_value(params) {
            Ok(params) => params,
            Err(error) => {
                return reject(sender, id, format!("session/list: invalid params: {error}")).await;
            }
        };
        if let Some(cwd) = params.cwd.as_deref()
            && (cwd.is_empty() || !Path::new(cwd).is_absolute())
        {
            return reject(
                sender,
                id,
                "session/list: cwd must be an absolute path".into(),
            )
            .await;
        }
        if params.cursor.is_some() {
            return reject(
                sender,
                id,
                "session/list: cursor is invalid because the previous response had no nextCursor"
                    .into(),
            )
            .await;
        }

        let sessions = self.sessions.lock().await;
        let mut entries = sessions.iter().collect::<Vec<_>>();
        entries.sort_by_key(|(_, session)| std::cmp::Reverse(session.last_used));
        let sessions = entries
            .into_iter()
            .filter(|(_, session)| {
                params
                    .cwd
                    .as_deref()
                    .is_none_or(|cwd| session.cwd == std::path::Path::new(cwd))
            })
            .map(|(session_id, session)| {
                let mut entry = json!({
                    "sessionId": session_id,
                    "cwd": session.cwd,
                });
                if let Some(title) = session.title.as_ref() {
                    entry["title"] = Value::String(title.clone());
                }
                entry
            })
            .collect::<Vec<_>>();
        wire::send(sender, wire::ok(id, json!({ "sessions": sessions }))).await;
    }

    async fn session_delete(&self, id: Value, params: Value, sender: &Sender) {
        let params: SessionCancelParams = match serde_json::from_value(params) {
            Ok(params) => params,
            Err(error) => {
                return reject(
                    sender,
                    id,
                    format!("session/delete: invalid params: {error}"),
                )
                .await;
            }
        };
        let _creation = self.session_creation.lock().await;
        if !self.sessions.lock().await.contains_key(&params.session_id) {
            // ACP specifies idempotent deletion for unknown/already-deleted IDs.
            return wire::send(sender, wire::ok(id, json!({}))).await;
        }
        if let Some(cancel) = self
            .sessions
            .lock()
            .await
            .get(&params.session_id)
            .and_then(|session| session.active_turn.as_ref())
            .map(|active| active.cancel.clone())
        {
            cancel.cancel();
        }
        if let Err(error) = self.ensure_session_resident(&params.session_id).await {
            return server_error(sender, id, format!("session/delete: {error}")).await;
        }
        let pane_id = self
            .sessions
            .lock()
            .await
            .get(&params.session_id)
            .and_then(|session| session.pane_id.clone())
            .expect("resident deleted session must have a pane");
        let mut client = match self.config.transport.connect().await {
            Ok(client) => client,
            Err(error) => return server_error(sender, id, error).await,
        };
        if let Err(error) = client
            .tag(pane_id.clone(), ACP_DELETED_TAG.to_string())
            .await
        {
            return server_error(
                sender,
                id,
                format!("session/delete: could not persist deletion tombstone: {error}"),
            )
            .await;
        }
        drop(client);
        if let Err(error) = self.kill_pane(&pane_id).await {
            return server_error(sender, id, format!("session/delete: {error}")).await;
        }
        self.sessions.lock().await.remove(&params.session_id);
        wire::send(sender, wire::ok(id, json!({}))).await;
    }

    async fn session_close(&self, id: Value, params: Value, sender: &Sender) {
        let params: SessionCancelParams = match serde_json::from_value(params) {
            Ok(params) => params,
            Err(error) => {
                return reject(
                    sender,
                    id,
                    format!("session/close: invalid params: {error}"),
                )
                .await;
            }
        };
        let _creation = self.session_creation.lock().await;
        let (pane_id, cancel) = {
            let sessions = self.sessions.lock().await;
            let Some(session) = sessions.get(&params.session_id) else {
                return reject(sender, id, "session/close: unknown session".into()).await;
            };
            (
                session.pane_id.clone(),
                session
                    .active_turn
                    .as_ref()
                    .map(|active| active.cancel.clone()),
            )
        };
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
        if let Some(pane_id) = pane_id {
            if let Err(error) = self
                .capture_native_session(&params.session_id, &pane_id)
                .await
            {
                tracing::warn!(
                    pane_id,
                    "could not refresh session metadata before close: {error}"
                );
            }
            if let Err(error) = self.kill_pane(&pane_id).await {
                return server_error(sender, id, format!("session/close: {error}")).await;
            }
            self.mark_pane_gone(&params.session_id, &pane_id, None)
                .await;
            self.capture_latest_grave(&params.session_id).await;
        }
        wire::send(sender, wire::ok(id, json!({}))).await;
    }

    async fn session_prompt(&self, id: Value, params: Value, sender: &Sender) {
        let params: SessionPromptParams = match serde_json::from_value(params) {
            Ok(params) => params,
            Err(error) => {
                return reject(
                    sender,
                    id,
                    format!("session/prompt: invalid params: {error}"),
                )
                .await;
            }
        };
        let user_prompt = match prompt_text("session/prompt", &params.prompt) {
            Ok(prompt) if !prompt.is_empty() => prompt,
            Ok(_) => {
                return reject(sender, id, "session/prompt: prompt is empty".into()).await;
            }
            Err(error) => return reject(sender, id, error).await,
        };

        let turn_id = self.next_turn_id.fetch_add(1, Ordering::Relaxed);
        let run_id = format!("slopd-turn-{turn_id}");
        let creation = self.session_creation.lock().await;
        if !self.sessions.lock().await.contains_key(&params.session_id) {
            return reject(sender, id, "session/prompt: unknown session".into()).await;
        }
        if let Err(error) = self.ensure_session_resident(&params.session_id).await {
            return server_error(sender, id, format!("session/prompt: {error}")).await;
        }
        let lease = {
            let mut sessions = self.sessions.lock().await;
            let Some(session) = sessions.get_mut(&params.session_id) else {
                return reject(sender, id, "session/prompt: unknown session".into()).await;
            };
            if session.active_turn.is_some() {
                return reject(
                    sender,
                    id,
                    "session/prompt: prompt already in flight".into(),
                )
                .await;
            }
            let pane_id = session
                .pane_id
                .clone()
                .expect("resident session must have a pane");
            let backend = session
                .backend
                .expect("resident session must have a backend");

            let system_prompt = (!session.system_prompt_delivered)
                .then_some(session.system_prompt.as_deref())
                .flatten();
            let prompt = frame_first_prompt(system_prompt, &user_prompt);
            let discard_replayed_history = std::mem::take(&mut session.pending_resume_history);
            let cancel = CancellationToken::new();
            session.active_turn = Some(ActiveTurn {
                id: turn_id,
                run_id: run_id.clone(),
                cancel: cancel.clone(),
            });
            session.last_used = self.next_activity_id.fetch_add(1, Ordering::Relaxed);
            TurnLease {
                turn_id,
                run_id,
                pane_id,
                backend,
                prompt,
                system_prompt_included: system_prompt.is_some(),
                discard_replayed_history,
                cancel,
            }
        };
        drop(creation);

        let accepted = Arc::new(AtomicBool::new(false));
        let run = self.run_turn(&params.session_id, &lease, Arc::clone(&accepted), sender);
        let result = match tokio::time::timeout(self.config.turn_timeout, run).await {
            Ok(result) => result,
            Err(_) => {
                if let Err(error) = self.interrupt_pane(&lease.pane_id).await {
                    tracing::warn!("failed to interrupt timed-out pane: {error}");
                }
                Err(format!(
                    "turn timed out after {} seconds",
                    self.config.turn_timeout.as_secs()
                ))
            }
        };

        let cleared_active_turn = {
            let mut sessions = self.sessions.lock().await;
            if let Some(session) = sessions.get_mut(&params.session_id)
                && session
                    .active_turn
                    .as_ref()
                    .is_some_and(|active| active.id == lease.turn_id)
            {
                session.active_turn = None;
                let was_accepted = result
                    .as_ref()
                    .map(|result| result.accepted)
                    .unwrap_or_else(|_| accepted.load(Ordering::Acquire));
                if lease.system_prompt_included && was_accepted {
                    session.system_prompt_delivered = true;
                }
                if lease.discard_replayed_history && !was_accepted {
                    session.pending_resume_history = true;
                }
                true
            } else {
                false
            }
        };
        if cleared_active_turn {
            wire::send(
                sender,
                wire::session_update(&params.session_id, active_run_update(None)),
            )
            .await;
        }

        match result {
            Ok(result) => {
                wire::send(
                    sender,
                    wire::ok(id, json!({ "stopReason": result.stop_reason })),
                )
                .await;
            }
            Err(error) => server_error(sender, id, error).await,
        }
    }

    async fn run_turn(
        &self,
        session_id: &str,
        lease: &TurnLease,
        accepted: Arc<AtomicBool>,
        sender: &Sender,
    ) -> Result<TurnResult, String> {
        let mut client = tokio::select! {
            result = self.config.transport.connect() => result?,
            _ = lease.cancel.cancelled() => {
                return Ok(TurnResult { accepted: false, stop_reason: "cancelled" });
            }
        };
        let mut state = tokio::select! {
            result = client.subscribe(vec![
                pane_event_filter(&lease.pane_id, "slopd", "StateChange"),
                pane_event_filter(&lease.pane_id, "slopd", "DetailedStateChange"),
                pane_event_filter(&lease.pane_id, "slopd", "PaneDestroyed"),
                pane_event_filter(&lease.pane_id, "hook", "Stop"),
                pane_event_filter(&lease.pane_id, "hook", "SessionEnd"),
            ]) => result.map_err(|error| error.to_string())?,
            _ = lease.cancel.cancelled() => {
                return Ok(TurnResult { accepted: false, stop_reason: "cancelled" });
            }
        };
        let mut transcript = tokio::select! {
            result = client.subscribe_transcript(lease.pane_id.clone(), 0) => {
                result.map_err(|error| error.to_string())?
            }
            _ = lease.cancel.cancelled() => {
                return Ok(TurnResult { accepted: false, stop_reason: "cancelled" });
            }
        };

        let send = client.send_prompt(
            lease.pane_id.clone(),
            lease.prompt.clone(),
            self.config.send_timeout_secs,
            false,
        );
        tokio::select! {
            result = send => result.map_err(|error| error.to_string())?,
            _ = lease.cancel.cancelled() => {
                if let Err(error) = self.interrupt_pane(&lease.pane_id).await {
                    tracing::warn!("failed to interrupt cancelled pane: {error}");
                }
                return Ok(TurnResult { accepted: false, stop_reason: "cancelled" });
            }
        };
        accepted.store(true, Ordering::Release);
        self.mark_session_resumable(session_id, &lease.pane_id)
            .await;
        wire::send(
            sender,
            wire::session_update(session_id, active_run_update(Some(&lease.run_id))),
        )
        .await;

        let mut projection = Projection::default();
        let mut transcript_gate =
            TurnTranscriptGate::new(lease.backend, &lease.prompt, lease.discard_replayed_history);
        let mut saw_busy = false;
        let mut saw_answer = false;
        loop {
            tokio::select! {
                item = transcript.next() => {
                    match subscription_event(item)? {
                        SubscriptionEvent::Record(record) => {
                            tracing::debug!(
                                pane_id = lease.pane_id,
                                event_type = record.event_type,
                                cursor = record.cursor,
                                "received transcript record"
                            );
                            if transcript_gate.accepts(&record) {
                                saw_answer |= emit_record_updates(
                                    &mut projection,
                                    lease.backend,
                                    &record,
                                    session_id,
                                    sender,
                                ).await;
                            }
                        }
                        SubscriptionEvent::Subscribed => continue,
                        SubscriptionEvent::Closed => {
                            return Err("transcript subscription closed during turn".into());
                        }
                    }
                }
                item = state.next() => {
                    let record = match subscription_event(item)? {
                        SubscriptionEvent::Record(record) => record,
                        SubscriptionEvent::Subscribed => continue,
                        SubscriptionEvent::Closed => {
                            return Err("state subscription closed during turn".into());
                        }
                    };
                    tracing::debug!(
                        pane_id = lease.pane_id,
                        event_type = record.event_type,
                        payload = %record.payload,
                        "received state record"
                    );
                    match record.event_type.as_str() {
                        "PaneDestroyed" | "SessionEnd" => {
                            self.mark_pane_gone(
                                session_id,
                                &lease.pane_id,
                                record.payload.get("session_id").and_then(Value::as_str),
                            )
                            .await;
                            return Err(format!(
                                "underlying pane {} ended during the turn",
                                lease.pane_id
                            ));
                        }
                        "Stop" => {
                            return complete_turn(
                                &mut transcript,
                                &mut projection,
                                &mut transcript_gate,
                                lease.backend,
                                session_id,
                                sender,
                            ).await;
                        }
                        "StateChange" => {
                            match record.payload.get("state").and_then(Value::as_str) {
                                Some("busy") => saw_busy = true,
                                Some("ready")
                                    if state_ready_completes(
                                        lease.backend,
                                        saw_busy,
                                        saw_answer,
                                    ) =>
                                {
                                    // State and transcript records travel through
                                    // independent slopd subscriptions. A fast
                                    // turn can deliver ready first even though
                                    // its assistant record is already queued (or
                                    // about to be tailed). Give transcript
                                    // delivery one short quiet window before
                                    // resolving the ACP prompt.
                                    return complete_turn(
                                        &mut transcript,
                                        &mut projection,
                                        &mut transcript_gate,
                                        lease.backend,
                                        session_id,
                                        sender,
                                    ).await;
                                }
                                _ => {}
                            }
                        }
                        "DetailedStateChange" => {
                            match record
                                .payload
                                .get("detailed_state")
                                .and_then(Value::as_str)
                            {
                                Some("busy_processing" | "busy_tool_use" | "busy_subagent" | "busy_compacting") => {
                                    saw_busy = true;
                                }
                                Some("awaiting_input_permission") => {
                                    wire::send(
                                        sender,
                                        wire::session_update(
                                            session_id,
                                            message_chunk(
                                                "\n\n[slopd-acp: the underlying agent is awaiting permission in its terminal pane.]\n",
                                            ),
                                        ),
                                    )
                                    .await;
                                }
                                Some("awaiting_input_elicitation") => {
                                    wire::send(
                                        sender,
                                        wire::session_update(
                                            session_id,
                                            message_chunk(
                                                "\n\n[slopd-acp: the underlying agent is awaiting input in its terminal pane.]\n",
                                            ),
                                        ),
                                    )
                                    .await;
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
                _ = lease.cancel.cancelled() => {
                    if let Err(error) = self.interrupt_pane(&lease.pane_id).await {
                        tracing::warn!("failed to interrupt cancelled pane: {error}");
                    }
                    return Ok(TurnResult {
                        accepted: true,
                        stop_reason: "cancelled",
                    });
                }
            }
        }
    }

    async fn start_pane(
        &self,
        start_directory: PathBuf,
        native_session_id: Option<&str>,
    ) -> Result<StartedPane, String> {
        let mut client = self.config.transport.connect().await?;
        let filters = vec![
            event_filter("slopd", "DetailedStateChange"),
            event_filter("slopd", "PaneDestroyed"),
            event_filter("hook", "SessionEnd"),
        ];
        let mut subscription = client
            .subscribe(filters)
            .await
            .map_err(|error| error.to_string())?;
        let pane_id = client
            .run(
                None,
                launch_args(&self.config.extra_args, native_session_id),
                Some(start_directory),
                self.config.env.clone(),
                self.config.account.clone(),
                self.config.backend,
            )
            .await
            .map_err(|error| error.to_string())?;

        if let Err(error) = wait_until_live(
            &self.config.transport,
            &mut subscription,
            &pane_id,
            self.config.ready_timeout,
        )
        .await
        {
            drop(subscription);
            drop(client);
            if let Err(cleanup_error) = self.kill_pane(&pane_id).await {
                tracing::warn!("failed to remove pane after startup error: {cleanup_error}");
            }
            return Err(error);
        }

        drop(subscription);
        drop(client);
        let mut client = match self.config.transport.connect().await {
            Ok(client) => client,
            Err(error) => {
                if let Err(cleanup_error) = self.kill_pane(&pane_id).await {
                    tracing::warn!(
                        "failed to remove pane after metadata connection error: {cleanup_error}"
                    );
                }
                return Err(error);
            }
        };
        let (backend, native_session_id) = match client.ps().await {
            Ok(panes) => match panes.into_iter().find(|pane| pane.pane_id == pane_id) {
                Some(pane) => (pane.backend, pane.session_id),
                None => {
                    if let Err(cleanup_error) = self.kill_pane(&pane_id).await {
                        tracing::warn!(
                            "failed to remove pane after it disappeared from ps: {cleanup_error}"
                        );
                    }
                    return Err(format!(
                        "pane {pane_id} disappeared while creating its ACP session"
                    ));
                }
            },
            Err(error) => {
                if let Err(cleanup_error) = self.kill_pane(&pane_id).await {
                    tracing::warn!(
                        "failed to remove pane after backend lookup error: {cleanup_error}"
                    );
                }
                return Err(format!("could not resolve backend for {pane_id}: {error}"));
            }
        };
        if let Err(error) = client.tag(pane_id.clone(), ACP_TAG.into()).await {
            tracing::warn!("could not tag ACP pane {pane_id}: {error}");
        }
        Ok(StartedPane {
            pane_id,
            backend,
            native_session_id,
        })
    }

    async fn revive_pane(&self, grave_id: &str) -> Result<StartedPane, String> {
        let mut client = self.config.transport.connect().await?;
        let (pane_id, _) = client
            .revive(Some(grave_id.to_string()), None)
            .await
            .map_err(|error| error.to_string())?;
        drop(client);

        let deadline = tokio::time::Instant::now() + self.config.ready_timeout;
        loop {
            let mut client = self.config.transport.connect().await?;
            let panes = client.ps().await.map_err(|error| error.to_string())?;
            if let Some(pane) = panes.into_iter().find(|pane| pane.pane_id == pane_id)
                && pane.detailed_state != libslop::PaneDetailedState::BootingUp
            {
                return Ok(StartedPane {
                    pane_id,
                    backend: pane.backend,
                    native_session_id: pane.session_id,
                });
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "timed out after {} seconds waiting for revived pane {pane_id}",
                    self.config.ready_timeout.as_secs()
                ));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn tag_session_pane(
        &self,
        pane_id: &str,
        session_id: &str,
        cwd: &str,
        resumable: bool,
    ) -> Result<(), String> {
        let mut client = self.config.transport.connect().await?;
        let existing = client
            .tags(pane_id.to_string())
            .await
            .map_err(|error| error.to_string())?;
        for tag in existing
            .iter()
            .filter(|tag| tag.starts_with(ACP_OWNER_TAG_PREFIX) && **tag != self.owner_tag)
        {
            client
                .untag(pane_id.to_string(), tag.clone())
                .await
                .map_err(|error| error.to_string())?;
        }
        for tag in [
            ACP_TAG.to_string(),
            session_tag(session_id),
            cwd_tag(cwd),
            self.owner_tag.clone(),
        ] {
            client
                .tag(pane_id.to_string(), tag)
                .await
                .map_err(|error| error.to_string())?;
        }
        if resumable {
            client
                .tag(pane_id.to_string(), ACP_RESUMABLE_TAG.to_string())
                .await
                .map_err(|error| error.to_string())?;
        } else if existing.iter().any(|tag| tag == ACP_RESUMABLE_TAG) {
            client
                .untag(pane_id.to_string(), ACP_RESUMABLE_TAG.to_string())
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    async fn remove_session_tags(&self, pane_id: &str, tags: &[String]) {
        let Ok(mut client) = self.config.transport.connect().await else {
            return;
        };
        for tag in tags.iter().filter(|tag| {
            *tag == ACP_TAG
                || *tag == ACP_RESUMABLE_TAG
                || tag.starts_with(ACP_SESSION_TAG_PREFIX)
                || tag.starts_with(ACP_CWD_TAG_PREFIX)
                || tag.starts_with(ACP_OWNER_TAG_PREFIX)
        }) {
            if let Err(error) = client.untag(pane_id.to_string(), tag.clone()).await {
                tracing::warn!(pane_id, tag, "failed to remove abandoned ACP tag: {error}");
            }
        }
    }

    async fn mark_session_resumable(&self, session_id: &str, pane_id: &str) {
        if let Some(session) = self.sessions.lock().await.get_mut(session_id) {
            session.system_prompt_delivered = true;
        }
        let mut client = match self.config.transport.connect().await {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!(pane_id, "could not persist resumable ACP state: {error}");
                return;
            }
        };
        if let Err(error) = client
            .tag(pane_id.to_string(), ACP_RESUMABLE_TAG.to_string())
            .await
        {
            tracing::warn!(pane_id, "could not persist resumable ACP state: {error}");
        }
        drop(client);
        if let Err(error) = self.capture_native_session(session_id, pane_id).await {
            tracing::debug!(
                pane_id,
                "native session metadata is not available yet: {error}"
            );
        }
    }

    async fn capture_native_session(&self, session_id: &str, pane_id: &str) -> Result<(), String> {
        let mut client = self.config.transport.connect().await?;
        let pane = client
            .ps()
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|pane| pane.pane_id == pane_id)
            .ok_or_else(|| format!("pane {pane_id} is no longer live"))?;
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| "session disappeared".to_string())?;
        if session.pane_id.as_deref() != Some(pane_id) {
            return Err("session is attached to a different pane".into());
        }
        if pane.session_id.is_some() {
            session.native_session_id = pane.session_id;
        }
        session.title = pane.pane_title;
        Ok(())
    }

    async fn capture_latest_grave(&self, session_id: &str) {
        let mut client = match self.config.transport.connect().await {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!(
                    session_id,
                    "could not find closed session in graveyard: {error}"
                );
                return;
            }
        };
        let graves = match client.graveyard(None, GRAVEYARD_RECOVERY_LIMIT).await {
            Ok(graves) => graves,
            Err(error) => {
                tracing::warn!(
                    session_id,
                    "could not find closed session in graveyard: {error}"
                );
                return;
            }
        };
        let Some(grave) = graves.into_iter().find(|grave| {
            tagged_session_id(&grave.pane.tags).as_deref() == Some(session_id)
                && grave.revived_at.is_none()
        }) else {
            return;
        };
        if let Some(session) = self.sessions.lock().await.get_mut(session_id) {
            session.grave_id = Some(grave.grave_id);
            if grave.pane.session_id.is_some() {
                session.native_session_id = grave.pane.session_id;
            }
            session.title = grave.pane.pane_title;
            session.last_used = session.last_used.max(grave.destroyed_at);
        }
    }

    async fn ensure_session_resident(&self, session_id: &str) -> Result<(), String> {
        let (start_directory, cwd, native_session_id, grave_id) = {
            let sessions = self.sessions.lock().await;
            let session = sessions
                .get(session_id)
                .ok_or_else(|| "unknown session".to_string())?;
            if session.pane_id.is_some() {
                return Ok(());
            }
            if session.active_turn.is_some() {
                return Err("session is still releasing its previous pane".into());
            }
            (
                session.start_directory.clone(),
                session.cwd.to_string_lossy().into_owned(),
                session.native_session_id.clone(),
                session.grave_id.clone(),
            )
        };

        self.make_live_pane_room().await?;
        let (started, resumed) = if let Some(grave_id) = grave_id.as_deref() {
            match self.revive_pane(grave_id).await {
                Ok(started) => (started, true),
                Err(error) => {
                    tracing::warn!(
                        session_id,
                        grave_id,
                        "grave revival failed, falling back to native resume: {error}"
                    );
                    (
                        self.start_pane(start_directory, native_session_id.as_deref())
                            .await?,
                        native_session_id.is_some(),
                    )
                }
            }
        } else {
            (
                self.start_pane(start_directory, native_session_id.as_deref())
                    .await?,
                native_session_id.is_some(),
            )
        };
        if let Err(error) = self
            .tag_session_pane(&started.pane_id, session_id, &cwd, resumed)
            .await
        {
            let _ = self.kill_pane(&started.pane_id).await;
            return Err(format!(
                "could not persist restored session metadata: {error}"
            ));
        }
        let mut sessions = self.sessions.lock().await;
        let Some(session) = sessions.get_mut(session_id) else {
            drop(sessions);
            let _ = self.kill_pane(&started.pane_id).await;
            return Err("session disappeared while restoring its pane".into());
        };
        if session.pane_id.is_some() {
            drop(sessions);
            let _ = self.kill_pane(&started.pane_id).await;
            return Err("session acquired another pane while it was being restored".into());
        }
        tracing::info!(
            session_id,
            pane_id = started.pane_id,
            native_session_id = ?native_session_id,
            resumed,
            "restored evicted ACP session"
        );
        session.pane_id = Some(started.pane_id);
        session.grave_id = None;
        session.backend = Some(started.backend);
        session.native_session_id = started.native_session_id.or(native_session_id);
        session.pending_resume_history =
            resumed && session.backend == Some(libslop::Backend::Codex);
        if !resumed {
            session.system_prompt_delivered = false;
        }
        Ok(())
    }

    async fn make_live_pane_room(&self) -> Result<(), String> {
        let mut client = self.config.transport.connect().await?;
        let panes = client.ps().await.map_err(|error| error.to_string())?;
        self.reconcile_live_sessions(&panes).await;

        let live_count = self
            .sessions
            .lock()
            .await
            .values()
            .filter(|session| session.pane_id.is_some())
            .count();
        if live_count < self.config.max_sessions {
            return Ok(());
        }
        self.evict_lru_inactive().await
    }

    async fn trim_recovered_panes(&self) -> Result<(), String> {
        loop {
            let live_count = self
                .sessions
                .lock()
                .await
                .values()
                .filter(|session| session.pane_id.is_some())
                .count();
            if live_count <= self.config.max_sessions {
                return Ok(());
            }
            self.evict_lru_inactive().await?;
        }
    }

    async fn evict_lru_inactive(&self) -> Result<(), String> {
        let victim = {
            let mut sessions = self.sessions.lock().await;
            let Some(victim_id) = sessions
                .iter()
                .filter(|(_, session)| session.pane_id.is_some() && session.active_turn.is_none())
                .min_by_key(|(_, session)| session.last_used)
                .map(|(session_id, _)| session_id.clone())
            else {
                return Err(format!(
                    "maximum of {} live panes reached and every pane has an active turn",
                    self.config.max_sessions
                ));
            };
            let victim = sessions
                .get_mut(&victim_id)
                .expect("selected eviction victim must still exist");
            let pane_id = victim
                .pane_id
                .take()
                .expect("selected eviction victim must have a pane");
            let backend = victim.backend.take();
            (
                victim_id,
                pane_id,
                backend,
                victim.native_session_id.clone(),
            )
        };

        tracing::info!(
            session_id = victim.0,
            pane_id = victim.1,
            native_session_id = ?victim.3,
            "evicting least-recently-used inactive ACP pane"
        );
        if let Err(error) = self.kill_pane(&victim.1).await {
            let pane_is_gone = match self.config.transport.connect().await {
                Ok(mut client) => client
                    .ps()
                    .await
                    .is_ok_and(|panes| panes.iter().all(|pane| pane.pane_id != victim.1)),
                Err(_) => false,
            };
            if !pane_is_gone {
                let mut sessions = self.sessions.lock().await;
                if let Some(session) = sessions.get_mut(&victim.0)
                    && session.pane_id.is_none()
                {
                    session.pane_id = Some(victim.1);
                    session.backend = victim.2;
                }
                return Err(format!("failed to evict oldest pane: {error}"));
            }
        }
        self.capture_latest_grave(&victim.0).await;
        Ok(())
    }

    async fn reconcile_live_sessions(&self, panes: &[libslop::PaneInfo]) {
        let live_panes: HashMap<&str, &libslop::PaneInfo> = panes
            .iter()
            .map(|pane| (pane.pane_id.as_str(), pane))
            .collect();
        let mut sessions = self.sessions.lock().await;
        for (session_id, session) in sessions.iter_mut() {
            let Some(pane_id) = session.pane_id.clone() else {
                continue;
            };
            if let Some(pane) = live_panes.get(pane_id.as_str()) {
                if let Some(native_session_id) = pane.session_id.as_ref() {
                    session.native_session_id = Some(native_session_id.clone());
                }
                continue;
            }
            tracing::info!(
                session_id,
                pane_id,
                "pruning dead ACP pane from resident session"
            );
            session.pane_id = None;
            session.backend = None;
            if session.native_session_id.is_none() {
                session.system_prompt_delivered = false;
            }
            if let Some(active) = session.active_turn.as_ref() {
                active.cancel.cancel();
            }
        }
    }

    async fn mark_pane_gone(
        &self,
        session_id: &str,
        pane_id: &str,
        native_session_id: Option<&str>,
    ) {
        let mut sessions = self.sessions.lock().await;
        let Some(session) = sessions.get_mut(session_id) else {
            return;
        };
        if session.pane_id.as_deref() != Some(pane_id) {
            return;
        }
        if let Some(native_session_id) = native_session_id {
            session.native_session_id = Some(native_session_id.to_string());
        }
        session.pane_id = None;
        session.backend = None;
        if session.native_session_id.is_none() {
            session.system_prompt_delivered = false;
        }
    }

    async fn native_steer(&self, id: Value, params: Value, sender: &Sender) {
        let params: NativeSteerParams = match serde_json::from_value(params) {
            Ok(params) => params,
            Err(error) => {
                return reject(
                    sender,
                    id,
                    format!("{NATIVE_STEER_METHOD}: invalid params: {error}"),
                )
                .await;
            }
        };
        let prompt = match prompt_text(NATIVE_STEER_METHOD, &params.prompt) {
            Ok(prompt) if !prompt.is_empty() => prompt,
            Ok(_) => {
                return reject(
                    sender,
                    id,
                    format!("{NATIVE_STEER_METHOD}: prompt is empty"),
                )
                .await;
            }
            Err(error) => return reject(sender, id, error).await,
        };

        let pane_id = {
            let mut sessions = self.sessions.lock().await;
            let Some(session) = sessions.get_mut(&params.session_id) else {
                return reject(
                    sender,
                    id,
                    format!("{NATIVE_STEER_METHOD}: unknown session"),
                )
                .await;
            };
            let Some(active_turn) = session.active_turn.as_ref() else {
                return reject(
                    sender,
                    id,
                    format!("{NATIVE_STEER_METHOD}: no prompt is in flight"),
                )
                .await;
            };
            if active_turn.run_id != params.expected_run_id {
                return reject(
                    sender,
                    id,
                    format!("{NATIVE_STEER_METHOD}: expectedRunId does not match the active turn"),
                )
                .await;
            }
            let Some(pane_id) = session.pane_id.clone() else {
                return reject(
                    sender,
                    id,
                    format!("{NATIVE_STEER_METHOD}: active pane is no longer live"),
                )
                .await;
            };
            session.last_used = self.next_activity_id.fetch_add(1, Ordering::Relaxed);
            pane_id
        };

        let mut client = match self.config.transport.connect().await {
            Ok(client) => client,
            Err(error) => return server_error(sender, id, error).await,
        };
        match client
            .send_prompt(pane_id, prompt, self.config.send_timeout_secs, false)
            .await
        {
            Ok(_) => wire::send(sender, wire::ok(id, Value::Null)).await,
            Err(error) => server_error(sender, id, error.to_string()).await,
        }
    }

    pub async fn shutdown(&self) {
        let _creation = self.session_creation.lock().await;
        let panes = {
            let mut sessions = self.sessions.lock().await;
            let sessions = std::mem::take(&mut *sessions);
            sessions
                .into_values()
                .filter_map(|session| match session.pane_id {
                    Some(pane_id) => {
                        let active = session.active_turn.map(|active| {
                            active.cancel.cancel();
                            true
                        });
                        Some((pane_id, active.unwrap_or(false)))
                    }
                    None => None,
                })
                .collect::<Vec<_>>()
        };
        for (pane_id, was_active) in panes {
            if was_active && let Err(error) = self.interrupt_pane(&pane_id).await {
                tracing::warn!(
                    pane_id,
                    "failed to interrupt ACP pane during shutdown: {error}"
                );
            }
            let Ok(mut client) = self.config.transport.connect().await else {
                continue;
            };
            if let Err(error) = client.untag(pane_id.clone(), self.owner_tag.clone()).await {
                tracing::warn!(
                    pane_id,
                    "failed to detach ACP pane owner during shutdown: {error}"
                );
            }
        }
    }

    async fn interrupt_pane(&self, pane_id: &str) -> Result<(), String> {
        let interrupt = async {
            let mut client = self.config.transport.connect().await?;
            client
                .interrupt(pane_id.to_string())
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        };
        tokio::time::timeout(Duration::from_secs(4), interrupt)
            .await
            .map_err(|_| "timed out interrupting pane".to_string())?
    }

    async fn kill_pane(&self, pane_id: &str) -> Result<(), String> {
        let kill = async {
            let mut client = self.config.transport.connect().await?;
            client
                .kill(pane_id.to_string())
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        };
        tokio::time::timeout(Duration::from_secs(4), kill)
            .await
            .map_err(|_| "timed out removing pane".to_string())?
    }

    async fn cancel(&self, params: Value) {
        let params: SessionCancelParams = match serde_json::from_value(params) {
            Ok(params) => params,
            Err(error) => {
                tracing::warn!("session/cancel: invalid params: {error}");
                return;
            }
        };
        let cancel = {
            let sessions = self.sessions.lock().await;
            let Some(session) = sessions.get(&params.session_id) else {
                return;
            };
            session
                .active_turn
                .as_ref()
                .map(|active| active.cancel.clone())
        };
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
    }
}

fn state_ready_completes(backend: libslop::Backend, saw_busy: bool, saw_answer: bool) -> bool {
    // Codex emits a stale busy -> ready pair when its first SessionStart hook
    // races the initial prompt. A Codex ready state is only sufficient after
    // transcript output; otherwise its per-turn Stop hook is authoritative.
    if backend == libslop::Backend::Codex {
        saw_answer
    } else {
        saw_busy || saw_answer
    }
}

/// Discard Codex history that a newly resumed pane can replay before its
/// current prompt appears. Fresh and resumed Codex panes learn their transcript
/// path from SessionStart, which fires only after submission; a subscription
/// established before `send` therefore cannot snapshot the old file boundary.
/// The echoed user message is the unambiguous boundary for this turn.
struct TurnTranscriptGate<'a> {
    prompt: Option<&'a str>,
    saw_boundary: bool,
    buffered: Vec<libslop::Record>,
}

impl<'a> TurnTranscriptGate<'a> {
    fn new(backend: libslop::Backend, prompt: &'a str, discard_replayed_history: bool) -> Self {
        let gated = backend == libslop::Backend::Codex && discard_replayed_history;
        Self {
            prompt: gated.then_some(prompt),
            saw_boundary: false,
            buffered: Vec::new(),
        }
    }

    fn accepts(&mut self, record: &libslop::Record) -> bool {
        let Some(prompt) = self.prompt else {
            return true;
        };
        if record.event_type == "userMessage"
            && record.payload.get("text").and_then(Value::as_str) == Some(prompt)
        {
            // A repeated prompt can also occur in the replayed history. Keep
            // only records after the last matching user echo; Stop provides
            // the authoritative point at which that boundary is final.
            self.saw_boundary = true;
            self.buffered.clear();
        } else if self.saw_boundary {
            self.buffered.push(record.clone());
        }
        false
    }

    fn take_buffered(&mut self) -> Vec<libslop::Record> {
        std::mem::take(&mut self.buffered)
    }
}

async fn complete_turn(
    transcript: &mut libslopctl::Subscription,
    projection: &mut Projection,
    transcript_gate: &mut TurnTranscriptGate<'_>,
    backend: libslop::Backend,
    session_id: &str,
    sender: &Sender,
) -> Result<TurnResult, String> {
    drain_transcript(
        transcript,
        projection,
        transcript_gate,
        backend,
        session_id,
        sender,
    )
    .await?;
    for record in transcript_gate.take_buffered() {
        emit_record_updates(projection, backend, &record, session_id, sender).await;
    }
    for update in projection.finish_open_tools() {
        wire::send(sender, wire::session_update(session_id, update)).await;
    }
    Ok(TurnResult {
        accepted: true,
        stop_reason: "end_turn",
    })
}

async fn emit_record_updates(
    projection: &mut Projection,
    backend: libslop::Backend,
    record: &libslop::Record,
    session_id: &str,
    sender: &Sender,
) -> bool {
    let mut saw_answer = false;
    for update in projection.updates(backend, record) {
        if update.get("sessionUpdate").and_then(Value::as_str) == Some("agent_message_chunk") {
            saw_answer = true;
        }
        wire::send(sender, wire::session_update(session_id, update)).await;
    }
    saw_answer
}

async fn drain_transcript(
    transcript: &mut libslopctl::Subscription,
    projection: &mut Projection,
    transcript_gate: &mut TurnTranscriptGate<'_>,
    backend: libslop::Backend,
    session_id: &str,
    sender: &Sender,
) -> Result<(), String> {
    tracing::debug!("draining transcript after ready state");
    loop {
        match tokio::time::timeout(Duration::from_millis(300), transcript.next()).await {
            Err(_) => {
                tracing::debug!("transcript drain quiet window elapsed");
                return Ok(());
            }
            Ok(item) => {
                match subscription_event(item)? {
                    SubscriptionEvent::Record(record) => {
                        tracing::debug!(
                            event_type = record.event_type,
                            cursor = record.cursor,
                            "drained transcript record"
                        );
                        if transcript_gate.accepts(&record) {
                            emit_record_updates(projection, backend, &record, session_id, sender)
                                .await;
                        }
                    }
                    // A subscription acknowledgement is not an EOF marker. Keep
                    // the quiet-window drain open for the transcript record that
                    // can race just behind the ready state transition.
                    SubscriptionEvent::Subscribed => continue,
                    SubscriptionEvent::Closed => return Ok(()),
                }
            }
        }
    }
}

fn event_filter(source: &str, event_type: &str) -> libslop::EventFilter {
    libslop::EventFilter {
        source: Some(source.into()),
        event_type: Some(event_type.into()),
        ..Default::default()
    }
}

fn pane_event_filter(pane_id: &str, source: &str, event_type: &str) -> libslop::EventFilter {
    libslop::EventFilter {
        pane_id: Some(pane_id.into()),
        ..event_filter(source, event_type)
    }
}

async fn wait_until_live(
    transport: &Transport,
    subscription: &mut libslopctl::Subscription,
    pane_id: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "timed out after {} seconds waiting for pane {pane_id} to start",
                timeout.as_secs()
            ));
        }

        match tokio::time::timeout(
            remaining.min(Duration::from_millis(200)),
            subscription.next(),
        )
        .await
        {
            Ok(Ok(Some(libslopctl::SubscriptionItem::Record(record))))
                if record.pane_id.as_deref() == Some(pane_id) =>
            {
                match (record.source.as_str(), record.event_type.as_str()) {
                    ("slopd", "PaneDestroyed") | ("hook", "SessionEnd") => {
                        return Err(format!("pane {pane_id} exited while starting"));
                    }
                    ("slopd", "DetailedStateChange")
                        if record
                            .payload
                            .get("detailed_state")
                            .and_then(Value::as_str)
                            .is_some_and(|state| state != "booting_up") =>
                    {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        return Ok(());
                    }
                    _ => {}
                }
            }
            Ok(Ok(Some(_))) | Err(_) => {}
            Ok(Ok(None)) => {
                return Err(format!(
                    "connection closed while waiting for pane {pane_id} to start"
                ));
            }
            Ok(Err(error)) => return Err(error.to_string()),
        }

        // The detailed-state event may be emitted before the Run response is
        // routed back to this client. The subscription normally retains it,
        // but ps is the authoritative race backstop and also catches a lagged
        // broadcast receiver.
        let poll = async {
            let mut client = transport.connect().await?;
            client.ps().await.map_err(|error| error.to_string())
        };
        match tokio::time::timeout(Duration::from_secs(1), poll).await {
            Ok(Ok(panes)) => {
                if let Some(pane) = panes.iter().find(|pane| pane.pane_id == pane_id) {
                    tracing::debug!(
                        "startup backstop sees {pane_id} in {}",
                        pane.detailed_state.as_str()
                    );
                    if pane.detailed_state != libslop::PaneDetailedState::BootingUp {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        return Ok(());
                    }
                }
            }
            Ok(Err(error)) => {
                tracing::debug!("startup ps backstop failed for {pane_id}: {error}")
            }
            Err(_) => tracing::debug!("startup ps backstop timed out for {pane_id}"),
        }
    }
}

enum SubscriptionEvent {
    Record(libslop::Record),
    Subscribed,
    Closed,
}

fn subscription_event(
    item: Result<Option<libslopctl::SubscriptionItem>, libslopctl::Error>,
) -> Result<SubscriptionEvent, String> {
    match item.map_err(|error| error.to_string())? {
        Some(libslopctl::SubscriptionItem::Record(record)) => Ok(SubscriptionEvent::Record(record)),
        Some(libslopctl::SubscriptionItem::Subscribed) => Ok(SubscriptionEvent::Subscribed),
        None => Ok(SubscriptionEvent::Closed),
    }
}

fn prompt_text(method: &str, blocks: &[Value]) -> Result<String, String> {
    let mut parts = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("{method}: text content block is missing text"))?;
                parts.push(text);
            }
            Some(kind) => {
                return Err(format!(
                    "{method}: unsupported {kind:?} content block; only text is supported"
                ));
            }
            None => {
                return Err(format!("{method}: content block is missing its type"));
            }
        }
    }
    Ok(parts.join("\n"))
}

fn validate_session_cwd_and_mcp(
    method: &str,
    cwd: &str,
    mcp_servers: &[Value],
) -> Result<(), String> {
    if cwd.is_empty() || !Path::new(cwd).is_absolute() {
        return Err(format!("{method}: cwd must be an absolute path"));
    }
    if !mcp_servers.is_empty() {
        return Err(format!(
            "{method}: MCP servers are not supported by the slopd control protocol"
        ));
    }
    Ok(())
}

fn session_tag(session_id: &str) -> String {
    format!(
        "{ACP_SESSION_TAG_PREFIX}{}",
        hex_encode(session_id.as_bytes())
    )
}

fn cwd_tag(cwd: &str) -> String {
    format!("{ACP_CWD_TAG_PREFIX}{}", hex_encode(cwd.as_bytes()))
}

fn tagged_session_id(tags: &[String]) -> Option<String> {
    decoded_tag(tags, ACP_SESSION_TAG_PREFIX)
}

fn tagged_cwd(tags: &[String]) -> Option<PathBuf> {
    decoded_tag(tags, ACP_CWD_TAG_PREFIX).map(PathBuf::from)
}

fn decoded_tag(tags: &[String], prefix: &str) -> Option<String> {
    tags.iter()
        .find_map(|tag| tag.strip_prefix(prefix))
        .and_then(hex_decode)
        .and_then(|bytes| String::from_utf8(bytes).ok())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0xf) as usize] as char);
    }
    encoded
}

fn hex_decode(encoded: &str) -> Option<Vec<u8>> {
    if !encoded.len().is_multiple_of(2) {
        return None;
    }
    encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)? as u8;
            let low = (pair[1] as char).to_digit(16)? as u8;
            Some((high << 4) | low)
        })
        .collect()
}

fn launch_args(base: &[String], native_session_id: Option<&str>) -> Vec<String> {
    let Some(native_session_id) = native_session_id else {
        return base.to_vec();
    };

    // A restored ACP session owns its backend-native conversation identity.
    // Remove any static resume selector before adding the captured one so
    // Claude does not see duplicate flags and slopd's backend normalization
    // cannot accidentally select an older value.
    let mut filtered = Vec::with_capacity(base.len() + 2);
    let mut args = base.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--resume" | "-s" | "--session" => {
                let _ = args.next();
            }
            value
                if value.starts_with("--resume=")
                    || value.starts_with("--session=")
                    || value.starts_with("-s=") => {}
            _ => filtered.push(arg.clone()),
        }
    }
    filtered.splice(
        0..0,
        ["--resume".to_string(), native_session_id.to_string()],
    );
    filtered
}

fn active_run_update(run_id: Option<&str>) -> Value {
    json!({
        "sessionUpdate": "session_info_update",
        "_meta": {
            "goose": {
                "activeRunId": run_id,
            },
        },
    })
}

fn frame_first_prompt(system_prompt: Option<&str>, user_prompt: &str) -> String {
    match system_prompt {
        None => user_prompt.to_string(),
        Some(system_prompt) => format!(
            "The following instructions were supplied as this session's ACP system prompt. \
             Treat them as persistent instructions for the session.\n\n\
             --- begin ACP system prompt ---\n\
             {system_prompt}\n\
             --- end ACP system prompt ---\n\n\
             User request:\n{user_prompt}"
        ),
    }
}

fn message_chunk(text: &str) -> Value {
    json!({
        "sessionUpdate": "agent_message_chunk",
        "content": {
            "type": "text",
            "text": text,
        },
    })
}

fn thought_chunk(text: &str) -> Value {
    json!({
        "sessionUpdate": "agent_thought_chunk",
        "content": {
            "type": "text",
            "text": text,
        },
    })
}

#[derive(Default)]
struct Projection {
    open_tools: HashSet<String>,
    opencode_text: HashMap<String, String>,
    opencode_role: Option<String>,
}

impl Projection {
    fn updates(&mut self, backend: libslop::Backend, record: &libslop::Record) -> Vec<Value> {
        let mut updates = Vec::new();
        if backend == libslop::Backend::Opencode
            && matches!(record.event_type.as_str(), "user" | "assistant")
        {
            self.opencode_role = Some(record.event_type.clone());
            // OpenCode text parts are cumulative within one message. A new
            // message starts a new delta baseline, including when an older
            // server or test double omits the normally-unique part id.
            self.opencode_text.clear();
        }
        if let Some(text) = assistant_text(
            backend,
            record,
            &mut self.opencode_text,
            self.opencode_role.as_deref(),
        ) && !text.is_empty()
        {
            updates.push(message_chunk(&text));
        }
        if record.event_type == "reasoning"
            && let Some(text) = record.payload.get("text").and_then(Value::as_str)
            && !text.is_empty()
        {
            updates.push(thought_chunk(text));
        }
        self.tool_updates(record, &mut updates);
        updates
    }

    fn tool_updates(&mut self, record: &libslop::Record, updates: &mut Vec<Value>) {
        // Codex-normalized tool call.
        if let Some(name) = record.payload.get("name").and_then(Value::as_str)
            && matches!(
                record.event_type.as_str(),
                "commandExecution"
                    | "fileChange"
                    | "plan"
                    | "webSearch"
                    | "mcpToolCall"
                    | "toolCall"
            )
        {
            let id = tool_id(record, name);
            if self.open_tools.insert(id.clone()) {
                updates.push(json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": id,
                    "title": name,
                    "kind": "other",
                    "status": "in_progress",
                    "rawInput": record.payload.get("arguments").cloned().unwrap_or(Value::Null),
                }));
            }
        }

        // Codex-normalized tool result.
        if record.event_type == "toolResult"
            && let Some(id) = record.payload.get("call_id").and_then(Value::as_str)
            && self.open_tools.remove(id)
        {
            updates.push(tool_completed(
                id,
                record.payload.get("output").cloned().unwrap_or(Value::Null),
            ));
        }

        // OpenCode tool part.
        if record.event_type == "tool"
            && let Some(part) = record.payload.get("part")
        {
            let name = part.get("tool").and_then(Value::as_str).unwrap_or("tool");
            let id = part
                .get("callID")
                .or_else(|| part.get("callId"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| tool_id(record, name));
            let status = part
                .pointer("/state/status")
                .and_then(Value::as_str)
                .unwrap_or("in_progress");
            match status {
                "pending" | "running" => {
                    if self.open_tools.insert(id.clone()) {
                        updates.push(json!({
                            "sessionUpdate": "tool_call",
                            "toolCallId": id,
                            "title": name,
                            "kind": "other",
                            "status": if status == "pending" { "pending" } else { "in_progress" },
                            "rawInput": part.pointer("/state/input").cloned().unwrap_or(Value::Null),
                        }));
                    } else if status == "running" {
                        updates.push(json!({
                            "sessionUpdate": "tool_call_update",
                            "toolCallId": id,
                            "status": "in_progress",
                        }));
                    }
                }
                "completed" => {
                    self.open_tools.remove(&id);
                    updates.push(tool_completed(
                        &id,
                        part.pointer("/state/output")
                            .cloned()
                            .unwrap_or(Value::Null),
                    ));
                }
                "error" | "failed" => {
                    self.open_tools.remove(&id);
                    updates.push(json!({
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": id,
                        "status": "failed",
                        "rawOutput": part.get("state").cloned().unwrap_or(Value::Null),
                    }));
                }
                _ => {}
            }
        }

        // Claude content blocks may carry text and tool activity in one
        // assistant/user transcript record.
        let Some(content) = record
            .payload
            .pointer("/message/content")
            .and_then(Value::as_array)
        else {
            return;
        };
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("tool_use") => {
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| tool_id(record, name));
                    if self.open_tools.insert(id.clone()) {
                        updates.push(json!({
                            "sessionUpdate": "tool_call",
                            "toolCallId": id,
                            "title": name,
                            "kind": "other",
                            "status": "in_progress",
                            "rawInput": block.get("input").cloned().unwrap_or(Value::Null),
                        }));
                    }
                }
                Some("tool_result") => {
                    let Some(id) = block
                        .get("tool_use_id")
                        .or_else(|| block.get("toolUseId"))
                        .and_then(Value::as_str)
                    else {
                        continue;
                    };
                    self.open_tools.remove(id);
                    updates.push(tool_completed(
                        id,
                        block.get("content").cloned().unwrap_or(Value::Null),
                    ));
                }
                _ => {}
            }
        }
    }

    fn finish_open_tools(&mut self) -> Vec<Value> {
        self.open_tools
            .drain()
            .map(|id| {
                json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": id,
                    "status": "completed",
                })
            })
            .collect()
    }
}

fn assistant_text(
    backend: libslop::Backend,
    record: &libslop::Record,
    opencode_previous: &mut HashMap<String, String>,
    opencode_role: Option<&str>,
) -> Option<String> {
    match backend {
        libslop::Backend::Codex if record.event_type == "agentMessage" => record
            .payload
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string),
        libslop::Backend::Claude if record.event_type == "assistant" => {
            content_text(record.payload.pointer("/message/content"))
        }
        libslop::Backend::Opencode
            if record.event_type == "text" && opencode_role == Some("assistant") =>
        {
            let part = record.payload.get("part")?;
            let text = part.get("text").and_then(Value::as_str)?;
            let part_id = part
                .get("id")
                .or_else(|| part.get("partID"))
                .or_else(|| part.get("partId"))
                .and_then(Value::as_str)
                .unwrap_or("default")
                .to_string();
            let previous = opencode_previous.entry(part_id).or_default();
            let delta = text
                .strip_prefix(previous.as_str())
                .unwrap_or(text)
                .to_string();
            *previous = text.to_string();
            Some(delta)
        }
        _ => None,
    }
}

fn content_text(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    let text = value
        .as_array()?
        .iter()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    (!text.is_empty()).then_some(text)
}

fn tool_id(record: &libslop::Record, name: &str) -> String {
    record
        .payload
        .get("call_id")
        .or_else(|| record.payload.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!(
                "slopd-{}-{}-{name}",
                record.pane_id.as_deref().unwrap_or("pane"),
                record.cursor.unwrap_or(0)
            )
        })
}

fn tool_completed(id: &str, output: Value) -> Value {
    let text = match &output {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "<unavailable>".into()),
    };
    json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": id,
        "status": "completed",
        "content": [{
            "type": "content",
            "content": {
                "type": "text",
                "text": text,
            },
        }],
        "rawOutput": output,
    })
}

async fn reject(sender: &Sender, id: Value, message: String) {
    wire::send(sender, wire::error(id, wire::INVALID_PARAMS, message)).await;
}

async fn server_error(sender: &Sender, id: Value, message: String) {
    wire::send(sender, wire::error(id, wire::SERVER_ERROR, message)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(event_type: &str, payload: Value) -> libslop::Record {
        libslop::Record {
            cursor: Some(12),
            source: "transcript".into(),
            event_type: event_type.into(),
            pane_id: Some("%4".into()),
            payload,
        }
    }

    #[test]
    fn first_prompt_frames_system_text_once_at_the_boundary() {
        let framed = frame_first_prompt(Some("[System]\nBe precise."), "Fix it.");
        assert!(framed.contains("--- begin ACP system prompt ---"));
        assert!(framed.contains("[System]\nBe precise."));
        assert!(framed.ends_with("User request:\nFix it."));
        assert_eq!(frame_first_prompt(None, "Fix it."), "Fix it.");
    }

    #[test]
    fn native_resume_overrides_static_resume_arguments() {
        let base = vec![
            "--foo".to_string(),
            "--resume".to_string(),
            "stale".to_string(),
            "--session=also-stale".to_string(),
            "--bar".to_string(),
        ];
        assert_eq!(
            launch_args(&base, Some("native-42")),
            ["--resume", "native-42", "--foo", "--bar"]
        );
        assert_eq!(launch_args(&base, None), base);
    }

    #[test]
    fn durable_tags_round_trip_acp_identity_and_cwd() {
        let session_id = "slopd:%42";
        let cwd = "/tmp/project with spaces";
        assert_eq!(
            tagged_session_id(&[session_tag(session_id)]).as_deref(),
            Some(session_id)
        );
        assert_eq!(tagged_cwd(&[cwd_tag(cwd)]).as_deref(), Some(Path::new(cwd)));
        assert!(
            session_tag(session_id)
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-')
        );
    }

    #[test]
    fn prompt_rejects_non_text_blocks() {
        let blocks = vec![json!({"type": "image", "data": "..."})];
        assert!(
            prompt_text("session/prompt", &blocks)
                .unwrap_err()
                .contains("unsupported")
        );
    }

    #[test]
    fn active_run_update_matches_buzz_native_steer_contract() {
        assert_eq!(
            active_run_update(Some("slopd-turn-7"))
                .pointer("/_meta/goose/activeRunId")
                .and_then(Value::as_str),
            Some("slopd-turn-7")
        );
        assert!(
            active_run_update(None)
                .pointer("/_meta/goose/activeRunId")
                .is_some_and(Value::is_null)
        );
    }

    #[test]
    fn subscription_ack_is_distinct_from_stream_close() {
        assert!(matches!(
            subscription_event(Ok(Some(libslopctl::SubscriptionItem::Subscribed))).unwrap(),
            SubscriptionEvent::Subscribed
        ));
        assert!(matches!(
            subscription_event(Ok(None)).unwrap(),
            SubscriptionEvent::Closed
        ));
    }

    #[test]
    fn codex_textless_ready_waits_for_the_stop_hook() {
        assert!(!state_ready_completes(libslop::Backend::Codex, true, false));
        assert!(state_ready_completes(libslop::Backend::Codex, true, true));
        assert!(state_ready_completes(libslop::Backend::Claude, true, false));
    }

    #[test]
    fn codex_turn_gate_discards_resumed_history_before_current_prompt() {
        let mut gate = TurnTranscriptGate::new(libslop::Backend::Codex, "current prompt", true);
        assert!(!gate.accepts(&record("agentMessage", json!({"text":"historical answer"}),)));
        assert!(!gate.accepts(&record("userMessage", json!({"text":"current prompt"}),)));
        assert!(!gate.accepts(&record(
            "agentMessage",
            json!({"text":"older matching answer"}),
        )));
        assert!(!gate.accepts(&record("userMessage", json!({"text":"current prompt"}),)));
        assert!(!gate.accepts(&record("agentMessage", json!({"text":"current answer"}),)));
        let buffered = gate.take_buffered();
        assert_eq!(buffered.len(), 1);
        assert_eq!(buffered[0].payload["text"], "current answer");
    }

    #[test]
    fn projects_claude_and_codex_assistant_text() {
        let mut previous = HashMap::new();
        let claude = record(
            "assistant",
            json!({"message":{"content":[{"type":"text","text":"hello"}]}}),
        );
        assert_eq!(
            assistant_text(libslop::Backend::Claude, &claude, &mut previous, None).as_deref(),
            Some("hello")
        );
        let codex = record("agentMessage", json!({"text":"world"}));
        assert_eq!(
            assistant_text(libslop::Backend::Codex, &codex, &mut previous, None).as_deref(),
            Some("world")
        );
    }

    #[test]
    fn opencode_cumulative_text_becomes_deltas() {
        let mut previous = HashMap::new();
        let first = record("text", json!({"part":{"id":"part-1","text":"hel"}}));
        let second = record("text", json!({"part":{"id":"part-1","text":"hello"}}));
        let other = record("text", json!({"part":{"id":"part-2","text":"world"}}));
        assert_eq!(
            assistant_text(
                libslop::Backend::Opencode,
                &first,
                &mut previous,
                Some("assistant")
            )
            .as_deref(),
            Some("hel")
        );
        assert_eq!(
            assistant_text(
                libslop::Backend::Opencode,
                &second,
                &mut previous,
                Some("assistant")
            )
            .as_deref(),
            Some("lo")
        );
        assert_eq!(
            assistant_text(
                libslop::Backend::Opencode,
                &other,
                &mut previous,
                Some("assistant")
            )
            .as_deref(),
            Some("world")
        );
    }

    #[test]
    fn opencode_user_parts_are_not_projected_as_agent_messages() {
        let mut projection = Projection::default();
        let user = record("user", json!({"info":{"role":"user"}}));
        let text = record("text", json!({"part":{"id":"part-user","text":"secret"}}));
        assert!(
            projection
                .updates(libslop::Backend::Opencode, &user)
                .is_empty()
        );
        assert!(
            projection
                .updates(libslop::Backend::Opencode, &text)
                .is_empty()
        );
    }
}
