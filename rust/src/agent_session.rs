use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt as _;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, ClientCapabilities, ClientSessionCapabilities, ErrorCode, Implementation,
    InitializeRequest, McpCapabilities, NewSessionRequest, NewSessionResponse, SessionConfigKind,
    SessionConfigOption, SessionConfigOptionValue, SessionConfigSelect, SessionConfigSelectOptions,
    SessionModeState, SetSessionConfigOptionRequest, SetSessionModeRequest,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectionTo};
use tokio::io::AsyncReadExt as _;
use tokio::process::{Child, Command};
use tokio::sync::Notify;
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
use tokio_util::sync::CancellationToken;

use crate::agent_error::AgentStartupFailure;
use crate::agent_launch::{AgentLaunchSpec, ResolvedAgentCommand, ResolvedModeApplication};
use crate::agent_profile::{ResolvedAgentProfile, WorkspaceLeaseV1};
use crate::result_mcp::{ResultMcpService, ResultRoute};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AgentReadySnapshot {
    pub(crate) pid: u32,
    pub(crate) session_id: String,
    pub(crate) agent_info: Option<Implementation>,
    pub(crate) agent_capabilities: AgentCapabilities,
    pub(crate) generation: u64,
    pub(crate) server_name: String,
    pub(crate) endpoint: String,
    pub(crate) effective_model: String,
    pub(crate) effective_effort: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AgentSessionState {
    Opening,
    Ready(Arc<AgentReadySnapshot>),
    AuthRequired(AgentStartupFailure),
    StartFailed(AgentStartupFailure),
    Closed,
}

pub(crate) struct AgentSessionSlot {
    state: Mutex<AgentSessionState>,
    changed: Notify,
    cancellation: CancellationToken,
    cleanup_complete: AtomicBool,
    cleanup_changed: Notify,
}

impl AgentSessionSlot {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(AgentSessionState::Opening),
            changed: Notify::new(),
            cancellation: CancellationToken::new(),
            cleanup_complete: AtomicBool::new(false),
            cleanup_changed: Notify::new(),
        })
    }

    #[cfg(feature = "agent-test-support")]
    pub(crate) fn inert(profile: &ResolvedAgentProfile) -> Arc<Self> {
        let slot = Self::new();
        slot.commit_ready(AgentReadySnapshot {
            pid: std::process::id(),
            session_id: format!("inert-{}", profile.agent.name()),
            agent_info: None,
            agent_capabilities: AgentCapabilities::default(),
            generation: 1,
            server_name: "inert-result-route".to_owned(),
            endpoint: "http://127.0.0.1:0/mcp".to_owned(),
            effective_model: profile.requested_model.clone(),
            effective_effort: profile.requested_effort.clone(),
        });
        slot.mark_cleanup_complete();
        slot
    }

    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub(crate) fn cancel(&self) {
        self.cancellation.cancel();
        let mut state = lock(&self.state);
        if matches!(
            *state,
            AgentSessionState::Opening | AgentSessionState::Ready(_)
        ) {
            *state = AgentSessionState::Closed;
            self.changed.notify_waiters();
        }
    }

    #[cfg(feature = "agent-test-support")]
    pub(crate) async fn readiness(&self) -> Result<Arc<AgentReadySnapshot>, AgentStartupFailure> {
        loop {
            let changed = self.changed.notified();
            match &*lock(&self.state) {
                AgentSessionState::Opening => {}
                AgentSessionState::Ready(snapshot) => return Ok(Arc::clone(snapshot)),
                AgentSessionState::AuthRequired(failure)
                | AgentSessionState::StartFailed(failure) => return Err(failure.clone()),
                AgentSessionState::Closed => {
                    return Err(AgentStartupFailure::start(
                        "preparation_failed",
                        "preparation",
                        "agent session was closed",
                    ));
                }
            }
            changed.await;
        }
    }

    #[cfg(feature = "agent-test-support")]
    pub(crate) fn state_name(&self) -> &'static str {
        match &*lock(&self.state) {
            AgentSessionState::Opening => "opening",
            AgentSessionState::Ready(_) => "ready",
            AgentSessionState::AuthRequired(_) => "auth_required",
            AgentSessionState::StartFailed(_) => "start_failed",
            AgentSessionState::Closed => "closed",
        }
    }

    pub(crate) fn cleanup_is_complete(&self) -> bool {
        self.cleanup_complete.load(Ordering::Acquire)
    }

    pub(crate) async fn wait_cleanup(&self) {
        while !self.cleanup_is_complete() {
            let changed = self.cleanup_changed.notified();
            if self.cleanup_is_complete() {
                break;
            }
            changed.await;
        }
    }

    fn mark_cleanup_complete(&self) {
        self.cleanup_complete.store(true, Ordering::Release);
        self.cleanup_changed.notify_waiters();
    }

    fn commit_ready(&self, snapshot: AgentReadySnapshot) {
        let mut state = lock(&self.state);
        if matches!(*state, AgentSessionState::Opening) {
            *state = AgentSessionState::Ready(Arc::new(snapshot));
            self.changed.notify_waiters();
        }
    }

    fn commit_failure(&self, failure: AgentStartupFailure) {
        let mut state = lock(&self.state);
        if matches!(*state, AgentSessionState::Opening) {
            *state = if failure.authentication_required {
                AgentSessionState::AuthRequired(failure)
            } else {
                AgentSessionState::StartFailed(failure)
            };
            self.changed.notify_waiters();
        }
    }
}

