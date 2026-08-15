use std::any::Any;
use std::sync::Arc;

use agent_client_protocol::schema::v1::McpServer;
use tokio_util::sync::CancellationToken;
use troupe_diagnostics_core::detail::{ResultIssue, ResultTransitionDetail};
use troupe_diagnostics_core::kinds::{CounterKind, InstantKind};
use uuid::Uuid;

use super::observer::{
    AgentDiagnosticCandidate, AgentDiagnosticObservation, AgentDiagnosticObserver,
};
use super::payload::ToolPayloadCapturePolicy;
use super::session::{AgentTurnDiagnosticIdentity, TurnDiagnosticContext};
use crate::profile::ResolvedAgentProfile;
use crate::result::{
    ArmedResultLease, ResultAtSettlement, ResultCancelHandoff, ResultMcpService, ResultRoute,
};
use crate::schema::{CompiledActSchema, ValidationIssue};
use crate::session::AgentSessionSlot;
use crate::session::turn::AgentTurnControl;

pub const RESULT_VALIDATION_REJECTIONS_CANDIDATE_KIND: &str = "result.validation_rejections";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentResultMetadata {
    identity: AgentTurnDiagnosticIdentity,
    session_generation: u64,
    operation_id: Uuid,
    turn_index: u64,
}

impl AgentResultMetadata {
    fn new(
        context: &TurnDiagnosticContext,
        session_generation: u64,
        operation_id: Uuid,
        turn_index: u64,
    ) -> Self {
        Self {
            identity: context.identity().clone(),
            session_generation,
            operation_id,
            turn_index,
        }
    }

    pub const fn identity(&self) -> &AgentTurnDiagnosticIdentity {
        &self.identity
    }

    pub const fn session_generation(&self) -> u64 {
        self.session_generation
    }

    pub const fn operation_id(&self) -> Uuid {
        self.operation_id
    }

