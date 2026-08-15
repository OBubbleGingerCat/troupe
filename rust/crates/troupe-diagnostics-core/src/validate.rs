use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use serde::{Deserialize, Serialize};

use crate::{
    event::{DiagnosticEvent, DiagnosticScope},
    id::CanonicalUuid,
    scalar::SchemaU64,
    time::ElapsedNs,
};

pub const MAX_CAUSAL_LINKS: usize = 16;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceValidationCode {
    CrossRun,
    ForwardLink,
    SelfLink,
    FinishBeforeStart,
    DoubleFinish,
    ChildOutsideParent,
    KindMismatch,
    TooManyCausalLinks,
    ReferenceNotFound,
    ReferenceClosed,
    ScopeMismatch,
    InvalidScope,
    DuplicateSequence,
}

impl ReferenceValidationCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CrossRun => "cross_run",
            Self::ForwardLink => "forward_link",
            Self::SelfLink => "self_link",
            Self::FinishBeforeStart => "finish_before_start",
            Self::DoubleFinish => "double_finish",
            Self::ChildOutsideParent => "child_outside_parent",
            Self::KindMismatch => "kind_mismatch",
            Self::TooManyCausalLinks => "too_many_causal_links",
            Self::ReferenceNotFound => "reference_not_found",
            Self::ReferenceClosed => "reference_closed",
            Self::ScopeMismatch => "scope_mismatch",
            Self::InvalidScope => "invalid_scope",
            Self::DuplicateSequence => "duplicate_sequence",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferenceValidationError {
    code: ReferenceValidationCode,
    run_id: CanonicalUuid,
    event_sequence: SchemaU64,
    referenced_sequence: Option<SchemaU64>,
}

impl ReferenceValidationError {
    const fn new(
        code: ReferenceValidationCode,
        run_id: CanonicalUuid,
        event_sequence: SchemaU64,
        referenced_sequence: Option<SchemaU64>,
    ) -> Self {
        Self {
            code,
            run_id,
            event_sequence,
            referenced_sequence,
        }
    }

    pub const fn code(&self) -> ReferenceValidationCode {
        self.code
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub const fn event_sequence(&self) -> SchemaU64 {
        self.event_sequence
    }

    pub const fn referenced_sequence(&self) -> Option<SchemaU64> {
        self.referenced_sequence
    }
}

impl fmt::Display for ReferenceValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} at event {}:{}",
            self.code.as_str(),
            self.run_id,
            self.event_sequence.get()
        )?;
        if let Some(referenced_sequence) = self.referenced_sequence {
            write!(formatter, " referencing {}", referenced_sequence.get())?;
        }
        Ok(())
    }
}

impl std::error::Error for ReferenceValidationError {}

#[derive(Clone, Copy, Debug)]
pub struct ValidatedEvent<'event> {
    event: &'event DiagnosticEvent,
}

impl<'event> ValidatedEvent<'event> {
    pub const fn event(&self) -> &'event DiagnosticEvent {
        self.event
    }
}

#[derive(Debug)]
pub struct ValidatedEventStream<'events> {
    events: &'events [DiagnosticEvent],
}

impl ValidatedEventStream<'_> {
    pub const fn len(&self) -> usize {
        self.events.len()
    }

    pub const fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = ValidatedEvent<'_>> {
        self.events.iter().map(|event| ValidatedEvent { event })
    }
}

pub fn validate_event_stream(
    events: &[DiagnosticEvent],
) -> Result<ValidatedEventStream<'_>, ReferenceValidationError> {
    let mut validator = ReferenceValidator::new();
    for event in events {
        validator.validate(event)?;
    }
    Ok(ValidatedEventStream { events })
}

#[derive(Clone, Debug, Default)]
pub struct ReferenceValidator {
    run_id: Option<CanonicalUuid>,
    event_sequences: BTreeSet<SchemaU64>,
    spans: BTreeMap<SchemaU64, SpanRecord>,
}

impl ReferenceValidator {
    pub const fn new() -> Self {
        Self {
            run_id: None,
            event_sequences: BTreeSet::new(),
            spans: BTreeMap::new(),
        }
    }

