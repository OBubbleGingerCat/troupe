use std::{fmt, time::Duration};

use troupe_diagnostics_core::{id::CanonicalUuid, scalar::SchemaU64};

use crate::store::{
    admission::{IngressCoreFailure, IngressStatus},
    batch::{MAX_BATCH_AGE, MAX_BATCH_CANONICAL_BYTES, MAX_BATCH_EVENTS},
    progress::{DrainState, WriterCoreFailure, WriterProgressStatus},
    quota::{QuotaFailure, QuotaStatus},
};

use super::reader::{CapturedEventSource, ReaderProfile};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusSource {
    Active,
    Archive,
}

impl StatusSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archive => "archive",
        }
    }
}

impl From<ReaderProfile> for StatusSource {
    fn from(profile: ReaderProfile) -> Self {
        match profile {
            ReaderProfile::Active => Self::Active,
            ReaderProfile::Archive => Self::Archive,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductionOutcome {
    Completed,
    Failed,
    Cancelled,
}

impl ProductionOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> Result<Self, StatusProjectionError> {
        match value {
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(StatusProjectionError::UnknownProductionOutcome(
                value.to_owned(),
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductionState {
    Active,
    Completed,
    Failed,
    Incomplete,
}

impl ProductionState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Incomplete => "incomplete",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnavailableReason {
    Archive,
    NotObserved,
    StateUnavailable,
}

impl UnavailableReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Archive => "archive",
            Self::NotObserved => "not_observed",
            Self::StateUnavailable => "state_unavailable",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Observation<T> {
    Available(T),
    Unavailable(UnavailableReason),
}

impl<T> Observation<T> {
    pub const fn available(value: T) -> Self {
        Self::Available(value)
    }

    pub const fn unavailable(reason: UnavailableReason) -> Self {
        Self::Unavailable(reason)
    }

    pub const fn as_available(&self) -> Option<&T> {
        match self {
            Self::Available(value) => Some(value),
            Self::Unavailable(_) => None,
        }
    }

    pub const fn unavailable_reason(&self) -> Option<UnavailableReason> {
        match self {
            Self::Available(_) => None,
            Self::Unavailable(reason) => Some(*reason),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanonicalDuration {
    seconds: SchemaU64,
    subsecond_nanoseconds: SchemaU64,
}

impl CanonicalDuration {
    pub const fn from_duration(value: Duration) -> Self {
        Self {
            seconds: SchemaU64::new(value.as_secs()),
            subsecond_nanoseconds: SchemaU64::new(value.subsec_nanos() as u64),
        }
    }

    pub const fn seconds(self) -> SchemaU64 {
        self.seconds
    }

    pub const fn subsecond_nanoseconds(self) -> SchemaU64 {
        self.subsecond_nanoseconds
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveWriterObservation {
    ingress: IngressStatus,
    progress: WriterProgressStatus,
}

impl ActiveWriterObservation {
    pub const fn new(ingress: IngressStatus, progress: WriterProgressStatus) -> Self {
        Self { ingress, progress }
    }

    pub const fn ingress(&self) -> IngressStatus {
        self.ingress
    }

    pub const fn progress(&self) -> WriterProgressStatus {
        self.progress
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveStatusObservation {
    writer: Observation<ActiveWriterObservation>,
    quota: Observation<QuotaStatus>,
}

impl ActiveStatusObservation {
    pub fn available(
        ingress: IngressStatus,
        progress: WriterProgressStatus,
        quota: QuotaStatus,
    ) -> Self {
        Self::new(
            Observation::available(ActiveWriterObservation::new(ingress, progress)),
            Observation::available(quota),
        )
    }

    pub const fn new(
        writer: Observation<ActiveWriterObservation>,
        quota: Observation<QuotaStatus>,
    ) -> Self {
        Self { writer, quota }
    }

    pub const fn unavailable() -> Self {
        Self::new(
            Observation::unavailable(UnavailableReason::StateUnavailable),
            Observation::unavailable(UnavailableReason::StateUnavailable),
        )
    }

    pub const fn writer(&self) -> &Observation<ActiveWriterObservation> {
        &self.writer
    }

    pub const fn quota(&self) -> &Observation<QuotaStatus> {
        &self.quota
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusIdentity {
    run_id: CanonicalUuid,
    source: StatusSource,
    store_schema_version: SchemaU64,
    store_schema_identity: &'static str,
    event_schema_version: SchemaU64,
}

impl StatusIdentity {
    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub const fn source(&self) -> StatusSource {
        self.source
    }

    pub const fn store_schema_version(&self) -> SchemaU64 {
        self.store_schema_version
    }

    pub const fn store_schema_identity(&self) -> &'static str {
        self.store_schema_identity
    }

    pub const fn event_schema_version(&self) -> SchemaU64 {
        self.event_schema_version
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductionLifecycleStatus {
    state: ProductionState,
    started_at: String,
    ended_at: Option<String>,
    outcome: Option<ProductionOutcome>,
    clean_shutdown: bool,
}

impl ProductionLifecycleStatus {
    pub const fn state(&self) -> ProductionState {
        self.state
    }

    pub fn started_at(&self) -> &str {
        &self.started_at
    }

    pub fn ended_at(&self) -> Option<&str> {
        self.ended_at.as_deref()
    }

    pub const fn outcome(&self) -> Option<ProductionOutcome> {
        self.outcome
    }

    pub const fn clean_shutdown(&self) -> bool {
        self.clean_shutdown
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IngressFailureStatus {
    code: &'static str,
    current_events: SchemaU64,
    current_canonical_bytes: SchemaU64,
    attempted_events: SchemaU64,
    attempted_canonical_bytes: SchemaU64,
    event_limit_exceeded: bool,
    byte_limit_exceeded: bool,
}

impl IngressFailureStatus {
    pub const fn code(&self) -> &'static str {
        self.code
    }

    pub const fn current_events(&self) -> SchemaU64 {
        self.current_events
    }

    pub const fn current_canonical_bytes(&self) -> SchemaU64 {
        self.current_canonical_bytes
    }

    pub const fn attempted_events(&self) -> SchemaU64 {
        self.attempted_events
    }

    pub const fn attempted_canonical_bytes(&self) -> SchemaU64 {
        self.attempted_canonical_bytes
    }

    pub const fn event_limit_exceeded(&self) -> bool {
        self.event_limit_exceeded
    }

    pub const fn byte_limit_exceeded(&self) -> bool {
        self.byte_limit_exceeded
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriterFailureStatus {
    component: &'static str,
    stage: &'static str,
    code: &'static str,
}

impl WriterFailureStatus {
    pub const fn component(self) -> &'static str {
        self.component
    }

    pub const fn stage(self) -> &'static str {
        self.stage
    }

    pub const fn code(self) -> &'static str {
        self.code
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriterDrainState {
    NotStarted,
    Draining,
    Drained,
    TimedOut,
}

impl WriterDrainState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::Draining => "draining",
            Self::Drained => "drained",
            Self::TimedOut => "timed_out",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriterStatus {
    max_uncommitted_events: SchemaU64,
    max_uncommitted_canonical_bytes: SchemaU64,
    max_batch_age: CanonicalDuration,
    max_batch_events: SchemaU64,
    max_batch_canonical_bytes: SchemaU64,
    accepted_uncommitted_events: SchemaU64,
    accepted_uncommitted_canonical_bytes: SchemaU64,
    queued_events: SchemaU64,
    in_flight_events: SchemaU64,
    ingress_committed_watermark: SchemaU64,
    normal_ingress_sealed: bool,
    ingress_failure: Option<IngressFailureStatus>,
    writer_stall_timeout: CanonicalDuration,
    shutdown_drain_timeout: CanonicalDuration,
    progress_committed_watermark: SchemaU64,
    accepted_tail_events: SchemaU64,
    stalled_for: Option<CanonicalDuration>,
    drain_state: WriterDrainState,
    progress_failure: Option<WriterFailureStatus>,
}

impl WriterStatus {
    pub const fn max_uncommitted_events(&self) -> SchemaU64 {
        self.max_uncommitted_events
    }

    pub const fn max_uncommitted_canonical_bytes(&self) -> SchemaU64 {
        self.max_uncommitted_canonical_bytes
    }

    pub const fn max_batch_age(&self) -> CanonicalDuration {
        self.max_batch_age
    }

    pub const fn max_batch_events(&self) -> SchemaU64 {
        self.max_batch_events
    }

    pub const fn max_batch_canonical_bytes(&self) -> SchemaU64 {
        self.max_batch_canonical_bytes
    }

    pub const fn accepted_uncommitted_events(&self) -> SchemaU64 {
        self.accepted_uncommitted_events
    }

    pub const fn accepted_uncommitted_canonical_bytes(&self) -> SchemaU64 {
        self.accepted_uncommitted_canonical_bytes
    }

    pub const fn queued_events(&self) -> SchemaU64 {
        self.queued_events
    }

    pub const fn in_flight_events(&self) -> SchemaU64 {
        self.in_flight_events
    }

    pub const fn ingress_committed_watermark(&self) -> SchemaU64 {
        self.ingress_committed_watermark
    }

    pub const fn normal_ingress_sealed(&self) -> bool {
        self.normal_ingress_sealed
    }

    pub const fn ingress_failure(&self) -> Option<&IngressFailureStatus> {
        self.ingress_failure.as_ref()
    }

    pub const fn writer_stall_timeout(&self) -> CanonicalDuration {
        self.writer_stall_timeout
    }

    pub const fn shutdown_drain_timeout(&self) -> CanonicalDuration {
        self.shutdown_drain_timeout
    }

    pub const fn progress_committed_watermark(&self) -> SchemaU64 {
        self.progress_committed_watermark
    }

    pub const fn accepted_tail_events(&self) -> SchemaU64 {
        self.accepted_tail_events
    }

    pub const fn stalled_for(&self) -> Option<CanonicalDuration> {
        self.stalled_for
    }

    pub const fn drain_state(&self) -> WriterDrainState {
        self.drain_state
    }

    pub const fn progress_failure(&self) -> Option<WriterFailureStatus> {
        self.progress_failure
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuotaFailureStatus {
    code: &'static str,
    limit_bytes: SchemaU64,
    current_bytes: Option<SchemaU64>,
    predicted_growth_bytes: Option<SchemaU64>,
}

impl QuotaFailureStatus {
    pub const fn code(&self) -> &'static str {
        self.code
    }

    pub const fn limit_bytes(&self) -> SchemaU64 {
        self.limit_bytes
    }

    pub const fn current_bytes(&self) -> Option<SchemaU64> {
        self.current_bytes
    }

    pub const fn predicted_growth_bytes(&self) -> Option<SchemaU64> {
        self.predicted_growth_bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuotaProjection {
    max_run_bytes: Option<SchemaU64>,
    current_measured_bytes: Option<SchemaU64>,
    last_measurement_at: Option<CanonicalDuration>,
    sealed: bool,
    failure: Option<QuotaFailureStatus>,
}

impl QuotaProjection {
    pub const fn max_run_bytes(&self) -> Option<SchemaU64> {
        self.max_run_bytes
    }

    pub const fn current_measured_bytes(&self) -> Option<SchemaU64> {
        self.current_measured_bytes
    }

    pub const fn last_measurement_at(&self) -> Option<CanonicalDuration> {
        self.last_measurement_at
    }

    pub const fn sealed(&self) -> bool {
        self.sealed
    }

    pub const fn failure(&self) -> Option<&QuotaFailureStatus> {
        self.failure.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticStatus {
    identity: StatusIdentity,
    lifecycle: ProductionLifecycleStatus,
    configuration_identity: String,
    event_watermark: SchemaU64,
    read_model_watermark: SchemaU64,
    writer: Observation<WriterStatus>,
    quota: Observation<QuotaProjection>,
}

impl DiagnosticStatus {
    pub const fn identity(&self) -> &StatusIdentity {
        &self.identity
    }

    pub const fn lifecycle(&self) -> &ProductionLifecycleStatus {
        &self.lifecycle
    }

    pub fn configuration_identity(&self) -> &str {
        &self.configuration_identity
    }

    pub const fn event_watermark(&self) -> SchemaU64 {
        self.event_watermark
    }

    pub const fn read_model_watermark(&self) -> SchemaU64 {
        self.read_model_watermark
    }

    pub const fn writer(&self) -> &Observation<WriterStatus> {
        &self.writer
    }

    pub const fn quota(&self) -> &Observation<QuotaProjection> {
        &self.quota
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StatusProjectionError {
    UnknownProductionOutcome(String),
    NumericOutOfRange { field: &'static str },
}

impl fmt::Display for StatusProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownProductionOutcome(outcome) => {
                write!(formatter, "unknown Production outcome {outcome:?}")
            }
            Self::NumericOutOfRange { field } => {
                write!(
                    formatter,
                    "status field {field} exceeds the canonical u64 domain"
                )
            }
        }
    }
}

impl std::error::Error for StatusProjectionError {}

pub fn project_status(
    source: &CapturedEventSource<'_>,
    active: Option<&ActiveStatusObservation>,
) -> Result<DiagnosticStatus, StatusProjectionError> {
    let metadata = source.metadata();
    let status_source = StatusSource::from(source.profile());
    let outcome = metadata
        .production_outcome()
        .map(ProductionOutcome::parse)
        .transpose()?;
    let lifecycle = ProductionLifecycleStatus {
        state: classify_production_state(
            status_source,
            metadata.ended_at(),
            outcome,
            metadata.clean_shutdown(),
        ),
        started_at: metadata.started_at().to_owned(),
        ended_at: metadata.ended_at().map(str::to_owned),
        outcome,
        clean_shutdown: metadata.clean_shutdown(),
    };
    let (writer, quota) = project_live_observations(status_source, active)?;

    Ok(DiagnosticStatus {
        identity: StatusIdentity {
            run_id: metadata.run_id(),
            source: status_source,
            store_schema_version: SchemaU64::new(u64::from(metadata.store_schema_version())),
            store_schema_identity: metadata.store_schema_identity(),
            event_schema_version: SchemaU64::new(u64::from(metadata.event_schema_version())),
        },
        lifecycle,
        configuration_identity: metadata.configuration_identity().to_owned(),
        event_watermark: metadata.committed_watermark(),
        read_model_watermark: metadata.read_model_watermark(),
        writer,
        quota,
    })
}

fn classify_production_state(
    source: StatusSource,
    ended_at: Option<&str>,
    outcome: Option<ProductionOutcome>,
    clean_shutdown: bool,
) -> ProductionState {
    match (ended_at, outcome, clean_shutdown) {
        (Some(_), Some(ProductionOutcome::Completed), true) => ProductionState::Completed,
        (Some(_), Some(ProductionOutcome::Failed | ProductionOutcome::Cancelled), true) => {
            ProductionState::Failed
        }
        (None, None, false) if source == StatusSource::Active => ProductionState::Active,
        _ => ProductionState::Incomplete,
    }
}

fn project_live_observations(
    source: StatusSource,
    active: Option<&ActiveStatusObservation>,
) -> Result<(Observation<WriterStatus>, Observation<QuotaProjection>), StatusProjectionError> {
    if source == StatusSource::Archive {
        return Ok((
            Observation::unavailable(UnavailableReason::Archive),
            Observation::unavailable(UnavailableReason::Archive),
        ));
    }
    let Some(active) = active else {
        return Ok((
            Observation::unavailable(UnavailableReason::NotObserved),
            Observation::unavailable(UnavailableReason::NotObserved),
        ));
    };
    let writer = match active.writer() {
        Observation::Available(value) => Observation::available(project_writer(value)?),
        Observation::Unavailable(reason) => Observation::unavailable(*reason),
    };
    let quota = match active.quota() {
        Observation::Available(value) => Observation::available(project_quota(value)),
        Observation::Unavailable(reason) => Observation::unavailable(*reason),
    };
    Ok((writer, quota))
}

fn project_writer(
    observation: &ActiveWriterObservation,
) -> Result<WriterStatus, StatusProjectionError> {
    let ingress = observation.ingress();
    let progress = observation.progress();
    Ok(WriterStatus {
        max_uncommitted_events: usize_value(
            "writer.max_uncommitted_events",
            ingress.max_uncommitted_events(),
        )?,
        max_uncommitted_canonical_bytes: usize_value(
            "writer.max_uncommitted_canonical_bytes",
            ingress.max_uncommitted_canonical_bytes(),
        )?,
        max_batch_age: CanonicalDuration::from_duration(MAX_BATCH_AGE),
        max_batch_events: usize_value("writer.max_batch_events", MAX_BATCH_EVENTS)?,
        max_batch_canonical_bytes: usize_value(
            "writer.max_batch_canonical_bytes",
            MAX_BATCH_CANONICAL_BYTES,
        )?,
        accepted_uncommitted_events: usize_value(
            "writer.accepted_uncommitted_events",
            ingress.accepted_uncommitted_events(),
        )?,
        accepted_uncommitted_canonical_bytes: usize_value(
            "writer.accepted_uncommitted_canonical_bytes",
            ingress.accepted_uncommitted_canonical_bytes(),
        )?,
        queued_events: usize_value("writer.queued_events", ingress.queued_events())?,
        in_flight_events: usize_value("writer.in_flight_events", ingress.in_flight_events())?,
        ingress_committed_watermark: SchemaU64::new(ingress.committed_sequence()),
        normal_ingress_sealed: ingress.normal_ingress_sealed(),
        ingress_failure: ingress.failure().map(project_ingress_failure).transpose()?,
        writer_stall_timeout: CanonicalDuration::from_duration(progress.writer_stall_timeout()),
        shutdown_drain_timeout: CanonicalDuration::from_duration(progress.shutdown_drain_timeout()),
        progress_committed_watermark: SchemaU64::new(progress.committed_sequence()),
        accepted_tail_events: usize_value(
            "writer.accepted_tail_events",
            progress.accepted_tail_events(),
        )?,
        stalled_for: progress.stalled_for().map(CanonicalDuration::from_duration),
        drain_state: project_drain_state(progress.drain_state()),
        progress_failure: progress.failure().map(project_writer_failure),
    })
}

fn project_ingress_failure(
    failure: IngressCoreFailure,
) -> Result<IngressFailureStatus, StatusProjectionError> {
    Ok(IngressFailureStatus {
        code: failure.code(),
        current_events: usize_value("writer.failure.current_events", failure.current_events())?,
        current_canonical_bytes: usize_value(
            "writer.failure.current_canonical_bytes",
            failure.current_canonical_bytes(),
        )?,
        attempted_events: usize_value(
            "writer.failure.attempted_events",
            failure.attempted_events(),
        )?,
        attempted_canonical_bytes: usize_value(
            "writer.failure.attempted_canonical_bytes",
            failure.attempted_canonical_bytes(),
        )?,
        event_limit_exceeded: failure.event_limit_exceeded(),
        byte_limit_exceeded: failure.byte_limit_exceeded(),
    })
}

fn project_writer_failure(failure: WriterCoreFailure) -> WriterFailureStatus {
    WriterFailureStatus {
        component: failure.component(),
        stage: failure.stage().as_str(),
        code: failure.code().as_str(),
    }
}

const fn project_drain_state(state: DrainState) -> WriterDrainState {
    match state {
        DrainState::NotStarted => WriterDrainState::NotStarted,
        DrainState::Draining => WriterDrainState::Draining,
        DrainState::Drained => WriterDrainState::Drained,
        DrainState::TimedOut => WriterDrainState::TimedOut,
    }
}

fn project_quota(status: &QuotaStatus) -> QuotaProjection {
    QuotaProjection {
        max_run_bytes: status.max_run_bytes().map(SchemaU64::new),
        current_measured_bytes: status.current_measured_bytes().map(SchemaU64::new),
        last_measurement_at: status
            .last_measurement_at()
            .map(CanonicalDuration::from_duration),
        sealed: status.sealed(),
        failure: status.failure().map(project_quota_failure),
    }
}

fn project_quota_failure(failure: &QuotaFailure) -> QuotaFailureStatus {
    QuotaFailureStatus {
        code: failure.code().as_str(),
        limit_bytes: SchemaU64::new(failure.limit_bytes()),
        current_bytes: failure.current_bytes().map(SchemaU64::new),
        predicted_growth_bytes: failure.predicted_growth_bytes().map(SchemaU64::new),
    }
}

fn usize_value(field: &'static str, value: usize) -> Result<SchemaU64, StatusProjectionError> {
    u64::try_from(value)
        .map(SchemaU64::new)
        .map_err(|_| StatusProjectionError::NumericOutOfRange { field })
}
