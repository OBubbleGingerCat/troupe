use std::{fmt, time::Duration};

use super::admission::IngressStatus;

pub const DEFAULT_WRITER_STALL_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriterDeadlines {
    writer_stall_timeout: Duration,
    shutdown_drain_timeout: Duration,
}

impl WriterDeadlines {
    pub fn new(
        writer_stall_timeout: Duration,
        shutdown_drain_timeout: Duration,
    ) -> Result<Self, DeadlineValidationError> {
        if writer_stall_timeout.is_zero() {
            return Err(DeadlineValidationError::WriterStallNotPositive);
        }
        if shutdown_drain_timeout.is_zero() {
            return Err(DeadlineValidationError::ShutdownDrainNotPositive);
        }
        Ok(Self {
            writer_stall_timeout,
            shutdown_drain_timeout,
        })
    }

    pub const fn writer_stall_timeout(self) -> Duration {
        self.writer_stall_timeout
    }

    pub const fn shutdown_drain_timeout(self) -> Duration {
        self.shutdown_drain_timeout
    }
}

impl Default for WriterDeadlines {
    fn default() -> Self {
        Self {
            writer_stall_timeout: DEFAULT_WRITER_STALL_TIMEOUT,
            shutdown_drain_timeout: DEFAULT_SHUTDOWN_DRAIN_TIMEOUT,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeadlineValidationError {
    WriterStallNotPositive,
    ShutdownDrainNotPositive,
}

impl fmt::Display for DeadlineValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WriterStallNotPositive => {
                formatter.write_str("writer stall timeout must be positive and finite")
            }
            Self::ShutdownDrainNotPositive => {
                formatter.write_str("shutdown drain timeout must be positive and finite")
            }
        }
    }
}

impl std::error::Error for DeadlineValidationError {}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WriterProgressSample {
    committed_sequence: u64,
    accepted_tail_events: usize,
}

impl WriterProgressSample {
    pub const fn new(committed_sequence: u64, accepted_tail_events: usize) -> Self {
        Self {
            committed_sequence,
            accepted_tail_events,
        }
    }

    pub const fn committed_sequence(self) -> u64 {
        self.committed_sequence
    }

    pub const fn accepted_tail_events(self) -> usize {
        self.accepted_tail_events
    }

    pub const fn has_accepted_tail(self) -> bool {
        self.accepted_tail_events > 0
    }
}

impl From<IngressStatus> for WriterProgressSample {
    fn from(status: IngressStatus) -> Self {
        Self::new(
            status.committed_sequence(),
            status.accepted_uncommitted_events(),
        )
    }
}

#[derive(Debug)]
pub struct WriterProgressSupervisor {
    deadlines: WriterDeadlines,
    sample: WriterProgressSample,
    has_sample: bool,
    last_observed_at: Option<Duration>,
    stalled_since: Option<Duration>,
    drain: DrainTracker,
    failure: Option<WriterCoreFailure>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum DrainTracker {
    #[default]
    NotStarted,
    Draining {
        started_at: Duration,
    },
    Drained,
    TimedOut,
}

impl WriterProgressSupervisor {
    pub fn new(deadlines: WriterDeadlines) -> Self {
        Self {
            deadlines,
            sample: WriterProgressSample::default(),
            has_sample: false,
            last_observed_at: None,
            stalled_since: None,
            drain: DrainTracker::NotStarted,
            failure: None,
        }
    }

    pub fn observe(
        &mut self,
        now: Duration,
        sample: WriterProgressSample,
    ) -> Result<Option<WriterCoreFailure>, ProgressObservationError> {
        self.validate_observation(now, sample)?;

        let watermark_advanced =
            self.has_sample && sample.committed_sequence() > self.sample.committed_sequence();
        let tail_started = sample.has_accepted_tail() && !self.sample.has_accepted_tail();
        if !sample.has_accepted_tail() {
            self.stalled_since = None;
        } else if !self.has_sample || watermark_advanced || tail_started {
            self.stalled_since = Some(now);
        }
        self.sample = sample;
        self.has_sample = true;
        self.last_observed_at = Some(now);

        if matches!(self.drain, DrainTracker::Draining { .. }) && !sample.has_accepted_tail() {
            self.drain = DrainTracker::Drained;
            return Ok(None);
        }

        let due = self.due_failure(now);
        if matches!(due, Some(DueFailure::Drain)) {
            self.drain = DrainTracker::TimedOut;
        }
        Ok(due.and_then(|due| self.latch(due.failure())))
    }

