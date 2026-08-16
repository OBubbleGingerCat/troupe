use std::{
    convert::Infallible,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
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
    scalar::SchemaU64,
    time::ElapsedNs,
};
use troupe_diagnostics_runtime::{
    archive::lease::{
        ActiveArchiveLease, ArchiveLeaseErrorCode, CleanupArchiveLease, SharedArchiveLease,
    },
    query::reader::{
        CAPTURED_EVENT_PAGE_SIZE, DiagnosticReader, ReaderErrorCode, ReaderFailureClass,
        ReaderProfile,
    },
    store::{
        batch::EventBatch,
        connection::{DiagnosticStore, InitialStoreMetadata, StoreOpenErrorCode},
        key::SortableU64Key,
        schema::DIAGNOSTIC_DATABASE_FILENAME,
        writer::{TransactionalWriter, WriterErrorCode, WriterTransactionHook},
    },
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
const OTHER_RUN_ID: &str = "87654321-4321-4321-8321-cba987654321";

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestRunDirectory(PathBuf);

impl TestRunDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "troupe-q00-reader-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create test Run directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn database_path(&self) -> PathBuf {
        self.0.join(DIAGNOSTIC_DATABASE_FILENAME)
    }

    fn sqlite_sidecars(&self) -> [PathBuf; 2] {
        [
            self.0.join(format!("{DIAGNOSTIC_DATABASE_FILENAME}-wal")),
            self.0.join(format!("{DIAGNOSTIC_DATABASE_FILENAME}-shm")),
        ]
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

fn other_run_id() -> CanonicalUuid {
    CanonicalUuid::parse(OTHER_RUN_ID).expect("canonical alternate Run UUID")
}

fn create_store(directory: &Path) -> DiagnosticStore {
    DiagnosticStore::create(
        directory,
        &InitialStoreMetadata::new(run_id(), "2026-08-15T00:00:00Z", "configuration-sha256:q00"),
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

fn counter_event(identity: EventIdentity) -> DiagnosticEvent {
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
}

fn accept_counter(hub: &ProductionDiagnosticHub<AcceptAll>) -> AcceptedDiagnosticEvent {
    hub.admit(counter_event, None)
        .expect("admit counter event")
        .accepted()
        .clone()
}

fn one_event_batch(hub: &ProductionDiagnosticHub<AcceptAll>) -> EventBatch {
    EventBatch::new(vec![accept_counter(hub)]).expect("one-event batch")
}

#[test]
fn active_reader_borrows_the_runtime_guard_and_uses_an_independent_connection() {
    let directory = TestRunDirectory::new("active-guard");
    let active_lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    let store = create_store(directory.path());

    let mut reader = DiagnosticReader::open_active(run_id(), active_lease.guard())
        .expect("open active reader while exclusive lease is held");
    assert_eq!(reader.profile(), ReaderProfile::Active);
    assert_eq!(reader.run_directory(), directory.path());
    assert_eq!(reader.database_path(), store.database_path());
    assert_eq!(
        SharedArchiveLease::acquire(directory.path())
            .expect_err("a reacquired shared lease would contend")
            .code(),
        ArchiveLeaseErrorCode::Contended
    );

    let captured = reader.capture().expect("capture empty active prefix");
    assert_eq!(captured.profile(), ReaderProfile::Active);
    assert_eq!(captured.captured_watermark().get(), 0);
    assert_eq!(captured.metadata().run_id(), run_id());
    assert_eq!(captured.metadata().started_at(), "2026-08-15T00:00:00Z");
    assert_eq!(
        captured.metadata().configuration_identity(),
        "configuration-sha256:q00"
    );
    assert!(!captured.metadata().clean_shutdown());
    assert!(captured.events().next().is_none());
    drop(captured);
    drop(reader);

    assert_eq!(
        CleanupArchiveLease::acquire(directory.path())
            .expect_err("dropping the reader must not release the Runtime guard")
            .code(),
        ArchiveLeaseErrorCode::Contended
    );
    drop(store);
    drop(active_lease);
    CleanupArchiveLease::acquire(directory.path()).expect("Runtime lease drop releases lock");
}

#[test]
fn archive_reader_holds_and_releases_its_request_owned_shared_lease() {
    let directory = TestRunDirectory::new("archive-lease");
    let active_lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    drop(create_store(directory.path()));
    drop(active_lease);
    assert!(
        directory
            .sqlite_sidecars()
            .iter()
            .all(|path| !path.exists())
    );

    let mut reader = DiagnosticReader::open_archive(directory.path(), run_id())
        .expect("open inactive archive reader");
    assert_eq!(reader.profile(), ReaderProfile::Archive);
    assert_eq!(
        CleanupArchiveLease::acquire(directory.path())
            .expect_err("reader-owned shared lease blocks cleanup")
            .code(),
        ArchiveLeaseErrorCode::Contended
    );
    let captured = reader.capture().expect("capture archive prefix");
    assert_eq!(captured.profile(), ReaderProfile::Archive);
    drop(captured);
    drop(reader);
    assert!(
        directory
            .sqlite_sidecars()
            .iter()
            .all(|path| !path.exists())
    );

    CleanupArchiveLease::acquire(directory.path())
        .expect("dropping archive reader releases its shared lease");
}

#[test]
fn identified_archive_reader_uses_the_stored_run_identity_and_holds_its_lease() {
    let directory = TestRunDirectory::new("identified-copied-archive");
    let active_lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    drop(create_store(directory.path()));
    drop(active_lease);

    let mut reader = DiagnosticReader::open_identified_archive(directory.path())
        .expect("identify an archive whose directory name is unrelated to its Run identity");
    assert_eq!(reader.profile(), ReaderProfile::Archive);
    assert_eq!(reader.run_directory(), directory.path());
    assert_eq!(
        CleanupArchiveLease::acquire(directory.path())
            .expect_err("identified reader-owned shared lease blocks cleanup")
            .code(),
        ArchiveLeaseErrorCode::Contended
    );

    let captured = reader.capture().expect("capture the identified archive");
    assert_eq!(captured.metadata().run_id(), run_id());
    assert_eq!(captured.captured_watermark().get(), 0);
    drop(captured);
    drop(reader);

    CleanupArchiveLease::acquire(directory.path())
        .expect("dropping identified reader releases its shared lease");
}

#[test]
fn a_capture_keeps_one_stable_typed_prefix_across_concurrent_commits() {
    let directory = TestRunDirectory::new("stable-prefix");
    let active_lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    let mut writer =
        TransactionalWriter::new(create_store(directory.path()), ()).expect("construct writer");
    let hub = diagnostic_hub();
    let first_batch = one_event_batch(&hub);
    let first_bytes = first_batch.events()[0].canonical_bytes().to_vec();
    writer
        .commit_batch(&first_batch)
        .expect("commit first event");
    let mut reader =
        DiagnosticReader::open_active(run_id(), active_lease.guard()).expect("open active reader");

    let captured = reader.capture().expect("capture W=1");
    assert_eq!(captured.captured_watermark().get(), 1);
    let second_batch = one_event_batch(&hub);
    writer
        .commit_batch(&second_batch)
        .expect("WAL writer commits while read transaction remains open");
    assert_eq!(writer.watermark().value().get(), 2);

    let events = captured
        .events()
        .collect::<Result<Vec<_>, _>>()
        .expect("iterate captured typed prefix");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sequence().get(), 1);
    assert_eq!(events[0].canonical_bytes(), first_bytes);
    assert!(matches!(
        events[0].event(),
        DiagnosticEvent::CounterSampled(_)
    ));
    let page = captured
        .read_event_page(SchemaU64::new(0))
        .expect("page captured prefix");
    assert_eq!(page.events(), events);
    assert_eq!(page.next_after(), None);
    drop(captured);

    let next_capture = reader.capture().expect("later capture observes W=2");
    assert_eq!(next_capture.captured_watermark().get(), 2);
    assert_eq!(next_capture.events().count(), 2);
}

struct ObserveBeforeRollback<'reader, 'lease> {
    reader: &'reader mut DiagnosticReader<'lease>,
}

impl WriterTransactionHook for ObserveBeforeRollback<'_, '_> {
    fn before_commit(&mut self, _transaction: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
        let captured = self
            .reader
            .capture()
            .expect("independent reader captures during writer transaction");
        assert_eq!(captured.captured_watermark().get(), 0);
        assert!(captured.events().next().is_none());
        Err(rusqlite::Error::InvalidQuery)
    }
}

#[test]
fn an_uncommitted_writer_tail_is_never_visible_to_a_capture() {
    let directory = TestRunDirectory::new("uncommitted-tail");
    let active_lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    let mut writer =
        TransactionalWriter::new(create_store(directory.path()), ()).expect("construct writer");
    let hub = diagnostic_hub();
    let batch = one_event_batch(&hub);
    let mut reader =
        DiagnosticReader::open_active(run_id(), active_lease.guard()).expect("open active reader");
    let mut hook = ObserveBeforeRollback {
        reader: &mut reader,
    };

    let error = writer
        .commit_batch_with_hook(&batch, &mut hook)
        .expect_err("abort after observing the uncommitted transaction");
    assert_eq!(error.code(), WriterErrorCode::BeforeCommit);
    assert_eq!(writer.watermark().value().get(), 0);
    assert_eq!(
        reader
            .capture()
            .expect("capture after rollback")
            .captured_watermark()
            .get(),
        0
    );

    writer.commit_batch(&batch).expect("retry committed tail");
    assert_eq!(
        reader
            .capture()
            .expect("capture after commit")
            .captured_watermark()
            .get(),
        1
    );
}

#[test]
fn event_pages_are_fixed_size_and_do_not_scale_with_the_captured_run() {
    let directory = TestRunDirectory::new("bounded-pages");
    let active_lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    let mut writer =
        TransactionalWriter::new(create_store(directory.path()), ()).expect("construct writer");
    let hub = diagnostic_hub();
    let first_events = (0..CAPTURED_EVENT_PAGE_SIZE)
        .map(|_| accept_counter(&hub))
        .collect();
    writer
        .commit_batch(&EventBatch::new(first_events).expect("full-size event batch"))
        .expect("commit first page");
    writer
        .commit_batch(&one_event_batch(&hub))
        .expect("commit second page");
    let mut reader =
        DiagnosticReader::open_active(run_id(), active_lease.guard()).expect("open active reader");
    let captured = reader.capture().expect("capture 513-event prefix");

    let first = captured
        .read_event_page(SchemaU64::new(0))
        .expect("read bounded first page");
    assert_eq!(first.events().len(), CAPTURED_EVENT_PAGE_SIZE);
    assert_eq!(first.events()[0].sequence().get(), 1);
    assert_eq!(
        first.next_after(),
        Some(SchemaU64::new(CAPTURED_EVENT_PAGE_SIZE as u64))
    );
    let second = captured
        .read_event_page(first.next_after().expect("continuation sequence"))
        .expect("read final page");
    assert_eq!(second.events().len(), 1);
    assert_eq!(second.events()[0].sequence().get(), 513);
    assert_eq!(second.next_after(), None);
}

fn assert_active_store_failure(mut reader: DiagnosticReader<'_>, expected: StoreOpenErrorCode) {
    let error = reader.capture().expect_err("active capture must fail");
    assert_eq!(error.class(), ReaderFailureClass::CoreFatal);
    assert_eq!(error.profile(), ReaderProfile::Active);
    assert_eq!(error.code(), ReaderErrorCode::StoreValidation);
    assert_eq!(error.store_code(), Some(expected));
}

#[test]
fn active_identity_dense_prefix_and_corruption_failures_are_core_fatal() {
    let identity_directory = TestRunDirectory::new("active-identity");
    let identity_lease =
        ActiveArchiveLease::acquire(identity_directory.path()).expect("identity active lease");
    let identity_store = create_store(identity_directory.path());
    assert_active_store_failure(
        DiagnosticReader::open_active(other_run_id(), identity_lease.guard())
            .expect("open identity reader"),
        StoreOpenErrorCode::RunIdentityMismatch,
    );
    drop(identity_store);

    let dense_directory = TestRunDirectory::new("active-dense");
    let dense_lease =
        ActiveArchiveLease::acquire(dense_directory.path()).expect("dense active lease");
    let dense_store = create_store(dense_directory.path());
    let one = SortableU64Key::new(1);
    dense_store
        .connection()
        .execute(
            "UPDATE run_metadata SET committed_key = ?1, committed_sequence = '1', \
             read_model_key = ?1, read_model_sequence = '1' WHERE singleton = 1",
            params![one.as_bytes().as_slice()],
        )
        .expect("create impossible committed head");
    drop(dense_store);
    assert_active_store_failure(
        DiagnosticReader::open_active(run_id(), dense_lease.guard()).expect("open dense reader"),
        StoreOpenErrorCode::DensePrefixViolation,
    );

    let corrupt_directory = TestRunDirectory::new("active-corrupt");
    let corrupt_lease =
        ActiveArchiveLease::acquire(corrupt_directory.path()).expect("corrupt active lease");
    drop(create_store(corrupt_directory.path()));
    fs::write(corrupt_directory.database_path(), b"not a sqlite database")
        .expect("corrupt database bytes");
    assert_active_store_failure(
        DiagnosticReader::open_active(run_id(), corrupt_lease.guard())
            .expect("open lazy SQLite connection"),
        StoreOpenErrorCode::CorruptStore,
    );
}

fn close_active_store(directory: &TestRunDirectory) {
    let active_lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    drop(create_store(directory.path()));
    drop(active_lease);
}

fn assert_archive_store_failure(directory: &TestRunDirectory, expected: StoreOpenErrorCode) {
    let mut reader = DiagnosticReader::open_archive(directory.path(), run_id())
        .expect("archive lease and lazy read connection succeed");
    let error = reader.capture().expect_err("archive capture must fail");
    assert_eq!(error.class(), ReaderFailureClass::ArchiveOperation);
    assert_eq!(error.profile(), ReaderProfile::Archive);
    assert_eq!(error.code(), ReaderErrorCode::StoreValidation);
    assert_eq!(error.store_code(), Some(expected));
}

#[test]
fn archive_lease_schema_corruption_and_identity_failures_stay_operation_local() {
    let missing_anchor = TestRunDirectory::new("archive-missing-anchor");
    let error = DiagnosticReader::open_archive(missing_anchor.path(), run_id())
        .expect_err("archive reader requires a shared lease");
    assert_eq!(error.class(), ReaderFailureClass::ArchiveOperation);
    assert_eq!(error.code(), ReaderErrorCode::ArchiveLease);
    assert_eq!(
        error.lease_code(),
        Some(ArchiveLeaseErrorCode::AnchorOpenFailed)
    );

    let newer_directory = TestRunDirectory::new("archive-newer");
    close_active_store(&newer_directory);
    rusqlite::Connection::open(newer_directory.database_path())
        .expect("open newer store")
        .pragma_update(None, "user_version", 2_u32)
        .expect("set newer schema version");
    assert_archive_store_failure(&newer_directory, StoreOpenErrorCode::NewerSchema);

    let identity_directory = TestRunDirectory::new("archive-identity");
    close_active_store(&identity_directory);
    let mut identity_reader =
        DiagnosticReader::open_archive(identity_directory.path(), other_run_id())
            .expect("open archive reader");
    let identity_error = identity_reader
        .capture()
        .expect_err("reject archive Run identity mismatch");
    assert_eq!(identity_error.class(), ReaderFailureClass::ArchiveOperation);
    assert_eq!(
        identity_error.store_code(),
        Some(StoreOpenErrorCode::RunIdentityMismatch)
    );

    let corrupt_directory = TestRunDirectory::new("archive-corrupt");
    close_active_store(&corrupt_directory);
    fs::write(corrupt_directory.database_path(), b"not a sqlite database")
        .expect("corrupt archive bytes");
    assert_archive_store_failure(&corrupt_directory, StoreOpenErrorCode::CorruptStore);
}

#[test]
fn structurally_healthy_incomplete_and_failed_archives_remain_readable() {
    let incomplete_directory = TestRunDirectory::new("archive-incomplete");
    close_active_store(&incomplete_directory);
    let mut incomplete_reader =
        DiagnosticReader::open_archive(incomplete_directory.path(), run_id())
            .expect("open incomplete archive");
    let incomplete = incomplete_reader
        .capture()
        .expect("capture incomplete archive");
    assert!(!incomplete.metadata().clean_shutdown());
    assert_eq!(incomplete.metadata().ended_at(), None);
    assert_eq!(incomplete.metadata().production_outcome(), None);

    let failed_directory = TestRunDirectory::new("archive-failed");
    let active_lease =
        ActiveArchiveLease::acquire(failed_directory.path()).expect("failed active lease");
    let failed_store = create_store(failed_directory.path());
    failed_store
        .connection()
        .execute(
            "UPDATE run_metadata SET ended_at = ?1, production_outcome = 'failed', \
             clean_shutdown = 1 WHERE singleton = 1",
            ["2026-08-15T00:00:01Z"],
        )
        .expect("record failed but clean Production outcome");
    drop(failed_store);
    drop(active_lease);
    let mut failed_reader = DiagnosticReader::open_archive(failed_directory.path(), run_id())
        .expect("open failed archive");
    let failed = failed_reader.capture().expect("capture failed archive");
    assert!(failed.metadata().clean_shutdown());
    assert_eq!(failed.metadata().ended_at(), Some("2026-08-15T00:00:01Z"));
    assert_eq!(failed.metadata().production_outcome(), Some("failed"));
}
