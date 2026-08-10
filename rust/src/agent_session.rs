use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt as _;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, ClientCapabilities, ClientSessionCapabilities, ErrorCode, Implementation,
    InitializeRequest, McpCapabilities, NewSessionRequest, NewSessionResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SessionConfigKind, SessionConfigOption, SessionConfigOptionValue, SessionConfigSelect,
    SessionConfigSelectOptions, SessionId, SessionModeState, SetSessionConfigOptionRequest,
    SetSessionModeRequest,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectionTo, Dispatch, Handled};
use tokio::io::AsyncReadExt as _;
use tokio::process::{Child, Command};
use tokio::sync::{Notify, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
use tokio_util::sync::CancellationToken;

use crate::act_schema::CompiledActSchema;
use crate::agent_error::{AgentSessionFailure, AgentStartupFailure};
#[cfg(feature = "agent-test-support")]
use crate::agent_launch::TestOpeningGate;
use crate::agent_launch::{AgentLaunchSpec, ResolvedAgentCommand, ResolvedModeApplication};
use crate::agent_profile::{ResolvedAgentProfile, WorkspaceLeaseV1};
use crate::agent_turn::{
    AgentTurnControl, AgentTurnOutcome, AgentTurnRequest, PromptResponseProvenance,
    run_agent_turn_worker,
};
use crate::result_mcp::{ResultMcpService, ResultRoute};
use crate::schema_validation_bridge::PythonSchemaValidationBridge;

const ACP_FRAME_MAX_BYTES: usize = 16 * 1024 * 1024;
const ACP_JSON_MAX_DEPTH: usize = 64;

#[derive(Clone, Copy)]
#[repr(u8)]
enum OpeningPhase {
    Initialize,
    SessionNew,
    Configure,
    McpReady,
}

impl OpeningPhase {
    fn load(value: &AtomicU8) -> Self {
        match value.load(Ordering::Acquire) {
            value if value == Self::Initialize as u8 => Self::Initialize,
            value if value == Self::SessionNew as u8 => Self::SessionNew,
            value if value == Self::Configure as u8 => Self::Configure,
            value if value == Self::McpReady as u8 => Self::McpReady,
            _ => unreachable!("only OpeningPhase values are stored"),
        }
    }

    fn store(self, value: &AtomicU8) {
        value.store(self as u8, Ordering::Release);
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::SessionNew => "session_new",
            Self::Configure => "configure",
            Self::McpReady => "mcp_ready",
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct AcpFrameLimitedReader<R> {
    inner: R,
    frame_bytes: usize,
    frame: Vec<u8>,
    pending: Vec<u8>,
    pending_offset: usize,
    terminal_error: bool,
    inner_eof: bool,
    exceeded: Arc<AtomicBool>,
    json_depth: usize,
    in_json_string: bool,
    escaped_json_byte: bool,
}

impl<R> AcpFrameLimitedReader<R> {
    fn new(inner: R, exceeded: Arc<AtomicBool>) -> Self {
        Self {
            inner,
            frame_bytes: 0,
            frame: Vec::new(),
            pending: Vec::new(),
            pending_offset: 0,
            terminal_error: false,
            inner_eof: false,
            exceeded,
            json_depth: 0,
            in_json_string: false,
            escaped_json_byte: false,
        }
    }

    fn reset_frame(&mut self) {
        self.frame_bytes = 0;
        self.json_depth = 0;
        self.in_json_string = false;
        self.escaped_json_byte = false;
    }

    fn parsed_frame_within_depth(&self) -> bool {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&self.frame) else {
            return true;
        };
        let mut pending = vec![(&value, 1_usize)];
        while let Some((value, depth)) = pending.pop() {
            if depth > ACP_JSON_MAX_DEPTH {
                return false;
            }
            match value {
                serde_json::Value::Array(values) => {
                    pending.extend(values.iter().map(|value| (value, depth + 1)));
                }
                serde_json::Value::Object(values) => {
                    pending.extend(values.values().map(|value| (value, depth + 1)));
                }
                serde_json::Value::Null
                | serde_json::Value::Bool(_)
                | serde_json::Value::Number(_)
                | serde_json::Value::String(_) => {}
            }
        }
        true
    }

    fn finish_frame(&mut self, newline: bool) -> bool {
        if !self.parsed_frame_within_depth() {
            return false;
        }
        self.pending.append(&mut self.frame);
        if newline {
            self.pending.push(b'\n');
        }
        self.reset_frame();
        true
    }

    fn json_byte_within_limit(&mut self, byte: u8) -> bool {
        if self.in_json_string {
            if self.escaped_json_byte {
                self.escaped_json_byte = false;
            } else if byte == b'\\' {
                self.escaped_json_byte = true;
            } else if byte == b'"' {
                self.in_json_string = false;
            }
            return true;
        }
        match byte {
            b'"' => self.in_json_string = true,
            b'{' | b'[' => {
                self.json_depth += 1;
                if self.json_depth > ACP_JSON_MAX_DEPTH {
                    return false;
                }
            }
            b'}' | b']' => self.json_depth = self.json_depth.saturating_sub(1),
            _ => {}
        }
        true
    }

    fn copy_pending(&mut self, output: &mut tokio::io::ReadBuf<'_>) -> bool {
        if self.pending_offset == self.pending.len() {
            self.pending.clear();
            self.pending_offset = 0;
            return false;
        }
        let count = output
            .remaining()
            .min(self.pending.len() - self.pending_offset);
        output.put_slice(&self.pending[self.pending_offset..self.pending_offset + count]);
        self.pending_offset += count;
        if self.pending_offset == self.pending.len() {
            self.pending.clear();
            self.pending_offset = 0;
        }
        count != 0
    }

    fn limit_error() -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "ACP frame exceeds ResourceLimitsV1",
        )
    }
}

