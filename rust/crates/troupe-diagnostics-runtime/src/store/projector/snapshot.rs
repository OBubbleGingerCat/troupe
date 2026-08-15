use std::fmt;

use serde::{Deserialize, Serialize};
use troupe_diagnostics_core::{
    event::{DiagnosticEvent, DiagnosticScope, ObservationGap},
    id::{CanonicalUuid, RunLocalId},
    scalar::SchemaU64,
    time::ElapsedNs,
    validate::{ReferenceValidationError, ReferenceValidator, ValidatedEvent},
};

use super::{
    counters::{self, CounterProjectionError, CounterProjector, CounterReadModel},
    messages::{self, MessageProjectionError, MessageProjector, MessageReadModel},
    plans::{self, PlanProjectionError, PlanProjector, PlanReadModel},
    spans::{self, SpanProjectionError, SpanProjector, SpanReadModel},
    usage::{self, UsageProjectionError, UsageProjector, UsageReadModel},
};

pub const SNAPSHOT_READ_MODEL_SCHEMA_VERSION: u8 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotReadModel {
    model_schema_version: u8,
    run_id: CanonicalUuid,
    through_sequence: SchemaU64,
    through_elapsed_ns: ElapsedNs,
    spans: SpanReadModel,
    messages: MessageReadModel,
    plans: PlanReadModel,
    counters: CounterReadModel,
    usage: UsageReadModel,
    gaps: Vec<ObservationGap>,
    truncations: Vec<ProjectedTruncation>,
}

