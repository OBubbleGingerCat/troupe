use std::any::Any;
use std::sync::Arc;

use troupe_diagnostics_core::detail::{ResultIssue, ResultTransitionDetail};
use troupe_diagnostics_core::kinds::{CounterKind, InstantKind};
use uuid::Uuid;

use super::observer::{
    AgentDiagnosticCandidate, AgentDiagnosticObservation, AgentDiagnosticObserver,
};
use super::session::{AgentTurnDiagnosticIdentity, TurnDiagnosticContext};
use crate::schema::ValidationIssue;

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
