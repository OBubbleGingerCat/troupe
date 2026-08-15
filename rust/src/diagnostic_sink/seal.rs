use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use troupe_diagnostics_core::event::DiagnosticEventKind;
use troupe_diagnostics_core::id::{CanonicalUuid, RunLocalId};

use super::dispatcher::{DispatchEvent, SinkDispatcher};
use super::queue::{AdmissionClass, AdmissionOutcome, QueueAccessError, SinkTerminalReason};
use super::summary::{ActOutcome, SinkCloseReason, SinkDeliverySummary};

const OPEN: u8 = 0;
const SEALING: u8 = 1;
const SEALED: u8 = 2;
const CLOSING: u8 = 3;
const CLOSED: u8 = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SinkSealCause {
    ActFinished,
    RuntimeShutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SinkSealFacts {
    cause: SinkSealCause,
    act_outcome: Option<ActOutcome>,
}

impl SinkSealFacts {
    pub(crate) const fn act_finished(act_outcome: ActOutcome) -> Self {
        Self {
            cause: SinkSealCause::ActFinished,
            act_outcome: Some(act_outcome),
        }
    }

    pub(crate) const fn runtime_shutdown(act_outcome: Option<ActOutcome>) -> Self {
        Self {
            cause: SinkSealCause::RuntimeShutdown,
            act_outcome,
        }
    }

    const fn nominal_close_reason(self) -> SinkCloseReason {
        match self.cause {
            SinkSealCause::ActFinished => SinkCloseReason::ActFinished,
            SinkSealCause::RuntimeShutdown => SinkCloseReason::RuntimeShutdown,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SinkSealError {
    TerminalNotAccounted,
    AlreadySealed,
}

impl fmt::Display for SinkSealError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TerminalNotAccounted => {
                formatter.write_str("diagnostic sink terminal event is not accounted")
            }
            Self::AlreadySealed => formatter.write_str("diagnostic sink is already sealed"),
        }
    }
}

impl std::error::Error for SinkSealError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SinkEnqueueRejection {
    Sealed,
    Closed,
}

impl fmt::Display for SinkEnqueueRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sealed => formatter.write_str("diagnostic sink is sealed"),
            Self::Closed => formatter.write_str("diagnostic sink is closed"),
        }
    }
}

impl std::error::Error for SinkEnqueueRejection {}

#[derive(Clone, Debug)]
pub(crate) struct SinkHandle {
    inner: Arc<SinkLifecycle>,
}

impl SinkHandle {
    pub(crate) fn new(
        run_id: CanonicalUuid,
        act_id: RunLocalId,
        dispatcher: SinkDispatcher,
    ) -> Self {
        Self {
            inner: Arc::new(SinkLifecycle {
                run_id,
                act_id,
                dispatcher,
                phase: AtomicU8::new(OPEN),
                active_enqueues: AtomicUsize::new(0),
                terminal_accounted: AtomicBool::new(false),
                seal_facts: OnceLock::new(),
                source_gaps: AtomicUsize::new(0),
                truncated_payloads: AtomicUsize::new(0),
                summary: OnceLock::new(),
            }),
        }
    }

    pub(crate) fn id(&self) -> u64 {
        self.inner.dispatcher.id()
    }

    pub(crate) fn try_enqueue(
        &self,
        event: DispatchEvent,
        event_kind: DiagnosticEventKind,
        encoded_bytes: usize,
        class: AdmissionClass,
    ) -> Result<AdmissionOutcome, SinkEnqueueRejection> {
        let _admission = self.inner.begin_enqueue()?;
        Ok(self
            .inner
            .dispatcher
            .try_enqueue(event, event_kind, encoded_bytes, class))
    }

