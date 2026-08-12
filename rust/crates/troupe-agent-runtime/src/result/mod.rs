use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use agent_client_protocol::schema::v1::{HttpHeader, McpServer, McpServerHttp};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::{Bytes, BytesMut};
use http_body_util::{BodyExt as _, Full};
use hyper::body::{Body, Incoming};
use hyper::header::{ACCEPT, AUTHORIZATION, CONNECTION, CONTENT_TYPE, ORIGIN};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Version};
use hyper_util::rt::TokioIo;
use pyo3::PyErr;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::error::AgentStartupFailure;
#[cfg(feature = "agent-test-support")]
use crate::launch::TestOpeningGate;
use crate::launch::fd_registry::ForkTracked;
use crate::schema::validation_bridge::{CustomValidationOutcome, PythonSchemaValidationBridge};
use crate::schema::{
    CompiledActSchema, CustomValidationJob, NativeValidationOutcome, ValidatedActValue,
    ValidationIssue,
};

#[cfg(any(test, feature = "agent-test-support"))]
const MCP_REVISION: &str = "2025-11-25";
const MCP_PATH: &str = "/mcp";
const RESULT_TOOL: &str = "troupe_submit_result";
const MAX_CONNECTIONS: usize = 65_536;
const MCP_HTTP_HEAD_MAX_BYTES: usize = 32 * 1024;
const MCP_HTTP_BODY_MAX_BYTES: usize = 8 * 1024 * 1024;
const MCP_JSON_MAX_DEPTH: usize = 64;
pub const MAX_REPAIRABLE_INVALID_CALLS: u8 = 8;
const VALIDATION_DETAIL_MAX_ISSUES: usize = 16;
const VALIDATION_DETAIL_MAX_BYTES: usize = 32 * 1024;
static SECURE_RANDOM_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BodyCollectionError {
    Invalid,
    TooLarge,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn fill_secure_random(bytes: &mut [u8]) -> Result<(), getrandom::Error> {
    let _guard = lock(&SECURE_RANDOM_LOCK);
    getrandom::fill(bytes)
}

enum ServiceState {
    Unstarted,
    Starting,
    Ready { endpoint: String, origin: String },
    Failed(AgentStartupFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RoutePhase {
    New,
    InitializeWriting,
    Initialized,
    InitializedNotificationWriting,
    ClientInitialized,
    ToolsListWriting,
    ReadyPending,
    Ready,
    Revoked,
}

#[derive(Debug)]
struct ResultRouteEpoch {
    session_generation: u64,
    state: Mutex<ResultRouteEpochState>,
}

#[derive(Debug)]
struct ResultRouteEpochState {
    revoked: bool,
    next_arm_generation: u64,
    active: Option<(u64, Arc<ResultSlot>)>,
}

#[derive(Debug)]
struct ResultSlot {
    session_generation: u64,
    arm_generation: u64,
    operation_id: Uuid,
    turn_index: u64,
    state: Mutex<ResultSlotState>,
    failure: CancellationToken,
}

#[derive(Debug)]
enum ResultSlotState {
    Awaiting {
        schema: Arc<CompiledActSchema>,
        validation_bridge: Option<Arc<PythonSchemaValidationBridge>>,
        invalid_calls: u8,
        last_invalid: Option<(Vec<ValidationIssue>, bool)>,
    },
    Accepted(ValidatedActValue),
    Rejected {
        invalid_calls: u8,
        issues: Vec<ValidationIssue>,
        truncated: bool,
    },
    Settling {
        callback_error: Option<PyErr>,
    },
    Disarmed,
}

impl ResultSlotState {
    fn validation_bridge(&self) -> Option<Arc<PythonSchemaValidationBridge>> {
        match self {
            Self::Awaiting {
                validation_bridge, ..
            } => validation_bridge.as_ref().map(Arc::clone),
            Self::Accepted(_) | Self::Rejected { .. } | Self::Settling { .. } | Self::Disarmed => {
                None
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ResultRequestLease {
    session_generation: u64,
    route_epoch: Arc<ResultRouteEpoch>,
    active_result: Option<(u64, Arc<ResultSlot>)>,
}

#[derive(Debug)]
pub(crate) struct ArmedResultLease {
    route_epoch: Arc<ResultRouteEpoch>,
    arm_generation: u64,
    slot: Arc<ResultSlot>,
    armed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResultArmError {
    RouteRevoked,
    AlreadyArmed,
    GenerationExhausted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ResultSubmission {
    NoActiveSlot,
    TurnUnavailable,
    Invalid {
        issues: Vec<ValidationIssue>,
        truncated: bool,
        invalid_calls: u8,
    },
    Rejected {
        invalid_calls: u8,
    },
    Accepted,
    AlreadySubmitted,
    ResultContractRejected,
    HybridValidationRequired,
    SchemaCallbackFailed,
}

#[derive(Debug)]
pub(crate) enum ResultAtSettlement {
    Accepted(ValidatedActValue),
    Missing,
    Rejected {
        issues: Vec<ValidationIssue>,
        invalid_calls: u8,
        truncated: bool,
    },
    SchemaCallbackFailed(PyErr),
    Unavailable,
}

pub(crate) struct PreparedResultSettlement {
    outcome: Option<ResultAtSettlement>,
    validation_bridge: Option<Arc<PythonSchemaValidationBridge>>,
}

impl PreparedResultSettlement {
    pub(crate) fn take_outcome(&mut self) -> ResultAtSettlement {
        self.outcome
            .take()
            .expect("a prepared result settlement has one outcome")
    }

    pub(crate) fn take_committed_failure(&mut self) -> Option<ResultFailureAtHandoff> {
        let outcome = self.outcome.take()?;
        match outcome {
            ResultAtSettlement::Rejected {
                issues,
                invalid_calls,
                truncated,
            } if invalid_calls > MAX_REPAIRABLE_INVALID_CALLS => {
                Some(ResultFailureAtHandoff::Rejected {
                    issues,
                    invalid_calls,
                    truncated,
                })
            }
            ResultAtSettlement::SchemaCallbackFailed(error) => {
                Some(ResultFailureAtHandoff::SchemaCallbackFailed(error))
            }
            outcome => {
                self.outcome = Some(outcome);
                None
            }
        }
    }

    pub(crate) fn discard_outcome(&mut self) {
        let _ = self.outcome.take();
    }

    pub(crate) fn finish(self) {
        if let Some(validation_bridge) = self.validation_bridge {
            validation_bridge.close();
        }
    }
}

#[derive(Debug)]
pub(crate) enum ResultFailureAtHandoff {
    Rejected {
        issues: Vec<ValidationIssue>,
        invalid_calls: u8,
        truncated: bool,
    },
    SchemaCallbackFailed(PyErr),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResultCancelHandoff {
    Cancelled,
    FailurePreceded,
    Unavailable,
}

#[derive(Debug)]
pub(crate) enum ResultSubmissionStart {
    Completed(ResultSubmission),
    Validated(ValidatedResultCandidate),
}

#[derive(Debug)]
pub(crate) struct ValidatedResultCandidate {
    request: ResultRequestLease,
    schema: Arc<CompiledActSchema>,
    value: ValidatedActValue,
    custom_jobs: Vec<CustomValidationJob>,
}

#[cfg(feature = "agent-test-support")]
impl RoutePhase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::InitializeWriting => "initialize_writing",
            Self::Initialized => "initialized",
            Self::InitializedNotificationWriting => "initialized_notification_writing",
            Self::ClientInitialized => "client_initialized",
            Self::ToolsListWriting => "tools_list_writing",
            Self::ReadyPending => "ready_pending",
            Self::Ready => "ready",
            Self::Revoked => "revoked",
        }
    }
}

impl ResultRouteEpoch {
    fn new(session_generation: u64) -> Arc<Self> {
        Arc::new(Self {
            session_generation,
            state: Mutex::new(ResultRouteEpochState {
                revoked: false,
                next_arm_generation: 0,
                active: None,
            }),
        })
    }

    fn arm(
        self: &Arc<Self>,
        operation_id: Uuid,
        turn_index: u64,
        schema: Arc<CompiledActSchema>,
        validation_bridge: Option<Arc<PythonSchemaValidationBridge>>,
    ) -> Result<ArmedResultLease, ResultArmError> {
        let mut epoch = lock(&self.state);
        if epoch.revoked {
            return Err(ResultArmError::RouteRevoked);
        }
        if epoch.active.is_some() {
            return Err(ResultArmError::AlreadyArmed);
        }
        let arm_generation = epoch
            .next_arm_generation
            .checked_add(1)
            .ok_or(ResultArmError::GenerationExhausted)?;
        epoch.next_arm_generation = arm_generation;
        let slot = Arc::new(ResultSlot {
            session_generation: self.session_generation,
            arm_generation,
            operation_id,
            turn_index,
            state: Mutex::new(ResultSlotState::Awaiting {
                schema,
                validation_bridge,
                invalid_calls: 0,
                last_invalid: None,
            }),
            failure: CancellationToken::new(),
        });
        epoch.active = Some((arm_generation, Arc::clone(&slot)));
        Ok(ArmedResultLease {
            route_epoch: Arc::clone(self),
            arm_generation,
            slot,
            armed: true,
        })
    }

    fn acquire_request(self: &Arc<Self>) -> ResultRequestLease {
        let epoch = lock(&self.state);
        ResultRequestLease {
            session_generation: self.session_generation,
            route_epoch: Arc::clone(self),
            active_result: (!epoch.revoked)
                .then(|| {
                    epoch
                        .active
                        .as_ref()
                        .map(|(generation, slot)| (*generation, Arc::clone(slot)))
                })
                .flatten(),
        }
    }

    fn disarm(&self, arm_generation: u64, expected: &Arc<ResultSlot>) {
        let mut epoch = lock(&self.state);
        let matches = epoch.active.as_ref().is_some_and(|(generation, slot)| {
            *generation == arm_generation && Arc::ptr_eq(slot, expected)
        });
        if !matches {
            return;
        }
        let validation_bridge = {
            let mut state = lock(&expected.state);
            let validation_bridge = state.validation_bridge();
            if let Some(validation_bridge) = &validation_bridge {
                validation_bridge.begin_close();
            }
            *state = ResultSlotState::Disarmed;
            validation_bridge
        };
        epoch.active = None;
        drop(epoch);
        if let Some(validation_bridge) = validation_bridge {
            validation_bridge.close();
        }
    }

    fn prepare_settlement(
        &self,
        arm_generation: u64,
        expected: &Arc<ResultSlot>,
    ) -> PreparedResultSettlement {
        let mut epoch = lock(&self.state);
        let matches = epoch.active.as_ref().is_some_and(|(generation, slot)| {
            *generation == arm_generation && Arc::ptr_eq(slot, expected)
        });
        if !matches {
            return PreparedResultSettlement {
                outcome: Some(ResultAtSettlement::Unavailable),
                validation_bridge: None,
            };
        }
        let (state, validation_bridge) = {
            let mut state = lock(&expected.state);
            let validation_bridge = state.validation_bridge();
            if let Some(validation_bridge) = &validation_bridge {
                validation_bridge.begin_close();
            }
            (
                std::mem::replace(&mut *state, ResultSlotState::Disarmed),
                validation_bridge,
            )
        };
        epoch.active = None;
        drop(epoch);
        PreparedResultSettlement {
            outcome: Some(result_at_settlement(state)),
            validation_bridge,
        }
    }

    fn begin_cancellation(
        &self,
        arm_generation: u64,
        expected: &Arc<ResultSlot>,
    ) -> ResultCancelHandoff {
        let epoch = lock(&self.state);
        let matches = epoch.active.as_ref().is_some_and(|(generation, slot)| {
            *generation == arm_generation && Arc::ptr_eq(slot, expected)
        });
        if epoch.revoked || !matches {
            return ResultCancelHandoff::Unavailable;
        }
        let mut bridge_to_close = None;
        let outcome = {
            let mut state = lock(&expected.state);
            match &mut *state {
                ResultSlotState::Awaiting {
                    validation_bridge, ..
                } => {
                    bridge_to_close = validation_bridge.as_ref().map(Arc::clone);
                    if let Some(validation_bridge) = validation_bridge {
                        validation_bridge.begin_close();
                    }
                    *state = ResultSlotState::Settling {
                        callback_error: None,
                    };
                    ResultCancelHandoff::Cancelled
                }
                ResultSlotState::Accepted(_) => {
                    *state = ResultSlotState::Settling {
                        callback_error: None,
                    };
                    ResultCancelHandoff::Cancelled
                }
                ResultSlotState::Rejected { .. }
                | ResultSlotState::Settling {
                    callback_error: Some(_),
                } => ResultCancelHandoff::FailurePreceded,
                ResultSlotState::Settling {
                    callback_error: None,
                } => ResultCancelHandoff::Cancelled,
                ResultSlotState::Disarmed => ResultCancelHandoff::Unavailable,
            }
        };
        drop(epoch);
        if let Some(validation_bridge) = bridge_to_close {
            validation_bridge.close();
        }
        outcome
    }

    fn take_failure_for_handoff(
        &self,
        arm_generation: u64,
        expected: &Arc<ResultSlot>,
    ) -> Option<ResultFailureAtHandoff> {
        let epoch = lock(&self.state);
        let matches = epoch.active.as_ref().is_some_and(|(generation, slot)| {
            *generation == arm_generation && Arc::ptr_eq(slot, expected)
        });
        if epoch.revoked || !matches {
            return None;
        }
        let mut state = lock(&expected.state);
        let outcome = match &mut *state {
            ResultSlotState::Rejected { .. } => {
                let ResultSlotState::Rejected {
                    issues,
                    invalid_calls,
                    truncated,
                } = std::mem::replace(
                    &mut *state,
                    ResultSlotState::Settling {
                        callback_error: None,
                    },
                )
                else {
                    unreachable!("the rejected result state was matched")
                };
                Some(ResultFailureAtHandoff::Rejected {
                    issues,
                    invalid_calls,
                    truncated,
                })
            }
            ResultSlotState::Settling { callback_error } => callback_error
                .take()
                .map(ResultFailureAtHandoff::SchemaCallbackFailed),
            ResultSlotState::Awaiting { .. }
            | ResultSlotState::Accepted(_)
            | ResultSlotState::Disarmed => None,
        };
        drop(state);
        drop(epoch);
        outcome
    }

    fn revoke(&self) {
        let mut epoch = lock(&self.state);
        epoch.revoked = true;
        let active = epoch.active.take();
        let validation_bridge = active.map(|(_, slot)| {
            let mut state = lock(&slot.state);
            let validation_bridge = state.validation_bridge();
            if let Some(validation_bridge) = &validation_bridge {
                validation_bridge.begin_close();
            }
            *state = ResultSlotState::Disarmed;
            validation_bridge
        });
        drop(epoch);
        if let Some(validation_bridge) = validation_bridge.flatten() {
            validation_bridge.close();
        }
    }
}

pub(crate) struct ResultRoute {
    token: String,
    pub(crate) server_name: String,
    pub(crate) generation: u64,
    mcp_revision: &'static str,
    endpoint: String,
    result_epoch: Arc<ResultRouteEpoch>,
    phase: Mutex<RoutePhase>,
    changed: Notify,
    connection_cancellation: CancellationToken,
    connections: Mutex<usize>,
    connections_changed: Notify,
    #[cfg(feature = "agent-test-support")]
    ready_gate: Option<Arc<TestOpeningGate>>,
}

impl ResultRoute {
    pub(crate) fn mcp_server(&self) -> McpServer {
        McpServer::Http(
            McpServerHttp::new(self.server_name.clone(), self.endpoint.clone()).headers(vec![
                HttpHeader::new("Authorization", format!("Bearer {}", self.token)),
            ]),
        )
    }

    pub(crate) async fn wait_ready(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<(), AgentStartupFailure> {
        loop {
            let changed = self.changed.notified();
            match *lock(&self.phase) {
                RoutePhase::Ready => return Ok(()),
                RoutePhase::Revoked => {
                    return Err(AgentStartupFailure::start(
                        "result_channel_unavailable",
                        "mcp_ready",
                        "agent result channel became unavailable",
                    ));
                }
                RoutePhase::New
                | RoutePhase::InitializeWriting
                | RoutePhase::Initialized
                | RoutePhase::InitializedNotificationWriting
                | RoutePhase::ClientInitialized
                | RoutePhase::ToolsListWriting
                | RoutePhase::ReadyPending => {}
            }
            tokio::select! {
                () = changed => {}
                () = cancellation.cancelled() => {
                    return Err(AgentStartupFailure::start(
                        "result_channel_unavailable",
                        "mcp_ready",
                        "agent result channel was closed",
                    ));
                }
            }
        }
    }

    pub(crate) fn arm_result(
        self: &Arc<Self>,
        operation_id: Uuid,
        turn_index: u64,
        schema: Arc<CompiledActSchema>,
        validation_bridge: Option<Arc<PythonSchemaValidationBridge>>,
    ) -> Result<ArmedResultLease, ResultArmError> {
        self.result_epoch
            .arm(operation_id, turn_index, schema, validation_bridge)
    }

    pub(crate) fn acquire_result_request(&self) -> ResultRequestLease {
        self.result_epoch.acquire_request()
    }

    fn revoke(&self) {
        self.result_epoch.revoke();
        *lock(&self.phase) = RoutePhase::Revoked;
        self.connection_cancellation.cancel();
        self.changed.notify_waiters();
        #[cfg(feature = "agent-test-support")]
        if let Some(gate) = &self.ready_gate {
            gate.release();
        }
    }

    fn publish_ready(self: &Arc<Self>) {
        #[cfg(feature = "agent-test-support")]
        if let Some(gate) = self.ready_gate.as_ref().map(Arc::clone) {
            let route = Arc::clone(self);
            pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
                gate.wait().await;
                route.commit_ready();
                gate.mark_completed();
            });
            return;
        }
        self.commit_ready();
    }

    fn commit_ready(&self) {
        let mut phase = lock(&self.phase);
        if *phase == RoutePhase::ReadyPending {
            *phase = RoutePhase::Ready;
            self.changed.notify_waiters();
        }
    }

    fn acquire_connection(self: &Arc<Self>) -> Option<RouteConnectionLease> {
        let phase = lock(&self.phase);
        if *phase == RoutePhase::Revoked {
            return None;
        }
        *lock(&self.connections) += 1;
        drop(phase);
        Some(RouteConnectionLease {
            route: Arc::clone(self),
        })
    }

    async fn wait_connections_closed(&self) {
        loop {
            let changed = self.connections_changed.notified();
            if *lock(&self.connections) == 0 {
                return;
            }
            changed.await;
        }
    }

    async fn wait_stable_phase(&self) -> Option<RoutePhase> {
        loop {
            let changed = self.changed.notified();
            let phase = *lock(&self.phase);
            match phase {
                RoutePhase::InitializeWriting
                | RoutePhase::InitializedNotificationWriting
                | RoutePhase::ToolsListWriting => changed.await,
                RoutePhase::Revoked => return None,
                RoutePhase::New
                | RoutePhase::Initialized
                | RoutePhase::ClientInitialized
                | RoutePhase::ReadyPending
                | RoutePhase::Ready => return Some(phase),
            }
        }
    }
}

impl ResultRequestLease {
    fn with_current_state<T>(
        &self,
        apply: impl FnOnce(&Arc<ResultSlot>, &mut ResultSlotState) -> T,
    ) -> Result<T, ResultSubmission> {
        let Some((arm_generation, expected)) = &self.active_result else {
            return Err(ResultSubmission::NoActiveSlot);
        };
        if self.session_generation != self.route_epoch.session_generation {
            return Err(ResultSubmission::TurnUnavailable);
        }
        let epoch = lock(&self.route_epoch.state);
        if epoch.revoked {
            return Err(ResultSubmission::TurnUnavailable);
        }
        let current = epoch.active.as_ref().is_some_and(|(generation, slot)| {
            generation == arm_generation && Arc::ptr_eq(slot, expected)
        });
        if !current
            || expected.session_generation != self.session_generation
            || expected.arm_generation != *arm_generation
        {
            return Err(ResultSubmission::TurnUnavailable);
        }
        let mut state = lock(&expected.state);
        Ok(apply(expected, &mut state))
    }

    pub(crate) fn start_submission(&self, value: &Value) -> ResultSubmissionStart {
        let schema = match self.with_current_state(|_, state| match state {
            ResultSlotState::Awaiting { schema, .. } => Ok(Arc::clone(schema)),
            ResultSlotState::Accepted(_) => Err(ResultSubmission::AlreadySubmitted),
            ResultSlotState::Rejected { .. } => Err(ResultSubmission::ResultContractRejected),
            ResultSlotState::Settling { .. } | ResultSlotState::Disarmed => {
                Err(ResultSubmission::TurnUnavailable)
            }
        }) {
            Ok(Ok(schema)) => schema,
            Ok(Err(outcome)) | Err(outcome) => {
                return ResultSubmissionStart::Completed(outcome);
            }
        };

        match schema.validate(value) {
            NativeValidationOutcome::Invalid { issues, truncated } => {
                ResultSubmissionStart::Completed(self.publish_invalid(issues, truncated))
            }
            NativeValidationOutcome::Valid { value, custom_jobs } => {
                ResultSubmissionStart::Validated(ValidatedResultCandidate {
                    request: self.clone(),
                    schema,
                    value,
                    custom_jobs,
                })
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn submit_value(&self, value: &Value) -> ResultSubmission {
        match self.start_submission(value) {
            ResultSubmissionStart::Completed(outcome) => outcome,
            ResultSubmissionStart::Validated(candidate) if candidate.custom_jobs.is_empty() => {
                candidate.accept()
            }
            ResultSubmissionStart::Validated(_) => ResultSubmission::HybridValidationRequired,
        }
    }

    pub(crate) async fn submit_value_async(&self, value: &Value) -> ResultSubmission {
        let validation_bridge = match self.validation_bridge() {
            Ok(validation_bridge) => validation_bridge,
            Err(outcome) => return outcome,
        };
        let permit = if let Some(validation_bridge) = validation_bridge.as_ref() {
            let Some(permit) = validation_bridge.acquire().await else {
                return ResultSubmission::TurnUnavailable;
            };
            Some(permit)
        } else {
            None
        };
        let submission = match self.start_submission(value) {
            ResultSubmissionStart::Completed(outcome) => outcome,
            ResultSubmissionStart::Validated(candidate) if candidate.custom_jobs.is_empty() => {
                candidate.accept()
            }
            ResultSubmissionStart::Validated(candidate) => {
                let Some(validation_bridge) = validation_bridge.as_ref() else {
                    return ResultSubmission::HybridValidationRequired;
                };
                match validation_bridge
                    .validate_jobs(candidate.schema(), candidate.custom_jobs())
                    .await
                {
                    CustomValidationOutcome::Accepted => candidate.accept(),
                    CustomValidationOutcome::Rejected {
                        path,
                        message,
                        truncated,
                    } => candidate.reject(path, message, truncated),
                    CustomValidationOutcome::CallbackFailed(error) => {
                        candidate.callback_failed(error)
                    }
                    CustomValidationOutcome::Closed => ResultSubmission::TurnUnavailable,
                }
            }
        };
        drop(permit);
        submission
    }

    fn validation_bridge(
        &self,
    ) -> Result<Option<Arc<PythonSchemaValidationBridge>>, ResultSubmission> {
        match self.with_current_state(|_, state| match state {
            ResultSlotState::Awaiting {
                validation_bridge, ..
            } => Ok(validation_bridge.as_ref().map(Arc::clone)),
            ResultSlotState::Accepted(_) => Err(ResultSubmission::AlreadySubmitted),
            ResultSlotState::Rejected { .. } => Err(ResultSubmission::ResultContractRejected),
            ResultSlotState::Settling { .. } | ResultSlotState::Disarmed => {
                Err(ResultSubmission::TurnUnavailable)
            }
        }) {
            Ok(outcome) => outcome,
            Err(outcome) => Err(outcome),
        }
    }

    fn publish_invalid(&self, issues: Vec<ValidationIssue>, truncated: bool) -> ResultSubmission {
        let mut bridge_to_close = None;
        let outcome = self.with_current_state(|_, state| match state {
            ResultSlotState::Awaiting {
                validation_bridge,
                invalid_calls,
                last_invalid,
                ..
            } => {
                *invalid_calls = invalid_calls
                    .checked_add(1)
                    .expect("invalid-call count is terminal before overflow");
                let count = *invalid_calls;
                let (issues, truncated) = bound_validation_issues(issues, truncated, count);
                if count <= MAX_REPAIRABLE_INVALID_CALLS {
                    *last_invalid = Some((issues.clone(), truncated));
                    ResultSubmission::Invalid {
                        issues,
                        truncated,
                        invalid_calls: count,
                    }
                } else {
                    bridge_to_close = validation_bridge.as_ref().map(Arc::clone);
                    if let Some(validation_bridge) = validation_bridge {
                        validation_bridge.begin_close();
                    }
                    *state = ResultSlotState::Rejected {
                        invalid_calls: count,
                        issues,
                        truncated,
                    };
                    ResultSubmission::Rejected {
                        invalid_calls: count,
                    }
                }
            }
            ResultSlotState::Accepted(_) => ResultSubmission::AlreadySubmitted,
            ResultSlotState::Rejected { .. } => ResultSubmission::ResultContractRejected,
            ResultSlotState::Settling { .. } | ResultSlotState::Disarmed => {
                ResultSubmission::TurnUnavailable
            }
        });
        if let Some(validation_bridge) = bridge_to_close {
            validation_bridge.close();
        }
        if matches!(outcome, Ok(ResultSubmission::Rejected { .. }))
            && let Some((_, slot)) = &self.active_result
        {
            slot.failure.cancel();
        }
        outcome.unwrap_or_else(|outcome| outcome)
    }

    fn accept(
        &self,
        schema: &Arc<CompiledActSchema>,
        value: ValidatedActValue,
    ) -> ResultSubmission {
        let mut bridge_to_close = None;
        let outcome = self.with_current_state(|_, state| match state {
            ResultSlotState::Awaiting {
                schema: current,
                validation_bridge,
                ..
            } if Arc::ptr_eq(current, schema) => {
                bridge_to_close = validation_bridge.as_ref().map(Arc::clone);
                if let Some(validation_bridge) = validation_bridge {
                    validation_bridge.begin_close();
                }
                *state = ResultSlotState::Accepted(value);
                ResultSubmission::Accepted
            }
            ResultSlotState::Awaiting { .. } => ResultSubmission::TurnUnavailable,
            ResultSlotState::Accepted(_) => ResultSubmission::AlreadySubmitted,
            ResultSlotState::Rejected { .. } => ResultSubmission::ResultContractRejected,
            ResultSlotState::Settling { .. } | ResultSlotState::Disarmed => {
                ResultSubmission::TurnUnavailable
            }
        });
        if let Some(validation_bridge) = bridge_to_close {
            validation_bridge.close();
        }
        outcome.unwrap_or_else(|outcome| outcome)
    }

    fn callback_failed(&self, schema: &Arc<CompiledActSchema>, error: PyErr) -> ResultSubmission {
        let mut bridge_to_close = None;
        let outcome = self.with_current_state(|_, state| match state {
            ResultSlotState::Awaiting {
                schema: current,
                validation_bridge,
                ..
            } if Arc::ptr_eq(current, schema) => {
                bridge_to_close = validation_bridge.as_ref().map(Arc::clone);
                if let Some(validation_bridge) = validation_bridge {
                    validation_bridge.begin_close();
                }
                *state = ResultSlotState::Settling {
                    callback_error: Some(error),
                };
                ResultSubmission::SchemaCallbackFailed
            }
            ResultSlotState::Awaiting { .. } => ResultSubmission::TurnUnavailable,
            ResultSlotState::Accepted(_) => ResultSubmission::AlreadySubmitted,
            ResultSlotState::Rejected { .. } => ResultSubmission::ResultContractRejected,
            ResultSlotState::Settling { .. } | ResultSlotState::Disarmed => {
                ResultSubmission::TurnUnavailable
            }
        });
        if let Some(validation_bridge) = bridge_to_close {
            validation_bridge.close();
        }
        if matches!(outcome, Ok(ResultSubmission::SchemaCallbackFailed))
            && let Some((_, slot)) = &self.active_result
        {
            slot.failure.cancel();
        }
        outcome.unwrap_or_else(|outcome| outcome)
    }
}

impl ValidatedResultCandidate {
    pub(crate) fn custom_jobs(&self) -> &[CustomValidationJob] {
        &self.custom_jobs
    }

    pub(crate) fn schema(&self) -> &Arc<CompiledActSchema> {
        &self.schema
    }

    pub(crate) fn accept(self) -> ResultSubmission {
        self.request.accept(&self.schema, self.value)
    }

    pub(crate) fn reject(self, path: String, message: String, truncated: bool) -> ResultSubmission {
        self.request.publish_invalid(
            vec![ValidationIssue {
                path,
                code: "custom_validation",
                message,
            }],
            truncated,
        )
    }

    pub(crate) fn callback_failed(self, error: PyErr) -> ResultSubmission {
        self.request.callback_failed(&self.schema, error)
    }
}

impl ArmedResultLease {
    #[cfg(test)]
    pub(crate) fn terminal_failure_for_test(failure: ResultFailureAtHandoff) -> Self {
        let arm_generation = 1;
        let route_epoch = ResultRouteEpoch::new(1);
        let state = match failure {
            ResultFailureAtHandoff::Rejected {
                issues,
                invalid_calls,
                truncated,
            } => ResultSlotState::Rejected {
                issues,
                invalid_calls,
                truncated,
            },
            ResultFailureAtHandoff::SchemaCallbackFailed(error) => ResultSlotState::Settling {
                callback_error: Some(error),
            },
        };
        let slot = Arc::new(ResultSlot {
            session_generation: 1,
            arm_generation,
            operation_id: Uuid::from_u128(1),
            turn_index: 1,
            state: Mutex::new(state),
            failure: CancellationToken::new(),
        });
        {
            let mut epoch = lock(&route_epoch.state);
            epoch.next_arm_generation = arm_generation;
            epoch.active = Some((arm_generation, Arc::clone(&slot)));
        }
        Self {
            route_epoch,
            arm_generation,
            slot,
            armed: true,
        }
    }

    pub(crate) fn operation_id(&self) -> Uuid {
        self.slot.operation_id
    }

    pub(crate) fn turn_index(&self) -> u64 {
        self.slot.turn_index
    }

    pub(crate) fn failure_notification(&self) -> CancellationToken {
        self.slot.failure.clone()
    }

    pub(crate) fn begin_cancellation(&mut self) -> ResultCancelHandoff {
        if !self.armed {
            return ResultCancelHandoff::Unavailable;
        }
        self.route_epoch
            .begin_cancellation(self.arm_generation, &self.slot)
    }

    pub(crate) fn take_failure_for_handoff(&mut self) -> Option<ResultFailureAtHandoff> {
        self.armed.then(|| {
            self.route_epoch
                .take_failure_for_handoff(self.arm_generation, &self.slot)
        })?
    }

    #[cfg(test)]
    pub(crate) fn accepted_result(&self) -> Option<ValidatedActValue> {
        match &*lock(&self.slot.state) {
            ResultSlotState::Accepted(value) => Some(value.clone()),
            ResultSlotState::Awaiting { .. }
            | ResultSlotState::Rejected { .. }
            | ResultSlotState::Settling { .. }
            | ResultSlotState::Disarmed => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn settle(self) -> ResultAtSettlement {
        let mut prepared = self.prepare_settlement();
        let outcome = prepared.take_outcome();
        prepared.finish();
        outcome
    }

    pub(crate) fn prepare_settlement(mut self) -> PreparedResultSettlement {
        let prepared = if self.armed {
            self.route_epoch
                .prepare_settlement(self.arm_generation, &self.slot)
        } else {
            PreparedResultSettlement {
                outcome: Some(ResultAtSettlement::Unavailable),
                validation_bridge: None,
            }
        };
        self.armed = false;
        prepared
    }

    pub(crate) fn disarm(mut self) {
        self.release();
    }

    fn release(&mut self) {
        if self.armed {
            self.route_epoch.disarm(self.arm_generation, &self.slot);
            self.armed = false;
        }
    }
}

fn result_at_settlement(state: ResultSlotState) -> ResultAtSettlement {
    match state {
        ResultSlotState::Awaiting {
            invalid_calls: 0, ..
        } => ResultAtSettlement::Missing,
        ResultSlotState::Awaiting {
            invalid_calls,
            last_invalid,
            ..
        } => {
            let (issues, truncated) =
                last_invalid.expect("an invalid call retains bounded validation evidence");
            ResultAtSettlement::Rejected {
                issues,
                invalid_calls,
                truncated,
            }
        }
        ResultSlotState::Accepted(value) => ResultAtSettlement::Accepted(value),
        ResultSlotState::Rejected {
            invalid_calls,
            issues,
            truncated,
        } => ResultAtSettlement::Rejected {
            issues,
            invalid_calls,
            truncated,
        },
        ResultSlotState::Settling { callback_error } => callback_error
            .map(ResultAtSettlement::SchemaCallbackFailed)
            .unwrap_or(ResultAtSettlement::Unavailable),
        ResultSlotState::Disarmed => ResultAtSettlement::Unavailable,
    }
}

impl Drop for ArmedResultLease {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(any(test, feature = "agent-test-support"))]
fn route_for_test(token: &str, generation: u64) -> Arc<ResultRoute> {
    route_for_test_with_revision(token, generation, MCP_REVISION)
}

#[cfg(any(test, feature = "agent-test-support"))]
fn route_for_test_with_revision(
    token: &str,
    generation: u64,
    mcp_revision: &'static str,
) -> Arc<ResultRoute> {
    Arc::new(ResultRoute {
        token: token.to_owned(),
        server_name: "test-server".to_owned(),
        generation,
        mcp_revision,
        endpoint: "http://127.0.0.1:1/mcp".to_owned(),
        result_epoch: ResultRouteEpoch::new(generation),
        phase: Mutex::new(RoutePhase::New),
        changed: Notify::new(),
        connection_cancellation: CancellationToken::new(),
        connections: Mutex::new(0),
        connections_changed: Notify::new(),
        #[cfg(feature = "agent-test-support")]
        ready_gate: None,
    })
}

struct RouteConnectionLease {
    route: Arc<ResultRoute>,
}

impl Drop for RouteConnectionLease {
    fn drop(&mut self) {
        let mut connections = lock(&self.route.connections);
        *connections = connections
            .checked_sub(1)
            .expect("a route connection lease is released once");
        if *connections == 0 {
            self.route.connections_changed.notify_waiters();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestBodyFraming {
    Fixed(u64),
    Chunked,
}

#[derive(Debug)]
enum Http1ReadPhase {
    Head {
        bytes: usize,
        terminator_prefix: u8,
    },
    AwaitingBodyFraming {
        trailing: Vec<u8>,
        waker: Option<std::task::Waker>,
    },
    FixedBody {
        remaining: u64,
    },
    ChunkedBody(ChunkedBodyTracker),
    OpaqueBody,
}

impl Http1ReadPhase {
    const fn head() -> Self {
        Self::Head {
            bytes: 0,
            terminator_prefix: 0,
        }
    }

    fn observe(self, input: &[u8]) -> Self {
        match self {
            Self::Head {
                mut bytes,
                mut terminator_prefix,
            } => {
                for (index, byte) in input.iter().copied().enumerate() {
                    bytes += 1;
                    debug_assert!(bytes <= MCP_HTTP_HEAD_MAX_BYTES);
                    if advance_http_head_terminator(&mut terminator_prefix, byte) {
                        return Self::AwaitingBodyFraming {
                            trailing: input[index + 1..].to_vec(),
                            waker: None,
                        };
                    }
                }
                Self::Head {
                    bytes,
                    terminator_prefix,
                }
            }
            Self::AwaitingBodyFraming {
                mut trailing,
                waker,
            } => {
                trailing.extend_from_slice(input);
                Self::AwaitingBodyFraming { trailing, waker }
            }
            Self::FixedBody { remaining } => {
                let consumed = remaining.min(input.len() as u64) as usize;
                let remaining = remaining - consumed as u64;
                if consumed < input.len() {
                    debug_assert_eq!(remaining, 0);
                    Self::head().observe(&input[consumed..])
                } else {
                    Self::FixedBody { remaining }
                }
            }
            Self::ChunkedBody(mut tracker) => match tracker.consume(input) {
                ChunkedProgress::Incomplete => Self::ChunkedBody(tracker),
                ChunkedProgress::Complete(consumed) => Self::head().observe(&input[consumed..]),
                ChunkedProgress::Invalid => Self::OpaqueBody,
            },
            Self::OpaqueBody => Self::OpaqueBody,
        }
    }
}

#[derive(Debug)]
struct Http1ReadLimiter {
    phase: Http1ReadPhase,
}

impl Http1ReadLimiter {
    fn new() -> Self {
        Self {
            phase: Http1ReadPhase::head(),
        }
    }

    fn allowance(&mut self, capacity: usize, waker: &std::task::Waker) -> Option<usize> {
        loop {
            match &mut self.phase {
                Http1ReadPhase::Head { bytes, .. } => {
                    return Some(capacity.min(MCP_HTTP_HEAD_MAX_BYTES.saturating_sub(*bytes)));
                }
                Http1ReadPhase::AwaitingBodyFraming { waker: waiting, .. } => {
                    if !waiting.as_ref().is_some_and(|old| old.will_wake(waker)) {
                        *waiting = Some(waker.clone());
                    }
                    return None;
                }
                Http1ReadPhase::FixedBody { remaining: 0 } => {
                    self.phase = Http1ReadPhase::head();
                }
                Http1ReadPhase::FixedBody { remaining } => {
                    return Some(capacity.min((*remaining).min(usize::MAX as u64) as usize));
                }
                Http1ReadPhase::ChunkedBody(_) | Http1ReadPhase::OpaqueBody => {
                    return Some(capacity.min(MCP_HTTP_HEAD_MAX_BYTES));
                }
            }
        }
    }

    fn observe(&mut self, input: &[u8]) {
        let phase = std::mem::replace(&mut self.phase, Http1ReadPhase::OpaqueBody);
        self.phase = phase.observe(input);
    }

    fn set_body_framing(&mut self, framing: RequestBodyFraming) {
        let phase = std::mem::replace(&mut self.phase, Http1ReadPhase::OpaqueBody);
        let Http1ReadPhase::AwaitingBodyFraming {
            trailing,
            mut waker,
        } = phase
        else {
            panic!("Hyper dispatched a request without one complete raw HTTP/1.1 head");
        };
        let body = match framing {
            RequestBodyFraming::Fixed(remaining) => Http1ReadPhase::FixedBody { remaining },
            RequestBodyFraming::Chunked => Http1ReadPhase::ChunkedBody(ChunkedBodyTracker::new()),
        };
        self.phase = body.observe(&trailing);
        if let Some(waker) = waker.take() {
            waker.wake();
        }
    }
}

#[derive(Debug)]
struct ChunkedBodyTracker {
    phase: ChunkedReadPhase,
}

#[derive(Debug)]
enum ChunkedReadPhase {
    Size {
        value: u64,
        saw_digit: bool,
        in_linear_whitespace: bool,
        in_extension: bool,
        saw_carriage_return: bool,
    },
    Data {
        remaining: u64,
    },
    DataCarriageReturn,
    DataLineFeed,
    TrailerLine {
        has_bytes: bool,
        saw_carriage_return: bool,
    },
    Invalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChunkedProgress {
    Incomplete,
    Complete(usize),
    Invalid,
}

impl ChunkedBodyTracker {
    fn new() -> Self {
        Self {
            phase: Self::size_phase(),
        }
    }

    const fn size_phase() -> ChunkedReadPhase {
        ChunkedReadPhase::Size {
            value: 0,
            saw_digit: false,
            in_linear_whitespace: false,
            in_extension: false,
            saw_carriage_return: false,
        }
    }

    fn consume(&mut self, input: &[u8]) -> ChunkedProgress {
        let mut index = 0;
        while index < input.len() {
            match &mut self.phase {
                ChunkedReadPhase::Size {
                    value,
                    saw_digit,
                    in_linear_whitespace,
                    in_extension,
                    saw_carriage_return,
                } => {
                    let byte = input[index];
                    index += 1;
                    if *saw_carriage_return {
                        if byte != b'\n' {
                            self.phase = ChunkedReadPhase::Invalid;
                            return ChunkedProgress::Invalid;
                        }
                        self.phase = if *value == 0 {
                            ChunkedReadPhase::TrailerLine {
                                has_bytes: false,
                                saw_carriage_return: false,
                            }
                        } else {
                            ChunkedReadPhase::Data { remaining: *value }
                        };
                    } else if *in_extension {
                        if byte == b'\r' {
                            *saw_carriage_return = true;
                        } else if byte == b'\n' {
                            self.phase = ChunkedReadPhase::Invalid;
                            return ChunkedProgress::Invalid;
                        }
                    } else if *in_linear_whitespace {
                        match byte {
                            b' ' | b'\t' => {}
                            b';' => *in_extension = true,
                            b'\r' => *saw_carriage_return = true,
                            _ => {
                                self.phase = ChunkedReadPhase::Invalid;
                                return ChunkedProgress::Invalid;
                            }
                        }
                    } else if byte == b'\r' && *saw_digit {
                        *saw_carriage_return = true;
                    } else if byte == b';' && *saw_digit {
                        *in_extension = true;
                    } else if matches!(byte, b' ' | b'\t') && *saw_digit {
                        *in_linear_whitespace = true;
                    } else if let Some(digit) = (byte as char).to_digit(16) {
                        let Some(next) = value
                            .checked_mul(16)
                            .and_then(|current| current.checked_add(u64::from(digit)))
                        else {
                            self.phase = ChunkedReadPhase::Invalid;
                            return ChunkedProgress::Invalid;
                        };
                        *value = next;
                        *saw_digit = true;
                    } else {
                        self.phase = ChunkedReadPhase::Invalid;
                        return ChunkedProgress::Invalid;
                    }
                }
                ChunkedReadPhase::Data { remaining } => {
                    let consumed = (*remaining)
                        .min((input.len() - index) as u64)
                        .min(usize::MAX as u64) as usize;
                    index += consumed;
                    *remaining -= consumed as u64;
                    if *remaining == 0 {
                        self.phase = ChunkedReadPhase::DataCarriageReturn;
                    }
                }
                ChunkedReadPhase::DataCarriageReturn => {
                    if input[index] != b'\r' {
                        self.phase = ChunkedReadPhase::Invalid;
                        return ChunkedProgress::Invalid;
                    }
                    index += 1;
                    self.phase = ChunkedReadPhase::DataLineFeed;
                }
                ChunkedReadPhase::DataLineFeed => {
                    if input[index] != b'\n' {
                        self.phase = ChunkedReadPhase::Invalid;
                        return ChunkedProgress::Invalid;
                    }
                    index += 1;
                    self.phase = Self::size_phase();
                }
                ChunkedReadPhase::TrailerLine {
                    has_bytes,
                    saw_carriage_return,
                } => {
                    let byte = input[index];
                    index += 1;
                    if *saw_carriage_return {
                        if byte != b'\n' {
                            self.phase = ChunkedReadPhase::Invalid;
                            return ChunkedProgress::Invalid;
                        }
                        if !*has_bytes {
                            return ChunkedProgress::Complete(index);
                        }
                        *has_bytes = false;
                        *saw_carriage_return = false;
                    } else if byte == b'\r' {
                        *saw_carriage_return = true;
                    } else if byte == b'\n' {
                        self.phase = ChunkedReadPhase::Invalid;
                        return ChunkedProgress::Invalid;
                    } else {
                        *has_bytes = true;
                    }
                }
                ChunkedReadPhase::Invalid => return ChunkedProgress::Invalid,
            }
        }
        ChunkedProgress::Incomplete
    }
}

fn advance_http_head_terminator(prefix: &mut u8, byte: u8) -> bool {
    *prefix = match (*prefix, byte) {
        (0, b'\r') | (1, b'\r') | (3, b'\r') => 1,
        (1, b'\n') => 2,
        (2, b'\r') => 3,
        (3, b'\n') => return true,
        _ => 0,
    };
    false
}

struct ConnectionControl {
    route: Mutex<Option<RouteConnectionLease>>,
    transition: Mutex<Option<ResponseTransition>>,
    read_limiter: Mutex<Http1ReadLimiter>,
    route_bound: Notify,
}

impl ConnectionControl {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            route: Mutex::new(None),
            transition: Mutex::new(None),
            read_limiter: Mutex::new(Http1ReadLimiter::new()),
            route_bound: Notify::new(),
        })
    }

    fn bind_route(&self, route: &Arc<ResultRoute>) -> bool {
        let mut bound = lock(&self.route);
        if let Some(lease) = bound.as_ref() {
            return Arc::ptr_eq(&lease.route, route);
        }
        let Some(lease) = route.acquire_connection() else {
            return false;
        };
        *bound = Some(lease);
        drop(bound);
        self.route_bound.notify_waiters();
        true
    }

    fn begin_request(&self, framing: RequestBodyFraming) {
        assert!(
            lock(&self.transition).is_none(),
            "Hyper must flush the previous HTTP/1.1 response before dispatching another request"
        );
        lock(&self.read_limiter).set_body_framing(framing);
    }

    fn read_allowance(&self, capacity: usize, waker: &std::task::Waker) -> Option<usize> {
        lock(&self.read_limiter).allowance(capacity, waker)
    }

    fn observe_read(&self, input: &[u8]) {
        lock(&self.read_limiter).observe(input);
    }

    fn set_transition(&self, transition: Option<ResponseTransition>) {
        let previous = std::mem::replace(&mut *lock(&self.transition), transition);
        assert!(
            previous.is_none(),
            "an HTTP connection carries one in-flight response"
        );
    }

    async fn route_cancelled(&self) {
        loop {
            let bound = self.route_bound.notified();
            let cancellation = lock(&self.route)
                .as_ref()
                .map(|lease| lease.route.connection_cancellation.clone());
            if let Some(cancellation) = cancellation {
                cancellation.cancelled().await;
                return;
            }
            bound.await;
        }
    }

    fn finish(&self, response_written: bool) {
        if let Some(transition) = lock(&self.transition).take() {
            transition.finish(response_written);
        }
        lock(&self.route).take();
    }

    fn response_flushed(&self) {
        if let Some(transition) = lock(&self.transition).take() {
            transition.finish(true);
        }
    }

    fn response_flush_failed(&self) {
        if let Some(transition) = lock(&self.transition).take() {
            transition.finish(false);
        }
    }
}

struct ResponseTrackedIo<T> {
    inner: T,
    control: Arc<ConnectionControl>,
}

impl<T> ResponseTrackedIo<T> {
    fn new(inner: T, control: Arc<ConnectionControl>) -> Self {
        Self { inner, control }
    }
}

impl<T> tokio::io::AsyncRead for ResponseTrackedIo<T>
where
    T: tokio::io::AsyncRead + Unpin,
{
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let control = Arc::clone(&self.control);
        let Some(allowance) = control.read_allowance(buffer.remaining(), cx.waker()) else {
            return std::task::Poll::Pending;
        };
        debug_assert_ne!(allowance, 0);
        let (outcome, initialized, filled) = {
            let mut limited = buffer.take(allowance);
            let outcome = std::pin::Pin::new(&mut self.inner).poll_read(cx, &mut limited);
            let initialized = limited.initialized().len();
            let filled = limited.filled().len();
            if matches!(outcome, std::task::Poll::Ready(Ok(()))) {
                control.observe_read(limited.filled());
            }
            (outcome, initialized, filled)
        };
        // SAFETY: the delegated reader initialized this prefix of the same backing region.
        unsafe {
            buffer.assume_init(initialized);
        }
        if matches!(outcome, std::task::Poll::Ready(Ok(()))) {
            buffer.advance(filled);
        }
        outcome
    }
}

impl<T> tokio::io::AsyncWrite for ResponseTrackedIo<T>
where
    T: tokio::io::AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buffer)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match std::pin::Pin::new(&mut self.inner).poll_flush(cx) {
            std::task::Poll::Ready(Ok(())) => {
                self.control.response_flushed();
                std::task::Poll::Ready(Ok(()))
            }
            std::task::Poll::Ready(Err(error)) => {
                self.control.response_flush_failed();
                std::task::Poll::Ready(Err(error))
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffers: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write_vectored(cx, buffers)
    }
}

async fn complete_connection<F, E>(
    connection: F,
    control: Arc<ConnectionControl>,
    shutdown: &CancellationToken,
) where
    F: Future<Output = Result<(), E>>,
{
    let response_written = {
        tokio::pin!(connection);
        tokio::select! {
            result = &mut connection => result.is_ok(),
            () = control.route_cancelled() => false,
            () = shutdown.cancelled() => false,
        }
    };
    control.finish(response_written);
}

pub(crate) struct ResultMcpService {
    state: Mutex<ServiceState>,
    changed: Notify,
    routes: Mutex<HashMap<String, Arc<ResultRoute>>>,
    shutdown: CancellationToken,
    listener_failure: CancellationToken,
    connections: Arc<Semaphore>,
    accept_started: AtomicBool,
    accept_complete: AtomicBool,
    accept_changed: Notify,
    connection_tasks: AtomicUsize,
    connection_tasks_changed: Notify,
}

struct ConnectionTaskLease {
    service: Arc<ResultMcpService>,
}

impl Drop for ConnectionTaskLease {
    fn drop(&mut self) {
        let previous = self.service.connection_tasks.fetch_sub(1, Ordering::AcqRel);
        assert!(previous > 0, "an accepted connection task is released once");
        if previous == 1 {
            self.service.connection_tasks_changed.notify_waiters();
        }
    }
}

impl ResultMcpService {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ServiceState::Unstarted),
            changed: Notify::new(),
            routes: Mutex::new(HashMap::new()),
            shutdown: CancellationToken::new(),
            listener_failure: CancellationToken::new(),
            connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            accept_started: AtomicBool::new(false),
            accept_complete: AtomicBool::new(false),
            accept_changed: Notify::new(),
            connection_tasks: AtomicUsize::new(0),
            connection_tasks_changed: Notify::new(),
        })
    }

    pub(crate) fn shutdown(&self) {
        self.shutdown.cancel();
    }

    pub(crate) fn listener_failure(&self) -> CancellationToken {
        self.listener_failure.clone()
    }

    fn commit_listener_failure(&self) {
        let failure = AgentStartupFailure::start(
            "result_channel_unavailable",
            "preparation",
            "agent result service listener failed",
        );
        let mut state = lock(&self.state);
        if matches!(*state, ServiceState::Failed(_)) {
            return;
        }
        *state = ServiceState::Failed(failure);
        drop(state);
        self.changed.notify_waiters();
        self.listener_failure.cancel();
        self.shutdown.cancel();
    }

    #[cfg(feature = "agent-test-support")]
    pub(crate) fn fail_listener_for_test(&self) {
        self.commit_listener_failure();
    }

    pub(crate) async fn shutdown_and_wait(&self) {
        self.shutdown();
        loop {
            let changed = self.accept_changed.notified();
            if !self.accept_started.load(Ordering::Acquire)
                || self.accept_complete.load(Ordering::Acquire)
            {
                break;
            }
            changed.await;
        }
        loop {
            let changed = self.connection_tasks_changed.notified();
            if self.connection_tasks.load(Ordering::Acquire) == 0 {
                return;
            }
            changed.await;
        }
    }

    fn begin_connection_task(self: &Arc<Self>) -> ConnectionTaskLease {
        self.connection_tasks.fetch_add(1, Ordering::AcqRel);
        ConnectionTaskLease {
            service: Arc::clone(self),
        }
    }

    pub(crate) async fn ensure_ready(self: &Arc<Self>) -> Result<String, AgentStartupFailure> {
        loop {
            let changed = self.changed.notified();
            let initialize = {
                let mut state = lock(&self.state);
                match &*state {
                    ServiceState::Ready { endpoint, .. } => return Ok(endpoint.clone()),
                    ServiceState::Failed(failure) => return Err(failure.clone()),
                    ServiceState::Starting => false,
                    ServiceState::Unstarted => {
                        *state = ServiceState::Starting;
                        true
                    }
                }
            };
            if !initialize {
                changed.await;
                continue;
            }

            let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await {
                Ok(listener) => ForkTracked::new(listener),
                Err(_) => {
                    let failure = AgentStartupFailure::start(
                        "result_channel_unavailable",
                        "preparation",
                        "agent result service could not start",
                    );
                    *lock(&self.state) = ServiceState::Failed(failure.clone());
                    self.changed.notify_waiters();
                    return Err(failure);
                }
            };
            let address = match listener.local_addr() {
                Ok(address) => address,
                Err(_) => {
                    let failure = AgentStartupFailure::start(
                        "result_channel_unavailable",
                        "preparation",
                        "agent result service address is unavailable",
                    );
                    *lock(&self.state) = ServiceState::Failed(failure.clone());
                    self.changed.notify_waiters();
                    return Err(failure);
                }
            };
            let origin = format!("http://127.0.0.1:{}", address.port());
            let endpoint = format!("{origin}{MCP_PATH}");
            *lock(&self.state) = ServiceState::Ready {
                endpoint: endpoint.clone(),
                origin,
            };
            self.changed.notify_waiters();
            let service = Arc::clone(self);
            let completion = Arc::clone(self);
            self.accept_started.store(true, Ordering::Release);
            pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
                service.accept(listener).await;
                completion.accept_complete.store(true, Ordering::Release);
                completion.accept_changed.notify_waiters();
            });
            return Ok(endpoint);
        }
    }

    pub(crate) fn register_route(
        &self,
        generation: u64,
        mcp_revision: &'static str,
        #[cfg(feature = "agent-test-support")] ready_gate: Option<Arc<TestOpeningGate>>,
    ) -> Result<Arc<ResultRoute>, AgentStartupFailure> {
        let endpoint = match &*lock(&self.state) {
            ServiceState::Ready { endpoint, .. } => endpoint.clone(),
            _ => {
                return Err(AgentStartupFailure::start(
                    "result_channel_unavailable",
                    "session_new",
                    "agent result service is not ready",
                ));
            }
        };
        let mut random = [0_u8; 44];
        fill_secure_random(&mut random).map_err(|_| {
            AgentStartupFailure::start(
                "preparation_failed",
                "session_new",
                "agent result route could not be created",
            )
        })?;
        let token = URL_SAFE_NO_PAD.encode(&random[..32]);
        let suffix = URL_SAFE_NO_PAD.encode(&random[32..]);
        let route = Arc::new(ResultRoute {
            token: token.clone(),
            server_name: format!("troupe-result-{suffix}"),
            generation,
            mcp_revision,
            endpoint,
            result_epoch: ResultRouteEpoch::new(generation),
            phase: Mutex::new(RoutePhase::New),
            changed: Notify::new(),
            connection_cancellation: CancellationToken::new(),
            connections: Mutex::new(0),
            connections_changed: Notify::new(),
            #[cfg(feature = "agent-test-support")]
            ready_gate,
        });
        lock(&self.routes).insert(token, Arc::clone(&route));
        Ok(route)
    }

    pub(crate) async fn revoke_route(&self, route: &ResultRoute) {
        {
            let mut routes = lock(&self.routes);
            route.revoke();
            routes.remove(&route.token);
        }
        route.wait_connections_closed().await;
    }

    async fn accept(self: Arc<Self>, listener: ForkTracked<TcpListener>) {
        let mut listener_failed = false;
        loop {
            let accepted = tokio::select! {
                () = self.shutdown.cancelled() => break,
                accepted = listener.accept() => accepted,
            };
            let Ok((stream, address)) = accepted else {
                listener_failed = true;
                break;
            };
            let stream = ForkTracked::new(stream);
            if !address.ip().is_loopback() {
                continue;
            }
            let Ok(permit) = Arc::clone(&self.connections).try_acquire_owned() else {
                continue;
            };
            let service = Arc::clone(&self);
            let connection_task = service.begin_connection_task();
            let control = ConnectionControl::new();
            pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
                let io = TokioIo::new(ResponseTrackedIo::new(stream, Arc::clone(&control)));
                let service_for_request = Arc::clone(&service);
                let control_for_request = Arc::clone(&control);
                let builder = result_http1_builder();
                let connection = builder.serve_connection(
                    io,
                    service_fn(move |request| {
                        let service = Arc::clone(&service_for_request);
                        let control = Arc::clone(&control_for_request);
                        async move { service.handle(request, control).await }
                    }),
                );
                complete_connection(connection, control, &service.shutdown).await;
                drop(permit);
                drop(connection_task);
            });
        }
        if listener_failed && !self.shutdown.is_cancelled() {
            self.commit_listener_failure();
        }
    }

    async fn handle(
        &self,
        request: Request<Incoming>,
        control: Arc<ConnectionControl>,
    ) -> Result<Response<Full<Bytes>>, Infallible> {
        let framing = request
            .body()
            .size_hint()
            .exact()
            .map_or(RequestBodyFraming::Chunked, RequestBodyFraming::Fixed);
        control.begin_request(framing);
        if request.version() != Version::HTTP_11 {
            return Ok(empty(StatusCode::HTTP_VERSION_NOT_SUPPORTED));
        }
        let outcome = self.handle_inner(request, &control).await;
        control.set_transition(outcome.transition);
        let response = outcome.response;
        Ok(response)
    }

    async fn handle_inner<B>(
        &self,
        request: Request<B>,
        control: &ConnectionControl,
    ) -> DispatchOutcome
    where
        B: Body<Data = Bytes> + Unpin,
    {
        if request.uri().path() != MCP_PATH {
            return DispatchOutcome::plain(empty(StatusCode::NOT_FOUND));
        }
        if request.method() != Method::POST {
            return DispatchOutcome::plain(empty(StatusCode::METHOD_NOT_ALLOWED));
        }
        if !common_transport_headers_valid(self, request.headers()) {
            return DispatchOutcome::plain(empty(StatusCode::BAD_REQUEST));
        }
        let Some((route, request_lease)) = self.bind_authorized_request(request.headers(), control)
        else {
            return DispatchOutcome::plain(empty(StatusCode::UNAUTHORIZED));
        };
        let Some(phase) = route.wait_stable_phase().await else {
            return DispatchOutcome::plain(empty(StatusCode::GONE));
        };
        if !protocol_header_valid(request.headers(), phase, route.mcp_revision) {
            return DispatchOutcome::plain(empty(StatusCode::BAD_REQUEST));
        }
        let body = match collect_bounded_body(request.into_body()).await {
            Ok(body) => body,
            Err(BodyCollectionError::Invalid) => {
                return DispatchOutcome::plain(empty(StatusCode::BAD_REQUEST));
            }
            Err(BodyCollectionError::TooLarge) => {
                return DispatchOutcome::plain(empty_and_close(StatusCode::PAYLOAD_TOO_LARGE));
            }
        };
        let message: Value = match serde_json::from_slice(&body) {
            Ok(message) => message,
            Err(_) => return DispatchOutcome::plain(empty(StatusCode::BAD_REQUEST)),
        };
        if !json_depth_within(&message, MCP_JSON_MAX_DEPTH) {
            return DispatchOutcome::plain(empty(StatusCode::BAD_REQUEST));
        }
        dispatch_message(&route, request_lease, message).await
    }

    fn bind_authorized_request(
        &self,
        headers: &hyper::HeaderMap,
        control: &ConnectionControl,
    ) -> Option<(Arc<ResultRoute>, ResultRequestLease)> {
        let mut values = headers.get_all(AUTHORIZATION).iter();
        let value = values.next()?.to_str().ok()?;
        if values.next().is_some() {
            return None;
        }
        let token = value.strip_prefix("Bearer ")?;
        let routes = lock(&self.routes);
        let route = routes.get(token).map(Arc::clone)?;
        if !control.bind_route(&route) {
            return None;
        }
        let request = route.acquire_result_request();
        Some((route, request))
    }
}

fn result_http1_builder() -> http1::Builder {
    let mut builder = http1::Builder::new();
    builder.max_buf_size(MCP_HTTP_HEAD_MAX_BYTES);
    builder
}

impl Drop for ResultMcpService {
    fn drop(&mut self) {
        self.shutdown.cancel();
        let routes: Vec<_> = lock(&self.routes).drain().map(|(_, route)| route).collect();
        for route in routes {
            route.revoke();
        }
    }
}

fn common_transport_headers_valid(service: &ResultMcpService, headers: &hyper::HeaderMap) -> bool {
    let mut content_types = headers.get_all(CONTENT_TYPE).iter();
    let content_type = content_types.next().and_then(|value| value.to_str().ok());
    if content_types.next().is_some()
        || !content_type.is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
        })
    {
        return false;
    }
    let accepts = headers
        .get_all(ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','));
    let mut accepts = accepts.peekable();
    if accepts.peek().is_none() {
        return false;
    }
    let accepted: Vec<_> = accepts.collect();
    if !accepted
        .iter()
        .any(|value| acceptable_media_range(value, "application/json"))
        || !accepted
            .iter()
            .any(|value| acceptable_media_range(value, "text/event-stream"))
    {
        return false;
    }
    if headers.contains_key("mcp-session-id") {
        return false;
    }
    let mut origins = headers.get_all(ORIGIN).iter();
    if let Some(origin) = origins.next() {
        if origins.next().is_some() {
            return false;
        }
        let expected = match &*lock(&service.state) {
            ServiceState::Ready { origin, .. } => origin.clone(),
            _ => return false,
        };
        if origin.to_str().ok() != Some(&expected) {
            return false;
        }
    }
    true
}

