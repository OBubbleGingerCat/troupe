use std::{
    collections::VecDeque,
    fmt,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
};

use troupe_diagnostics_core::hub::{
    AcceptedDiagnosticEvent, AdmissionReservation, AdmissionReserver, AdmissionSize,
    CueCaptureDecision, MandatoryDurableReserver,
};

use super::watermark::{CommitNotification, CommitObserver};

pub const MAX_UNCOMMITTED_EVENTS: usize = 32_768;
pub const MAX_UNCOMMITTED_CANONICAL_BYTES: usize = 64 * 1024 * 1024;
pub const CUE_CAPTURE_PAUSE_EVENTS: usize = MAX_UNCOMMITTED_EVENTS * 3 / 4;
pub const CUE_CAPTURE_PAUSE_CANONICAL_BYTES: usize = MAX_UNCOMMITTED_CANONICAL_BYTES * 3 / 4;
pub const CUE_CAPTURE_RESUME_EVENTS: usize = MAX_UNCOMMITTED_EVENTS / 4;
pub const CUE_CAPTURE_RESUME_CANONICAL_BYTES: usize = MAX_UNCOMMITTED_CANONICAL_BYTES / 4;

#[derive(Clone)]
pub struct MandatoryIngress {
    shared: Arc<SharedIngress>,
}

struct SharedIngress {
    state: Mutex<IngressState>,
    failure_sender: SyncSender<IngressCoreFailure>,
}

#[derive(Default)]
struct IngressState {
    reserved_events: usize,
    reserved_canonical_bytes: usize,
    outstanding: VecDeque<TrackedEvent>,
    outstanding_canonical_bytes: usize,
    committed_sequence: u64,
    run_id: Option<troupe_diagnostics_core::id::CanonicalUuid>,
    normal_ingress_sealed: bool,
    cue_capture_paused: bool,
    suppressed_cues: u64,
    fatal_admission_authorized: bool,
    failure: Option<IngressCoreFailure>,
    fatal_state: FatalAdmissionState,
}

struct TrackedEvent {
    event: AcceptedDiagnosticEvent,
    canonical_bytes: usize,
    phase: DeliveryPhase,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DeliveryPhase {
    Queued,
    InFlight,
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum FatalAdmissionState {
    #[default]
    Available,
    Reserved,
    Admitted,
}

impl MandatoryIngress {
    pub fn new() -> (Self, IngressFailureReceiver) {
        let (failure_sender, receiver) = mpsc::sync_channel(1);
        (
            Self {
                shared: Arc::new(SharedIngress {
                    state: Mutex::new(IngressState::default()),
                    failure_sender,
                }),
            },
            IngressFailureReceiver { receiver },
        )
    }

    pub fn status(&self) -> Result<IngressStatus, IngressStateError> {
        let state = self
            .shared
            .state
            .lock()
            .map_err(|_| IngressStateError::StatePoisoned)?;
        Ok(IngressStatus::from_state(&state))
    }

    pub fn try_dequeue(&self) -> Result<Option<AcceptedDiagnosticEvent>, IngressStateError> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| IngressStateError::StatePoisoned)?;
        let Some(tracked) = state
            .outstanding
            .iter_mut()
            .find(|tracked| tracked.phase == DeliveryPhase::Queued)
        else {
            return Ok(None);
        };
        tracked.phase = DeliveryPhase::InFlight;
        Ok(Some(tracked.event.clone()))
    }