impl Drop for AgentSessionSlot {
    fn drop(&mut self) {
        self.cancellation.cancel();
        *lock(&self.state) = AgentSessionState::Closed;
        self.changed.notify_waiters();
    }
}

pub(crate) fn spawn_opening(
    slot: &Arc<AgentSessionSlot>,
    profile: Arc<ResolvedAgentProfile>,
    spec: &'static AgentLaunchSpec,
    command: ResolvedAgentCommand,
    result_service: Arc<ResultMcpService>,
) {
    let slot = Arc::downgrade(slot);
    pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
        open_agent_session(slot.clone(), profile, spec, command, result_service).await;
        if let Some(slot) = slot.upgrade() {
            slot.mark_cleanup_complete();
        }
    });
}

async fn open_agent_session(
    slot: Weak<AgentSessionSlot>,
    profile: Arc<ResolvedAgentProfile>,
    spec: &'static AgentLaunchSpec,
    command: ResolvedAgentCommand,
    result_service: Arc<ResultMcpService>,
) {
    let Some(strong_slot) = slot.upgrade() else {
        return;
    };
    let cancellation = strong_slot.cancellation();
    drop(strong_slot);
    if cancellation.is_cancelled() {
        return;
    }
    #[cfg(feature = "agent-test-support")]
    if let Some(gate) = &command.opening_gate {
        tokio::select! {
            () = gate.wait() => {}
            () = cancellation.cancelled() => return,
        }
    }
    if let Err(failure) = revalidate_workspace(&profile.workspace, "preparation") {
        commit_failure(&slot, failure);
        return;
    }
    let endpoint = match result_service.ensure_ready().await {
        Ok(endpoint) => endpoint,
        Err(failure) => {
            commit_failure(&slot, failure);
            return;
        }
    };
    if cancellation.is_cancelled() {
        return;
    }

    let mut child = match spawn_child(&command, &profile.workspace) {
        Ok(child) => child,
        Err(failure) => {
            commit_failure(&slot, failure);
            return;
        }
    };
    let pid = child.id().expect("a running agent child has a process id");
    let stdin = child.stdin.take().expect("agent stdin was configured");
    let stdout = child.stdout.take().expect("agent stdout was configured");
    let stderr_drain = child.stderr.take().map(|mut stderr| {
        pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
            let mut buffer = [0_u8; 8192];
            while stderr.read(&mut buffer).await.is_ok_and(|read| read != 0) {}
        })
    });

    let current_route: Arc<Mutex<Option<Arc<ResultRoute>>>> = Arc::new(Mutex::new(None));
    let route_for_connection = Arc::clone(&current_route);
    let slot_for_connection = slot.clone();
    let profile_for_connection = Arc::clone(&profile);
    let service_for_connection = Arc::clone(&result_service);
    let cancellation_for_connection = cancellation.clone();
    let command_for_connection = command.clone();
    let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
    let connection = Client.builder().name("troupe").connect_with(
        transport,
        move |connection: ConnectionTo<Agent>| async move {
            let result = open_handshake(
                &connection,
                &slot_for_connection,
                &profile_for_connection,
                spec,
                &command_for_connection,
                &service_for_connection,
                &route_for_connection,
                pid,
                &endpoint,
                &cancellation_for_connection,
            )
            .await;
            match result {
                Ok(()) => cancellation_for_connection.cancelled().await,
                Err(failure) => commit_failure(&slot_for_connection, failure),
            }
            Ok(())
        },
    );
    tokio::pin!(connection);

    enum Completion {
        Connection,
        Child,
        Cancelled,
    }
    let completion = {
        let child_wait = child.wait();
        tokio::pin!(child_wait);
        tokio::select! {
            _ = &mut connection => Completion::Connection,
            _ = &mut child_wait => Completion::Child,
            () = cancellation.cancelled() => Completion::Cancelled,
        }
    };
    if !matches!(completion, Completion::Cancelled) {
        commit_failure(
            &slot,
            AgentStartupFailure::start(
                "spawn_failed",
                "spawn",
                "agent process exited during startup",
            ),
        );
    }
    let route = { lock(&current_route).take() };
    if let Some(route) = route {
        result_service.revoke_route(&route).await;
    }
    terminate_and_reap(&mut child, pid).await;
    wait_for_stderr_drain(stderr_drain).await;
}