fn acceptable_media_range(value: &str, expected: &str) -> bool {
    let mut parts = value.split(';');
    if !parts
        .next()
        .is_some_and(|media| media.trim().eq_ignore_ascii_case(expected))
    {
        return false;
    }
    let mut quality = None;
    for parameter in parts {
        let Some((name, value)) = parameter.split_once('=') else {
            return false;
        };
        if name.trim().eq_ignore_ascii_case("q") {
            if quality.is_some() {
                return false;
            }
            let Some(parsed) = quality_is_positive(value.trim()) else {
                return false;
            };
            quality = Some(parsed);
        }
    }
    quality.unwrap_or(true)
}

fn quality_is_positive(value: &str) -> Option<bool> {
    let (whole, fraction) = value
        .split_once('.')
        .map_or((value, None), |(whole, fraction)| (whole, Some(fraction)));
    if fraction.is_some_and(|fraction| {
        fraction.len() > 3 || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        return None;
    }
    match whole {
        "0" => Some(fraction.is_some_and(|fraction| fraction.bytes().any(|byte| byte != b'0'))),
        "1" if fraction.is_none_or(|fraction| fraction.bytes().all(|byte| byte == b'0')) => {
            Some(true)
        }
        _ => None,
    }
}

fn protocol_header_valid(
    headers: &hyper::HeaderMap,
    phase: RoutePhase,
    mcp_revision: &str,
) -> bool {
    let versions: Vec<_> = headers.get_all("mcp-protocol-version").iter().collect();
    match phase {
        RoutePhase::New => versions.is_empty(),
        RoutePhase::Initialized
        | RoutePhase::ClientInitialized
        | RoutePhase::ReadyPending
        | RoutePhase::Ready => {
            versions.len() == 1 && versions[0].to_str().ok() == Some(mcp_revision)
        }
        RoutePhase::InitializeWriting
        | RoutePhase::InitializedNotificationWriting
        | RoutePhase::ToolsListWriting
        | RoutePhase::Revoked => false,
    }
}

async fn collect_bounded_body<B>(mut body: B) -> Result<Bytes, BodyCollectionError>
where
    B: Body<Data = Bytes> + Unpin,
{
    let hint = body.size_hint();
    if hint.lower() > MCP_HTTP_BODY_MAX_BYTES as u64 {
        return Err(BodyCollectionError::TooLarge);
    }
    let capacity = hint
        .upper()
        .unwrap_or_default()
        .min(MCP_HTTP_BODY_MAX_BYTES as u64) as usize;
    let mut collected = BytesMut::with_capacity(capacity);
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| BodyCollectionError::Invalid)?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        if data.len() > MCP_HTTP_BODY_MAX_BYTES.saturating_sub(collected.len()) {
            return Err(BodyCollectionError::TooLarge);
        }
        collected.extend_from_slice(&data);
    }
    Ok(collected.freeze())
}

