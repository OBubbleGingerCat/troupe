use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, ClientCapabilities, ClientSessionCapabilities, ErrorCode, Implementation,
    InitializeRequest, McpCapabilities, NewSessionRequest, NewSessionResponse, RequestId,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SessionConfigKind, SessionConfigOption, SessionConfigOptionValue, SessionConfigSelect,
    SessionConfigSelectOptions, SessionId, SessionModeState, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, SetSessionModeRequest,
};
use agent_client_protocol::{
    Agent, Client, ConnectionTo, Dispatch, Handled, Lines, RawJsonRpcMessage, TransportBatchEntry,
    TransportFrame,
};
use futures::{AsyncBufReadExt as _, AsyncWriteExt as _, StreamExt as _};
use tokio::io::AsyncReadExt as _;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{Notify, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
use tokio_util::sync::CancellationToken;

use crate::act_schema::CompiledActSchema;
use crate::agent_adapter::{AcpAgentAdapter, agent_adapter};
use crate::agent_error::{AgentSessionFailure, AgentStartupFailure};
#[cfg(feature = "agent-test-support")]
use crate::agent_launch::TestOpeningGate;
use crate::agent_launch::{
    AgentLaunchSpec, OpeningRequestPhaseV1, ResolvedAgentCommand, ResolvedModeApplication,
};
use crate::agent_profile::{ResolvedAgentProfile, WorkspaceLeaseV1};
use crate::agent_turn::{
    AgentTurnControl, AgentTurnOutcome, AgentTurnRequest, PromptResponseProvenance,
    run_agent_turn_worker,
};
use crate::fork_fd_registry::{ForkExecGuard, ForkTracked};
use crate::result_mcp::{ResultMcpService, ResultRoute, fill_secure_random};
use crate::schema_validation_bridge::PythonSchemaValidationBridge;

const ACP_FRAME_MAX_BYTES: usize = 16 * 1024 * 1024;
const ACP_JSON_MAX_DEPTH: usize = 64;
const OPENING_CRASH_LOOP_THRESHOLD: u8 = 3;

fn opening_retry_window_ms(ordinal: u32) -> (u64, u64) {
    let exponent = ordinal.min(7);
    let window = (250_u64 << exponent).min(30_000);
    (window.div_ceil(2), window)
}

fn sample_opening_retry_delay_ms<E>(
    ordinal: u32,
    mut next_random_word: impl FnMut() -> Result<u64, E>,
) -> Result<u64, E> {
    let (lower, upper) = opening_retry_window_ms(ordinal);
    let span = upper - lower + 1;
    let rejected_prefix = span.wrapping_neg() % span;
    loop {
        let word = next_random_word()?;
        if word >= rejected_prefix {
            return Ok(lower + word % span);
        }
    }
}

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
    terminal_failure: Option<AcpFrameFailure>,
    inner_eof: bool,
    exceeded: Arc<AtomicBool>,
    protocol_violation: Arc<ProtocolViolationBoundary>,
    json_depth: usize,
    in_json_string: bool,
    escaped_json_byte: bool,
}

#[derive(Clone, Copy)]
enum AcpFrameFailure {
    ResourceLimit,
    ProtocolViolation,
}

