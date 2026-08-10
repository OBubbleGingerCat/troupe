use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{
    ContentBlock, PromptRequest, RequestId, SessionId, StopReason, TextContent,
};
use agent_client_protocol::{Agent, ConnectionTo, Error, is_incoming_transport_closed};
use pyo3::PyErr;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::act_schema::{ValidatedActValue, ValidationIssue};
use crate::agent_error::AgentSessionFailure;
use crate::agent_session::{AgentSessionSlot, SessionTurnLease};
use crate::result_mcp::{ArmedResultLease, ResultAtSettlement};

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

pub(crate) struct AgentTurnRequest {
    prompt: String,
    armed_result: ArmedResultLease,
    session_turn: SessionTurnLease,
    response: oneshot::Sender<AgentTurnOutcome>,
}

impl AgentTurnRequest {
    pub(crate) fn new(
        prompt: String,
        armed_result: ArmedResultLease,
        session_turn: SessionTurnLease,
        response: oneshot::Sender<AgentTurnOutcome>,
    ) -> Self {
        Self {
            prompt,
            armed_result,
            session_turn,
            response,
        }
    }
}

pub(crate) async fn run_agent_turn_worker(
    connection: &ConnectionTo<Agent>,
    slot: Arc<AgentSessionSlot>,
    session_id: SessionId,
    response_provenance: &PromptResponseProvenance,
    cancellation: &CancellationToken,
) {
    loop {
        let request = tokio::select! {
            request = slot.next_turn() => request,
            () = cancellation.cancelled() => None,
        };
        let Some(request) = request else {
            return;
        };
        if request.response.is_closed() {
            request.armed_result.disarm();
            request.session_turn.release();
            continue;
        }

        let prompt = PromptRequest::new(
            session_id.clone(),
            vec![ContentBlock::Text(TextContent::new(request.prompt))],
        );
        let sent = connection.send_request(prompt);
        let request_id = sent.id().clone();
        let response = sent.block_task().await;
        let outcome = match response {
            Ok(response) => {
                outcome_from_settlement(response.stop_reason, request.armed_result.settle())
            }
            Err(error) => {
                let result = request.armed_result.settle();
                let settlement = classify_prompt_error(
                    &error,
                    response_provenance.take_remote_error(&request_id),
                );
                match settlement {
                    PromptErrorSettlement::AuthoritativeRequestFailure => {
                        outcome_from_authoritative_request_error(result)
                    }
                    PromptErrorSettlement::TransportLost => {
                        AgentTurnOutcome::SessionBroken(AgentSessionFailure::transport_lost())
                    }
                    PromptErrorSettlement::ProtocolViolation => {
                        AgentTurnOutcome::SessionBroken(AgentSessionFailure::protocol_violation())
                    }
                }
            }
        };
        let outcome = match outcome {
            AgentTurnOutcome::SessionBroken(failure) => {
                AgentTurnOutcome::SessionBroken(slot.mark_broken(failure))
            }
            outcome => outcome,
        };
        request.session_turn.release();
        let broken = matches!(outcome, AgentTurnOutcome::SessionBroken(_));
        let _ = request.response.send(outcome);
        if broken {
            return;
        }
    }
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

    use super::*;

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
}