fn json_depth_within(value: &Value, maximum: usize) -> bool {
    let mut pending = vec![(value, 1_usize)];
    while let Some((value, depth)) = pending.pop() {
        if depth > maximum {
            return false;
        }
        match value {
            Value::Array(values) => {
                pending.extend(values.iter().map(|value| (value, depth + 1)));
            }
            Value::Object(values) => {
                pending.extend(values.values().map(|value| (value, depth + 1)));
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }
    true
}

struct DispatchOutcome {
    response: Response<Full<Bytes>>,
    transition: Option<ResponseTransition>,
}

impl DispatchOutcome {
    fn plain(response: Response<Full<Bytes>>) -> Self {
        Self {
            response,
            transition: None,
        }
    }
}

struct ResponseTransition {
    route: Arc<ResultRoute>,
    writing: RoutePhase,
    previous: RoutePhase,
    success: RoutePhase,
    finished: bool,
}

impl ResponseTransition {
    fn finish(mut self, response_written: bool) {
        self.apply(response_written);
        self.finished = true;
    }

    fn apply(&self, response_written: bool) {
        let mut phase = lock(&self.route.phase);
        if *phase != self.writing {
            return;
        }
        *phase = if response_written {
            self.success
        } else {
            self.previous
        };
        let publish_ready = response_written && self.success == RoutePhase::ReadyPending;
        self.route.changed.notify_waiters();
        drop(phase);
        if publish_ready {
            self.route.publish_ready();
        }
    }
}

impl Drop for ResponseTransition {
    fn drop(&mut self) {
        if !self.finished {
            self.apply(false);
        }
    }
}

async fn dispatch_message(
    route: &Arc<ResultRoute>,
    request: ResultRequestLease,
    message: Value,
) -> DispatchOutcome {
    if message.get("method").and_then(Value::as_str) != Some("tools/call") {
        return dispatch(route, message);
    }
    if message.get("jsonrpc") != Some(&Value::String("2.0".to_owned())) {
        return DispatchOutcome::plain(empty(StatusCode::BAD_REQUEST));
    }
    let Some(id) = message.get("id").filter(|id| valid_request_id(id)).cloned() else {
        return DispatchOutcome::plain(json_rpc_error(
            Value::Null,
            -32600,
            "invalid JSON-RPC request id",
        ));
    };
    if *lock(&route.phase) != RoutePhase::Ready {
        return DispatchOutcome::plain(json_rpc_error(id, -32600, "invalid MCP lifecycle request"));
    }
    let Some(params) = message.get("params").and_then(Value::as_object) else {
        return DispatchOutcome::plain(json_rpc_error(id, -32602, "invalid tools/call parameters"));
    };
    if !optional_field(params, "_meta", valid_request_meta)
        || !optional_field(params, "task", valid_task_metadata)
    {
        return DispatchOutcome::plain(json_rpc_error(id, -32602, "invalid tools/call parameters"));
    }
    if params.get("name").and_then(Value::as_str) != Some(RESULT_TOOL) {
        return DispatchOutcome::plain(json_rpc_error(id, -32602, "unknown tool"));
    }
    let Some(arguments) = params.get("arguments").and_then(Value::as_object) else {
        return DispatchOutcome::plain(json_rpc_error(id, -32602, "invalid tools/call arguments"));
    };
    if arguments.len() != 1 || !arguments.contains_key("value") {
        return DispatchOutcome::plain(json_rpc_error(id, -32602, "invalid tools/call arguments"));
    }
    let result = request
        .submit_value_async(
            arguments
                .get("value")
                .expect("the exact arguments object contains value"),
        )
        .await;
    DispatchOutcome::plain(tool_submission_response(id, result))
}

fn valid_request_id(id: &Value) -> bool {
    id.is_string() || id.is_number()
}

fn tool_submission_response(id: Value, submission: ResultSubmission) -> Response<Full<Bytes>> {
    match submission {
        ResultSubmission::Accepted => tool_result(id, false, "result accepted"),
        ResultSubmission::Invalid {
            issues,
            truncated,
            invalid_calls,
        } => tool_result(
            id,
            true,
            &validation_tool_text(&issues, truncated, invalid_calls),
        ),
        ResultSubmission::Rejected { invalid_calls } => tool_result(
            id,
            true,
            &format!("result contract rejected after {invalid_calls} invalid calls"),
        ),
        ResultSubmission::NoActiveSlot => tool_result(id, true, "no active result slot"),
        ResultSubmission::TurnUnavailable => tool_result(id, true, "turn is settling"),
        ResultSubmission::AlreadySubmitted => tool_result(id, true, "result already submitted"),
        ResultSubmission::ResultContractRejected => {
            tool_result(id, true, "result contract rejected")
        }
        ResultSubmission::HybridValidationRequired => {
            tool_result(id, true, "schema validation callback unavailable")
        }
        ResultSubmission::SchemaCallbackFailed => {
            tool_result(id, true, "schema validation callback failed")
        }
    }
}

fn validation_tool_text(issues: &[ValidationIssue], truncated: bool, invalid_calls: u8) -> String {
    let mut text = format!(
        "result validation failed (invalid call {invalid_calls}/{}):",
        MAX_REPAIRABLE_INVALID_CALLS
    );
    for issue in issues {
        text.push('\n');
        text.push_str(if issue.path.is_empty() {
            "/"
        } else {
            &issue.path
        });
        text.push_str(": ");
        text.push_str(issue.message.as_str());
    }
    if truncated {
        text.push_str("\nadditional validation issues were truncated");
    }
    text
}

fn bound_validation_issues(
    mut issues: Vec<ValidationIssue>,
    mut truncated: bool,
    invalid_calls: u8,
) -> (Vec<ValidationIssue>, bool) {
    if issues.len() > VALIDATION_DETAIL_MAX_ISSUES {
        issues.truncate(VALIDATION_DETAIL_MAX_ISSUES);
        truncated = true;
    }
    if encoded_generated_validation_response_size(&issues, truncated, invalid_calls)
        <= VALIDATION_DETAIL_MAX_BYTES
    {
        return (issues, truncated);
    }

    truncated = true;
    loop {
        if encoded_generated_validation_response_size(&issues, truncated, invalid_calls)
            <= VALIDATION_DETAIL_MAX_BYTES
        {
            return (issues, truncated);
        }
        let Some(last_index) = issues.len().checked_sub(1) else {
            return (issues, truncated);
        };
        let original = std::mem::take(&mut issues[last_index].message);
        if encoded_generated_validation_response_size(&issues, truncated, invalid_calls)
            > VALIDATION_DETAIL_MAX_BYTES
        {
            issues.pop();
            continue;
        }

        let mut boundaries = original
            .char_indices()
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if boundaries.last().copied() != Some(original.len()) {
            boundaries.push(original.len());
        }
        let mut low = 0;
        let mut high = boundaries.len();
        let mut best = 0;
        while low < high {
            let middle = low + (high - low) / 2;
            issues[last_index].message = original[..boundaries[middle]].to_owned();
            if encoded_generated_validation_response_size(&issues, truncated, invalid_calls)
                <= VALIDATION_DETAIL_MAX_BYTES
            {
                best = middle;
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        issues[last_index].message = original[..boundaries[best]].to_owned();
        return (issues, truncated);
    }
}

fn tool_result(id: Value, is_error: bool, text: &str) -> Response<Full<Bytes>> {
    json_response(tool_result_value(id, is_error, text))
}

fn tool_result_value(id: Value, is_error: bool, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{"type": "text", "text": text}],
            "isError": is_error
        }
    })
}

