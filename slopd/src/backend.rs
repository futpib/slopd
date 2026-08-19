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
pub(super) struct GrokState {
    pub(super) client: Arc<std::sync::Mutex<Option<grok::GrokClient>>>,
    pub(super) cancel: tokio_util::sync::CancellationToken,
    pub(super) leader_socket: Option<std::path::PathBuf>,
}

impl GrokState {
    pub(super) fn new(
        cancel: tokio_util::sync::CancellationToken,
        leader_socket: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            client: Arc::new(std::sync::Mutex::new(None)),
            cancel,
            leader_socket,
        }
    }

    pub(super) fn client(&self) -> Option<grok::GrokClient> {
        self.client.lock().unwrap().clone()
    }

    pub(super) fn set_client(&self, client: Option<grok::GrokClient>) {
        *self.client.lock().unwrap() = client;
    }

    pub(super) fn clear_client(&self, disconnected: &grok::GrokClient) {
        let mut client = self.client.lock().unwrap();
        if client
            .as_ref()
            .is_some_and(|current| current.same_connection(disconnected))
        {
            *client = None;
        }
    }

    pub(super) fn acp_connected(&self) -> bool {
        self.client()
            .is_some_and(|client| !client.is_disconnected())
    }
}

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
    Grok(GrokState),
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
    Grok(GrokState),
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

impl BackendRuntime for GrokState {
    fn backend(&self) -> libslop::Backend {
        libslop::Backend::Grok
    }

    fn cancel(&self) {
        self.cancel.cancel();
        if let Some(client) = self.client() {
            client.stop();
        }
        if let Some(socket) = self.leader_socket.clone() {
            grok::schedule_private_leader_cleanup(socket);
        }
    }

    fn send_transport(&self) -> SendTransport {
        SendTransport::Grok(self.clone())
    }

    async fn interrupt(&self) -> Option<Result<(), String>> {
        let client = self.client().filter(|client| !client.is_disconnected())?;
        let result = client.interrupt().await;
        if result.is_err() && client.is_disconnected() {
            None
        } else {
            Some(result)
        }
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
            Self::Grok(runtime) => runtime.backend(),
        }
    }

    fn cancel(&self) {
        match self {
            Self::Unbound(runtime) => runtime.cancel(),
            Self::Claude(runtime) => runtime.cancel(),
            Self::Opencode(runtime) => runtime.cancel(),
            Self::Codex(runtime) => runtime.cancel(),
            Self::Grok(runtime) => runtime.cancel(),
        }
    }

    fn send_transport(&self) -> SendTransport {
        match self {
            Self::Unbound(runtime) => runtime.send_transport(),
            Self::Claude(runtime) => runtime.send_transport(),
            Self::Opencode(runtime) => runtime.send_transport(),
            Self::Codex(runtime) => runtime.send_transport(),
            Self::Grok(runtime) => runtime.send_transport(),
        }
    }

    async fn interrupt(&self) -> Option<Result<(), String>> {
        match self {
            Self::Unbound(runtime) => runtime.interrupt().await,
            Self::Claude(runtime) => runtime.interrupt().await,
            Self::Opencode(runtime) => runtime.interrupt().await,
            Self::Codex(runtime) => runtime.interrupt().await,
            Self::Grok(runtime) => runtime.interrupt().await,
        }
    }

    async fn transcript(&self, pane_id: &str) -> Option<Result<Vec<libslop::Record>, String>> {
        match self {
            Self::Unbound(runtime) => runtime.transcript(pane_id).await,
            Self::Claude(runtime) => runtime.transcript(pane_id).await,
            Self::Opencode(runtime) => runtime.transcript(pane_id).await,
            Self::Codex(runtime) => runtime.transcript(pane_id).await,
            Self::Grok(runtime) => runtime.transcript(pane_id).await,
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

// Managed panes must not stop at Codex's startup update chooser before hooks
// and prompt submission become available.
fn codex_unattended_update_args() -> Vec<String> {
    vec![
        "-c".to_string(),
        "check_for_update_on_startup=false".to_string(),
    ]
}

pub(super) struct PreparedGrokRun {
    pub(super) session_id: String,
    pub(super) leader_socket: std::path::PathBuf,
}

pub(super) enum PreparedBackendRun {
    Claude,
    Opencode {
        port: u16,
        resume_session: Option<String>,
    },
    Codex(PreparedCodexRun),
    Grok(PreparedGrokRun),
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
                let mut prefix = codex_unattended_update_args();
                prefix.extend([
                    "--dangerously-bypass-hook-trust".to_string(),
                    "--no-alt-screen".to_string(),
                    "-C".to_string(),
                    cwd.to_string_lossy().into_owned(),
                ]);
                if let Some(session_id) = &runtime.resume_session {
                    prefix.extend(["resume".to_string(), session_id.clone()]);
                }
                trailing.splice(0..0, prefix);
                trailing
            }
            Self::Grok(runtime) => {
                let mut trailing = strip_grok_transport_flags(extra_args);
                let has_session_id = extract_grok_session_id(&trailing).is_some();
                let has_resume = extract_grok_resume_target(&trailing).is_some()
                    || trailing
                        .iter()
                        .any(|arg| arg == "--continue" || arg == "-c");
                let mut prefix = vec![
                    "--leader".to_string(),
                    "--leader-socket".to_string(),
                    runtime.leader_socket.to_string_lossy().into_owned(),
                    "--no-alt-screen".to_string(),
                ];
                if !has_session_id && !has_resume {
                    prefix.extend(["--session-id".to_string(), runtime.session_id.clone()]);
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
    pub(super) working_dir: &'a std::path::Path,
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
struct GrokBackend;

static CLAUDE_BACKEND: ClaudeBackend = ClaudeBackend;
static OPENCODE_BACKEND: OpencodeBackend = OpencodeBackend;
static CODEX_BACKEND: CodexBackend = CodexBackend;
static GROK_BACKEND: GrokBackend = GrokBackend;

pub(super) fn backend_lifecycle(backend: libslop::Backend) -> &'static dyn BackendLifecycle {
    match backend {
        libslop::Backend::Claude => &CLAUDE_BACKEND,
        libslop::Backend::Opencode => &OPENCODE_BACKEND,
        libslop::Backend::Codex => &CODEX_BACKEND,
        libslop::Backend::Grok => &GROK_BACKEND,
    }
}

fn extract_grok_resume_target(args: &[String]) -> Option<String> {
    let mut args = args.iter();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--resume" | "-r" => {
                if let Some(value) = args.next()
                    && !value.starts_with('-')
                {
                    return Some(value.clone());
                }
            }
            other => {
                for prefix in ["--resume=", "-r="] {
                    if let Some(value) = other.strip_prefix(prefix) {
                        return Some(value.to_string());
                    }
                }
            }
        }
    }
    None
}

