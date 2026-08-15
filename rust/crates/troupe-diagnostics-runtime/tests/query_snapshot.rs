use std::{
    collections::BTreeMap,
    convert::Infallible,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use rusqlite::params;
use troupe_diagnostics_core::{
    detail::{CanonicalInteger, CustomNumber, DiagnosticDimension, PlanEntry},
    event::{
        AffectedElapsedInterval, AgentMessageCompleted, AgentMessageDelta, AgentPlanSnapshot,
        CausalLink, CounterSampled, CustomCounterSampled, DiagnosticEvent, DiagnosticEventHeader,
        DiagnosticEventKind, DiagnosticScope, ObservationGap,
    },
    hub::{
        AcceptedDiagnosticEvent, AdmissionReservation, AdmissionReserver, AdmissionSize,
        DeliveryFailure, EventIdentity, LiveEventNotifier, MandatoryDurableReserver,
        ProductionDiagnosticHub,
    },
    id::{CanonicalUuid, RunLocalId},
    kinds::{CausalRelation, CounterKind, PlanEntryPriority, PlanEntryStatus},
    scalar::SchemaU64,
    time::ElapsedNs,
};
use troupe_diagnostics_runtime::{
    archive::lease::ActiveArchiveLease,
    query::{
        reader::{DiagnosticReader, ReaderFailureClass, ReaderProfile},
        snapshot::{SnapshotQueryErrorCode, SnapshotSynchronization, project_snapshot},
    },
    store::{
        batch::EventBatch,
        connection::{DiagnosticStore, InitialStoreMetadata},
        projector::counters::ProjectedCounterValue,
        writer::TransactionalWriter,
    },
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
const OTHER_RUN_ID: &str = "87654321-4321-4321-8321-cba987654321";
const ARBITRARY_INTEGER: &str = "1234567890123456789012345678901234567890";

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestRunDirectory(PathBuf);

impl TestRunDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "troupe-q03-snapshot-{label}-{}-{sequence}",
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
        &InitialStoreMetadata::new(run_id(), "2026-08-16T00:00:00Z", "configuration-sha256:q03"),
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

fn local_id(value: &str) -> RunLocalId {
    RunLocalId::parse(value).expect("valid Run-local ID")
}

fn act_scope() -> DiagnosticScope {
    DiagnosticScope::new(
        Some(local_id("scene-1")),
        Some(local_id("actor-1")),
        Some(local_id("cue-1")),
        None,
        Some(local_id("act-1")),
        None,
        Some(SchemaU64::new(1)),
    )
}

fn header(
    identity: EventIdentity,
    scope: DiagnosticScope,
    caused_by: Vec<CausalLink>,
) -> DiagnosticEventHeader {
    DiagnosticEventHeader::new(
        identity.run_id(),
        identity.sequence(),
        ElapsedNs::new(identity.sequence().get() * 10),
        scope,
        caused_by,
    )
    .expect("valid event header")
}

fn admit(
    hub: &ProductionDiagnosticHub<AcceptAll>,
    candidate: impl FnOnce(EventIdentity) -> DiagnosticEvent,
) -> AcceptedDiagnosticEvent {
    hub.admit(candidate, None)
        .expect("admit diagnostic event")
        .accepted()
        .clone()
}

fn arbitrary_counter(hub: &ProductionDiagnosticHub<AcceptAll>) -> AcceptedDiagnosticEvent {
    admit(hub, |identity| {
        DiagnosticEvent::CustomCounterSampled(
            CustomCounterSampled::new(
                header(identity, act_scope(), Vec::new()),
                "example.pending".to_owned(),
                CustomNumber::Integer(
                    CanonicalInteger::parse(ARBITRARY_INTEGER)
                        .expect("canonical arbitrary integer"),
                ),
                Some("items".to_owned()),
                BTreeMap::from([(
                    "region".to_owned(),
                    DiagnosticDimension::String("east".to_owned()),
                )]),
            )
            .expect("valid custom counter"),
        )
    })
}

fn max_counter(hub: &ProductionDiagnosticHub<AcceptAll>) -> AcceptedDiagnosticEvent {
    admit(hub, |identity| {
        DiagnosticEvent::CounterSampled(CounterSampled::new(
            header(identity, act_scope(), Vec::new()),
            CounterKind::AgentTurnActive,
            SchemaU64::new(u64::MAX),
        ))
    })
}

fn message_delta(hub: &ProductionDiagnosticHub<AcceptAll>) -> AcceptedDiagnosticEvent {
    admit(hub, |identity| {
        DiagnosticEvent::AgentMessageDelta(AgentMessageDelta::new(
            header(identity, act_scope(), Vec::new()),
            local_id("message-1"),
            None,
            "hello".to_owned(),
        ))
    })
}

fn message_completed(hub: &ProductionDiagnosticHub<AcceptAll>) -> AcceptedDiagnosticEvent {
    admit(hub, |identity| {
        DiagnosticEvent::AgentMessageCompleted(AgentMessageCompleted::new(
            header(identity, act_scope(), Vec::new()),
            local_id("message-1"),
            SchemaU64::new(5),
            SchemaU64::new(5),
            true,
        ))
    })
}

fn plan_snapshot(hub: &ProductionDiagnosticHub<AcceptAll>) -> AcceptedDiagnosticEvent {
    admit(hub, |identity| {
        DiagnosticEvent::AgentPlanSnapshot(AgentPlanSnapshot::new(
            header(identity, act_scope(), Vec::new()),
            vec![PlanEntry::new(
                "inspect".to_owned(),
                PlanEntryPriority::High,
                PlanEntryStatus::InProgress,
            )],
            true,
        ))
    })
}

fn gap(hub: &ProductionDiagnosticHub<AcceptAll>) -> AcceptedDiagnosticEvent {
    admit(hub, |identity| {
        DiagnosticEvent::ObservationGap(ObservationGap::new(
            header(
                identity,
                act_scope(),
                vec![CausalLink::new(
                    SchemaU64::new(2),
                    CausalRelation::FollowsFrom,
                )],
            ),
            "acp-normalizer".to_owned(),
            Some("message-stream".to_owned()),
            "provider_sequence_gap".to_owned(),
            Some(SchemaU64::new(0)),
            Some(AffectedElapsedInterval::new(
                ElapsedNs::new(15),
                ElapsedNs::new(45),
            )),
            Some(DiagnosticEventKind::AgentMessageDelta),
            Some(act_scope()),
        ))
    })
}

fn rich_batch(hub: &ProductionDiagnosticHub<AcceptAll>) -> EventBatch {
    EventBatch::new(vec![
        arbitrary_counter(hub),
        message_delta(hub),
        message_completed(hub),
        plan_snapshot(hub),
        gap(hub),
        max_counter(hub),
    ])
    .expect("valid rich event batch")
}

#[test]
fn empty_snapshot_uses_the_s12_zero_model_without_reading_events() {
    let directory = TestRunDirectory::new("empty");
    let active_lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    let _store = create_store(directory.path());
    let mut reader =
        DiagnosticReader::open_active(run_id(), active_lease.guard()).expect("open active reader");
    let captured = reader.capture().expect("capture empty store");

    let snapshot = project_snapshot(&captured).expect("project empty snapshot");
    assert_eq!(snapshot.run_id(), run_id());
    assert_eq!(snapshot.event_watermark().get(), 0);
    assert_eq!(snapshot.watermark_sequence().get(), 0);
    assert_eq!(snapshot.earliest_available_sequence(), None);
    assert_eq!(
        snapshot.synchronization(),
        SnapshotSynchronization::CaughtUp
    );
    assert_eq!(snapshot.state().through_sequence().get(), 0);
    assert!(snapshot.state().spans().spans().is_empty());
    assert!(snapshot.state().messages().messages().is_empty());
    assert!(snapshot.state().plans().plans().is_empty());
    assert!(snapshot.state().counters().series().is_empty());
    assert!(snapshot.state().gaps().is_empty());
    assert!(snapshot.state().truncations().is_empty());
    assert_eq!(
        snapshot.canonical_state(),
        snapshot
            .state()
            .canonical_json()
            .expect("canonical empty state")
    );
}

#[test]
fn event_watermark_ahead_is_explicitly_classified_without_weakening_capture() {
    let materialized = SchemaU64::new(41);

    assert_eq!(
        SnapshotSynchronization::classify(SchemaU64::new(41), materialized),
        Some(SnapshotSynchronization::CaughtUp)
    );
    assert_eq!(
        SnapshotSynchronization::classify(SchemaU64::new(42), materialized),
        Some(SnapshotSynchronization::EventHeadAhead)
    );
    assert_eq!(
        SnapshotSynchronization::EventHeadAhead.as_str(),
        "event_head_ahead"
    );
    assert_eq!(
        SnapshotSynchronization::classify(SchemaU64::new(40), materialized),
        None
    );
}

#[test]
fn captured_materialized_snapshot_does_not_chase_a_newer_live_event_head() {
    let directory = TestRunDirectory::new("captured-watermark");
    let active_lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    let mut writer =
        TransactionalWriter::new(create_store(directory.path()), ()).expect("construct writer");
    let hub = diagnostic_hub();
    writer
        .commit_batch(&EventBatch::new(vec![arbitrary_counter(&hub)]).expect("first batch"))
        .expect("commit W=1");
    let expected = writer
        .snapshot()
        .canonical_json()
        .expect("captured S12 bytes");
    let mut reader =
        DiagnosticReader::open_active(run_id(), active_lease.guard()).expect("open active reader");
    let captured = reader.capture().expect("capture W=1");

    writer
        .commit_batch(&EventBatch::new(vec![max_counter(&hub)]).expect("second batch"))
        .expect("advance live event head to W=2");
    assert_eq!(writer.watermark().value().get(), 2);

    let snapshot = project_snapshot(&captured).expect("read captured materialized snapshot");
    assert_eq!(snapshot.event_watermark().get(), 1);
    assert_eq!(snapshot.watermark_sequence().get(), 1);
    assert_eq!(
        snapshot.synchronization(),
        SnapshotSynchronization::CaughtUp
    );
    assert_eq!(snapshot.state().through_sequence().get(), 1);
    assert_eq!(
        snapshot.earliest_available_sequence().map(SchemaU64::get),
        Some(1)
    );
    assert_eq!(snapshot.canonical_state(), expected);
    assert_eq!(snapshot.state().counters().series().len(), 1);
}

#[test]
fn gaps_truncations_and_canonical_scalars_are_returned_from_the_stored_model() {
    let directory = TestRunDirectory::new("rich-state");
    let active_lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    let mut writer =
        TransactionalWriter::new(create_store(directory.path()), ()).expect("construct writer");
    let hub = diagnostic_hub();
    writer
        .commit_batch(&rich_batch(&hub))
        .expect("commit rich S12 state");
    let mut reader =
        DiagnosticReader::open_active(run_id(), active_lease.guard()).expect("open active reader");
    let captured = reader.capture().expect("capture rich state");

    let snapshot = project_snapshot(&captured).expect("read rich materialized snapshot");
    assert_eq!(snapshot.watermark_sequence().get(), 6);
    assert_eq!(snapshot.state().gaps().len(), 1);
    assert_eq!(snapshot.state().gaps()[0].header().sequence().get(), 5);
    assert_eq!(
        snapshot.state().gaps()[0]
            .dropped_count()
            .map(SchemaU64::get),
        Some(0)
    );
    assert_eq!(snapshot.state().truncations().len(), 2);
    assert_eq!(snapshot.state().truncations()[0].sequence().get(), 3);
    assert_eq!(snapshot.state().truncations()[1].sequence().get(), 4);

    let values: Vec<_> = snapshot
        .state()
        .counters()
        .series()
        .iter()
        .map(|counter| counter.value())
        .collect();
    assert!(values.iter().any(|value| {
        matches!(value, ProjectedCounterValue::Integer(integer) if integer.as_str() == ARBITRARY_INTEGER)
    }));
    assert!(values.iter().any(|value| {
        matches!(value, ProjectedCounterValue::Unsigned(value) if value.get() == u64::MAX)
    }));

    let bytes = std::str::from_utf8(snapshot.canonical_state()).expect("snapshot is UTF-8 JSON");
    assert!(bytes.contains(ARBITRARY_INTEGER));
    assert!(bytes.contains("18446744073709551615"));
    assert_eq!(
        snapshot.canonical_state(),
        snapshot
            .state()
            .canonical_json()
            .expect("canonical rich state")
    );
}

fn archive_snapshot(
    label: &str,
    ended_at: Option<&str>,
    outcome: Option<&str>,
    clean_shutdown: bool,
) {
    let directory = TestRunDirectory::new(label);
    let active_lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    let mut writer =
        TransactionalWriter::new(create_store(directory.path()), ()).expect("construct writer");
    let hub = diagnostic_hub();
    writer
        .commit_batch(&EventBatch::new(vec![max_counter(&hub)]).expect("archive batch"))
        .expect("commit archive state");
    let store = writer.into_store();
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
    let captured = reader.capture().expect("capture archive snapshot");
    assert_eq!(captured.metadata().production_outcome(), outcome);
    assert_eq!(captured.metadata().clean_shutdown(), clean_shutdown);
    let snapshot = project_snapshot(&captured).expect("failed/incomplete state is query data");
    assert_eq!(snapshot.watermark_sequence().get(), 1);
    assert_eq!(snapshot.state().through_sequence().get(), 1);
}

#[test]
fn failed_and_incomplete_archives_remain_successful_snapshot_queries() {
    archive_snapshot("failed", Some("2026-08-16T00:00:01Z"), Some("failed"), true);
    archive_snapshot("incomplete", None, None, false);
}

fn store_with_one_snapshot(label: &str) -> (TestRunDirectory, ActiveArchiveLease, DiagnosticStore) {
    let directory = TestRunDirectory::new(label);
    let active_lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    let mut writer =
        TransactionalWriter::new(create_store(directory.path()), ()).expect("construct writer");
    let hub = diagnostic_hub();
    writer
        .commit_batch(&EventBatch::new(vec![max_counter(&hub)]).expect("single batch"))
        .expect("commit materialized snapshot");
    (directory, active_lease, writer.into_store())
}

#[test]
fn missing_materialized_state_is_typed_by_reader_profile() {
    let (active_directory, active_lease, active_store) = store_with_one_snapshot("missing-active");
    active_store
        .connection()
        .execute("DELETE FROM materialized_snapshot", [])
        .expect("remove active materialized snapshot");
    let mut active_reader =
        DiagnosticReader::open_active(run_id(), active_lease.guard()).expect("open active reader");
    let active_capture = active_reader
        .capture()
        .expect("capture generic store state");
    let active_error = project_snapshot(&active_capture).expect_err("missing active snapshot");
    assert_eq!(active_error.profile(), ReaderProfile::Active);
    assert_eq!(active_error.class(), ReaderFailureClass::CoreFatal);
    assert_eq!(
        active_error.code(),
        SnapshotQueryErrorCode::MaterializedMissing
    );
    drop(active_capture);
    drop(active_reader);
    drop(active_store);
    drop(active_lease);
    drop(active_directory);

    let (archive_directory, archive_lease, archive_store) =
        store_with_one_snapshot("missing-archive");
    archive_store
        .connection()
        .execute("DELETE FROM materialized_snapshot", [])
        .expect("remove archive materialized snapshot");
    drop(archive_store);
    drop(archive_lease);
    let mut archive_reader = DiagnosticReader::open_archive(archive_directory.path(), run_id())
        .expect("open archive reader");
    let archive_capture = archive_reader
        .capture()
        .expect("capture generic archive state");
    let archive_error = project_snapshot(&archive_capture).expect_err("missing archive snapshot");
    assert_eq!(archive_error.profile(), ReaderProfile::Archive);
    assert_eq!(archive_error.class(), ReaderFailureClass::ArchiveOperation);
    assert_eq!(
        archive_error.code(),
        SnapshotQueryErrorCode::MaterializedMissing
    );
}

#[test]
fn noncanonical_and_wrong_identity_payloads_fail_without_event_replay() {
    let (canonical_directory, canonical_lease, canonical_store) =
        store_with_one_snapshot("noncanonical");
    let payload: Vec<u8> = canonical_store
        .connection()
        .query_row(
            "SELECT payload_json FROM materialized_snapshot WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .expect("read canonical snapshot payload");
    let mut padded = Vec::with_capacity(payload.len() + 1);
    padded.push(b' ');
    padded.extend_from_slice(&payload);
    canonical_store
        .connection()
        .execute(
            "UPDATE materialized_snapshot SET payload_json = ?1 WHERE singleton = 1",
            [padded],
        )
        .expect("store valid but noncanonical JSON");
    let mut canonical_reader = DiagnosticReader::open_active(run_id(), canonical_lease.guard())
        .expect("open canonical reader");
    let canonical_capture = canonical_reader
        .capture()
        .expect("capture generic JSON state");
    assert_eq!(
        project_snapshot(&canonical_capture)
            .expect_err("noncanonical state must fail")
            .code(),
        SnapshotQueryErrorCode::NonCanonicalState
    );
    drop(canonical_capture);
    drop(canonical_reader);
    drop(canonical_store);
    drop(canonical_lease);
    drop(canonical_directory);

    let (_identity_directory, identity_lease, identity_store) = store_with_one_snapshot("identity");
    let payload: Vec<u8> = identity_store
        .connection()
        .query_row(
            "SELECT payload_json FROM materialized_snapshot WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .expect("read identity payload");
    let replaced = std::str::from_utf8(&payload)
        .expect("snapshot JSON")
        .replace(RUN_ID, OTHER_RUN_ID)
        .into_bytes();
    identity_store
        .connection()
        .execute(
            "UPDATE materialized_snapshot SET payload_json = ?1 WHERE singleton = 1",
            [replaced],
        )
        .expect("store another Run identity");
    let mut identity_reader = DiagnosticReader::open_active(run_id(), identity_lease.guard())
        .expect("open identity reader");
    let identity_capture = identity_reader
        .capture()
        .expect("capture generic identity state");
    assert_eq!(
        project_snapshot(&identity_capture)
            .expect_err("snapshot identity drift must fail")
            .code(),
        SnapshotQueryErrorCode::ModelIdentityMismatch
    );
}

#[test]
fn query_source_contains_no_event_pairing_or_replay_path() {
    let source = include_str!("../src/query/snapshot.rs");

    assert!(source.contains("FROM materialized_snapshot WHERE singleton = 1"));
    assert_eq!(source.matches("SnapshotProjector::new").count(), 1);
    assert!(!source.contains("SnapshotProjector::apply"));
    assert!(!source.contains(".events()"));
    assert!(!source.contains("read_event_page"));
    assert!(!source.contains("DiagnosticEvent::"));
}