fn encoded_validation_response_size(
    id: &Value,
    issues: &[ValidationIssue],
    truncated: bool,
    invalid_calls: u8,
) -> usize {
    let text = validation_tool_text(issues, truncated, invalid_calls);
    serde_json::to_vec(&tool_result_value(id.clone(), true, &text))
        .expect("validation response values must serialize")
        .len()
}

fn encoded_generated_validation_response_size(
    issues: &[ValidationIssue],
    truncated: bool,
    invalid_calls: u8,
) -> usize {
    let placeholder_id = Value::Null;
    encoded_validation_response_size(&placeholder_id, issues, truncated, invalid_calls)
        - serde_json::to_vec(&placeholder_id)
            .expect("a JSON null request id must serialize")
            .len()
}

fn dispatch(route: &Arc<ResultRoute>, message: Value) -> DispatchOutcome {
    if message.get("jsonrpc") != Some(&Value::String("2.0".to_owned())) {
        return DispatchOutcome::plain(empty(StatusCode::BAD_REQUEST));
    }
    let method = message.get("method").and_then(Value::as_str);
    let request_id = match message.get("id") {
        Some(id) if valid_request_id(id) => Some(id.clone()),
        Some(_) => {
            return DispatchOutcome::plain(json_rpc_error(
                Value::Null,
                -32600,
                "invalid JSON-RPC request id",
            ));
        }
        None => None,
    };
    let mut phase = lock(&route.phase);
    match (method, *phase, request_id) {
        (Some("initialize"), RoutePhase::New, Some(id)) => {
            let params = message.get("params").and_then(Value::as_object);
            let revision = params
                .and_then(|params| params.get("protocolVersion"))
                .and_then(Value::as_str);
            if revision != Some(route.mcp_revision) {
                return DispatchOutcome::plain(json_rpc_error(
                    id,
                    -32602,
                    "unsupported protocol version",
                ));
            }
            let valid_capabilities = params
                .and_then(|params| params.get("capabilities"))
                .is_some_and(valid_client_capabilities);
            let valid_client_info = params
                .and_then(|params| params.get("clientInfo"))
                .is_some_and(valid_implementation);
            let valid_meta =
                params.is_some_and(|params| optional_field(params, "_meta", valid_request_meta));
            if !valid_capabilities || !valid_client_info || !valid_meta {
                return DispatchOutcome::plain(json_rpc_error(
                    id,
                    -32602,
                    "invalid initialize parameters",
                ));
            }
            *phase = RoutePhase::InitializeWriting;
            let response = json_response(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": route.mcp_revision,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "troupe", "version": env!("CARGO_PKG_VERSION")}
                }
            }));
            drop(phase);
            DispatchOutcome {
                response,
                transition: Some(ResponseTransition {
                    route: Arc::clone(route),
                    writing: RoutePhase::InitializeWriting,
                    previous: RoutePhase::New,
                    success: RoutePhase::Initialized,
                    finished: false,
                }),
            }
        }
        (Some("notifications/initialized"), RoutePhase::Initialized, None) => {
            if !valid_initialized_notification_params(&message) {
                return DispatchOutcome::plain(empty(StatusCode::BAD_REQUEST));
            }
            *phase = RoutePhase::InitializedNotificationWriting;
            drop(phase);
            DispatchOutcome {
                response: empty(StatusCode::ACCEPTED),
                transition: Some(ResponseTransition {
                    route: Arc::clone(route),
                    writing: RoutePhase::InitializedNotificationWriting,
                    previous: RoutePhase::Initialized,
                    success: RoutePhase::ClientInitialized,
                    finished: false,
                }),
            }
        }
        (Some("tools/list"), RoutePhase::ClientInitialized, Some(id)) => {
            if !valid_list_tools_params(&message) {
                return DispatchOutcome::plain(json_rpc_error(
                    id,
                    -32602,
                    "invalid tools/list parameters",
                ));
            }
            *phase = RoutePhase::ToolsListWriting;
            let response = json_response(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [{
                        "name": RESULT_TOOL,
                        "description": "Submit the structured result for the current Actor turn.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"value": {"type": "object"}},
                            "required": ["value"],
                            "additionalProperties": false
                        }
                    }]
                }
            }));
            drop(phase);
            DispatchOutcome {
                response,
                transition: Some(ResponseTransition {
                    route: Arc::clone(route),
                    writing: RoutePhase::ToolsListWriting,
                    previous: RoutePhase::ClientInitialized,
                    success: RoutePhase::ReadyPending,
                    finished: false,
                }),
            }
        }
        _ => match message.get("id").cloned() {
            Some(id) => {
                DispatchOutcome::plain(json_rpc_error(id, -32600, "invalid MCP lifecycle request"))
            }
            None => DispatchOutcome::plain(empty(StatusCode::BAD_REQUEST)),
        },
    }
}