fn extract_grok_session_id(args: &[String]) -> Option<String> {
    let mut args = args.iter();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--session-id" | "-s" => {
                if let Some(value) = args.next()
                    && !value.starts_with('-')
                {
                    return Some(value.clone());
                }
            }
            other => {
                for prefix in ["--session-id=", "-s="] {
                    if let Some(value) = other.strip_prefix(prefix) {
                        return Some(value.to_string());
                    }
                }
            }
        }
    }
    None
}

fn strip_grok_transport_flags(args: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut args = args.into_iter();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--leader" | "--no-leader" => {}
            "--leader-socket" => {
                let _ = args.next();
            }
            other if other.starts_with("--leader-socket=") => {}
            _ => out.push(argument),
        }
    }
    out
}

/// Grok's leader transport belongs to slopd, even when an account's executable
/// array contains its own transport flags. Preserve every ordinary global flag
/// while replacing leader selection/socket arguments with the pane-private
/// values prepared for this launch.
pub(super) fn normalized_grok_executable(executable: &libslop::Executable) -> libslop::Executable {
    let arguments = strip_grok_transport_flags(executable.args().to_vec());
    if arguments.is_empty() {
        libslop::Executable::String(executable.program().to_string())
    } else {
        let mut command = Vec::with_capacity(arguments.len() + 1);
        command.push(executable.program().to_string());
        command.extend(arguments);
        libslop::Executable::Array(command)
    }
}

