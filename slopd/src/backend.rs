use super::*;

#[derive(Clone)]
pub(super) struct OpencodeState {
    pub(super) client: opencode::OpencodeClient,
    pub(super) session_id: String,
    pub(super) cancel: tokio_util::sync::CancellationToken,
    pub(super) last_prompt: Arc<std::sync::Mutex<Option<String>>>,
}

impl OpencodeState {
    pub(super) fn new(
        client: opencode::OpencodeClient,
        session_id: String,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            client,
            session_id,
            cancel,
            last_prompt: Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

#[derive(Clone)]
pub(super) struct CodexState {
    pub(super) client: codex::CodexClient,
    pub(super) thread_id: String,
    pub(super) cancel: tokio_util::sync::CancellationToken,
    pub(super) active_turn: Arc<std::sync::Mutex<Option<String>>>,
    pub(super) pending_requests:
        Arc<std::sync::Mutex<std::collections::HashMap<String, libslop::PaneDetailedState>>>,
}

impl CodexState {
    pub(super) fn new(
        client: codex::CodexClient,
        thread_id: String,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            client,
            thread_id,
            cancel,
            active_turn: Arc::new(std::sync::Mutex::new(None)),
            pending_requests: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct ClaudeState;

#[derive(Clone)]
pub(super) struct UnboundState {
    pub(super) backend: libslop::Backend,
}

#[derive(Clone)]
pub(super) enum PaneRuntime {
    Unbound(UnboundState),
    Claude(ClaudeState),
    Opencode(OpencodeState),
    Codex(CodexState),
}

impl Default for PaneRuntime {
    fn default() -> Self {
        Self::Unbound(UnboundState {
            backend: libslop::Backend::Claude,
        })
    }
}

/// Backend behavior attached to a live pane. The enum provides closed-world
/// storage; this trait keeps backend operations out of request dispatch.
pub(super) trait BackendRuntime {
    fn backend(&self) -> libslop::Backend;
    fn cancel(&self);
    fn send_transport(&self) -> SendTransport;
    async fn interrupt(&self) -> Option<Result<(), String>>;
    async fn transcript(&self, pane_id: &str) -> Option<Result<Vec<libslop::Record>, String>>;
}

#[derive(Clone)]
pub(super) enum SendTransport {
    Unavailable(libslop::Backend),
    Tui { settle_before_enter: bool },
    Opencode(OpencodeState),
}

impl BackendRuntime for UnboundState {
    fn backend(&self) -> libslop::Backend {
        self.backend
    }
    fn cancel(&self) {}
    fn send_transport(&self) -> SendTransport {
        SendTransport::Unavailable(self.backend)
    }
    async fn interrupt(&self) -> Option<Result<(), String>> {
        Some(Err(format!(
            "{} runtime is still attaching",
            self.backend.canonical_executable()
        )))
    }
    async fn transcript(&self, _pane_id: &str) -> Option<Result<Vec<libslop::Record>, String>> {
        Some(Err(format!(
            "{} runtime is still attaching",
            self.backend.canonical_executable()
        )))
    }
}

impl BackendRuntime for ClaudeState {
    fn backend(&self) -> libslop::Backend {
        libslop::Backend::Claude
    }

    fn cancel(&self) {}

    fn send_transport(&self) -> SendTransport {
        SendTransport::Tui {
            settle_before_enter: false,
        }
    }

    async fn interrupt(&self) -> Option<Result<(), String>> {
        None
    }

    async fn transcript(&self, _pane_id: &str) -> Option<Result<Vec<libslop::Record>, String>> {
        None
    }
}

impl BackendRuntime for OpencodeState {
    fn backend(&self) -> libslop::Backend {
        libslop::Backend::Opencode
    }

    fn cancel(&self) {
        self.cancel.cancel();
    }

    fn send_transport(&self) -> SendTransport {
        SendTransport::Opencode(self.clone())
    }

    async fn interrupt(&self) -> Option<Result<(), String>> {
        Some(self.client.abort(&self.session_id).await)
    }

    async fn transcript(&self, pane_id: &str) -> Option<Result<Vec<libslop::Record>, String>> {
        Some(
            self.client
                .messages(&self.session_id)
                .await
                .map(|messages| {
                    opencode::messages_to_records(&messages)
                        .into_iter()
                        .enumerate()
                        .map(|(index, (event_type, payload))| libslop::Record {
                            cursor: Some(index as u64),
                            source: "transcript".to_string(),
                            event_type,
                            pane_id: Some(pane_id.to_string()),
                            payload,
                        })
                        .collect()
                }),
        )
    }
}

impl BackendRuntime for CodexState {
    fn backend(&self) -> libslop::Backend {
        libslop::Backend::Codex
    }

    fn cancel(&self) {
        self.cancel.cancel();
    }

    fn send_transport(&self) -> SendTransport {
        SendTransport::Tui {
            settle_before_enter: true,
        }
    }

    async fn interrupt(&self) -> Option<Result<(), String>> {
        let turn = self.active_turn.lock().unwrap().clone();
        Some(match turn {
            Some(turn) => self.client.interrupt_turn(&self.thread_id, &turn).await,
            None => Ok(()),
        })
    }

    async fn transcript(&self, pane_id: &str) -> Option<Result<Vec<libslop::Record>, String>> {
        Some(
            self.client
                .resume_thread(&self.thread_id)
                .await
                .map(|thread| codex::thread_records(&thread, pane_id)),
        )
    }
}

impl BackendRuntime for PaneRuntime {
    fn backend(&self) -> libslop::Backend {
        match self {
            Self::Unbound(runtime) => runtime.backend(),
            Self::Claude(runtime) => runtime.backend(),
            Self::Opencode(runtime) => runtime.backend(),
            Self::Codex(runtime) => runtime.backend(),
        }
    }

    fn cancel(&self) {
        match self {
            Self::Unbound(runtime) => runtime.cancel(),
            Self::Claude(runtime) => runtime.cancel(),
            Self::Opencode(runtime) => runtime.cancel(),
            Self::Codex(runtime) => runtime.cancel(),
        }
    }

    fn send_transport(&self) -> SendTransport {
        match self {
            Self::Unbound(runtime) => runtime.send_transport(),
            Self::Claude(runtime) => runtime.send_transport(),
            Self::Opencode(runtime) => runtime.send_transport(),
            Self::Codex(runtime) => runtime.send_transport(),
        }
    }

    async fn interrupt(&self) -> Option<Result<(), String>> {
        match self {
            Self::Unbound(runtime) => runtime.interrupt().await,
            Self::Claude(runtime) => runtime.interrupt().await,
            Self::Opencode(runtime) => runtime.interrupt().await,
            Self::Codex(runtime) => runtime.interrupt().await,
        }
    }

    async fn transcript(&self, pane_id: &str) -> Option<Result<Vec<libslop::Record>, String>> {
        match self {
            Self::Unbound(runtime) => runtime.transcript(pane_id).await,
            Self::Claude(runtime) => runtime.transcript(pane_id).await,
            Self::Opencode(runtime) => runtime.transcript(pane_id).await,
            Self::Codex(runtime) => runtime.transcript(pane_id).await,
        }
    }
}

pub(super) trait BackendPolicy {
    fn serializes_fresh_runs(self) -> bool;
    fn fork_session_is_known_before_spawn(self) -> bool;
    fn driver_owns_initial_state(self) -> bool;
}

impl BackendPolicy for libslop::Backend {
    fn serializes_fresh_runs(self) -> bool {
        matches!(self, Self::Codex)
    }

    fn fork_session_is_known_before_spawn(self) -> bool {
        !matches!(self, Self::Claude)
    }

    fn driver_owns_initial_state(self) -> bool {
        !matches!(self, Self::Claude)
    }
}

pub(super) struct PreparedCodexRun {
    pub(super) client: codex::CodexClient,
    pub(super) thread_id: Option<String>,
    pub(super) socket: std::path::PathBuf,
    pub(super) events: tokio::sync::broadcast::Receiver<serde_json::Value>,
    pub(super) existing_threads: std::collections::HashSet<String>,
}

pub(super) enum PreparedBackendRun {
    Claude,
    Opencode {
        port: u16,
        resume_session: Option<String>,
    },
    Codex(PreparedCodexRun),
}

impl PreparedBackendRun {
    pub(super) fn trailing_args(
        &self,
        extra_args: Vec<String>,
        cwd: &std::path::Path,
    ) -> Vec<String> {
        match self {
            Self::Claude => extra_args,
            Self::Opencode { resume_session, .. } => {
                let mut trailing = strip_resume_flags(extra_args);
                if let Some(session_id) = resume_session {
                    trailing.extend(["-s".to_string(), session_id.clone()]);
                }
                trailing
            }
            Self::Codex(runtime) => {
                let mut trailing = strip_resume_flags(extra_args);
                let mut prefix = vec![
                    "--remote".to_string(),
                    "unix://".to_string(),
                    "--no-alt-screen".to_string(),
                    "-C".to_string(),
                    cwd.to_string_lossy().into_owned(),
                ];
                if let Some(thread_id) = &runtime.thread_id {
                    prefix.extend(["resume".to_string(), thread_id.clone()]);
                }
                trailing.splice(0..0, prefix);
                trailing
            }
        }
    }

    pub(super) fn opencode_port(&self) -> Option<u16> {
        match self {
            Self::Opencode { port, .. } => Some(*port),
            _ => None,
        }
    }
}

pub(super) struct PrepareRunContext<'a> {
    pub(super) resolved: &'a libslop::ResolvedAccount,
    pub(super) merged_env: &'a [(String, String)],
    pub(super) start_directory: Option<&'a std::path::Path>,
    pub(super) extra_args: &'a [String],
}

pub(super) struct ForkContext<'a> {
    pub(super) pane_id: &'a str,
    pub(super) session_id: &'a str,
    pub(super) extra_args: Vec<String>,
    pub(super) config: &'a libslop::SlopdConfig,
    pub(super) panes: &'a PaneMap,
}

pub(super) struct RecoverContext<'a> {
    pub(super) pane_id: &'a str,
    pub(super) options: &'a ParsedPaneOptions,
    pub(super) resolved: &'a libslop::ResolvedAccount,
    pub(super) config: &'a Arc<libslop::SlopdConfig>,
    pub(super) panes: &'a PaneMap,
    pub(super) event_tx: &'a EventTx,
}

pub(super) struct RestoreContext<'a> {
    pub(super) old_pane_id: &'a str,
    pub(super) session_id: &'a str,
    pub(super) working_dir: &'a Option<String>,
    pub(super) transcript_path: &'a Option<String>,
    pub(super) resolved: &'a libslop::ResolvedAccount,
    pub(super) config: &'a Arc<libslop::SlopdConfig>,
    pub(super) session_lock: &'a SessionLock,
    pub(super) panes: &'a PaneMap,
    pub(super) event_tx: &'a EventTx,
}