fn optional_field(
    object: &serde_json::Map<String, Value>,
    name: &str,
    validate: impl FnOnce(&Value) -> bool,
) -> bool {
    object.get(name).is_none_or(validate)
}

fn valid_optional_params(
    message: &Value,
    validate: impl FnOnce(&serde_json::Map<String, Value>) -> bool,
) -> bool {
    match message.get("params") {
        None => true,
        Some(Value::Object(params)) => validate(params),
        Some(_) => false,
    }
}

fn valid_initialized_notification_params(message: &Value) -> bool {
    valid_optional_params(message, |params| {
        optional_field(params, "_meta", Value::is_object)
    })
}

fn valid_request_meta(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|meta| optional_field(meta, "progressToken", valid_progress_token))
}

fn valid_progress_token(value: &Value) -> bool {
    value.is_string()
        || value.as_number().is_some_and(|number| {
            !number
                .as_str()
                .bytes()
                .any(|byte| matches!(byte, b'.' | b'e' | b'E'))
        })
}

fn valid_task_metadata(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|task| optional_field(task, "ttl", Value::is_number))
}

fn valid_list_tools_params(message: &Value) -> bool {
    valid_optional_params(message, |params| {
        optional_field(params, "_meta", valid_request_meta)
            && optional_field(params, "cursor", Value::is_string)
    })
}