    pub const fn turn_index(&self) -> u64 {
        self.turn_index
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentResultTransitionCandidate {
    metadata: AgentResultMetadata,
    instant_kind: InstantKind,
    detail: ResultTransitionDetail,
}

impl AgentResultTransitionCandidate {
    fn new(
        metadata: AgentResultMetadata,
        instant_kind: InstantKind,
        issue: Option<ResultIssue>,
        error_code: Option<&'static str>,
    ) -> Self {
        Self {
            metadata,
            instant_kind,
            detail: ResultTransitionDetail::new(issue, error_code.map(str::to_owned)),
        }
    }

    pub const fn metadata(&self) -> &AgentResultMetadata {
        &self.metadata
    }

    pub const fn instant_kind(&self) -> InstantKind {
        self.instant_kind
    }

    pub const fn detail(&self) -> &ResultTransitionDetail {
        &self.detail
    }

    pub const fn issue(&self) -> Option<&ResultIssue> {
        self.detail.issue()
    }

    pub fn error_code(&self) -> Option<&str> {
        self.detail.error_code()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentResultValidationRejectionsCandidate {
    metadata: AgentResultMetadata,
    value: u64,
}

impl AgentResultValidationRejectionsCandidate {
    fn new(metadata: AgentResultMetadata, value: u64) -> Self {
        Self { metadata, value }
    }

    pub const fn metadata(&self) -> &AgentResultMetadata {
        &self.metadata
    }

    pub const fn counter_kind(&self) -> CounterKind {
        CounterKind::ResultValidationRejections
    }

    pub const fn value(&self) -> u64 {
        self.value
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentResultCandidate {
    Transition(AgentResultTransitionCandidate),
    ValidationRejections(AgentResultValidationRejectionsCandidate),
}

impl AgentResultCandidate {
    pub const fn transition(&self) -> Option<&AgentResultTransitionCandidate> {
        match self {
            Self::Transition(candidate) => Some(candidate),
            Self::ValidationRejections(_) => None,
        }
    }

    pub const fn validation_rejections(&self) -> Option<&AgentResultValidationRejectionsCandidate> {
        match self {
            Self::ValidationRejections(candidate) => Some(candidate),
            Self::Transition(_) => None,
        }
    }

    pub const fn metadata(&self) -> &AgentResultMetadata {
        match self {
            Self::Transition(candidate) => candidate.metadata(),
            Self::ValidationRejections(candidate) => candidate.metadata(),
        }
    }
}

impl AgentDiagnosticCandidate for AgentResultCandidate {
    fn kind(&self) -> &'static str {
        match self {
            Self::Transition(candidate) => candidate.instant_kind().as_str(),
            Self::ValidationRejections(_) => RESULT_VALIDATION_REJECTIONS_CANDIDATE_KIND,
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn observation_context(
    context: Option<&TurnDiagnosticContext>,
    session_generation: u64,
    operation_id: Uuid,
    turn_index: u64,
) -> Option<(AgentDiagnosticObserver, AgentResultMetadata)> {
    let context = context?;
    let observer = context.effective_observer()?.clone();
    let metadata = AgentResultMetadata::new(context, session_generation, operation_id, turn_index);
    Some((observer, metadata))
}

fn observe_transition(
    context: Option<&TurnDiagnosticContext>,
    session_generation: u64,
    operation_id: Uuid,
    turn_index: u64,
    instant_kind: InstantKind,
    issue: Option<ResultIssue>,
    error_code: Option<&'static str>,
) {
    let Some((observer, metadata)) =
        observation_context(context, session_generation, operation_id, turn_index)
    else {
        return;
    };
    observer.observe(AgentDiagnosticObservation::Candidate(Arc::new(
        AgentResultCandidate::Transition(AgentResultTransitionCandidate::new(
            metadata,
            instant_kind,
            issue,
            error_code,
        )),
    )));
}

#[inline]
pub(crate) fn observe_submitted(
    context: Option<&TurnDiagnosticContext>,
    session_generation: u64,
    operation_id: Uuid,
    turn_index: u64,
) {
    observe_transition(
        context,
        session_generation,
        operation_id,
        turn_index,
        InstantKind::ResultSubmitted,
        None,
        None,
    );
}

#[inline]
pub(crate) fn observe_validation_rejected(
    context: Option<&TurnDiagnosticContext>,
    session_generation: u64,
    operation_id: Uuid,
    turn_index: u64,
    invalid_calls: u8,
    issues: &[ValidationIssue],
    _truncated: bool,
) {
    let Some((observer, metadata)) =
        observation_context(context, session_generation, operation_id, turn_index)
    else {
        return;
    };
    let issue = issues
        .first()
        .map(|issue| ResultIssue::new(issue.code.to_owned(), issue.path.clone()));
    observer.observe(AgentDiagnosticObservation::Candidate(Arc::new(
        AgentResultCandidate::Transition(AgentResultTransitionCandidate::new(
            metadata.clone(),
            InstantKind::ResultRejected,
            issue,
            Some("invalid_result"),
        )),
    )));
    observer.observe(AgentDiagnosticObservation::Candidate(Arc::new(
        AgentResultCandidate::ValidationRejections(AgentResultValidationRejectionsCandidate::new(
            metadata,
            u64::from(invalid_calls),
        )),
    )));
}

#[inline]
pub(crate) fn observe_repair_requested(
    context: Option<&TurnDiagnosticContext>,
    session_generation: u64,
    operation_id: Uuid,
    turn_index: u64,
    _invalid_calls: u8,
) {
    observe_transition(
        context,
        session_generation,
        operation_id,
        turn_index,
        InstantKind::ResultRepairRequested,
        None,
        None,
    );
}

#[inline]
pub(crate) fn observe_accepted(
    context: Option<&TurnDiagnosticContext>,
    session_generation: u64,
    operation_id: Uuid,
    turn_index: u64,
) {
    observe_transition(
        context,
        session_generation,
        operation_id,
        turn_index,
        InstantKind::ResultAccepted,
        None,
        None,
    );
}

#[inline]
pub(crate) fn observe_missing(
    context: Option<&TurnDiagnosticContext>,
    session_generation: u64,
    operation_id: Uuid,
    turn_index: u64,
) {
    observe_transition(
        context,
        session_generation,
        operation_id,
        turn_index,
        InstantKind::ResultMissing,
        None,
        Some("missing_result"),
    );
}

const RESULT_DIAGNOSTIC_TEST_MCP_REVISION: &str = "2025-11-25";
const RESULT_DIAGNOSTIC_TEST_SESSION_GENERATION: u64 = 73;

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResultDiagnosticSettlementForTest {
    Accepted,
    Missing,
    Rejected { validation_rejections: u8 },
    SchemaCallbackFailed,
    Unavailable,
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResultDiagnosticCancellationForTest {
    Cancelled,
    FailurePreceded,
    Unavailable,
}

/// Focused integration driver for the real Result MCP transport and F06 observation seam.
#[doc(hidden)]
pub struct ResultDiagnosticTestDriver {
    service: Arc<ResultMcpService>,
    route: Arc<ResultRoute>,
    profile: ResolvedAgentProfile,
    endpoint: String,
    authorization: String,
    armed: Option<ArmedResultLease>,
}

impl ResultDiagnosticTestDriver {
    #[doc(hidden)]
    pub async fn start_for_test(profile: ResolvedAgentProfile) -> Self {
        let service = ResultMcpService::new();
        service
            .ensure_ready()
            .await
            .expect("the Result MCP test service starts");
        #[cfg(feature = "agent-test-support")]
        let route = service
            .register_route(
                RESULT_DIAGNOSTIC_TEST_SESSION_GENERATION,
                RESULT_DIAGNOSTIC_TEST_MCP_REVISION,
                None,
            )
            .expect("the Result MCP test route registers");
        #[cfg(not(feature = "agent-test-support"))]
        let route = service
            .register_route(
                RESULT_DIAGNOSTIC_TEST_SESSION_GENERATION,
                RESULT_DIAGNOSTIC_TEST_MCP_REVISION,
            )
            .expect("the Result MCP test route registers");

        let configuration = match route.mcp_server() {
            McpServer::Http(configuration) => configuration,
            _ => unreachable!("the Result MCP service uses HTTP"),
        };
        let endpoint = configuration.url;
        let authorization = configuration
            .headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case("authorization"))
            .map(|header| header.value.clone())
            .expect("the Result MCP route carries bearer authorization");
        Self {
            service,
            route,
            profile,
            endpoint,
            authorization,
            armed: None,
        }
    }

    #[doc(hidden)]
    pub const fn session_generation_for_test(&self) -> u64 {
        RESULT_DIAGNOSTIC_TEST_SESSION_GENERATION
    }

    #[doc(hidden)]
    pub fn endpoint_for_test(&self) -> &str {
        &self.endpoint
    }

    #[doc(hidden)]
    pub fn authorization_for_test(&self) -> &str {
        &self.authorization
    }

    #[doc(hidden)]
    pub async fn wait_ready_for_test(&self) {
        self.route
            .wait_ready(&CancellationToken::new())
            .await
            .expect("the Result MCP test route reaches ready");
    }

    #[doc(hidden)]
    pub fn arm_for_test(
        &mut self,
        identity: AgentTurnDiagnosticIdentity,
        operation_id: Uuid,
        turn_index: u64,
        schema: Arc<CompiledActSchema>,
        observer: AgentDiagnosticObserver,
    ) {
        assert!(self.armed.is_none(), "the Result MCP test slot is idle");
        let slot = AgentSessionSlot::new_with_session_diagnostics(None, None, &self.profile);
        let control = AgentTurnControl::new(Arc::clone(&slot));
        let admission = slot
            .try_claim_admission()
            .expect("the Result MCP test Act is admitted");
        assert!(control.install_admission(admission));
        let context = control.new_diagnostic_context(
            identity,
            Some(observer),
            ToolPayloadCapturePolicy::default(),
        );
        control
            .install_diagnostic_context(context)
            .expect("the F06 turn diagnostic context attaches once");
        let context = control
            .diagnostic_context()
            .expect("the F06 turn diagnostic context is installed");
        self.armed = Some(
            self.route
                .arm_result_with_diagnostics(operation_id, turn_index, schema, None, Some(context))
                .expect("the Result MCP test slot arms"),
        );
    }

    #[doc(hidden)]
    pub fn cancel_for_test(&mut self) -> ResultDiagnosticCancellationForTest {
        let Some(armed) = self.armed.as_mut() else {
            return ResultDiagnosticCancellationForTest::Unavailable;
        };
        match armed.begin_cancellation() {
            ResultCancelHandoff::Cancelled => ResultDiagnosticCancellationForTest::Cancelled,
            ResultCancelHandoff::FailurePreceded => {
                ResultDiagnosticCancellationForTest::FailurePreceded
            }
            ResultCancelHandoff::Unavailable => ResultDiagnosticCancellationForTest::Unavailable,
        }
    }

    #[doc(hidden)]
    pub fn settle_for_test(&mut self) -> ResultDiagnosticSettlementForTest {
        let Some(armed) = self.armed.take() else {
            return ResultDiagnosticSettlementForTest::Unavailable;
        };
        let mut prepared = armed.prepare_settlement();
        let outcome = prepared.take_outcome();
        let outcome = match outcome {
            ResultAtSettlement::Accepted(_) => ResultDiagnosticSettlementForTest::Accepted,
            ResultAtSettlement::Missing => ResultDiagnosticSettlementForTest::Missing,
            ResultAtSettlement::Rejected { invalid_calls, .. } => {
                ResultDiagnosticSettlementForTest::Rejected {
                    validation_rejections: invalid_calls,
                }
            }
            ResultAtSettlement::SchemaCallbackFailed(_) => {
                ResultDiagnosticSettlementForTest::SchemaCallbackFailed
            }
            ResultAtSettlement::Unavailable => ResultDiagnosticSettlementForTest::Unavailable,
        };
        prepared.finish();
        outcome
    }

    #[doc(hidden)]
    pub async fn shutdown_for_test(mut self) {
        if let Some(armed) = self.armed.take() {
            armed.disarm();
        }
        self.service.revoke_route(&self.route).await;
        self.service.shutdown_and_wait().await;
    }
}

impl Drop for ResultDiagnosticTestDriver {
    fn drop(&mut self) {
        if let Some(armed) = self.armed.take() {
            armed.disarm();
        }
        self.service.shutdown();
    }
}