    pub fn begin_cue_capture(&self) -> Result<CueCaptureDecision, IngressStateError> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| IngressStateError::StatePoisoned)?;
        if state.normal_ingress_sealed {
            return Ok(CueCaptureDecision::Capture);
        }

        let current_events = state
            .reserved_events
            .saturating_add(state.outstanding.len());
        let current_bytes = state
            .reserved_canonical_bytes
            .saturating_add(state.outstanding_canonical_bytes);
        if state.cue_capture_paused {
            if current_events <= CUE_CAPTURE_RESUME_EVENTS
                && current_bytes <= CUE_CAPTURE_RESUME_CANONICAL_BYTES
            {
                state.cue_capture_paused = false;
                let suppressed_cues = std::mem::take(&mut state.suppressed_cues);
                return Ok(if suppressed_cues == 0 {
                    CueCaptureDecision::Capture
                } else {
                    CueCaptureDecision::Resume { suppressed_cues }
                });
            }
            state.suppressed_cues = state.suppressed_cues.saturating_add(1);
            return Ok(CueCaptureDecision::Suppress);
        }

        if current_events >= CUE_CAPTURE_PAUSE_EVENTS
            || current_bytes >= CUE_CAPTURE_PAUSE_CANONICAL_BYTES
        {
            state.cue_capture_paused = true;
            state.suppressed_cues = 1;
            return Ok(CueCaptureDecision::Suppress);
        }
        Ok(CueCaptureDecision::Capture)
    }

    pub fn finish_cue_capture(&self) -> Result<Option<u64>, IngressStateError> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| IngressStateError::StatePoisoned)?;
        state.cue_capture_paused = false;
        let suppressed_cues = std::mem::take(&mut state.suppressed_cues);
        Ok((suppressed_cues > 0).then_some(suppressed_cues))
    }

    pub fn retry_in_flight(&self) -> Result<Vec<AcceptedDiagnosticEvent>, IngressStateError> {
        let state = self
            .shared
            .state
            .lock()
            .map_err(|_| IngressStateError::StatePoisoned)?;
        Ok(state
            .outstanding
            .iter()
            .filter(|tracked| tracked.phase == DeliveryPhase::InFlight)
            .map(|tracked| tracked.event.clone())
            .collect())
    }

    pub fn seal_normal_ingress(&self) -> Result<bool, IngressStateError> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| IngressStateError::StatePoisoned)?;
        let changed = !state.normal_ingress_sealed;
        state.normal_ingress_sealed = true;
        Ok(changed)
    }

    pub fn seal_for_external_core_failure(&self) -> Result<bool, IngressStateError> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| IngressStateError::StatePoisoned)?;
        let changed = !state.normal_ingress_sealed || !state.fatal_admission_authorized;
        state.normal_ingress_sealed = true;
        state.fatal_admission_authorized = true;
        Ok(changed)
    }

    pub fn mark_committed(
        &self,
        notification: CommitNotification,
    ) -> Result<(), CommitAccountingError> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| CommitAccountingError::StatePoisoned)?;

        if let Err(error) = validate_commit(&state, notification) {
            latch_failure(
                &self.shared,
                &mut state,
                IngressCoreFailure::commit_accounting_failed(),
            );
            return Err(error);
        }

        let mut released_bytes = 0_usize;
        for _ in 0..notification.event_count() {
            let tracked = state
                .outstanding
                .pop_front()
                .expect("commit validation proved the complete prefix exists");
            released_bytes += tracked.canonical_bytes;
        }
        state.outstanding_canonical_bytes -= released_bytes;
        state.committed_sequence = notification.committed().get();
        Ok(())
    }

    fn reserve(
        &self,
        size: AdmissionSize,
        class: ReservationClass,
    ) -> Result<IngressReservation, IngressAdmissionError> {
        if size.event_count() != 1 {
            return Err(IngressAdmissionError::InvalidEventCount {
                actual: size.event_count(),
            });
        }

        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| IngressAdmissionError::StatePoisoned)?;
        match class {
            ReservationClass::Normal | ReservationClass::Cue => {
                if state.normal_ingress_sealed {
                    return Err(IngressAdmissionError::Sealed(state.failure));
                }
            }
            ReservationClass::Fatal => {
                if !state.fatal_admission_authorized {
                    return Err(IngressAdmissionError::FatalBeforeCoreFailure);
                }
                match state.fatal_state {
                    FatalAdmissionState::Available => {}
                    FatalAdmissionState::Reserved => {
                        return Err(IngressAdmissionError::FatalAlreadyReserved);
                    }
                    FatalAdmissionState::Admitted => {
                        return Err(IngressAdmissionError::FatalAlreadyAdmitted);
                    }
                }
            }
        }

        let current_events = state
            .reserved_events
            .saturating_add(state.outstanding.len());
        let current_bytes = state
            .reserved_canonical_bytes
            .saturating_add(state.outstanding_canonical_bytes);
        let candidate_events = current_events.checked_add(size.event_count());
        let candidate_bytes = current_bytes.checked_add(size.canonical_bytes());
        let event_limit_exceeded =
            candidate_events.is_none_or(|value| value > MAX_UNCOMMITTED_EVENTS);
        let byte_limit_exceeded =
            candidate_bytes.is_none_or(|value| value > MAX_UNCOMMITTED_CANONICAL_BYTES);

        if event_limit_exceeded || byte_limit_exceeded {
            if class == ReservationClass::Fatal {
                return Err(IngressAdmissionError::FatalCapacityUnavailable);
            }
            let failure = IngressCoreFailure::budget_exhausted(
                current_events,
                current_bytes,
                size,
                event_limit_exceeded,
                byte_limit_exceeded,
            );
            latch_failure(&self.shared, &mut state, failure);
            return Err(IngressAdmissionError::BudgetExhausted(failure));
        }

        if class == ReservationClass::Cue
            && (candidate_events.is_some_and(|value| value >= CUE_CAPTURE_PAUSE_EVENTS)
                || candidate_bytes.is_some_and(|value| value >= CUE_CAPTURE_PAUSE_CANONICAL_BYTES))
        {
            state.cue_capture_paused = true;
        }

        state.reserved_events = state
            .reserved_events
            .checked_add(size.event_count())
            .expect("the total budget check also bounds reserved events");
        state.reserved_canonical_bytes = state
            .reserved_canonical_bytes
            .checked_add(size.canonical_bytes())
            .expect("the total budget check also bounds reserved bytes");
        if class == ReservationClass::Fatal {
            state.fatal_state = FatalAdmissionState::Reserved;
        }
        drop(state);

        Ok(IngressReservation {
            shared: Arc::clone(&self.shared),
            size,
            class,
            committed: false,
        })
    }
}