    pub(crate) fn try_enqueue_terminal(
        &self,
        event: DispatchEvent,
        event_kind: DiagnosticEventKind,
        encoded_bytes: usize,
    ) -> Result<AdmissionOutcome, SinkEnqueueRejection> {
        let admission = self.inner.begin_enqueue()?;
        if self
            .inner
            .terminal_accounted
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(SinkEnqueueRejection::Sealed);
        }
        let outcome = self.inner.dispatcher.try_enqueue(
            event,
            event_kind,
            encoded_bytes,
            AdmissionClass::Structural,
        );
        drop(admission);
        Ok(outcome)
    }

    pub(crate) fn record_source_gaps(&self, count: usize) -> Result<(), SinkEnqueueRejection> {
        let _admission = self.inner.begin_enqueue()?;
        saturating_atomic_add(&self.inner.source_gaps, count);
        Ok(())
    }

    pub(crate) fn record_truncated_payloads(
        &self,
        count: usize,
    ) -> Result<(), SinkEnqueueRejection> {
        let _admission = self.inner.begin_enqueue()?;
        saturating_atomic_add(&self.inner.truncated_payloads, count);
        Ok(())
    }

    pub(crate) fn seal(&self, facts: SinkSealFacts) -> Result<(), SinkSealError> {
        if facts.cause == SinkSealCause::ActFinished
            && !self.inner.terminal_accounted.load(Ordering::Acquire)
        {
            return Err(SinkSealError::TerminalNotAccounted);
        }
        if self
            .inner
            .phase
            .compare_exchange(OPEN, SEALING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(SinkSealError::AlreadySealed);
        }
        while self.inner.active_enqueues.load(Ordering::Acquire) != 0 {
            std::thread::yield_now();
        }
        self.inner
            .seal_facts
            .set(facts)
            .expect("first diagnostic sink seal must latch facts");
        self.inner.phase.store(SEALED, Ordering::Release);
        Ok(())
    }

    pub(crate) fn is_open(&self) -> bool {
        self.inner.phase.load(Ordering::Acquire) == OPEN
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.inner.phase.load(Ordering::Acquire) == CLOSED
    }

    pub(crate) fn summary(&self) -> Option<Arc<SinkDeliverySummary>> {
        self.inner.summary.get().cloned()
    }

    pub(crate) fn try_close_drained(&self) -> Result<SinkClosePoll, SinkCloseError> {
        match self.inner.phase.load(Ordering::Acquire) {
            CLOSED => {
                return Ok(SinkClosePoll::Closed(
                    self.summary()
                        .expect("closed diagnostic sink must retain its summary"),
                ));
            }
            OPEN => return Err(SinkCloseError::NotSealed),
            SEALING | CLOSING => return Ok(SinkClosePoll::Pending),
            SEALED => {}
            phase => panic!("invalid diagnostic sink phase {phase}"),
        }
        let queue = self
            .inner
            .dispatcher
            .try_queue_snapshot()
            .map_err(SinkCloseError::Queue)?;
        if queue.queued_events() != 0 || queue.callback_active() {
            return Ok(SinkClosePoll::Pending);
        }
        let nominal = match queue.terminal_reason() {
            Some(SinkTerminalReason::DeliveryOverflow) => SinkCloseReason::DeliveryOverflow,
            None => self.inner.seal_facts().nominal_close_reason(),
        };
        Ok(SinkClosePoll::Closed(
            self.inner.latch_summary(nominal, false),
        ))
    }

    pub(crate) fn try_discard_pending(&self) -> Result<(), QueueAccessError> {
        self.inner.dispatcher.try_discard_queued().map(|_| ())
    }

    pub(crate) fn callback_is_active(&self) -> Result<bool, QueueAccessError> {
        self.inner
            .dispatcher
            .try_queue_snapshot()
            .map(|snapshot| snapshot.callback_active())
    }

    pub(crate) fn close_for_runtime_shutdown(
        &self,
        callback_abandoned: bool,
    ) -> Arc<SinkDeliverySummary> {
        if let Some(summary) = self.summary() {
            return summary;
        }
        let terminal_overflow = self
            .inner
            .dispatcher
            .try_queue_snapshot()
            .ok()
            .and_then(|snapshot| snapshot.terminal_reason())
            == Some(SinkTerminalReason::DeliveryOverflow);
        let nominal = if terminal_overflow && !callback_abandoned {
            SinkCloseReason::DeliveryOverflow
        } else {
            SinkCloseReason::RuntimeShutdown
        };
        self.inner.latch_summary(nominal, callback_abandoned)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SinkClosePoll {
    Pending,
    Closed(Arc<SinkDeliverySummary>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SinkCloseError {
    NotSealed,
    Queue(QueueAccessError),
}

impl fmt::Display for SinkCloseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSealed => formatter.write_str("diagnostic sink is not sealed"),
            Self::Queue(error) => write!(formatter, "inspect diagnostic sink queue: {error}"),
        }
    }
}