impl<R> AcpFrameLimitedReader<R> {
    fn new(
        inner: R,
        exceeded: Arc<AtomicBool>,
        protocol_violation: Arc<ProtocolViolationBoundary>,
    ) -> Self {
        Self {
            inner,
            frame_bytes: 0,
            frame: Vec::new(),
            pending: Vec::new(),
            pending_offset: 0,
            terminal_failure: None,
            inner_eof: false,
            exceeded,
            protocol_violation,
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

    fn parsed_frame_within_depth(&self) -> Result<bool, AcpFrameFailure> {
        let value = serde_json::from_slice::<serde_json::Value>(&self.frame)
            .map_err(|_| AcpFrameFailure::ProtocolViolation)?;
        let mut pending = vec![(&value, 1_usize)];
        while let Some((value, depth)) = pending.pop() {
            if depth > ACP_JSON_MAX_DEPTH {
                return Ok(false);
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
        let wire =
            std::str::from_utf8(&self.frame).map_err(|_| AcpFrameFailure::ProtocolViolation)?;
        match TransportFrame::parse_json(wire) {
            TransportFrame::Single(_) => {}
            TransportFrame::Malformed { .. } => {
                return Err(AcpFrameFailure::ProtocolViolation);
            }
            TransportFrame::Batch(batch)
                if batch
                    .entries()
                    .any(|entry| matches!(entry, TransportBatchEntry::Malformed { .. })) =>
            {
                return Err(AcpFrameFailure::ProtocolViolation);
            }
            TransportFrame::Batch(_) => {}
        }
        Ok(true)
    }

    fn finish_frame(&mut self, newline: bool) -> Result<(), AcpFrameFailure> {
        if !self.parsed_frame_within_depth()? {
            return Err(AcpFrameFailure::ResourceLimit);
        }
        self.pending.append(&mut self.frame);
        if newline {
            self.pending.push(b'\n');
        }
        self.reset_frame();
        Ok(())
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

    fn record_failure(&mut self, failure: AcpFrameFailure) {
        self.terminal_failure = Some(failure);
        match failure {
            AcpFrameFailure::ResourceLimit => self.exceeded.store(true, Ordering::Release),
            AcpFrameFailure::ProtocolViolation => {
                self.protocol_violation.mark_observed();
            }
        }
    }

    fn failure_error(failure: AcpFrameFailure) -> std::io::Error {
        let message = match failure {
            AcpFrameFailure::ResourceLimit => "ACP frame exceeds ResourceLimitsV1",
            AcpFrameFailure::ProtocolViolation => "ACP frame is not valid JSON-RPC",
        };
        std::io::Error::new(std::io::ErrorKind::InvalidData, message)
    }
}

struct AcpResponseTracker {
    pending: Mutex<HashMap<RequestId, Arc<str>>>,
    predeferred_responses: Mutex<HashSet<RequestId>>,
    response_flush_waiters: Mutex<HashMap<RequestId, Vec<oneshot::Sender<()>>>>,
    protocol_violation: Arc<ProtocolViolationBoundary>,
}

impl AcpResponseTracker {
    fn new(protocol_violation: Arc<ProtocolViolationBoundary>) -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            predeferred_responses: Mutex::new(HashSet::new()),
            response_flush_waiters: Mutex::new(HashMap::new()),
            protocol_violation,
        }
    }

    fn wait_for_response_flush(&self, id: RequestId) -> oneshot::Receiver<()> {
        let (sender, receiver) = oneshot::channel();
        lock(&self.response_flush_waiters)
            .entry(id)
            .or_default()
            .push(sender);
        receiver
    }

    fn observe_outgoing_flushed(&self, line: &str) {
        let frame = TransportFrame::parse_json(line);
        let observe_message = |message: &RawJsonRpcMessage| {
            let RawJsonRpcMessage::Response(response) = message else {
                return;
            };
            let id = match response {
                agent_client_protocol::schema::v1::Response::Result { id, .. }
                | agent_client_protocol::schema::v1::Response::Error { id, .. } => id,
            };
            if lock(&self.predeferred_responses).remove(id) {
                self.protocol_violation.complete_deferred_response();
            }
            if let Some(waiters) = lock(&self.response_flush_waiters).remove(id) {
                for waiter in waiters {
                    let _ = waiter.send(());
                }
            }
        };
        match frame {
            TransportFrame::Single(message) => observe_message(&message),
            TransportFrame::Batch(batch) => {
                for entry in batch.entries() {
                    if let TransportBatchEntry::Message(message) = entry {
                        observe_message(message);
                    }
                }
            }
            TransportFrame::Malformed { .. } => {}
        }
    }

    #[cfg(feature = "agent-test-support")]
    fn outgoing_contains_response(line: &str) -> bool {
        match TransportFrame::parse_json(line) {
            TransportFrame::Single(RawJsonRpcMessage::Response(_)) => true,
            TransportFrame::Batch(batch) => batch.entries().any(|entry| {
                matches!(
                    entry,
                    TransportBatchEntry::Message(RawJsonRpcMessage::Response(_))
                )
            }),
            TransportFrame::Single(_) | TransportFrame::Malformed { .. } => false,
        }
    }

    fn observe_outgoing(&self, line: &str) -> std::io::Result<()> {
        self.observe_with_provenance(line, AcpFrameDirection::Outgoing)
    }

    fn observe_incoming(&self, line: &str) -> std::io::Result<()> {
        self.observe_with_provenance(line, AcpFrameDirection::Incoming)
    }

    fn response_was_predeferred(&self, id: &RequestId) -> bool {
        lock(&self.predeferred_responses).contains(id)
    }

    fn observe_with_provenance(
        &self,
        line: &str,
        direction: AcpFrameDirection,
    ) -> std::io::Result<()> {
        let result = self.observe(line, direction);
        if result.is_err() {
            self.protocol_violation.mark_observed();
        }
        result
    }

    fn observe(&self, line: &str, direction: AcpFrameDirection) -> std::io::Result<()> {
        let frame = TransportFrame::parse_json(line);
        let mut pending = lock(&self.pending);
        let mut prompt_response_seen = false;
        let mut observe_message = |message: &RawJsonRpcMessage| -> std::io::Result<()> {
            match (direction, message) {
                (AcpFrameDirection::Outgoing, RawJsonRpcMessage::Request(request)) => {
                    if pending
                        .insert(request.id.clone(), Arc::clone(&request.method))
                        .is_some()
                    {
                        return Err(acp_protocol_error("duplicate outgoing ACP request id"));
                    }
                }
                (AcpFrameDirection::Incoming, RawJsonRpcMessage::Response(response)) => {
                    let response_id = match response {
                        agent_client_protocol::schema::v1::Response::Result { id, .. }
                        | agent_client_protocol::schema::v1::Response::Error { id, .. } => id,
                    };
                    let method = pending.remove(response_id).ok_or_else(|| {
                        acp_protocol_error("unknown or duplicate ACP response id")
                    })?;
                    prompt_response_seen |= method.as_ref() == "session/prompt";
                }
                (AcpFrameDirection::Incoming, RawJsonRpcMessage::Request(request))
                    if prompt_response_seen =>
                {
                    if lock(&self.predeferred_responses).insert(request.id.clone()) {
                        self.protocol_violation.begin_deferred_response();
                    }
                }
                _ => {}
            }
            Ok(())
        };
        match frame {
            TransportFrame::Single(message) => observe_message(&message),
            TransportFrame::Batch(batch) => {
                for entry in batch.entries() {
                    let TransportBatchEntry::Message(message) = entry else {
                        return Err(acp_protocol_error("malformed ACP batch entry"));
                    };
                    observe_message(message)?;
                }
                Ok(())
            }
            TransportFrame::Malformed { .. } => Err(acp_protocol_error("malformed ACP frame")),
        }
    }
}

#[derive(Default)]
pub(crate) struct ProtocolViolationBoundary {
    observed: AtomicBool,
    deferred_responses: AtomicU64,
    settlement_changed: Notify,
}

impl ProtocolViolationBoundary {
    fn mark_observed(&self) {
        self.observed.store(true, Ordering::Release);
    }

    pub(crate) fn is_observed(&self) -> bool {
        self.observed.load(Ordering::Acquire)
    }

    fn begin_deferred_response(&self) {
        self.deferred_responses
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                pending.checked_add(1)
            })
            .expect("the deferred ACP response counter cannot exhaust");
        self.mark_observed();
    }

    fn complete_deferred_response(&self) -> bool {
        let previous = self.deferred_responses.fetch_sub(1, Ordering::AcqRel);
        assert_ne!(previous, 0, "a deferred ACP response must be registered");
        self.settlement_changed.notify_waiters();
        previous == 1
    }

    pub(crate) async fn wait_for_settlement_evidence(&self) -> bool {
        loop {
            let changed = self.settlement_changed.notified();
            if !self.is_observed() {
                return false;
            }
            if self.deferred_responses.load(Ordering::Acquire) == 0 {
                return true;
            }
            changed.await;
        }
    }

    fn settlement_evidence_is_ready(&self) -> bool {
        self.is_observed() && self.deferred_responses.load(Ordering::Acquire) == 0
    }
}

fn defer_protocol_violation_until_response_flush(
    connection: &ConnectionTo<Agent>,
    response_flushed: oneshot::Receiver<()>,
    already_registered: bool,
    slot: Weak<AgentSessionSlot>,
    phase: &'static str,
    boundary: Arc<ProtocolViolationBoundary>,
    protocol_violation: CancellationToken,
) -> Result<(), agent_client_protocol::Error> {
    if !already_registered {
        boundary.begin_deferred_response();
    }
    let boundary_for_task = Arc::clone(&boundary);
    let spawn = connection.spawn(async move {
        let flushed = response_flushed.await.is_ok();
        let settlement_ready = if already_registered {
            boundary_for_task.settlement_evidence_is_ready()
        } else {
            boundary_for_task.complete_deferred_response()
        };
        if flushed && settlement_ready {
            if let Some(slot) = slot.upgrade() {
                slot.commit_protocol_violation(phase);
            }
            protocol_violation.cancel();
        }
        Ok(())
    });
    if spawn.is_err() && !already_registered {
        boundary.complete_deferred_response();
    }
    spawn
}

#[derive(Clone, Copy)]
enum AcpFrameDirection {
    Incoming,
    Outgoing,
}

fn acp_protocol_error(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
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
        if let Some(failure) = this.terminal_failure {
            return std::task::Poll::Ready(Err(Self::failure_error(failure)));
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
                    if !this.frame.is_empty()
                        && let Err(failure) = this.finish_frame(false)
                    {
                        this.record_failure(failure);
                    }
                    if this.copy_pending(output) {
                        return std::task::Poll::Ready(Ok(()));
                    }
                    return if let Some(failure) = this.terminal_failure {
                        std::task::Poll::Ready(Err(Self::failure_error(failure)))
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
                        if let Err(failure) = this.finish_frame(true) {
                            this.record_failure(failure);
                            break;
                        }
                    } else if this.frame_bytes == ACP_FRAME_MAX_BYTES
                        || !this.json_byte_within_limit(byte)
                    {
                        this.record_failure(AcpFrameFailure::ResourceLimit);
                        break;
                    } else {
                        this.frame_bytes += 1;
                        this.frame.push(byte);
                    }
                }
                if this.copy_pending(output) {
                    return std::task::Poll::Ready(Ok(()));
                } else if let Some(failure) = this.terminal_failure {
                    return std::task::Poll::Ready(Err(Self::failure_error(failure)));
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
    configuration_monitor: Option<Arc<OpeningConfigurationMonitor>>,
}

#[derive(Clone)]
struct ReadyConfigurationContract {
    session_id: SessionId,
    mode_application: ResolvedModeApplication,
    model_config_id: &'static str,
    effort_config_id: &'static str,
    requested_model: String,
    effective_effort: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ModeObservationSource {
    CurrentModeUpdate,
    ConfigOptionSnapshot,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
enum PostReadyModeTransition {
    #[default]
    Stable,
    AwaitingTemporarySnapshot(String),
    Temporary(String),
    AwaitingBaselineSnapshot(String),
}

impl PostReadyModeTransition {
    fn observe(
        &mut self,
        source: ModeObservationSource,
        expected: &str,
        observed: &str,
        turn_is_active: bool,
    ) -> bool {
        if !turn_is_active && !self.is_stable() {
            return false;
        }
        let next = match (self.clone(), source) {
            (Self::Stable, _) if observed == expected => Self::Stable,
            (Self::Stable, ModeObservationSource::CurrentModeUpdate) => {
                Self::AwaitingTemporarySnapshot(observed.to_owned())
            }
            (
                Self::AwaitingTemporarySnapshot(temporary),
                ModeObservationSource::CurrentModeUpdate,
            ) if observed == temporary => Self::AwaitingTemporarySnapshot(temporary),
            (
                Self::AwaitingTemporarySnapshot(temporary),
                ModeObservationSource::ConfigOptionSnapshot,
            ) if observed == temporary => Self::Temporary(temporary),
            (Self::Temporary(temporary), _) if observed == temporary => Self::Temporary(temporary),
            (Self::Temporary(temporary), ModeObservationSource::CurrentModeUpdate)
                if observed == expected =>
            {
                Self::AwaitingBaselineSnapshot(temporary)
            }
            (
                Self::AwaitingBaselineSnapshot(temporary),
                ModeObservationSource::CurrentModeUpdate,
            ) if observed == expected => Self::AwaitingBaselineSnapshot(temporary),
            (Self::AwaitingBaselineSnapshot(_), ModeObservationSource::ConfigOptionSnapshot)
                if observed == expected =>
            {
                Self::Stable
            }
            _ => return false,
        };
        *self = next;
        true
    }

    fn is_stable(&self) -> bool {
        matches!(self, Self::Stable)
    }
}

#[derive(Clone)]
struct PendingReadyConfigurationContract {
    session_id: SessionId,
    mode_application: ResolvedModeApplication,
    model_config_id: &'static str,
    effort_config_id: &'static str,
    effort_option_optional_when_unspecified: bool,
    requested_model: String,
    requested_effort: Option<String>,
}

#[derive(Clone)]
struct ModeConfigurationContract {
    session_id: SessionId,
    mode_application: ResolvedModeApplication,
}

enum PendingConfigurationContract {
    Mode(ModeConfigurationContract),
    Ready(PendingReadyConfigurationContract),
}

impl PendingConfigurationContract {
    fn response_method(&self) -> &'static str {
        match self {
            Self::Mode(ModeConfigurationContract {
                mode_application: ResolvedModeApplication::LegacySessionMode { .. },
                ..
            }) => "session/set_mode",
            Self::Mode(ModeConfigurationContract {
                mode_application: ResolvedModeApplication::SessionConfigOption { .. },
                ..
            })
            | Self::Ready(_) => "session/set_config_option",
        }
    }
}

impl PendingReadyConfigurationContract {
    fn resolve(
        self,
        options: &[SessionConfigOption],
    ) -> Result<ReadyConfigurationContract, AgentStartupFailure> {
        require_applied_mode(options, &self.mode_application)?;
        require_current(options, self.model_config_id, &self.requested_model)?;
        let effective_effort = match &self.requested_effort {
            Some(requested) => {
                require_current(options, self.effort_config_id, requested)?;
                Some(requested.clone())
            }
            None => effective_unspecified_effort(
                options,
                self.effort_config_id,
                self.effort_option_optional_when_unspecified,
            )?,
        };
        Ok(ReadyConfigurationContract {
            session_id: self.session_id,
            mode_application: self.mode_application,
            model_config_id: self.model_config_id,
            effort_config_id: self.effort_config_id,
            requested_model: self.requested_model,
            effective_effort,
        })
    }
}

impl ReadyConfigurationContract {
    fn accepts(
        &self,
        notification: &SessionNotification,
        adapter: &dyn AcpAgentAdapter,
        turn_is_active: bool,
        mode_transition: &mut PostReadyModeTransition,
    ) -> bool {
        if notification.session_id != self.session_id {
            return false;
        }
        let observation = match &notification.update {
            SessionUpdate::ConfigOptionUpdate(update) => {
                let observed = match self.validate_config_options(&update.config_options) {
                    Ok(observed) => observed,
                    Err(_) => return false,
                };
                observed.map(|mode| (ModeObservationSource::ConfigOptionSnapshot, mode))
            }
            SessionUpdate::CurrentModeUpdate(update) => Some((
                ModeObservationSource::CurrentModeUpdate,
                update.current_mode_id.0.as_ref(),
            )),
            _ => return true,
        };
        let Some((source, observed)) = observation else {
            return true;
        };
        if !adapter.accepts_post_ready_mode(self.expected_mode(), observed, turn_is_active) {
            return false;
        }
        mode_transition.observe(source, self.expected_mode(), observed, turn_is_active)
    }

    fn validate_config_options<'a>(
        &self,
        options: &'a [SessionConfigOption],
    ) -> Result<Option<&'a str>, AgentStartupFailure> {
        let observed_mode = match &self.mode_application {
            ResolvedModeApplication::SessionConfigOption { config_id, .. } => {
                Some(current_select(options, config_id)?)
            }
            ResolvedModeApplication::LegacySessionMode { .. } => None,
        };
        require_current(options, self.model_config_id, &self.requested_model)?;
        if let Some(effective_effort) = &self.effective_effort {
            require_current(options, self.effort_config_id, effective_effort)?;
        } else if options
            .iter()
            .any(|option| option.id.to_string() == self.effort_config_id)
        {
            return Err(configuration_invalid());
        }
        Ok(observed_mode)
    }

    fn expected_mode(&self) -> &str {
        match &self.mode_application {
            ResolvedModeApplication::SessionConfigOption { value, .. } => value,
            ResolvedModeApplication::LegacySessionMode { mode_id } => mode_id,
        }
    }
}

impl ModeConfigurationContract {
    fn validate_response(&self, value: &serde_json::Value) -> Result<(), AgentStartupFailure> {
        let ResolvedModeApplication::SessionConfigOption {
            config_id,
            value: expected,
        } = &self.mode_application
        else {
            return Ok(());
        };
        let response = serde_json::from_value::<SetSessionConfigOptionResponse>(value.clone())
            .map_err(|_| configuration_invalid())?;
        require_current(&response.config_options, config_id, expected)
    }