impl<R> tokio::io::AsyncRead for AcpFrameLimitedReader<R>
where
    R: tokio::io::AsyncRead + Unpin,
{
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        output: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.as_mut().get_mut();
        if output.remaining() == 0 || this.copy_pending(output) {
            return std::task::Poll::Ready(Ok(()));
        }
        if this.terminal_error {
            return std::task::Poll::Ready(Err(Self::limit_error()));
        }
        if this.inner_eof {
            return std::task::Poll::Ready(Ok(()));
        }

        loop {
            let mut buffer = [0_u8; 8192];
            let mut read = tokio::io::ReadBuf::new(&mut buffer);
            match std::pin::Pin::new(&mut this.inner).poll_read(context, &mut read) {
                std::task::Poll::Pending => return std::task::Poll::Pending,
                std::task::Poll::Ready(Err(error)) => {
                    return std::task::Poll::Ready(Err(error));
                }
                std::task::Poll::Ready(Ok(())) if read.filled().is_empty() => {
                    this.inner_eof = true;
                    if !this.frame.is_empty() && !this.finish_frame(false) {
                        this.terminal_error = true;
                        this.exceeded.store(true, Ordering::Release);
                    }
                    if this.copy_pending(output) {
                        return std::task::Poll::Ready(Ok(()));
                    }
                    return if this.terminal_error {
                        std::task::Poll::Ready(Err(Self::limit_error()))
                    } else {
                        std::task::Poll::Ready(Ok(()))
                    };
                }
                std::task::Poll::Ready(Ok(())) => {}
            }

            {
                let input = read.filled();
                for byte in input.iter().copied() {
                    if byte == b'\n' {
                        if !this.finish_frame(true) {
                            this.terminal_error = true;
                            this.exceeded.store(true, Ordering::Release);
                            break;
                        }
                    } else if this.frame_bytes == ACP_FRAME_MAX_BYTES
                        || !this.json_byte_within_limit(byte)
                    {
                        this.terminal_error = true;
                        this.exceeded.store(true, Ordering::Release);
                        break;
                    } else {
                        this.frame_bytes += 1;
                        this.frame.push(byte);
                    }
                }
                if this.copy_pending(output) {
                    return std::task::Poll::Ready(Ok(()));
                } else if this.terminal_error {
                    return std::task::Poll::Ready(Err(Self::limit_error()));
                }
            }
        }
    }
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

struct AgentReadySession {
    snapshot: Arc<AgentReadySnapshot>,
    route: Option<Arc<ResultRoute>>,
}

enum AgentSessionState {
    Opening,
    Ready(Arc<AgentReadySession>),
    Active(Arc<AgentReadySession>),
    Cancelling(Arc<AgentReadySession>),
    AuthRequired(AgentStartupFailure),
    StartFailed(AgentStartupFailure),
    Broken(AgentSessionFailure),
    Closed,
}

pub(crate) struct AgentSessionSlot {
    state: Mutex<AgentSessionState>,
    changed: Notify,
    cancellation: CancellationToken,
    caller_admission: AtomicBool,
    next_turn_index: AtomicU64,
    turn_registry: Mutex<AgentTurnRegistry>,
    turn_requested: Notify,
    #[cfg(feature = "agent-test-support")]
    turn_registration_gate: Mutex<Option<Arc<TestOpeningGate>>>,
    #[cfg(all(feature = "agent-test-support", test))]
    turn_cancellation_delivery_gate: Mutex<Option<Arc<TestOpeningGate>>>,
    #[cfg(feature = "agent-test-support")]
    turn_terminal_delivery_gate: Mutex<Option<Arc<TestOpeningGate>>>,
    cleanup_complete: AtomicBool,
    cleanup_changed: Notify,
}

