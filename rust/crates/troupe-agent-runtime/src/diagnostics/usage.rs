use std::sync::Arc;

use agent_client_protocol::schema::v1::{PromptResponse, Usage};
pub use troupe_diagnostics_core::kinds::{UsageAvailability, UsageSource, UsageUnavailableReason};
pub use troupe_diagnostics_core::scalar::TokenCount;

use super::observer::{
    AgentDiagnosticCandidate, AgentDiagnosticErrorCode, AgentDiagnosticObservation,
    AgentTurnDiagnosticOutcome,
};
use super::session::{AgentDiagnosticProvider, AgentTurnDiagnosticMetadata, TurnDiagnosticContext};
use crate::adapter::{AcpAgentAdapter, agent_adapter};
use crate::launch::{AcpWireProtocolVersion, LaunchRunner};
use crate::profile::AgentKind;

pub const AGENT_TURN_USAGE_CANDIDATE_KIND: &str = "agent_turn_usage_terminal";
pub const ACP_TURN_USAGE_SOURCE: &str = "acp.prompt_response.usage";
pub const ACP_TURN_USAGE_CLIENT_SDK_VERSION: &str = "2.0.0";
pub const QUALIFIED_CODEX_ADAPTER_PACKAGE: &str = "@agentclientprotocol/codex-acp";
pub const QUALIFIED_CODEX_ADAPTER_VERSION: &str = "1.1.9";
pub const QUALIFIED_CLAUDE_ADAPTER_PACKAGE: &str = "@agentclientprotocol/claude-agent-acp";
pub const QUALIFIED_CLAUDE_ADAPTER_VERSION: &str = "0.64.2";

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AgentUsageQualification {
    WholeTurn,
    SourceUnsupported,
}

impl AgentUsageQualification {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WholeTurn => "whole_turn",
            Self::SourceUnsupported => "source_unsupported",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentTurnUsageValidationError;

impl AgentTurnUsageValidationError {
    pub const fn code(self) -> &'static str {
        "inconsistent_terminal_usage"
    }
}

impl std::fmt::Display for AgentTurnUsageValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for AgentTurnUsageValidationError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentTurnUsage {
    availability: UsageAvailability,
    source: Option<UsageSource>,
    unavailable_reason: Option<UsageUnavailableReason>,
    provider_total_tokens: Option<TokenCount>,
    input_tokens: Option<TokenCount>,
    output_tokens: Option<TokenCount>,
    thought_tokens: Option<TokenCount>,
    cached_read_tokens: Option<TokenCount>,
    cached_write_tokens: Option<TokenCount>,
}