fn optional_object(
    object: &serde_json::Map<String, Value>,
    name: &str,
    validate: impl FnOnce(&serde_json::Map<String, Value>) -> bool,
) -> bool {
    optional_field(object, name, |value| {
        value.as_object().is_some_and(validate)
    })
}

fn valid_client_capabilities(value: &Value) -> bool {
    let Some(capabilities) = value.as_object() else {
        return false;
    };
    optional_object(capabilities, "experimental", |experimental| {
        experimental.values().all(Value::is_object)
    }) && optional_object(capabilities, "roots", |roots| {
        optional_field(roots, "listChanged", Value::is_boolean)
    }) && optional_object(capabilities, "sampling", |sampling| {
        optional_field(sampling, "context", Value::is_object)
            && optional_field(sampling, "tools", Value::is_object)
    }) && optional_object(capabilities, "elicitation", |elicitation| {
        optional_field(elicitation, "form", Value::is_object)
            && optional_field(elicitation, "url", Value::is_object)
    }) && optional_object(capabilities, "tasks", valid_task_capabilities)
}

fn valid_task_capabilities(tasks: &serde_json::Map<String, Value>) -> bool {
    optional_field(tasks, "list", Value::is_object)
        && optional_field(tasks, "cancel", Value::is_object)
        && optional_object(tasks, "requests", |requests| {
            optional_object(requests, "sampling", |sampling| {
                optional_field(sampling, "createMessage", Value::is_object)
            }) && optional_object(requests, "elicitation", |elicitation| {
                optional_field(elicitation, "create", Value::is_object)
            })
        })
}