    pub fn begin_shutdown(
        &mut self,
        now: Duration,
    ) -> Result<DrainState, ProgressObservationError> {
        if !self.has_sample {
            return Err(ProgressObservationError::SampleUnavailable);
        }
        if self.last_observed_at.is_some_and(|last| now < last) {
            return Err(ProgressObservationError::ClockRegressed);
        }
        match self.drain {
            DrainTracker::NotStarted if self.sample.has_accepted_tail() => {
                self.drain = DrainTracker::Draining { started_at: now };
            }
            DrainTracker::NotStarted => self.drain = DrainTracker::Drained,
            _ => {}
        }
        Ok(self.drain_state())
    }

    pub fn report_writer_outcome(
        &mut self,
        now: Duration,
        sample: WriterProgressSample,
        outcome: WriterTaskOutcome,
    ) -> Result<Option<WriterCoreFailure>, ProgressObservationError> {
        if let Some(failure) = self.observe(now, sample)? {
            return Ok(Some(failure));
        }
        if outcome == WriterTaskOutcome::Exited && self.drain == DrainTracker::Drained {
            return Ok(None);
        }
        Ok(self.latch(outcome.failure()))
    }

    pub fn status(&self) -> WriterProgressStatus {
        let stalled_for = self
            .last_observed_at
            .zip(self.stalled_since)
            .map(|(now, since)| now.saturating_sub(since));
        WriterProgressStatus {
            deadlines: self.deadlines,
            committed_sequence: self.sample.committed_sequence(),
            accepted_tail_events: self.sample.accepted_tail_events(),
            stalled_for,
            drain_state: self.drain_state(),
            failure: self.failure,
        }
    }

    fn validate_observation(
        &self,
        now: Duration,
        sample: WriterProgressSample,
    ) -> Result<(), ProgressObservationError> {
        if self.last_observed_at.is_some_and(|last| now < last) {
            return Err(ProgressObservationError::ClockRegressed);
        }
        if self.has_sample && sample.committed_sequence() < self.sample.committed_sequence() {
            return Err(ProgressObservationError::WatermarkRegressed {
                previous: self.sample.committed_sequence(),
                actual: sample.committed_sequence(),
            });
        }
        Ok(())
    }

    fn due_failure(&self, now: Duration) -> Option<DueFailure> {
        let stall_due = self.stalled_since.map(|since| {
            (
                since.saturating_add(self.deadlines.writer_stall_timeout()),
                DueFailure::Stall,
            )
        });
        let drain_due = match self.drain {
            DrainTracker::Draining { started_at } => Some((
                started_at.saturating_add(self.deadlines.shutdown_drain_timeout()),
                DueFailure::Drain,
            )),
            _ => None,
        };
        [stall_due, drain_due]
            .into_iter()
            .flatten()
            .filter(|(deadline, _)| now >= *deadline)
            .min_by_key(|(deadline, due)| (*deadline, due.precedence()))
            .map(|(_, due)| due)
    }

    fn latch(&mut self, failure: WriterCoreFailure) -> Option<WriterCoreFailure> {
        if self.failure.is_some() {
            return None;
        }
        self.failure = Some(failure);
        Some(failure)
    }