impl AgentTurnUsage {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        availability: UsageAvailability,
        source: Option<UsageSource>,
        unavailable_reason: Option<UsageUnavailableReason>,
        provider_total_tokens: Option<TokenCount>,
        input_tokens: Option<TokenCount>,
        output_tokens: Option<TokenCount>,
        thought_tokens: Option<TokenCount>,
        cached_read_tokens: Option<TokenCount>,
        cached_write_tokens: Option<TokenCount>,
    ) -> Result<Self, AgentTurnUsageValidationError> {
        let primary_complete =
            provider_total_tokens.is_some() && input_tokens.is_some() && output_tokens.is_some();
        let any_value = provider_total_tokens.is_some()
            || input_tokens.is_some()
            || output_tokens.is_some()
            || thought_tokens.is_some()
            || cached_read_tokens.is_some()
            || cached_write_tokens.is_some();
        let valid = match availability {
            UsageAvailability::Available => {
                primary_complete
                    && source == Some(UsageSource::AcpPromptResponseUsage)
                    && unavailable_reason.is_none()
            }
            UsageAvailability::Partial => {
                any_value
                    && !primary_complete
                    && source == Some(UsageSource::AcpPromptResponseUsage)
                    && unavailable_reason.is_none()
            }
            UsageAvailability::Unavailable => {
                !any_value && source.is_none() && unavailable_reason.is_some()
            }
        };
        if !valid {
            return Err(AgentTurnUsageValidationError);
        }

        Ok(Self {
            availability,
            source,
            unavailable_reason,
            provider_total_tokens,
            input_tokens,
            output_tokens,
            thought_tokens,
            cached_read_tokens,
            cached_write_tokens,
        })
    }

    pub fn unavailable(reason: UsageUnavailableReason) -> Self {
        Self::new(
            UsageAvailability::Unavailable,
            None,
            Some(reason),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("an unavailable terminal usage has no source or token values")
    }

    pub fn prompt_not_submitted() -> Self {
        Self::unavailable(UsageUnavailableReason::PromptNotSubmitted)
    }

    pub fn turn_settlement_unknown() -> Self {
        Self::unavailable(UsageUnavailableReason::TurnSettlementUnknown)
    }

    fn from_acp_usage(usage: &Usage) -> Self {
        Self::reported(
            Some(token_count_from_acp(usage.total_tokens)),
            Some(token_count_from_acp(usage.input_tokens)),
            Some(token_count_from_acp(usage.output_tokens)),
            usage.thought_tokens.map(token_count_from_acp),
            usage.cached_read_tokens.map(token_count_from_acp),
            usage.cached_write_tokens.map(token_count_from_acp),
        )
    }

    pub fn from_prompt_response(
        provider: AgentDiagnosticProvider,
        response: Option<&PromptResponse>,
    ) -> Self {
        normalize_authoritative(adapter_for_provider(provider), response)
    }

    pub const fn availability(&self) -> UsageAvailability {
        self.availability
    }

    pub const fn source(&self) -> Option<UsageSource> {
        self.source
    }

    pub const fn unavailable_reason(&self) -> Option<UsageUnavailableReason> {
        self.unavailable_reason
    }

    pub const fn provider_total_tokens(&self) -> Option<&TokenCount> {
        self.provider_total_tokens.as_ref()
    }

    pub const fn input_tokens(&self) -> Option<&TokenCount> {
        self.input_tokens.as_ref()
    }

    pub const fn output_tokens(&self) -> Option<&TokenCount> {
        self.output_tokens.as_ref()
    }

    pub const fn thought_tokens(&self) -> Option<&TokenCount> {
        self.thought_tokens.as_ref()
    }

    pub const fn cached_read_tokens(&self) -> Option<&TokenCount> {
        self.cached_read_tokens.as_ref()
    }

    pub const fn cached_write_tokens(&self) -> Option<&TokenCount> {
        self.cached_write_tokens.as_ref()
    }

    fn reported(
        provider_total_tokens: Option<TokenCount>,
        input_tokens: Option<TokenCount>,
        output_tokens: Option<TokenCount>,
        thought_tokens: Option<TokenCount>,
        cached_read_tokens: Option<TokenCount>,
        cached_write_tokens: Option<TokenCount>,
    ) -> Self {
        let primary_complete =
            provider_total_tokens.is_some() && input_tokens.is_some() && output_tokens.is_some();
        let any_value = provider_total_tokens.is_some()
            || input_tokens.is_some()
            || output_tokens.is_some()
            || thought_tokens.is_some()
            || cached_read_tokens.is_some()
            || cached_write_tokens.is_some();
        if !any_value {
            return Self::unavailable(UsageUnavailableReason::UsageNotReported);
        }
        let availability = if primary_complete {
            UsageAvailability::Available
        } else {
            UsageAvailability::Partial
        };
        Self::new(
            availability,
            Some(UsageSource::AcpPromptResponseUsage),
            None,
            provider_total_tokens,
            input_tokens,
            output_tokens,
            thought_tokens,
            cached_read_tokens,
            cached_write_tokens,
        )
        .expect("reported terminal usage derives a consistent availability")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentTurnUsageCandidate {
    turn: Arc<AgentTurnDiagnosticMetadata>,
    usage: AgentTurnUsage,
}

impl AgentTurnUsageCandidate {
    pub(crate) const fn new(turn: Arc<AgentTurnDiagnosticMetadata>, usage: AgentTurnUsage) -> Self {
        Self { turn, usage }
    }

    pub fn turn(&self) -> &AgentTurnDiagnosticMetadata {
        &self.turn
    }

    pub const fn usage(&self) -> &AgentTurnUsage {
        &self.usage
    }
}

impl AgentDiagnosticCandidate for AgentTurnUsageCandidate {
    fn kind(&self) -> &'static str {
        AGENT_TURN_USAGE_CANDIDATE_KIND
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

pub fn pinned_usage_qualification(provider: AgentDiagnosticProvider) -> AgentUsageQualification {
    adapter_usage_qualification(adapter_for_provider(provider))
}

fn adapter_for_provider(provider: AgentDiagnosticProvider) -> &'static dyn AcpAgentAdapter {
    let agent = match provider {
        AgentDiagnosticProvider::Codex => AgentKind::Codex,
        AgentDiagnosticProvider::Claude => AgentKind::Claude,
        AgentDiagnosticProvider::Kimi => AgentKind::Kimi,
    };
    agent_adapter(agent)
}

fn adapter_usage_qualification(adapter: &dyn AcpAgentAdapter) -> AgentUsageQualification {
    let spec = adapter.launch_spec();
    if spec.acp_wire_protocol != AcpWireProtocolVersion::StableV1
        || spec.client_sdk_version != ACP_TURN_USAGE_CLIENT_SDK_VERSION
    {
        return AgentUsageQualification::SourceUnsupported;
    }

    let qualified = match (spec.agent, &spec.runner) {
        (
            AgentKind::Codex,
            LaunchRunner::Npx {
                package,
                exact_version,
                ..
            },
        ) => {
            *package == QUALIFIED_CODEX_ADAPTER_PACKAGE
                && *exact_version == QUALIFIED_CODEX_ADAPTER_VERSION
        }
        (
            AgentKind::Claude,
            LaunchRunner::Npx {
                package,
                exact_version,
                ..
            },
        ) => {
            *package == QUALIFIED_CLAUDE_ADAPTER_PACKAGE
                && *exact_version == QUALIFIED_CLAUDE_ADAPTER_VERSION
        }
        (AgentKind::Kimi, _) => false,
        _ => false,
    };
    if qualified {
        AgentUsageQualification::WholeTurn
    } else {
        AgentUsageQualification::SourceUnsupported
    }
}

fn normalize_authoritative(
    adapter: &dyn AcpAgentAdapter,
    response: Option<&PromptResponse>,
) -> AgentTurnUsage {
    if adapter_usage_qualification(adapter) != AgentUsageQualification::WholeTurn {
        return AgentTurnUsage::unavailable(UsageUnavailableReason::SourceUnsupported);
    }
    let Some(usage) = response.and_then(|response| response.usage.as_ref()) else {
        return AgentTurnUsage::unavailable(UsageUnavailableReason::UsageNotReported);
    };
    AgentTurnUsage::from_acp_usage(usage)
}

fn token_count_from_acp(value: u64) -> TokenCount {
    TokenCount::parse(&value.to_string()).expect("an ACP u64 is a canonical token count")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TurnTerminalSettlement {
    NotSubmitted,
    Authoritative,
    Unknown,
}

#[derive(Clone, Copy)]
pub(crate) struct TurnTerminalObservation<'a> {
    pub(crate) settlement: TurnTerminalSettlement,
    pub(crate) outcome: AgentTurnDiagnosticOutcome,
    pub(crate) error_code: Option<AgentDiagnosticErrorCode>,
    pub(crate) response: Option<&'a PromptResponse>,
    pub(crate) adapter: Option<&'static dyn AcpAgentAdapter>,
}

impl TurnTerminalObservation<'_> {
    pub(crate) const fn not_submitted() -> Self {
        Self {
            settlement: TurnTerminalSettlement::NotSubmitted,
            outcome: AgentTurnDiagnosticOutcome::Cancelled,
            error_code: Some(AgentDiagnosticErrorCode::new("prompt_not_submitted")),
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
            outcome: terminal_outcome(response.stop_reason),
            error_code: terminal_error_code(response.stop_reason),
            response: Some(response),
            adapter: Some(adapter),
        }
    }

    pub(crate) const fn authoritative_without_response(
        adapter: &'static dyn AcpAgentAdapter,
    ) -> Self {
        Self {
            settlement: TurnTerminalSettlement::Authoritative,
            outcome: AgentTurnDiagnosticOutcome::Failed,
            error_code: Some(AgentDiagnosticErrorCode::new("request_failed")),
            response: None,
            adapter: Some(adapter),
        }
    }

    pub(crate) const fn unknown(adapter: Option<&'static dyn AcpAgentAdapter>) -> Self {
        Self {
            settlement: TurnTerminalSettlement::Unknown,
            outcome: AgentTurnDiagnosticOutcome::Failed,
            error_code: Some(AgentDiagnosticErrorCode::new("turn_settlement_unknown")),
            response: None,
            adapter,
        }
    }

    pub(crate) const fn with_failure(mut self, code: &'static str) -> Self {
        self.outcome = AgentTurnDiagnosticOutcome::Failed;
        self.error_code = Some(AgentDiagnosticErrorCode::new(code));
        self
    }
}

const fn terminal_outcome(stop_reason: agent_client_protocol::schema::v1::StopReason) -> AgentTurnDiagnosticOutcome {
    use agent_client_protocol::schema::v1::StopReason;

    match stop_reason {
        StopReason::EndTurn => AgentTurnDiagnosticOutcome::Completed,
        StopReason::Cancelled => AgentTurnDiagnosticOutcome::Cancelled,
        StopReason::MaxTokens | StopReason::MaxTurnRequests | StopReason::Refusal => {
            AgentTurnDiagnosticOutcome::Failed
        }
        _ => AgentTurnDiagnosticOutcome::Failed,
    }
}

const fn terminal_error_code(
    stop_reason: agent_client_protocol::schema::v1::StopReason,
) -> Option<AgentDiagnosticErrorCode> {
    use agent_client_protocol::schema::v1::StopReason;

    match stop_reason {
        StopReason::EndTurn => None,
        StopReason::Cancelled => Some(AgentDiagnosticErrorCode::new("remote_cancelled")),
        StopReason::MaxTokens => Some(AgentDiagnosticErrorCode::new("max_tokens")),
        StopReason::MaxTurnRequests => Some(AgentDiagnosticErrorCode::new("max_turn_requests")),
        StopReason::Refusal => Some(AgentDiagnosticErrorCode::new("refused")),
        _ => Some(AgentDiagnosticErrorCode::new("request_failed")),
    }
}

fn normalize_terminal_observation(observation: &TurnTerminalObservation<'_>) -> AgentTurnUsage {
    match observation.settlement {
        TurnTerminalSettlement::NotSubmitted => AgentTurnUsage::prompt_not_submitted(),
        TurnTerminalSettlement::Unknown => AgentTurnUsage::turn_settlement_unknown(),
        TurnTerminalSettlement::Authoritative => observation.adapter.map_or_else(
            || AgentTurnUsage::unavailable(UsageUnavailableReason::SourceUnsupported),
            |adapter| normalize_authoritative(adapter, observation.response),
        ),
    }
}

#[inline]
pub(crate) fn observe_turn_terminal(
    context: &TurnDiagnosticContext,
    observation: &TurnTerminalObservation<'_>,
) {
    let (Some(observer), Some(turn)) = (
        context.effective_observer(),
        context.runtime_metadata().cloned(),
    ) else {
        return;
    };
    let candidate =
        AgentTurnUsageCandidate::new(Arc::new(turn), normalize_terminal_observation(observation));
    observer.observe(AgentDiagnosticObservation::Candidate(Arc::new(candidate)));
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::schema::v1::{PromptResponse, StopReason};

    use super::*;

    #[test]
    fn terminal_boundaries_have_closed_outcomes_and_stable_error_codes() {
        let adapter = agent_adapter(AgentKind::Codex);
        for (stop_reason, outcome, error_code) in [
            (StopReason::EndTurn, AgentTurnDiagnosticOutcome::Completed, None),
            (
                StopReason::Cancelled,
                AgentTurnDiagnosticOutcome::Cancelled,
                Some("remote_cancelled"),
            ),
            (
                StopReason::MaxTokens,
                AgentTurnDiagnosticOutcome::Failed,
                Some("max_tokens"),
            ),
            (
                StopReason::MaxTurnRequests,
                AgentTurnDiagnosticOutcome::Failed,
                Some("max_turn_requests"),
            ),
            (
                StopReason::Refusal,
                AgentTurnDiagnosticOutcome::Failed,
                Some("refused"),
            ),
        ] {
            let response = PromptResponse::new(stop_reason);
            let observation = TurnTerminalObservation::settled(&response, adapter);
            assert_eq!(observation.outcome, outcome);
            assert_eq!(
                observation
                    .error_code
                    .map(AgentDiagnosticErrorCode::as_str),
                error_code
            );
        }

        let unknown = TurnTerminalObservation::unknown(None).with_failure("transport_lost");
        assert_eq!(unknown.outcome, AgentTurnDiagnosticOutcome::Failed);
        assert_eq!(
            unknown.error_code.map(AgentDiagnosticErrorCode::as_str),
            Some("transport_lost")
        );
    }
}
