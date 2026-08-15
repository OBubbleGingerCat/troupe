use std::fmt;

use serde::{Deserialize, Serialize};
use troupe_diagnostics_core::{
    detail::PlanEntry,
    event::{CausalLink, DiagnosticEvent, DiagnosticScope},
    id::{CanonicalUuid, RunLocalId},
    scalar::SchemaU64,
    time::ElapsedNs,
    validate::{ReferenceValidationError, ReferenceValidator, ValidatedEvent},
};

pub const PLAN_READ_MODEL_SCHEMA_VERSION: u8 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanReadModel {
    model_schema_version: u8,
    run_id: CanonicalUuid,
    through_sequence: SchemaU64,
    through_elapsed_ns: ElapsedNs,
    plans: Vec<ProjectedPlan>,
}

impl PlanReadModel {
    fn empty(run_id: CanonicalUuid) -> Self {
        Self {
            model_schema_version: PLAN_READ_MODEL_SCHEMA_VERSION,
            run_id,
            through_sequence: SchemaU64::new(0),
            through_elapsed_ns: ElapsedNs::new(0),
            plans: Vec::new(),
        }
    }

    pub const fn model_schema_version(&self) -> u8 {
        self.model_schema_version
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub const fn through_sequence(&self) -> SchemaU64 {
        self.through_sequence
    }

    pub const fn through_elapsed_ns(&self) -> ElapsedNs {
        self.through_elapsed_ns
    }

    pub fn plans(&self) -> &[ProjectedPlan] {
        &self.plans
    }

    pub fn plan_for_scope(&self, scope: &DiagnosticScope) -> Option<&ProjectedPlan> {
        self.plans.iter().find(|plan| plan.scope() == scope)
    }

    pub fn plan_for_act(&self, act_id: &RunLocalId) -> Option<&ProjectedPlan> {
        self.plans
            .iter()
            .find(|plan| plan.scope().act_id() == Some(act_id))
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectedPlan {
    run_id: CanonicalUuid,
    scope: DiagnosticScope,
    sequence: SchemaU64,
    elapsed_ns: ElapsedNs,
    entries: Vec<PlanEntry>,
    truncated: bool,
    caused_by: Vec<CausalLink>,
}

impl ProjectedPlan {
    fn from_event(event: &troupe_diagnostics_core::event::AgentPlanSnapshot) -> Self {
        Self {
            run_id: event.header().run_id(),
            scope: event.header().scope().clone(),
            sequence: event.header().sequence(),
            elapsed_ns: event.header().elapsed_ns(),
            entries: event.entries().to_vec(),
            truncated: event.truncated(),
            caused_by: event.header().caused_by().to_vec(),
        }
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub const fn scope(&self) -> &DiagnosticScope {
        &self.scope
    }

    pub const fn sequence(&self) -> SchemaU64 {
        self.sequence
    }

    pub const fn elapsed_ns(&self) -> ElapsedNs {
        self.elapsed_ns
    }

    pub fn entries(&self) -> &[PlanEntry] {
        &self.entries
    }

    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn caused_by(&self) -> &[CausalLink] {
        &self.caused_by
    }

    pub fn scope_key(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.scope)
    }
}

#[derive(Debug)]
pub struct PlanProjector {
    references: ReferenceValidator,
    model: PlanReadModel,
}

impl PlanProjector {
    pub fn new(run_id: CanonicalUuid) -> Self {
        Self {
            references: ReferenceValidator::new(),
            model: PlanReadModel::empty(run_id),
        }
    }

    pub const fn model(&self) -> &PlanReadModel {
        &self.model
    }

    pub fn into_model(self) -> PlanReadModel {
        self.model
    }

    pub fn apply(&mut self, event: &DiagnosticEvent) -> Result<(), PlanProjectionError> {
        let candidate = candidate_for_event(&self.model, event)?;
        self.references
            .validate(event)
            .map_err(PlanProjectionError::InvalidReference)?;
        self.model = candidate;
        Ok(())
    }

    pub fn apply_all(&mut self, events: &[DiagnosticEvent]) -> Result<(), PlanProjectionError> {
        for event in events {
            self.apply(event)?;
        }
        Ok(())
    }
}

pub(crate) fn project_validated_event(
    model: &PlanReadModel,
    validated: ValidatedEvent<'_>,
) -> Result<PlanReadModel, PlanProjectionError> {
    candidate_for_event(model, validated.event())
}

fn candidate_for_event(
    model: &PlanReadModel,
    event: &DiagnosticEvent,
) -> Result<PlanReadModel, PlanProjectionError> {
    validate_position(model, event)?;
    let mut candidate = model.clone();
    if let DiagnosticEvent::AgentPlanSnapshot(snapshot) = event {
        validate_snapshot(&candidate, snapshot)?;
        apply_snapshot(&mut candidate, snapshot);
    }
    candidate.through_sequence = event.header().sequence();
    candidate.through_elapsed_ns = candidate
        .through_elapsed_ns
        .max(event.header().elapsed_ns());
    Ok(candidate)
}

fn validate_snapshot(
    model: &PlanReadModel,
    event: &troupe_diagnostics_core::event::AgentPlanSnapshot,
) -> Result<(), PlanProjectionError> {
    if let Some(act_id) = event.header().scope().act_id()
        && model.plans.iter().any(|plan| {
            plan.scope().act_id() == Some(act_id) && plan.scope() != event.header().scope()
        })
    {
        return Err(PlanProjectionError::ScopeMismatch {
            act_id: act_id.clone(),
            event_sequence: event.header().sequence(),
        });
    }
    Ok(())
}

fn apply_snapshot(
    model: &mut PlanReadModel,
    event: &troupe_diagnostics_core::event::AgentPlanSnapshot,
) {
    if let Some(plan) = model
        .plans
        .iter_mut()
        .find(|plan| plan.scope() == event.header().scope())
    {
        *plan = ProjectedPlan::from_event(event);
    } else {
        model.plans.push(ProjectedPlan::from_event(event));
    }
}

fn validate_position(
    model: &PlanReadModel,
    event: &DiagnosticEvent,
) -> Result<(), PlanProjectionError> {
    let header = event.header();
    if header.run_id() != model.run_id() {
        return Err(PlanProjectionError::RunIdentityMismatch {
            expected: model.run_id(),
            actual: header.run_id(),
            event_sequence: header.sequence(),
        });
    }
    let Some(expected) = model.through_sequence().get().checked_add(1) else {
        return Err(PlanProjectionError::SequenceExhausted {
            event_sequence: header.sequence(),
        });
    };
    if header.sequence().get() != expected {
        return Err(PlanProjectionError::NonCanonicalSequence {
            expected: SchemaU64::new(expected),
            actual: header.sequence(),
        });
    }
    Ok(())
}

pub fn project_plans(
    run_id: CanonicalUuid,
    events: &[DiagnosticEvent],
) -> Result<PlanReadModel, PlanProjectionError> {
    let mut projector = PlanProjector::new(run_id);
    projector.apply_all(events)?;
    Ok(projector.into_model())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlanProjectionError {
    RunIdentityMismatch {
        expected: CanonicalUuid,
        actual: CanonicalUuid,
        event_sequence: SchemaU64,
    },
    NonCanonicalSequence {
        expected: SchemaU64,
        actual: SchemaU64,
    },
    SequenceExhausted {
        event_sequence: SchemaU64,
    },
    InvalidReference(ReferenceValidationError),
    ScopeMismatch {
        act_id: RunLocalId,
        event_sequence: SchemaU64,
    },
}

impl PlanProjectionError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::RunIdentityMismatch { .. } => "cross_run",
            Self::NonCanonicalSequence { .. } => "noncanonical_sequence",
            Self::SequenceExhausted { .. } => "sequence_exhausted",
            Self::InvalidReference(error) => error.code().as_str(),
            Self::ScopeMismatch { .. } => "scope_mismatch",
        }
    }

    pub const fn event_sequence(&self) -> SchemaU64 {
        match self {
            Self::RunIdentityMismatch { event_sequence, .. }
            | Self::SequenceExhausted { event_sequence }
            | Self::ScopeMismatch { event_sequence, .. } => *event_sequence,
            Self::NonCanonicalSequence { actual, .. } => *actual,
            Self::InvalidReference(error) => error.event_sequence(),
        }
    }
}

impl fmt::Display for PlanProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RunIdentityMismatch {
                expected, actual, ..
            } => write!(
                formatter,
                "plan projection expected Run {expected}, found {actual}"
            ),
            Self::NonCanonicalSequence { expected, actual } => write!(
                formatter,
                "plan projection expected sequence {}, found {}",
                expected.get(),
                actual.get()
            ),
            Self::SequenceExhausted { .. } => {
                formatter.write_str("plan projection sequence space is exhausted")
            }
            Self::InvalidReference(error) => fmt::Display::fmt(error, formatter),
            Self::ScopeMismatch { act_id, .. } => {
                write!(formatter, "plan scope changed for Act {act_id:?}")
            }
        }
    }
}

impl std::error::Error for PlanProjectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidReference(error) => Some(error),
            _ => None,
        }
    }
}
