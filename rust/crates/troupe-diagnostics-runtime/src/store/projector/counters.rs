use std::{collections::BTreeSet, fmt};

use serde::{Deserialize, Serialize};
use troupe_diagnostics_core::{
    detail::{CanonicalInteger, CustomNumber, DiagnosticDimensions},
    event::{CausalLink, DiagnosticEvent, DiagnosticScope},
    id::CanonicalUuid,
    kinds::CounterKind,
    scalar::{DecimalString, SchemaU64},
    time::ElapsedNs,
    validate::{ReferenceValidationError, ReferenceValidator, ValidatedEvent},
};

pub const COUNTER_READ_MODEL_SCHEMA_VERSION: u8 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CounterReadModel {
    model_schema_version: u8,
    run_id: CanonicalUuid,
    through_sequence: SchemaU64,
    through_elapsed_ns: ElapsedNs,
    series: Vec<ProjectedCounter>,
}

impl CounterReadModel {
    fn empty(run_id: CanonicalUuid) -> Self {
        Self {
            model_schema_version: COUNTER_READ_MODEL_SCHEMA_VERSION,
            run_id,
            through_sequence: SchemaU64::new(0),
            through_elapsed_ns: ElapsedNs::new(0),
            series: Vec::new(),
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

    pub fn series(&self) -> &[ProjectedCounter] {
        &self.series
    }

    pub fn sample(&self, identity: &CounterSeriesIdentity) -> Option<&ProjectedCounter> {
        self.series
            .iter()
            .find(|sample| sample.identity() == identity)
    }

    pub fn sample_by_key(&self, series_key: &str) -> Option<&ProjectedCounter> {
        self.series
            .iter()
            .find(|sample| sample.series_key() == series_key)
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    pub fn validate(&self) -> Result<(), CounterProjectionError> {
        if self.model_schema_version != COUNTER_READ_MODEL_SCHEMA_VERSION {
            return Err(CounterProjectionError::ModelSchemaMismatch {
                expected: COUNTER_READ_MODEL_SCHEMA_VERSION,
                actual: self.model_schema_version,
                event_sequence: self.through_sequence,
            });
        }

        let mut keys = BTreeSet::new();
        let mut sequences = BTreeSet::new();
        for sample in &self.series {
            if sample.run_id() != self.run_id
                || sample.sequence().get() == 0
                || sample.sequence() > self.through_sequence
                || sample.series_key() != canonical_series_key(sample.identity())
                || !keys.insert(sample.series_key().to_owned())
            {
                return Err(CounterProjectionError::SeriesMismatch {
                    series_key: sample.series_key().to_owned(),
                    event_sequence: sample.sequence(),
                });
            }
            if !sample.identity().accepts_value(sample.value()) {
                return Err(CounterProjectionError::TagMismatch {
                    series_key: sample.series_key().to_owned(),
                    event_sequence: sample.sequence(),
                    expected: sample.identity().value_class(),
                    actual: sample.value().tag(),
                });
            }
            if !sequences.insert(sample.sequence()) {
                return Err(CounterProjectionError::SequenceTie {
                    event_sequence: sample.sequence(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectedCounter {
    run_id: CanonicalUuid,
    series_key: String,
    identity: CounterSeriesIdentity,
    sequence: SchemaU64,
    elapsed_ns: ElapsedNs,
    value: ProjectedCounterValue,
    caused_by: Vec<CausalLink>,
}

impl ProjectedCounter {
    fn built_in(event: &troupe_diagnostics_core::event::CounterSampled) -> Self {
        let identity = CounterSeriesIdentity::BuiltIn {
            scope: event.header().scope().clone(),
            counter_kind: event.counter_kind(),
        };
        Self {
            run_id: event.header().run_id(),
            series_key: canonical_series_key(&identity),
            identity,
            sequence: event.header().sequence(),
            elapsed_ns: event.header().elapsed_ns(),
            value: ProjectedCounterValue::Unsigned(event.value()),
            caused_by: event.header().caused_by().to_vec(),
        }
    }

    fn custom(event: &troupe_diagnostics_core::event::CustomCounterSampled) -> Self {
        let identity = CounterSeriesIdentity::Custom {
            scope: event.header().scope().clone(),
            name: event.name().to_owned(),
            unit: event.unit().map(str::to_owned),
            dimensions: event.dimensions().clone(),
        };
        let value = match event.value() {
            CustomNumber::Integer(value) => ProjectedCounterValue::Integer(value.clone()),
            CustomNumber::Decimal(value) => ProjectedCounterValue::Decimal(value.clone()),
        };
        Self {
            run_id: event.header().run_id(),
            series_key: canonical_series_key(&identity),
            identity,
            sequence: event.header().sequence(),
            elapsed_ns: event.header().elapsed_ns(),
            value,
            caused_by: event.header().caused_by().to_vec(),
        }
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub fn series_key(&self) -> &str {
        &self.series_key
    }

    pub const fn identity(&self) -> &CounterSeriesIdentity {
        &self.identity
    }

    pub const fn sequence(&self) -> SchemaU64 {
        self.sequence
    }

    pub const fn elapsed_ns(&self) -> ElapsedNs {
        self.elapsed_ns
    }

    pub const fn value(&self) -> &ProjectedCounterValue {
        &self.value
    }

    pub fn caused_by(&self) -> &[CausalLink] {
        &self.caused_by
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "family", rename_all = "snake_case")]
pub enum CounterSeriesIdentity {
    BuiltIn {
        scope: DiagnosticScope,
        counter_kind: CounterKind,
    },
    Custom {
        scope: DiagnosticScope,
        name: String,
        unit: Option<String>,
        dimensions: DiagnosticDimensions,
    },
}

impl CounterSeriesIdentity {
    pub const fn family(&self) -> CounterSeriesFamily {
        match self {
            Self::BuiltIn { .. } => CounterSeriesFamily::BuiltIn,
            Self::Custom { .. } => CounterSeriesFamily::Custom,
        }
    }

    pub const fn scope(&self) -> &DiagnosticScope {
        match self {
            Self::BuiltIn { scope, .. } | Self::Custom { scope, .. } => scope,
        }
    }

    pub const fn built_in_kind(&self) -> Option<CounterKind> {
        match self {
            Self::BuiltIn { counter_kind, .. } => Some(*counter_kind),
            Self::Custom { .. } => None,
        }
    }

    pub fn custom_name(&self) -> Option<&str> {
        match self {
            Self::Custom { name, .. } => Some(name),
            Self::BuiltIn { .. } => None,
        }
    }

    pub fn custom_unit(&self) -> Option<&str> {
        match self {
            Self::Custom { unit, .. } => unit.as_deref(),
            Self::BuiltIn { .. } => None,
        }
    }

    pub const fn custom_dimensions(&self) -> Option<&DiagnosticDimensions> {
        match self {
            Self::Custom { dimensions, .. } => Some(dimensions),
            Self::BuiltIn { .. } => None,
        }
    }

    const fn value_class(&self) -> CounterValueClass {
        match self {
            Self::BuiltIn { .. } => CounterValueClass::Unsigned,
            Self::Custom { .. } => CounterValueClass::CustomNumber,
        }
    }

    const fn accepts_value(&self, value: &ProjectedCounterValue) -> bool {
        matches!(
            (self, value),
            (Self::BuiltIn { .. }, ProjectedCounterValue::Unsigned(_))
                | (
                    Self::Custom { .. },
                    ProjectedCounterValue::Integer(_) | ProjectedCounterValue::Decimal(_)
                )
        )
    }

    pub fn canonical_key(&self) -> String {
        canonical_series_key(self)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CounterSeriesFamily {
    BuiltIn,
    Custom,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ProjectedCounterValue {
    Unsigned(SchemaU64),
    Integer(CanonicalInteger),
    Decimal(DecimalString),
}

impl ProjectedCounterValue {
    pub const fn tag(&self) -> CounterValueTag {
        match self {
            Self::Unsigned(_) => CounterValueTag::Unsigned,
            Self::Integer(_) => CounterValueTag::Integer,
            Self::Decimal(_) => CounterValueTag::Decimal,
        }
    }

    pub const fn unsigned(&self) -> Option<SchemaU64> {
        match self {
            Self::Unsigned(value) => Some(*value),
            Self::Integer(_) | Self::Decimal(_) => None,
        }
    }

    pub fn integer(&self) -> Option<&CanonicalInteger> {
        match self {
            Self::Integer(value) => Some(value),
            Self::Unsigned(_) | Self::Decimal(_) => None,
        }
    }

    pub fn decimal(&self) -> Option<&DecimalString> {
        match self {
            Self::Decimal(value) => Some(value),
            Self::Unsigned(_) | Self::Integer(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CounterValueTag {
    Unsigned,
    Integer,
    Decimal,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CounterValueClass {
    Unsigned,
    CustomNumber,
}

#[derive(Debug)]
pub struct CounterProjector {
    references: ReferenceValidator,
    model: CounterReadModel,
}

impl CounterProjector {
    pub fn new(run_id: CanonicalUuid) -> Self {
        Self {
            references: ReferenceValidator::new(),
            model: CounterReadModel::empty(run_id),
        }
    }

    pub const fn model(&self) -> &CounterReadModel {
        &self.model
    }

    pub fn into_model(self) -> CounterReadModel {
        self.model
    }

    pub fn apply(&mut self, event: &DiagnosticEvent) -> Result<(), CounterProjectionError> {
        let candidate = candidate_for_event(&self.model, event)?;
        self.references
            .validate(event)
            .map_err(CounterProjectionError::InvalidReference)?;
        self.model = candidate;
        Ok(())
    }

    pub fn apply_all(&mut self, events: &[DiagnosticEvent]) -> Result<(), CounterProjectionError> {
        for event in events {
            self.apply(event)?;
        }
        Ok(())
    }
}

pub(crate) fn project_validated_event(
    model: &CounterReadModel,
    validated: ValidatedEvent<'_>,
) -> Result<CounterReadModel, CounterProjectionError> {
    candidate_for_event(model, validated.event())
}

fn candidate_for_event(
    model: &CounterReadModel,
    event: &DiagnosticEvent,
) -> Result<CounterReadModel, CounterProjectionError> {
    validate_position(model, event)?;
    model.validate()?;
    let mut candidate = model.clone();
    let sample = match event {
        DiagnosticEvent::CounterSampled(event) => Some(ProjectedCounter::built_in(event)),
        DiagnosticEvent::CustomCounterSampled(event) => Some(ProjectedCounter::custom(event)),
        DiagnosticEvent::SpanStarted(_)
        | DiagnosticEvent::SpanFinished(_)
        | DiagnosticEvent::InstantOccurred(_)
        | DiagnosticEvent::AgentMessageDelta(_)
        | DiagnosticEvent::AgentMessageCompleted(_)
        | DiagnosticEvent::AgentPlanSnapshot(_)
        | DiagnosticEvent::ContextUsageSampled(_)
        | DiagnosticEvent::ActTokenUsageFinalized(_)
        | DiagnosticEvent::ObservationGap(_)
        | DiagnosticEvent::CustomSpanStarted(_)
        | DiagnosticEvent::CustomSpanFinished(_)
        | DiagnosticEvent::CustomInstantOccurred(_) => None,
    };
    if let Some(sample) = sample {
        apply_sample(&mut candidate, sample)?;
    }
    candidate.through_sequence = event.header().sequence();
    candidate.through_elapsed_ns = candidate
        .through_elapsed_ns
        .max(event.header().elapsed_ns());
    Ok(candidate)
}

fn apply_sample(
    model: &mut CounterReadModel,
    sample: ProjectedCounter,
) -> Result<(), CounterProjectionError> {
    let Some(existing) = model
        .series
        .iter_mut()
        .find(|existing| existing.series_key() == sample.series_key())
    else {
        model.series.push(sample);
        return Ok(());
    };
    if existing.identity() != sample.identity() {
        return Err(CounterProjectionError::SeriesMismatch {
            series_key: sample.series_key().to_owned(),
            event_sequence: sample.sequence(),
        });
    }
    if existing.value().tag() != sample.value().tag() {
        return Err(CounterProjectionError::TagMismatch {
            series_key: sample.series_key().to_owned(),
            event_sequence: sample.sequence(),
            expected: match existing.value().tag() {
                CounterValueTag::Unsigned => CounterValueClass::Unsigned,
                CounterValueTag::Integer | CounterValueTag::Decimal => {
                    CounterValueClass::CustomNumber
                }
            },
            actual: sample.value().tag(),
        });
    }
    *existing = sample;
    Ok(())
}

fn validate_position(
    model: &CounterReadModel,
    event: &DiagnosticEvent,
) -> Result<(), CounterProjectionError> {
    let header = event.header();
    if header.run_id() != model.run_id() {
        return Err(CounterProjectionError::RunIdentityMismatch {
            expected: model.run_id(),
            actual: header.run_id(),
            event_sequence: header.sequence(),
        });
    }
    if model.through_sequence().get() > 0 && header.sequence() == model.through_sequence() {
        return Err(CounterProjectionError::SequenceTie {
            event_sequence: header.sequence(),
        });
    }
    let Some(expected) = model.through_sequence().get().checked_add(1) else {
        return Err(CounterProjectionError::SequenceExhausted {
            event_sequence: header.sequence(),
        });
    };
    if header.sequence().get() != expected {
        return Err(CounterProjectionError::NonCanonicalSequence {
            expected: SchemaU64::new(expected),
            actual: header.sequence(),
        });
    }
    Ok(())
}

pub fn project_counters(
    run_id: CanonicalUuid,
    events: &[DiagnosticEvent],
) -> Result<CounterReadModel, CounterProjectionError> {
    let mut projector = CounterProjector::new(run_id);
    projector.apply_all(events)?;
    Ok(projector.into_model())
}

fn canonical_series_key(identity: &CounterSeriesIdentity) -> String {
    serde_json::to_string(identity)
        .expect("the closed diagnostic counter identity is always JSON-serializable")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CounterProjectionError {
    RunIdentityMismatch {
        expected: CanonicalUuid,
        actual: CanonicalUuid,
        event_sequence: SchemaU64,
    },
    NonCanonicalSequence {
        expected: SchemaU64,
        actual: SchemaU64,
    },
    SequenceTie {
        event_sequence: SchemaU64,
    },
    SequenceExhausted {
        event_sequence: SchemaU64,
    },
    InvalidReference(ReferenceValidationError),
    SeriesMismatch {
        series_key: String,
        event_sequence: SchemaU64,
    },
    TagMismatch {
        series_key: String,
        event_sequence: SchemaU64,
        expected: CounterValueClass,
        actual: CounterValueTag,
    },
    ModelSchemaMismatch {
        expected: u8,
        actual: u8,
        event_sequence: SchemaU64,
    },
}

impl CounterProjectionError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::RunIdentityMismatch { .. } => "cross_run",
            Self::NonCanonicalSequence { .. } => "noncanonical_sequence",
            Self::SequenceTie { .. } => "sequence_tie",
            Self::SequenceExhausted { .. } => "sequence_exhausted",
            Self::InvalidReference(error) => error.code().as_str(),
            Self::SeriesMismatch { .. } => "series_mismatch",
            Self::TagMismatch { .. } => "tag_mismatch",
            Self::ModelSchemaMismatch { .. } => "counter_model_schema_mismatch",
        }
    }

    pub const fn event_sequence(&self) -> SchemaU64 {
        match self {
            Self::RunIdentityMismatch { event_sequence, .. }
            | Self::SequenceTie { event_sequence }
            | Self::SequenceExhausted { event_sequence }
            | Self::SeriesMismatch { event_sequence, .. }
            | Self::TagMismatch { event_sequence, .. }
            | Self::ModelSchemaMismatch { event_sequence, .. } => *event_sequence,
            Self::NonCanonicalSequence { actual, .. } => *actual,
            Self::InvalidReference(error) => error.event_sequence(),
        }
    }
}

impl fmt::Display for CounterProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RunIdentityMismatch {
                expected, actual, ..
            } => write!(
                formatter,
                "counter projection expected Run {expected}, found {actual}"
            ),
            Self::NonCanonicalSequence { expected, actual } => write!(
                formatter,
                "counter projection expected sequence {}, found {}",
                expected.get(),
                actual.get()
            ),
            Self::SequenceTie { event_sequence } => write!(
                formatter,
                "counter projection sequence {} is already materialized",
                event_sequence.get()
            ),
            Self::SequenceExhausted { .. } => {
                formatter.write_str("counter projection sequence space is exhausted")
            }
            Self::InvalidReference(error) => fmt::Display::fmt(error, formatter),
            Self::SeriesMismatch { series_key, .. } => {
                write!(formatter, "counter series key does not match {series_key}")
            }
            Self::TagMismatch {
                series_key,
                expected,
                actual,
                ..
            } => write!(
                formatter,
                "counter series {series_key} expected {expected:?}, found {actual:?}"
            ),
            Self::ModelSchemaMismatch {
                expected, actual, ..
            } => write!(
                formatter,
                "counter model schema version {actual} does not match {expected}"
            ),
        }
    }
}

impl std::error::Error for CounterProjectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidReference(error) => Some(error),
            _ => None,
        }
    }
}
