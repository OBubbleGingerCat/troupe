use std::array;
use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, TryLockError};

use troupe_diagnostics_core::event::DiagnosticEventKind;

use super::budget::{BudgetError, BudgetLimits, BudgetUsage, RuntimeBudget};

pub(crate) const PER_SINK_MAX_EVENTS: usize = 1_024;
pub(crate) const PER_SINK_MAX_ENCODED_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const STRUCTURAL_RESERVE_EVENTS: usize = 32;
pub(crate) const STRUCTURAL_RESERVE_ENCODED_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SinkQueueLimits {
    total: BudgetLimits,
    structural_reserve: BudgetUsage,
    content: BudgetLimits,
}

impl SinkQueueLimits {
    pub(crate) fn new(total: BudgetUsage, structural_reserve: BudgetUsage) -> Self {
        let content = total
            .checked_sub(structural_reserve)
            .expect("structural reserve must fit within the per-sink limits");
        Self {
            total: BudgetLimits::new(total.events(), total.encoded_bytes()),
            structural_reserve,
            content: BudgetLimits::new(content.events(), content.encoded_bytes()),
        }
    }

    pub(crate) fn product() -> Self {
        Self::new(
            BudgetUsage::new(PER_SINK_MAX_EVENTS, PER_SINK_MAX_ENCODED_BYTES),
            BudgetUsage::new(STRUCTURAL_RESERVE_EVENTS, STRUCTURAL_RESERVE_ENCODED_BYTES),
        )
    }

    pub(crate) const fn total(self) -> BudgetLimits {
        self.total
    }

    pub(crate) const fn structural_reserve(self) -> BudgetUsage {
        self.structural_reserve
    }

