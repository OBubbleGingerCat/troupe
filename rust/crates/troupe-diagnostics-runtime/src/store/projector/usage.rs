use std::{collections::BTreeMap, fmt, str::FromStr};

use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use troupe_diagnostics_core::{
    event::{ActTokenUsageFinalized, CausalLink, DiagnosticEvent, DiagnosticScope},
    id::{CanonicalUuid, RunLocalId},
    kinds::{UsageAvailability, UsageSource, UsageUnavailableReason},
    scalar::{SchemaU64, TokenCount},
    time::ElapsedNs,
    validate::{ReferenceValidationError, ReferenceValidator, ValidatedEvent},
};

pub const USAGE_READ_MODEL_SCHEMA_VERSION: u8 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UsageReadModel {
    model_schema_version: u8,
    run_id: CanonicalUuid,
    through_sequence: SchemaU64,
    through_elapsed_ns: ElapsedNs,
    usages: Vec<ProjectedActUsage>,
    aggregate: UsageAggregate,
    scoped_aggregates: Vec<ScopedUsageAggregate>,
}

impl UsageReadModel {
    fn empty(run_id: CanonicalUuid) -> Self {
        Self {
            model_schema_version: USAGE_READ_MODEL_SCHEMA_VERSION,
            run_id,
            through_sequence: SchemaU64::new(0),
            through_elapsed_ns: ElapsedNs::new(0),
            usages: Vec::new(),
            aggregate: UsageAggregate::empty(),
            scoped_aggregates: Vec::new(),
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

    pub fn usages(&self) -> &[ProjectedActUsage] {
        &self.usages
    }

    pub const fn aggregate(&self) -> &UsageAggregate {
        &self.aggregate
    }

    pub fn scoped_aggregates(&self) -> &[ScopedUsageAggregate] {
        &self.scoped_aggregates
    }

    pub fn usage_for_act(&self, act_id: &RunLocalId) -> Option<&ProjectedActUsage> {
        self.usages.iter().find(|usage| usage.act_id() == act_id)
    }

    pub fn usages_within_scope<'model, 'scope>(
        &'model self,
        scope: &'scope DiagnosticScope,
    ) -> impl Iterator<Item = &'model ProjectedActUsage> + 'scope
    where
        'model: 'scope,
    {
        self.usages
            .iter()
            .filter(move |usage| scope_contains(scope, usage.scope()))
    }

    pub fn aggregate_within_scope(
        &self,
        scope: &DiagnosticScope,
    ) -> Result<UsageAggregate, UsageProjectionError> {
        self.validate()?;
        aggregate_records(self.usages_within_scope(scope))
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    pub fn validate(&self) -> Result<(), UsageProjectionError> {
        if self.model_schema_version != USAGE_READ_MODEL_SCHEMA_VERSION {
            return Err(UsageProjectionError::ModelSchemaMismatch {
                expected: USAGE_READ_MODEL_SCHEMA_VERSION,
                actual: self.model_schema_version,
                event_sequence: self.through_sequence,
            });
        }

        let mut acts = BTreeMap::new();
        let mut previous_sequence = SchemaU64::new(0);
        for usage in &self.usages {
            if usage.run_id() != self.run_id
                || usage.sequence().get() == 0
                || usage.sequence() > self.through_sequence
                || usage.elapsed_ns() > self.through_elapsed_ns
                || usage.scope().act_id() != Some(usage.act_id())
            {
                return Err(UsageProjectionError::UsageRecordMismatch {
                    event_sequence: usage.sequence(),
                });
            }
            if usage.sequence() <= previous_sequence {
                return Err(UsageProjectionError::UsageRecordMismatch {
                    event_sequence: usage.sequence(),
                });
            }
            previous_sequence = usage.sequence();
            if let Some(first_sequence) = acts.get(usage.act_id()) {
                return Err(UsageProjectionError::DuplicateActUsage {
                    act_id: usage.act_id().clone(),
                    first_sequence: *first_sequence,
                    event_sequence: usage.sequence(),
                });
            }
            acts.insert(usage.act_id().clone(), usage.sequence());
            if !usage.availability_is_valid() {
                return Err(UsageProjectionError::AvailabilityMismatch {
                    act_id: usage.act_id().clone(),
                    event_sequence: usage.sequence(),
                });
            }
        }

        let expected = aggregate_records(self.usages.iter())?;
        let expected_scoped = aggregate_records_by_scope(self.usages.iter())?;
        if self.aggregate != expected || self.scoped_aggregates != expected_scoped {
            return Err(UsageProjectionError::AggregateMismatch {
                event_sequence: self.through_sequence,
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedUsageAggregate {
    scope: DiagnosticScope,
    aggregate: UsageAggregate,
}

impl ScopedUsageAggregate {
    pub const fn scope(&self) -> &DiagnosticScope {
        &self.scope
    }

    pub const fn aggregate(&self) -> &UsageAggregate {
        &self.aggregate
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectedActUsage {
    run_id: CanonicalUuid,
    act_id: RunLocalId,
    scope: DiagnosticScope,
    sequence: SchemaU64,
    elapsed_ns: ElapsedNs,
    caused_by: Vec<CausalLink>,
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

impl ProjectedActUsage {
    fn from_event(event: &ActTokenUsageFinalized, act_id: RunLocalId) -> Self {
        Self {
            run_id: event.header().run_id(),
            act_id,
            scope: event.header().scope().clone(),
            sequence: event.header().sequence(),
            elapsed_ns: event.header().elapsed_ns(),
            caused_by: event.header().caused_by().to_vec(),
            availability: event.availability(),
            source: event.source(),
            unavailable_reason: event.unavailable_reason(),
            provider_total_tokens: event.provider_total_tokens().cloned(),
            input_tokens: event.input_tokens().cloned(),
            output_tokens: event.output_tokens().cloned(),
            thought_tokens: event.thought_tokens().cloned(),
            cached_read_tokens: event.cached_read_tokens().cloned(),
            cached_write_tokens: event.cached_write_tokens().cloned(),
        }
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub const fn act_id(&self) -> &RunLocalId {
        &self.act_id
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

    pub fn caused_by(&self) -> &[CausalLink] {
        &self.caused_by
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

    fn availability_is_valid(&self) -> bool {
        let primary_complete = self.provider_total_tokens.is_some()
            && self.input_tokens.is_some()
            && self.output_tokens.is_some();
        let any_value = self.provider_total_tokens.is_some()
            || self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.thought_tokens.is_some()
            || self.cached_read_tokens.is_some()
            || self.cached_write_tokens.is_some();
        match self.availability {
            UsageAvailability::Available => {
                primary_complete
                    && self.source == Some(UsageSource::AcpPromptResponseUsage)
                    && self.unavailable_reason.is_none()
            }
            UsageAvailability::Partial => {
                any_value
                    && !primary_complete
                    && self.source == Some(UsageSource::AcpPromptResponseUsage)
                    && self.unavailable_reason.is_none()
            }
            UsageAvailability::Unavailable => {
                !any_value && self.source.is_none() && self.unavailable_reason.is_some()
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UsageAggregate {
    finalized_acts: SchemaU64,
    reported_acts: SchemaU64,
    available_acts: SchemaU64,
    partial_acts: SchemaU64,
    unavailable_acts: SchemaU64,
    provider_total_tokens: UsageFieldAggregate,
    input_tokens: UsageFieldAggregate,
    output_tokens: UsageFieldAggregate,
    thought_tokens: UsageFieldAggregate,
    cached_read_tokens: UsageFieldAggregate,
    cached_write_tokens: UsageFieldAggregate,
}

impl UsageAggregate {
    fn empty() -> Self {
        Self {
            finalized_acts: SchemaU64::new(0),
            reported_acts: SchemaU64::new(0),
            available_acts: SchemaU64::new(0),
            partial_acts: SchemaU64::new(0),
            unavailable_acts: SchemaU64::new(0),
            provider_total_tokens: UsageFieldAggregate::empty(),
            input_tokens: UsageFieldAggregate::empty(),
            output_tokens: UsageFieldAggregate::empty(),
            thought_tokens: UsageFieldAggregate::empty(),
            cached_read_tokens: UsageFieldAggregate::empty(),
            cached_write_tokens: UsageFieldAggregate::empty(),
        }
    }

    pub const fn finalized_acts(&self) -> SchemaU64 {
        self.finalized_acts
    }

    pub const fn reported_acts(&self) -> SchemaU64 {
        self.reported_acts
    }

    pub const fn available_acts(&self) -> SchemaU64 {
        self.available_acts
    }

    pub const fn partial_acts(&self) -> SchemaU64 {
        self.partial_acts
    }

    pub const fn unavailable_acts(&self) -> SchemaU64 {
        self.unavailable_acts
    }

    pub const fn provider_total_tokens(&self) -> &UsageFieldAggregate {
        &self.provider_total_tokens
    }

    pub const fn input_tokens(&self) -> &UsageFieldAggregate {
        &self.input_tokens
    }

    pub const fn output_tokens(&self) -> &UsageFieldAggregate {
        &self.output_tokens
    }

    pub const fn thought_tokens(&self) -> &UsageFieldAggregate {
        &self.thought_tokens
    }

    pub const fn cached_read_tokens(&self) -> &UsageFieldAggregate {
        &self.cached_read_tokens
    }

    pub const fn cached_write_tokens(&self) -> &UsageFieldAggregate {
        &self.cached_write_tokens
    }

    fn record(&mut self, usage: &ProjectedActUsage) -> Result<(), UsageProjectionError> {
        self.finalized_acts = increment_count(self.finalized_acts, usage.sequence())?;
        match usage.availability() {
            UsageAvailability::Available => {
                self.reported_acts = increment_count(self.reported_acts, usage.sequence())?;
                self.available_acts = increment_count(self.available_acts, usage.sequence())?;
            }
            UsageAvailability::Partial => {
                self.reported_acts = increment_count(self.reported_acts, usage.sequence())?;
                self.partial_acts = increment_count(self.partial_acts, usage.sequence())?;
            }
            UsageAvailability::Unavailable => {
                self.unavailable_acts = increment_count(self.unavailable_acts, usage.sequence())?;
            }
        }
        self.provider_total_tokens
            .record(usage.provider_total_tokens(), usage.sequence())?;
        self.input_tokens
            .record(usage.input_tokens(), usage.sequence())?;
        self.output_tokens
            .record(usage.output_tokens(), usage.sequence())?;
        self.thought_tokens
            .record(usage.thought_tokens(), usage.sequence())?;
        self.cached_read_tokens
            .record(usage.cached_read_tokens(), usage.sequence())?;
        self.cached_write_tokens
            .record(usage.cached_write_tokens(), usage.sequence())?;
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UsageFieldAggregate {
    known_sum: Option<TokenCount>,
    reported_acts: SchemaU64,
    finalized_acts: SchemaU64,
}

impl UsageFieldAggregate {
    fn empty() -> Self {
        Self {
            known_sum: None,
            reported_acts: SchemaU64::new(0),
            finalized_acts: SchemaU64::new(0),
        }
    }

    pub const fn known_sum(&self) -> Option<&TokenCount> {
        self.known_sum.as_ref()
    }

    pub const fn reported_acts(&self) -> SchemaU64 {
        self.reported_acts
    }

    pub const fn finalized_acts(&self) -> SchemaU64 {
        self.finalized_acts
    }

    fn record(
        &mut self,
        value: Option<&TokenCount>,
        event_sequence: SchemaU64,
    ) -> Result<(), UsageProjectionError> {
        self.finalized_acts = increment_count(self.finalized_acts, event_sequence)?;
        let Some(value) = value else {
            return Ok(());
        };
        self.reported_acts = increment_count(self.reported_acts, event_sequence)?;
        self.known_sum = Some(sum_token_counts(
            self.known_sum.as_ref(),
            value,
            event_sequence,
        )?);
        Ok(())
    }
}

#[derive(Debug)]
pub struct UsageProjector {
    references: ReferenceValidator,
    model: UsageReadModel,
}

impl UsageProjector {
    pub fn new(run_id: CanonicalUuid) -> Self {
        Self {
            references: ReferenceValidator::new(),
            model: UsageReadModel::empty(run_id),
        }
    }

    pub const fn model(&self) -> &UsageReadModel {
        &self.model
    }

    pub fn into_model(self) -> UsageReadModel {
        self.model
    }

    pub fn apply(&mut self, event: &DiagnosticEvent) -> Result<(), UsageProjectionError> {
        let candidate = candidate_for_event(&self.model, event)?;
        self.references
            .validate(event)
            .map_err(UsageProjectionError::InvalidReference)?;
        self.model = candidate;
        Ok(())
    }

    pub fn apply_all(&mut self, events: &[DiagnosticEvent]) -> Result<(), UsageProjectionError> {
        for event in events {
            self.apply(event)?;
        }
        Ok(())
    }
}

pub(crate) fn project_validated_event(
    model: &UsageReadModel,
    validated: ValidatedEvent<'_>,
) -> Result<UsageReadModel, UsageProjectionError> {
    candidate_for_event(model, validated.event())
}

fn candidate_for_event(
    model: &UsageReadModel,
    event: &DiagnosticEvent,
) -> Result<UsageReadModel, UsageProjectionError> {
    validate_position(model, event)?;
    model.validate()?;
    let mut candidate = model.clone();
    if let DiagnosticEvent::ActTokenUsageFinalized(event) = event {
        let Some(act_id) = event.header().scope().act_id().cloned() else {
            return Err(UsageProjectionError::MissingActIdentity {
                event_sequence: event.header().sequence(),
            });
        };
        if let Some(existing) = candidate.usage_for_act(&act_id) {
            return Err(UsageProjectionError::DuplicateActUsage {
                act_id,
                first_sequence: existing.sequence(),
                event_sequence: event.header().sequence(),
            });
        }
        let projected = ProjectedActUsage::from_event(event, act_id);
        if !projected.availability_is_valid() {
            return Err(UsageProjectionError::AvailabilityMismatch {
                act_id: projected.act_id().clone(),
                event_sequence: projected.sequence(),
            });
        }
        candidate.aggregate.record(&projected)?;
        record_scoped_aggregates(&mut candidate.scoped_aggregates, &projected)?;
        candidate.usages.push(projected);
    }
    candidate.through_sequence = event.header().sequence();
    candidate.through_elapsed_ns = candidate
        .through_elapsed_ns
        .max(event.header().elapsed_ns());
    Ok(candidate)
}

fn validate_position(
    model: &UsageReadModel,
    event: &DiagnosticEvent,
) -> Result<(), UsageProjectionError> {
    let header = event.header();
    if header.run_id() != model.run_id() {
        return Err(UsageProjectionError::RunIdentityMismatch {
            expected: model.run_id(),
            actual: header.run_id(),
            event_sequence: header.sequence(),
        });
    }
    if model.through_sequence().get() > 0 && header.sequence() == model.through_sequence() {
        return Err(UsageProjectionError::SequenceTie {
            event_sequence: header.sequence(),
        });
    }
    let Some(expected) = model.through_sequence().get().checked_add(1) else {
        return Err(UsageProjectionError::SequenceExhausted {
            event_sequence: header.sequence(),
        });
    };
    if header.sequence().get() != expected {
        return Err(UsageProjectionError::NonCanonicalSequence {
            expected: SchemaU64::new(expected),
            actual: header.sequence(),
        });
    }
    Ok(())
}

pub fn project_usage(
    run_id: CanonicalUuid,
    events: &[DiagnosticEvent],
) -> Result<UsageReadModel, UsageProjectionError> {
    let mut projector = UsageProjector::new(run_id);
    projector.apply_all(events)?;
    Ok(projector.into_model())
}

fn aggregate_records<'a>(
    usages: impl IntoIterator<Item = &'a ProjectedActUsage>,
) -> Result<UsageAggregate, UsageProjectionError> {
    let mut aggregate = UsageAggregate::empty();
    for usage in usages {
        aggregate.record(usage)?;
    }
    Ok(aggregate)
}

fn aggregate_records_by_scope<'a>(
    usages: impl IntoIterator<Item = &'a ProjectedActUsage>,
) -> Result<Vec<ScopedUsageAggregate>, UsageProjectionError> {
    let mut aggregates = Vec::new();
    for usage in usages {
        record_scoped_aggregates(&mut aggregates, usage)?;
    }
    Ok(aggregates)
}

fn record_scoped_aggregates(
    aggregates: &mut Vec<ScopedUsageAggregate>,
    usage: &ProjectedActUsage,
) -> Result<(), UsageProjectionError> {
    let Some(scene_id) = usage.scope().scene_id().cloned() else {
        return Ok(());
    };
    let scene = DiagnosticScope::new(
        Some(scene_id.clone()),
        None,
        None,
        None,
        None,
        None,
        None,
    );
    record_scoped_aggregate(aggregates, scene, usage)?;
    if let Some(actor_id) = usage.scope().actor_id().cloned() {
        let actor = DiagnosticScope::new(
            Some(scene_id),
            Some(actor_id),
            None,
            None,
            None,
            None,
            None,
        );
        record_scoped_aggregate(aggregates, actor, usage)?;
    }
    Ok(())
}

fn record_scoped_aggregate(
    aggregates: &mut Vec<ScopedUsageAggregate>,
    scope: DiagnosticScope,
    usage: &ProjectedActUsage,
) -> Result<(), UsageProjectionError> {
    if let Some(existing) = aggregates.iter_mut().find(|item| item.scope == scope) {
        return existing.aggregate.record(usage);
    }
    let mut aggregate = UsageAggregate::empty();
    aggregate.record(usage)?;
    aggregates.push(ScopedUsageAggregate { scope, aggregate });
    Ok(())
}

fn increment_count(
    value: SchemaU64,
    event_sequence: SchemaU64,
) -> Result<SchemaU64, UsageProjectionError> {
    value
        .get()
        .checked_add(1)
        .map(SchemaU64::new)
        .ok_or(UsageProjectionError::CountExhausted { event_sequence })
}

fn sum_token_counts(
    current: Option<&TokenCount>,
    incoming: &TokenCount,
    event_sequence: SchemaU64,
) -> Result<TokenCount, UsageProjectionError> {
    let left = match current {
        Some(value) => BigUint::from_str(value.as_str())
            .map_err(|_| UsageProjectionError::InvalidTokenValue { event_sequence })?,
        None => BigUint::default(),
    };
    let right = BigUint::from_str(incoming.as_str())
        .map_err(|_| UsageProjectionError::InvalidTokenValue { event_sequence })?;
    TokenCount::parse(&(left + right).to_str_radix(10))
        .map_err(|_| UsageProjectionError::InvalidTokenValue { event_sequence })
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UsageProjectionError {
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
    MissingActIdentity {
        event_sequence: SchemaU64,
    },
    DuplicateActUsage {
        act_id: RunLocalId,
        first_sequence: SchemaU64,
        event_sequence: SchemaU64,
    },
    AvailabilityMismatch {
        act_id: RunLocalId,
        event_sequence: SchemaU64,
    },
    UsageRecordMismatch {
        event_sequence: SchemaU64,
    },
    AggregateMismatch {
        event_sequence: SchemaU64,
    },
    CountExhausted {
        event_sequence: SchemaU64,
    },
    InvalidTokenValue {
        event_sequence: SchemaU64,
    },
    ModelSchemaMismatch {
        expected: u8,
        actual: u8,
        event_sequence: SchemaU64,
    },
}

impl UsageProjectionError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::RunIdentityMismatch { .. } => "cross_run",
            Self::NonCanonicalSequence { .. } => "noncanonical_sequence",
            Self::SequenceTie { .. } => "sequence_tie",
            Self::SequenceExhausted { .. } => "sequence_exhausted",
            Self::InvalidReference(error) => error.code().as_str(),
            Self::MissingActIdentity { .. } => "missing_act_identity",
            Self::DuplicateActUsage { .. } => "duplicate_act_usage",
            Self::AvailabilityMismatch { .. } => "usage_availability_mismatch",
            Self::UsageRecordMismatch { .. } => "usage_record_mismatch",
            Self::AggregateMismatch { .. } => "usage_aggregate_mismatch",
            Self::CountExhausted { .. } => "usage_count_exhausted",
            Self::InvalidTokenValue { .. } => "usage_token_invalid",
            Self::ModelSchemaMismatch { .. } => "usage_model_schema_mismatch",
        }
    }

    pub const fn event_sequence(&self) -> SchemaU64 {
        match self {
            Self::RunIdentityMismatch { event_sequence, .. }
            | Self::SequenceTie { event_sequence }
            | Self::SequenceExhausted { event_sequence }
            | Self::MissingActIdentity { event_sequence }
            | Self::DuplicateActUsage { event_sequence, .. }
            | Self::AvailabilityMismatch { event_sequence, .. }
            | Self::UsageRecordMismatch { event_sequence }
            | Self::AggregateMismatch { event_sequence }
            | Self::CountExhausted { event_sequence }
            | Self::InvalidTokenValue { event_sequence }
            | Self::ModelSchemaMismatch { event_sequence, .. } => *event_sequence,
            Self::NonCanonicalSequence { actual, .. } => *actual,
            Self::InvalidReference(error) => error.event_sequence(),
        }
    }
}

impl fmt::Display for UsageProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RunIdentityMismatch {
                expected, actual, ..
            } => write!(
                formatter,
                "usage projection expected Run {expected}, found {actual}"
            ),
            Self::NonCanonicalSequence { expected, actual } => write!(
                formatter,
                "usage projection expected sequence {}, found {}",
                expected.get(),
                actual.get()
            ),
            Self::SequenceTie { event_sequence } => write!(
                formatter,
                "usage projection sequence {} is already materialized",
                event_sequence.get()
            ),
            Self::SequenceExhausted { .. } => {
                formatter.write_str("usage projection sequence space is exhausted")
            }
            Self::InvalidReference(error) => fmt::Display::fmt(error, formatter),
            Self::MissingActIdentity { .. } => {
                formatter.write_str("terminal usage is missing its Act identity")
            }
            Self::DuplicateActUsage {
                act_id,
                first_sequence,
                ..
            } => write!(
                formatter,
                "Act {} already has terminal usage at sequence {}",
                act_id.as_str(),
                first_sequence.get()
            ),
            Self::AvailabilityMismatch { act_id, .. } => write!(
                formatter,
                "Act {} terminal usage availability fields are inconsistent",
                act_id.as_str()
            ),
            Self::UsageRecordMismatch { .. } => {
                formatter.write_str("stored terminal usage identity or order is inconsistent")
            }
            Self::AggregateMismatch { .. } => {
                formatter.write_str("stored usage aggregate does not match terminal facts")
            }
            Self::CountExhausted { .. } => {
                formatter.write_str("usage aggregate Act count is exhausted")
            }
            Self::InvalidTokenValue { .. } => {
                formatter.write_str("usage aggregate contains an invalid token integer")
            }
            Self::ModelSchemaMismatch {
                expected, actual, ..
            } => write!(
                formatter,
                "usage model schema version {actual} does not match {expected}"
            ),
        }
    }
}

impl std::error::Error for UsageProjectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidReference(error) => Some(error),
            _ => None,
        }
    }
}