impl AdmissionReserver for MandatoryIngress {
    type Error = IngressAdmissionError;
    type Reservation = IngressReservation;

    fn try_reserve(&mut self, size: AdmissionSize) -> Result<Self::Reservation, Self::Error> {
        self.reserve(size, ReservationClass::Normal)
    }

    fn try_reserve_fatal(&mut self, size: AdmissionSize) -> Result<Self::Reservation, Self::Error> {
        self.reserve(size, ReservationClass::Fatal)
    }
}

impl MandatoryDurableReserver for MandatoryIngress {
    fn try_reserve_cue(&mut self, size: AdmissionSize) -> Result<Self::Reservation, Self::Error> {
        self.reserve(size, ReservationClass::Cue)
    }

    fn begin_cue_capture(&self) -> Result<CueCaptureDecision, Self::Error> {
        MandatoryIngress::begin_cue_capture(self).map_err(|_| IngressAdmissionError::StatePoisoned)
    }

    fn finish_cue_capture(&self) -> Result<Option<u64>, Self::Error> {
        MandatoryIngress::finish_cue_capture(self).map_err(|_| IngressAdmissionError::StatePoisoned)
    }
}

impl CommitObserver for MandatoryIngress {
    fn committed(&mut self, notification: CommitNotification) {
        let _ = self.mark_committed(notification);
    }
}

pub struct IngressReservation {
    shared: Arc<SharedIngress>,
    size: AdmissionSize,
    class: ReservationClass,
    committed: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ReservationClass {
    Normal,
    Cue,
    Fatal,
}

impl AdmissionReservation for IngressReservation {
    fn commit(mut self, event: AcceptedDiagnosticEvent) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        release_reserved(&mut state, self.size);