impl SnapshotReadModel {
    fn empty(run_id: CanonicalUuid) -> Self {
        Self {
            model_schema_version: SNAPSHOT_READ_MODEL_SCHEMA_VERSION,
            run_id,
            through_sequence: SchemaU64::new(0),
            through_elapsed_ns: ElapsedNs::new(0),
            spans: SpanProjector::new(run_id).into_model(),
            messages: MessageProjector::new(run_id).into_model(),
            plans: PlanProjector::new(run_id).into_model(),
            counters: CounterProjector::new(run_id).into_model(),
            usage: UsageProjector::new(run_id).into_model(),
            gaps: Vec::new(),
            truncations: Vec::new(),
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

    pub const fn spans(&self) -> &SpanReadModel {
        &self.spans
    }

    pub const fn messages(&self) -> &MessageReadModel {
        &self.messages
    }

    pub const fn plans(&self) -> &PlanReadModel {
        &self.plans
    }

    pub const fn counters(&self) -> &CounterReadModel {
        &self.counters
    }

    pub const fn usage(&self) -> &UsageReadModel {
        &self.usage
    }

    pub fn gaps(&self) -> &[ObservationGap] {
        &self.gaps
    }

    pub fn truncations(&self) -> &[ProjectedTruncation] {
        &self.truncations
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ProjectedTruncation {
    AgentMessage {
        sequence: SchemaU64,
        scope: DiagnosticScope,
        message_id: RunLocalId,
    },
    AgentPlan {
        sequence: SchemaU64,
        scope: DiagnosticScope,
    },
}

impl ProjectedTruncation {
    pub const fn sequence(&self) -> SchemaU64 {
        match self {
            Self::AgentMessage { sequence, .. } | Self::AgentPlan { sequence, .. } => *sequence,
        }
    }

    pub const fn scope(&self) -> &DiagnosticScope {
        match self {
            Self::AgentMessage { scope, .. } | Self::AgentPlan { scope, .. } => scope,
        }
    }

    pub const fn message_id(&self) -> Option<&RunLocalId> {
        match self {
            Self::AgentMessage { message_id, .. } => Some(message_id),
            Self::AgentPlan { .. } => None,
        }
    }
}

#[derive(Debug)]
pub struct SnapshotProjector {
    references: ReferenceValidator,
    model: SnapshotReadModel,
}

impl SnapshotProjector {
    pub fn new(run_id: CanonicalUuid) -> Self {
        Self {
            references: ReferenceValidator::new(),
            model: SnapshotReadModel::empty(run_id),
        }
    }

    pub const fn model(&self) -> &SnapshotReadModel {
        &self.model
    }

    pub fn into_model(self) -> SnapshotReadModel {
        self.model
    }

    pub fn apply(&mut self, event: &DiagnosticEvent) -> Result<(), SnapshotProjectionError> {
        let model = &self.model;
        let candidate = self
            .references
            .validate_then(event, |validated| candidate_for_event(model, validated))
            .map_err(SnapshotProjectionError::InvalidReference)??;
        self.model = candidate;
        Ok(())
    }

    pub fn apply_all(&mut self, events: &[DiagnosticEvent]) -> Result<(), SnapshotProjectionError> {
        for event in events {
            self.apply(event)?;
        }
        Ok(())
    }
}

fn candidate_for_event(
    model: &SnapshotReadModel,
    validated: ValidatedEvent<'_>,
) -> Result<SnapshotReadModel, SnapshotProjectionError> {
    let spans = spans::project_validated_event(&model.spans, validated)
        .map_err(SnapshotProjectionError::Span)?;
    let messages = messages::project_validated_event(&model.messages, validated)
        .map_err(SnapshotProjectionError::Message)?;
    let plans = plans::project_validated_event(&model.plans, validated)
        .map_err(SnapshotProjectionError::Plan)?;
    let counters = counters::project_validated_event(&model.counters, validated)
        .map_err(SnapshotProjectionError::Counter)?;
    let usage = usage::project_validated_event(&model.usage, validated)
        .map_err(SnapshotProjectionError::Usage)?;

    let mut gaps = model.gaps.clone();
    if let DiagnosticEvent::ObservationGap(gap) = validated.event() {
        gaps.push(gap.clone());
    }
    let truncations = collect_truncations(&messages, &plans);

    Ok(SnapshotReadModel {
        model_schema_version: SNAPSHOT_READ_MODEL_SCHEMA_VERSION,
        run_id: model.run_id,
        through_sequence: spans.through_sequence(),
        through_elapsed_ns: spans.through_elapsed_ns(),
        spans,
        messages,
        plans,
        counters,
        usage,
        gaps,
        truncations,
    })
}

fn collect_truncations(
    messages: &MessageReadModel,
    plans: &PlanReadModel,
) -> Vec<ProjectedTruncation> {
    let mut truncations: Vec<_> = messages
        .messages()
        .iter()
        .filter_map(|message| {
            let completion = message.completion()?;
            completion
                .truncated()
                .then(|| ProjectedTruncation::AgentMessage {
                    sequence: completion.sequence(),
                    scope: message.scope().clone(),
                    message_id: message.message_id().clone(),
                })
        })
        .chain(
            plans
                .plans()
                .iter()
                .filter(|plan| plan.truncated())
                .map(|plan| ProjectedTruncation::AgentPlan {
                    sequence: plan.sequence(),
                    scope: plan.scope().clone(),
                }),
        )
        .collect();
    truncations.sort_by_key(ProjectedTruncation::sequence);
    truncations
}

pub fn project_snapshot(
    run_id: CanonicalUuid,
    events: &[DiagnosticEvent],
) -> Result<SnapshotReadModel, SnapshotProjectionError> {
    let mut projector = SnapshotProjector::new(run_id);
    projector.apply_all(events)?;
    Ok(projector.into_model())
}

#[derive(Debug, Eq, PartialEq)]
pub enum SnapshotProjectionError {
    InvalidReference(ReferenceValidationError),
    Span(SpanProjectionError),
    Message(MessageProjectionError),
    Plan(PlanProjectionError),
    Counter(CounterProjectionError),
    Usage(UsageProjectionError),
}

impl SnapshotProjectionError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidReference(error) => error.code().as_str(),
            Self::Span(error) => error.code(),
            Self::Message(error) => error.code(),
            Self::Plan(error) => error.code(),
            Self::Counter(error) => error.code(),
            Self::Usage(error) => error.code(),
        }
    }

    pub const fn event_sequence(&self) -> SchemaU64 {
        match self {
            Self::InvalidReference(error) => error.event_sequence(),
            Self::Span(error) => error.event_sequence(),
            Self::Message(error) => error.event_sequence(),
            Self::Plan(error) => error.event_sequence(),
            Self::Counter(error) => error.event_sequence(),
            Self::Usage(error) => error.event_sequence(),
        }
    }
}

impl fmt::Display for SnapshotProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidReference(error) => fmt::Display::fmt(error, formatter),
            Self::Span(error) => fmt::Display::fmt(error, formatter),
            Self::Message(error) => fmt::Display::fmt(error, formatter),
            Self::Plan(error) => fmt::Display::fmt(error, formatter),
            Self::Counter(error) => fmt::Display::fmt(error, formatter),
            Self::Usage(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for SnapshotProjectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidReference(error) => Some(error),
            Self::Span(error) => Some(error),
            Self::Message(error) => Some(error),
            Self::Plan(error) => Some(error),
            Self::Counter(error) => Some(error),
            Self::Usage(error) => Some(error),
        }
    }
}
