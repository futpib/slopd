use super::*;

#[derive(Clone)]
pub(super) struct OpencodeState {
    pub(super) client: opencode::OpencodeClient,
    pub(super) session_id: String,
    pub(super) cancel: tokio_util::sync::CancellationToken,
    /// Most recent composer input waiting to become a real user message.
    /// OpenCode TUI commands never produce that message, so they never become
    /// automatic-retry candidates.
    pub(super) pending_prompt: Arc<std::sync::Mutex<Option<String>>>,
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
            pending_prompt: Arc::new(std::sync::Mutex::new(None)),
            last_prompt: Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct CodexState;

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

    fn cancel(&self) {}

    fn send_transport(&self) -> SendTransport {
        SendTransport::Tui {
            settle_before_enter: true,
        }
    }

    async fn interrupt(&self) -> Option<Result<(), String>> {
        None
    }

    async fn transcript(&self, _pane_id: &str) -> Option<Result<Vec<libslop::Record>, String>> {
        None
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
    fn driver_owns_initial_state(self) -> bool;
}

impl BackendPolicy for libslop::Backend {
    fn driver_owns_initial_state(self) -> bool {
        matches!(self, Self::Opencode)
    }
}

pub(super) struct PreparedCodexRun {
    pub(super) resume_session: Option<String>,
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
                    "--dangerously-bypass-hook-trust".to_string(),
                    "--no-alt-screen".to_string(),
                    "-C".to_string(),
                    cwd.to_string_lossy().into_owned(),
                ];
                if let Some(session_id) = &runtime.resume_session {
                    prefix.extend(["resume".to_string(), session_id.clone()]);
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
    pub(super) extra_args: &'a [String],
}

pub(super) struct ForkContext<'a> {
    pub(super) pane_id: &'a str,
    pub(super) session_id: &'a str,
    pub(super) extra_args: Vec<String>,
    pub(super) config: &'a libslop::SlopdConfig,
}

pub(super) struct RecoverContext<'a> {
    pub(super) pane_id: &'a str,
    pub(super) options: &'a ParsedPaneOptions,
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
    pub(super) extra_env: &'a [(String, String)],
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

    async fn recover(&self, context: RecoverContext<'_>) -> Result<(), String> {
        context
            .panes
            .get_or_insert(context.pane_id)
            .set_runtime(PaneRuntime::Claude(ClaudeState));
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
        let pane_id = spawn_pane(
            context.config,
            context.session_lock,
            &SpawnSpec {
                working_dir: launch_dir,
                config_dir: context.resolved.config_dir.clone(),
                backend: context.resolved.backend,
                executable: context.resolved.executable.clone(),
                extra_env: context.extra_env.to_vec(),
                trailing_args: vec!["--resume".to_string(), context.session_id.to_string()],
            },
        )
        .await
        .map_err(|e| {
            format!(
                "failed to restore pane {} (session {}): {}",
                context.old_pane_id, context.session_id, e
            )
        })?;
        context
            .panes
            .get_or_insert(&pane_id)
            .set_runtime(PaneRuntime::Claude(ClaudeState));
        Ok(pane_id)
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
                extra_env: context.extra_env.to_vec(),
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
        Ok(PreparedBackendRun::Codex(PreparedCodexRun {
            resume_session: extract_resume_target(context.extra_args),
        }))
    }

    async fn fork(&self, context: ForkContext<'_>) -> Result<(Vec<String>, String), String> {
        let mut args = vec!["fork".to_string(), context.session_id.to_string()];
        args.extend(context.extra_args);
        // Standalone `codex fork` creates the new session in the child process.
        // Its SessionStart hook binds the real id after spawn.
        Ok((args, String::new()))
    }

    async fn recover(&self, context: RecoverContext<'_>) -> Result<(), String> {
        context
            .panes
            .get_or_insert(context.pane_id)
            .set_runtime(PaneRuntime::Codex(CodexState));
        Ok(())
    }

    async fn restore(&self, context: RestoreContext<'_>) -> Result<String, String> {
        let hook_path = context.config.resolved_hook_path(context.resolved);
        if let Err(error) = libslop::inject_backend_hooks_into_file(
            &hook_path,
            &context.config.hook_slopctl(),
            libslop::Backend::Codex,
        ) {
            warn!(
                "failed to inject Codex hooks into {}: {}",
                hook_path.display(),
                error
            );
        }
        let launch_dir = context
            .transcript_path
            .as_deref()
            .and_then(transcript_launch_cwd)
            .or_else(|| context.working_dir.clone());
        let id = spawn_pane(
            context.config,
            context.session_lock,
            &SpawnSpec {
                working_dir: launch_dir.clone(),
                config_dir: context.resolved.config_dir.clone(),
                backend: context.resolved.backend,
                executable: context.resolved.executable.clone(),
                extra_env: context.extra_env.to_vec(),
                trailing_args: vec![
                    "--dangerously-bypass-hook-trust".to_string(),
                    "--no-alt-screen".to_string(),
                    "-C".to_string(),
                    launch_dir.unwrap_or_else(|| ".".to_string()),
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
        context
            .panes
            .get_or_insert(&id)
            .set_runtime(PaneRuntime::Codex(CodexState));
        Ok(id)
    }
}