        let bytes_match = event.canonical_bytes().len() == self.size.canonical_bytes();
        let identity = event.identity();
        let run_matches = state
            .run_id
            .is_none_or(|run_id| run_id == identity.run_id());
        let expected_sequence = state
            .outstanding
            .back()
            .map_or(state.committed_sequence + 1, |tracked| {
                tracked.event.identity().sequence().get().saturating_add(1)
            });
        if !bytes_match || !run_matches || identity.sequence().get() != expected_sequence {
            latch_failure(
                &self.shared,
                &mut state,
                IngressCoreFailure::commit_accounting_failed(),
            );
        }
        state.run_id.get_or_insert(identity.run_id());
        state.outstanding_canonical_bytes = state
            .outstanding_canonical_bytes
            .saturating_add(self.size.canonical_bytes());
        state.outstanding.push_back(TrackedEvent {
            event,
            canonical_bytes: self.size.canonical_bytes(),
            phase: DeliveryPhase::Queued,
        });
        if self.class == ReservationClass::Fatal {
            state.fatal_state = FatalAdmissionState::Admitted;
        }
        self.committed = true;
    }
}

impl Drop for IngressReservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        release_reserved(&mut state, self.size);
        if self.class == ReservationClass::Fatal
            && state.fatal_state == FatalAdmissionState::Reserved
        {
            state.fatal_state = FatalAdmissionState::Available;
        }
    }
}

fn release_reserved(state: &mut IngressState, size: AdmissionSize) {
    state.reserved_events = state.reserved_events.saturating_sub(size.event_count());
    state.reserved_canonical_bytes = state
        .reserved_canonical_bytes
        .saturating_sub(size.canonical_bytes());
}

fn latch_failure(shared: &SharedIngress, state: &mut IngressState, failure: IngressCoreFailure) {
    state.normal_ingress_sealed = true;
    state.fatal_admission_authorized = true;
    if state.failure.is_some() {
        return;
    }
    state.failure = Some(failure);
    match shared.failure_sender.try_send(failure) {
        Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
    }
}

fn validate_commit(
    state: &IngressState,
    notification: CommitNotification,
) -> Result<(), CommitAccountingError> {
    if notification.previous().get() != state.committed_sequence {
        return Err(CommitAccountingError::StalePrevious {
            expected: state.committed_sequence,
            actual: notification.previous().get(),
        });
    }
    if state
        .run_id
        .is_some_and(|run_id| run_id != notification.run_id())
    {
        return Err(CommitAccountingError::RunIdentityMismatch);
    }
    if state.outstanding.len() < notification.event_count() {
        return Err(CommitAccountingError::MissingPrefix {
            expected: notification.event_count(),
            actual: state.outstanding.len(),
        });
    }

    let mut canonical_bytes = 0_usize;
    for (offset, tracked) in state
        .outstanding
        .iter()
        .take(notification.event_count())
        .enumerate()
    {
        if tracked.phase != DeliveryPhase::InFlight {
            return Err(CommitAccountingError::PrefixNotInFlight);
        }
        let expected = state
            .committed_sequence
            .checked_add(offset as u64 + 1)
            .ok_or(CommitAccountingError::SequenceExhausted)?;
        if tracked.event.identity().sequence().get() != expected {
            return Err(CommitAccountingError::NonDensePrefix {
                expected,
                actual: tracked.event.identity().sequence().get(),
            });
        }
        canonical_bytes = canonical_bytes
            .checked_add(tracked.canonical_bytes)
            .ok_or(CommitAccountingError::CanonicalByteCountOverflow)?;
    }

    let expected_committed = state
        .committed_sequence
        .checked_add(notification.event_count() as u64)
        .ok_or(CommitAccountingError::SequenceExhausted)?;
    if notification.committed().get() != expected_committed {
        return Err(CommitAccountingError::CommittedSequenceMismatch {
            expected: expected_committed,
            actual: notification.committed().get(),
        });
    }
    if notification.canonical_bytes() != canonical_bytes {
        return Err(CommitAccountingError::CanonicalByteCountMismatch {
            expected: canonical_bytes,
            actual: notification.canonical_bytes(),
        });
    }
    Ok(())
}