#[allow(clippy::too_many_arguments)]
async fn open_handshake(
    connection: &ConnectionTo<Agent>,
    slot: &Weak<AgentSessionSlot>,
    profile: &ResolvedAgentProfile,
    spec: &'static AgentLaunchSpec,
    command: &ResolvedAgentCommand,
    result_service: &ResultMcpService,
    current_route: &Mutex<Option<Arc<ResultRoute>>>,
    pid: u32,
    endpoint: &str,
    cancellation: &CancellationToken,
) -> Result<(), AgentStartupFailure> {
    if !spec.supports_step1_opening(profile.agent) {
        return Err(AgentStartupFailure::start(
            "protocol_incompatible",
            "initialize",
            "agent launch contract is incompatible with this runtime",
        ));
    }
    let initialize = InitializeRequest::new(ProtocolVersion::V1)
        .client_capabilities(ClientCapabilities::new().session(ClientSessionCapabilities::new()))
        .client_info(Implementation::new("troupe", env!("CARGO_PKG_VERSION")));
    let initialized = connection
        .send_request(initialize)
        .block_task()
        .await
        .map_err(|_| {
            AgentStartupFailure::start(
                "protocol_incompatible",
                "initialize",
                "agent initialization failed",
            )
        })?;
    if initialized.protocol_version != ProtocolVersion::V1
        || !supports_http_mcp(&initialized.agent_capabilities.mcp_capabilities)
    {
        return Err(AgentStartupFailure::start(
            "protocol_incompatible",
            "initialize",
            "agent does not support the required protocol",
        ));
    }
    let agent_info = initialized.agent_info;
    let agent_capabilities = initialized.agent_capabilities;

    let route = result_service.register_route(
        1,
        #[cfg(feature = "agent-test-support")]
        command.mcp_ready_gate.clone(),
    )?;
    *lock(current_route) = Some(Arc::clone(&route));
    revalidate_workspace(&profile.workspace, "session_new")?;
    let session = send_new_session(connection, profile, &route).await;
    if session
        .as_ref()
        .is_err_and(|error| error.code == ErrorCode::AuthRequired)
    {
        result_service.revoke_route(&route).await;
        *lock(current_route) = None;
        return Err(AgentStartupFailure::authentication_required("session_new"));
    }
    let session = session.map_err(|error| {
        if error.code == ErrorCode::AuthRequired {
            AgentStartupFailure::authentication_required("session_new")
        } else {
            AgentStartupFailure::start(
                "protocol_incompatible",
                "session_new",
                "agent session creation failed",
            )
        }
    })?;
    revalidate_workspace(&profile.workspace, "session_new")?;

    let (effective_model, effective_effort) = configure_session(
        connection,
        spec,
        &command.mode_application,
        &session,
        &profile.requested_model,
        profile.requested_effort.as_deref(),
    )
    .await?;
    #[cfg(feature = "agent-test-support")]
    if let Some(gate) = &command.configuration_ready_gate {
        tokio::select! {
            () = gate.wait() => {}
            () = cancellation.cancelled() => {
                return Err(AgentStartupFailure::start(
                    "result_channel_unavailable",
                    "configuration",
                    "agent session was closed before configuration readiness",
                ));
            }
        }
        gate.mark_completed();
    }
    route.wait_ready(cancellation).await?;
    revalidate_workspace(&profile.workspace, "session_new")?;
    let Some(slot) = slot.upgrade() else {
        return Ok(());
    };
    slot.commit_ready(AgentReadySnapshot {
        pid,
        session_id: session.session_id.to_string(),
        agent_info,
        agent_capabilities,
        generation: route.generation,
        server_name: route.server_name.clone(),
        endpoint: endpoint.to_owned(),
        effective_model,
        effective_effort,
    });
    Ok(())
}

