use agent_client_protocol::schema::v1::PromptResponse;

use super::session::TurnDiagnosticContext;
use crate::adapter::AcpAgentAdapter;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TurnTerminalSettlement {
    NotSubmitted,
    Authoritative,
    Unknown,
}

pub(crate) struct TurnTerminalObservation<'a> {
    pub(crate) settlement: TurnTerminalSettlement,
    pub(crate) response: Option<&'a PromptResponse>,
    pub(crate) adapter: Option<&'static dyn AcpAgentAdapter>,
}

impl TurnTerminalObservation<'_> {
    pub(crate) const fn not_submitted() -> Self {
        Self {
            settlement: TurnTerminalSettlement::NotSubmitted,
            response: None,
            adapter: None,
        }
    }

    pub(crate) const fn settled<'a>(
        response: &'a PromptResponse,
        adapter: &'static dyn AcpAgentAdapter,
    ) -> TurnTerminalObservation<'a> {
        TurnTerminalObservation {
            settlement: TurnTerminalSettlement::Authoritative,
            response: Some(response),
            adapter: Some(adapter),
        }
    }

    pub(crate) const fn authoritative_without_response(
        adapter: &'static dyn AcpAgentAdapter,
    ) -> Self {
        Self {
            settlement: TurnTerminalSettlement::Authoritative,
            response: None,
            adapter: Some(adapter),
        }
    }

    pub(crate) const fn unknown(adapter: Option<&'static dyn AcpAgentAdapter>) -> Self {
        Self {
            settlement: TurnTerminalSettlement::Unknown,
            response: None,
            adapter,
        }
    }
}

#[inline]
pub(crate) fn observe_turn_terminal(
    _context: &TurnDiagnosticContext,
    observation: &TurnTerminalObservation<'_>,
) {
    let _ = (
        observation.settlement,
        observation.response,
        observation.adapter,
    );
}