fn valid_implementation(value: &Value) -> bool {
    let Some(implementation) = value.as_object() else {
        return false;
    };
    implementation.get("name").is_some_and(Value::is_string)
        && implementation.get("version").is_some_and(Value::is_string)
        && ["title", "description", "websiteUrl"]
            .into_iter()
            .all(|name| optional_field(implementation, name, Value::is_string))
        && optional_field(implementation, "icons", |icons| {
            icons
                .as_array()
                .is_some_and(|icons| icons.iter().all(valid_icon))
        })
}

fn valid_icon(value: &Value) -> bool {
    let Some(icon) = value.as_object() else {
        return false;
    };
    icon.get("src").is_some_and(Value::is_string)
        && optional_field(icon, "mimeType", Value::is_string)
        && optional_field(icon, "sizes", |sizes| {
            sizes
                .as_array()
                .is_some_and(|sizes| sizes.iter().all(Value::is_string))
        })
        && optional_field(icon, "theme", |theme| {
            matches!(theme.as_str(), Some("light" | "dark"))
        })
}

fn json_rpc_error(id: Value, code: i32, message: &str) -> Response<Full<Bytes>> {
    json_response(json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    }))
}

fn json_response(value: Value) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(
        serde_json::to_vec(&value).expect("JSON-RPC values must serialize"),
    )));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    response
}