fn supports_http_mcp(capabilities: &McpCapabilities) -> bool {
    capabilities.http
}

async fn send_new_session(
    connection: &ConnectionTo<Agent>,
    profile: &ResolvedAgentProfile,
    route: &ResultRoute,
) -> agent_client_protocol::Result<agent_client_protocol::schema::v1::NewSessionResponse> {
    connection
        .send_request(
            NewSessionRequest::new(profile.workspace.acp_cwd_alias.clone())
                .mcp_servers(vec![route.mcp_server()]),
        )
        .block_task()
        .await
}

async fn configure_session(
    connection: &ConnectionTo<Agent>,
    spec: &AgentLaunchSpec,
    mode_application: &ResolvedModeApplication,
    session: &NewSessionResponse,
    requested_model: &str,
    requested_effort: Option<&str>,
) -> Result<(String, Option<String>), AgentStartupFailure> {
    let session_id = &session.session_id;
    let initial = session.config_options.as_deref().unwrap_or_default();
    let after_mode = match mode_application {
        ResolvedModeApplication::SessionConfigOption { config_id, value } => {
            require_select_value(initial, config_id, value)?;
            let after_mode = apply_select(connection, session_id, config_id, value).await?;
            require_current(&after_mode, config_id, value)?;
            after_mode
        }
        ResolvedModeApplication::LegacySessionMode { mode_id } => {
            require_legacy_mode(session.modes.as_ref(), mode_id)?;
            apply_legacy_mode(connection, session_id, mode_id).await?;
            initial.to_vec()
        }
    };

    require_select_value(&after_mode, spec.model_config_id, requested_model)?;
    let after_model = apply_select(
        connection,
        session_id,
        spec.model_config_id,
        requested_model,
    )
    .await?;
    require_applied_mode(&after_model, mode_application)?;
    require_current(&after_model, spec.model_config_id, requested_model)?;

    let effective_effort = if let Some(requested_effort) = requested_effort {
        require_select_value(&after_model, spec.effort_config_id, requested_effort)?;
        let after_effort = apply_select(
            connection,
            session_id,
            spec.effort_config_id,
            requested_effort,
        )
        .await?;
        require_applied_mode(&after_effort, mode_application)?;
        require_current(&after_effort, spec.model_config_id, requested_model)?;
        require_current(&after_effort, spec.effort_config_id, requested_effort)?;
        Some(requested_effort.to_owned())
    } else {
        Some(current_select(&after_model, spec.effort_config_id)?.to_owned())
    };
    Ok((requested_model.to_owned(), effective_effort))
}

