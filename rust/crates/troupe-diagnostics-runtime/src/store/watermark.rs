use std::fmt;

use troupe_diagnostics_core::id::CanonicalUuid;

use super::{batch::EventBatch, key::SortableU64Key};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommitNotification {
    run_id: CanonicalUuid,
    previous: SortableU64Key,
    committed: SortableU64Key,
    event_count: usize,
    canonical_bytes: usize,
}

impl CommitNotification {
    pub const fn run_id(self) -> CanonicalUuid {
        self.run_id
    }

    pub const fn previous(self) -> SortableU64Key {
        self.previous
    }

    pub const fn committed(self) -> SortableU64Key {
        self.committed
    }

    pub const fn event_count(self) -> usize {
        self.event_count
    }

    pub const fn canonical_bytes(self) -> usize {
        self.canonical_bytes
    }
}

pub trait CommitObserver {
    fn committed(&mut self, notification: CommitNotification);
}

impl CommitObserver for () {
    fn committed(&mut self, _notification: CommitNotification) {}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommittedWatermark {
    run_id: CanonicalUuid,
    value: SortableU64Key,
}

impl CommittedWatermark {
    pub const fn fresh(run_id: CanonicalUuid) -> Self {
        Self {
            run_id,
            value: SortableU64Key::new(0),
        }
    }

    pub const fn run_id(self) -> CanonicalUuid {
        self.run_id
    }

    pub const fn value(self) -> SortableU64Key {
        self.value
    }

    pub fn candidate(self, batch: &EventBatch) -> Result<CommitNotification, WatermarkError> {
        if batch.run_id() != self.run_id {
            return Err(WatermarkError::RunIdentityMismatch {
                expected: self.run_id,
                actual: batch.run_id(),
            });
        }
        let expected = self
            .value
            .get()
            .checked_add(1)
            .ok_or(WatermarkError::SequenceExhausted)?;
        let first = batch.first().identity().sequence().get();
        if first != expected {
            return Err(WatermarkError::NonCanonicalSequence {
                expected,
                actual: first,
            });
        }

        Ok(CommitNotification {
            run_id: self.run_id,
            previous: self.value,
            committed: SortableU64Key::new(batch.last().identity().sequence().get()),
            event_count: batch.event_count(),
            canonical_bytes: batch.canonical_bytes(),
        })
    }

    pub fn advance(&mut self, notification: CommitNotification) -> Result<(), WatermarkError> {
        if notification.run_id != self.run_id {
            return Err(WatermarkError::RunIdentityMismatch {
                expected: self.run_id,
                actual: notification.run_id,
            });
        }
        if notification.previous != self.value {
            return Err(WatermarkError::StaleCandidate {
                expected: self.value.get(),
                actual: notification.previous.get(),
            });
        }
        self.value = notification.committed;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WatermarkError {
    RunIdentityMismatch {
        expected: CanonicalUuid,
        actual: CanonicalUuid,
    },
    NonCanonicalSequence {
        expected: u64,
        actual: u64,
    },
    StaleCandidate {
        expected: u64,
        actual: u64,
    },
    SequenceExhausted,
}

impl fmt::Display for WatermarkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RunIdentityMismatch { expected, actual } => write!(
                formatter,
                "diagnostic watermark Run identity {actual} differs from {expected}"
            ),
            Self::NonCanonicalSequence { expected, actual } => write!(
                formatter,
                "diagnostic watermark expected sequence {expected}, found {actual}"
            ),
            Self::StaleCandidate { expected, actual } => write!(
                formatter,
                "diagnostic watermark candidate starts at {actual}, current watermark is {expected}"
            ),
            Self::SequenceExhausted => {
                formatter.write_str("diagnostic committed sequence is exhausted")
            }
        }
    }
}

impl std::error::Error for WatermarkError {}