fn new_grok_leader_socket() -> Result<std::path::PathBuf, String> {
    let directory = libslop::runtime_dir().join("slopd").join("grok-leaders");
    std::fs::create_dir_all(&directory).map_err(|error| {
        format!(
            "failed to create Grok leader directory {}: {error}",
            directory.display()
        )
    })?;
    Ok(directory.join(format!("{}.sock", uuid::Uuid::new_v4())))
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
        let mut args = codex_unattended_update_args();
        args.extend(["fork".to_string(), context.session_id.to_string()]);
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
                trailing_args: {
                    let mut args = codex_unattended_update_args();
                    args.extend([
                        "--dangerously-bypass-hook-trust".to_string(),
                        "--no-alt-screen".to_string(),
                        "-C".to_string(),
                        launch_dir.unwrap_or_else(|| ".".to_string()),
                        "resume".to_string(),
                        context.session_id.to_string(),
                    ]);
                    args
                },
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

#[async_trait::async_trait]
impl BackendLifecycle for GrokBackend {
    async fn prepare_run(
        &self,
        context: PrepareRunContext<'_>,
    ) -> Result<PreparedBackendRun, String> {
        let resume_session = extract_grok_resume_target(context.extra_args);
        let continues_latest = context
            .extra_args
            .iter()
            .any(|argument| argument == "--continue" || argument == "-c");
        let session_id = extract_grok_session_id(context.extra_args)
            .or_else(|| {
                (!context.extra_args.iter().any(|arg| arg == "--fork-session"))
                    .then(|| resume_session.clone())
                    .flatten()
            })
            .unwrap_or_else(|| {
                if continues_latest {
                    String::new()
                } else {
                    uuid::Uuid::new_v4().to_string()
                }
            });
        Ok(PreparedBackendRun::Grok(PreparedGrokRun {
            session_id,
            leader_socket: new_grok_leader_socket()?,
        }))
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
        let session_id = context
            .options
            .session_id
            .as_ref()
            .filter(|session| !session.is_empty())
            .ok_or_else(|| format!("Grok pane {} has no recorded session id", context.pane_id))?
            .clone();
        let leader_socket = context
            .options
            .grok_leader_socket
            .as_ref()
            .ok_or_else(|| {
                format!(
                    "Grok pane {} has no recorded leader socket",
                    context.pane_id
                )
            })?
            .clone();
        let mut resolved = context
            .config
            .resolve_account(context.options.account.as_deref())
            .or_else(|_| {
                context
                    .config
                    .resolve_account(Some(libslop::DEFAULT_ACCOUNT))
            })?;
        if resolved.backend != libslop::Backend::Grok {
            resolved.backend = libslop::Backend::Grok;
            if libslop::Backend::infer_from_program(resolved.executable.program()).is_some() {
                resolved.executable = libslop::Executable::String("grok".to_string());
            }
        }
        let cwd = context
            .options
            .transcript_path
            .as_deref()
            .and_then(transcript_launch_cwd)
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| context.working_dir.to_path_buf());
        let env = merge_spawn_env(context.config, Vec::new())?;
        let executable_spec = normalized_grok_executable(&resolved.executable);
        let executable = resolve_spawn_program(&executable_spec, &env, &cwd)?;
        let attach = grok::AttachSpec {
            executable,
            executable_args: executable_spec.args().to_vec(),
            leader_socket,
            session_id,
            cwd,
            config_dir: resolved.config_dir,
            env,
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let runtime = GrokState::new(cancel, Some(attach.leader_socket.clone()));
        let pane_state = context.panes.get_or_insert(context.pane_id);
        pane_state.set_runtime(PaneRuntime::Grok(runtime.clone()));
        tokio::spawn(grok::run_driver(
            attach,
            context.pane_id.to_string(),
            runtime,
            pane_state,
            context.panes.clone(),
            context.config.clone(),
            context.event_tx.clone(),
            false,
        ));
        Ok(())
    }

    async fn restore(&self, context: RestoreContext<'_>) -> Result<String, String> {
        let hook_path = context.config.resolved_hook_path(context.resolved);
        if let Err(error) = libslop::inject_backend_hooks_into_file(
            &hook_path,
            &context.config.hook_slopctl(),
            libslop::Backend::Grok,
        ) {
            warn!(
                "failed to inject Grok hooks into {}: {}",
                hook_path.display(),
                error
            );
        }
        let launch_dir = context
            .transcript_path
            .as_deref()
            .and_then(transcript_launch_cwd)
            .or_else(|| context.working_dir.clone());
        let cwd = launch_dir
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let leader_socket = new_grok_leader_socket()?;
        let trailing_args = vec![
            "--leader".to_string(),
            "--leader-socket".to_string(),
            leader_socket.to_string_lossy().into_owned(),
            "--no-alt-screen".to_string(),
            "--resume".to_string(),
            context.session_id.to_string(),
        ];
        let id = spawn_pane(
            context.config,
            context.session_lock,
            &SpawnSpec {
                working_dir: launch_dir,
                config_dir: context.resolved.config_dir.clone(),
                backend: libslop::Backend::Grok,
                executable: normalized_grok_executable(&context.resolved.executable),
                extra_env: context.extra_env.to_vec(),
                trailing_args,
            },
        )
        .await
        .map_err(|error| {
            format!(
                "failed to restore Grok pane {} (session {}): {}",
                context.old_pane_id, context.session_id, error
            )
        })?;
        let _ = tmux_set_pane_option(
            context.config,
            &id,
            libslop::TmuxOption::SlopdBackend.as_str(),
            "grok",
        )
        .await;
        let _ = tmux_set_pane_option(
            context.config,
            &id,
            libslop::TmuxOption::SlopdGrokLeaderSocket.as_str(),
            leader_socket.to_string_lossy().as_ref(),
        )
        .await;
        let env = merge_spawn_env(context.config, context.extra_env.to_vec())?;
        let executable_spec = normalized_grok_executable(&context.resolved.executable);
        let executable = resolve_spawn_program(&executable_spec, &env, &cwd)?;
        let attach = grok::AttachSpec {
            executable,
            executable_args: executable_spec.args().to_vec(),
            leader_socket,
            session_id: context.session_id.to_string(),
            cwd,
            config_dir: context.resolved.config_dir.clone(),
            env,
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let runtime = GrokState::new(cancel, Some(attach.leader_socket.clone()));
        let pane_state = context.panes.get_or_insert(&id);
        pane_state.note_session_id(context.session_id);
        pane_state.set_runtime(PaneRuntime::Grok(runtime.clone()));
        tokio::spawn(grok::run_driver(
            attach,
            id.clone(),
            runtime,
            pane_state,
            context.panes.clone(),
            context.config.clone(),
            context.event_tx.clone(),
            true,
        ));
        Ok(id)
    }
}