    pub(crate) const fn content(self) -> BudgetLimits {
        self.content
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AdmissionClass {
    Content,
    Structural,
}

#[derive(Debug)]
pub(crate) struct QueueEvent<T> {
    item: T,
    metadata: EventMetadata,
}

impl<T> QueueEvent<T> {
    pub(crate) fn new(
        item: T,
        event_kind: DiagnosticEventKind,
        encoded_bytes: usize,
        class: AdmissionClass,
    ) -> Self {
        Self {
            item,
            metadata: EventMetadata {
                event_kind,
                encoded_bytes,
                class,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EventMetadata {
    event_kind: DiagnosticEventKind,
    encoded_bytes: usize,
    class: AdmissionClass,
}

impl EventMetadata {
    const fn usage(self) -> BudgetUsage {
        BudgetUsage::new(1, self.encoded_bytes)
    }

    const fn drop_delta(self) -> DropDelta {
        DropDelta::new(self.event_kind, 1, self.encoded_bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DropDelta {
    event_kind: DiagnosticEventKind,
    events: usize,
    encoded_bytes: usize,
}

impl DropDelta {
    pub(crate) const fn new(
        event_kind: DiagnosticEventKind,
        events: usize,
        encoded_bytes: usize,
    ) -> Self {
        Self {
            event_kind,
            events,
            encoded_bytes,
        }
    }

    pub(crate) const fn event_kind(self) -> DiagnosticEventKind {
        self.event_kind
    }

    pub(crate) const fn events(self) -> usize {
        self.events
    }

    pub(crate) const fn encoded_bytes(self) -> usize {
        self.encoded_bytes
    }
}

#[derive(Debug)]
struct DropLedger {
    by_kind: [DropCounter; DiagnosticEventKind::ALL.len()],
}

impl DropLedger {
    fn new() -> Self {
        Self {
            by_kind: array::from_fn(|_| DropCounter::new()),
        }
    }

    fn record(&self, delta: DropDelta) {
        let index = DiagnosticEventKind::ALL
            .iter()
            .position(|kind| *kind == delta.event_kind())
            .expect("closed diagnostic event kind must have a drop counter");
        self.by_kind[index].record(delta);
    }

    fn record_all(&self, deltas: &[DropDelta]) {
        for delta in deltas {
            self.record(*delta);
        }
    }

    fn snapshot(&self) -> Vec<DropDelta> {
        DiagnosticEventKind::ALL
            .iter()
            .copied()
            .zip(self.by_kind.iter())
            .filter_map(|(kind, counter)| counter.snapshot(kind))
            .collect()
    }
}

#[derive(Debug)]
struct DropCounter {
    events: AtomicUsize,
    encoded_bytes: AtomicUsize,
}

impl DropCounter {
    const fn new() -> Self {
        Self {
            events: AtomicUsize::new(0),
            encoded_bytes: AtomicUsize::new(0),
        }
    }

    fn record(&self, delta: DropDelta) {
        saturating_atomic_add(&self.events, delta.events());
        saturating_atomic_add(&self.encoded_bytes, delta.encoded_bytes());
    }

    fn snapshot(&self, kind: DiagnosticEventKind) -> Option<DropDelta> {
        let events = self.events.load(Ordering::Relaxed);
        let encoded_bytes = self.encoded_bytes.load(Ordering::Relaxed);
        (events != 0 || encoded_bytes != 0).then(|| DropDelta::new(kind, events, encoded_bytes))
    }
}

fn saturating_atomic_add(target: &AtomicUsize, delta: usize) {
    let _ = target.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(delta))
    });
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DropReason {
    SinkCapacity,
    RuntimeCapacity,
    QueueContended,
    SinkTerminal,
    BudgetUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AdmissionOutcome {
    Enqueued {
        evicted: Vec<DropDelta>,
    },
    Dropped {
        reason: DropReason,
        delta: DropDelta,
    },
    Terminalized {
        dropped: Vec<DropDelta>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SinkTerminalReason {
    DeliveryOverflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CallbackTicket(u64);

#[derive(Debug)]
pub(crate) struct CallbackDelivery<T> {
    ticket: CallbackTicket,
    item: T,
    metadata: EventMetadata,
}

impl<T> CallbackDelivery<T> {
    pub(crate) const fn ticket(&self) -> CallbackTicket {
        self.ticket
    }

    pub(crate) const fn item(&self) -> &T {
        &self.item
    }

    pub(crate) const fn event_kind(&self) -> DiagnosticEventKind {
        self.metadata.event_kind
    }

    pub(crate) const fn encoded_bytes(&self) -> usize {
        self.metadata.encoded_bytes
    }

    pub(crate) fn into_parts(self) -> (CallbackTicket, T) {
        (self.ticket, self.item)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct QueueSnapshot {
    queued_events: usize,
    callback_active: bool,
    total_usage: BudgetUsage,
    content_usage: BudgetUsage,
    terminal_reason: Option<SinkTerminalReason>,
}

impl QueueSnapshot {
    pub(crate) const fn queued_events(self) -> usize {
        self.queued_events
    }

    pub(crate) const fn callback_active(self) -> bool {
        self.callback_active
    }

    pub(crate) const fn total_usage(self) -> BudgetUsage {
        self.total_usage
    }

    pub(crate) const fn content_usage(self) -> BudgetUsage {
        self.content_usage
    }

    pub(crate) const fn terminal_reason(self) -> Option<SinkTerminalReason> {
        self.terminal_reason
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum QueueAccessError {
    Contended,
    NoCallbackActive,
    CallbackTicketMismatch,
    BudgetInvariant(BudgetError),
}

impl fmt::Display for QueueAccessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Contended => formatter.write_str("diagnostic sink queue is contended"),
            Self::NoCallbackActive => {
                formatter.write_str("diagnostic sink queue has no active callback")
            }
            Self::CallbackTicketMismatch => {
                formatter.write_str("diagnostic sink callback ticket does not match")
            }
            Self::BudgetInvariant(error) => write!(formatter, "diagnostic sink budget: {error}"),
        }
    }
}

impl std::error::Error for QueueAccessError {}

#[derive(Debug)]
struct StoredEvent<T> {
    item: T,
    metadata: EventMetadata,
}

#[derive(Clone, Copy, Debug)]
struct ActiveCallback {
    ticket: CallbackTicket,
    metadata: EventMetadata,
}

#[derive(Debug)]
struct QueueState<T> {
    queued: VecDeque<StoredEvent<T>>,
    active_callback: Option<ActiveCallback>,
    total_usage: BudgetUsage,
    content_usage: BudgetUsage,
    terminal_reason: Option<SinkTerminalReason>,
    next_ticket: u64,
}

impl<T> QueueState<T> {
    fn new(capacity: usize) -> Self {
        Self {
            queued: VecDeque::with_capacity(capacity),
            active_callback: None,
            total_usage: BudgetUsage::ZERO,
            content_usage: BudgetUsage::ZERO,
            terminal_reason: None,
            next_ticket: 1,
        }
    }

    fn issue_ticket(&mut self) -> CallbackTicket {
        let ticket = CallbackTicket(self.next_ticket);
        self.next_ticket = self.next_ticket.wrapping_add(1);
        if self.next_ticket == 0 {
            self.next_ticket = 1;
        }
        ticket
    }
}

#[derive(Clone, Copy, Debug)]
struct Victim {
    index: usize,
    metadata: EventMetadata,
}

#[derive(Debug)]
pub(crate) struct SinkQueue<T> {
    runtime: RuntimeBudget,
    limits: SinkQueueLimits,
    state: Mutex<QueueState<T>>,
    drops: DropLedger,
}

impl<T> SinkQueue<T> {
    pub(crate) fn new(runtime: RuntimeBudget) -> Self {
        Self::with_limits(runtime, SinkQueueLimits::product())
    }

    pub(crate) fn with_limits(runtime: RuntimeBudget, limits: SinkQueueLimits) -> Self {
        Self {
            runtime,
            limits,
            state: Mutex::new(QueueState::new(limits.total().max_events())),
            drops: DropLedger::new(),
        }
    }

    pub(crate) const fn limits(&self) -> SinkQueueLimits {
        self.limits
    }

    pub(crate) fn drop_snapshot(&self) -> Vec<DropDelta> {
        self.drops.snapshot()
    }

    pub(crate) fn try_snapshot(&self) -> Result<QueueSnapshot, QueueAccessError> {
        let state = self.try_state()?;
        Ok(QueueSnapshot {
            queued_events: state.queued.len(),
            callback_active: state.active_callback.is_some(),
            total_usage: state.total_usage,
            content_usage: state.content_usage,
            terminal_reason: state.terminal_reason,
        })
    }

    pub(crate) fn try_admit(&self, event: QueueEvent<T>) -> AdmissionOutcome {
        let incoming_delta = event.metadata.drop_delta();
        let mut state = match self.try_state() {
            Ok(state) => state,
            Err(QueueAccessError::Contended) => {
                self.drops.record(incoming_delta);
                return AdmissionOutcome::Dropped {
                    reason: DropReason::QueueContended,
                    delta: incoming_delta,
                };
            }
            Err(error) => {
                debug_assert!(false, "unexpected queue admission error: {error}");
                self.drops.record(incoming_delta);
                return AdmissionOutcome::Dropped {
                    reason: DropReason::BudgetUnavailable,
                    delta: incoming_delta,
                };
            }
        };

        if state.terminal_reason.is_some() {
            self.drops.record(incoming_delta);
            return AdmissionOutcome::Dropped {
                reason: DropReason::SinkTerminal,
                delta: incoming_delta,
            };
        }

        let incoming_usage = event.metadata.usage();
        let single_event_fits = match event.metadata.class {
            AdmissionClass::Content => incoming_usage.fits_within(self.limits.content()),
            AdmissionClass::Structural => incoming_usage.fits_within(self.limits.total()),
        };
        if !single_event_fits {
            return match event.metadata.class {
                AdmissionClass::Content => {
                    self.drops.record(incoming_delta);
                    AdmissionOutcome::Dropped {
                        reason: DropReason::SinkCapacity,
                        delta: incoming_delta,
                    }
                }
                AdmissionClass::Structural => self.terminalize_locked(&mut state, event),
            };
        }

        let incoming_content = match event.metadata.class {
            AdmissionClass::Content => incoming_usage,
            AdmissionClass::Structural => BudgetUsage::ZERO,
        };
        let mut victims = Vec::new();
        let mut released = BudgetUsage::ZERO;
        let mut released_content = BudgetUsage::ZERO;
        let mut victim_cursor = 0;
        let mut runtime_capacity_failed = false;

        let (next_total, next_content) = loop {
            let next_total = state
                .total_usage
                .checked_sub(released)
                .and_then(|usage| usage.checked_add(incoming_usage));
            let next_content = state
                .content_usage
                .checked_sub(released_content)
                .and_then(|usage| usage.checked_add(incoming_content));
            let local_fits = next_total
                .zip(next_content)
                .is_some_and(|(total, content)| {
                    total.fits_within(self.limits.total())
                        && content.fits_within(self.limits.content())
                });

            if local_fits {
                match self.runtime.try_replace(released, incoming_usage) {
                    Ok(_) => break (next_total.unwrap(), next_content.unwrap()),
                    Err(BudgetError::LimitExceeded { .. }) => {
                        runtime_capacity_failed = true;
                    }
                    Err(error) => {
                        debug_assert!(false, "diagnostic runtime budget invariant failed: {error}");
                        self.drops.record(incoming_delta);
                        return AdmissionOutcome::Dropped {
                            reason: DropReason::BudgetUnavailable,
                            delta: incoming_delta,
                        };
                    }
                }
            }

            let next_victim = state
                .queued
                .iter()
                .enumerate()
                .skip(victim_cursor)
                .find(|(_, queued)| queued.metadata.class == AdmissionClass::Content)
                .map(|(index, queued)| Victim {
                    index,
                    metadata: queued.metadata,
                });
            let Some(victim) = next_victim else {
                return match event.metadata.class {
                    AdmissionClass::Content => {
                        self.drops.record(incoming_delta);
                        AdmissionOutcome::Dropped {
                            reason: if runtime_capacity_failed {
                                DropReason::RuntimeCapacity
                            } else {
                                DropReason::SinkCapacity
                            },
                            delta: incoming_delta,
                        }
                    }
                    AdmissionClass::Structural => self.terminalize_locked(&mut state, event),
                };
            };
            victim_cursor = victim.index + 1;
            released = released
                .checked_add(victim.metadata.usage())
                .expect("bounded per-sink victim usage must not overflow");
            released_content = released_content
                .checked_add(victim.metadata.usage())
                .expect("bounded per-sink content usage must not overflow");
            victims.push(victim);
        };

        let evicted = victims
            .iter()
            .map(|victim| victim.metadata.drop_delta())
            .collect::<Vec<_>>();
        for victim in victims.iter().rev() {
            let removed = state
                .queued
                .remove(victim.index)
                .expect("planned diagnostic queue victim must remain present");
            debug_assert_eq!(removed.metadata, victim.metadata);
        }
        state.total_usage = next_total;
        state.content_usage = next_content;
        state.queued.push_back(StoredEvent {
            item: event.item,
            metadata: event.metadata,
        });
        self.drops.record_all(&evicted);
        AdmissionOutcome::Enqueued { evicted }
    }

    pub(crate) fn try_begin_callback(
        &self,
    ) -> Result<Option<CallbackDelivery<T>>, QueueAccessError> {
        let mut state = self.try_state()?;
        if state.active_callback.is_some() {
            return Ok(None);
        }
        let Some(queued) = state.queued.pop_front() else {
            return Ok(None);
        };
        let ticket = state.issue_ticket();
        state.active_callback = Some(ActiveCallback {
            ticket,
            metadata: queued.metadata,
        });
        Ok(Some(CallbackDelivery {
            ticket,
            item: queued.item,
            metadata: queued.metadata,
        }))
    }

    pub(crate) fn try_complete_callback(
        &self,
        ticket: CallbackTicket,
    ) -> Result<(), QueueAccessError> {
        let mut state = self.try_state()?;
        let active = state
            .active_callback
            .ok_or(QueueAccessError::NoCallbackActive)?;
        if active.ticket != ticket {
            return Err(QueueAccessError::CallbackTicketMismatch);
        }

        self.runtime
            .try_replace(active.metadata.usage(), BudgetUsage::ZERO)
            .map_err(QueueAccessError::BudgetInvariant)?;
        state.total_usage = state
            .total_usage
            .checked_sub(active.metadata.usage())
            .ok_or(QueueAccessError::BudgetInvariant(
                BudgetError::ReleaseExceedsUsage {
                    current: state.total_usage,
                    released: active.metadata.usage(),
                },
            ))?;
        if active.metadata.class == AdmissionClass::Content {
            state.content_usage = state
                .content_usage
                .checked_sub(active.metadata.usage())
                .ok_or(QueueAccessError::BudgetInvariant(
                    BudgetError::ReleaseExceedsUsage {
                        current: state.content_usage,
                        released: active.metadata.usage(),
                    },
                ))?;
        }
        state.active_callback = None;
        Ok(())
    }

    pub(crate) fn try_discard_queued(&self) -> Result<Vec<DropDelta>, QueueAccessError> {
        let mut state = self.try_state()?;
        self.discard_queued_locked(&mut state)
    }

    fn terminalize_locked(
        &self,
        state: &mut QueueState<T>,
        incoming: QueueEvent<T>,
    ) -> AdmissionOutcome {
        let mut dropped = self
            .discard_queued_locked(state)
            .expect("owned diagnostic queue usage must be releasable");
        let incoming_delta = incoming.metadata.drop_delta();
        dropped.push(incoming_delta);
        self.drops.record(incoming_delta);
        state.terminal_reason = Some(SinkTerminalReason::DeliveryOverflow);
        AdmissionOutcome::Terminalized { dropped }
    }

    fn discard_queued_locked(
        &self,
        state: &mut QueueState<T>,
    ) -> Result<Vec<DropDelta>, QueueAccessError> {
        let callback_usage = state
            .active_callback
            .map(|active| active.metadata.usage())
            .unwrap_or(BudgetUsage::ZERO);
        let callback_content = state
            .active_callback
            .filter(|active| active.metadata.class == AdmissionClass::Content)
            .map(|active| active.metadata.usage())
            .unwrap_or(BudgetUsage::ZERO);
        let queued_usage = state.total_usage.checked_sub(callback_usage).ok_or(
            QueueAccessError::BudgetInvariant(BudgetError::ReleaseExceedsUsage {
                current: state.total_usage,
                released: callback_usage,
            }),
        )?;

        self.runtime
            .try_replace(queued_usage, BudgetUsage::ZERO)
            .map_err(QueueAccessError::BudgetInvariant)?;
        let dropped = state
            .queued
            .iter()
            .map(|queued| queued.metadata.drop_delta())
            .collect::<Vec<_>>();
        state.queued.clear();
        state.total_usage = callback_usage;
        state.content_usage = callback_content;
        self.drops.record_all(&dropped);
        Ok(dropped)
    }

    fn try_state(&self) -> Result<MutexGuard<'_, QueueState<T>>, QueueAccessError> {
        match self.state.try_lock() {
            Ok(state) => Ok(state),
            Err(TryLockError::Poisoned(poisoned)) => Ok(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => Err(QueueAccessError::Contended),
        }
    }
}

impl<T> Drop for SinkQueue<T> {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.total_usage != BudgetUsage::ZERO {
            let released = self
                .runtime
                .try_replace(state.total_usage, BudgetUsage::ZERO);
            debug_assert!(released.is_ok(), "dropping a sink must release owned usage");
        }
    }
}