enum AgentTurnRegistry {
    Open {
        request: Option<AgentTurnRequest>,
        control: Option<Weak<AgentTurnControl>>,
        submitted_session: Option<SessionId>,
    },
    Terminal(AgentSessionFailure),
}

pub(crate) struct ActAdmissionLease {
    slot: Arc<AgentSessionSlot>,
    claimed: bool,
}

pub(crate) struct SessionTurnLease {
    slot: Arc<AgentSessionSlot>,
    session: Arc<AgentReadySession>,
    claimed: bool,
}

pub(crate) struct SessionTurnMarker {
    slot: Arc<AgentSessionSlot>,
    session: Arc<AgentReadySession>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AgentActError {
    Startup(AgentStartupFailure),
    SessionBroken(AgentSessionFailure),
    SessionClosed,
    CallerCancelled,
}

impl AgentSessionSlot {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(AgentSessionState::Opening),
            changed: Notify::new(),
            cancellation: CancellationToken::new(),
            caller_admission: AtomicBool::new(false),
            next_turn_index: AtomicU64::new(0),
            turn_registry: Mutex::new(AgentTurnRegistry::Open {
                request: None,
                control: None,
                submitted_session: None,
            }),
            turn_requested: Notify::new(),
            #[cfg(feature = "agent-test-support")]
            turn_registration_gate: Mutex::new(None),
            #[cfg(all(feature = "agent-test-support", test))]
            turn_cancellation_delivery_gate: Mutex::new(None),
            #[cfg(feature = "agent-test-support")]
            turn_terminal_delivery_gate: Mutex::new(None),
            cleanup_complete: AtomicBool::new(false),
            cleanup_changed: Notify::new(),
        })
    }

    #[cfg(feature = "agent-test-support")]
    pub(crate) fn inert(profile: &ResolvedAgentProfile) -> Arc<Self> {
        let slot = Self::new();
        slot.commit_ready(
            AgentReadySnapshot {
                pid: std::process::id(),
                session_id: format!("inert-{}", profile.agent.name()),
                agent_info: None,
                agent_capabilities: AgentCapabilities::default(),
                generation: 1,
                server_name: "inert-result-route".to_owned(),
                endpoint: "http://127.0.0.1:0/mcp".to_owned(),
                effective_model: profile.requested_model.clone(),
                effective_effort: profile.requested_effort.clone(),
            },
            None,
        );
        slot.mark_cleanup_complete();
        slot
    }

    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    #[cfg(feature = "agent-test-support")]
    pub(crate) fn install_test_turn_registration_gate(&self, gate: Option<Arc<TestOpeningGate>>) {
        *lock(&self.turn_registration_gate) = gate;
    }

    #[cfg(all(feature = "agent-test-support", test))]
    pub(crate) fn install_test_turn_cancellation_delivery_gate(
        &self,
        gate: Option<Arc<TestOpeningGate>>,
    ) {
        *lock(&self.turn_cancellation_delivery_gate) = gate;
    }

    #[cfg(feature = "agent-test-support")]
    pub(crate) fn install_test_turn_terminal_delivery_gate(
        &self,
        gate: Option<Arc<TestOpeningGate>>,
    ) {
        *lock(&self.turn_terminal_delivery_gate) = gate;
    }

    #[cfg(feature = "agent-test-support")]
    async fn wait_test_turn_registration(&self, cancellation: &CancellationToken) -> bool {
        let gate = lock(&self.turn_registration_gate).clone();
        let Some(gate) = gate else {
            return true;
        };
        tokio::select! {
            () = gate.wait() => {
                gate.mark_completed();
                true
            }
            () = cancellation.cancelled() => false,
        }
    }

    #[cfg(all(feature = "agent-test-support", test))]
    pub(crate) fn wait_test_turn_cancellation_delivery(&self) {
        if let Some(gate) = lock(&self.turn_cancellation_delivery_gate).clone() {
            gate.wait_blocking();
            gate.mark_completed();
        }
    }

    pub(crate) fn try_claim_admission(self: &Arc<Self>) -> Option<ActAdmissionLease> {
        self.caller_admission
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| ActAdmissionLease {
                slot: Arc::clone(self),
                claimed: true,
            })
    }

    pub(crate) async fn run_turn(
        self: &Arc<Self>,
        prompt: String,
        schema: Arc<CompiledActSchema>,
        validation_bridge: Option<Arc<PythonSchemaValidationBridge>>,
        control: Arc<AgentTurnControl>,
    ) -> Result<AgentTurnOutcome, AgentActError> {
        let caller_cancellation = control.caller_cancellation();
        let (session, session_turn, turn_index) = tokio::select! {
            turn = self.claim_session_turn() => turn?,
            () = caller_cancellation.cancelled() => {
                return Err(AgentActError::CallerCancelled);
            }
        };
        let Some(route) = session.route.as_ref().map(Arc::clone) else {
            return Err(AgentActError::SessionBroken(
                self.mark_broken(AgentSessionFailure::result_channel_lost()),
            ));
        };
        if route.generation != session.snapshot.generation
            || route.server_name != session.snapshot.server_name
        {
            return Err(AgentActError::SessionBroken(
                self.mark_broken(AgentSessionFailure::result_channel_lost()),
            ));
        }
        let operation_id = uuid::Uuid::new_v4();
        let armed_result =
            match route.arm_result(operation_id, turn_index, schema, validation_bridge) {
                Ok(armed_result) => armed_result,
                Err(_) => {
                    return Err(AgentActError::SessionBroken(
                        self.mark_broken(AgentSessionFailure::result_channel_lost()),
                    ));
                }
            };
        debug_assert_eq!(armed_result.operation_id(), operation_id);
        debug_assert_eq!(armed_result.turn_index(), turn_index);
        let result_failure = armed_result.failure_notification();
        let (response, outcome) = oneshot::channel();
        if !control.install_armed(armed_result, session_turn, response) {
            return Err(AgentActError::CallerCancelled);
        }
        #[cfg(feature = "agent-test-support")]
        if !self.wait_test_turn_registration(&caller_cancellation).await {
            return Err(AgentActError::CallerCancelled);
        }
        let request = AgentTurnRequest::new(prompt, Arc::clone(&control), result_failure);
        match control.queue_if_armed(request) {
            Ok(true) => {}
            Ok(false) => return Err(AgentActError::CallerCancelled),
            Err(failure) => {
                control.fail_terminal(failure.clone());
                return Err(AgentActError::SessionBroken(failure));
            }
        }
        tokio::select! {
            biased;
            outcome = outcome => outcome.map_err(|_| {
                if caller_cancellation.is_cancelled() {
                    AgentActError::CallerCancelled
                } else {
                    AgentActError::SessionBroken(
                        self.mark_broken(AgentSessionFailure::transport_lost())
                    )
                }
            }),
            () = caller_cancellation.cancelled() => Err(AgentActError::CallerCancelled),
        }
    }

    async fn claim_session_turn(
        self: &Arc<Self>,
    ) -> Result<(Arc<AgentReadySession>, SessionTurnLease, u64), AgentActError> {
        loop {
            let changed = self.changed.notified();
            let session = {
                let mut state = lock(&self.state);
                match &*state {
                    AgentSessionState::Ready(session) => {
                        let session = Arc::clone(session);
                        *state = AgentSessionState::Active(Arc::clone(&session));
                        Some(Ok(session))
                    }
                    AgentSessionState::Opening
                    | AgentSessionState::Active(_)
                    | AgentSessionState::Cancelling(_) => None,
                    AgentSessionState::AuthRequired(failure)
                    | AgentSessionState::StartFailed(failure) => {
                        Some(Err(AgentActError::Startup(failure.clone())))
                    }
                    AgentSessionState::Broken(failure) => {
                        Some(Err(AgentActError::SessionBroken(failure.clone())))
                    }
                    AgentSessionState::Closed => Some(Err(AgentActError::SessionClosed)),
                }
            };
            if let Some(session) = session {
                let session = session?;
                let turn_index = match self.next_turn_index.fetch_update(
                    Ordering::AcqRel,
                    Ordering::Acquire,
                    |value| value.checked_add(1),
                ) {
                    Ok(previous) => previous + 1,
                    Err(_) => {
                        return Err(AgentActError::SessionBroken(
                            self.mark_broken(AgentSessionFailure::protocol_violation()),
                        ));
                    }
                };
                let session_turn = SessionTurnLease {
                    slot: Arc::clone(self),
                    session: Arc::clone(&session),
                    claimed: true,
                };
                return Ok((session, session_turn, turn_index));
            }
            changed.await;
        }
    }

    pub(crate) async fn next_turn(&self) -> Option<AgentTurnRequest> {
        loop {
            let requested = self.turn_requested.notified();
            let next = {
                let mut registry = lock(&self.turn_registry);
                match &mut *registry {
                    AgentTurnRegistry::Open { request, .. } => request.take().map(Some),
                    AgentTurnRegistry::Terminal(_) => Some(None),
                }
            };
            if let Some(request) = next {
                return request;
            }
            tokio::select! {
                () = requested => {}
                () = self.cancellation.cancelled() => return None,
            }
        }
    }

    #[cfg(feature = "agent-test-support")]
    pub(crate) fn has_queued_turn_for_test(&self) -> bool {
        matches!(
            &*lock(&self.turn_registry),
            AgentTurnRegistry::Open {
                request: Some(_),
                ..
            }
        )
    }

    pub(crate) fn queue_turn(
        &self,
        request: AgentTurnRequest,
        control: &Arc<AgentTurnControl>,
    ) -> Result<(), AgentSessionFailure> {
        let queued = {
            let mut registry = lock(&self.turn_registry);
            match &mut *registry {
                AgentTurnRegistry::Open {
                    request: pending,
                    control: active,
                    submitted_session,
                } if pending.is_none()
                    && active.as_ref().and_then(Weak::upgrade).is_none()
                    && submitted_session.is_none() =>
                {
                    *active = Some(Arc::downgrade(control));
                    *pending = Some(request);
                    Ok(())
                }
                AgentTurnRegistry::Open { .. } => Err(AgentSessionFailure::protocol_violation()),
                AgentTurnRegistry::Terminal(failure) => Err(failure.clone()),
            }
        };
        if queued.is_ok() {
            self.turn_requested.notify_one();
        }
        queued
    }

    pub(crate) fn register_submitted_turn(
        &self,
        session_id: &SessionId,
        control: &Arc<AgentTurnControl>,
    ) -> Result<(), AgentSessionFailure> {
        let mut registry = lock(&self.turn_registry);
        match &mut *registry {
            AgentTurnRegistry::Open {
                request,
                control: active,
                submitted_session,
            } if request.is_none()
                && active
                    .as_ref()
                    .is_some_and(|current| Weak::ptr_eq(current, &Arc::downgrade(control)))
                && submitted_session.is_none() =>
            {
                *submitted_session = Some(session_id.clone());
                Ok(())
            }
            AgentTurnRegistry::Open { .. } => Err(AgentSessionFailure::protocol_violation()),
            AgentTurnRegistry::Terminal(failure) => Err(failure.clone()),
        }
    }

    pub(crate) fn clear_turn_control(&self, control: &AgentTurnControl) {
        let mut registry = lock(&self.turn_registry);
        if let AgentTurnRegistry::Open {
            request,
            control: active,
            submitted_session,
        } = &mut *registry
            && active
                .as_ref()
                .is_some_and(|current| std::ptr::eq(current.as_ptr(), control))
        {
            *request = None;
            *active = None;
            *submitted_session = None;
        }
    }

    fn submitted_turn(&self, session_id: &SessionId) -> Option<Arc<AgentTurnControl>> {
        let registry = lock(&self.turn_registry);
        let AgentTurnRegistry::Open {
            control,
            submitted_session,
            ..
        } = &*registry
        else {
            return None;
        };
        submitted_session
            .as_ref()
            .filter(|current| *current == session_id)
            .and_then(|_| control.as_ref().and_then(Weak::upgrade))
    }

    pub(crate) fn mark_broken(&self, failure: AgentSessionFailure) -> AgentSessionFailure {
        let mut state = lock(&self.state);
        match &*state {
            AgentSessionState::Broken(existing) => existing.clone(),
            AgentSessionState::Ready(_)
            | AgentSessionState::Active(_)
            | AgentSessionState::Cancelling(_) => {
                *state = AgentSessionState::Broken(failure.clone());
                self.changed.notify_waiters();
                failure
            }
            AgentSessionState::Opening
            | AgentSessionState::AuthRequired(_)
            | AgentSessionState::StartFailed(_)
            | AgentSessionState::Closed => failure,
        }
    }

    pub(crate) fn commit_turn_transition<R>(
        &self,
        transition: impl FnOnce(Option<AgentSessionFailure>) -> (R, Option<AgentSessionFailure>),
    ) -> R {
        let mut state = lock(&self.state);
        let existing = match &*state {
            AgentSessionState::Broken(failure) => Some(failure.clone()),
            _ => None,
        };
        let (result, proposed_failure) = transition(existing.clone());
        let failure = existing.or(proposed_failure);
        if matches!(
            *state,
            AgentSessionState::Ready(_)
                | AgentSessionState::Active(_)
                | AgentSessionState::Cancelling(_)
        ) && let Some(failure) = &failure
        {
            *state = AgentSessionState::Broken(failure.clone());
            self.changed.notify_waiters();
        }
        result
    }

    fn mark_acp_resource_limit(&self, phase: &'static str) {
        self.commit_terminal_failure(
            AgentStartupFailure::start(
                "resource_limit",
                phase,
                "agent sent an ACP frame above ResourceLimitsV1",
            ),
            AgentSessionFailure::resource_limit(),
        );
    }

    pub(crate) fn cancel(&self) {
        self.cancellation.cancel();
        let mut state = lock(&self.state);
        if matches!(
            *state,
            AgentSessionState::Opening
                | AgentSessionState::Ready(_)
                | AgentSessionState::Active(_)
                | AgentSessionState::Cancelling(_)
        ) {
            *state = AgentSessionState::Closed;
            self.changed.notify_waiters();
        }
        drop(state);
        if let AgentTurnRegistry::Open {
            request,
            control,
            submitted_session,
        } = &mut *lock(&self.turn_registry)
        {
            *request = None;
            *control = None;
            *submitted_session = None;
        }
        self.turn_requested.notify_waiters();
    }

    #[cfg(feature = "agent-test-support")]
    pub(crate) async fn readiness(&self) -> Result<Arc<AgentReadySnapshot>, AgentStartupFailure> {
        loop {
            let changed = self.changed.notified();
            match &*lock(&self.state) {
                AgentSessionState::Opening => {}
                AgentSessionState::Ready(session) | AgentSessionState::Active(session) => {
                    return Ok(Arc::clone(&session.snapshot));
                }
                AgentSessionState::Cancelling(_) => {}
                AgentSessionState::AuthRequired(failure)
                | AgentSessionState::StartFailed(failure) => return Err(failure.clone()),
                AgentSessionState::Broken(failure) => {
                    return Err(AgentStartupFailure::start(
                        failure.code,
                        "initialize",
                        failure.message,
                    ));
                }
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
            AgentSessionState::Active(_) => "active",
            AgentSessionState::Cancelling(_) => "cancelling",
            AgentSessionState::AuthRequired(_) => "auth_required",
            AgentSessionState::StartFailed(_) => "start_failed",
            AgentSessionState::Broken(_) => "broken",
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

    fn commit_ready(&self, snapshot: AgentReadySnapshot, route: Option<Arc<ResultRoute>>) {
        let mut state = lock(&self.state);
        if matches!(*state, AgentSessionState::Opening) {
            *state = AgentSessionState::Ready(Arc::new(AgentReadySession {
                snapshot: Arc::new(snapshot),
                route,
            }));
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

    fn commit_connection_loss(&self, startup_failure: AgentStartupFailure) {
        self.commit_terminal_failure(startup_failure, AgentSessionFailure::transport_lost());
    }

    fn commit_terminal_failure(
        &self,
        startup_failure: AgentStartupFailure,
        failure: AgentSessionFailure,
    ) {
        let delivery = {
            let mut state = lock(&self.state);
            let failure = match &*state {
                AgentSessionState::Opening => {
                    *state = AgentSessionState::StartFailed(startup_failure);
                    self.changed.notify_waiters();
                    return;
                }
                AgentSessionState::Ready(_)
                | AgentSessionState::Active(_)
                | AgentSessionState::Cancelling(_) => failure,
                AgentSessionState::AuthRequired(_)
                | AgentSessionState::StartFailed(_)
                | AgentSessionState::Closed => return,
                AgentSessionState::Broken(existing) => existing.clone(),
            };
            let (failure, control) = self.freeze_turn_registry(failure);
            let delivery = control.map(|control| {
                let cleanup = control.prepare_terminal_delivery(failure.clone());
                (control, cleanup)
            });
            *state = AgentSessionState::Broken(failure.clone());
            self.changed.notify_waiters();
            delivery
        };
        self.turn_requested.notify_waiters();
        #[cfg(feature = "agent-test-support")]
        if delivery.is_some()
            && let Some(gate) = lock(&self.turn_terminal_delivery_gate).clone()
        {
            gate.wait_blocking();
            gate.mark_completed();
        }
        if let Some((control, cleanup)) = delivery {
            control.finish_terminal_delivery(cleanup);
        }
    }

    fn freeze_turn_registry(
        &self,
        failure: AgentSessionFailure,
    ) -> (AgentSessionFailure, Option<Arc<AgentTurnControl>>) {
        let mut registry = lock(&self.turn_registry);
        match &mut *registry {
            AgentTurnRegistry::Terminal(existing) => (existing.clone(), None),
            AgentTurnRegistry::Open {
                request, control, ..
            } => {
                let queued = request.take().map(AgentTurnRequest::into_control);
                let active = control.take().and_then(|control| control.upgrade());
                let active = queued.or(active);
                *registry = AgentTurnRegistry::Terminal(failure.clone());
                (failure, active)
            }
        }
    }

    pub(crate) fn commit_transport_loss(&self) {
        self.commit_connection_loss(AgentStartupFailure::start(
            "spawn_failed",
            "spawn",
            "agent connection closed",
        ));
    }
}

impl ActAdmissionLease {
    fn release(&mut self) {
        if self.claimed {
            let previous = self.slot.caller_admission.swap(false, Ordering::AcqRel);
            assert!(previous, "an act admission lease is released once");
            self.claimed = false;
        }
    }
}

impl Drop for ActAdmissionLease {
    fn drop(&mut self) {
        self.release();
    }
}

impl SessionTurnLease {
    pub(crate) fn cancelling_marker(&self) -> SessionTurnMarker {
        SessionTurnMarker {
            slot: Arc::clone(&self.slot),
            session: Arc::clone(&self.session),
        }
    }

    pub(crate) fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if !self.claimed {
            return;
        }
        let mut state = lock(&self.slot.state);
        if matches!(
            &*state,
            AgentSessionState::Active(current) | AgentSessionState::Cancelling(current)
                if Arc::ptr_eq(current, &self.session)
        ) {
            *state = AgentSessionState::Ready(Arc::clone(&self.session));
            self.slot.changed.notify_waiters();
        }
        self.claimed = false;
    }
}

impl SessionTurnMarker {
    pub(crate) fn mark_cancelling(self) {
        let mut state = lock(&self.slot.state);
        if matches!(
            &*state,
            AgentSessionState::Active(current) if Arc::ptr_eq(current, &self.session)
        ) {
            *state = AgentSessionState::Cancelling(Arc::clone(&self.session));
            self.slot.changed.notify_waiters();
        }
    }
}

impl Drop for SessionTurnLease {
    fn drop(&mut self) {
        self.release_inner();
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
    let frame_limit_exceeded = Arc::new(AtomicBool::new(false));
    let frame_limit_for_connection = Arc::clone(&frame_limit_exceeded);
    let opening_phase = Arc::new(AtomicU8::new(OpeningPhase::Initialize as u8));
    let opening_phase_for_connection = Arc::clone(&opening_phase);
    let prompt_response_provenance = Arc::new(PromptResponseProvenance::default());
    let provenance_for_handler = Arc::clone(&prompt_response_provenance);
    let slot_for_permissions = slot.clone();
    let stdout = AcpFrameLimitedReader::new(stdout, Arc::clone(&frame_limit_exceeded));
    let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
    let connection = Client
        .builder()
        .name("troupe")
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _connection| {
                let Some(slot) = slot_for_permissions.upgrade() else {
                    return responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ));
                };
                let Some(control) = slot.submitted_turn(&request.session_id) else {
                    return responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ));
                };
                control.respond_permission(&request, responder)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_dispatch(
            async move |dispatch: Dispatch, _connection: ConnectionTo<Agent>| match dispatch {
                Dispatch::Response(result, router) => {
                    if router.method() == "session/prompt" && result.is_err() {
                        provenance_for_handler.record_remote_error(router.id().clone());
                    }
                    router.route_with_result(result)?;
                    Ok(Handled::Yes)
                }
                message => Ok(Handled::No {
                    message,
                    retry: false,
                }),
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_with(
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
                    &opening_phase_for_connection,
                )
                .await;
                match result {
                    Ok(session_id) => {
                        if let Some(slot) = slot_for_connection.upgrade() {
                            run_agent_turn_worker(
                                &connection,
                                slot,
                                session_id,
                                &prompt_response_provenance,
                                &cancellation_for_connection,
                                #[cfg(feature = "agent-test-support")]
                                command_for_connection.turn_gates.clone(),
                            )
                            .await;
                        }
                    }
                    Err(failure) => {
                        if frame_limit_for_connection.load(Ordering::Acquire) {
                            commit_acp_resource_limit(
                                &slot_for_connection,
                                OpeningPhase::load(&opening_phase_for_connection).name(),
                            );
                        } else {
                            commit_failure(&slot_for_connection, failure);
                        }
                    }
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
        if frame_limit_exceeded.load(Ordering::Acquire) {
            commit_acp_resource_limit(&slot, OpeningPhase::load(&opening_phase).name());
        } else {
            commit_connection_loss(
                &slot,
                AgentStartupFailure::start(
                    "spawn_failed",
                    "spawn",
                    "agent process exited during startup",
                ),
            );
        }
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
    opening_phase: &AtomicU8,
) -> Result<SessionId, AgentStartupFailure> {
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
    OpeningPhase::SessionNew.store(opening_phase);
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

    OpeningPhase::Configure.store(opening_phase);
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
                    "configure",
                    "agent session was closed before configuration readiness",
                ));
            }
        }
    }
    OpeningPhase::McpReady.store(opening_phase);
    #[cfg(feature = "agent-test-support")]
    if let Some(gate) = &command.configuration_ready_gate {
        gate.mark_completed();
    }
    route.wait_ready(cancellation).await?;
    revalidate_workspace(&profile.workspace, "session_new")?;
    let Some(slot) = slot.upgrade() else {
        return Ok(session.session_id);
    };
    let session_id = session.session_id.clone();
    slot.commit_ready(
        AgentReadySnapshot {
            pid,
            session_id: session.session_id.to_string(),
            agent_info,
            agent_capabilities,
            generation: route.generation,
            server_name: route.server_name.clone(),
            endpoint: endpoint.to_owned(),
            effective_model,
            effective_effort,
        },
        Some(Arc::clone(&route)),
    );
    Ok(session_id)
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