    fn accepts(&self, notification: &SessionNotification) -> bool {
        if notification.session_id != self.session_id {
            return false;
        }
        match (&self.mode_application, &notification.update) {
            (
                ResolvedModeApplication::SessionConfigOption { config_id, value },
                SessionUpdate::ConfigOptionUpdate(update),
            ) => require_current(&update.config_options, config_id, value).is_ok(),
            (
                ResolvedModeApplication::SessionConfigOption { value, .. },
                SessionUpdate::CurrentModeUpdate(update),
            ) => update.current_mode_id.0.as_ref() == value,
            (
                ResolvedModeApplication::LegacySessionMode { mode_id },
                SessionUpdate::CurrentModeUpdate(update),
            ) => update.current_mode_id.0.as_ref() == mode_id,
            _ => true,
        }
    }
}

struct OpeningConfigurationMonitor {
    state: Mutex<OpeningConfigurationState>,
    invalidated: CancellationToken,
}

#[derive(Default)]
struct OpeningConfigurationState {
    session_id: Option<SessionId>,
    pending_contract: Option<PendingConfigurationContract>,
    mode_contract: Option<ModeConfigurationContract>,
    contract: Option<ReadyConfigurationContract>,
    mode_transition: PostReadyModeTransition,
    invalid: bool,
    ready: bool,
}

enum ConfigurationObservation {
    Accepted,
    InvalidBeforeReady,
    InvalidAfterReady,
}

impl OpeningConfigurationMonitor {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(OpeningConfigurationState::default()),
            invalidated: CancellationToken::new(),
        })
    }

    fn invalidated(&self) -> CancellationToken {
        self.invalidated.clone()
    }

    fn arm_mode(&self, contract: ModeConfigurationContract) {
        let mut state = lock(&self.state);
        debug_assert!(state.pending_contract.is_none());
        debug_assert!(state.mode_contract.is_none());
        debug_assert!(state.contract.is_none());
        debug_assert!(
            state
                .session_id
                .as_ref()
                .is_none_or(|session_id| session_id == &contract.session_id)
        );
        state.session_id = Some(contract.session_id.clone());
        state.pending_contract = Some(PendingConfigurationContract::Mode(contract));
    }

    fn arm_ready(&self, contract: PendingReadyConfigurationContract) {
        let mut state = lock(&self.state);
        debug_assert!(state.pending_contract.is_none());
        debug_assert!(state.contract.is_none());
        state.pending_contract = Some(PendingConfigurationContract::Ready(contract));
    }

    fn observe_configuration_response(
        &self,
        method: &str,
        result: &Result<serde_json::Value, agent_client_protocol::Error>,
    ) {
        let mut state = lock(&self.state);
        let Some(pending) = state.pending_contract.as_ref() else {
            return;
        };
        if pending.response_method() != method {
            return;
        }
        let pending = state
            .pending_contract
            .take()
            .expect("the matching pending configuration contract exists");
        let Ok(value) = result else {
            return;
        };
        let resolved = match pending {
            PendingConfigurationContract::Mode(contract) => contract
                .validate_response(value)
                .map(|()| ConfigurationContract::Mode(contract)),
            PendingConfigurationContract::Ready(contract) => {
                serde_json::from_value::<SetSessionConfigOptionResponse>(value.clone())
                    .map_err(|_| configuration_invalid())
                    .and_then(|response| contract.resolve(&response.config_options))
                    .map(ConfigurationContract::Ready)
            }
        };
        match resolved {
            Ok(ConfigurationContract::Mode(contract)) => state.mode_contract = Some(contract),
            Ok(ConfigurationContract::Ready(contract)) => state.contract = Some(contract),
            Err(_) => {
                state.invalid = true;
                drop(state);
                self.invalidated.cancel();
            }
        }
    }

    fn observe_session_response(
        &self,
        method: &str,
        result: &Result<serde_json::Value, agent_client_protocol::Error>,
    ) {
        if method != "session/new" {
            return;
        }
        let Ok(value) = result else {
            return;
        };
        let Ok(response) = serde_json::from_value::<NewSessionResponse>(value.clone()) else {
            return;
        };
        let mut state = lock(&self.state);
        if state
            .session_id
            .as_ref()
            .is_some_and(|session_id| session_id != &response.session_id)
        {
            state.invalid = true;
            drop(state);
            self.invalidated.cancel();
            return;
        }
        state.session_id = Some(response.session_id);
    }

    fn owns_session(&self, session_id: &SessionId) -> bool {
        lock(&self.state)
            .session_id
            .as_ref()
            .is_some_and(|current| current == session_id)
    }

    fn observe(
        &self,
        notification: &SessionNotification,
        adapter: &dyn AcpAgentAdapter,
        turn_is_active: bool,
    ) -> ConfigurationObservation {
        let mut state = lock(&self.state);
        let accepted = if let Some(contract) = state.contract.clone() {
            contract.accepts(
                notification,
                adapter,
                turn_is_active,
                &mut state.mode_transition,
            )
        } else if let Some(contract) = &state.mode_contract {
            contract.accepts(notification)
        } else {
            true
        };
        if accepted {
            return ConfigurationObservation::Accepted;
        }
        if state.ready {
            return ConfigurationObservation::InvalidAfterReady;
        }
        state.invalid = true;
        drop(state);
        self.invalidated.cancel();
        ConfigurationObservation::InvalidBeforeReady
    }

    fn commit_ready(
        self: &Arc<Self>,
        slot: &AgentSessionSlot,
        snapshot: AgentReadySnapshot,
        route: Arc<ResultRoute>,
    ) -> Result<bool, AgentStartupFailure> {
        let mut state = lock(&self.state);
        if state.invalid {
            return Err(configuration_invalid());
        }
        debug_assert!(state.contract.is_some());
        debug_assert!(state.mode_transition.is_stable());
        let committed = slot.commit_ready(snapshot, Some(route), Some(Arc::clone(self)));
        if committed {
            state.ready = true;
        }
        Ok(committed)
    }

    fn mode_is_restored_for_settlement(&self) -> bool {
        let state = lock(&self.state);
        state.ready && !state.invalid && state.mode_transition.is_stable()
    }

    #[cfg(feature = "agent-test-support")]
    fn mode_transition_name_for_test(&self) -> &'static str {
        match &lock(&self.state).mode_transition {
            PostReadyModeTransition::Stable => "stable",
            PostReadyModeTransition::AwaitingTemporarySnapshot(_) => "awaiting_temporary_snapshot",
            PostReadyModeTransition::Temporary(_) => "temporary",
            PostReadyModeTransition::AwaitingBaselineSnapshot(_) => "awaiting_baseline_snapshot",
        }
    }
}

enum ConfigurationContract {
    Mode(ModeConfigurationContract),
    Ready(ReadyConfigurationContract),
}

