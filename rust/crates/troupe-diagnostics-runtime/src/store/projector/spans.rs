use std::fmt;

use serde::{Deserialize, Serialize};
use troupe_diagnostics_core::{
    detail::{DiagnosticAttributes, SpanStartDetail},
    event::{CausalLink, DiagnosticEvent, DiagnosticScope},
    id::CanonicalUuid,
    kinds::{SpanKind, SpanOutcome},
    scalar::SchemaU64,
    time::ElapsedNs,
    validate::{ReferenceValidationError, ReferenceValidator, ValidatedEvent},
};

pub const SPAN_READ_MODEL_SCHEMA_VERSION: u8 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpanReadModel {
    model_schema_version: u8,
    run_id: CanonicalUuid,
    through_sequence: SchemaU64,
    through_elapsed_ns: ElapsedNs,
    spans: Vec<ProjectedSpan>,
}

impl SpanReadModel {
    fn empty(run_id: CanonicalUuid) -> Self {
        Self {
            model_schema_version: SPAN_READ_MODEL_SCHEMA_VERSION,
            run_id,
            through_sequence: SchemaU64::new(0),
            through_elapsed_ns: ElapsedNs::new(0),
            spans: Vec::new(),
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

    pub fn spans(&self) -> &[ProjectedSpan] {
        &self.spans
    }

    pub fn span(&self, span_id: SchemaU64) -> Option<&ProjectedSpan> {
        self.spans.iter().find(|span| span.span_id() == span_id)
    }

    pub fn open_spans(&self) -> impl Iterator<Item = &ProjectedSpan> {
        self.spans.iter().filter(|span| span.is_open())
    }

    pub fn completed_spans(&self) -> impl Iterator<Item = &ProjectedSpan> {
        self.spans.iter().filter(|span| !span.is_open())
    }

    pub fn roots(&self) -> impl Iterator<Item = &ProjectedSpan> {
        self.spans
            .iter()
            .filter(|span| span.parent_span_id().is_none())
    }

    pub fn children_of(&self, parent_span_id: SchemaU64) -> impl Iterator<Item = &ProjectedSpan> {
        self.spans
            .iter()
            .filter(move |span| span.parent_span_id() == Some(parent_span_id))
    }

    pub fn causal_successors_of(
        &self,
        source_sequence: SchemaU64,
    ) -> impl Iterator<Item = &ProjectedSpan> {
        self.spans.iter().filter(move |span| {
            span.started_caused_by()
                .iter()
                .any(|link| link.source_sequence() == source_sequence)
        })
    }

    pub fn spans_in_scope<'model, 'scope>(
        &'model self,
        scope: &'scope DiagnosticScope,
    ) -> impl Iterator<Item = &'model ProjectedSpan> + 'scope
    where
        'model: 'scope,
    {
        self.spans.iter().filter(move |span| span.scope() == scope)
    }

    pub fn spans_within_scope<'model, 'scope>(
        &'model self,
        scope: &'scope DiagnosticScope,
    ) -> impl Iterator<Item = &'model ProjectedSpan> + 'scope
    where
        'model: 'scope,
    {
        self.spans
            .iter()
            .filter(move |span| scope_contains(scope, span.scope()))
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectedSpan {
    run_id: CanonicalUuid,
    span_id: SchemaU64,
    started_at_ns: ElapsedNs,
    scope: DiagnosticScope,
    parent_span_id: Option<SchemaU64>,
    started_caused_by: Vec<CausalLink>,
    definition: ProjectedSpanDefinition,
    completion: Option<SpanCompletion>,
}

impl ProjectedSpan {
    fn built_in(event: &troupe_diagnostics_core::event::SpanStarted) -> Self {
        Self {
            run_id: event.header().run_id(),
            span_id: event.header().sequence(),
            started_at_ns: event.header().elapsed_ns(),
            scope: event.header().scope().clone(),
            parent_span_id: event.parent_span_id(),
            started_caused_by: event.header().caused_by().to_vec(),
            definition: ProjectedSpanDefinition::BuiltIn {
                detail: event.detail().clone(),
            },
            completion: None,
        }
    }

    fn custom(event: &troupe_diagnostics_core::event::CustomSpanStarted) -> Self {
        Self {
            run_id: event.header().run_id(),
            span_id: event.header().sequence(),
            started_at_ns: event.header().elapsed_ns(),
            scope: event.header().scope().clone(),
            parent_span_id: event.parent_span_id(),
            started_caused_by: event.header().caused_by().to_vec(),
            definition: ProjectedSpanDefinition::Custom {
                name: event.name().to_owned(),
                attributes: event.attributes().clone(),
            },
            completion: None,
        }
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub const fn span_id(&self) -> SchemaU64 {
        self.span_id
    }

    pub const fn started_at_ns(&self) -> ElapsedNs {
        self.started_at_ns
    }

    pub const fn scope(&self) -> &DiagnosticScope {
        &self.scope
    }

    pub const fn parent_span_id(&self) -> Option<SchemaU64> {
        self.parent_span_id
    }

    pub fn started_caused_by(&self) -> &[CausalLink] {
        &self.started_caused_by
    }

    pub const fn definition(&self) -> &ProjectedSpanDefinition {
        &self.definition
    }

    pub const fn completion(&self) -> Option<&SpanCompletion> {
        self.completion.as_ref()
    }

    pub const fn is_open(&self) -> bool {
        self.completion.is_none()
    }

    pub fn latest_sequence(&self) -> SchemaU64 {
        self.completion
            .as_ref()
            .map_or(self.span_id, SpanCompletion::finish_sequence)
    }

    pub fn elapsed_duration_ns(&self) -> Option<ElapsedNs> {
        self.completion.as_ref().and_then(|completion| {
            completion
                .finished_at_ns()
                .get()
                .checked_sub(self.started_at_ns.get())
                .map(ElapsedNs::new)
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "family", rename_all = "snake_case")]
pub enum ProjectedSpanDefinition {
    BuiltIn {
        detail: SpanStartDetail,
    },
    Custom {
        name: String,
        attributes: DiagnosticAttributes,
    },
}

impl ProjectedSpanDefinition {
    pub const fn family(&self) -> ProjectedSpanFamily {
        match self {
            Self::BuiltIn { .. } => ProjectedSpanFamily::BuiltIn,
            Self::Custom { .. } => ProjectedSpanFamily::Custom,
        }
    }

    pub const fn built_in_kind(&self) -> Option<SpanKind> {
        match self {
            Self::BuiltIn { detail } => Some(detail.kind()),
            Self::Custom { .. } => None,
        }
    }

    pub fn custom_name(&self) -> Option<&str> {
        match self {
            Self::Custom { name, .. } => Some(name),
            Self::BuiltIn { .. } => None,
        }
    }

    pub const fn built_in_detail(&self) -> Option<&SpanStartDetail> {
        match self {
            Self::BuiltIn { detail } => Some(detail),
            Self::Custom { .. } => None,
        }
    }

    pub const fn custom_attributes(&self) -> Option<&DiagnosticAttributes> {
        match self {
            Self::Custom { attributes, .. } => Some(attributes),
            Self::BuiltIn { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectedSpanFamily {
    BuiltIn,
    Custom,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpanCompletion {
    finish_sequence: SchemaU64,
    finished_at_ns: ElapsedNs,
    outcome: SpanOutcome,
    error_code: Option<String>,
    caused_by: Vec<CausalLink>,
}

impl SpanCompletion {
    fn built_in(event: &troupe_diagnostics_core::event::SpanFinished) -> Self {
        Self {
            finish_sequence: event.header().sequence(),
            finished_at_ns: event.header().elapsed_ns(),
            outcome: event.outcome(),
            error_code: event.error_code().map(str::to_owned),
            caused_by: event.header().caused_by().to_vec(),
        }
    }

    fn custom(event: &troupe_diagnostics_core::event::CustomSpanFinished) -> Self {
        Self {
            finish_sequence: event.header().sequence(),
            finished_at_ns: event.header().elapsed_ns(),
            outcome: event.outcome(),
            error_code: None,
            caused_by: event.header().caused_by().to_vec(),
        }
    }

    pub const fn finish_sequence(&self) -> SchemaU64 {
        self.finish_sequence
    }

    pub const fn finished_at_ns(&self) -> ElapsedNs {
        self.finished_at_ns
    }

    pub const fn outcome(&self) -> SpanOutcome {
        self.outcome
    }

    pub fn error_code(&self) -> Option<&str> {
        self.error_code.as_deref()
    }

    pub fn caused_by(&self) -> &[CausalLink] {
        &self.caused_by
    }
}

#[derive(Debug)]
pub struct SpanProjector {
    references: ReferenceValidator,
    model: SpanReadModel,
}

impl SpanProjector {
    pub fn new(run_id: CanonicalUuid) -> Self {
        Self {
            references: ReferenceValidator::new(),
            model: SpanReadModel::empty(run_id),
        }
    }

    pub const fn model(&self) -> &SpanReadModel {
        &self.model
    }

    pub fn into_model(self) -> SpanReadModel {
        self.model
    }

    pub fn apply(&mut self, event: &DiagnosticEvent) -> Result<(), SpanProjectionError> {
        validate_position(&self.model, event)?;
        self.references
            .validate(event)
            .map_err(SpanProjectionError::InvalidReference)?;
        apply_validated_event(&mut self.model, event);
        Ok(())
    }

    pub fn apply_all(&mut self, events: &[DiagnosticEvent]) -> Result<(), SpanProjectionError> {
        for event in events {
            self.apply(event)?;
        }
        Ok(())
    }
}

pub(crate) fn project_validated_event(
    model: &SpanReadModel,
    validated: ValidatedEvent<'_>,
) -> Result<SpanReadModel, SpanProjectionError> {
    let event = validated.event();
    validate_position(model, event)?;
    let mut candidate = model.clone();
    apply_validated_event(&mut candidate, event);
    Ok(candidate)
}

fn validate_position(
    model: &SpanReadModel,
    event: &DiagnosticEvent,
) -> Result<(), SpanProjectionError> {
    let header = event.header();
    if header.run_id() != model.run_id() {
        return Err(SpanProjectionError::RunIdentityMismatch {
            expected: model.run_id(),
            actual: header.run_id(),
            event_sequence: header.sequence(),
        });
    }
    let Some(expected) = model.through_sequence().get().checked_add(1) else {
        return Err(SpanProjectionError::SequenceExhausted {
            event_sequence: header.sequence(),
        });
    };
    if header.sequence().get() != expected {
        return Err(SpanProjectionError::NonCanonicalSequence {
            expected: SchemaU64::new(expected),
            actual: header.sequence(),
        });
    }
    Ok(())
}

fn apply_validated_event(model: &mut SpanReadModel, event: &DiagnosticEvent) {
    match event {
        DiagnosticEvent::SpanStarted(start) => {
            model.spans.push(ProjectedSpan::built_in(start));
        }
        DiagnosticEvent::CustomSpanStarted(start) => {
            model.spans.push(ProjectedSpan::custom(start));
        }
        DiagnosticEvent::SpanFinished(finish) => {
            let span = model
                .spans
                .iter_mut()
                .find(|span| span.span_id() == finish.span_id())
                .expect("validated event references a matching built-in span");
            span.completion = Some(SpanCompletion::built_in(finish));
        }
        DiagnosticEvent::CustomSpanFinished(finish) => {
            let span = model
                .spans
                .iter_mut()
                .find(|span| span.span_id() == finish.span_id())
                .expect("validated event references a matching custom span");
            span.completion = Some(SpanCompletion::custom(finish));
        }
        DiagnosticEvent::InstantOccurred(_)
        | DiagnosticEvent::CounterSampled(_)
        | DiagnosticEvent::AgentMessageDelta(_)
        | DiagnosticEvent::AgentMessageCompleted(_)
        | DiagnosticEvent::AgentPlanSnapshot(_)
        | DiagnosticEvent::ContextUsageSampled(_)
        | DiagnosticEvent::ActTokenUsageFinalized(_)
        | DiagnosticEvent::ObservationGap(_)
        | DiagnosticEvent::CustomInstantOccurred(_)
        | DiagnosticEvent::CustomCounterSampled(_) => {}
    }
    model.through_sequence = event.header().sequence();
    model.through_elapsed_ns = model.through_elapsed_ns.max(event.header().elapsed_ns());
}

pub fn project_spans(
    run_id: CanonicalUuid,
    events: &[DiagnosticEvent],
) -> Result<SpanReadModel, SpanProjectionError> {
    let mut projector = SpanProjector::new(run_id);
    projector.apply_all(events)?;
    Ok(projector.into_model())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpanProjectionError {
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
}

impl SpanProjectionError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::RunIdentityMismatch { .. } => "cross_run",
            Self::NonCanonicalSequence { .. } => "noncanonical_sequence",
            Self::SequenceExhausted { .. } => "sequence_exhausted",
            Self::InvalidReference(error) => error.code().as_str(),
        }
    }

    pub const fn event_sequence(&self) -> SchemaU64 {
        match self {
            Self::RunIdentityMismatch { event_sequence, .. }
            | Self::SequenceExhausted { event_sequence } => *event_sequence,
            Self::NonCanonicalSequence { actual, .. } => *actual,
            Self::InvalidReference(error) => error.event_sequence(),
        }
    }
}

impl fmt::Display for SpanProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RunIdentityMismatch {
                expected, actual, ..
            } => write!(
                formatter,
                "span projection expected Run {expected}, found {actual}"
            ),
            Self::NonCanonicalSequence { expected, actual } => write!(
                formatter,
                "span projection expected sequence {}, found {}",
                expected.get(),
                actual.get()
            ),
            Self::SequenceExhausted { .. } => {
                formatter.write_str("span projection sequence space is exhausted")
            }
            Self::InvalidReference(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for SpanProjectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidReference(error) => Some(error),
            _ => None,
        }
    }
}

fn scope_contains(parent: &DiagnosticScope, child: &DiagnosticScope) -> bool {
    optional_contains(parent.scene_id(), child.scene_id())
        && optional_contains(parent.actor_id(), child.actor_id())
        && optional_contains(parent.cue_id(), child.cue_id())
        && optional_contains(parent.effect_id(), child.effect_id())
        && optional_contains(parent.act_id(), child.act_id())
        && optional_contains(parent.tool_call_id(), child.tool_call_id())
        && parent
            .session_generation()
            .is_none_or(|generation| child.session_generation() == Some(generation))
}

fn optional_contains<T: PartialEq>(parent: Option<&T>, child: Option<&T>) -> bool {
    parent.is_none_or(|value| child == Some(value))
}