fn require_applied_mode(
    options: &[SessionConfigOption],
    mode_application: &ResolvedModeApplication,
) -> Result<(), AgentStartupFailure> {
    match mode_application {
        ResolvedModeApplication::SessionConfigOption { config_id, value } => {
            require_current(options, config_id, value)
        }
        ResolvedModeApplication::LegacySessionMode { .. } => Ok(()),
    }
}

fn require_legacy_mode(
    modes: Option<&SessionModeState>,
    requested: &str,
) -> Result<(), AgentStartupFailure> {
    let modes = modes.ok_or_else(configuration_invalid)?;
    let mut matches = modes
        .available_modes
        .iter()
        .filter(|mode| mode.id.0.as_ref() == requested);
    matches.next().ok_or_else(configuration_invalid)?;
    if matches.next().is_some() {
        return Err(configuration_invalid());
    }
    Ok(())
}

async fn apply_legacy_mode(
    connection: &ConnectionTo<Agent>,
    session_id: &agent_client_protocol::schema::v1::SessionId,
    mode_id: &str,
) -> Result<(), AgentStartupFailure> {
    connection
        .send_request(SetSessionModeRequest::new(
            session_id.clone(),
            mode_id.to_owned(),
        ))
        .block_task()
        .await
        .map(|_| ())
        .map_err(|_| configuration_invalid())
}

async fn apply_select(
    connection: &ConnectionTo<Agent>,
    session_id: &agent_client_protocol::schema::v1::SessionId,
    config_id: &str,
    value: &str,
) -> Result<Vec<SessionConfigOption>, AgentStartupFailure> {
    connection
        .send_request(SetSessionConfigOptionRequest::new(
            session_id.clone(),
            config_id.to_owned(),
            SessionConfigOptionValue::value_id(value.to_owned()),
        ))
        .block_task()
        .await
        .map(|response| response.config_options)
        .map_err(|_| configuration_invalid())
}

fn configuration_invalid() -> AgentStartupFailure {
    AgentStartupFailure::start(
        "configuration_invalid",
        "configure",
        "agent session configuration is invalid",
    )
}

fn select_option<'a>(
    options: &'a [SessionConfigOption],
    config_id: &str,
) -> Result<&'a SessionConfigOption, AgentStartupFailure> {
    let mut matches = options
        .iter()
        .filter(|option| option.id.to_string() == config_id);
    let option = matches.next().ok_or_else(configuration_invalid)?;
    if matches.next().is_some() {
        return Err(configuration_invalid());
    }
    Ok(option)
}

fn current_select<'a>(
    options: &'a [SessionConfigOption],
    config_id: &str,
) -> Result<&'a str, AgentStartupFailure> {
    let select = validated_select(options, config_id)?;
    Ok(select.current_value.0.as_ref())
}

fn validated_select<'a>(
    options: &'a [SessionConfigOption],
    config_id: &str,
) -> Result<&'a SessionConfigSelect, AgentStartupFailure> {
    let SessionConfigKind::Select(select) = &select_option(options, config_id)?.kind else {
        return Err(configuration_invalid());
    };
    if select_contains(select, select.current_value.0.as_ref()) {
        Ok(select)
    } else {
        Err(configuration_invalid())
    }
}