enum AgentSessionState {
    Opening,
    BackingOff,
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
    terminal_fault: CancellationToken,
    ready_committed: AtomicBool,
    caller_admission: AtomicBool,
    next_turn_index: AtomicU64,
    turn_registry: Mutex<AgentTurnRegistry>,
    turn_requested: Notify,
    owned_process: Mutex<Option<ProcProcess>>,
    shutdown_descendants: Mutex<Vec<ProcProcess>>,
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
            terminal_fault: CancellationToken::new(),
            ready_committed: AtomicBool::new(false),
            caller_admission: AtomicBool::new(false),
            next_turn_index: AtomicU64::new(0),
            turn_registry: Mutex::new(AgentTurnRegistry::Open {
                request: None,
                control: None,
                submitted_session: None,
            }),
            turn_requested: Notify::new(),
            owned_process: Mutex::new(None),
            shutdown_descendants: Mutex::new(Vec::new()),
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
            None,
        );
        slot.mark_cleanup_complete();
        slot
    }

    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub(crate) fn terminal_fault(&self) -> CancellationToken {
        self.terminal_fault.clone()
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
                    | AgentSessionState::BackingOff
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

    fn current_submitted_turn(&self) -> Option<Arc<AgentTurnControl>> {
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
            .and_then(|_| control.as_ref().and_then(Weak::upgrade))
    }

    pub(crate) fn mark_broken(&self, failure: AgentSessionFailure) -> AgentSessionFailure {
        let mut state = lock(&self.state);
        let (failure, committed) = match &*state {
            AgentSessionState::Broken(existing) => (existing.clone(), false),
            AgentSessionState::Ready(_)
            | AgentSessionState::Active(_)
            | AgentSessionState::Cancelling(_) => {
                *state = AgentSessionState::Broken(failure.clone());
                self.changed.notify_waiters();
                (failure, true)
            }
            AgentSessionState::Opening
            | AgentSessionState::BackingOff
            | AgentSessionState::AuthRequired(_)
            | AgentSessionState::StartFailed(_)
            | AgentSessionState::Closed => (failure, false),
        };
        drop(state);
        if committed {
            self.capture_shutdown_descendants();
            self.terminal_fault.cancel();
        }
        failure
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
        let entered_broken = matches!(
            *state,
            AgentSessionState::Ready(_)
                | AgentSessionState::Active(_)
                | AgentSessionState::Cancelling(_)
        ) && failure.is_some();
        if entered_broken {
            let failure = failure.as_ref().expect("a Broken transition has a failure");
            *state = AgentSessionState::Broken(failure.clone());
            self.changed.notify_waiters();
        }
        drop(state);
        if entered_broken {
            self.capture_shutdown_descendants();
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
        self.capture_shutdown_descendants();
        self.cancellation.cancel();
        let delivery = {
            let mut state = lock(&self.state);
            let control = if matches!(
                *state,
                AgentSessionState::Ready(_)
                    | AgentSessionState::Active(_)
                    | AgentSessionState::Cancelling(_)
            ) {
                let (_, control) = self.freeze_turn_registry(AgentSessionFailure::transport_lost());
                control
            } else {
                None
            };
            if matches!(
                *state,
                AgentSessionState::Opening
                    | AgentSessionState::BackingOff
                    | AgentSessionState::Ready(_)
                    | AgentSessionState::Active(_)
                    | AgentSessionState::Cancelling(_)
                    | AgentSessionState::Broken(_)
            ) {
                *state = AgentSessionState::Closed;
                self.changed.notify_waiters();
            }
            control.map(|control| {
                let cleanup =
                    control.prepare_terminal_delivery(AgentSessionFailure::transport_lost());
                (control, cleanup)
            })
        };
        if let Some((control, cleanup)) = delivery {
            control.finish_terminal_delivery(cleanup);
        }
        self.terminal_fault.cancel();
        self.turn_requested.notify_waiters();
    }

    fn record_owned_process(&self, pid: u32) {
        let Some(pid) = i32::try_from(pid).ok() else {
            return;
        };
        *lock(&self.owned_process) = read_proc_process_at(Path::new("/proc"), pid);
        self.capture_shutdown_descendants();
    }

    fn capture_shutdown_descendants(&self) {
        let root = *lock(&self.owned_process);
        let mut descendants = lock(&self.shutdown_descendants);
        let mut roots = descendants.clone();
        roots.extend(root);
        if roots.is_empty() {
            return;
        }
        let discovered = descendant_processes_from_live_roots_at(Path::new("/proc"), &roots);
        descendants.extend(discovered);
        descendants.sort_unstable_by_key(|process| (process.pid, process.start_time));
        descendants.dedup_by_key(|process| (process.pid, process.start_time));
    }

    fn take_shutdown_descendants(&self) -> Vec<ProcProcess> {
        std::mem::take(&mut *lock(&self.shutdown_descendants))
    }

    #[cfg(feature = "agent-test-support")]
    pub(crate) async fn readiness(&self) -> Result<Arc<AgentReadySnapshot>, AgentStartupFailure> {
        loop {
            let changed = self.changed.notified();
            match &*lock(&self.state) {
                AgentSessionState::Opening | AgentSessionState::BackingOff => {}
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
            AgentSessionState::BackingOff => "backing_off",
            AgentSessionState::Ready(_) => "ready",
            AgentSessionState::Active(_) => "active",
            AgentSessionState::Cancelling(_) => "cancelling",
            AgentSessionState::AuthRequired(_) => "auth_required",
            AgentSessionState::StartFailed(_) => "start_failed",
            AgentSessionState::Broken(_) => "broken",
            AgentSessionState::Closed => "closed",
        }
    }

    #[cfg(feature = "agent-test-support")]
    pub(crate) fn mode_transition_name_for_test(&self) -> &'static str {
        let monitor = {
            let state = lock(&self.state);
            match &*state {
                AgentSessionState::Ready(session)
                | AgentSessionState::Active(session)
                | AgentSessionState::Cancelling(session) => {
                    session.configuration_monitor.as_ref().map(Arc::clone)
                }
                _ => None,
            }
        };
        monitor.as_ref().map_or("unavailable", |monitor| {
            monitor.mode_transition_name_for_test()
        })
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

    fn commit_ready(
        &self,
        snapshot: AgentReadySnapshot,
        route: Option<Arc<ResultRoute>>,
        configuration_monitor: Option<Arc<OpeningConfigurationMonitor>>,
    ) -> bool {
        let mut state = lock(&self.state);
        if matches!(*state, AgentSessionState::Opening) {
            *state = AgentSessionState::Ready(Arc::new(AgentReadySession {
                snapshot: Arc::new(snapshot),
                route,
                configuration_monitor,
            }));
            self.ready_committed.store(true, Ordering::Release);
            self.changed.notify_waiters();
            true
        } else {
            false
        }
    }

    fn enter_opening_backoff(&self) -> bool {
        let mut state = lock(&self.state);
        if matches!(*state, AgentSessionState::Opening) {
            *state = AgentSessionState::BackingOff;
            self.changed.notify_waiters();
            true
        } else {
            false
        }
    }

    fn begin_opening_retry(&self) -> bool {
        let mut state = lock(&self.state);
        if matches!(*state, AgentSessionState::BackingOff) {
            *state = AgentSessionState::Opening;
            self.changed.notify_waiters();
            true
        } else {
            false
        }
    }

    fn has_reached_ready(&self) -> bool {
        self.ready_committed.load(Ordering::Acquire)
    }

    fn commit_failure(&self, failure: AgentStartupFailure) {
        let mut state = lock(&self.state);
        if matches!(
            *state,
            AgentSessionState::Opening | AgentSessionState::BackingOff
        ) {
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
                AgentSessionState::Opening | AgentSessionState::BackingOff => {
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
        self.capture_shutdown_descendants();
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
        self.terminal_fault.cancel();
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

    fn commit_process_exit(&self) {
        self.commit_terminal_failure(
            AgentStartupFailure::start(
                "spawn_failed",
                "spawn",
                "agent process exited during startup",
            ),
            AgentSessionFailure::process_exited(),
        );
    }

    pub(crate) fn commit_protocol_violation(&self, startup_phase: &'static str) {
        self.commit_terminal_failure(
            AgentStartupFailure::start(
                "protocol_incompatible",
                startup_phase,
                "agent session violated the protocol contract",
            ),
            AgentSessionFailure::protocol_violation(),
        );
    }

    fn commit_result_channel_loss(&self) {
        self.commit_terminal_failure(
            AgentStartupFailure::start(
                "result_channel_unavailable",
                "mcp_ready",
                "agent result channel was lost during startup",
            ),
            AgentSessionFailure::result_channel_lost(),
        );
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
    pub(crate) fn configuration_is_restored(&self) -> bool {
        self.session
            .configuration_monitor
            .as_ref()
            .is_none_or(|monitor| monitor.mode_is_restored_for_settlement())
    }

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
        if !self.configuration_is_restored() {
            self.slot.commit_protocol_violation("configure");
            self.claimed = false;
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
        // Losing the worker future is not settlement evidence and cannot publish Ready.
        self.claimed = false;
    }
}

impl Drop for AgentSessionSlot {
    fn drop(&mut self) {
        self.capture_shutdown_descendants();
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

    let mut generation = 1_u64;
    let mut retry_ordinal = 0_u32;
    let mut previous_ambiguous = None;
    let mut consecutive_ambiguous = 0_u8;
    loop {
        let outcome = run_opening_attempt(
            &slot,
            &profile,
            spec,
            &command,
            &result_service,
            &endpoint,
            generation,
            &cancellation,
        )
        .await;
        match outcome {
            OpeningAttemptOutcome::Cancelled | OpeningAttemptOutcome::ReadyTerminated => return,
            OpeningAttemptOutcome::Terminal(failure) => {
                commit_failure(&slot, failure);
                return;
            }
            OpeningAttemptOutcome::Transient => {
                previous_ambiguous = None;
                consecutive_ambiguous = 0;
            }
            OpeningAttemptOutcome::Ambiguous(fingerprint) => {
                if previous_ambiguous == Some(fingerprint) {
                    consecutive_ambiguous += 1;
                } else {
                    previous_ambiguous = Some(fingerprint);
                    consecutive_ambiguous = 1;
                }
                if consecutive_ambiguous >= OPENING_CRASH_LOOP_THRESHOLD {
                    commit_failure(
                        &slot,
                        AgentStartupFailure::start(
                            "crash_loop",
                            fingerprint.phase,
                            "agent repeatedly failed during startup",
                        ),
                    );
                    return;
                }
            }
        }
        let Some(strong_slot) = slot.upgrade() else {
            return;
        };
        if !strong_slot.enter_opening_backoff() {
            return;
        }
        drop(strong_slot);
        let backoff_completed =
            match wait_opening_backoff(&command, retry_ordinal, &cancellation).await {
                Ok(completed) => completed,
                Err(failure) => {
                    commit_failure(&slot, failure);
                    return;
                }
            };
        if !backoff_completed {
            return;
        }
        let Some(next_generation) = generation.checked_add(1) else {
            commit_failure(&slot, preparation_failure());
            return;
        };
        let Some(strong_slot) = slot.upgrade() else {
            return;
        };
        if !strong_slot.begin_opening_retry() {
            return;
        }
        drop(strong_slot);
        generation = next_generation;
        retry_ordinal = retry_ordinal.saturating_add(1);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OpeningFailureFingerprint {
    phase: &'static str,
    observation: OpeningFailureObservation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OpeningFailureObservation {
    ProcessExited,
    TransportLost,
}

enum OpeningAttemptOutcome {
    Cancelled,
    Terminal(AgentStartupFailure),
    Transient,
    Ambiguous(OpeningFailureFingerprint),
    ReadyTerminated,
}

#[derive(Clone, Copy)]
enum OpeningAttemptCompletion {
    ConnectionClosed,
    ConnectionProtocolError,
    Child,
    ResultServiceLost,
    Cancelled,
}

#[derive(Clone)]
enum OpeningHandshakeFailure {
    Transient,
    Terminal(AgentStartupFailure),
}

impl From<AgentStartupFailure> for OpeningHandshakeFailure {
    fn from(failure: AgentStartupFailure) -> Self {
        Self::Terminal(failure)
    }
}

fn preparation_failure() -> AgentStartupFailure {
    AgentStartupFailure::start(
        "preparation_failed",
        "preparation",
        "agent session preparation failed",
    )
}

fn opening_resource_limit_failure(phase: &'static str) -> AgentStartupFailure {
    AgentStartupFailure::start(
        "resource_limit",
        phase,
        "agent sent an ACP frame above ResourceLimitsV1",
    )
}

fn classify_opening_request_error(
    spec: &AgentLaunchSpec,
    command: &ResolvedAgentCommand,
    phase: OpeningRequestPhaseV1,
    error: &agent_client_protocol::Error,
    terminal: AgentStartupFailure,
) -> OpeningHandshakeFailure {
    let code = i32::from(error.code);
    let transient = spec
        .opening_transient_errors
        .iter()
        .chain(command.opening_transient_errors.iter())
        .any(|matcher| matcher.phase == phase && matcher.code == code);
    if transient {
        OpeningHandshakeFailure::Transient
    } else {
        OpeningHandshakeFailure::Terminal(terminal)
    }
}

fn opening_retry_delay_ms(
    _command: &ResolvedAgentCommand,
    ordinal: u32,
) -> Result<u64, AgentStartupFailure> {
    #[cfg(feature = "agent-test-support")]
    if let Some(backoff) = &_command.opening_backoff {
        return sample_opening_retry_delay_ms(ordinal, || backoff.next_random_word().ok_or(()))
            .map_err(|()| preparation_failure());
    }

    sample_opening_retry_delay_ms(ordinal, || -> Result<u64, getrandom::Error> {
        let mut bytes = [0_u8; size_of::<u64>()];
        fill_secure_random(&mut bytes)?;
        Ok(u64::from_ne_bytes(bytes))
    })
    .map_err(|_| preparation_failure())
}

async fn wait_opening_backoff(
    command: &ResolvedAgentCommand,
    ordinal: u32,
    cancellation: &CancellationToken,
) -> Result<bool, AgentStartupFailure> {
    let delay_ms = opening_retry_delay_ms(command, ordinal)?;
    #[cfg(feature = "agent-test-support")]
    if let Some(backoff) = &command.opening_backoff {
        return Ok(backoff.wait(delay_ms, cancellation).await);
    }
    tokio::select! {
        () = tokio::time::sleep(Duration::from_millis(delay_ms)) => Ok(true),
        () = cancellation.cancelled() => Ok(false),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_opening_attempt(
    slot: &Weak<AgentSessionSlot>,
    profile: &Arc<ResolvedAgentProfile>,
    spec: &'static AgentLaunchSpec,
    command: &ResolvedAgentCommand,
    result_service: &Arc<ResultMcpService>,
    endpoint: &str,
    generation: u64,
    cancellation: &CancellationToken,
) -> OpeningAttemptOutcome {
    if cancellation.is_cancelled() {
        return OpeningAttemptOutcome::Cancelled;
    }
    if let Err(failure) = revalidate_workspace(&profile.workspace, "preparation") {
        return OpeningAttemptOutcome::Terminal(failure);
    }

    let spawned = match spawn_child(command, &profile.workspace) {
        Ok(spawned) => spawned,
        Err(failure) => return OpeningAttemptOutcome::Terminal(failure),
    };
    let SpawnedAgent {
        mut child,
        stdin,
        stdout,
        mut stderr,
    } = spawned;
    let pid = child.id().expect("a running agent child has a process id");
    let stderr_drain = Some({
        pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
            let mut buffer = [0_u8; 8192];
            while stderr.read(&mut buffer).await.is_ok_and(|read| read != 0) {}
        })
    });

    let current_route: Arc<Mutex<Option<Arc<ResultRoute>>>> = Arc::new(Mutex::new(None));
    let route_for_connection = Arc::clone(&current_route);
    let slot_for_connection = slot.clone();
    let profile_for_connection = Arc::clone(profile);
    let service_for_connection = Arc::clone(result_service);
    let cancellation_for_connection = cancellation.clone();
    let command_for_connection = command.clone();
    let opening_failure: Arc<Mutex<Option<OpeningHandshakeFailure>>> = Arc::new(Mutex::new(None));
    let opening_failure_for_connection = Arc::clone(&opening_failure);
    let frame_limit_exceeded = Arc::new(AtomicBool::new(false));
    let frame_limit_for_connection = Arc::clone(&frame_limit_exceeded);
    let opening_phase = Arc::new(AtomicU8::new(OpeningPhase::Initialize as u8));
    let opening_phase_for_connection = Arc::clone(&opening_phase);
    let prompt_response_provenance = Arc::new(PromptResponseProvenance::default());
    let provenance_for_handler = Arc::clone(&prompt_response_provenance);
    let opening_response_observed = Arc::new(AtomicBool::new(false));
    let response_observed_for_handler = Arc::clone(&opening_response_observed);
    let response_observed_for_connection = Arc::clone(&opening_response_observed);
    let configuration_monitor = OpeningConfigurationMonitor::new();
    let configuration_monitor_for_handler = Arc::clone(&configuration_monitor);
    let configuration_monitor_for_connection = Arc::clone(&configuration_monitor);
    let protocol_violation = CancellationToken::new();
    let protocol_violation_for_handler = protocol_violation.clone();
    let protocol_violation_for_permissions = protocol_violation.clone();
    let protocol_violation_for_connection = protocol_violation.clone();
    let protocol_violation_observed = Arc::new(ProtocolViolationBoundary::default());
    let protocol_violation_observed_for_handler = Arc::clone(&protocol_violation_observed);
    let protocol_violation_observed_for_permissions = Arc::clone(&protocol_violation_observed);
    let protocol_violation_observed_for_turn = Arc::clone(&protocol_violation_observed);
    let opening_phase_for_handler = Arc::clone(&opening_phase);
    let opening_phase_for_permissions = Arc::clone(&opening_phase);
    let slot_for_permissions = slot.clone();
    let slot_for_updates = slot.clone();
    let stdout = AcpFrameLimitedReader::new(
        stdout,
        Arc::clone(&frame_limit_exceeded),
        Arc::clone(&protocol_violation_observed),
    );
    let response_tracker = Arc::new(AcpResponseTracker::new(Arc::clone(
        &protocol_violation_observed,
    )));
    let outgoing_tracker = Arc::clone(&response_tracker);
    #[cfg(feature = "agent-test-support")]
    let response_flush_gate = command.turn_gates.response_flush.clone();
    let outgoing = futures::sink::unfold(stdin.compat_write(), move |mut writer, line: String| {
        let tracker = Arc::clone(&outgoing_tracker);
        #[cfg(feature = "agent-test-support")]
        let response_flush_gate = response_flush_gate.clone();
        async move {
            tracker.observe_outgoing(&line)?;
            writer.write_all(line.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            #[cfg(feature = "agent-test-support")]
            if AcpResponseTracker::outgoing_contains_response(&line)
                && let Some(gate) = &response_flush_gate
            {
                gate.wait().await;
                gate.mark_completed();
            }
            writer.flush().await?;
            tracker.observe_outgoing_flushed(&line);
            Ok::<_, std::io::Error>(writer)
        }
    });
    let incoming_tracker = Arc::clone(&response_tracker);
    let flush_tracker_for_permissions = Arc::clone(&response_tracker);
    let flush_tracker_for_handler = Arc::clone(&response_tracker);
    let incoming = futures::io::BufReader::new(stdout.compat())
        .lines()
        .map(move |line| {
            line.and_then(|line| {
                incoming_tracker.observe_incoming(&line)?;
                Ok(line)
            })
        });
    let transport = Lines::new(outgoing, incoming);
    let listener_failure = result_service.listener_failure();
    let mut tracked_descendants = Vec::new();
    let completion = {
        let connection = Client
            .builder()
            .name("troupe")
            .on_receive_request(
                async move |request: RequestPermissionRequest, responder, connection| {
                    let response_id = responder.id().clone();
                    let already_registered =
                        flush_tracker_for_permissions.response_was_predeferred(&response_id);
                    let response_flushed =
                        flush_tracker_for_permissions.wait_for_response_flush(response_id);
                    let Some(slot) = slot_for_permissions.upgrade() else {
                        return responder.respond(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Cancelled,
                        ));
                    };
                    let Some(control) = slot.submitted_turn(&request.session_id) else {
                        let response = responder.respond(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Cancelled,
                        ));
                        let phase = OpeningPhase::load(&opening_phase_for_permissions).name();
                        defer_protocol_violation_until_response_flush(
                            &connection,
                            response_flushed,
                            already_registered,
                            Arc::downgrade(&slot),
                            phase,
                            Arc::clone(&protocol_violation_observed_for_permissions),
                            protocol_violation_for_permissions.clone(),
                        )?;
                        return response;
                    };
                    let (attributable, response) =
                        control.respond_permission(&request, responder, agent_adapter(spec.agent));
                    if !attributable {
                        let phase = OpeningPhase::load(&opening_phase_for_permissions).name();
                        defer_protocol_violation_until_response_flush(
                            &connection,
                            response_flushed,
                            already_registered,
                            Arc::downgrade(&slot),
                            phase,
                            Arc::clone(&protocol_violation_observed_for_permissions),
                            protocol_violation_for_permissions.clone(),
                        )?;
                    }
                    response
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_dispatch(
                async move |dispatch: Dispatch, connection: ConnectionTo<Agent>| match dispatch {
                    message @ Dispatch::Notification(_) if message.method() == "session/update" => {
                        match message.into_notification::<SessionNotification>() {
                            Ok(Ok(notification)) => {
                                let submitted_turn = slot_for_updates
                                    .upgrade()
                                    .and_then(|slot| slot.submitted_turn(&notification.session_id));
                                let turn_is_active = submitted_turn
                                    .as_ref()
                                    .is_some_and(|control| control.accepts_ordinary_update());
                                let configuration_update = matches!(
                                    &notification.update,
                                    SessionUpdate::ConfigOptionUpdate(_)
                                        | SessionUpdate::CurrentModeUpdate(_)
                                );
                                let session_scoped_update = matches!(
                                    &notification.update,
                                    SessionUpdate::AvailableCommandsUpdate(_)
                                        | SessionUpdate::SessionInfoUpdate(_)
                                        | SessionUpdate::UsageUpdate(_)
                                );
                                let invalid_after_ready = matches!(
                                    configuration_monitor_for_handler.observe(
                                        &notification,
                                        agent_adapter(spec.agent),
                                        turn_is_active,
                                    ),
                                    ConfigurationObservation::InvalidAfterReady
                                );
                                if invalid_after_ready {
                                    if let Some(slot) = slot_for_updates.upgrade() {
                                        slot.commit_protocol_violation("configure");
                                    }
                                } else if !configuration_update {
                                    let accepted = (session_scoped_update
                                        && configuration_monitor_for_handler
                                            .owns_session(&notification.session_id))
                                        || submitted_turn.is_some_and(|control| {
                                            control.accepts_ordinary_update()
                                        });
                                    if !accepted {
                                        protocol_violation_observed_for_handler.mark_observed();
                                        let phase =
                                            OpeningPhase::load(&opening_phase_for_handler).name();
                                        if let Some(slot) = slot_for_updates.upgrade() {
                                            slot.commit_protocol_violation(phase);
                                        }
                                        protocol_violation_for_handler.cancel();
                                    }
                                }
                            }
                            Ok(Err(_)) => unreachable!("the notification method matched"),
                            Err(_) => {
                                protocol_violation_observed_for_handler.mark_observed();
                                let phase = OpeningPhase::load(&opening_phase_for_handler).name();
                                if let Some(slot) = slot_for_updates.upgrade() {
                                    slot.commit_protocol_violation(phase);
                                }
                                protocol_violation_for_handler.cancel();
                            }
                        }
                        Ok(Handled::Yes)
                    }
                    Dispatch::Response(result, router) => {
                        response_observed_for_handler.store(true, Ordering::Release);
                        configuration_monitor_for_handler
                            .observe_session_response(router.method(), &result);
                        configuration_monitor_for_handler
                            .observe_configuration_response(router.method(), &result);
                        if router.method() == "session/prompt" {
                            if let Some(control) = slot_for_updates
                                .upgrade()
                                .and_then(|slot| slot.current_submitted_turn())
                            {
                                control.mark_prompt_response_observed();
                            }
                            if result.is_err() {
                                provenance_for_handler.record_remote_error(router.id().clone());
                            }
                        }
                        router.route_with_result(result)?;
                        Ok(Handled::Yes)
                    }
                    Dispatch::Request(message, responder) => {
                        let method = message.method;
                        let response_id = responder.id().clone();
                        let already_registered =
                            flush_tracker_for_handler.response_was_predeferred(&response_id);
                        let response_flushed =
                            flush_tracker_for_handler.wait_for_response_flush(response_id);
                        let control = slot_for_updates
                            .upgrade()
                            .and_then(|slot| slot.current_submitted_turn());
                        let (attributable, response) = if let Some(control) = control {
                            control.respond_unsupported_request(|| {
                                responder.respond_with_error(
                                    agent_client_protocol::Error::method_not_found().data(method),
                                )
                            })
                        } else {
                            (
                                false,
                                responder.respond_with_error(
                                    agent_client_protocol::Error::method_not_found().data(method),
                                ),
                            )
                        };
                        if !attributable {
                            let phase = OpeningPhase::load(&opening_phase_for_handler).name();
                            defer_protocol_violation_until_response_flush(
                                &connection,
                                response_flushed,
                                already_registered,
                                slot_for_updates.clone(),
                                phase,
                                Arc::clone(&protocol_violation_observed_for_handler),
                                protocol_violation_for_handler.clone(),
                            )?;
                        }
                        response?;
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
                    let configuration_invalidated =
                        configuration_monitor_for_connection.invalidated();
                    let result = tokio::select! {
                        biased;
                        result = open_handshake(
                            &connection,
                            &slot_for_connection,
                            &profile_for_connection,
                            spec,
                            &command_for_connection,
                            &service_for_connection,
                            &route_for_connection,
                            pid,
                            endpoint,
                            &cancellation_for_connection,
                            &opening_phase_for_connection,
                            &response_observed_for_connection,
                            &configuration_monitor_for_connection,
                            generation,
                        ) => Some(result),
                        () = connection.incoming_closed() => None,
                        () = configuration_invalidated.cancelled() => {
                            Some(Err(OpeningHandshakeFailure::Terminal(
                                configuration_invalid()
                            )))
                        }
                        () = protocol_violation_for_connection.cancelled() => {
                            Some(Err(OpeningHandshakeFailure::Terminal(
                                AgentStartupFailure::start(
                                    "protocol_incompatible",
                                    OpeningPhase::load(&opening_phase_for_connection).name(),
                                    "agent connection violated the protocol during startup",
                                )
                            )))
                        }
                    };
                    let Some(result) = result else {
                        return Ok(());
                    };
                    match result {
                        Ok(Some(session_id)) => {
                            if let Some(slot) = slot_for_connection.upgrade() {
                                run_agent_turn_worker(
                                    &connection,
                                    slot,
                                    session_id,
                                    &prompt_response_provenance,
                                    agent_adapter(spec.agent),
                                    Arc::clone(
                                        &command_for_connection.authoritative_prompt_error_codes,
                                    ),
                                    Arc::clone(&protocol_violation_observed_for_turn),
                                    &cancellation_for_connection,
                                    #[cfg(feature = "agent-test-support")]
                                    command_for_connection.turn_gates.clone(),
                                )
                                .await;
                            }
                        }
                        Ok(None) => {}
                        Err(failure) => {
                            if !frame_limit_for_connection.load(Ordering::Acquire)
                                && (!connection.is_incoming_closed()
                                    || response_observed_for_connection.load(Ordering::Acquire))
                            {
                                *lock(&opening_failure_for_connection) = Some(failure);
                            }
                        }
                    }
                    Ok(())
                },
            );
        tokio::pin!(connection);
        let child_wait = child.wait();
        tokio::pin!(child_wait);
        tokio::select! {
            result = &mut connection => {
                if result.is_ok() {
                    OpeningAttemptCompletion::ConnectionClosed
                } else {
                    OpeningAttemptCompletion::ConnectionProtocolError
                }
            },
            _ = &mut child_wait => OpeningAttemptCompletion::Child,
            () = listener_failure.cancelled() => OpeningAttemptCompletion::ResultServiceLost,
            () = cancellation.cancelled() => {
                tracked_descendants = slot
                    .upgrade()
                    .map(|slot| slot.take_shutdown_descendants())
                    .unwrap_or_default();
                tracked_descendants.extend(i32::try_from(pid)
                    .ok()
                    .map(|root_pid| descendant_processes_at(Path::new("/proc"), root_pid))
                    .unwrap_or_default());
                OpeningAttemptCompletion::Cancelled
            },
        }
    };
    let cancelled =
        cancellation.is_cancelled() || matches!(completion, OpeningAttemptCompletion::Cancelled);
    let phase = OpeningPhase::load(&opening_phase).name();
    let reached_ready = slot.upgrade().is_some_and(|slot| slot.has_reached_ready());
    if !cancelled && reached_ready {
        let strong_slot = slot.upgrade();
        if frame_limit_exceeded.load(Ordering::Acquire) {
            if let Some(slot) = strong_slot {
                slot.mark_acp_resource_limit(phase);
            }
        } else if let Some(slot) = strong_slot {
            match completion {
                OpeningAttemptCompletion::Child => slot.commit_process_exit(),
                OpeningAttemptCompletion::ConnectionClosed => slot.commit_transport_loss(),
                OpeningAttemptCompletion::ConnectionProtocolError => {
                    slot.commit_protocol_violation(phase);
                }
                OpeningAttemptCompletion::ResultServiceLost => {
                    slot.commit_result_channel_loss();
                }
                OpeningAttemptCompletion::Cancelled => {}
            }
        }
    }
    tracked_descendants.extend(
        slot.upgrade()
            .map(|slot| slot.take_shutdown_descendants())
            .unwrap_or_default(),
    );
    let route = { lock(&current_route).take() };
    if let Some(route) = route {
        result_service.revoke_route(&route).await;
    }
    terminate_and_reap(&mut child, pid, tracked_descendants).await;
    wait_for_stderr_drain(stderr_drain).await;

    if cancelled {
        return OpeningAttemptOutcome::Cancelled;
    }
    if reached_ready {
        return OpeningAttemptOutcome::ReadyTerminated;
    }
    if frame_limit_exceeded.load(Ordering::Acquire) {
        return OpeningAttemptOutcome::Terminal(opening_resource_limit_failure(phase));
    }
    if matches!(completion, OpeningAttemptCompletion::ResultServiceLost) {
        return OpeningAttemptOutcome::Terminal(AgentStartupFailure::start(
            "result_channel_unavailable",
            "mcp_ready",
            "agent result service listener failed",
        ));
    }
    if let Some(failure) = lock(&opening_failure).take() {
        return match failure {
            OpeningHandshakeFailure::Transient => OpeningAttemptOutcome::Transient,
            OpeningHandshakeFailure::Terminal(failure) => OpeningAttemptOutcome::Terminal(failure),
        };
    }
    if matches!(
        completion,
        OpeningAttemptCompletion::ConnectionProtocolError
    ) {
        return OpeningAttemptOutcome::Terminal(AgentStartupFailure::start(
            "protocol_incompatible",
            phase,
            "agent connection violated the protocol during startup",
        ));
    }
    let observation = match completion {
        OpeningAttemptCompletion::Child => OpeningFailureObservation::ProcessExited,
        OpeningAttemptCompletion::ConnectionClosed => OpeningFailureObservation::TransportLost,
        OpeningAttemptCompletion::ConnectionProtocolError => {
            unreachable!("connection protocol failure returned above")
        }
        OpeningAttemptCompletion::ResultServiceLost => {
            unreachable!("result service failure returned above")
        }
        OpeningAttemptCompletion::Cancelled => unreachable!("cancellation returned above"),
    };
    OpeningAttemptOutcome::Ambiguous(OpeningFailureFingerprint { phase, observation })
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
    response_observed: &AtomicBool,
    configuration_monitor: &Arc<OpeningConfigurationMonitor>,
    generation: u64,
) -> Result<Option<SessionId>, OpeningHandshakeFailure> {
    if !spec.supports_step1_opening(profile.agent) {
        return Err(AgentStartupFailure::start(
            "protocol_incompatible",
            "initialize",
            "agent launch contract is incompatible with this runtime",
        )
        .into());
    }
    let initialize = InitializeRequest::new(ProtocolVersion::V1)
        .client_capabilities(ClientCapabilities::new().session(ClientSessionCapabilities::new()))
        .client_info(Implementation::new("troupe", env!("CARGO_PKG_VERSION")));
    response_observed.store(false, Ordering::Release);
    let initialized = connection
        .send_request(initialize)
        .block_task()
        .await
        .map_err(|error| {
            classify_opening_request_error(
                spec,
                command,
                OpeningRequestPhaseV1::Initialize,
                &error,
                AgentStartupFailure::start(
                    "protocol_incompatible",
                    "initialize",
                    "agent initialization failed",
                ),
            )
        })?;
    if initialized.protocol_version != ProtocolVersion::V1
        || !supports_http_mcp(&initialized.agent_capabilities.mcp_capabilities)
        || spec.required_agent_info_version().is_some_and(|expected| {
            initialized
                .agent_info
                .as_ref()
                .is_none_or(|info| info.version != expected)
        })
    {
        return Err(AgentStartupFailure::start(
            "protocol_incompatible",
            "initialize",
            "agent does not support the required protocol",
        )
        .into());
    }
    let agent_info = initialized.agent_info;
    let agent_capabilities = initialized.agent_capabilities;

    let route = result_service.register_route(
        generation,
        spec.mcp_wire_protocol.as_str(),
        #[cfg(feature = "agent-test-support")]
        command.mcp_ready_gate.clone(),
    )?;
    *lock(current_route) = Some(Arc::clone(&route));
    revalidate_workspace(&profile.workspace, "session_new")?;
    OpeningPhase::SessionNew.store(opening_phase);
    let session = send_new_session(connection, profile, &route, response_observed).await;
    if session
        .as_ref()
        .is_err_and(|error| error.code == ErrorCode::AuthRequired)
    {
        result_service.revoke_route(&route).await;
        *lock(current_route) = None;
        return Err(AgentStartupFailure::authentication_required("session_new").into());
    }
    let session = session.map_err(|error| {
        if error.code == ErrorCode::AuthRequired {
            OpeningHandshakeFailure::Terminal(AgentStartupFailure::authentication_required(
                "session_new",
            ))
        } else {
            classify_opening_request_error(
                spec,
                command,
                OpeningRequestPhaseV1::SessionNew,
                &error,
                AgentStartupFailure::start(
                    "protocol_incompatible",
                    "session_new",
                    "agent session creation failed",
                ),
            )
        }
    })?;
    revalidate_workspace(&profile.workspace, "session_new")?;

    OpeningPhase::Configure.store(opening_phase);
    let (effective_model, effective_effort) = configure_session(
        connection,
        spec,
        &session,
        &profile.requested_model,
        profile.requested_effort.as_deref(),
        response_observed,
        command,
        configuration_monitor,
    )
    .await?;
    let session_id = session.session_id.clone();
    #[cfg(feature = "agent-test-support")]
    if let Some(gate) = &command.configuration_ready_gate {
        tokio::select! {
            () = gate.wait() => {}
            () = cancellation.cancelled() => {
                return Err(AgentStartupFailure::start(
                    "result_channel_unavailable",
                    "configure",
                    "agent session was closed before configuration readiness",
                ).into());
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
        return Ok(None);
    };
    slot.record_owned_process(pid);
    let committed = configuration_monitor.commit_ready(
        &slot,
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
        Arc::clone(&route),
    )?;
    Ok(committed.then_some(session_id))
}

fn supports_http_mcp(capabilities: &McpCapabilities) -> bool {
    capabilities.http
}

async fn send_new_session(
    connection: &ConnectionTo<Agent>,
    profile: &ResolvedAgentProfile,
    route: &ResultRoute,
    response_observed: &AtomicBool,
) -> agent_client_protocol::Result<agent_client_protocol::schema::v1::NewSessionResponse> {
    response_observed.store(false, Ordering::Release);
    connection
        .send_request(
            NewSessionRequest::new(profile.workspace.acp_cwd_alias.clone())
                .mcp_servers(vec![route.mcp_server()]),
        )
        .block_task()
        .await
}

#[allow(clippy::too_many_arguments)]
async fn configure_session(
    connection: &ConnectionTo<Agent>,
    spec: &AgentLaunchSpec,
    session: &NewSessionResponse,
    requested_model: &str,
    requested_effort: Option<&str>,
    response_observed: &AtomicBool,
    command: &ResolvedAgentCommand,
    configuration_monitor: &OpeningConfigurationMonitor,
) -> Result<(String, Option<String>), OpeningHandshakeFailure> {
    let session_id = &session.session_id;
    let initial = session.config_options.as_deref().unwrap_or_default();
    let after_mode = match &command.mode_application {
        ResolvedModeApplication::SessionConfigOption { config_id, value } => {
            require_select_value(initial, config_id, value)?;
            configuration_monitor.arm_mode(mode_configuration(session, command));
            let after_mode = apply_select(
                connection,
                session_id,
                config_id,
                value,
                response_observed,
                spec,
                command,
            )
            .await?;
            require_current(&after_mode, config_id, value)?;
            after_mode
        }
        ResolvedModeApplication::LegacySessionMode { mode_id } => {
            require_legacy_mode(session.modes.as_ref(), mode_id)?;
            configuration_monitor.arm_mode(mode_configuration(session, command));
            apply_legacy_mode(
                connection,
                session_id,
                mode_id,
                response_observed,
                spec,
                command,
            )
            .await?;
            initial.to_vec()
        }
    };

    require_select_value(&after_mode, spec.model_config_id, requested_model)?;
    let effective_effort = if let Some(requested_effort) = requested_effort {
        let after_model = apply_select(
            connection,
            session_id,
            spec.model_config_id,
            requested_model,
            response_observed,
            spec,
            command,
        )
        .await?;
        require_applied_mode(&after_model, &command.mode_application)?;
        require_current(&after_model, spec.model_config_id, requested_model)?;
        require_select_value(&after_model, spec.effort_config_id, requested_effort)?;
        let after_effort = apply_final_select(
            connection,
            session_id,
            spec.effort_config_id,
            requested_effort,
            response_observed,
            spec,
            command,
            configuration_monitor,
            pending_ready_configuration(
                session,
                spec,
                command,
                requested_model,
                Some(requested_effort),
            ),
        )
        .await?;
        require_applied_mode(&after_effort, &command.mode_application)?;
        require_current(&after_effort, spec.model_config_id, requested_model)?;
        require_current(&after_effort, spec.effort_config_id, requested_effort)?;
        Some(requested_effort.to_owned())
    } else {
        let after_model = apply_final_select(
            connection,
            session_id,
            spec.model_config_id,
            requested_model,
            response_observed,
            spec,
            command,
            configuration_monitor,
            pending_ready_configuration(session, spec, command, requested_model, None),
        )
        .await?;
        require_applied_mode(&after_model, &command.mode_application)?;
        require_current(&after_model, spec.model_config_id, requested_model)?;
        effective_unspecified_effort(
            &after_model,
            spec.effort_config_id,
            spec.effort_option_optional_when_unspecified,
        )?
    };
    Ok((requested_model.to_owned(), effective_effort))
}

fn pending_ready_configuration(
    session: &NewSessionResponse,
    spec: &AgentLaunchSpec,
    command: &ResolvedAgentCommand,
    requested_model: &str,
    requested_effort: Option<&str>,
) -> PendingReadyConfigurationContract {
    PendingReadyConfigurationContract {
        session_id: session.session_id.clone(),
        mode_application: command.mode_application.clone(),
        model_config_id: spec.model_config_id,
        effort_config_id: spec.effort_config_id,
        effort_option_optional_when_unspecified: spec.effort_option_optional_when_unspecified,
        requested_model: requested_model.to_owned(),
        requested_effort: requested_effort.map(str::to_owned),
    }
}

fn mode_configuration(
    session: &NewSessionResponse,
    command: &ResolvedAgentCommand,
) -> ModeConfigurationContract {
    ModeConfigurationContract {
        session_id: session.session_id.clone(),
        mode_application: command.mode_application.clone(),
    }
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

fn effective_unspecified_effort(
    options: &[SessionConfigOption],
    config_id: &str,
    option_optional: bool,
) -> Result<Option<String>, AgentStartupFailure> {
    let present = options
        .iter()
        .any(|option| option.id.to_string() == config_id);
    if option_optional && !present {
        return Ok(None);
    }
    Ok(Some(current_select(options, config_id)?.to_owned()))
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
    response_observed: &AtomicBool,
    spec: &AgentLaunchSpec,
    command: &ResolvedAgentCommand,
) -> Result<(), OpeningHandshakeFailure> {
    response_observed.store(false, Ordering::Release);
    connection
        .send_request(SetSessionModeRequest::new(
            session_id.clone(),
            mode_id.to_owned(),
        ))
        .block_task()
        .await
        .map(|_| ())
        .map_err(|error| {
            classify_opening_request_error(
                spec,
                command,
                OpeningRequestPhaseV1::Configure,
                &error,
                configuration_invalid(),
            )
        })
}

async fn apply_select(
    connection: &ConnectionTo<Agent>,
    session_id: &agent_client_protocol::schema::v1::SessionId,
    config_id: &str,
    value: &str,
    response_observed: &AtomicBool,
    spec: &AgentLaunchSpec,
    command: &ResolvedAgentCommand,
) -> Result<Vec<SessionConfigOption>, OpeningHandshakeFailure> {
    response_observed.store(false, Ordering::Release);
    connection
        .send_request(SetSessionConfigOptionRequest::new(
            session_id.clone(),
            config_id.to_owned(),
            SessionConfigOptionValue::value_id(value.to_owned()),
        ))
        .block_task()
        .await
        .map(|response| response.config_options)
        .map_err(|error| {
            classify_opening_request_error(
                spec,
                command,
                OpeningRequestPhaseV1::Configure,
                &error,
                configuration_invalid(),
            )
        })
}

#[allow(clippy::too_many_arguments)]
async fn apply_final_select(
    connection: &ConnectionTo<Agent>,
    session_id: &agent_client_protocol::schema::v1::SessionId,
    config_id: &str,
    value: &str,
    response_observed: &AtomicBool,
    spec: &AgentLaunchSpec,
    command: &ResolvedAgentCommand,
    configuration_monitor: &OpeningConfigurationMonitor,
    pending_contract: PendingReadyConfigurationContract,
) -> Result<Vec<SessionConfigOption>, OpeningHandshakeFailure> {
    configuration_monitor.arm_ready(pending_contract);
    apply_select(
        connection,
        session_id,
        config_id,
        value,
        response_observed,
        spec,
        command,
    )
    .await
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

struct SpawnedAgent {
    child: Child,
    stdin: ForkTracked<ChildStdin>,
    stdout: ForkTracked<ChildStdout>,
    stderr: ForkTracked<ChildStderr>,
}

fn spawn_child(
    command: &ResolvedAgentCommand,
    workspace: &WorkspaceLeaseV1,
) -> Result<SpawnedAgent, AgentStartupFailure> {
    let mut standard = std::process::Command::new(&command.program);
    standard
        .args(&command.args)
        .envs(command.environment.iter().cloned())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in &command.removed_environment {
        standard.env_remove(name);
    }
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
    let guard = ForkExecGuard::begin();
    let mut child = Command::from(standard).spawn().map_err(|_| {
        AgentStartupFailure::start("spawn_failed", "spawn", "agent process could not start")
    })?;
    let stdin = child.stdin.take().expect("agent stdin was configured");
    let stdout = child.stdout.take().expect("agent stdout was configured");
    let stderr = child.stderr.take().expect("agent stderr was configured");
    let spawned = SpawnedAgent {
        child,
        stdin: guard.track(stdin),
        stdout: guard.track(stdout),
        stderr: guard.track(stderr),
    };
    drop(guard);
    Ok(spawned)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcProcess {
    pid: i32,
    state: char,
    parent_pid: i32,
    process_group: i32,
    start_time: u64,
}

fn parse_proc_process(pid: i32, stat: &str) -> Option<ProcProcess> {
    let command_end = stat.rfind(')')?;
    let mut fields = stat[command_end + 1..].split_whitespace();
    let state = fields.next()?.chars().next()?;
    let parent_pid = fields.next()?.parse::<i32>().ok()?;
    let process_group = fields.next()?.parse::<i32>().ok()?;
    // Linux /proc/<pid>/stat starttime is field 22, sixteen fields after pgrp.
    let start_time = fields.nth(16)?.parse::<u64>().ok()?;
    Some(ProcProcess {
        pid,
        state,
        parent_pid,
        process_group,
        start_time,
    })
}

fn read_proc_process(entry: &std::fs::DirEntry) -> Option<ProcProcess> {
    let name = entry.file_name();
    let name = name.to_str()?;
    if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let pid = name.parse::<i32>().ok()?;
    let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
    parse_proc_process(pid, &stat)
}

fn read_proc_process_at(proc_root: &Path, pid: i32) -> Option<ProcProcess> {
    let stat = std::fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?;
    parse_proc_process(pid, &stat)
}

fn process_table_at(proc_root: &Path) -> Option<Vec<ProcProcess>> {
    Some(
        std::fs::read_dir(proc_root)
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|entry| read_proc_process(&entry))
            .collect(),
    )
}

fn process_group_has_live_members_at(proc_root: &Path, process_group: i32) -> bool {
    let Some(processes) = process_table_at(proc_root) else {
        return true;
    };
    processes.into_iter().any(|process| {
        process.process_group == process_group && !matches!(process.state, 'X' | 'Z')
    })
}

fn descendant_processes_at(proc_root: &Path, root_pid: i32) -> Vec<ProcProcess> {
    let Some(processes) = process_table_at(proc_root) else {
        return Vec::new();
    };
    let mut depths = std::collections::HashMap::from([(root_pid, 0_usize)]);
    loop {
        let mut changed = false;
        for process in &processes {
            let Some(parent_depth) = depths.get(&process.parent_pid).copied() else {
                continue;
            };
            if let std::collections::hash_map::Entry::Vacant(entry) = depths.entry(process.pid) {
                entry.insert(parent_depth + 1);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut descendants = processes
        .into_iter()
        .filter_map(|process| {
            depths
                .get(&process.pid)
                .copied()
                .filter(|_| process.pid != root_pid)
                .map(|depth| (process, depth))
        })
        .collect::<Vec<_>>();
    descendants.sort_unstable_by(|left, right| right.1.cmp(&left.1));
    descendants
        .into_iter()
        .map(|(process, _)| process)
        .collect()
}

fn descendant_processes_from_live_roots_at(
    proc_root: &Path,
    roots: &[ProcProcess],
) -> Vec<ProcProcess> {
    let Some(processes) = process_table_at(proc_root) else {
        return Vec::new();
    };
    let root_pids = roots
        .iter()
        .filter(|root| {
            processes.iter().any(|process| {
                process.pid == root.pid
                    && process.start_time == root.start_time
                    && !matches!(process.state, 'X' | 'Z')
            })
        })
        .map(|root| root.pid)
        .collect::<std::collections::HashSet<_>>();
    let mut depths = root_pids
        .iter()
        .copied()
        .map(|pid| (pid, 0_usize))
        .collect::<std::collections::HashMap<_, _>>();
    loop {
        let mut changed = false;
        for process in &processes {
            let Some(parent_depth) = depths.get(&process.parent_pid).copied() else {
                continue;
            };
            if let std::collections::hash_map::Entry::Vacant(entry) = depths.entry(process.pid) {
                entry.insert(parent_depth + 1);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut descendants = processes
        .into_iter()
        .filter_map(|process| {
            depths
                .get(&process.pid)
                .copied()
                .filter(|_| !root_pids.contains(&process.pid))
                .map(|depth| (process, depth))
        })
        .collect::<Vec<_>>();
    descendants.sort_unstable_by(|left, right| right.1.cmp(&left.1));
    descendants
        .into_iter()
        .map(|(process, _)| process)
        .collect()
}

fn process_is_live_at(proc_root: &Path, identity: ProcProcess) -> bool {
    read_proc_process_at(proc_root, identity.pid).is_some_and(|process| {
        process.start_time == identity.start_time && !matches!(process.state, 'X' | 'Z')
    })
}

#[cfg(test)]
async fn wait_for_process_group_exit_at(proc_root: &Path, process_group: i32) {
    while process_group_has_live_members_at(proc_root, process_group) {
        tokio::task::yield_now().await;
    }
}

async fn wait_for_process_group_exit(process_group: i32) {
    while process_group_has_live_members_at(Path::new("/proc"), process_group) {
        unsafe {
            libc::kill(-process_group, libc::SIGKILL);
        }
        tokio::task::yield_now().await;
    }
}

async fn wait_for_descendant_exit(descendants: &[ProcProcess]) {
    while descendants
        .iter()
        .any(|process| process_is_live_at(Path::new("/proc"), *process))
    {
        for process in descendants {
            if process_is_live_at(Path::new("/proc"), *process) {
                unsafe {
                    libc::kill(process.pid, libc::SIGKILL);
                }
            }
        }
        tokio::task::yield_now().await;
    }
}

async fn terminate_and_reap(child: &mut Child, pid: u32, mut descendants: Vec<ProcProcess>) {
    let process_group = i32::try_from(pid).ok();
    if let Some(root_pid) = process_group {
        descendants.extend(descendant_processes_at(Path::new("/proc"), root_pid));
        descendants.sort_unstable_by_key(|process| (process.pid, process.start_time));
        descendants.dedup_by_key(|process| (process.pid, process.start_time));
    }
    for descendant in &descendants {
        if process_is_live_at(Path::new("/proc"), *descendant) {
            unsafe {
                libc::kill(descendant.pid, libc::SIGKILL);
            }
        }
    }
    if let Some(process_group) = process_group {
        unsafe {
            libc::kill(-process_group, libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
    if let Some(process_group) = process_group {
        wait_for_process_group_exit(process_group).await;
    }
    wait_for_descendant_exit(&descendants).await;
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
    use tokio::io::AsyncReadExt as _;
    use tokio::sync::oneshot;

    use super::*;

    fn write_proc_process(
        proc_root: &Path,
        pid: i32,
        command: &str,
        state: char,
        parent_pid: i32,
        process_group: i32,
        start_time: u64,
    ) {
        let process = proc_root.join(pid.to_string());
        std::fs::create_dir_all(&process).unwrap();
        let intermediate_fields = ["0"; 16].join(" ");
        std::fs::write(
            process.join("stat"),
            format!(
                "{pid} ({command}) {state} {parent_pid} {process_group} {intermediate_fields} {start_time}\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn opening_retry_window_uses_half_to_full_exponential_range_with_cap() {
        assert_eq!(opening_retry_window_ms(0), (125, 250));
        assert_eq!(opening_retry_window_ms(1), (250, 500));
        assert_eq!(opening_retry_window_ms(6), (8_000, 16_000));
        assert_eq!(opening_retry_window_ms(7), (15_000, 30_000));
        assert_eq!(opening_retry_window_ms(u32::MAX), (15_000, 30_000));
    }

    #[test]
    fn opening_retry_sampling_rejects_the_biased_prefix_and_keeps_boundaries() {
        let mut words = [15_u64, 16].into_iter();
        assert_eq!(
            sample_opening_retry_delay_ms(0, || words.next().ok_or(())),
            Ok(141)
        );
        assert_eq!(
            sample_opening_retry_delay_ms(0, || Ok::<u64, ()>(126)),
            Ok(125)
        );
        assert_eq!(
            sample_opening_retry_delay_ms(0, || Ok::<u64, ()>(125)),
            Ok(250)
        );
    }

    #[test]
    fn post_ready_mode_transition_requires_both_pinned_claude_snapshots() {
        let mut transition = PostReadyModeTransition::default();

        assert!(transition.observe(
            ModeObservationSource::CurrentModeUpdate,
            "default",
            "plan",
            true,
        ));
        assert!(!transition.is_stable());
        assert!(transition.observe(
            ModeObservationSource::ConfigOptionSnapshot,
            "default",
            "plan",
            true,
        ));
        assert!(!transition.is_stable());
        assert!(transition.observe(
            ModeObservationSource::CurrentModeUpdate,
            "default",
            "default",
            true,
        ));
        assert!(!transition.is_stable());
        assert!(transition.observe(
            ModeObservationSource::ConfigOptionSnapshot,
            "default",
            "default",
            true,
        ));
        assert!(transition.is_stable());
    }

    #[test]
    fn post_ready_mode_transition_rejects_restoration_before_plan_snapshot() {
        let mut transition = PostReadyModeTransition::default();

        assert!(transition.observe(
            ModeObservationSource::CurrentModeUpdate,
            "default",
            "plan",
            true,
        ));
        assert!(!transition.observe(
            ModeObservationSource::CurrentModeUpdate,
            "default",
            "default",
            true,
        ));
        assert!(!transition.is_stable());
    }

    #[test]
    fn post_ready_mode_transition_cannot_advance_after_the_prompt_response() {
        let mut transition = PostReadyModeTransition::default();
        assert!(transition.observe(
            ModeObservationSource::CurrentModeUpdate,
            "default",
            "plan",
            true,
        ));
        assert!(transition.observe(
            ModeObservationSource::ConfigOptionSnapshot,
            "default",
            "plan",
            true,
        ));

        assert!(!transition.observe(
            ModeObservationSource::CurrentModeUpdate,
            "default",
            "default",
            false,
        ));
        assert!(!transition.is_stable());
    }

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
    async fn process_group_wait_requires_os_confirmed_no_live_members() {
        let proc_root = std::env::temp_dir().join(format!(
            "troupe-process-group-test-{}",
            uuid::Uuid::new_v4()
        ));
        write_proc_process(&proc_root, 41001, "mock agent", 'R', 1, 41000, 101);

        let mut wait = std::pin::pin!(wait_for_process_group_exit_at(&proc_root, 41000));
        tokio::select! {
            biased;
            () = &mut wait => panic!("cleanup accepted a live process-group member"),
            () = tokio::task::yield_now() => {}
        }

        write_proc_process(&proc_root, 41001, "mock agent", 'Z', 1, 41000, 101);
        wait.await;
        std::fs::remove_dir_all(&proc_root).unwrap();
    }

    #[test]
    fn descendant_scan_crosses_process_groups_and_rejects_reused_pids() {
        let proc_root = std::env::temp_dir().join(format!(
            "troupe-descendant-scan-test-{}",
            uuid::Uuid::new_v4()
        ));
        write_proc_process(&proc_root, 42000, "agent", 'R', 1, 42000, 200);
        write_proc_process(&proc_root, 42001, "tool wrapper", 'R', 42000, 42000, 201);
        write_proc_process(
            &proc_root,
            42002,
            "detached) worker",
            'S',
            42001,
            42002,
            202,
        );
        write_proc_process(&proc_root, 42003, "unrelated", 'R', 1, 42003, 203);

        let descendants = descendant_processes_at(&proc_root, 42000);
        assert_eq!(
            descendants
                .iter()
                .map(|process| process.pid)
                .collect::<Vec<_>>(),
            vec![42002, 42001],
        );
        let detached_identity = descendants[0];
        assert!(process_is_live_at(&proc_root, detached_identity));

        write_proc_process(&proc_root, 42002, "reused pid", 'R', 1, 42002, 999);
        assert!(!process_is_live_at(&proc_root, detached_identity));

        std::fs::remove_dir_all(&proc_root).unwrap();
    }

    #[test]
    fn retained_descendant_identity_remains_a_scan_root_after_the_agent_exits() {
        let proc_root = std::env::temp_dir().join(format!(
            "troupe-retained-descendant-test-{}",
            uuid::Uuid::new_v4()
        ));
        write_proc_process(&proc_root, 43000, "agent", 'R', 1, 43000, 300);
        write_proc_process(&proc_root, 43001, "detached worker", 'S', 43000, 43001, 301);

        let retained = descendant_processes_at(&proc_root, 43000);
        assert_eq!(
            retained
                .iter()
                .map(|process| process.pid)
                .collect::<Vec<_>>(),
            vec![43001],
        );

        std::fs::remove_dir_all(proc_root.join("43000")).unwrap();
        write_proc_process(&proc_root, 43001, "detached worker", 'S', 1, 43001, 301);
        write_proc_process(&proc_root, 43002, "nested tool", 'S', 43001, 43002, 302);

        let discovered = descendant_processes_from_live_roots_at(&proc_root, &retained);
        assert_eq!(
            discovered
                .iter()
                .map(|process| process.pid)
                .collect::<Vec<_>>(),
            vec![43002],
        );

        write_proc_process(&proc_root, 43001, "reused pid", 'R', 1, 43001, 999);
        assert!(descendant_processes_from_live_roots_at(&proc_root, &retained).is_empty());

        std::fs::remove_dir_all(&proc_root).unwrap();
    }

    #[tokio::test]
    async fn acp_frame_reader_enforces_the_exact_sixteen_mibibyte_boundary() {
        async fn read_frame(size: usize) -> (std::io::Result<Vec<u8>>, bool) {
            const PREFIX: &[u8] = br#"{"jsonrpc":"2.0","id":"size","result":""#;
            const SUFFIX: &[u8] = br#""}"#;
            assert!(size >= PREFIX.len() + SUFFIX.len());
            let mut wire = Vec::with_capacity(size + 1);
            wire.extend_from_slice(PREFIX);
            wire.resize(size - SUFFIX.len(), b'x');
            wire.extend_from_slice(SUFFIX);
            wire.push(b'\n');
            let exceeded = Arc::new(AtomicBool::new(false));
            let mut reader = AcpFrameLimitedReader::new(
                wire.as_slice(),
                Arc::clone(&exceeded),
                Arc::new(ProtocolViolationBoundary::default()),
            );
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
        let mut reader = AcpFrameLimitedReader::new(
            br#"{"jsonrpc":"2.0","id":1,"result":"first"}
{"jsonrpc":"2.0","id":2,"result":"second"}
"#
            .as_slice(),
            Arc::clone(&exceeded),
            Arc::new(ProtocolViolationBoundary::default()),
        );
        let mut output = Vec::new();
        reader.read_to_end(&mut output).await.unwrap();
        assert_eq!(
            output,
            br#"{"jsonrpc":"2.0","id":1,"result":"first"}
{"jsonrpc":"2.0","id":2,"result":"second"}
"#,
        );
        assert!(!exceeded.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn acp_frame_reader_enforces_protocol_depth_63_64_65() {
        let nested_frame = |depth: usize| {
            let mut nested = serde_json::json!(null);
            for _ in 3..depth {
                nested = serde_json::json!({"nested": nested});
            }
            let value = serde_json::json!({
                "jsonrpc": "2.0",
                "id": "depth",
                "result": {"_meta": nested},
            });
            let mut frame = serde_json::to_vec(&value).unwrap();
            frame.push(b'\n');
            frame
        };

        for depth in [63, 64] {
            let exceeded = Arc::new(AtomicBool::new(false));
            let frame = nested_frame(depth);
            let mut reader = AcpFrameLimitedReader::new(
                frame.as_slice(),
                Arc::clone(&exceeded),
                Arc::new(ProtocolViolationBoundary::default()),
            );
            let mut output = Vec::new();
            reader.read_to_end(&mut output).await.unwrap();
            assert_eq!(output, frame);
            assert!(!exceeded.load(Ordering::Acquire));
        }

        let exceeded = Arc::new(AtomicBool::new(false));
        let frame = nested_frame(65);
        let mut reader = AcpFrameLimitedReader::new(
            frame.as_slice(),
            Arc::clone(&exceeded),
            Arc::new(ProtocolViolationBoundary::default()),
        );
        let mut output = Vec::new();
        assert_eq!(
            reader.read_to_end(&mut output).await.unwrap_err().kind(),
            std::io::ErrorKind::InvalidData,
        );
        assert!(exceeded.load(Ordering::Acquire));

        let exceeded = Arc::new(AtomicBool::new(false));
        let mut reader = AcpFrameLimitedReader::new(
            b"{not-json}\n".as_slice(),
            Arc::clone(&exceeded),
            Arc::new(ProtocolViolationBoundary::default()),
        );
        let mut output = Vec::new();
        assert_eq!(
            reader.read_to_end(&mut output).await.unwrap_err().kind(),
            std::io::ErrorKind::InvalidData,
        );
        assert!(!exceeded.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn acp_frame_reader_rejects_malformed_json_rpc_response_envelope() {
        let exceeded = Arc::new(AtomicBool::new(false));
        let protocol_violation = Arc::new(ProtocolViolationBoundary::default());
        let frame = br#"{"jsonrpc":"2.0","id":"request","result":{},"error":{"code":-32603,"message":"ambiguous"}}
"#;
        let mut reader = AcpFrameLimitedReader::new(
            frame.as_slice(),
            Arc::clone(&exceeded),
            Arc::clone(&protocol_violation),
        );
        let mut output = Vec::new();

        assert_eq!(
            reader.read_to_end(&mut output).await.unwrap_err().kind(),
            std::io::ErrorKind::InvalidData,
        );
        assert!(!exceeded.load(Ordering::Acquire));
        assert!(protocol_violation.is_observed());
    }

    #[test]
    fn acp_response_tracker_rejects_unknown_and_duplicate_response_ids() {
        let protocol_violation = Arc::new(ProtocolViolationBoundary::default());
        let tracker = AcpResponseTracker::new(Arc::clone(&protocol_violation));
        tracker
            .observe_outgoing(
                r#"{"jsonrpc":"2.0","id":"expected","method":"initialize","params":{}}"#,
            )
            .unwrap();
        tracker
            .observe_incoming(r#"{"jsonrpc":"2.0","id":"expected","result":{}}"#)
            .unwrap();
        assert!(!protocol_violation.is_observed());

        assert_eq!(
            tracker
                .observe_incoming(r#"{"jsonrpc":"2.0","id":"expected","result":{}}"#)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidData,
        );
        assert!(protocol_violation.is_observed());
    }

    #[tokio::test]
    async fn acp_response_tracker_signals_only_after_the_response_is_flushed() {
        let tracker = AcpResponseTracker::new(Arc::new(ProtocolViolationBoundary::default()));
        let response_id = serde_json::from_str::<RequestId>(r#""late-request""#).unwrap();
        let mut flushed = tracker.wait_for_response_flush(response_id);
        let response = r#"{"jsonrpc":"2.0","id":"late-request","error":{"code":-32601,"message":"Method not found"}}"#;

        tracker.observe_outgoing(response).unwrap();
        assert!(flushed.try_recv().is_err());
        tracker.observe_outgoing_flushed(response);
        flushed.await.unwrap();
    }

    #[tokio::test]
    async fn response_then_request_batch_predeclares_deferred_settlement_evidence() {
        let boundary = Arc::new(ProtocolViolationBoundary::default());
        let tracker = AcpResponseTracker::new(Arc::clone(&boundary));
        tracker
            .observe_outgoing(
                r#"{"jsonrpc":"2.0","id":"prompt","method":"session/prompt","params":{}}"#,
            )
            .unwrap();
        tracker
            .observe_incoming(
                r#"[{"jsonrpc":"2.0","id":"prompt","result":{"stopReason":"end_turn"}},{"jsonrpc":"2.0","id":"late","method":"terminal/create","params":{}}]"#,
            )
            .unwrap();
        let late_id = serde_json::from_str::<RequestId>(r#""late""#).unwrap();
        assert!(boundary.is_observed());
        assert!(tracker.response_was_predeferred(&late_id));
        let waiter = tokio::spawn({
            let boundary = Arc::clone(&boundary);
            async move { boundary.wait_for_settlement_evidence().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        let response =
            r#"{"jsonrpc":"2.0","id":"late","error":{"code":-32601,"message":"Method not found"}}"#;
        tracker.observe_outgoing(response).unwrap();
        tracker.observe_outgoing_flushed(response);
        assert!(waiter.await.unwrap());
        assert!(!tracker.response_was_predeferred(&late_id));
    }

    #[tokio::test]
    async fn deferred_protocol_violation_waits_for_every_response_flush() {
        let boundary = Arc::new(ProtocolViolationBoundary::default());

        boundary.begin_deferred_response();
        boundary.begin_deferred_response();
        assert!(boundary.is_observed());
        let waiter_boundary = Arc::clone(&boundary);
        let waiter =
            tokio::spawn(async move { waiter_boundary.wait_for_settlement_evidence().await });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        assert!(!boundary.complete_deferred_response());
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        assert!(boundary.complete_deferred_response());
        assert!(waiter.await.unwrap());
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

    #[tokio::test]
    async fn abandoned_cancelling_turn_cannot_publish_ready_without_settlement() {
        let slot = AgentSessionSlot::new();
        slot.commit_ready(
            AgentReadySnapshot {
                pid: std::process::id(),
                session_id: "abandoned-turn-test".to_owned(),
                agent_info: None,
                agent_capabilities: AgentCapabilities::default(),
                generation: 1,
                server_name: "abandoned-turn-test".to_owned(),
                endpoint: "http://127.0.0.1:1/mcp".to_owned(),
                effective_model: "test-model".to_owned(),
                effective_effort: None,
            },
            None,
            None,
        );
        let (_, session_turn, _) = slot.claim_session_turn().await.unwrap();
        session_turn.cancelling_marker().mark_cancelling();

        drop(session_turn);

        assert!(matches!(
            &*lock(&slot.state),
            AgentSessionState::Cancelling(_)
        ));
    }
}