pub struct IngressFailureReceiver {
    receiver: Receiver<IngressCoreFailure>,
}

impl IngressFailureReceiver {
    pub fn try_recv(&self) -> Option<IngressCoreFailure> {
        self.receiver.try_recv().ok()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IngressStatus {
    accepted_uncommitted_events: usize,
    accepted_uncommitted_canonical_bytes: usize,
    queued_events: usize,
    in_flight_events: usize,
    committed_sequence: u64,
    normal_ingress_sealed: bool,
    cue_capture_paused: bool,
    failure: Option<IngressCoreFailure>,
}

impl IngressStatus {
    fn from_state(state: &IngressState) -> Self {
        let queued_events = state
            .outstanding
            .iter()
            .filter(|tracked| tracked.phase == DeliveryPhase::Queued)
            .count();
        let in_flight_events = state.outstanding.len() - queued_events;
        Self {
            accepted_uncommitted_events: state.reserved_events + state.outstanding.len(),
            accepted_uncommitted_canonical_bytes: state.reserved_canonical_bytes
                + state.outstanding_canonical_bytes,
            queued_events,
            in_flight_events,
            committed_sequence: state.committed_sequence,
            normal_ingress_sealed: state.normal_ingress_sealed,
            cue_capture_paused: state.cue_capture_paused,
            failure: state.failure,
        }
    }

    pub const fn max_uncommitted_events(&self) -> usize {
        MAX_UNCOMMITTED_EVENTS
    }

    pub const fn max_uncommitted_canonical_bytes(&self) -> usize {
        MAX_UNCOMMITTED_CANONICAL_BYTES
    }

    pub const fn accepted_uncommitted_events(&self) -> usize {
        self.accepted_uncommitted_events
    }

    pub const fn accepted_uncommitted_canonical_bytes(&self) -> usize {
        self.accepted_uncommitted_canonical_bytes
    }

    pub const fn queued_events(&self) -> usize {
        self.queued_events
    }

    pub const fn in_flight_events(&self) -> usize {
        self.in_flight_events
    }

    pub const fn committed_sequence(&self) -> u64 {
        self.committed_sequence
    }

    pub const fn normal_ingress_sealed(&self) -> bool {
        self.normal_ingress_sealed
    }

    pub const fn cue_capture_paused(&self) -> bool {
        self.cue_capture_paused
    }

    pub const fn failure(&self) -> Option<IngressCoreFailure> {
        self.failure
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IngressCoreFailure {
    kind: IngressCoreFailureKind,
    current_events: usize,
    current_canonical_bytes: usize,
    attempted_events: usize,
    attempted_canonical_bytes: usize,
    event_limit_exceeded: bool,
    byte_limit_exceeded: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IngressCoreFailureKind {
    BudgetExhausted,
    CommitAccounting,
}

impl IngressCoreFailure {
    fn budget_exhausted(
        current_events: usize,
        current_canonical_bytes: usize,
        attempted: AdmissionSize,
        event_limit_exceeded: bool,
        byte_limit_exceeded: bool,
    ) -> Self {
        Self {
            kind: IngressCoreFailureKind::BudgetExhausted,
            current_events,
            current_canonical_bytes,
            attempted_events: attempted.event_count(),
            attempted_canonical_bytes: attempted.canonical_bytes(),
            event_limit_exceeded,
            byte_limit_exceeded,
        }
    }

    const fn commit_accounting_failed() -> Self {
        Self {
            kind: IngressCoreFailureKind::CommitAccounting,
            current_events: 0,
            current_canonical_bytes: 0,
            attempted_events: 0,
            attempted_canonical_bytes: 0,
            event_limit_exceeded: false,
            byte_limit_exceeded: false,
        }
    }

    pub const fn code(self) -> &'static str {
        match self.kind {
            IngressCoreFailureKind::BudgetExhausted => "mandatory_ingress_budget_exhausted",
            IngressCoreFailureKind::CommitAccounting => "mandatory_ingress_commit_accounting",
        }
    }

    pub const fn current_events(self) -> usize {
        self.current_events
    }

    pub const fn current_canonical_bytes(self) -> usize {
        self.current_canonical_bytes
    }

    pub const fn attempted_events(self) -> usize {
        self.attempted_events
    }

    pub const fn attempted_canonical_bytes(self) -> usize {
        self.attempted_canonical_bytes
    }

    pub const fn event_limit_exceeded(self) -> bool {
        self.event_limit_exceeded
    }

    pub const fn byte_limit_exceeded(self) -> bool {
        self.byte_limit_exceeded
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IngressAdmissionError {
    StatePoisoned,
    InvalidEventCount { actual: usize },
    BudgetExhausted(IngressCoreFailure),
    Sealed(Option<IngressCoreFailure>),
    FatalBeforeCoreFailure,
    FatalAlreadyReserved,
    FatalAlreadyAdmitted,
    FatalCapacityUnavailable,
}

impl fmt::Display for IngressAdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StatePoisoned => formatter.write_str("mandatory ingress state is poisoned"),
            Self::InvalidEventCount { actual } => {
                write!(
                    formatter,
                    "mandatory ingress reservation contains {actual} events"
                )
            }
            Self::BudgetExhausted(_) => {
                formatter.write_str("mandatory ingress budget is exhausted")
            }
            Self::Sealed(_) => formatter.write_str("mandatory ingress is sealed"),
            Self::FatalBeforeCoreFailure => {
                formatter.write_str("fatal admission requires a latched core failure")
            }
            Self::FatalAlreadyReserved => {
                formatter.write_str("fatal admission is already reserved")
            }
            Self::FatalAlreadyAdmitted => formatter.write_str("fatal fact was already admitted"),
            Self::FatalCapacityUnavailable => {
                formatter.write_str("fatal fact does not fit the current mandatory budget")
            }
        }
    }
}

impl std::error::Error for IngressAdmissionError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IngressStateError {
    StatePoisoned,
}

impl fmt::Display for IngressStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("mandatory ingress state is poisoned")
    }
}

impl std::error::Error for IngressStateError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitAccountingError {
    StatePoisoned,
    RunIdentityMismatch,
    StalePrevious { expected: u64, actual: u64 },
    MissingPrefix { expected: usize, actual: usize },
    PrefixNotInFlight,
    NonDensePrefix { expected: u64, actual: u64 },
    CommittedSequenceMismatch { expected: u64, actual: u64 },
    CanonicalByteCountMismatch { expected: usize, actual: usize },
    CanonicalByteCountOverflow,
    SequenceExhausted,
}

impl fmt::Display for CommitAccountingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StatePoisoned => formatter.write_str("mandatory ingress state is poisoned"),
            Self::RunIdentityMismatch => {
                formatter.write_str("commit Run identity differs from mandatory ingress")
            }
            Self::StalePrevious { expected, actual } => write!(
                formatter,
                "commit previous watermark is {actual}, expected {expected}"
            ),
            Self::MissingPrefix { expected, actual } => write!(
                formatter,
                "commit contains {expected} events but ingress has {actual}"
            ),
            Self::PrefixNotInFlight => formatter.write_str("commit prefix contains a queued event"),
            Self::NonDensePrefix { expected, actual } => write!(
                formatter,
                "commit prefix expected sequence {expected}, found {actual}"
            ),
            Self::CommittedSequenceMismatch { expected, actual } => write!(
                formatter,
                "commit watermark is {actual}, expected {expected}"
            ),
            Self::CanonicalByteCountMismatch { expected, actual } => write!(
                formatter,
                "commit canonical byte count is {actual}, expected {expected}"
            ),
            Self::CanonicalByteCountOverflow => {
                formatter.write_str("commit canonical byte count overflowed")
            }
            Self::SequenceExhausted => formatter.write_str("commit sequence is exhausted"),
        }
    }
}

impl std::error::Error for CommitAccountingError {}
