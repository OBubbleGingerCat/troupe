use std::{
    convert::Infallible,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use rusqlite::params;
use troupe_diagnostics_core::{
    event::{CounterSampled, DiagnosticEvent, DiagnosticEventHeader, DiagnosticScope},
    hub::{
        AcceptedDiagnosticEvent, AdmissionReservation, AdmissionReserver, AdmissionSize,
        DeliveryFailure, EventIdentity, LiveEventNotifier, MandatoryDurableReserver,
        ProductionDiagnosticHub,
    },
    id::CanonicalUuid,
    kinds::CounterKind,
    time::ElapsedNs,
};
use troupe_diagnostics_runtime::{
    archive::lease::ActiveArchiveLease,
    query::{
        reader::DiagnosticReader,
        status::{
            ActiveStatusObservation, DiagnosticStatus, Observation, ProductionOutcome,
            ProductionState, StatusSource, UnavailableReason, WriterDrainState, project_status,
        },
    },
    store::{
        admission::MandatoryIngress,
        batch::{EventBatch, MAX_BATCH_AGE, MAX_BATCH_CANONICAL_BYTES, MAX_BATCH_EVENTS},
        connection::{DiagnosticStore, InitialStoreMetadata},
        progress::{
            WriterDeadlines, WriterProgressSample, WriterProgressSupervisor, WriterTaskOutcome,
        },
        quota::{QuotaError, RunQuota},
        watermark::CommittedWatermark,
        writer::TransactionalWriter,
    },
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestRunDirectory(PathBuf);

impl TestRunDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "troupe-q02-status-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create test Run directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRunDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_id() -> CanonicalUuid {
    CanonicalUuid::parse(RUN_ID).expect("canonical test Run UUID")
}

fn create_store(directory: &Path) -> DiagnosticStore {
    DiagnosticStore::create(
        directory,
        &InitialStoreMetadata::new(run_id(), "2026-08-16T00:00:00Z", "configuration-sha256:q02"),
    )
    .expect("create diagnostic store")
}

#[derive(Clone, Copy, Debug, Default)]
struct AcceptAll;

#[derive(Debug)]
struct AcceptedReservation;

impl AdmissionReservation for AcceptedReservation {
    fn commit(self, _event: AcceptedDiagnosticEvent) {}
}

impl AdmissionReserver for AcceptAll {
    type Error = Infallible;
    type Reservation = AcceptedReservation;

    fn try_reserve(&mut self, _size: AdmissionSize) -> Result<Self::Reservation, Self::Error> {
        Ok(AcceptedReservation)
    }
}

impl MandatoryDurableReserver for AcceptAll {}

#[derive(Debug)]
struct IgnoreLive;

impl LiveEventNotifier for IgnoreLive {
    fn notify(&mut self, _event: AcceptedDiagnosticEvent) -> Result<(), DeliveryFailure> {
        Ok(())
    }
}

fn diagnostic_hub() -> ProductionDiagnosticHub<AcceptAll> {
    ProductionDiagnosticHub::production(run_id(), AcceptAll, Box::new(IgnoreLive))
}

fn one_event_batch(hub: &ProductionDiagnosticHub<AcceptAll>) -> EventBatch {
    let admitted = hub
        .admit(
            |identity: EventIdentity| {
                let sequence = identity.sequence();
                DiagnosticEvent::CounterSampled(CounterSampled::new(
                    DiagnosticEventHeader::new(
                        identity.run_id(),
                        sequence,
                        ElapsedNs::new(sequence.get()),
                        DiagnosticScope::new(None, None, None, None, None, None, None),
                        Vec::new(),
                    )
                    .expect("valid test event header"),
                    CounterKind::DiagnosticDroppedEvents,
                    sequence,
                ))
            },
            None,
        )
        .expect("admit counter event");
    EventBatch::new(vec![admitted.accepted().clone()]).expect("one-event batch")
}

fn archive_status(
    label: &str,
    ended_at: Option<&str>,
    outcome: Option<&str>,
    clean_shutdown: bool,
) -> DiagnosticStatus {
    let directory = TestRunDirectory::new(label);
    let active_lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    let store = create_store(directory.path());
    store
        .connection()
        .execute(
            "UPDATE run_metadata SET ended_at = ?1, production_outcome = ?2, \
             clean_shutdown = ?3 WHERE singleton = 1",
            params![ended_at, outcome, i64::from(clean_shutdown)],
        )
        .expect("set archive lifecycle metadata");
    drop(store);
    drop(active_lease);

    let mut reader =
        DiagnosticReader::open_archive(directory.path(), run_id()).expect("open archive reader");
    let captured = reader.capture().expect("capture archive metadata");
    project_status(&captured, None).expect("project archive status")
}

#[test]
fn active_status_projects_captured_identity_and_healthy_live_observations() {
    let directory = TestRunDirectory::new("active");
    let active_lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    let _store = create_store(directory.path());
    let mut reader =
        DiagnosticReader::open_active(run_id(), active_lease.guard()).expect("open active reader");
    let captured = reader.capture().expect("capture active metadata");
    let (ingress, _) = MandatoryIngress::new();
    let progress = WriterProgressSupervisor::default();
    let (quota, _) = RunQuota::new(directory.path(), None).expect("disabled quota");
    let live = ActiveStatusObservation::available(
        ingress.status().unwrap(),
        progress.status(),
        quota.status().unwrap(),
    );

    let status = project_status(&captured, Some(&live)).expect("project active status");

    assert_eq!(status.identity().run_id(), run_id());
    assert_eq!(status.identity().source(), StatusSource::Active);
    assert_eq!(status.identity().store_schema_version().get(), 1);
    assert_eq!(status.identity().event_schema_version().get(), 1);
    assert_eq!(
        status.identity().store_schema_identity(),
        "troupe.diagnostics.store.v1"
    );
    assert_eq!(status.lifecycle().state(), ProductionState::Active);
    assert_eq!(status.lifecycle().started_at(), "2026-08-16T00:00:00Z");
    assert_eq!(status.lifecycle().ended_at(), None);
    assert_eq!(status.lifecycle().outcome(), None);
    assert!(!status.lifecycle().clean_shutdown());
    assert_eq!(status.configuration_identity(), "configuration-sha256:q02");
    assert_eq!(status.event_watermark().get(), 0);
    assert_eq!(status.read_model_watermark().get(), 0);
    let Observation::Available(writer) = status.writer() else {
        panic!("active writer status must be available")
    };
    assert_eq!(writer.max_uncommitted_events().get(), 32_768);
    assert_eq!(
        writer.max_uncommitted_canonical_bytes().get(),
        64 * 1024 * 1024
    );
    assert_eq!(writer.max_batch_events().get(), MAX_BATCH_EVENTS as u64);
    assert_eq!(
        writer.max_batch_canonical_bytes().get(),
        MAX_BATCH_CANONICAL_BYTES as u64
    );
    assert_eq!(
        writer.max_batch_age().seconds().get(),
        MAX_BATCH_AGE.as_secs()
    );
    assert_eq!(
        writer.max_batch_age().subsecond_nanoseconds().get(),
        u64::from(MAX_BATCH_AGE.subsec_nanos())
    );
    assert_eq!(writer.accepted_uncommitted_events().get(), 0);
    assert_eq!(writer.accepted_uncommitted_canonical_bytes().get(), 0);
    assert_eq!(writer.queued_events().get(), 0);
    assert_eq!(writer.in_flight_events().get(), 0);
    assert_eq!(writer.ingress_committed_watermark().get(), 0);
    assert_eq!(writer.progress_committed_watermark().get(), 0);
    assert_eq!(writer.accepted_tail_events().get(), 0);
    assert_eq!(writer.drain_state(), WriterDrainState::NotStarted);
    assert_eq!(writer.ingress_failure(), None);
    assert_eq!(writer.progress_failure(), None);

    let Observation::Available(quota) = status.quota() else {
        panic!("active quota status must be available")
    };
    assert_eq!(quota.max_run_bytes(), None);
    assert_eq!(quota.current_measured_bytes(), None);
    assert_eq!(quota.last_measurement_at(), None);
    assert!(!quota.sealed());
    assert_eq!(quota.failure(), None);
}

#[test]
fn archive_lifecycle_and_live_only_availability_are_exact() {
    let completed = archive_status(
        "completed",
        Some("2026-08-16T00:00:01Z"),
        Some("completed"),
        true,
    );
    assert_eq!(completed.identity().source(), StatusSource::Archive);
    assert_eq!(completed.lifecycle().state(), ProductionState::Completed);
    assert_eq!(
        completed.lifecycle().outcome(),
        Some(ProductionOutcome::Completed)
    );
    assert_eq!(
        completed.writer().unavailable_reason(),
        Some(UnavailableReason::Archive)
    );
    assert_eq!(
        completed.quota().unavailable_reason(),
        Some(UnavailableReason::Archive)
    );

    for (label, outcome, expected_outcome) in [
        ("failed", "failed", ProductionOutcome::Failed),
        ("cancelled", "cancelled", ProductionOutcome::Cancelled),
    ] {
        let failed = archive_status(label, Some("2026-08-16T00:00:01Z"), Some(outcome), true);
        assert_eq!(failed.lifecycle().state(), ProductionState::Failed);
        assert_eq!(failed.lifecycle().outcome(), Some(expected_outcome));
        assert!(failed.lifecycle().clean_shutdown());
    }

    let incomplete = archive_status("incomplete", None, None, false);
    assert_eq!(incomplete.lifecycle().state(), ProductionState::Incomplete);
    assert_eq!(incomplete.lifecycle().outcome(), None);
    assert!(!incomplete.lifecycle().clean_shutdown());

    let inconsistent = archive_status(
        "inconsistent",
        Some("2026-08-16T00:00:01Z"),
        Some("completed"),
        false,
    );
    assert_eq!(
        inconsistent.lifecycle().state(),
        ProductionState::Incomplete
    );
}

#[test]
fn active_live_state_absence_is_explicit_and_component_local() {
    let directory = TestRunDirectory::new("unavailable");
    let active_lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    let _store = create_store(directory.path());
    let mut reader =
        DiagnosticReader::open_active(run_id(), active_lease.guard()).expect("open active reader");
    let captured = reader.capture().expect("capture active metadata");

    let not_observed = project_status(&captured, None).expect("project unobserved status");
    assert_eq!(
        not_observed.writer().unavailable_reason(),
        Some(UnavailableReason::NotObserved)
    );
    assert_eq!(
        not_observed.quota().unavailable_reason(),
        Some(UnavailableReason::NotObserved)
    );

    let (quota, _) = RunQuota::new(directory.path(), None).expect("disabled quota");
    let partial = ActiveStatusObservation::new(
        Observation::unavailable(UnavailableReason::StateUnavailable),
        Observation::available(quota.status().expect("quota status")),
    );
    let status = project_status(&captured, Some(&partial)).expect("project partial status");
    assert_eq!(
        status.writer().unavailable_reason(),
        Some(UnavailableReason::StateUnavailable)
    );
    assert!(matches!(status.quota(), Observation::Available(_)));
}

#[test]
fn canonical_numeric_projection_preserves_u64_and_subsecond_boundaries() {
    let directory = TestRunDirectory::new("numeric-boundaries");
    let active_lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    let _store = create_store(directory.path());
    let mut reader =
        DiagnosticReader::open_active(run_id(), active_lease.guard()).expect("open active reader");
    let captured = reader.capture().expect("capture active metadata");
    let (ingress, _) = MandatoryIngress::new();

    let deadlines = WriterDeadlines::new(Duration::new(3, 456_789), Duration::new(7, 987_654_321))
        .expect("valid exact deadlines");
    let mut progress = WriterProgressSupervisor::new(deadlines);
    progress
        .observe(
            Duration::from_nanos(1),
            WriterProgressSample::new(u64::MAX, usize::MAX),
        )
        .expect("observe maximum progress values");

    let (quota, _) = RunQuota::new(directory.path(), Some(u64::MAX)).expect("maximum quota");
    quota
        .precheck(Duration::new(u64::MAX, 999_999_999), 0)
        .expect("measure below maximum quota");
    let live = ActiveStatusObservation::available(
        ingress.status().expect("ingress status"),
        progress.status(),
        quota.status().expect("quota status"),
    );

    let status = project_status(&captured, Some(&live)).expect("project boundary status");
    let writer = status.writer().as_available().expect("writer status");
    assert_eq!(writer.progress_committed_watermark().get(), u64::MAX);
    assert_eq!(
        writer.accepted_tail_events().get(),
        u64::try_from(usize::MAX).expect("usize belongs to canonical domain")
    );
    assert_eq!(writer.writer_stall_timeout().seconds().get(), 3);
    assert_eq!(
        writer.writer_stall_timeout().subsecond_nanoseconds().get(),
        456_789
    );
    assert_eq!(writer.shutdown_drain_timeout().seconds().get(), 7);
    assert_eq!(
        writer
            .shutdown_drain_timeout()
            .subsecond_nanoseconds()
            .get(),
        987_654_321
    );

    let quota = status.quota().as_available().expect("quota status");
    assert_eq!(
        quota.max_run_bytes().expect("configured quota").get(),
        u64::MAX
    );
    let measured_at = quota.last_measurement_at().expect("measurement timestamp");
    assert_eq!(measured_at.seconds().get(), u64::MAX);
    assert_eq!(measured_at.subsecond_nanoseconds().get(), 999_999_999);
}

#[test]
fn observed_writer_and_quota_failures_are_successful_status_data() {
    let directory = TestRunDirectory::new("observed-failures");
    let active_lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    let _store = create_store(directory.path());
    let mut reader =
        DiagnosticReader::open_active(run_id(), active_lease.guard()).expect("open active reader");
    let captured = reader.capture().expect("capture active metadata");
    let (ingress, _) = MandatoryIngress::new();

    let ingress_notification = CommittedWatermark::fresh(run_id())
        .candidate(&one_event_batch(&diagnostic_hub()))
        .expect("valid commit notification");
    ingress
        .mark_committed(ingress_notification)
        .expect_err("unaccepted commit must latch ingress failure");

    let mut progress = WriterProgressSupervisor::default();
    progress
        .report_writer_outcome(
            Duration::ZERO,
            WriterProgressSample::new(0, 0),
            WriterTaskOutcome::StorageUnavailable,
        )
        .expect("valid writer observation")
        .expect("writer failure is newly latched");

    let (quota, _) = RunQuota::new(directory.path(), Some(1)).expect("one-byte quota");
    assert!(matches!(
        quota.precheck(Duration::from_nanos(9), 0),
        Err(QuotaError::LimitReached(_))
    ));
    let live = ActiveStatusObservation::available(
        ingress.status().expect("ingress status"),
        progress.status(),
        quota.status().expect("quota status"),
    );

    let status = project_status(&captured, Some(&live))
        .expect("observed component failure is not a query failure");
    let writer = status.writer().as_available().expect("writer status");
    let ingress_failure = writer.ingress_failure().expect("ingress failure");
    assert_eq!(
        ingress_failure.code(),
        "mandatory_ingress_commit_accounting"
    );
    assert_eq!(ingress_failure.current_events().get(), 0);
    assert_eq!(ingress_failure.current_canonical_bytes().get(), 0);
    assert_eq!(ingress_failure.attempted_events().get(), 0);
    assert_eq!(ingress_failure.attempted_canonical_bytes().get(), 0);
    assert!(!ingress_failure.event_limit_exceeded());
    assert!(!ingress_failure.byte_limit_exceeded());

    let writer_failure = writer.progress_failure().expect("writer failure");
    assert_eq!(writer_failure.component(), "writer");
    assert_eq!(writer_failure.stage(), "storage");
    assert_eq!(writer_failure.code(), "writer_storage_unavailable");

    let quota = status.quota().as_available().expect("quota status");
    assert!(quota.sealed());
    let quota_failure = quota.failure().expect("quota failure");
    assert_eq!(quota_failure.code(), "run_quota_measured_limit_reached");
    assert_eq!(quota_failure.limit_bytes().get(), 1);
    assert!(quota_failure.current_bytes().is_some());
    assert_eq!(quota_failure.predicted_growth_bytes(), None);
}

#[test]
fn status_uses_the_captured_watermarks_without_reading_or_advancing_the_prefix() {
    let directory = TestRunDirectory::new("stable-watermark");
    let active_lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    let mut writer =
        TransactionalWriter::new(create_store(directory.path()), ()).expect("construct writer");
    let hub = diagnostic_hub();
    writer
        .commit_batch(&one_event_batch(&hub))
        .expect("commit first event");
    let mut reader =
        DiagnosticReader::open_active(run_id(), active_lease.guard()).expect("open active reader");
    let captured = reader.capture().expect("capture W=1");

    writer
        .commit_batch(&one_event_batch(&hub))
        .expect("commit second event after capture");
    assert_eq!(writer.watermark().value().get(), 2);

    let first = project_status(&captured, None).expect("first status projection");
    let second = project_status(&captured, None).expect("repeat status projection");
    assert_eq!(first.event_watermark().get(), 1);
    assert_eq!(first.read_model_watermark().get(), 1);
    assert_eq!(second.event_watermark().get(), 1);
    assert_eq!(second.read_model_watermark().get(), 1);
    assert_eq!(captured.captured_watermark().get(), 1);
}