    const fn drain_state(&self) -> DrainState {
        match self.drain {
            DrainTracker::NotStarted => DrainState::NotStarted,
            DrainTracker::Draining { .. } => DrainState::Draining,
            DrainTracker::Drained => DrainState::Drained,
            DrainTracker::TimedOut => DrainState::TimedOut,
        }
    }
}

impl Default for WriterProgressSupervisor {
    fn default() -> Self {
        Self::new(WriterDeadlines::default())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DueFailure {
    Stall,
    Drain,
}

impl DueFailure {
    const fn failure(self) -> WriterCoreFailure {
        match self {
            Self::Stall => WriterCoreFailure::new(
                WriterFailureStage::Progress,
                WriterFailureCode::ProgressStalled,
            ),
            Self::Drain => WriterCoreFailure::new(
                WriterFailureStage::Drain,
                WriterFailureCode::ShutdownDrainTimedOut,
            ),
        }
    }

    const fn precedence(self) -> u8 {
        match self {
            Self::Stall => 0,
            Self::Drain => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgressObservationError {
    ClockRegressed,
    WatermarkRegressed { previous: u64, actual: u64 },
    SampleUnavailable,
}

impl fmt::Display for ProgressObservationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClockRegressed => formatter.write_str("writer progress clock regressed"),
            Self::WatermarkRegressed { previous, actual } => write!(
                formatter,
                "writer committed watermark regressed from {previous} to {actual}"
            ),
            Self::SampleUnavailable => formatter.write_str("writer progress sample is unavailable"),
        }
    }
}

impl std::error::Error for ProgressObservationError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriterTaskOutcome {
    Exited,
    Panicked,
    CommitUnavailable,
    FlushUnavailable,
    StorageUnavailable,
}

impl WriterTaskOutcome {
    const fn failure(self) -> WriterCoreFailure {
        let (stage, code) = match self {
            Self::Exited => (
                WriterFailureStage::TaskExit,
                WriterFailureCode::UnexpectedExit,
            ),
            Self::Panicked => (WriterFailureStage::TaskExit, WriterFailureCode::Panicked),
            Self::CommitUnavailable => (
                WriterFailureStage::Commit,
                WriterFailureCode::CommitUnavailable,
            ),
            Self::FlushUnavailable => (
                WriterFailureStage::Flush,
                WriterFailureCode::FlushUnavailable,
            ),
            Self::StorageUnavailable => (
                WriterFailureStage::Storage,
                WriterFailureCode::StorageUnavailable,
            ),
        };
        WriterCoreFailure::new(stage, code)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriterCoreFailure {
    stage: WriterFailureStage,
    code: WriterFailureCode,
}

impl WriterCoreFailure {
    const fn new(stage: WriterFailureStage, code: WriterFailureCode) -> Self {
        Self { stage, code }
    }

    pub const fn component(self) -> &'static str {
        "writer"
    }

    pub const fn stage(self) -> WriterFailureStage {
        self.stage
    }

    pub const fn code(self) -> WriterFailureCode {
        self.code
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriterFailureStage {
    Progress,
    TaskExit,
    Commit,
    Flush,
    Storage,
    Drain,
}

impl WriterFailureStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Progress => "progress",
            Self::TaskExit => "task_exit",
            Self::Commit => "commit",
            Self::Flush => "flush",
            Self::Storage => "storage",
            Self::Drain => "drain",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriterFailureCode {
    ProgressStalled,
    UnexpectedExit,
    Panicked,
    CommitUnavailable,
    FlushUnavailable,
    StorageUnavailable,
    ShutdownDrainTimedOut,
}

impl WriterFailureCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProgressStalled => "writer_progress_stalled",
            Self::UnexpectedExit => "writer_unexpected_exit",
            Self::Panicked => "writer_panicked",
            Self::CommitUnavailable => "writer_commit_unavailable",
            Self::FlushUnavailable => "writer_flush_unavailable",
            Self::StorageUnavailable => "writer_storage_unavailable",
            Self::ShutdownDrainTimedOut => "writer_shutdown_drain_timed_out",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrainState {
    NotStarted,
    Draining,
    Drained,
    TimedOut,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriterProgressStatus {
    deadlines: WriterDeadlines,
    committed_sequence: u64,
    accepted_tail_events: usize,
    stalled_for: Option<Duration>,
    drain_state: DrainState,
    failure: Option<WriterCoreFailure>,
}

impl WriterProgressStatus {
    pub const fn deadlines(self) -> WriterDeadlines {
        self.deadlines
    }

    pub const fn writer_stall_timeout(self) -> Duration {
        self.deadlines.writer_stall_timeout()
    }

    pub const fn shutdown_drain_timeout(self) -> Duration {
        self.deadlines.shutdown_drain_timeout()
    }

    pub const fn committed_sequence(self) -> u64 {
        self.committed_sequence
    }

    pub const fn accepted_tail_events(self) -> usize {
        self.accepted_tail_events
    }

    pub const fn stalled_for(self) -> Option<Duration> {
        self.stalled_for
    }

    pub const fn drain_state(self) -> DrainState {
        self.drain_state
    }

    pub const fn drain_complete(self) -> bool {
        matches!(self.drain_state, DrainState::Drained)
    }

    pub const fn failure(self) -> Option<WriterCoreFailure> {
        self.failure
    }
}
