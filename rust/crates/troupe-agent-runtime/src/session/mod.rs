use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
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
use tokio::sync::{Notify, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
use tokio_util::sync::CancellationToken;

pub(super) mod supervisor;
pub(super) mod turn;

use crate::adapter::{AcpAgentAdapter, agent_adapter};
use crate::diagnostics::observer::AgentDiagnosticObserver;
use crate::diagnostics::session::{
    self as diagnostic_session, AgentDiagnosticProvider, AgentSessionDiagnosticContext,
    AgentSessionDiagnosticMetadata, SessionDiagnosticCleanupHandle, SessionDiagnostics,
};
use crate::error::{AgentSessionFailure, AgentStartupFailure};
#[cfg(feature = "agent-test-support")]
use crate::launch::TestOpeningGate;
use crate::launch::process::{SpawnedAgent, spawn_agent, terminate_and_reap};
use crate::launch::{
    AgentLaunchSpec, OpeningRequestPhaseV1, ResolvedAgentCommand, ResolvedModeApplication,
};
use crate::profile::{ResolvedAgentProfile, WorkspaceLeaseV1};
use crate::result::{ResultMcpService, ResultRoute, fill_secure_random};
use crate::schema::CompiledActSchema;
use crate::schema::validation_bridge::PythonSchemaValidationBridge;
use crate::session::turn::{
    AgentTurnControl, AgentTurnOutcome, AgentTurnRequest, PromptResponseProvenance,
    run_agent_turn_worker,
};

const ACP_FRAME_MAX_BYTES: usize = 16 * 1024 * 1024;
const ACP_JSON_MAX_DEPTH: usize = 64;
const OPENING_CRASH_LOOP_THRESHOLD: u8 = 3;
const NPM_ERROR_CODE_LINE_MAX_BYTES: usize = 128;

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

pub(crate) struct NpxPreparationGate {
    state: Mutex<NpxPreparationState>,
    changed: Notify,
}

#[derive(Default)]
struct NpxPreparationState {
    ready: bool,
    failure: Option<AgentStartupFailure>,
    leader: Option<u64>,
    next_generation: u64,
}

pub(crate) enum NpxPreparationAdmission {
    Prepared,
    Failed(AgentStartupFailure),
    Leader(NpxPreparationLeader),
    Cancelled,
}

pub(crate) struct NpxPreparationLeader {
    gate: Arc<NpxPreparationGate>,
    generation: u64,
    completed: AtomicBool,
}

impl NpxPreparationGate {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(NpxPreparationState::default()),
            changed: Notify::new(),
        })
    }

    pub(crate) async fn enter(
        self: &Arc<Self>,
        cancellation: &CancellationToken,
    ) -> NpxPreparationAdmission {
        loop {
            let changed = self.changed.notified();
            {
                let mut state = lock(&self.state);
                if let Some(failure) = &state.failure {
                    return NpxPreparationAdmission::Failed(failure.clone());
                }
                if state.ready {
                    return NpxPreparationAdmission::Prepared;
                }
                if state.leader.is_none() {
                    let generation = state.next_generation;
                    state.next_generation = state.next_generation.wrapping_add(1);
                    state.leader = Some(generation);
                    return NpxPreparationAdmission::Leader(NpxPreparationLeader {
                        gate: Arc::clone(self),
                        generation,
                        completed: AtomicBool::new(false),
                    });
                }
            }
            tokio::select! {
                () = changed => {}
                () = cancellation.cancelled() => {
                    return NpxPreparationAdmission::Cancelled;
                }
            }
        }
    }
}

impl NpxPreparationLeader {
    fn mark_ready(&self) {
        let mut state = lock(&self.gate.state);
        if state.leader == Some(self.generation) {
            state.ready = true;
            state.leader = None;
            self.completed.store(true, Ordering::Release);
        }
        drop(state);
        self.gate.changed.notify_waiters();
    }