fn commit_connection_loss(slot: &Weak<AgentSessionSlot>, failure: AgentStartupFailure) {
    if let Some(slot) = slot.upgrade() {
        slot.commit_connection_loss(failure);
    }
}

fn commit_acp_resource_limit(slot: &Weak<AgentSessionSlot>, phase: &'static str) {
    if let Some(slot) = slot.upgrade() {
        slot.mark_acp_resource_limit(phase);
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt as _;
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

    #[tokio::test]
    async fn acp_frame_reader_enforces_the_exact_sixteen_mibibyte_boundary() {
        async fn read_frame(size: usize) -> (std::io::Result<Vec<u8>>, bool) {
            let mut wire = vec![b' '; size];
            wire.push(b'\n');
            let exceeded = Arc::new(AtomicBool::new(false));
            let mut reader = AcpFrameLimitedReader::new(wire.as_slice(), Arc::clone(&exceeded));
            let mut output = Vec::new();
            let result = reader.read_to_end(&mut output).await.map(|_| output);
            (result, exceeded.load(Ordering::Acquire))
        }

        for size in [ACP_FRAME_MAX_BYTES - 1, ACP_FRAME_MAX_BYTES] {
            let (result, exceeded) = read_frame(size).await;
            assert_eq!(result.unwrap().len(), size + 1);
            assert!(!exceeded);
        }
        let (result, exceeded) = read_frame(ACP_FRAME_MAX_BYTES + 1).await;
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
        assert!(exceeded);

        let exceeded = Arc::new(AtomicBool::new(false));
        let mut reader =
            AcpFrameLimitedReader::new(b"first\nsecond\n".as_slice(), Arc::clone(&exceeded));
        let mut output = Vec::new();
        reader.read_to_end(&mut output).await.unwrap();
        assert_eq!(output, b"first\nsecond\n");
        assert!(!exceeded.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn acp_frame_reader_enforces_protocol_depth_63_64_65() {
        let nested_frame = |depth: usize| {
            let mut value = serde_json::json!(null);
            for _ in 1..depth {
                value = serde_json::json!({"nested": value});
            }
            let mut frame = serde_json::to_vec(&value).unwrap();
            frame.push(b'\n');
            frame
        };

        for depth in [63, 64] {
            let exceeded = Arc::new(AtomicBool::new(false));
            let frame = nested_frame(depth);
            let mut reader = AcpFrameLimitedReader::new(frame.as_slice(), Arc::clone(&exceeded));
            let mut output = Vec::new();
            reader.read_to_end(&mut output).await.unwrap();
            assert_eq!(output, frame);
            assert!(!exceeded.load(Ordering::Acquire));
        }

        let exceeded = Arc::new(AtomicBool::new(false));
        let frame = nested_frame(65);
        let mut reader = AcpFrameLimitedReader::new(frame.as_slice(), Arc::clone(&exceeded));
        let mut output = Vec::new();
        assert_eq!(
            reader.read_to_end(&mut output).await.unwrap_err().kind(),
            std::io::ErrorKind::InvalidData,
        );
        assert!(exceeded.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn turn_index_overflow_is_internalized_as_protocol_violation() {
        let slot = AgentSessionSlot::new();
        slot.commit_ready(
            AgentReadySnapshot {
                pid: std::process::id(),
                session_id: "overflow-test".to_owned(),
                agent_info: None,
                agent_capabilities: AgentCapabilities::default(),
                generation: 1,
                server_name: "overflow-test".to_owned(),
                endpoint: "http://127.0.0.1:1/mcp".to_owned(),
                effective_model: "test-model".to_owned(),
                effective_effort: None,
            },
            None,
        );
        slot.next_turn_index.store(u64::MAX, Ordering::Release);

        let error = match slot.claim_session_turn().await {
            Ok(_) => panic!("an exhausted internal turn index was reused"),
            Err(error) => error,
        };
        let AgentActError::SessionBroken(failure) = error else {
            panic!("turn index overflow escaped through an unplanned public branch");
        };
        assert_eq!(failure.code, "protocol_violation");
        assert!(matches!(
            &*lock(&slot.state),
            AgentSessionState::Broken(stored) if stored.code == "protocol_violation"
        ));
    }
}