    pub fn validate<'event>(
        &mut self,
        event: &'event DiagnosticEvent,
    ) -> Result<ValidatedEvent<'event>, ReferenceValidationError> {
        let change = self.prepare(event)?;
        self.commit(event, change);
        Ok(ValidatedEvent { event })
    }

    pub fn validate_then<'event, T, E>(
        &mut self,
        event: &'event DiagnosticEvent,
        consumer: impl FnOnce(ValidatedEvent<'event>) -> Result<T, E>,
    ) -> Result<Result<T, E>, ReferenceValidationError> {
        let change = self.prepare(event)?;
        let result = consumer(ValidatedEvent { event });
        if result.is_ok() {
            self.commit(event, change);
        }
        Ok(result)
    }

    fn prepare(&self, event: &DiagnosticEvent) -> Result<StateChange, ReferenceValidationError> {
        let header = event.header();
        let run_id = header.run_id();
        let sequence = header.sequence();

        if self.run_id.is_some_and(|expected| expected != run_id) {
            return Err(error(event, ReferenceValidationCode::CrossRun, None));
        }
        if self.event_sequences.contains(&sequence) {
            return Err(error(
                event,
                ReferenceValidationCode::DuplicateSequence,
                Some(sequence),
            ));
        }
        validate_scope(event)?;
        self.validate_causal_links(event)?;

        let change = match event {
            DiagnosticEvent::SpanStarted(start) => {
                self.validate_span_start(event, SpanFamily::BuiltIn, start.parent_span_id())?
            }
            DiagnosticEvent::CustomSpanStarted(start) => {
                self.validate_span_start(event, SpanFamily::Custom, start.parent_span_id())?
            }
            DiagnosticEvent::SpanFinished(finish) => {
                self.validate_span_finish(event, SpanFamily::BuiltIn, finish.span_id())?
            }
            DiagnosticEvent::CustomSpanFinished(finish) => {
                self.validate_span_finish(event, SpanFamily::Custom, finish.span_id())?
            }
            DiagnosticEvent::InstantOccurred(instant) => {
                self.validate_containing_span(event, instant.containing_span_id())?
            }
            DiagnosticEvent::CustomInstantOccurred(instant) => {
                self.validate_containing_span(event, instant.containing_span_id())?
            }
            DiagnosticEvent::CounterSampled(_)
            | DiagnosticEvent::AgentMessageDelta(_)
            | DiagnosticEvent::AgentMessageCompleted(_)
            | DiagnosticEvent::AgentPlanSnapshot(_)
            | DiagnosticEvent::ContextUsageSampled(_)
            | DiagnosticEvent::ActTokenUsageFinalized(_)
            | DiagnosticEvent::ObservationGap(_)
            | DiagnosticEvent::CustomCounterSampled(_) => StateChange::RecordEvent,
        };
        Ok(change)
    }

    fn commit(&mut self, event: &DiagnosticEvent, change: StateChange) {
        let header = event.header();
        let run_id = header.run_id();
        let sequence = header.sequence();
        let inserted = self.event_sequences.insert(sequence);
        debug_assert!(
            inserted,
            "duplicate event sequence was checked before commit"
        );
        match change {
            StateChange::RecordEvent => {}
            StateChange::StartSpan(record) => {
                let previous = self.spans.insert(sequence, *record);
                debug_assert!(previous.is_none(), "a span ID is its unique start sequence");
            }
            StateChange::FinishSpan {
                span_id,
                elapsed_ns,
            } => {
                let span = self
                    .spans
                    .get_mut(&span_id)
                    .expect("finish reference was checked before commit");
                debug_assert!(span.finished_at.is_none());
                span.finished_at = Some(elapsed_ns);
            }
            StateChange::ContainedInstant {
                span_id,
                elapsed_ns,
            } => {
                let span = self
                    .spans
                    .get_mut(&span_id)
                    .expect("containing reference was checked before commit");
                span.latest_contained_at = Some(
                    span.latest_contained_at
                        .map_or(elapsed_ns, |current| current.max(elapsed_ns)),
                );
            }
        }
        self.run_id = Some(run_id);
    }

    fn validate_causal_links(
        &self,
        event: &DiagnosticEvent,
    ) -> Result<(), ReferenceValidationError> {
        let header = event.header();
        if header.caused_by().len() > MAX_CAUSAL_LINKS {
            return Err(error(
                event,
                ReferenceValidationCode::TooManyCausalLinks,
                None,
            ));
        }

        let sequence = header.sequence();
        for link in header.caused_by() {
            let source = link.source_sequence();
            if source == sequence {
                return Err(error(
                    event,
                    ReferenceValidationCode::SelfLink,
                    Some(source),
                ));
            }
            if source > sequence {
                return Err(error(
                    event,
                    ReferenceValidationCode::ForwardLink,
                    Some(source),
                ));
            }
            if !self.event_sequences.contains(&source) {
                return Err(error(
                    event,
                    ReferenceValidationCode::ReferenceNotFound,
                    Some(source),
                ));
            }
        }
        Ok(())
    }

    fn validate_span_start(
        &self,
        event: &DiagnosticEvent,
        family: SpanFamily,
        parent_span_id: Option<SchemaU64>,
    ) -> Result<StateChange, ReferenceValidationError> {
        if let Some(parent_span_id) = parent_span_id {
            let parent = self.validate_open_span_reference(event, parent_span_id)?;
            if !scope_contains(&parent.scope, event.header().scope()) {
                return Err(error(
                    event,
                    ReferenceValidationCode::ScopeMismatch,
                    Some(parent_span_id),
                ));
            }
            if event.header().elapsed_ns() < parent.started_at {
                return Err(error(
                    event,
                    ReferenceValidationCode::ChildOutsideParent,
                    Some(parent_span_id),
                ));
            }
        }

        Ok(StateChange::StartSpan(Box::new(SpanRecord {
            family,
            scope: event.header().scope().clone(),
            started_at: event.header().elapsed_ns(),
            finished_at: None,
            latest_contained_at: None,
            parent_span_id,
        })))
    }

    fn validate_span_finish(
        &self,
        event: &DiagnosticEvent,
        family: SpanFamily,
        span_id: SchemaU64,
    ) -> Result<StateChange, ReferenceValidationError> {
        let header = event.header();
        if span_id >= header.sequence() {
            return Err(error(
                event,
                ReferenceValidationCode::FinishBeforeStart,
                Some(span_id),
            ));
        }

        let Some(span) = self.spans.get(&span_id) else {
            return Err(error(event, self.missing_span_code(span_id), Some(span_id)));
        };
        if span.family != family {
            return Err(error(
                event,
                ReferenceValidationCode::KindMismatch,
                Some(span_id),
            ));
        }
        if span.finished_at.is_some() {
            return Err(error(
                event,
                ReferenceValidationCode::DoubleFinish,
                Some(span_id),
            ));
        }
        if &span.scope != header.scope() {
            return Err(error(
                event,
                ReferenceValidationCode::ScopeMismatch,
                Some(span_id),
            ));
        }
        if header.elapsed_ns() < span.started_at {
            return Err(error(
                event,
                ReferenceValidationCode::FinishBeforeStart,
                Some(span_id),
            ));
        }
        if span
            .latest_contained_at
            .is_some_and(|contained_at| contained_at > header.elapsed_ns())
        {
            return Err(error(
                event,
                ReferenceValidationCode::ChildOutsideParent,
                Some(span_id),
            ));
        }

        if self
            .spans
            .values()
            .filter(|candidate| candidate.parent_span_id == Some(span_id))
            .any(|child| {
                child
                    .finished_at
                    .is_none_or(|finished_at| finished_at > header.elapsed_ns())
            })
        {
            return Err(error(
                event,
                ReferenceValidationCode::ChildOutsideParent,
                Some(span_id),
            ));
        }

        Ok(StateChange::FinishSpan {
            span_id,
            elapsed_ns: header.elapsed_ns(),
        })
    }

    fn validate_containing_span(
        &self,
        event: &DiagnosticEvent,
        containing_span_id: Option<SchemaU64>,
    ) -> Result<StateChange, ReferenceValidationError> {
        let Some(containing_span_id) = containing_span_id else {
            return Ok(StateChange::RecordEvent);
        };
        let containing = self.validate_open_span_reference(event, containing_span_id)?;
        if !scope_contains(&containing.scope, event.header().scope()) {
            return Err(error(
                event,
                ReferenceValidationCode::ScopeMismatch,
                Some(containing_span_id),
            ));
        }
        if event.header().elapsed_ns() < containing.started_at {
            return Err(error(
                event,
                ReferenceValidationCode::ChildOutsideParent,
                Some(containing_span_id),
            ));
        }
        Ok(StateChange::ContainedInstant {
            span_id: containing_span_id,
            elapsed_ns: event.header().elapsed_ns(),
        })
    }

    fn validate_open_span_reference(
        &self,
        event: &DiagnosticEvent,
        span_id: SchemaU64,
    ) -> Result<&SpanRecord, ReferenceValidationError> {
        let header = event.header();
        if span_id == header.sequence() {
            return Err(error(
                event,
                ReferenceValidationCode::SelfLink,
                Some(span_id),
            ));
        }
        if span_id > header.sequence() {
            return Err(error(
                event,
                ReferenceValidationCode::ForwardLink,
                Some(span_id),
            ));
        }

        let Some(span) = self.spans.get(&span_id) else {
            return Err(error(event, self.missing_span_code(span_id), Some(span_id)));
        };
        if span.finished_at.is_some() {
            return Err(error(
                event,
                ReferenceValidationCode::ReferenceClosed,
                Some(span_id),
            ));
        }
        Ok(span)
    }

    fn missing_span_code(&self, span_id: SchemaU64) -> ReferenceValidationCode {
        if self.event_sequences.contains(&span_id) {
            ReferenceValidationCode::KindMismatch
        } else {
            ReferenceValidationCode::ReferenceNotFound
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SpanFamily {
    BuiltIn,
    Custom,
}

#[derive(Clone, Debug)]
struct SpanRecord {
    family: SpanFamily,
    scope: DiagnosticScope,
    started_at: ElapsedNs,
    finished_at: Option<ElapsedNs>,
    latest_contained_at: Option<ElapsedNs>,
    parent_span_id: Option<SchemaU64>,
}

#[derive(Debug)]
enum StateChange {
    RecordEvent,
    StartSpan(Box<SpanRecord>),
    FinishSpan {
        span_id: SchemaU64,
        elapsed_ns: ElapsedNs,
    },
    ContainedInstant {
        span_id: SchemaU64,
        elapsed_ns: ElapsedNs,
    },
}

fn validate_scope(event: &DiagnosticEvent) -> Result<(), ReferenceValidationError> {
    let header_invalid = has_unknown_scope_sentinel(event.header().scope());
    let affected_invalid = match event {
        DiagnosticEvent::ObservationGap(gap) => {
            gap.affected_scope().is_some_and(has_unknown_scope_sentinel)
        }
        _ => false,
    };
    if header_invalid || affected_invalid {
        return Err(error(event, ReferenceValidationCode::InvalidScope, None));
    }
    Ok(())
}

fn has_unknown_scope_sentinel(scope: &DiagnosticScope) -> bool {
    scope
        .session_generation()
        .is_some_and(|generation| generation.get() == 0)
}

fn scope_contains(parent: &DiagnosticScope, child: &DiagnosticScope) -> bool {
    optional_id_contains(parent.scene_id(), child.scene_id())
        && optional_id_contains(parent.actor_id(), child.actor_id())
        && optional_id_contains(parent.cue_id(), child.cue_id())
        && optional_id_contains(parent.effect_id(), child.effect_id())
        && optional_id_contains(parent.act_id(), child.act_id())
        && optional_id_contains(parent.tool_call_id(), child.tool_call_id())
        && parent
            .session_generation()
            .is_none_or(|generation| child.session_generation() == Some(generation))
}

fn optional_id_contains<T: PartialEq>(parent: Option<&T>, child: Option<&T>) -> bool {
    parent.is_none_or(|value| child == Some(value))
}

fn error(
    event: &DiagnosticEvent,
    code: ReferenceValidationCode,
    referenced_sequence: Option<SchemaU64>,
) -> ReferenceValidationError {
    ReferenceValidationError::new(
        code,
        event.header().run_id(),
        event.header().sequence(),
        referenced_sequence,
    )
}