    fn mark_failed(&self, failure: AgentStartupFailure) {
        let mut state = lock(&self.gate.state);
        if state.leader == Some(self.generation) {
            state.failure = Some(failure);
            state.leader = None;
            self.completed.store(true, Ordering::Release);
        }
        drop(state);
        self.gate.changed.notify_waiters();
    }

    fn is_completed(&self) -> bool {
        self.completed.load(Ordering::Acquire)
    }
}

impl Drop for NpxPreparationLeader {
    fn drop(&mut self) {
        if self.completed.load(Ordering::Acquire) {
            return;
        }
        let mut state = lock(&self.gate.state);
        if state.leader == Some(self.generation) {
            state.leader = None;
        }
        drop(state);
        self.gate.changed.notify_waiters();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NpxPreparationFailureKind {
    Transient,
    Deterministic,
}

#[derive(Default)]
struct NpxPreparationStderrClassifier {
    line: Vec<u8>,
    line_overflowed: bool,
    overflowed_npm_record: bool,
    evidence: Option<NpxPreparationFailureKind>,
    ambiguous_evidence: bool,
}

impl NpxPreparationStderrClassifier {
    fn push(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if byte == b'\n' {
                self.finish_line();
            } else if !self.line_overflowed {
                if self.line.len() < NPM_ERROR_CODE_LINE_MAX_BYTES {
                    self.line.push(byte);
                } else {
                    self.overflowed_npm_record = npm_error_code(&self.line).is_some();
                    self.line.clear();
                    self.line_overflowed = true;
                }
            }
        }
    }

    fn finish(mut self) -> Option<NpxPreparationFailureKind> {
        self.finish_line();
        (!self.ambiguous_evidence)
            .then_some(self.evidence)
            .flatten()
    }

    fn finish_line(&mut self) {
        if self.overflowed_npm_record {
            self.ambiguous_evidence = true;
        } else if !self.line_overflowed {
            let line = self.line.strip_suffix(b"\r").unwrap_or(&self.line);
            if let Some(record) = classify_npm_error_code_line(line) {
                match record {
                    NpmErrorCodeRecord::Known(kind) => match self.evidence {
                        None => self.evidence = Some(kind),
                        Some(previous) if previous == kind => {}
                        Some(_) => self.ambiguous_evidence = true,
                    },
                    NpmErrorCodeRecord::Unknown => self.ambiguous_evidence = true,
                }
            }
        }
        self.line.clear();
        self.line_overflowed = false;
        self.overflowed_npm_record = false;
    }
}

enum NpmErrorCodeRecord {
    Known(NpxPreparationFailureKind),
    Unknown,
}

fn npm_error_code(line: &[u8]) -> Option<&[u8]> {
    line.strip_prefix(b"npm error code ")
        .or_else(|| line.strip_prefix(b"npm ERR! code "))
}

fn classify_npm_error_code_line(line: &[u8]) -> Option<NpmErrorCodeRecord> {
    let code = npm_error_code(line)?;
    if code.is_empty()
        || !code
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_')
    {
        return Some(NpmErrorCodeRecord::Unknown);
    }
    match code {
        b"E404" | b"ETARGET" => Some(NpmErrorCodeRecord::Known(
            NpxPreparationFailureKind::Deterministic,
        )),
        b"EAI_AGAIN"
        | b"EADDRINUSE"
        | b"ECONNECTIONTIMEOUT"
        | b"ECONNREFUSED"
        | b"ECONNRESET"
        | b"EIDLETIMEOUT"
        | b"ERR_SOCKET_TIMEOUT"
        | b"ERESPONSETIMEOUT"
        | b"ESOCKETTIMEDOUT"
        | b"ETIMEDOUT"
        | b"ETRANSFERTIMEOUT"
        | b"E408"
        | b"E420"
        | b"E429" => Some(NpmErrorCodeRecord::Known(
            NpxPreparationFailureKind::Transient,
        )),
        [b'E', b'5', tens, ones] if tens.is_ascii_digit() && ones.is_ascii_digit() => Some(
            NpmErrorCodeRecord::Known(NpxPreparationFailureKind::Transient),
        ),
        _ => Some(NpmErrorCodeRecord::Unknown),
    }
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

#[cfg(feature = "agent-test-support")]
pub struct AgentInfoSnapshotForTest {
    pub name: String,
    pub title: Option<String>,
    pub version: String,
}

#[cfg(feature = "agent-test-support")]
pub struct AgentReadySnapshotForTest {
    pub pid: u32,
    pub session_id: String,
    pub agent_info: Option<AgentInfoSnapshotForTest>,
    pub load_session: bool,
    pub mcp_http: bool,
    pub generation: u64,
    pub server_name: String,
    pub endpoint: String,
    pub effective_model: String,
    pub effective_effort: Option<String>,
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
    Closing,
    Closed,
}

pub struct AgentSessionSlot {
    state: Mutex<AgentSessionState>,
    diagnostics: SessionDiagnostics,
    diagnostic_provider: Option<AgentDiagnosticProvider>,
    changed: Notify,
    cancellation: CancellationToken,
    terminal_fault: CancellationToken,
    ready_committed: AtomicBool,
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

pub struct ActAdmissionLease {
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
pub enum AgentActError {
    Startup(AgentStartupFailure),
    SessionBroken(AgentSessionFailure),
    SessionClosed,
    CallerCancelled,
}

impl AgentSessionSlot {
    #[cfg(test)]
    pub(crate) fn new() -> Arc<Self> {
        Self::new_with_diagnostic_observer(None)
    }

    #[cfg(test)]
    pub(crate) fn new_with_diagnostic_observer(
        diagnostic_observer: Option<AgentDiagnosticObserver>,
    ) -> Arc<Self> {
        Self::new_with_diagnostics(SessionDiagnostics::new(diagnostic_observer), None)
    }

    pub(crate) fn new_with_session_diagnostics(
        diagnostic_observer: Option<AgentDiagnosticObserver>,
        diagnostic_context: Option<AgentSessionDiagnosticContext>,
        profile: &ResolvedAgentProfile,
    ) -> Arc<Self> {
        let provider = AgentDiagnosticProvider::from_agent_kind(profile.agent);
        Self::new_with_diagnostics(
            SessionDiagnostics::from_profile(diagnostic_observer, diagnostic_context, profile),
            Some(provider),
        )
    }

    fn new_with_diagnostics(
        diagnostics: SessionDiagnostics,
        diagnostic_provider: Option<AgentDiagnosticProvider>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(AgentSessionState::Opening),
            diagnostics,
            diagnostic_provider,
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
    pub(crate) fn inert(
        profile: &ResolvedAgentProfile,
        diagnostic_observer: Option<AgentDiagnosticObserver>,
        diagnostic_context: Option<AgentSessionDiagnosticContext>,
    ) -> Arc<Self> {
        let slot =
            Self::new_with_session_diagnostics(diagnostic_observer, diagnostic_context, profile);
        slot.observe_opening();
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

    pub(crate) fn diagnostic_observer(&self) -> Option<&AgentDiagnosticObserver> {
        self.diagnostics.observer()
    }

    pub(crate) fn diagnostic_context(&self) -> Option<AgentSessionDiagnosticContext> {
        self.diagnostics.context()
    }

    pub(crate) fn diagnostic_metadata(&self) -> Option<Arc<AgentSessionDiagnosticMetadata>> {
        self.diagnostics.metadata()
    }

    pub(crate) fn diagnostic_cleanup_handle(&self) -> Option<SessionDiagnosticCleanupHandle> {
        self.diagnostics.cleanup_handle()
    }

    pub(crate) fn observe_opening(&self) {
        diagnostic_session::observe_opening(&self.diagnostics);
    }

    fn observe_opening_attempt(&self, generation: u64) {
        diagnostic_session::observe_opening_attempt(&self.diagnostics, generation);
    }

    fn observe_update(
        &self,
        turn: Option<&AgentTurnControl>,
        session_id: &SessionId,
        update: &SessionUpdate,
    ) {
        let context = turn.and_then(AgentTurnControl::diagnostic_context);
        diagnostic_session::observe_update(&self.diagnostics, context.as_ref(), session_id, update);
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

    pub fn try_claim_admission(self: &Arc<Self>) -> Option<ActAdmissionLease> {
        self.caller_admission
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| ActAdmissionLease {
                slot: Arc::clone(self),
                claimed: true,
            })
    }

    pub async fn run_turn(
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
        if let Some(provider) = self.diagnostic_provider {
            control.bind_diagnostic_metadata(provider, &session.snapshot, operation_id, turn_index);
        } else {
            debug_assert!(control.diagnostic_context().is_none());
        }
        let diagnostic_context = control.diagnostic_context();
        let armed_result = match route.arm_result_with_diagnostics(
            operation_id,
            turn_index,
            schema,
            validation_bridge,
            diagnostic_context,
        ) {
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
                    AgentSessionState::Closing | AgentSessionState::Closed => {
                        Some(Err(AgentActError::SessionClosed))
                    }
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
    pub fn has_queued_turn_for_test(&self) -> bool {
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
            | AgentSessionState::Closing
            | AgentSessionState::Closed => (failure, false),
        };
        drop(state);
        if committed {
            diagnostic_session::observe_broken(&self.diagnostics, failure.code);
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
            diagnostic_session::observe_broken(
                &self.diagnostics,
                failure
                    .as_ref()
                    .expect("an entered Broken transition has a failure")
                    .code,
            );
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

    pub fn cancel(&self) {
        self.cancellation.cancel();
        let (delivery, entered_closing) = {
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
            let entered_closing = !matches!(
                *state,
                AgentSessionState::Closing | AgentSessionState::Closed
            );
            if entered_closing {
                *state = AgentSessionState::Closing;
                self.changed.notify_waiters();
            }
            (
                control.map(|control| {
                    let cleanup =
                        control.prepare_terminal_delivery(AgentSessionFailure::transport_lost());
                    (control, cleanup)
                }),
                entered_closing,
            )
        };
        if entered_closing {
            diagnostic_session::observe_closing(&self.diagnostics);
        }
        self.finish_closed_state_if_cleanup_complete();
        if let Some((control, cleanup)) = delivery {
            control.finish_terminal_delivery(cleanup);
        }
        self.terminal_fault.cancel();
        self.turn_requested.notify_waiters();
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
                AgentSessionState::Closing | AgentSessionState::Closed => {
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
    pub async fn readiness_for_test(
        &self,
    ) -> Result<AgentReadySnapshotForTest, AgentStartupFailure> {
        let snapshot = self.readiness().await?;
        Ok(AgentReadySnapshotForTest {
            pid: snapshot.pid,
            session_id: snapshot.session_id.clone(),
            agent_info: snapshot
                .agent_info
                .as_ref()
                .map(|info| AgentInfoSnapshotForTest {
                    name: info.name.clone(),
                    title: info.title.clone(),
                    version: info.version.clone(),
                }),
            load_session: snapshot.agent_capabilities.load_session,
            mcp_http: snapshot.agent_capabilities.mcp_capabilities.http,
            generation: snapshot.generation,
            server_name: snapshot.server_name.clone(),
            endpoint: snapshot.endpoint.clone(),
            effective_model: snapshot.effective_model.clone(),
            effective_effort: snapshot.effective_effort.clone(),
        })
    }

    #[cfg(feature = "agent-test-support")]
    pub fn state_name(&self) -> &'static str {
        match &*lock(&self.state) {
            AgentSessionState::Opening => "opening",
            AgentSessionState::BackingOff => "backing_off",
            AgentSessionState::Ready(_) => "ready",
            AgentSessionState::Active(_) => "active",
            AgentSessionState::Cancelling(_) => "cancelling",
            AgentSessionState::AuthRequired(_) => "auth_required",
            AgentSessionState::StartFailed(_) => "start_failed",
            AgentSessionState::Broken(_) => "broken",
            AgentSessionState::Closing => "closing",
            AgentSessionState::Closed => "closed",
        }
    }

    #[cfg(feature = "agent-test-support")]
    pub fn mode_transition_name_for_test(&self) -> &'static str {
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
        if self.cleanup_complete.swap(true, Ordering::AcqRel) {
            return;
        }
        self.finish_closed_state_if_cleanup_complete();
        diagnostic_session::observe_closed(&self.diagnostics);
        self.cleanup_changed.notify_waiters();
    }

    fn finish_closed_state_if_cleanup_complete(&self) {
        if !self.cleanup_is_complete() {
            return;
        }
        let closed = {
            let mut state = lock(&self.state);
            if matches!(*state, AgentSessionState::Closing) {
                *state = AgentSessionState::Closed;
                true
            } else {
                false
            }
        };
        if closed {
            self.changed.notify_waiters();
        }
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
            let AgentSessionState::Ready(session) = &*state else {
                unreachable!("the Ready state was just committed")
            };
            diagnostic_session::observe_ready(&self.diagnostics, &session.snapshot);
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
            let code = failure.code;
            *state = if failure.authentication_required {
                AgentSessionState::AuthRequired(failure)
            } else {
                AgentSessionState::StartFailed(failure)
            };
            self.changed.notify_waiters();
            diagnostic_session::observe_broken(&self.diagnostics, code);
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
                    let code = startup_failure.code;
                    *state = AgentSessionState::StartFailed(startup_failure);
                    self.changed.notify_waiters();
                    diagnostic_session::observe_broken(&self.diagnostics, code);
                    return;
                }
                AgentSessionState::Ready(_)
                | AgentSessionState::Active(_)
                | AgentSessionState::Cancelling(_) => failure,
                AgentSessionState::AuthRequired(_)
                | AgentSessionState::StartFailed(_)
                | AgentSessionState::Closing
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
            diagnostic_session::observe_broken(&self.diagnostics, failure.code);
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

    pub fn commit_transport_loss(&self) {
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
        self.cancellation.cancel();
        let mut state = lock(&self.state);
        let entered_closing = !matches!(
            *state,
            AgentSessionState::Closing | AgentSessionState::Closed
        );
        if entered_closing {
            *state = if self.cleanup_complete.load(Ordering::Acquire) {
                AgentSessionState::Closed
            } else {
                AgentSessionState::Closing
            };
        }
        drop(state);
        if entered_closing {
            diagnostic_session::observe_closing(&self.diagnostics);
        }
        self.changed.notify_waiters();
    }
}

pub(crate) fn spawn_opening(
    slot: &Arc<AgentSessionSlot>,
    profile: Arc<ResolvedAgentProfile>,
    spec: &'static AgentLaunchSpec,
    command: ResolvedAgentCommand,
    result_service: Arc<ResultMcpService>,
    package_preparation: Option<Arc<NpxPreparationGate>>,
    cleanup_complete: Box<dyn FnOnce(Arc<AgentSessionSlot>) + Send>,
) {
    let diagnostic_cleanup = slot.diagnostic_cleanup_handle();
    let slot = Arc::downgrade(slot);
    pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
        open_agent_session(
            slot.clone(),
            profile,
            spec,
            command,
            result_service,
            package_preparation,
        )
        .await;
        if let Some(slot) = slot.upgrade() {
            slot.mark_cleanup_complete();
            cleanup_complete(slot);
        } else if let Some(diagnostic_cleanup) = diagnostic_cleanup {
            diagnostic_cleanup.complete();
        }
    });
}

async fn open_agent_session(
    slot: Weak<AgentSessionSlot>,
    profile: Arc<ResolvedAgentProfile>,
    spec: &'static AgentLaunchSpec,
    command: ResolvedAgentCommand,
    result_service: Arc<ResultMcpService>,
    package_preparation: Option<Arc<NpxPreparationGate>>,
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

    let package_leader = match package_preparation {
        Some(gate) => match gate.enter(&cancellation).await {
            NpxPreparationAdmission::Prepared => None,
            NpxPreparationAdmission::Failed(failure) => {
                if cancellation.is_cancelled() {
                    return;
                }
                commit_failure(&slot, failure);
                return;
            }
            NpxPreparationAdmission::Leader(leader) => Some(leader),
            NpxPreparationAdmission::Cancelled => return,
        },
        None => None,
    };
    if cancellation.is_cancelled() {
        return;
    }

    let mut generation = 1_u64;
    let mut retry_ordinal = 0_u32;
    let mut previous_ambiguous = None;
    let mut consecutive_ambiguous = 0_u8;
    loop {
        let Some(strong_slot) = slot.upgrade() else {
            return;
        };
        strong_slot.observe_opening_attempt(generation);
        drop(strong_slot);
        let outcome = run_opening_attempt(
            &slot,
            &profile,
            spec,
            &command,
            &result_service,
            &endpoint,
            generation,
            &cancellation,
            package_leader.as_ref(),
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
    package_leader: Option<&NpxPreparationLeader>,
) -> OpeningAttemptOutcome {
    if cancellation.is_cancelled() {
        return OpeningAttemptOutcome::Cancelled;
    }
    if let Err(failure) = revalidate_workspace(&profile.workspace, "preparation") {
        return OpeningAttemptOutcome::Terminal(failure);
    }

    let spawned = match spawn_agent(command, &profile.workspace) {
        Ok(spawned) => spawned,
        Err(failure) => return OpeningAttemptOutcome::Terminal(failure),
    };
    let SpawnedAgent {
        mut guardian,
        adapter_pid: pid,
        stdin,
        stdout,
        mut stderr,
        mut shutdown,
    } = spawned;
    let classify_npx_preparation = package_leader.is_some();
    let stderr_drain = Some({
        pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
            let mut classifier = NpxPreparationStderrClassifier::default();
            let mut buffer = [0_u8; 8192];
            while let Ok(read) = stderr.read(&mut buffer).await {
                if read == 0 {
                    break;
                }
                if classify_npx_preparation {
                    classifier.push(&buffer[..read]);
                }
            }
            classify_npx_preparation
                .then(|| classifier.finish())
                .flatten()
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
                                let configuration_observation = configuration_monitor_for_handler
                                    .observe(
                                        &notification,
                                        agent_adapter(spec.agent),
                                        turn_is_active,
                                    );
                                let invalid_after_ready = matches!(
                                    configuration_observation,
                                    ConfigurationObservation::InvalidAfterReady
                                );
                                let mut accepted_for_diagnostics = matches!(
                                    configuration_observation,
                                    ConfigurationObservation::Accepted
                                );
                                if invalid_after_ready {
                                    accepted_for_diagnostics = false;
                                    if let Some(slot) = slot_for_updates.upgrade() {
                                        slot.commit_protocol_violation("configure");
                                    }
                                } else if !configuration_update {
                                    let accepted = (session_scoped_update
                                        && configuration_monitor_for_handler
                                            .owns_session(&notification.session_id))
                                        || submitted_turn.as_ref().is_some_and(|control| {
                                            control.accepts_ordinary_update()
                                        });
                                    accepted_for_diagnostics &= accepted;
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
                                if accepted_for_diagnostics
                                    && let Some(slot) = slot_for_updates.upgrade()
                                {
                                    slot.observe_update(
                                        submitted_turn.as_deref(),
                                        &notification.session_id,
                                        &notification.update,
                                    );
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
                            package_leader,
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
        let child_wait = guardian.wait();
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
            () = cancellation.cancelled() => OpeningAttemptCompletion::Cancelled,
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
    let route = { lock(&current_route).take() };
    if let Some(route) = route {
        result_service.revoke_route(&route).await;
    }
    terminate_and_reap(&mut guardian, &mut shutdown).await;
    let npx_preparation_failure = wait_for_stderr_drain(stderr_drain).await;

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
    if let Some(package_leader) = package_leader
        && !package_leader.is_completed()
        && let Some(failure_kind) = npx_preparation_failure
    {
        match failure_kind {
            NpxPreparationFailureKind::Transient => {
                return OpeningAttemptOutcome::Transient;
            }
            NpxPreparationFailureKind::Deterministic => {
                let failure = preparation_failure();
                package_leader.mark_failed(failure.clone());
                return OpeningAttemptOutcome::Terminal(failure);
            }
        }
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
    package_leader: Option<&NpxPreparationLeader>,
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
    if let Some(package_leader) = package_leader {
        package_leader.mark_ready();
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

async fn wait_for_stderr_drain(
    stderr_drain: Option<tokio::task::JoinHandle<Option<NpxPreparationFailureKind>>>,
) -> Option<NpxPreparationFailureKind> {
    if let Some(stderr_drain) = stderr_drain {
        return stderr_drain.await.unwrap_or(None);
    }
    None
}

fn commit_failure(slot: &Weak<AgentSessionSlot>, failure: AgentStartupFailure) {
    if let Some(slot) = slot.upgrade() {
        slot.commit_failure(failure);
    }
}

#[cfg(test)]
mod diagnostic_lifecycle_tests {
    use super::*;

    struct Destination;

    fn diagnostic_slot() -> (Arc<AgentSessionSlot>, SessionDiagnosticCleanupHandle) {
        let observer = AgentDiagnosticObserver::from_destination(Arc::new(Destination));
        let diagnostics = SessionDiagnostics::for_test(observer);
        let slot = AgentSessionSlot::new_with_diagnostics(
            diagnostics,
            Some(AgentDiagnosticProvider::Codex),
        );
        let cleanup = slot
            .diagnostic_cleanup_handle()
            .expect("the diagnostic lifecycle owns a cleanup latch");
        (slot, cleanup)
    }

    #[test]
    fn diagnostic_inert_cleanup_closes_only_after_logical_cancellation() {
        let (slot, cleanup) = diagnostic_slot();

        slot.mark_cleanup_complete();
        assert!(matches!(*lock(&slot.state), AgentSessionState::Opening));
        assert_eq!(cleanup.lifecycle_counts(), (0, 0));

        slot.cancel();
        assert!(matches!(*lock(&slot.state), AgentSessionState::Closed));
        assert_eq!(cleanup.lifecycle_counts(), (1, 1));
        slot.cancel();
        assert_eq!(cleanup.lifecycle_counts(), (1, 1));
    }

    #[test]
    fn diagnostic_cancel_stays_closing_until_cleanup_completes() {
        let (slot, cleanup) = diagnostic_slot();

        slot.cancel();
        assert!(matches!(*lock(&slot.state), AgentSessionState::Closing));
        assert_eq!(cleanup.lifecycle_counts(), (1, 0));
        slot.cancel();
        assert_eq!(cleanup.lifecycle_counts(), (1, 0));

        slot.mark_cleanup_complete();
        assert!(matches!(*lock(&slot.state), AgentSessionState::Closed));
        assert_eq!(cleanup.lifecycle_counts(), (1, 1));
        slot.mark_cleanup_complete();
        assert_eq!(cleanup.lifecycle_counts(), (1, 1));
    }

    #[test]
    fn diagnostic_start_failure_enters_the_same_cleanup_lifecycle() {
        let (slot, cleanup) = diagnostic_slot();
        slot.commit_failure(AgentStartupFailure::start(
            "preparation_failed",
            "preparation",
            "test startup failure",
        ));
        assert!(matches!(
            *lock(&slot.state),
            AgentSessionState::StartFailed(_)
        ));

        slot.cancel();
        assert!(matches!(*lock(&slot.state), AgentSessionState::Closing));
        assert_eq!(cleanup.lifecycle_counts(), (1, 0));
        slot.mark_cleanup_complete();
        assert!(matches!(*lock(&slot.state), AgentSessionState::Closed));
        assert_eq!(cleanup.lifecycle_counts(), (1, 1));
    }

    #[test]
    fn diagnostic_drop_leaves_closed_to_the_cleanup_guardian() {
        let (slot, cleanup) = diagnostic_slot();

        drop(slot);
        assert_eq!(cleanup.lifecycle_counts(), (1, 0));

        cleanup.complete();
        assert_eq!(cleanup.lifecycle_counts(), (1, 1));
        cleanup.complete();
        assert_eq!(cleanup.lifecycle_counts(), (1, 1));
    }

    #[test]
    fn diagnostic_cancel_and_cleanup_race_keeps_lifecycle_order() {
        let (slot, cleanup) = diagnostic_slot();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let cancel_slot = Arc::clone(&slot);
        let cancel_barrier = Arc::clone(&barrier);
        let cancel = std::thread::spawn(move || {
            cancel_barrier.wait();
            cancel_slot.cancel();
        });
        let cleanup_slot = Arc::clone(&slot);
        let cleanup_barrier = Arc::clone(&barrier);
        let complete = std::thread::spawn(move || {
            cleanup_barrier.wait();
            cleanup_slot.mark_cleanup_complete();
        });

        barrier.wait();
        cancel.join().expect("cancel thread finishes");
        complete.join().expect("cleanup thread finishes");

        assert!(matches!(*lock(&slot.state), AgentSessionState::Closed));
        assert_eq!(cleanup.lifecycle_counts(), (1, 1));
        assert_eq!(cleanup.lifecycle_observations(), ["closing", "closed"]);
    }

    #[test]
    fn diagnostic_session_metadata_tracks_attempt_and_ready_generation() {
        let (slot, _cleanup) = diagnostic_slot();
        slot.observe_opening_attempt(7);
        let opening = slot
            .diagnostics
            .metadata()
            .expect("opening metadata is retained");
        assert_eq!(opening.context().actor_id(), "actor-test");
        assert_eq!(opening.context().session_id(), "session-test");
        assert_eq!(opening.provider(), AgentDiagnosticProvider::Codex);
        assert_eq!(opening.generation(), Some(7));
        assert_eq!(opening.requested_model(), "test-model");
        assert_eq!(opening.effective_model(), None);

        assert!(slot.commit_ready(
            AgentReadySnapshot {
                pid: std::process::id(),
                session_id: "provider-session-7".to_owned(),
                agent_info: None,
                agent_capabilities: AgentCapabilities::default(),
                generation: 7,
                server_name: "test-result-route".to_owned(),
                endpoint: "http://127.0.0.1:1/mcp".to_owned(),
                effective_model: "effective-model".to_owned(),
                effective_effort: Some("high".to_owned()),
            },
            None,
            None,
        ));
        let ready = slot
            .diagnostics
            .metadata()
            .expect("ready metadata is retained");
        assert_eq!(ready.generation(), Some(7));
        assert_eq!(ready.provider_session_id(), Some("provider-session-7"));
        assert_eq!(ready.effective_model(), Some("effective-model"));
        assert_eq!(ready.effective_effort(), Some("high"));
    }

    #[test]
    fn diagnostics_disabled_slot_has_no_cleanup_observer_state() {
        let slot = AgentSessionSlot::new();
        assert!(slot.diagnostic_cleanup_handle().is_none());
    }
}

#[cfg(test)]
mod tests;
