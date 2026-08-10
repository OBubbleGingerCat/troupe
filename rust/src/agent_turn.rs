use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};

use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, PermissionOptionKind, PromptRequest, PromptResponse,
    RequestId, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionId, StopReason, TextContent,
};
use agent_client_protocol::{Agent, ConnectionTo, Error, Responder, is_incoming_transport_closed};
use pyo3::PyErr;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::act_schema::{ValidatedActValue, ValidationIssue};
use crate::agent_error::AgentSessionFailure;
#[cfg(feature = "agent-test-support")]
use crate::agent_launch::{TestOpeningGate, TestTurnGates};
use crate::agent_session::{ActAdmissionLease, AgentSessionSlot, SessionTurnLease};
use crate::result_mcp::{
    ArmedResultLease, MAX_REPAIRABLE_INVALID_CALLS, PreparedResultSettlement, ResultAtSettlement,
    ResultCancelHandoff, ResultFailureAtHandoff,
};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentTurnStop {
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
    RequestFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PromptErrorSettlement {
    AuthoritativeRequestFailure,
    TransportLost,
    ProtocolViolation,
}

#[derive(Default)]
pub(crate) struct PromptResponseProvenance {
    remote_errors: Mutex<HashSet<RequestId>>,
}

impl PromptResponseProvenance {
    pub(crate) fn record_remote_error(&self, request_id: RequestId) {
        self.remote_errors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(request_id);
    }

    fn take_remote_error(&self, request_id: &RequestId) -> bool {
        self.remote_errors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(request_id)
    }
}

#[derive(Debug)]
pub(crate) enum AgentTurnOutcome {
    Success(ValidatedActValue),
    MissingResult,
    ResultRejected {
        issues: Vec<ValidationIssue>,
        invalid_calls: u8,
        truncated: bool,
    },
    Stopped(AgentTurnStop),
    SchemaCallbackFailed(PyErr),
    SessionBroken(AgentSessionFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentTurnCancelDecision {
    Accepted,
    Rejected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentTurnControlPhase {
    Preparing,
    Armed,
    Submitted,
    CancelledBeforeSubmission,
    SupervisorOwnedCancelled,
    SupervisorOwnedFailed,
    CallerOutcomeCommitted,
    Settled,
}

struct AgentTurnControlState {
    phase: AgentTurnControlPhase,
    caller_admission: Option<ActAdmissionLease>,
    armed_result: Option<ArmedResultLease>,
    session_turn: Option<SessionTurnLease>,
    caller_response: Option<oneshot::Sender<AgentTurnOutcome>>,
    caller_outcome: Option<AgentTurnOutcome>,
}

pub(crate) struct AgentTurnControl {
    slot: Arc<AgentSessionSlot>,
    state: Mutex<AgentTurnControlState>,
    caller_cancelled: CancellationToken,
    supervisor_requested: CancellationToken,
}

struct AgentTurnCompletion {
    broken: bool,
}

pub(crate) struct AgentTurnTerminalCleanup {
    prepared_result: Option<PreparedResultSettlement>,
    session_turn: Option<SessionTurnLease>,
    caller_admission: Option<ActAdmissionLease>,
    caller_response: Option<oneshot::Sender<AgentTurnOutcome>>,
    caller_outcome: Option<AgentTurnOutcome>,
    publish: bool,
}

impl AgentTurnControl {
    pub(crate) fn new(slot: Arc<AgentSessionSlot>) -> Arc<Self> {
        Arc::new(Self {
            slot,
            state: Mutex::new(AgentTurnControlState {
                phase: AgentTurnControlPhase::Preparing,
                caller_admission: None,
                armed_result: None,
                session_turn: None,
                caller_response: None,
                caller_outcome: None,
            }),
            caller_cancelled: CancellationToken::new(),
            supervisor_requested: CancellationToken::new(),
        })
    }

    pub(crate) fn install_admission(&self, admission: ActAdmissionLease) -> bool {
        let mut state = lock(&self.state);
        if state.phase != AgentTurnControlPhase::Preparing || state.caller_admission.is_some() {
            drop(state);
            drop(admission);
            return false;
        }
        state.caller_admission = Some(admission);
        true
    }

    pub(crate) fn install_armed(
        &self,
        armed_result: ArmedResultLease,
        session_turn: SessionTurnLease,
        caller_response: oneshot::Sender<AgentTurnOutcome>,
    ) -> bool {
        let mut state = lock(&self.state);
        if state.phase != AgentTurnControlPhase::Preparing {
            drop(state);
            armed_result.disarm();
            session_turn.release();
            return false;
        }
        state.armed_result = Some(armed_result);
        state.session_turn = Some(session_turn);
        state.caller_response = Some(caller_response);
        state.phase = AgentTurnControlPhase::Armed;
        true
    }

    fn mark_submitted(&self) -> bool {
        let mut state = lock(&self.state);
        if state.phase != AgentTurnControlPhase::Armed {
            return false;
        }
        state.phase = AgentTurnControlPhase::Submitted;
        true
    }

    pub(crate) fn queue_if_armed(
        self: &Arc<Self>,
        request: AgentTurnRequest,
    ) -> Result<bool, AgentSessionFailure> {
        let state = lock(&self.state);
        if state.phase != AgentTurnControlPhase::Armed {
            return Ok(false);
        }
        self.slot.queue_turn(request, self).map(|()| true)
    }

    pub(crate) fn request_cancel(&self) -> AgentTurnCancelDecision {
        let mut pre_submission_cleanup = None;
        let mut caller_admission = None;
        let mut caller_response = None;
        let mut caller_outcome = None;
        let mut cancelling_marker = None;
        let mut supervisor_handoff = false;
        let mut clear_turn_control = false;
        let accepted = self.slot.commit_turn_transition(|terminal_failure| {
            let mut state = lock(&self.state);
            if terminal_failure.is_some()
                && matches!(
                    state.phase,
                    AgentTurnControlPhase::Preparing
                        | AgentTurnControlPhase::Armed
                        | AgentTurnControlPhase::Submitted
                )
            {
                return (false, None);
            }
            let accepted = match state.phase {
                AgentTurnControlPhase::CancelledBeforeSubmission
                | AgentTurnControlPhase::SupervisorOwnedCancelled => true,
                AgentTurnControlPhase::SupervisorOwnedFailed
                | AgentTurnControlPhase::CallerOutcomeCommitted
                | AgentTurnControlPhase::Settled => false,
                AgentTurnControlPhase::Preparing => {
                    state.phase = AgentTurnControlPhase::CancelledBeforeSubmission;
                    caller_admission = state.caller_admission.take();
                    caller_response = state.caller_response.take();
                    caller_outcome = state.caller_outcome.take();
                    clear_turn_control = true;
                    true
                }
                AgentTurnControlPhase::Armed => {
                    state.phase = AgentTurnControlPhase::CancelledBeforeSubmission;
                    caller_admission = state.caller_admission.take();
                    caller_response = state.caller_response.take();
                    caller_outcome = state.caller_outcome.take();
                    pre_submission_cleanup =
                        Some((state.armed_result.take(), state.session_turn.take()));
                    clear_turn_control = true;
                    true
                }
                AgentTurnControlPhase::Submitted => {
                    let result = state
                        .armed_result
                        .as_mut()
                        .expect("a submitted turn retains its result arm")
                        .begin_cancellation();
                    if result == ResultCancelHandoff::FailurePreceded {
                        false
                    } else {
                        cancelling_marker = Some(
                            state
                                .session_turn
                                .as_ref()
                                .expect("a submitted turn retains its session lease")
                                .cancelling_marker(),
                        );
                        state.phase = AgentTurnControlPhase::SupervisorOwnedCancelled;
                        caller_admission = state.caller_admission.take();
                        caller_response = state.caller_response.take();
                        caller_outcome = state.caller_outcome.take();
                        supervisor_handoff = true;
                        true
                    }
                }
            };
            (accepted, None)
        });

        if let Some(marker) = cancelling_marker {
            marker.mark_cancelling();
        }
        drop(caller_admission);
        if clear_turn_control {
            self.slot.clear_turn_control(self);
        }
        if let Some((armed_result, session_turn)) = pre_submission_cleanup {
            if let Some(armed_result) = armed_result {
                armed_result.disarm();
            }
            if let Some(session_turn) = session_turn {
                session_turn.release();
            }
        }
        #[cfg(all(feature = "agent-test-support", test))]
        if accepted {
            self.slot.wait_test_turn_cancellation_delivery();
        }
        if accepted {
            self.caller_cancelled.cancel();
        }
        drop(caller_response);
        drop(caller_outcome);
        if supervisor_handoff {
            self.supervisor_requested.cancel();
        }
        if accepted {
            AgentTurnCancelDecision::Accepted
        } else {
            AgentTurnCancelDecision::Rejected
        }
    }

    pub(crate) fn caller_cancellation(&self) -> CancellationToken {
        self.caller_cancelled.clone()
    }

    fn supervisor_notification(&self) -> CancellationToken {
        self.supervisor_requested.clone()
    }

    pub(crate) fn respond_permission(
        &self,
        request: &RequestPermissionRequest,
        responder: Responder<RequestPermissionResponse>,
    ) -> Result<(), Error> {
        let state = lock(&self.state);
        let outcome = if state.phase == AgentTurnControlPhase::Submitted {
            reject_unmapped_permission(request)
        } else {
            RequestPermissionOutcome::Cancelled
        };
        responder.respond(RequestPermissionResponse::new(outcome))
    }

    fn begin_result_failure_handoff(&self) -> bool {
        let mut cancelling_marker = None;
        let mut accepted = false;
        let caller_admission = self.slot.commit_turn_transition(|terminal_failure| {
            let mut state = lock(&self.state);
            if terminal_failure.is_some() || state.phase != AgentTurnControlPhase::Submitted {
                return (None, None);
            }
            let failure = state
                .armed_result
                .as_mut()
                .expect("a submitted turn retains its result arm")
                .take_failure_for_handoff();
            let Some(failure) = failure else {
                return (None, None);
            };
            cancelling_marker = Some(
                state
                    .session_turn
                    .as_ref()
                    .expect("a submitted turn retains its session lease")
                    .cancelling_marker(),
            );
            state.phase = AgentTurnControlPhase::SupervisorOwnedFailed;
            state.caller_outcome = Some(outcome_from_result_failure(failure));
            accepted = true;
            (state.caller_admission.take(), None)
        });
        if !accepted {
            return false;
        }
        if let Some(marker) = cancelling_marker {
            marker.mark_cancelling();
        }
        drop(caller_admission);
        self.supervisor_requested.cancel();
        self.publish_caller_outcome();
        true
    }

    fn complete_response(
        &self,
        response: Result<PromptResponse, Error>,
        remote_error: bool,
        #[cfg(feature = "agent-test-support")] settlement_gate: Option<&TestOpeningGate>,
    ) -> AgentTurnCompletion {
        #[cfg(feature = "agent-test-support")]
        if let Some(gate) = settlement_gate {
            gate.wait_blocking();
            gate.mark_completed();
        }
        let settlement = self.slot.commit_turn_transition(|terminal_failure| {
            let mut state = lock(&self.state);
            if matches!(
                state.phase,
                AgentTurnControlPhase::CallerOutcomeCommitted | AgentTurnControlPhase::Settled
            ) {
                return (None, None);
            }
            let supervisor_owned = matches!(
                state.phase,
                AgentTurnControlPhase::SupervisorOwnedCancelled
                    | AgentTurnControlPhase::SupervisorOwnedFailed
            );
            debug_assert!(supervisor_owned || state.phase == AgentTurnControlPhase::Submitted);
            let mut prepared_result = state
                .armed_result
                .take()
                .map(ArmedResultLease::prepare_settlement);
            let result = prepared_result
                .as_mut()
                .map_or(ResultAtSettlement::Unavailable, |prepared| {
                    prepared.take_outcome()
                });
            let (caller_outcome, proposed_failure) = if supervisor_owned {
                let failure = match response {
                    Ok(_) => None,
                    Err(error) => match classify_prompt_error(&error, remote_error) {
                        PromptErrorSettlement::AuthoritativeRequestFailure => None,
                        PromptErrorSettlement::TransportLost => {
                            Some(AgentSessionFailure::transport_lost())
                        }
                        PromptErrorSettlement::ProtocolViolation => {
                            Some(AgentSessionFailure::protocol_violation())
                        }
                    },
                };
                (None, failure)
            } else {
                let (outcome, failure) = match response {
                    Ok(response) => {
                        let outcome = outcome_from_settlement(response.stop_reason, result);
                        let failure = match &outcome {
                            AgentTurnOutcome::SessionBroken(failure) => Some(failure.clone()),
                            _ => None,
                        };
                        (outcome, failure)
                    }
                    Err(error) => match classify_prompt_error(&error, remote_error) {
                        PromptErrorSettlement::AuthoritativeRequestFailure => {
                            let outcome = outcome_from_authoritative_request_error(result);
                            let failure = match &outcome {
                                AgentTurnOutcome::SessionBroken(failure) => Some(failure.clone()),
                                _ => None,
                            };
                            (outcome, failure)
                        }
                        PromptErrorSettlement::TransportLost => {
                            let failure = AgentSessionFailure::transport_lost();
                            (
                                outcome_from_terminal_failure(result, failure.clone()),
                                Some(failure),
                            )
                        }
                        PromptErrorSettlement::ProtocolViolation => {
                            let failure = AgentSessionFailure::protocol_violation();
                            (
                                outcome_from_terminal_failure(result, failure.clone()),
                                Some(failure),
                            )
                        }
                    },
                };
                (Some(outcome), failure)
            };
            let broken_failure = terminal_failure.or_else(|| proposed_failure.clone());
            let local_failure_committed = matches!(
                caller_outcome.as_ref(),
                Some(AgentTurnOutcome::ResultRejected { .. })
                    | Some(AgentTurnOutcome::SchemaCallbackFailed(_))
            );
            let caller_outcome = if supervisor_owned {
                None
            } else if local_failure_committed {
                caller_outcome
            } else if let Some(failure) = &broken_failure {
                Some(AgentTurnOutcome::SessionBroken(failure.clone()))
            } else {
                caller_outcome
            };
            if let Some(caller_outcome) = caller_outcome {
                state.caller_outcome = Some(caller_outcome);
                state.phase = AgentTurnControlPhase::CallerOutcomeCommitted;
            } else {
                state.phase = AgentTurnControlPhase::Settled;
            }
            (
                Some((prepared_result, state.session_turn.take(), broken_failure)),
                proposed_failure,
            )
        });
        let Some((prepared_result, session_turn, broken_failure)) = settlement else {
            self.publish_caller_outcome();
            return AgentTurnCompletion { broken: true };
        };
        if let Some(prepared_result) = prepared_result {
            prepared_result.finish();
        }
        let broken = broken_failure.is_some();
        self.slot.clear_turn_control(self);
        if let Some(session_turn) = session_turn {
            session_turn.release();
        }
        AgentTurnCompletion { broken }
    }

    fn publish_caller_outcome(&self) {
        let publication = {
            let mut state = lock(&self.state);
            if state.caller_response.is_some() && state.caller_outcome.is_some() {
                Some((
                    state
                        .caller_response
                        .take()
                        .expect("a checked caller response exists"),
                    state
                        .caller_outcome
                        .take()
                        .expect("a checked caller outcome exists"),
                ))
            } else {
                None
            }
        };
        if let Some((response, outcome)) = publication {
            let _ = response.send(outcome);
        }
    }

    pub(crate) fn finish_caller(&self) {
        let caller_admission = {
            let mut state = lock(&self.state);
            if state.phase == AgentTurnControlPhase::CallerOutcomeCommitted {
                state.phase = AgentTurnControlPhase::Settled;
            }
            state.caller_admission.take()
        };
        drop(caller_admission);
    }

    pub(crate) fn fail_terminal(&self, failure: AgentSessionFailure) {
        let cleanup = self.slot.commit_turn_transition(|terminal_failure| {
            let failure = terminal_failure.unwrap_or(failure);
            let cleanup = self.prepare_terminal_delivery(failure.clone());
            (cleanup, Some(failure))
        });
        self.finish_terminal_delivery(cleanup);
    }

    pub(crate) fn prepare_terminal_delivery(
        &self,
        failure: AgentSessionFailure,
    ) -> AgentTurnTerminalCleanup {
        let mut state = lock(&self.state);
        match state.phase {
            AgentTurnControlPhase::Armed | AgentTurnControlPhase::Submitted => {
                let submitted = state.phase == AgentTurnControlPhase::Submitted;
                let mut prepared_result = state
                    .armed_result
                    .take()
                    .map(ArmedResultLease::prepare_settlement);
                let local_failure = submitted
                    .then(|| {
                        prepared_result
                            .as_mut()
                            .and_then(PreparedResultSettlement::take_committed_failure)
                    })
                    .flatten();
                state.caller_outcome = Some(local_failure.map_or_else(
                    || AgentTurnOutcome::SessionBroken(failure),
                    outcome_from_result_failure,
                ));
                state.phase = AgentTurnControlPhase::CallerOutcomeCommitted;
                AgentTurnTerminalCleanup {
                    prepared_result,
                    session_turn: state.session_turn.take(),
                    caller_admission: None,
                    caller_response: None,
                    caller_outcome: None,
                    publish: true,
                }
            }
            AgentTurnControlPhase::SupervisorOwnedCancelled => {
                let prepared_result = state
                    .armed_result
                    .take()
                    .map(ArmedResultLease::prepare_settlement);
                state.phase = AgentTurnControlPhase::Settled;
                AgentTurnTerminalCleanup {
                    prepared_result,
                    session_turn: state.session_turn.take(),
                    caller_admission: state.caller_admission.take(),
                    caller_response: state.caller_response.take(),
                    caller_outcome: state.caller_outcome.take(),
                    publish: false,
                }
            }
            AgentTurnControlPhase::SupervisorOwnedFailed => {
                let prepared_result = state
                    .armed_result
                    .take()
                    .map(ArmedResultLease::prepare_settlement);
                let publish = state.caller_response.is_some() && state.caller_outcome.is_some();
                debug_assert_eq!(
                    state.caller_response.is_some(),
                    state.caller_outcome.is_some()
                );
                state.phase = if publish {
                    AgentTurnControlPhase::CallerOutcomeCommitted
                } else {
                    AgentTurnControlPhase::Settled
                };
                AgentTurnTerminalCleanup {
                    prepared_result,
                    session_turn: state.session_turn.take(),
                    caller_admission: None,
                    caller_response: (!publish).then(|| state.caller_response.take()).flatten(),
                    caller_outcome: (!publish).then(|| state.caller_outcome.take()).flatten(),
                    publish,
                }
            }
            AgentTurnControlPhase::CallerOutcomeCommitted => AgentTurnTerminalCleanup {
                prepared_result: None,
                session_turn: None,
                caller_admission: None,
                caller_response: None,
                caller_outcome: None,
                publish: true,
            },
            AgentTurnControlPhase::Preparing
            | AgentTurnControlPhase::CancelledBeforeSubmission
            | AgentTurnControlPhase::Settled => AgentTurnTerminalCleanup {
                prepared_result: None,
                session_turn: None,
                caller_admission: None,
                caller_response: None,
                caller_outcome: None,
                publish: false,
            },
        }
    }

    pub(crate) fn finish_terminal_delivery(&self, cleanup: AgentTurnTerminalCleanup) {
        let AgentTurnTerminalCleanup {
            mut prepared_result,
            session_turn,
            caller_admission,
            caller_response,
            caller_outcome,
            publish,
        } = cleanup;
        if let Some(prepared_result) = prepared_result.as_mut() {
            prepared_result.discard_outcome();
        }
        if let Some(prepared_result) = prepared_result {
            prepared_result.finish();
        }
        self.slot.clear_turn_control(self);
        if let Some(session_turn) = session_turn {
            session_turn.release();
        }
        drop(caller_admission);
        drop(caller_response);
        drop(caller_outcome);
        if publish {
            self.publish_caller_outcome();
        }
    }

    fn fail_supervisor(&self, failure: AgentSessionFailure) {
        self.fail_terminal(failure);
    }
}

pub(crate) struct AgentTurnRequest {
    prompt: String,
    control: Arc<AgentTurnControl>,
    result_failure: CancellationToken,
}

impl AgentTurnRequest {
    pub(crate) fn new(
        prompt: String,
        control: Arc<AgentTurnControl>,
        result_failure: CancellationToken,
    ) -> Self {
        Self {
            prompt,
            control,
            result_failure,
        }
    }

    pub(crate) fn into_control(self) -> Arc<AgentTurnControl> {
        self.control
    }
}

pub(crate) async fn run_agent_turn_worker(
    connection: &ConnectionTo<Agent>,
    slot: Arc<AgentSessionSlot>,
    session_id: SessionId,
    response_provenance: &PromptResponseProvenance,
    cancellation: &CancellationToken,
    #[cfg(feature = "agent-test-support")] turn_gates: TestTurnGates,
) {
    loop {
        #[cfg(feature = "agent-test-support")]
        if let Some(gate) = &turn_gates.intake {
            tokio::select! {
                () = gate.wait() => gate.mark_completed(),
                () = cancellation.cancelled() => return,
            }
        }
        let request = tokio::select! {
            request = slot.next_turn() => request,
            () = connection.incoming_closed() => {
                slot.commit_transport_loss();
                return;
            }
            () = cancellation.cancelled() => None,
        };
        let Some(request) = request else {
            return;
        };
        if !request.control.mark_submitted() {
            slot.clear_turn_control(&request.control);
            continue;
        }
        if let Err(failure) = slot.register_submitted_turn(&session_id, &request.control) {
            request.control.fail_supervisor(failure);
            return;
        };
        #[cfg(feature = "agent-test-support")]
        if let Some(gate) = &turn_gates.submission {
            tokio::select! {
                () = gate.wait() => gate.mark_completed(),
                () = cancellation.cancelled() => return,
            }
        }

        let prompt = PromptRequest::new(
            session_id.clone(),
            vec![ContentBlock::Text(TextContent::new(request.prompt))],
        );
        let sent = connection.send_request(prompt);
        let request_id = sent.id().clone();
        let mut response = std::pin::pin!(sent.block_task());
        let supervisor_notification = request.control.supervisor_notification();
        let mut result_failure_handled = false;
        let mut cancel_sent = false;
        let response = loop {
            tokio::select! {
                biased;
                response = &mut response => break response,
                () = request.result_failure.cancelled(), if !result_failure_handled => {
                    result_failure_handled = true;
                    request.control.begin_result_failure_handoff();
                }
                () = supervisor_notification.cancelled(), if !cancel_sent => {
                    if connection
                        .send_notification(CancelNotification::new(session_id.clone()))
                        .is_err()
                    {
                        request
                            .control
                            .fail_supervisor(AgentSessionFailure::transport_lost());
                        return;
                    }
                    cancel_sent = true;
                }
                () = cancellation.cancelled() => return,
            }
        };
        let remote_error = response
            .as_ref()
            .is_err_and(|_| response_provenance.take_remote_error(&request_id));
        let completion = request.control.complete_response(
            response,
            remote_error,
            #[cfg(feature = "agent-test-support")]
            turn_gates.settlement.as_deref(),
        );
        #[cfg(feature = "agent-test-support")]
        if let Some(gate) = &turn_gates.outcome {
            tokio::select! {
                () = gate.wait() => gate.mark_completed(),
                () = cancellation.cancelled() => return,
            }
        }
        request.control.publish_caller_outcome();
        if completion.broken {
            return;
        }
    }
}

fn reject_unmapped_permission(request: &RequestPermissionRequest) -> RequestPermissionOutcome {
    let mut reject_once = request
        .options
        .iter()
        .filter(|option| option.kind == PermissionOptionKind::RejectOnce);
    let selected = reject_once.next();
    if let Some(option) = selected
        && reject_once.next().is_none()
    {
        return RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            option.option_id.clone(),
        ));
    }
    RequestPermissionOutcome::Cancelled
}

fn classify_prompt_error(error: &Error, remote_error: bool) -> PromptErrorSettlement {
    if remote_error {
        return PromptErrorSettlement::AuthoritativeRequestFailure;
    }
    if is_incoming_transport_closed(error) {
        return PromptErrorSettlement::TransportLost;
    }
    PromptErrorSettlement::ProtocolViolation
}

fn outcome_from_result_failure(failure: ResultFailureAtHandoff) -> AgentTurnOutcome {
    match failure {
        ResultFailureAtHandoff::Rejected {
            issues,
            invalid_calls,
            truncated,
        } => AgentTurnOutcome::ResultRejected {
            issues,
            invalid_calls,
            truncated,
        },
        ResultFailureAtHandoff::SchemaCallbackFailed(error) => {
            AgentTurnOutcome::SchemaCallbackFailed(error)
        }
    }
}

fn outcome_from_terminal_failure(
    result: ResultAtSettlement,
    failure: AgentSessionFailure,
) -> AgentTurnOutcome {
    match result {
        ResultAtSettlement::Rejected {
            issues,
            invalid_calls,
            truncated,
        } if invalid_calls > MAX_REPAIRABLE_INVALID_CALLS => AgentTurnOutcome::ResultRejected {
            issues,
            invalid_calls,
            truncated,
        },
        ResultAtSettlement::SchemaCallbackFailed(error) => {
            AgentTurnOutcome::SchemaCallbackFailed(error)
        }
        ResultAtSettlement::Accepted(_)
        | ResultAtSettlement::Missing
        | ResultAtSettlement::Rejected { .. }
        | ResultAtSettlement::Unavailable => AgentTurnOutcome::SessionBroken(failure),
    }
}

fn outcome_from_authoritative_request_error(result: ResultAtSettlement) -> AgentTurnOutcome {
    match result {
        ResultAtSettlement::Rejected {
            issues,
            invalid_calls,
            truncated,
        } => AgentTurnOutcome::ResultRejected {
            issues,
            invalid_calls,
            truncated,
        },
        ResultAtSettlement::SchemaCallbackFailed(error) => {
            AgentTurnOutcome::SchemaCallbackFailed(error)
        }
        ResultAtSettlement::Unavailable => {
            AgentTurnOutcome::SessionBroken(AgentSessionFailure::result_channel_lost())
        }
        ResultAtSettlement::Accepted(_) | ResultAtSettlement::Missing => {
            AgentTurnOutcome::Stopped(AgentTurnStop::RequestFailed)
        }
    }
}

fn outcome_from_settlement(
    stop_reason: StopReason,
    result: ResultAtSettlement,
) -> AgentTurnOutcome {
    match result {
        ResultAtSettlement::Rejected {
            issues,
            invalid_calls,
            truncated,
        } => AgentTurnOutcome::ResultRejected {
            issues,
            invalid_calls,
            truncated,
        },
        ResultAtSettlement::SchemaCallbackFailed(error) => {
            AgentTurnOutcome::SchemaCallbackFailed(error)
        }
        _ if stop_reason != StopReason::EndTurn => AgentTurnOutcome::Stopped(match stop_reason {
            StopReason::MaxTokens => AgentTurnStop::MaxTokens,
            StopReason::MaxTurnRequests => AgentTurnStop::MaxTurnRequests,
            StopReason::Refusal => AgentTurnStop::Refusal,
            StopReason::Cancelled => AgentTurnStop::Cancelled,
            StopReason::EndTurn => unreachable!("end_turn was handled above"),
            _ => AgentTurnStop::RequestFailed,
        }),
        ResultAtSettlement::Accepted(value) => AgentTurnOutcome::Success(value),
        ResultAtSettlement::Missing => AgentTurnOutcome::MissingResult,
        ResultAtSettlement::Unavailable => {
            AgentTurnOutcome::SessionBroken(AgentSessionFailure::result_channel_lost())
        }
    }
}

#[cfg(test)]
mod tests {
    use pyo3::Python;
    use pyo3::exceptions::PyValueError;
    use serde_json::json;
    #[cfg(feature = "agent-test-support")]
    use tokio::sync::oneshot::error::TryRecvError;

    use super::*;

    fn terminal_delivery_for_supervisor_failure(
        outcome: AgentTurnOutcome,
    ) -> Result<AgentTurnOutcome, tokio::sync::oneshot::error::TryRecvError> {
        let slot = AgentSessionSlot::new();
        let control = AgentTurnControl::new(slot);
        let (response, mut caller) = oneshot::channel();
        {
            let mut state = lock(&control.state);
            state.phase = AgentTurnControlPhase::SupervisorOwnedFailed;
            state.caller_response = Some(response);
            state.caller_outcome = Some(outcome);
        }

        control.fail_terminal(AgentSessionFailure::transport_lost());
        caller.try_recv()
    }

    fn terminal_delivery_before_result_failure_handoff(
        failure: ResultFailureAtHandoff,
    ) -> AgentTurnOutcome {
        let slot = AgentSessionSlot::new();
        let control = AgentTurnControl::new(slot);
        let (response, mut caller) = oneshot::channel();
        {
            let mut state = lock(&control.state);
            state.phase = AgentTurnControlPhase::Submitted;
            state.armed_result = Some(ArmedResultLease::terminal_failure_for_test(failure));
            state.caller_response = Some(response);
        }

        control.fail_terminal(AgentSessionFailure::transport_lost());
        caller
            .try_recv()
            .expect("terminal delivery must publish one caller outcome")
    }

    fn terminal_response_before_result_failure_handoff(
        failure: ResultFailureAtHandoff,
    ) -> AgentTurnOutcome {
        let slot = AgentSessionSlot::new();
        let control = AgentTurnControl::new(slot);
        let (response, mut caller) = oneshot::channel();
        {
            let mut state = lock(&control.state);
            state.phase = AgentTurnControlPhase::Submitted;
            state.armed_result = Some(ArmedResultLease::terminal_failure_for_test(failure));
            state.caller_response = Some(response);
        }

        let completion = control.complete_response(
            Err(Error::internal_error().data(json!({
                "reason": agent_client_protocol::INCOMING_TRANSPORT_CLOSED_REASON,
                "method": "session/prompt",
            }))),
            false,
            #[cfg(feature = "agent-test-support")]
            None,
        );
        assert!(completion.broken);
        control.publish_caller_outcome();
        caller
            .try_recv()
            .expect("terminal response must publish one caller outcome")
    }

    #[test]
    fn prompt_error_classifier_uses_response_provenance_not_peer_controlled_payloads() {
        let remote = Error::new(-32603, "remote request failure");
        assert_eq!(
            classify_prompt_error(&remote, true),
            PromptErrorSettlement::AuthoritativeRequestFailure
        );

        let incoming_closed = Error::internal_error().data(json!({
            "reason": agent_client_protocol::INCOMING_TRANSPORT_CLOSED_REASON,
            "method": "session/prompt",
        }));
        assert_eq!(
            classify_prompt_error(&incoming_closed, false),
            PromptErrorSettlement::TransportLost
        );
        assert_eq!(
            classify_prompt_error(&incoming_closed, true),
            PromptErrorSettlement::AuthoritativeRequestFailure
        );

        let malformed = Error::parse_error().data(json!({
            "phase": "deserialization",
            "json": {"stopReason": "unknown"},
        }));
        assert_eq!(
            classify_prompt_error(&malformed, false),
            PromptErrorSettlement::ProtocolViolation
        );
        assert_eq!(
            classify_prompt_error(&malformed, true),
            PromptErrorSettlement::AuthoritativeRequestFailure
        );

        let dropped = Error::internal_error()
            .data("response to `session/prompt` never received: channel closed");
        assert_eq!(
            classify_prompt_error(&dropped, false),
            PromptErrorSettlement::ProtocolViolation
        );
        assert_eq!(
            classify_prompt_error(&dropped, true),
            PromptErrorSettlement::AuthoritativeRequestFailure
        );

        let outgoing_closed = Error::internal_error().data(
            "failed to send outgoing request `session/prompt`: connection is no longer running",
        );
        assert_eq!(
            classify_prompt_error(&outgoing_closed, false),
            PromptErrorSettlement::ProtocolViolation
        );
    }

    #[test]
    fn committed_result_rejection_precedes_non_end_turn_stop() {
        let issue = ValidationIssue {
            path: "/decision".to_owned(),
            code: "not_in_choices",
            message: "must be one of the allowed values".to_owned(),
        };

        let outcome = outcome_from_settlement(
            StopReason::MaxTokens,
            ResultAtSettlement::Rejected {
                issues: vec![issue.clone()],
                invalid_calls: 2,
                truncated: false,
            },
        );

        let AgentTurnOutcome::ResultRejected {
            issues,
            invalid_calls,
            truncated,
        } = outcome
        else {
            panic!("committed result rejection was replaced by {outcome:?}");
        };
        assert_eq!(issues, vec![issue]);
        assert_eq!(invalid_calls, 2);
        assert!(!truncated);
    }

    #[test]
    fn committed_schema_callback_failure_precedes_non_end_turn_stop() {
        let _guard = crate::initialize_python_for_test();
        let outcome = Python::attach(|_| {
            outcome_from_settlement(
                StopReason::Refusal,
                ResultAtSettlement::SchemaCallbackFailed(PyValueError::new_err("callback failed")),
            )
        });

        assert!(matches!(outcome, AgentTurnOutcome::SchemaCallbackFailed(_)));
    }

    #[test]
    fn terminal_failure_precedes_a_repairable_invalid_result() {
        let failure = AgentSessionFailure::transport_lost();
        let outcome = outcome_from_terminal_failure(
            ResultAtSettlement::Rejected {
                issues: vec![ValidationIssue {
                    path: "/value".to_owned(),
                    code: "invalid_type",
                    message: "expected int64".to_owned(),
                }],
                invalid_calls: 1,
                truncated: false,
            },
            failure.clone(),
        );

        assert!(matches!(
            outcome,
            AgentTurnOutcome::SessionBroken(actual) if actual == failure
        ));
    }

    #[test]
    fn non_end_turn_stop_still_discards_an_accepted_result() {
        let outcome = outcome_from_settlement(
            StopReason::MaxTokens,
            ResultAtSettlement::Accepted(ValidatedActValue::Object(Vec::new())),
        );

        assert!(matches!(
            outcome,
            AgentTurnOutcome::Stopped(AgentTurnStop::MaxTokens)
        ));
    }

    #[cfg(feature = "agent-test-support")]
    #[test]
    fn cancellation_keeps_response_sender_alive_until_token_publication() {
        let slot = AgentSessionSlot::new();
        let gate = TestOpeningGate::new();
        slot.install_test_turn_cancellation_delivery_gate(Some(Arc::clone(&gate)));
        let control = AgentTurnControl::new(slot);
        let cancellation = control.caller_cancellation();
        let (response, mut caller) = oneshot::channel();
        lock(&control.state).caller_response = Some(response);

        let cancelling = Arc::clone(&control);
        let worker = std::thread::spawn(move || cancelling.request_cancel());
        while !gate.states().0 {
            std::thread::yield_now();
        }

        assert!(!cancellation.is_cancelled());
        assert!(matches!(caller.try_recv(), Err(TryRecvError::Empty)));
        gate.release();
        assert_eq!(
            worker.join().expect("cancellation worker panicked"),
            AgentTurnCancelDecision::Accepted
        );
        assert!(cancellation.is_cancelled());
        assert!(matches!(caller.try_recv(), Err(TryRecvError::Closed)));
    }

    #[test]
    fn terminal_delivery_preserves_committed_result_failure() {
        let issue = ValidationIssue {
            path: "/value".to_owned(),
            code: "invalid_type",
            message: "expected int64".to_owned(),
        };
        let outcome = terminal_delivery_for_supervisor_failure(AgentTurnOutcome::ResultRejected {
            issues: vec![issue.clone()],
            invalid_calls: 9,
            truncated: false,
        })
        .expect("terminal delivery dropped the committed result failure");

        let AgentTurnOutcome::ResultRejected {
            issues,
            invalid_calls,
            truncated,
        } = outcome
        else {
            panic!("terminal delivery replaced the committed result failure");
        };
        assert_eq!(issues, vec![issue]);
        assert_eq!(invalid_calls, 9);
        assert!(!truncated);
    }

    #[test]
    fn terminal_delivery_preserves_committed_schema_callback_failure() {
        let _guard = crate::initialize_python_for_test();
        let outcome = Python::attach(|_| {
            terminal_delivery_for_supervisor_failure(AgentTurnOutcome::SchemaCallbackFailed(
                PyValueError::new_err("callback failed"),
            ))
        })
        .expect("terminal delivery dropped the committed schema callback failure");

        assert!(matches!(outcome, AgentTurnOutcome::SchemaCallbackFailed(_)));
    }

    #[test]
    fn terminal_delivery_preserves_result_failure_before_worker_handoff() {
        let issue = ValidationIssue {
            path: "/value".to_owned(),
            code: "invalid_type",
            message: "expected int64".to_owned(),
        };
        let outcome =
            terminal_delivery_before_result_failure_handoff(ResultFailureAtHandoff::Rejected {
                issues: vec![issue.clone()],
                invalid_calls: 9,
                truncated: false,
            });

        let AgentTurnOutcome::ResultRejected {
            issues,
            invalid_calls,
            truncated,
        } = outcome
        else {
            panic!("terminal delivery replaced the pre-handoff result failure");
        };
        assert_eq!(issues, vec![issue]);
        assert_eq!(invalid_calls, 9);
        assert!(!truncated);
    }

    #[test]
    fn terminal_delivery_preserves_callback_failure_before_worker_handoff() {
        let _guard = crate::initialize_python_for_test();
        let outcome = Python::attach(|_| {
            terminal_delivery_before_result_failure_handoff(
                ResultFailureAtHandoff::SchemaCallbackFailed(PyValueError::new_err(
                    "callback failed",
                )),
            )
        });

        assert!(matches!(outcome, AgentTurnOutcome::SchemaCallbackFailed(_)));
    }

    #[test]
    fn terminal_response_preserves_result_failure_before_worker_handoff() {
        let issue = ValidationIssue {
            path: "/value".to_owned(),
            code: "invalid_type",
            message: "expected int64".to_owned(),
        };
        let outcome =
            terminal_response_before_result_failure_handoff(ResultFailureAtHandoff::Rejected {
                issues: vec![issue.clone()],
                invalid_calls: 9,
                truncated: false,
            });

        let AgentTurnOutcome::ResultRejected {
            issues,
            invalid_calls,
            truncated,
        } = outcome
        else {
            panic!("terminal response replaced the pre-handoff result failure");
        };
        assert_eq!(issues, vec![issue]);
        assert_eq!(invalid_calls, 9);
        assert!(!truncated);
    }

    #[test]
    fn terminal_response_preserves_callback_failure_before_worker_handoff() {
        let _guard = crate::initialize_python_for_test();
        let outcome = Python::attach(|_| {
            terminal_response_before_result_failure_handoff(
                ResultFailureAtHandoff::SchemaCallbackFailed(PyValueError::new_err(
                    "callback failed",
                )),
            )
        });

        assert!(matches!(outcome, AgentTurnOutcome::SchemaCallbackFailed(_)));
    }

    #[test]
    fn terminal_cleanup_accepts_an_already_published_supervisor_failure() {
        let slot = AgentSessionSlot::new();
        let control = AgentTurnControl::new(slot);
        let (response, mut caller) = oneshot::channel();
        {
            let mut state = lock(&control.state);
            state.phase = AgentTurnControlPhase::SupervisorOwnedFailed;
            state.caller_response = Some(response);
            state.caller_outcome = Some(AgentTurnOutcome::ResultRejected {
                issues: Vec::new(),
                invalid_calls: 9,
                truncated: false,
            });
        }
        control.publish_caller_outcome();
        assert!(matches!(
            caller.try_recv(),
            Ok(AgentTurnOutcome::ResultRejected {
                invalid_calls: 9,
                ..
            })
        ));

        control.fail_terminal(AgentSessionFailure::transport_lost());

        assert_eq!(lock(&control.state).phase, AgentTurnControlPhase::Settled);
    }
}
