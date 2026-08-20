use std::{fmt, time::Duration};

use troupe_diagnostics_core::{hub::AcceptedDiagnosticEvent, id::CanonicalUuid};

pub const MAX_BATCH_AGE: Duration = Duration::from_millis(25);
pub const MAX_BATCH_EVENTS: usize = 512;
pub const MAX_BATCH_CANONICAL_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct EventBatch {
    events: Vec<AcceptedDiagnosticEvent>,
    canonical_bytes: usize,
}

impl EventBatch {
    pub fn new(events: Vec<AcceptedDiagnosticEvent>) -> Result<Self, BatchError> {
        if events.is_empty() {
            return Err(BatchError::Empty);
        }

        let run_id = events[0].identity().run_id();
        let mut previous: Option<u64> = None;
        let mut canonical_bytes = 0_usize;
        for event in &events {
            let identity = event.identity();
            if identity.run_id() != run_id {
                return Err(BatchError::RunIdentityMismatch {
                    expected: run_id,
                    actual: identity.run_id(),
                });
            }
            if let Some(previous) = previous {
                let expected = previous
                    .checked_add(1)
                    .ok_or(BatchError::SequenceExhausted)?;
                if identity.sequence().get() != expected {
                    return Err(BatchError::NonCanonicalSequence {
                        expected,
                        actual: identity.sequence().get(),
                    });
                }
            }
            previous = Some(identity.sequence().get());
            canonical_bytes = canonical_bytes
                .checked_add(event.canonical_bytes().len())
                .ok_or(BatchError::CanonicalByteCountOverflow)?;
        }

        Ok(Self {
            events,
            canonical_bytes,
        })
    }

    pub fn events(&self) -> &[AcceptedDiagnosticEvent] {
        &self.events
    }

    pub fn first(&self) -> &AcceptedDiagnosticEvent {
        &self.events[0]
    }

    pub fn last(&self) -> &AcceptedDiagnosticEvent {
        self.events.last().expect("an EventBatch is non-empty")
    }

    pub fn run_id(&self) -> CanonicalUuid {
        self.first().identity().run_id()
    }

    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    pub const fn canonical_bytes(&self) -> usize {
        self.canonical_bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BatchTrigger {
    OldestAge,
    EventCount,
    CanonicalBytes,
    ExplicitFlush,
}

#[derive(Clone, Debug)]
pub struct TriggeredBatch {
    batch: EventBatch,
    trigger: BatchTrigger,
}

impl TriggeredBatch {
    pub const fn batch(&self) -> &EventBatch {
        &self.batch
    }

    pub const fn trigger(&self) -> BatchTrigger {
        self.trigger
    }

    pub fn into_batch(self) -> EventBatch {
        self.batch
    }
}

#[derive(Debug, Default)]
pub struct BatchAccumulator {
    pending: Vec<AcceptedDiagnosticEvent>,
    pending_bytes: usize,
    oldest_at: Option<Duration>,
    last_observed_at: Option<Duration>,
    last_sequence: Option<u64>,
    run_id: Option<CanonicalUuid>,
}

impl BatchAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(
        &mut self,
        event: AcceptedDiagnosticEvent,
        now: Duration,
    ) -> Result<Option<TriggeredBatch>, BatchError> {
        self.validate_next(&event)?;
        self.observe_time(now)?;

        let event_bytes = event.canonical_bytes().len();
        self.pending_bytes = self
            .pending_bytes
            .checked_add(event_bytes)
            .ok_or(BatchError::CanonicalByteCountOverflow)?;
        self.oldest_at.get_or_insert(now);
        self.last_sequence = Some(event.identity().sequence().get());
        self.run_id = Some(event.identity().run_id());
        self.pending.push(event);

        let trigger = if self.oldest_is_due(now) {
            Some(BatchTrigger::OldestAge)
        } else if self.pending.len() >= MAX_BATCH_EVENTS {
            Some(BatchTrigger::EventCount)
        } else if self.pending_bytes >= MAX_BATCH_CANONICAL_BYTES {
            Some(BatchTrigger::CanonicalBytes)
        } else {
            None
        };
        Ok(trigger.map(|trigger| self.take(trigger)))
    }

    pub fn poll(&mut self, now: Duration) -> Result<Option<TriggeredBatch>, BatchError> {
        self.observe_time(now)?;
        if self.oldest_is_due(now) {
            Ok(Some(self.take(BatchTrigger::OldestAge)))
        } else {
            Ok(None)
        }
    }

    pub fn flush(&mut self) -> Result<Option<TriggeredBatch>, BatchError> {
        if self.pending.is_empty() {
            Ok(None)
        } else {
            Ok(Some(self.take(BatchTrigger::ExplicitFlush)))
        }
    }

    pub fn pending_event_count(&self) -> usize {
        self.pending.len()
    }

    pub const fn pending_canonical_bytes(&self) -> usize {
        self.pending_bytes
    }

    fn observe_time(&mut self, now: Duration) -> Result<(), BatchError> {
        if self.last_observed_at.is_some_and(|last| now < last) {
            return Err(BatchError::ClockRegressed);
        }
        self.last_observed_at = Some(now);
        Ok(())
    }

    fn validate_next(&self, event: &AcceptedDiagnosticEvent) -> Result<(), BatchError> {
        let identity = event.identity();
        if let Some(run_id) = self.run_id
            && identity.run_id() != run_id
        {
            return Err(BatchError::RunIdentityMismatch {
                expected: run_id,
                actual: identity.run_id(),
            });
        }
        if let Some(previous) = self.last_sequence {
            let expected = previous
                .checked_add(1)
                .ok_or(BatchError::SequenceExhausted)?;
            if identity.sequence().get() != expected {
                return Err(BatchError::NonCanonicalSequence {
                    expected,
                    actual: identity.sequence().get(),
                });
            }
        }
        Ok(())
    }

    fn oldest_is_due(&self, now: Duration) -> bool {
        self.oldest_at
            .is_some_and(|oldest| now.saturating_sub(oldest) >= MAX_BATCH_AGE)
    }

    fn take(&mut self, trigger: BatchTrigger) -> TriggeredBatch {
        let events = std::mem::take(&mut self.pending);
        let canonical_bytes = self.pending_bytes;
        self.pending_bytes = 0;
        self.oldest_at = None;
        TriggeredBatch {
            batch: EventBatch {
                events,
                canonical_bytes,
            },
            trigger,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BatchError {
    Empty,
    RunIdentityMismatch {
        expected: CanonicalUuid,
        actual: CanonicalUuid,
    },
    NonCanonicalSequence {
        expected: u64,
        actual: u64,
    },
    SequenceExhausted,
    CanonicalByteCountOverflow,
    ClockRegressed,
}

impl fmt::Display for BatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("diagnostic event batch cannot be empty"),
            Self::RunIdentityMismatch { expected, actual } => write!(
                formatter,
                "diagnostic batch Run identity {actual} differs from {expected}"
            ),
            Self::NonCanonicalSequence { expected, actual } => write!(
                formatter,
                "diagnostic batch expected sequence {expected}, found {actual}"
            ),
            Self::SequenceExhausted => {
                formatter.write_str("diagnostic batch sequence is exhausted")
            }
            Self::CanonicalByteCountOverflow => {
                formatter.write_str("diagnostic batch canonical byte count overflowed")
            }
            Self::ClockRegressed => formatter.write_str("diagnostic batch clock regressed"),
        }
    }
}

impl std::error::Error for BatchError {}