fn require_current(
    options: &[SessionConfigOption],
    config_id: &str,
    expected: &str,
) -> Result<(), AgentStartupFailure> {
    if current_select(options, config_id)? == expected {
        Ok(())
    } else {
        Err(configuration_invalid())
    }
}

fn require_select_value(
    options: &[SessionConfigOption],
    config_id: &str,
    requested: &str,
) -> Result<(), AgentStartupFailure> {
    if select_contains(validated_select(options, config_id)?, requested) {
        Ok(())
    } else {
        Err(configuration_invalid())
    }
}

fn select_contains(select: &SessionConfigSelect, requested: &str) -> bool {
    match &select.options {
        SessionConfigSelectOptions::Ungrouped(options) => options
            .iter()
            .any(|option| option.value.0.as_ref() == requested),
        SessionConfigSelectOptions::Grouped(groups) => groups.iter().any(|group| {
            group
                .options
                .iter()
                .any(|option| option.value.0.as_ref() == requested)
        }),
        _ => false,
    }
}

fn revalidate_workspace(
    workspace: &WorkspaceLeaseV1,
    phase: &'static str,
) -> Result<(), AgentStartupFailure> {
    let invalid = || {
        AgentStartupFailure::start(
            "preparation_failed",
            phase,
            "agent workspace identity changed",
        )
    };
    if workspace.owner_pid != std::process::id() {
        return Err(invalid());
    }
    let metadata = std::fs::metadata(&workspace.canonical_path).map_err(|_| invalid())?;
    if !metadata.is_dir()
        || metadata.dev() != workspace.st_dev
        || metadata.ino() != workspace.st_ino
    {
        return Err(invalid());
    }
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let status = unsafe { libc::fstat(workspace.directory.as_raw_fd(), stat.as_mut_ptr()) };
    if status != 0 {
        return Err(invalid());
    }
    let stat = unsafe { stat.assume_init() };
    if stat.st_dev != workspace.st_dev || stat.st_ino != workspace.st_ino {
        return Err(invalid());
    }
    let path =
        CString::new(workspace.canonical_path.as_os_str().as_bytes()).map_err(|_| invalid())?;
    if unsafe { libc::access(path.as_ptr(), libc::R_OK | libc::X_OK) } != 0 {
        return Err(invalid());
    }
    Ok(())
}

fn spawn_child(
    command: &ResolvedAgentCommand,
    workspace: &WorkspaceLeaseV1,
) -> Result<Child, AgentStartupFailure> {
    let mut standard = std::process::Command::new(&command.program);
    standard
        .args(&command.args)
        .envs(command.environment.iter().cloned())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    standard.process_group(0);
    let directory = workspace.directory.as_raw_fd();
    unsafe {
        standard.pre_exec(move || {
            if libc::fchdir(directory) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    Command::from(standard).spawn().map_err(|_| {
        AgentStartupFailure::start("spawn_failed", "spawn", "agent process could not start")
    })
}

async fn terminate_and_reap(child: &mut Child, pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

async fn wait_for_stderr_drain(stderr_drain: Option<tokio::task::JoinHandle<()>>) {
    if let Some(stderr_drain) = stderr_drain {
        let _ = stderr_drain.await;
    }
}

fn commit_failure(slot: &Weak<AgentSessionSlot>, failure: AgentStartupFailure) {
    if let Some(slot) = slot.upgrade() {
        slot.commit_failure(failure);
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::oneshot;

    use super::*;

    #[tokio::test]
    async fn cleanup_waits_for_the_stderr_drain_task() {
        let (release, released) = oneshot::channel();
        let stderr_drain = tokio::spawn(async move {
            let _ = released.await;
        });
        let cleanup = tokio::spawn(async move {
            wait_for_stderr_drain(Some(stderr_drain)).await;
        });
        tokio::task::yield_now().await;
        assert!(!cleanup.is_finished());

        release
            .send(())
            .expect("stderr drain receiver remains live");

        cleanup.await.expect("cleanup task completes");
    }
}