#[async_trait::async_trait]
pub(super) trait BackendLifecycle: Sync {
    async fn prepare_run(
        &self,
        context: PrepareRunContext<'_>,
    ) -> Result<PreparedBackendRun, String>;
    async fn fork(&self, context: ForkContext<'_>) -> Result<(Vec<String>, String), String>;
    async fn recover(&self, context: RecoverContext<'_>) -> Result<(), String>;
    async fn restore(&self, context: RestoreContext<'_>) -> Result<String, String>;
}

struct ClaudeBackend;
struct OpencodeBackend;
struct CodexBackend;

static CLAUDE_BACKEND: ClaudeBackend = ClaudeBackend;
static OPENCODE_BACKEND: OpencodeBackend = OpencodeBackend;
static CODEX_BACKEND: CodexBackend = CodexBackend;

pub(super) fn backend_lifecycle(backend: libslop::Backend) -> &'static dyn BackendLifecycle {
    match backend {
        libslop::Backend::Claude => &CLAUDE_BACKEND,
        libslop::Backend::Opencode => &OPENCODE_BACKEND,
        libslop::Backend::Codex => &CODEX_BACKEND,
    }
}

#[async_trait::async_trait]
impl BackendLifecycle for ClaudeBackend {
    async fn prepare_run(
        &self,
        _context: PrepareRunContext<'_>,
    ) -> Result<PreparedBackendRun, String> {
        Ok(PreparedBackendRun::Claude)
    }

    async fn fork(&self, context: ForkContext<'_>) -> Result<(Vec<String>, String), String> {
        let new_id = uuid::Uuid::new_v4().to_string();
        let mut args = vec![
            "--resume".to_string(),
            context.session_id.to_string(),
            "--fork-session".to_string(),
            "--session-id".to_string(),
            new_id.clone(),
        ];
        args.extend(context.extra_args);
        Ok((args, new_id))
    }

    async fn recover(&self, _context: RecoverContext<'_>) -> Result<(), String> {
        Ok(())
    }

    async fn restore(&self, context: RestoreContext<'_>) -> Result<String, String> {
        let settings_path = context.config.resolved_settings_path(context.resolved);
        if let Err(error) =
            libslop::inject_hooks_into_file(&settings_path, &context.config.hook_slopctl())
        {
            warn!(
                "failed to inject hooks into {}: {}",
                settings_path.display(),
                error
            );
        }
        let launch_dir = context
            .transcript_path
            .as_deref()
            .and_then(transcript_launch_cwd)
            .or_else(|| context.working_dir.clone());
        spawn_pane(
            context.config,
            context.session_lock,
            &SpawnSpec {
                working_dir: launch_dir,
                config_dir: context.resolved.config_dir.clone(),
                backend: context.resolved.backend,
                executable: context.resolved.executable.clone(),
                extra_env: Vec::new(),
                trailing_args: vec!["--resume".to_string(), context.session_id.to_string()],
            },
        )
        .await
        .map_err(|e| {
            format!(
                "failed to restore pane {} (session {}): {}",
                context.old_pane_id, context.session_id, e
            )
        })
    }
}

#[async_trait::async_trait]
impl BackendLifecycle for OpencodeBackend {
    async fn prepare_run(
        &self,
        context: PrepareRunContext<'_>,
    ) -> Result<PreparedBackendRun, String> {
        Ok(PreparedBackendRun::Opencode {
            port: opencode::alloc_port()
                .map_err(|e| format!("failed to allocate opencode port: {}", e))?,
            resume_session: extract_resume_target(context.extra_args),
        })
    }

    async fn fork(&self, context: ForkContext<'_>) -> Result<(Vec<String>, String), String> {
        let port = read_pane_opencode_port(context.config, context.pane_id)
            .await
            .ok_or_else(|| {
                format!(
                    "opencode pane {} has no recorded port; cannot fork",
                    context.pane_id
                )
            })?;
        let client = opencode::OpencodeClient::new(port, None);
        let new_id = client
            .fork_session(context.session_id, None)
            .await
            .map_err(|e| {
                format!(
                    "opencode fork of session {} failed: {}",
                    context.session_id, e
                )
            })?;
        let mut args = vec!["-s".to_string(), new_id.clone()];
        args.extend(context.extra_args);
        Ok((args, new_id))
    }

    async fn recover(&self, context: RecoverContext<'_>) -> Result<(), String> {
        let (port, session_id) = match (
            context.options.opencode_port,
            context.options.session_id.as_deref(),
        ) {
            (Some(port), Some(session_id)) if !session_id.is_empty() => (port, session_id),
            _ => {
                return Err(format!(
                    "opencode pane {} lacks recovery metadata",
                    context.pane_id
                ));
            }
        };
        let client = opencode::OpencodeClient::new(port, context.options.opencode_token.clone());
        let cancel = tokio_util::sync::CancellationToken::new();
        context
            .panes
            .get_or_insert(context.pane_id)
            .set_runtime(PaneRuntime::Opencode(OpencodeState::new(
                client.clone(),
                session_id.to_string(),
                cancel.clone(),
            )));
        tokio::spawn(run_opencode_driver(
            client,
            session_id.to_string(),
            context.pane_id.to_string(),
            context.config.clone(),
            context.panes.clone(),
            context.event_tx.clone(),
            cancel,
        ));
        Ok(())
    }

    async fn restore(&self, context: RestoreContext<'_>) -> Result<String, String> {
        let port = opencode::alloc_port()
            .map_err(|e| format!("failed to allocate opencode port: {}", e))?;
        let id = spawn_pane(
            context.config,
            context.session_lock,
            &SpawnSpec {
                working_dir: context.working_dir.clone(),
                config_dir: context.resolved.config_dir.clone(),
                backend: context.resolved.backend,
                executable: context.resolved.executable.clone(),
                extra_env: Vec::new(),
                trailing_args: vec![
                    "-s".to_string(),
                    context.session_id.to_string(),
                    "--port".to_string(),
                    port.to_string(),
                    "--hostname".to_string(),
                    "127.0.0.1".to_string(),
                ],
            },
        )
        .await
        .map_err(|e| {
            format!(
                "failed to restore opencode pane {}: {}",
                context.old_pane_id, e
            )
        })?;
        let _ = tmux_set_pane_option(
            context.config,
            &id,
            libslop::TmuxOption::SlopdBackend.as_str(),
            "opencode",
        )
        .await;
        let _ = tmux_set_pane_option(
            context.config,
            &id,
            libslop::TmuxOption::SlopdOpencodePort.as_str(),
            &port.to_string(),
        )
        .await;
        let client = opencode::OpencodeClient::new(port, None);
        let cancel = tokio_util::sync::CancellationToken::new();
        context
            .panes
            .get_or_insert(&id)
            .set_runtime(PaneRuntime::Opencode(OpencodeState::new(
                client.clone(),
                context.session_id.to_string(),
                cancel.clone(),
            )));
        tokio::spawn(run_opencode_driver(
            client,
            context.session_id.to_string(),
            id.clone(),
            context.config.clone(),
            context.panes.clone(),
            context.event_tx.clone(),
            cancel,
        ));
        Ok(id)
    }
}

#[async_trait::async_trait]
impl BackendLifecycle for CodexBackend {
    async fn prepare_run(
        &self,
        context: PrepareRunContext<'_>,
    ) -> Result<PreparedBackendRun, String> {
        let lookup_path = context
            .merged_env
            .iter()
            .rev()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| std::ffi::OsString::from(value))
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_default();
        let lookup_cwd = context
            .start_directory
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let program = libslop::resolve_executable(
            context.resolved.executable.program(),
            &lookup_path,
            &lookup_cwd,
        )
        .ok_or_else(|| {
            format!(
                "configured Codex executable {:?} not found",
                context.resolved.executable.program()
            )
        })?;
        let home = context
            .resolved
            .config_dir
            .as_deref()
            .map(libslop::expand_path);
        let socket = codex::socket_path(home.as_deref());
        codex::ensure_daemon(&program, home.as_deref(), &socket).await?;
        let client = codex::CodexClient::connect(&socket).await?;
        let events = client.subscribe();
        let existing_threads = client.thread_ids().await.unwrap_or_default();
        let thread_id = match extract_resume_target(context.extra_args) {
            Some(id) => {
                client.resume_thread(&id).await?;
                Some(id)
            }
            None => None,
        };
        Ok(PreparedBackendRun::Codex(PreparedCodexRun {
            client,
            thread_id,
            socket,
            events,
            existing_threads,
        }))
    }

    async fn fork(&self, context: ForkContext<'_>) -> Result<(Vec<String>, String), String> {
        let runtime = context
            .panes
            .get(context.pane_id)
            .and_then(|state| state.codex())
            .ok_or_else(|| format!("Codex pane {} has no app-server runtime", context.pane_id))?;
        let new_id = runtime.client.fork_thread(context.session_id).await?;
        let mut args = vec!["--resume".to_string(), new_id.clone()];
        args.extend(context.extra_args);
        Ok((args, new_id))
    }

    async fn recover(&self, context: RecoverContext<'_>) -> Result<(), String> {
        let session_id = context
            .options
            .session_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| format!("Codex pane {} lacks a thread id", context.pane_id))?;
        let home = context
            .resolved
            .config_dir
            .as_deref()
            .map(libslop::expand_path);
        let socket = context
            .options
            .codex_socket
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| codex::socket_path(home.as_deref()));
        let path = std::env::var_os("PATH").unwrap_or_default();
        let cwd = std::env::current_dir().unwrap_or_default();
        let program =
            libslop::resolve_executable(context.resolved.executable.program(), &path, &cwd)
                .ok_or_else(|| {
                    format!(
                        "Codex executable {:?} not found",
                        context.resolved.executable.program()
                    )
                })?;
        codex::ensure_daemon(&program, home.as_deref(), &socket).await?;
        let client = codex::CodexClient::connect(&socket).await?;
        let cancel = tokio_util::sync::CancellationToken::new();
        context
            .panes
            .get_or_insert(context.pane_id)
            .set_runtime(PaneRuntime::Codex(CodexState::new(
                client.clone(),
                session_id.to_string(),
                cancel.clone(),
            )));
        tokio::spawn(run_codex_driver(
            client,
            session_id.to_string(),
            context.pane_id.to_string(),
            context.config.clone(),
            context.panes.clone(),
            context.event_tx.clone(),
            cancel,
        ));
        Ok(())
    }

    async fn restore(&self, context: RestoreContext<'_>) -> Result<String, String> {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let cwd = context
            .working_dir
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let program =
            libslop::resolve_executable(context.resolved.executable.program(), &path, &cwd)
                .ok_or_else(|| {
                    format!("Codex executable missing for pane {}", context.old_pane_id)
                })?;
        let home = context
            .resolved
            .config_dir
            .as_deref()
            .map(libslop::expand_path);
        let socket = codex::socket_path(home.as_deref());
        codex::ensure_daemon(&program, home.as_deref(), &socket).await?;
        let client = codex::CodexClient::connect(&socket).await?;
        client.resume_thread(context.session_id).await?;
        let id = spawn_pane(
            context.config,
            context.session_lock,
            &SpawnSpec {
                working_dir: context.working_dir.clone(),
                config_dir: context.resolved.config_dir.clone(),
                backend: context.resolved.backend,
                executable: context.resolved.executable.clone(),
                extra_env: Vec::new(),
                trailing_args: vec![
                    "--remote".to_string(),
                    "unix://".to_string(),
                    "--no-alt-screen".to_string(),
                    "-C".to_string(),
                    cwd.to_string_lossy().into_owned(),
                    "resume".to_string(),
                    context.session_id.to_string(),
                ],
            },
        )
        .await
        .map_err(|e| {
            format!(
                "failed to restore Codex pane {}: {}",
                context.old_pane_id, e
            )
        })?;
        let _ = tmux_set_pane_option(
            context.config,
            &id,
            libslop::TmuxOption::SlopdBackend.as_str(),
            "codex",
        )
        .await;
        let _ = tmux_set_pane_option(
            context.config,
            &id,
            libslop::TmuxOption::SlopdCodexSocket.as_str(),
            &socket.to_string_lossy(),
        )
        .await;
        let cancel = tokio_util::sync::CancellationToken::new();
        context
            .panes
            .get_or_insert(&id)
            .set_runtime(PaneRuntime::Codex(CodexState::new(
                client.clone(),
                context.session_id.to_string(),
                cancel.clone(),
            )));
        tokio::spawn(run_codex_driver(
            client,
            context.session_id.to_string(),
            id.clone(),
            context.config.clone(),
            context.panes.clone(),
            context.event_tx.clone(),
            cancel,
        ));
        Ok(id)
    }
}