fn empty(status: StatusCode) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::new()));
    *response.status_mut() = status;
    response
}

fn empty_and_close(status: StatusCode) -> Response<Full<Bytes>> {
    let mut response = empty(status);
    response
        .headers_mut()
        .insert(CONNECTION, hyper::header::HeaderValue::from_static("close"));
    response
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_result_generation_isolation")]
pub fn result_generation_isolation_for_test(
    py: pyo3::Python<'_>,
) -> pyo3::PyResult<pyo3::Py<pyo3::PyAny>> {
    use pyo3::types::{PyDict, PyDictMethods as _};

    let service = ResultMcpService::new();
    let old = route_for_test("old-token", 1);
    let successor = route_for_test("successor-token", 2);
    lock(&service.routes).insert(old.token.clone(), Arc::clone(&old));
    let stale_transition = dispatch(
        &old,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_REVISION,
                "capabilities": {},
                "clientInfo": {"name": "troupe-test-client", "version": "1"}
            }
        }),
    )
    .transition
    .expect("initialize creates a lifecycle transition");
    {
        let mut routes = lock(&service.routes);
        old.revoke();
        routes.remove(&old.token);
        routes.insert(successor.token.clone(), Arc::clone(&successor));
    }

    let mut stale_headers = hyper::HeaderMap::new();
    stale_headers.insert(
        AUTHORIZATION,
        hyper::header::HeaderValue::from_static("Bearer old-token"),
    );
    let stale_control = ConnectionControl::new();
    let stale_bearer_bound = service
        .bind_authorized_request(&stale_headers, &stale_control)
        .is_some();
    stale_transition.finish(true);

    let mut successor_headers = hyper::HeaderMap::new();
    successor_headers.insert(
        AUTHORIZATION,
        hyper::header::HeaderValue::from_static("Bearer successor-token"),
    );
    let successor_control = ConnectionControl::new();
    let successor_bearer_bound = service
        .bind_authorized_request(&successor_headers, &successor_control)
        .is_some();
    drop(successor_control);

    let snapshot = PyDict::new(py);
    snapshot.set_item("old_generation", old.generation)?;
    snapshot.set_item("old_phase", lock(&old.phase).as_str())?;
    snapshot.set_item("stale_bearer_bound", stale_bearer_bound)?;
    snapshot.set_item("successor_generation", successor.generation)?;
    snapshot.set_item("successor_phase", lock(&successor.phase).as_str())?;
    snapshot.set_item("successor_bearer_bound", successor_bearer_bound)?;
    snapshot.set_item(
        "successor_connections_after_release",
        *lock(&successor.connections),
    )?;
    Ok(snapshot.into_any().unbind())
}

#[cfg(test)]
mod tests;
