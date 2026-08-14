use std::{
    fmt,
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use crate::{
    event::DiagnosticEvent,
    id::CanonicalUuid,
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

    fn try_reserve(
        &mut self,
        size: AdmissionSize,
    ) -> Result<Self::Reservation, Self::Error>;
}

pub trait MandatoryDurableReserver: AdmissionReserver {}

pub trait BoundedInMemoryReserver: AdmissionReserver {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedDiagnosticEvent(Arc<AcceptedDiagnosticEventInner>);

#[derive(Debug, Eq, PartialEq)]
struct AcceptedDiagnosticEventInner {
    event: DiagnosticEvent,
    canonical_bytes: Box<[u8]>,
}

impl AcceptedDiagnosticEvent {
    fn new(event: DiagnosticEvent, canonical_bytes: Vec<u8>) -> Self {
        Self(Arc::new(AcceptedDiagnosticEventInner {
            event,
            canonical_bytes: canonical_bytes.into_boxed_slice(),
        }))
    }

    pub fn event(&self) -> &DiagnosticEvent {
        &self.0.event
    }

    pub fn canonical_bytes(&self) -> &[u8] {
        &self.0.canonical_bytes
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
    fn notify(
        &mut self,
        event: AcceptedDiagnosticEvent,
    ) -> Result<(), DeliveryFailure>;
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
            Self::SequenceExhausted => formatter.write_str("diagnostic event sequence is exhausted"),
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
    state: Mutex<HubState<R>>,
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
        self.admit_inner(candidate, subscriber)
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
        self.admit_inner(candidate, Some(subscriber))
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
            state: Mutex::new(HubState {
                run_id,
                next_sequence: Some(1),
                reserver,
                validator: ReferenceValidator::new(),
                live_notifier,
            }),
            profile: PhantomData,
        }
    }

    fn admit_inner<C>(
        &self,
        candidate: C,
        subscriber: Option<&dyn ActEventSubscriber>,
    ) -> Result<AdmissionReceipt, HubAdmissionError<R::Error>>
    where
        C: DiagnosticEventCandidate,
    {
        let mut state = self
            .state
            .lock()
            .map_err(|_| HubAdmissionError::StatePoisoned)?;
        let next = state
            .next_sequence
            .ok_or(HubAdmissionError::SequenceExhausted)?;
        let identity = EventIdentity::new(state.run_id, SchemaU64::new(next));

        let event = candidate.materialize(identity);
        let actual = EventIdentity::new(
            event.header().run_id(),
            event.header().sequence(),
        );
        if actual != identity {
            return Err(HubAdmissionError::CandidateIdentityMismatch {
                expected: identity,
                actual,
            });
        }

        let canonical_bytes =
            serde_json::to_vec(&event).map_err(|_| HubAdmissionError::CanonicalEncoding)?;
        let size = AdmissionSize::one_event(canonical_bytes.len());
        let reservation = state
            .reserver
            .try_reserve(size)
            .map_err(HubAdmissionError::Reservation)?;

        state
            .validator
            .validate(&event)
            .map_err(HubAdmissionError::Reference)?;
        let accepted = AcceptedDiagnosticEvent::new(event, canonical_bytes);
        state.next_sequence = next.checked_add(1);
        reservation.commit(accepted.clone());

        let live_delivery = match state.live_notifier.as_mut() {
            Some(notifier) => match notifier.notify(accepted.clone()) {
                Ok(()) => DeliveryOutcome::Delivered,
                Err(error) => DeliveryOutcome::Failed(error),
            },
            None => DeliveryOutcome::NotConfigured,
        };
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
