use std::{
    fmt,
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use crate::{
    event::DiagnosticEvent,
    id::CanonicalUuid,
    kinds::SpanKind,
    scalar::SchemaU64,
    validate::{ReferenceValidationError, ReferenceValidator},
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct EventIdentity {
    run_id: CanonicalUuid,
    sequence: SchemaU64,
}

impl EventIdentity {
    const fn new(run_id: CanonicalUuid, sequence: SchemaU64) -> Self {
        Self { run_id, sequence }
    }

    pub const fn run_id(self) -> CanonicalUuid {
        self.run_id
    }

    pub const fn sequence(self) -> SchemaU64 {
        self.sequence
    }
}

pub trait DiagnosticEventCandidate {
    fn materialize(self, identity: EventIdentity) -> DiagnosticEvent;
}

impl<F> DiagnosticEventCandidate for F
where
    F: FnOnce(EventIdentity) -> DiagnosticEvent,
{
    fn materialize(self, identity: EventIdentity) -> DiagnosticEvent {
        self(identity)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmissionSize {
    event_count: usize,
    canonical_bytes: usize,
}

impl AdmissionSize {
    const fn one_event(canonical_bytes: usize) -> Self {
        Self {
            event_count: 1,
            canonical_bytes,
        }
    }

    pub const fn event_count(self) -> usize {
        self.event_count
    }

    pub const fn canonical_bytes(self) -> usize {
        self.canonical_bytes
    }
}

pub trait AdmissionReservation: Send {
    fn commit(self, event: AcceptedDiagnosticEvent);
}

pub trait AdmissionReserver: Send {
    type Error: std::error::Error + Send + Sync + 'static;
    type Reservation: AdmissionReservation;

    fn try_reserve(&mut self, size: AdmissionSize) -> Result<Self::Reservation, Self::Error>;

    fn try_reserve_fatal(&mut self, size: AdmissionSize) -> Result<Self::Reservation, Self::Error> {
        self.try_reserve(size)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CueCaptureDecision {
    Capture,
    Suppress,
    Resume { suppressed_cues: u64 },
}

pub trait MandatoryDurableReserver: AdmissionReserver {
    fn try_reserve_cue(&mut self, size: AdmissionSize) -> Result<Self::Reservation, Self::Error> {
        self.try_reserve(size)
    }

    fn begin_cue_capture(&self) -> Result<CueCaptureDecision, Self::Error> {
        Ok(CueCaptureDecision::Capture)
    }

    fn finish_cue_capture(&self) -> Result<Option<u64>, Self::Error> {
        Ok(None)
    }
}

pub trait BoundedInMemoryReserver: AdmissionReserver {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedDiagnosticEvent(Arc<AcceptedDiagnosticEventInner>);

#[derive(Debug, Eq, PartialEq)]
struct AcceptedDiagnosticEventInner {
    event: DiagnosticEvent,
    canonical_bytes: Box<[u8]>,
    built_in_span_kind: Option<SpanKind>,
    subscriber_local: bool,
}

impl AcceptedDiagnosticEvent {
    fn new(
        event: DiagnosticEvent,
        canonical_bytes: Vec<u8>,
        built_in_span_kind: Option<SpanKind>,
        subscriber_local: bool,
    ) -> Self {
        Self(Arc::new(AcceptedDiagnosticEventInner {
            event,
            canonical_bytes: canonical_bytes.into_boxed_slice(),
            built_in_span_kind,
            subscriber_local,
        }))
    }

    pub fn event(&self) -> &DiagnosticEvent {
        &self.0.event
    }

    pub fn canonical_bytes(&self) -> &[u8] {
        &self.0.canonical_bytes
    }

    /// Returns the built-in kind carried by a start or resolved from its finish reference.
    pub fn built_in_span_kind(&self) -> Option<SpanKind> {
        self.0.built_in_span_kind
    }

    pub fn is_subscriber_local(&self) -> bool {
        self.0.subscriber_local
    }

    pub fn identity(&self) -> EventIdentity {
        EventIdentity::new(
            self.event().header().run_id(),
            self.event().header().sequence(),
        )
    }

    pub fn same_fact(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DeliveryFailure {
    code: &'static str,
}

impl DeliveryFailure {
    pub const fn new(code: &'static str) -> Self {
        Self { code }
    }

    pub const fn code(self) -> &'static str {
        self.code
    }
}

impl fmt::Display for DeliveryFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code)
    }
}

impl std::error::Error for DeliveryFailure {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryOutcome {
    NotConfigured,
    Delivered,
    Failed(DeliveryFailure),
}

pub trait LiveEventNotifier: Send {
    fn notify(&mut self, event: AcceptedDiagnosticEvent) -> Result<(), DeliveryFailure>;
}

pub trait ActEventSubscriber: Send + Sync {
    fn deliver(&self, event: AcceptedDiagnosticEvent) -> Result<(), DeliveryFailure>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscriberLocalGap {
    dropped_events: usize,
    dropped_canonical_bytes: usize,
}

impl SubscriberLocalGap {
    pub const fn new(dropped_events: usize, dropped_canonical_bytes: usize) -> Self {
        Self {
            dropped_events,
            dropped_canonical_bytes,
        }
    }

    pub const fn dropped_events(self) -> usize {
        self.dropped_events
    }

    pub const fn dropped_canonical_bytes(self) -> usize {
        self.dropped_canonical_bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmissionReceipt {
    accepted: AcceptedDiagnosticEvent,
    live_delivery: DeliveryOutcome,
    subscriber_delivery: DeliveryOutcome,
}

impl AdmissionReceipt {
    pub const fn accepted(&self) -> &AcceptedDiagnosticEvent {
        &self.accepted
    }

    pub const fn live_delivery(&self) -> &DeliveryOutcome {
        &self.live_delivery
    }

    pub const fn subscriber_delivery(&self) -> &DeliveryOutcome {
        &self.subscriber_delivery
    }
}

pub struct SubscriberLocalStream {
    run_id: CanonicalUuid,
    next_sequence: Option<u64>,
    sequence_exhausted: bool,
    validator: ReferenceValidator,
}

impl SubscriberLocalStream {
    pub const fn new(run_id: CanonicalUuid) -> Self {
        Self {
            run_id,
            next_sequence: None,
            sequence_exhausted: false,
            validator: ReferenceValidator::new(),
        }
    }

    pub fn seed(&mut self, event: DiagnosticEvent) -> Result<(), SubscriberLocalStreamError> {
        let identity = EventIdentity::new(event.header().run_id(), event.header().sequence());
        self.validate_seed_identity(identity)?;
        self.validator
            .validate(&event)
            .map_err(SubscriberLocalStreamError::Reference)?;
        self.advance(identity.sequence());
        Ok(())
    }

    pub fn admit<C>(
        &mut self,
        candidate: C,
        subscriber: Option<&dyn ActEventSubscriber>,
    ) -> Result<AdmissionReceipt, SubscriberLocalStreamError>
    where
        C: DiagnosticEventCandidate,
    {
        if self.sequence_exhausted {
            return Err(SubscriberLocalStreamError::SequenceExhausted);
        }
        let next = self
            .next_sequence
            .ok_or(SubscriberLocalStreamError::SequenceUnavailable)?;
        let identity = EventIdentity::new(self.run_id, SchemaU64::new(next));
        let event = candidate.materialize(identity);
        let actual = EventIdentity::new(event.header().run_id(), event.header().sequence());
        if actual != identity {
            return Err(SubscriberLocalStreamError::IdentityMismatch {
                expected: identity,
                actual,
            });
        }
        self.accept(event, subscriber)
    }

    fn accept(
        &mut self,
        event: DiagnosticEvent,
        subscriber: Option<&dyn ActEventSubscriber>,
    ) -> Result<AdmissionReceipt, SubscriberLocalStreamError> {
        let canonical_bytes = serde_json::to_vec(&event)
            .map_err(|_| SubscriberLocalStreamError::CanonicalEncoding)?;
        let validated = self
            .validator
            .validate(&event)
            .map_err(SubscriberLocalStreamError::Reference)?;
        let built_in_span_kind = validated.built_in_span_kind();
        let accepted =
            AcceptedDiagnosticEvent::new(event, canonical_bytes, built_in_span_kind, true);
        self.advance(accepted.identity().sequence());
        let subscriber_delivery = match subscriber {
            Some(subscriber) => match subscriber.deliver(accepted.clone()) {
                Ok(()) => DeliveryOutcome::Delivered,
                Err(error) => DeliveryOutcome::Failed(error),
            },
            None => DeliveryOutcome::NotConfigured,
        };
        Ok(AdmissionReceipt {
            accepted,
            live_delivery: DeliveryOutcome::NotConfigured,
            subscriber_delivery,
        })
    }

    fn validate_seed_identity(
        &self,
        identity: EventIdentity,
    ) -> Result<(), SubscriberLocalStreamError> {
        if identity.run_id() != self.run_id {
            return Err(SubscriberLocalStreamError::RunIdentityMismatch);
        }
        if self.sequence_exhausted {
            return Err(SubscriberLocalStreamError::SequenceExhausted);
        }
        if let Some(next) = self.next_sequence {
            let actual = identity.sequence().get();
            if actual < next {
                return Err(SubscriberLocalStreamError::SequenceRegression {
                    expected_at_least: next,
                    actual,
                });
            }
        }
        Ok(())
    }

    fn advance(&mut self, sequence: SchemaU64) {
        match sequence.get().checked_add(1) {
            Some(next) => self.next_sequence = Some(next),
            None => {
                self.next_sequence = None;
                self.sequence_exhausted = true;
            }
        }
    }
}

#[derive(Debug)]
pub enum SubscriberLocalStreamError {
    RunIdentityMismatch,
    SequenceUnavailable,
    SequenceExhausted,
    SequenceRegression {
        expected_at_least: u64,
        actual: u64,
    },
    IdentityMismatch {
        expected: EventIdentity,
        actual: EventIdentity,
    },
    CanonicalEncoding,
    Reference(ReferenceValidationError),
}

impl fmt::Display for SubscriberLocalStreamError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RunIdentityMismatch => {
                formatter.write_str("subscriber-local Run identity differs")
            }
            Self::SequenceUnavailable => {
                formatter.write_str("subscriber-local sequence is not initialized")
            }
            Self::SequenceExhausted => {
                formatter.write_str("subscriber-local sequence is exhausted")
            }
            Self::SequenceRegression {
                expected_at_least,
                actual,
            } => write!(
                formatter,
                "subscriber-local sequence is {actual}, expected at least {expected_at_least}"
            ),
            Self::IdentityMismatch { expected, actual } => write!(
                formatter,
                "subscriber-local candidate identity {}:{} differs from assigned {}:{}",
                actual.run_id(),
                actual.sequence().get(),
                expected.run_id(),
                expected.sequence().get(),
            ),
            Self::CanonicalEncoding => {
                formatter.write_str("subscriber-local event canonical encoding failed")
            }
            Self::Reference(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for SubscriberLocalStreamError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Reference(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum HubAdmissionError<E> {
    StatePoisoned,
    SequenceExhausted,
    CandidateIdentityMismatch {
        expected: EventIdentity,
        actual: EventIdentity,
    },
    CanonicalEncoding,
    Reference(ReferenceValidationError),
    Reservation(E),
}

impl<E> HubAdmissionError<E> {
    pub const fn expected_identity(&self) -> Option<EventIdentity> {
        match self {
            Self::CandidateIdentityMismatch { expected, .. } => Some(*expected),
            _ => None,
        }
    }

    pub const fn actual_identity(&self) -> Option<EventIdentity> {
        match self {
            Self::CandidateIdentityMismatch { actual, .. } => Some(*actual),
            _ => None,
        }
    }
}

impl<E: fmt::Display> fmt::Display for HubAdmissionError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StatePoisoned => formatter.write_str("diagnostic hub state is poisoned"),
            Self::SequenceExhausted => {
                formatter.write_str("diagnostic event sequence is exhausted")
            }
            Self::CandidateIdentityMismatch { expected, actual } => write!(
                formatter,
                "diagnostic candidate identity {}:{} differs from assigned {}:{}",
                actual.run_id(),
                actual.sequence().get(),
                expected.run_id(),
                expected.sequence().get(),
            ),
            Self::CanonicalEncoding => {
                formatter.write_str("diagnostic event canonical encoding failed")
            }
            Self::Reference(error) => fmt::Display::fmt(error, formatter),
            Self::Reservation(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl<E> std::error::Error for HubAdmissionError<E>
where
    E: std::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Reference(error) => Some(error),
            Self::Reservation(error) => Some(error),
            _ => None,
        }
    }
}

pub enum ProductionProfile {}

pub enum SinkOnlyProfile {}

pub struct DiagnosticHub<R, P> {
    run_id: CanonicalUuid,
    state: Mutex<HubState<R>>,
    subscriber_delivery_order: Mutex<()>,
    profile: PhantomData<fn() -> P>,
}

pub type ProductionDiagnosticHub<R> = DiagnosticHub<R, ProductionProfile>;
pub type SinkOnlyDiagnosticHub<R> = DiagnosticHub<R, SinkOnlyProfile>;

struct HubState<R> {
    run_id: CanonicalUuid,
    next_sequence: Option<u64>,
    reserver: R,
    validator: ReferenceValidator,
    live_notifier: Option<Box<dyn LiveEventNotifier>>,
}

impl<R> DiagnosticHub<R, ProductionProfile>
where
    R: MandatoryDurableReserver,
{
    pub fn production(
        run_id: CanonicalUuid,
        reserver: R,
        live_notifier: Box<dyn LiveEventNotifier>,
    ) -> Self {
        Self::new(run_id, reserver, Some(live_notifier))
    }

    pub fn admit<C>(
        &self,
        candidate: C,
        subscriber: Option<&dyn ActEventSubscriber>,
    ) -> Result<AdmissionReceipt, HubAdmissionError<R::Error>>
    where
        C: DiagnosticEventCandidate,
    {
        self.admit_inner(
            candidate,
            subscriber,
            |reserver, size| reserver.try_reserve(size),
        )
    }

    pub fn admit_cue<C>(
        &self,
        candidate: C,
        subscriber: Option<&dyn ActEventSubscriber>,
    ) -> Result<AdmissionReceipt, HubAdmissionError<R::Error>>
    where
        C: DiagnosticEventCandidate,
    {
        self.admit_inner(
            candidate,
            subscriber,
            |reserver, size| reserver.try_reserve_cue(size),
        )
    }

    pub fn admit_fatal<C>(
        &self,
        candidate: C,
        subscriber: Option<&dyn ActEventSubscriber>,
    ) -> Result<AdmissionReceipt, HubAdmissionError<R::Error>>
    where
        C: DiagnosticEventCandidate,
    {
        self.admit_inner(
            candidate,
            subscriber,
            |reserver, size| reserver.try_reserve_fatal(size),
        )
    }

    pub fn begin_cue_capture(&self) -> Result<CueCaptureDecision, HubAdmissionError<R::Error>> {
        let state = self
            .state
            .lock()
            .map_err(|_| HubAdmissionError::StatePoisoned)?;
        state
            .reserver
            .begin_cue_capture()
            .map_err(HubAdmissionError::Reservation)
    }

    pub fn finish_cue_capture(&self) -> Result<Option<u64>, HubAdmissionError<R::Error>> {
        let state = self
            .state
            .lock()
            .map_err(|_| HubAdmissionError::StatePoisoned)?;
        state
            .reserver
            .finish_cue_capture()
            .map_err(HubAdmissionError::Reservation)
    }
}

impl<R> DiagnosticHub<R, SinkOnlyProfile>
where
    R: BoundedInMemoryReserver,
{
    pub fn sink_only(run_id: CanonicalUuid, reserver: R) -> Self {
        Self::new(run_id, reserver, None)
    }

    pub fn admit<C>(
        &self,
        candidate: C,
        subscriber: &dyn ActEventSubscriber,
    ) -> Result<AdmissionReceipt, HubAdmissionError<R::Error>>
    where
        C: DiagnosticEventCandidate,
    {
        self.admit_inner(
            candidate,
            Some(subscriber),
            |reserver, size| reserver.try_reserve(size),
        )
    }
}

impl<R, P> DiagnosticHub<R, P>
where
    R: AdmissionReserver,
{
    fn new(
        run_id: CanonicalUuid,
        reserver: R,
        live_notifier: Option<Box<dyn LiveEventNotifier>>,
    ) -> Self {
        Self {
            run_id,
            state: Mutex::new(HubState {
                run_id,
                next_sequence: Some(1),
                reserver,
                validator: ReferenceValidator::new(),
                live_notifier,
            }),
            subscriber_delivery_order: Mutex::new(()),
            profile: PhantomData,
        }
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    fn admit_inner<C, F>(
        &self,
        candidate: C,
        subscriber: Option<&dyn ActEventSubscriber>,
        reserve: F,
    ) -> Result<AdmissionReceipt, HubAdmissionError<R::Error>>
    where
        C: DiagnosticEventCandidate,
        F: FnOnce(&mut R, AdmissionSize) -> Result<R::Reservation, R::Error>,
    {
        let _subscriber_delivery_order = subscriber
            .map(|_| {
                self.subscriber_delivery_order
                    .lock()
                    .map_err(|_| HubAdmissionError::StatePoisoned)
            })
            .transpose()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| HubAdmissionError::StatePoisoned)?;
        let next = state
            .next_sequence
            .ok_or(HubAdmissionError::SequenceExhausted)?;
        let identity = EventIdentity::new(state.run_id, SchemaU64::new(next));

        let event = candidate.materialize(identity);
        let actual = EventIdentity::new(event.header().run_id(), event.header().sequence());
        if actual != identity {
            return Err(HubAdmissionError::CandidateIdentityMismatch {
                expected: identity,
                actual,
            });
        }
        let canonical_bytes =
            serde_json::to_vec(&event).map_err(|_| HubAdmissionError::CanonicalEncoding)?;
        let size = AdmissionSize::one_event(canonical_bytes.len());
        let reservation = reserve(&mut state.reserver, size)
            .map_err(HubAdmissionError::Reservation)?;

        let validated = state
            .validator
            .validate(&event)
            .map_err(HubAdmissionError::Reference)?;
        let built_in_span_kind = validated.built_in_span_kind();
        let accepted =
            AcceptedDiagnosticEvent::new(event, canonical_bytes, built_in_span_kind, false);
        state.next_sequence = next.checked_add(1);
        reservation.commit(accepted.clone());

        let live_delivery = match state.live_notifier.as_mut() {
            Some(notifier) => match notifier.notify(accepted.clone()) {
                Ok(()) => DeliveryOutcome::Delivered,
                Err(error) => DeliveryOutcome::Failed(error),
            },
            None => DeliveryOutcome::NotConfigured,
        };
        drop(state);
        let subscriber_delivery = match subscriber {
            Some(subscriber) => match subscriber.deliver(accepted.clone()) {
                Ok(()) => DeliveryOutcome::Delivered,
                Err(error) => DeliveryOutcome::Failed(error),
            },
            None => DeliveryOutcome::NotConfigured,
        };

        Ok(AdmissionReceipt {
            accepted,
            live_delivery,
            subscriber_delivery,
        })
    }
}