impl std::error::Error for SinkCloseError {}

#[derive(Debug)]
struct SinkLifecycle {
    run_id: CanonicalUuid,
    act_id: RunLocalId,
    dispatcher: SinkDispatcher,
    phase: AtomicU8,
    active_enqueues: AtomicUsize,
    terminal_accounted: AtomicBool,
    seal_facts: OnceLock<SinkSealFacts>,
    source_gaps: AtomicUsize,
    truncated_payloads: AtomicUsize,
    summary: OnceLock<Arc<SinkDeliverySummary>>,
}

impl SinkLifecycle {
    fn begin_enqueue(&self) -> Result<ActiveEnqueue<'_>, SinkEnqueueRejection> {
        let phase = self.phase.load(Ordering::Acquire);
        if phase != OPEN {
            return Err(enqueue_rejection(phase));
        }
        if self.terminal_accounted.load(Ordering::Acquire) {
            return Err(SinkEnqueueRejection::Sealed);
        }
        self.active_enqueues.fetch_add(1, Ordering::AcqRel);
        let phase = self.phase.load(Ordering::Acquire);
        if phase != OPEN {
            self.active_enqueues.fetch_sub(1, Ordering::AcqRel);
            return Err(enqueue_rejection(phase));
        }
        if self.terminal_accounted.load(Ordering::Acquire) {
            self.active_enqueues.fetch_sub(1, Ordering::AcqRel);
            return Err(SinkEnqueueRejection::Sealed);
        }
        Ok(ActiveEnqueue {
            active: &self.active_enqueues,
        })
    }

    fn seal_facts(&self) -> SinkSealFacts {
        *self
            .seal_facts
            .get()
            .expect("sealed diagnostic sink must retain seal facts")
    }

    fn latch_summary(
        &self,
        nominal_close_reason: SinkCloseReason,
        callback_abandoned: bool,
    ) -> Arc<SinkDeliverySummary> {
        loop {
            match self.phase.load(Ordering::Acquire) {
                CLOSED => {
                    return self
                        .summary
                        .get()
                        .cloned()
                        .expect("closed diagnostic sink must retain its summary");
                }
                CLOSING => std::thread::yield_now(),
                SEALED => {
                    if self
                        .phase
                        .compare_exchange(SEALED, CLOSING, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        break;
                    }
                }
                phase => panic!("cannot close diagnostic sink from phase {phase}"),
            }
        }

        let seal = self.seal_facts();
        let summary = Arc::new(SinkDeliverySummary::from_delivery_facts(
            self.run_id,
            self.act_id.clone(),
            seal.act_outcome,
            nominal_close_reason,
            self.dispatcher.delivery_progress(),
            self.dispatcher.drop_snapshot(),
            self.source_gaps.load(Ordering::Acquire),
            self.truncated_payloads.load(Ordering::Acquire),
            self.dispatcher.callback_failure(),
            callback_abandoned,
            self.dispatcher.unexpected_failure().is_some(),
        ));
        self.summary
            .set(Arc::clone(&summary))
            .expect("first diagnostic sink close must latch summary");
        self.phase.store(CLOSED, Ordering::Release);
        summary
    }
}

struct ActiveEnqueue<'a> {
    active: &'a AtomicUsize,
}

impl Drop for ActiveEnqueue<'_> {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

fn enqueue_rejection(phase: u8) -> SinkEnqueueRejection {
    match phase {
        OPEN => unreachable!("open diagnostic sink accepts enqueue"),
        SEALING | SEALED => SinkEnqueueRejection::Sealed,
        CLOSING | CLOSED => SinkEnqueueRejection::Closed,
        phase => panic!("invalid diagnostic sink phase {phase}"),
    }
}

fn saturating_atomic_add(target: &AtomicUsize, delta: usize) {
    let _ = target.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        Some(current.saturating_add(delta))
    });
}
