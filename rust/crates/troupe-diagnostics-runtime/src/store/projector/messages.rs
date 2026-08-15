use std::fmt;

use serde::{Deserialize, Serialize};
use troupe_diagnostics_core::{
    event::{CausalLink, DiagnosticEvent, DiagnosticScope},
    id::{CanonicalUuid, RunLocalId},
    scalar::SchemaU64,
    time::ElapsedNs,
    validate::{ReferenceValidationError, ReferenceValidator, ValidatedEvent},
};

pub const MESSAGE_READ_MODEL_SCHEMA_VERSION: u8 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MessageReadModel {
    model_schema_version: u8,
    run_id: CanonicalUuid,
    through_sequence: SchemaU64,
    through_elapsed_ns: ElapsedNs,
    messages: Vec<ProjectedMessage>,
}

impl MessageReadModel {
    fn empty(run_id: CanonicalUuid) -> Self {
        Self {
            model_schema_version: MESSAGE_READ_MODEL_SCHEMA_VERSION,
            run_id,
            through_sequence: SchemaU64::new(0),
            through_elapsed_ns: ElapsedNs::new(0),
            messages: Vec::new(),
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

    pub fn messages(&self) -> &[ProjectedMessage] {
        &self.messages
    }

    pub fn message(&self, message_id: &RunLocalId) -> Option<&ProjectedMessage> {
        self.messages
            .iter()
            .find(|message| message.message_id() == message_id)
    }

    pub fn open_messages(&self) -> impl Iterator<Item = &ProjectedMessage> {
        self.messages.iter().filter(|message| message.is_open())
    }

    pub fn completed_messages(&self) -> impl Iterator<Item = &ProjectedMessage> {
        self.messages.iter().filter(|message| !message.is_open())
    }

    pub fn messages_in_scope<'model, 'scope>(
        &'model self,
        scope: &'scope DiagnosticScope,
    ) -> impl Iterator<Item = &'model ProjectedMessage> + 'scope
    where
        'model: 'scope,
    {
        self.messages
            .iter()
            .filter(move |message| message.scope() == scope)
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectedMessage {
    run_id: CanonicalUuid,
    message_id: RunLocalId,
    scope: DiagnosticScope,
    first_sequence: SchemaU64,
    first_elapsed_ns: ElapsedNs,
    latest_sequence: SchemaU64,
    latest_elapsed_ns: ElapsedNs,
    source_message_id: Option<String>,
    text: String,
    completion: Option<MessageCompletion>,
}

impl ProjectedMessage {
    fn from_delta(event: &troupe_diagnostics_core::event::AgentMessageDelta) -> Self {
        let header = event.header();
        Self {
            run_id: header.run_id(),
            message_id: event.message_id().clone(),
            scope: header.scope().clone(),
            first_sequence: header.sequence(),
            first_elapsed_ns: header.elapsed_ns(),
            latest_sequence: header.sequence(),
            latest_elapsed_ns: header.elapsed_ns(),
            source_message_id: event.source_message_id().map(str::to_owned),
            text: event.text_delta().to_owned(),
            completion: None,
        }
    }

    fn from_completion(event: &troupe_diagnostics_core::event::AgentMessageCompleted) -> Self {
        let header = event.header();
        Self {
            run_id: header.run_id(),
            message_id: event.message_id().clone(),
            scope: header.scope().clone(),
            first_sequence: header.sequence(),
            first_elapsed_ns: header.elapsed_ns(),
            latest_sequence: header.sequence(),
            latest_elapsed_ns: header.elapsed_ns(),
            source_message_id: None,
            text: String::new(),
            completion: Some(MessageCompletion::from_event(event)),
        }
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub const fn message_id(&self) -> &RunLocalId {
        &self.message_id
    }

    pub const fn scope(&self) -> &DiagnosticScope {
        &self.scope
    }

    pub const fn first_sequence(&self) -> SchemaU64 {
        self.first_sequence
    }

    pub const fn first_elapsed_ns(&self) -> ElapsedNs {
        self.first_elapsed_ns
    }

    pub const fn latest_sequence(&self) -> SchemaU64 {
        self.latest_sequence
    }

    pub const fn latest_elapsed_ns(&self) -> ElapsedNs {
        self.latest_elapsed_ns
    }

    pub fn source_message_id(&self) -> Option<&str> {
        self.source_message_id.as_deref()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub const fn completion(&self) -> Option<&MessageCompletion> {
        self.completion.as_ref()
    }

    pub const fn is_open(&self) -> bool {
        self.completion.is_none()
    }

    pub fn is_truncated(&self) -> bool {
        self.completion
            .as_ref()
            .is_some_and(MessageCompletion::truncated)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MessageCompletion {
    sequence: SchemaU64,
    elapsed_ns: ElapsedNs,
    utf8_bytes: SchemaU64,
    unicode_scalar_count: SchemaU64,
    truncated: bool,
    caused_by: Vec<CausalLink>,
}

impl MessageCompletion {
    fn from_event(event: &troupe_diagnostics_core::event::AgentMessageCompleted) -> Self {
        Self {
            sequence: event.header().sequence(),
            elapsed_ns: event.header().elapsed_ns(),
            utf8_bytes: event.utf8_bytes(),
            unicode_scalar_count: event.unicode_scalar_count(),
            truncated: event.truncated(),
            caused_by: event.header().caused_by().to_vec(),
        }
    }

    pub const fn sequence(&self) -> SchemaU64 {
        self.sequence
    }

    pub const fn elapsed_ns(&self) -> ElapsedNs {
        self.elapsed_ns
    }

    pub const fn utf8_bytes(&self) -> SchemaU64 {
        self.utf8_bytes
    }

    pub const fn unicode_scalar_count(&self) -> SchemaU64 {
        self.unicode_scalar_count
    }

    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn caused_by(&self) -> &[CausalLink] {
        &self.caused_by
    }
}

#[derive(Debug)]
pub struct MessageProjector {
    references: ReferenceValidator,
    model: MessageReadModel,
}

impl MessageProjector {
    pub fn new(run_id: CanonicalUuid) -> Self {
        Self {
            references: ReferenceValidator::new(),
            model: MessageReadModel::empty(run_id),
        }
    }

    pub const fn model(&self) -> &MessageReadModel {
        &self.model
    }

    pub fn into_model(self) -> MessageReadModel {
        self.model
    }

    pub fn apply(&mut self, event: &DiagnosticEvent) -> Result<(), MessageProjectionError> {
        let candidate = candidate_for_event(&self.model, event)?;
        self.references
            .validate(event)
            .map_err(MessageProjectionError::InvalidReference)?;
        self.model = candidate;
        Ok(())
    }

    pub fn apply_all(&mut self, events: &[DiagnosticEvent]) -> Result<(), MessageProjectionError> {
        for event in events {
            self.apply(event)?;
        }
        Ok(())
    }
}

pub(crate) fn project_validated_event(
    model: &MessageReadModel,
    validated: ValidatedEvent<'_>,
) -> Result<MessageReadModel, MessageProjectionError> {
    candidate_for_event(model, validated.event())
}

fn candidate_for_event(
    model: &MessageReadModel,
    event: &DiagnosticEvent,
) -> Result<MessageReadModel, MessageProjectionError> {
    validate_position(model, event)?;
    let mut candidate = model.clone();
    match event {
        DiagnosticEvent::AgentMessageDelta(delta) => apply_delta(&mut candidate, delta)?,
        DiagnosticEvent::AgentMessageCompleted(completion) => {
            apply_completion(&mut candidate, completion)?;
        }
        DiagnosticEvent::SpanStarted(_)
        | DiagnosticEvent::SpanFinished(_)
        | DiagnosticEvent::InstantOccurred(_)
        | DiagnosticEvent::CounterSampled(_)
        | DiagnosticEvent::AgentPlanSnapshot(_)
        | DiagnosticEvent::ContextUsageSampled(_)
        | DiagnosticEvent::ActTokenUsageFinalized(_)
        | DiagnosticEvent::ObservationGap(_)
        | DiagnosticEvent::CustomSpanStarted(_)
        | DiagnosticEvent::CustomSpanFinished(_)
        | DiagnosticEvent::CustomInstantOccurred(_)
        | DiagnosticEvent::CustomCounterSampled(_) => {}
    }
    candidate.through_sequence = event.header().sequence();
    candidate.through_elapsed_ns = candidate
        .through_elapsed_ns
        .max(event.header().elapsed_ns());
    Ok(candidate)
}

fn apply_delta(
    model: &mut MessageReadModel,
    event: &troupe_diagnostics_core::event::AgentMessageDelta,
) -> Result<(), MessageProjectionError> {
    let header = event.header();
    let Some(message) = model
        .messages
        .iter_mut()
        .find(|message| message.message_id() == event.message_id())
    else {
        model.messages.push(ProjectedMessage::from_delta(event));
        return Ok(());
    };

    validate_scope_identity(message, header.scope(), header.sequence())?;
    if message.completion.is_some() {
        return Err(MessageProjectionError::DeltaAfterCompletion {
            message_id: event.message_id().clone(),
            event_sequence: header.sequence(),
        });
    }
    if let Some(source_message_id) = event.source_message_id() {
        if let Some(expected) = message.source_message_id()
            && expected != source_message_id
        {
            return Err(MessageProjectionError::IdentityMismatch {
                message_id: event.message_id().clone(),
                event_sequence: header.sequence(),
                field: MessageIdentityField::SourceMessageId,
            });
        }
        if message.source_message_id.is_none() {
            message.source_message_id = Some(source_message_id.to_owned());
        }
    }
    message.text.push_str(event.text_delta());
    message.latest_sequence = header.sequence();
    message.latest_elapsed_ns = header.elapsed_ns();
    Ok(())
}

fn apply_completion(
    model: &mut MessageReadModel,
    event: &troupe_diagnostics_core::event::AgentMessageCompleted,
) -> Result<(), MessageProjectionError> {
    let header = event.header();
    let Some(message) = model
        .messages
        .iter_mut()
        .find(|message| message.message_id() == event.message_id())
    else {
        model
            .messages
            .push(ProjectedMessage::from_completion(event));
        return Ok(());
    };

    validate_scope_identity(message, header.scope(), header.sequence())?;
    if message.completion.is_some() {
        return Err(MessageProjectionError::DuplicateCompletion {
            message_id: event.message_id().clone(),
            event_sequence: header.sequence(),
        });
    }
    message.completion = Some(MessageCompletion::from_event(event));
    message.latest_sequence = header.sequence();
    message.latest_elapsed_ns = header.elapsed_ns();
    Ok(())
}

fn validate_scope_identity(
    message: &ProjectedMessage,
    actual: &DiagnosticScope,
    event_sequence: SchemaU64,
) -> Result<(), MessageProjectionError> {
    if message.scope() != actual {
        return Err(MessageProjectionError::IdentityMismatch {
            message_id: message.message_id().clone(),
            event_sequence,
            field: MessageIdentityField::Scope,
        });
    }
    Ok(())
}

fn validate_position(
    model: &MessageReadModel,
    event: &DiagnosticEvent,
) -> Result<(), MessageProjectionError> {
    let header = event.header();
    if header.run_id() != model.run_id() {
        return Err(MessageProjectionError::RunIdentityMismatch {
            expected: model.run_id(),
            actual: header.run_id(),
            event_sequence: header.sequence(),
        });
    }
    let Some(expected) = model.through_sequence().get().checked_add(1) else {
        return Err(MessageProjectionError::SequenceExhausted {
            event_sequence: header.sequence(),
        });
    };
    if header.sequence().get() != expected {
        return Err(MessageProjectionError::NonCanonicalSequence {
            expected: SchemaU64::new(expected),
            actual: header.sequence(),
        });
    }
    Ok(())
}

pub fn project_messages(
    run_id: CanonicalUuid,
    events: &[DiagnosticEvent],
) -> Result<MessageReadModel, MessageProjectionError> {
    let mut projector = MessageProjector::new(run_id);
    projector.apply_all(events)?;
    Ok(projector.into_model())
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageIdentityField {
    Scope,
    SourceMessageId,
}

impl MessageIdentityField {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scope => "scope",
            Self::SourceMessageId => "source_message_id",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MessageProjectionError {
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
    IdentityMismatch {
        message_id: RunLocalId,
        event_sequence: SchemaU64,
        field: MessageIdentityField,
    },
    DuplicateCompletion {
        message_id: RunLocalId,
        event_sequence: SchemaU64,
    },
    DeltaAfterCompletion {
        message_id: RunLocalId,
        event_sequence: SchemaU64,
    },
}

impl MessageProjectionError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::RunIdentityMismatch { .. } => "cross_run",
            Self::NonCanonicalSequence { .. } => "noncanonical_sequence",
            Self::SequenceExhausted { .. } => "sequence_exhausted",
            Self::InvalidReference(error) => error.code().as_str(),
            Self::IdentityMismatch { .. } => "message_identity_mismatch",
            Self::DuplicateCompletion { .. } => "duplicate_completion",
            Self::DeltaAfterCompletion { .. } => "delta_after_completion",
        }
    }

    pub const fn event_sequence(&self) -> SchemaU64 {
        match self {
            Self::RunIdentityMismatch { event_sequence, .. }
            | Self::SequenceExhausted { event_sequence }
            | Self::IdentityMismatch { event_sequence, .. }
            | Self::DuplicateCompletion { event_sequence, .. }
            | Self::DeltaAfterCompletion { event_sequence, .. } => *event_sequence,
            Self::NonCanonicalSequence { actual, .. } => *actual,
            Self::InvalidReference(error) => error.event_sequence(),
        }
    }
}

impl fmt::Display for MessageProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RunIdentityMismatch {
                expected, actual, ..
            } => write!(
                formatter,
                "message projection expected Run {expected}, found {actual}"
            ),
            Self::NonCanonicalSequence { expected, actual } => write!(
                formatter,
                "message projection expected sequence {}, found {}",
                expected.get(),
                actual.get()
            ),
            Self::SequenceExhausted { .. } => {
                formatter.write_str("message projection sequence space is exhausted")
            }
            Self::InvalidReference(error) => fmt::Display::fmt(error, formatter),
            Self::IdentityMismatch {
                message_id, field, ..
            } => write!(
                formatter,
                "message {message_id:?} changed its {} identity",
                field.as_str()
            ),
            Self::DuplicateCompletion { message_id, .. } => {
                write!(formatter, "message {message_id:?} completed more than once")
            }
            Self::DeltaAfterCompletion { message_id, .. } => {
                write!(
                    formatter,
                    "message {message_id:?} received a delta after completion"
                )
            }
        }
    }
}

impl std::error::Error for MessageProjectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidReference(error) => Some(error),
            _ => None,
        }
    }
}
