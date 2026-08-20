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
    archive::lease::ActiveArchiveLease,
    query::{
        events::{CapturedEventRange, FiniteEventQuery, query_events},
        reader::{DiagnosticReader, ReaderErrorCode, ReaderFailureClass},
    },
    store::{
        batch::{EventBatch, MAX_BATCH_EVENTS},
        connection::{DiagnosticStore, InitialStoreMetadata, StoreOpenErrorCode},
        key::SortableU64Key,
        schema::DIAGNOSTIC_DATABASE_FILENAME,
        writer::TransactionalWriter,
    },
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestRunDirectory(PathBuf);

impl TestRunDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "troupe-q04-events-{label}-{}-{sequence}",
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
        &InitialStoreMetadata::new(run_id(), "2026-08-16T00:00:00Z", "configuration-sha256:q04"),
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

fn commit_counters(
    writer: &mut TransactionalWriter<()>,
    hub: &ProductionDiagnosticHub<AcceptAll>,
    count: usize,
) -> Vec<Vec<u8>> {
    let events = (0..count).map(|_| accept_counter(hub)).collect::<Vec<_>>();
    let canonical = events
        .iter()
        .map(|event| event.canonical_bytes().to_vec())
        .collect::<Vec<_>>();
    for chunk in events.chunks(MAX_BATCH_EVENTS) {
        writer
            .commit_batch(&EventBatch::new(chunk.to_vec()).expect("nonempty event batch"))
            .expect("commit diagnostic event batch");
    }
    canonical
}

fn sequences(
    iterator: impl Iterator<
        Item = Result<
            troupe_diagnostics_runtime::query::reader::CapturedEvent,
            troupe_diagnostics_runtime::query::events::EventQueryError,
        >,
    >,
) -> Vec<u64> {
    iterator
        .map(|event| event.expect("valid finite event").sequence().get())
        .collect()
}

fn assert_range(
    range: CapturedEventRange,
    watermark: u64,
    after: u64,
    through: u64,
    first: Option<u64>,
    last: Option<u64>,
    count: u64,
) {
    assert_eq!(range.captured_watermark().get(), watermark);
    assert_eq!(range.after_exclusive().get(), after);
    assert_eq!(range.through_inclusive().get(), through);
    assert_eq!(range.first_sequence().map(SchemaU64::get), first);
    assert_eq!(range.last_sequence().map(SchemaU64::get), last);
    assert_eq!(range.event_count().get(), count);
    assert_eq!(range.is_empty(), count == 0);
}

fn remove_event_write_guards(connection: &rusqlite::Connection) -> (String, String) {
    let update = connection
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type = 'trigger' AND name = 'events_no_update'",
            [],
            |row| row.get(0),
        )
        .expect("read update guard definition");
    let delete = connection
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type = 'trigger' AND name = 'events_no_delete'",
            [],
            |row| row.get(0),
        )
        .expect("read delete guard definition");
    connection
        .execute_batch("DROP TRIGGER events_no_update; DROP TRIGGER events_no_delete;")
        .expect("remove append-only guards for fault injection");
    (update, delete)
}

fn restore_event_write_guards(connection: &rusqlite::Connection, definitions: (String, String)) {
    connection
        .execute_batch(&definitions.0)
        .expect("restore update guard after fault injection");
    connection
        .execute_batch(&definitions.1)
        .expect("restore delete guard after fault injection");
}

#[test]
fn empty_after_tail_and_range_queries_are_total() {
    let directory = TestRunDirectory::new("empty");
    let lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    drop(create_store(directory.path()));
    let mut reader =
        DiagnosticReader::open_active(run_id(), lease.guard()).expect("open active reader");
    let captured = reader.capture().expect("capture empty prefix");

    for query in [
        FiniteEventQuery::after(SchemaU64::new(0)),
        FiniteEventQuery::after(SchemaU64::new(u64::MAX)),
        FiniteEventQuery::tail(SchemaU64::new(0)),
        FiniteEventQuery::tail(SchemaU64::new(u64::MAX)),
        FiniteEventQuery::range(SchemaU64::new(0), SchemaU64::new(u64::MAX)),
    ] {
        let iterator = query_events(&captured, query);
        assert_range(iterator.range(), 0, 0, 0, None, None, 0);
        assert!(sequences(iterator).is_empty());
    }
}

#[test]
fn finite_queries_cross_page_boundaries_and_never_pass_the_captured_head() {
    let directory = TestRunDirectory::new("boundaries");
    let lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    let mut writer =
        TransactionalWriter::new(create_store(directory.path()), ()).expect("construct writer");
    let hub = diagnostic_hub();
    let canonical = commit_counters(&mut writer, &hub, MAX_BATCH_EVENTS + 1);
    let mut reader =
        DiagnosticReader::open_active(run_id(), lease.guard()).expect("open active reader");
    let captured = reader.capture().expect("capture 513-event prefix");
    commit_counters(&mut writer, &hub, 1);

    let all = query_events(&captured, FiniteEventQuery::after(SchemaU64::new(0)))
        .collect::<Result<Vec<_>, _>>()
        .expect("read captured prefix across the page boundary");
    assert_eq!(all.len(), MAX_BATCH_EVENTS + 1);
    assert_eq!(all.first().expect("first event").sequence().get(), 1);
    assert_eq!(
        all.last().expect("last captured event").sequence().get(),
        (MAX_BATCH_EVENTS + 1) as u64
    );
    assert_eq!(
        all.iter()
            .map(|event| event.canonical_bytes())
            .collect::<Vec<_>>(),
        canonical.iter().map(Vec::as_slice).collect::<Vec<_>>()
    );
    assert!(all.iter().all(|event| {
        event.event().header().run_id() == run_id()
            && event.event().header().sequence() == event.sequence()
    }));

    assert_eq!(
        sequences(query_events(
            &captured,
            FiniteEventQuery::after(SchemaU64::new(MAX_BATCH_EVENTS as u64)),
        )),
        vec![(MAX_BATCH_EVENTS + 1) as u64]
    );
    assert!(
        sequences(query_events(
            &captured,
            FiniteEventQuery::after(SchemaU64::new((MAX_BATCH_EVENTS + 1) as u64)),
        ))
        .is_empty()
    );
    assert!(
        sequences(query_events(
            &captured,
            FiniteEventQuery::after(SchemaU64::new(u64::MAX)),
        ))
        .is_empty()
    );

    assert!(
        sequences(query_events(
            &captured,
            FiniteEventQuery::tail(SchemaU64::new(0)),
        ))
        .is_empty()
    );
    assert_eq!(
        sequences(query_events(
            &captured,
            FiniteEventQuery::tail(SchemaU64::new(2)),
        )),
        vec![MAX_BATCH_EVENTS as u64, (MAX_BATCH_EVENTS + 1) as u64]
    );
    assert_eq!(
        sequences(query_events(
            &captured,
            FiniteEventQuery::tail(SchemaU64::new(u64::MAX)),
        )),
        (1..=(MAX_BATCH_EVENTS + 1) as u64).collect::<Vec<_>>()
    );

    assert_eq!(
        sequences(query_events(
            &captured,
            FiniteEventQuery::range(
                SchemaU64::new(0),
                SchemaU64::new((MAX_BATCH_EVENTS + 1) as u64),
            ),
        )),
        (1..=(MAX_BATCH_EVENTS + 1) as u64).collect::<Vec<_>>()
    );
    assert_eq!(
        sequences(query_events(
            &captured,
            FiniteEventQuery::range(SchemaU64::new(0), SchemaU64::new(MAX_BATCH_EVENTS as u64),),
        )),
        (1..=MAX_BATCH_EVENTS as u64).collect::<Vec<_>>()
    );
    assert_eq!(
        sequences(query_events(
            &captured,
            FiniteEventQuery::range(SchemaU64::new(0), SchemaU64::new(u64::MAX)),
        )),
        (1..=(MAX_BATCH_EVENTS + 1) as u64).collect::<Vec<_>>()
    );
    assert_eq!(
        sequences(query_events(
            &captured,
            FiniteEventQuery::range(
                SchemaU64::new((MAX_BATCH_EVENTS - 1) as u64),
                SchemaU64::new((MAX_BATCH_EVENTS + 1) as u64),
            ),
        )),
        vec![MAX_BATCH_EVENTS as u64, (MAX_BATCH_EVENTS + 1) as u64]
    );
    assert!(
        sequences(query_events(
            &captured,
            FiniteEventQuery::range(SchemaU64::new(12), SchemaU64::new(11)),
        ))
        .is_empty()
    );
    assert!(
        sequences(query_events(
            &captured,
            FiniteEventQuery::range(SchemaU64::new(u64::MAX - 1), SchemaU64::new(u64::MAX),),
        ))
        .is_empty()
    );
    assert!(
        sequences(query_events(
            &captured,
            FiniteEventQuery::range(SchemaU64::new(0), SchemaU64::new(0)),
        ))
        .is_empty()
    );
    assert_eq!(captured.captured_watermark().get(), 513);
    assert_eq!(writer.watermark().value().get(), 514);
}

#[test]
fn range_resolution_preserves_the_full_u64_domain_without_overflow() {
    let maximum = SchemaU64::new(u64::MAX);

    assert_range(
        FiniteEventQuery::after(SchemaU64::new(0)).resolve(maximum),
        u64::MAX,
        0,
        u64::MAX,
        Some(1),
        Some(u64::MAX),
        u64::MAX,
    );
    assert_range(
        FiniteEventQuery::after(SchemaU64::new(u64::MAX - 1)).resolve(maximum),
        u64::MAX,
        u64::MAX - 1,
        u64::MAX,
        Some(u64::MAX),
        Some(u64::MAX),
        1,
    );
    assert_range(
        FiniteEventQuery::after(maximum).resolve(maximum),
        u64::MAX,
        u64::MAX,
        u64::MAX,
        None,
        None,
        0,
    );
    assert_range(
        FiniteEventQuery::tail(SchemaU64::new(0)).resolve(maximum),
        u64::MAX,
        u64::MAX,
        u64::MAX,
        None,
        None,
        0,
    );
    assert_range(
        FiniteEventQuery::tail(SchemaU64::new(1)).resolve(maximum),
        u64::MAX,
        u64::MAX - 1,
        u64::MAX,
        Some(u64::MAX),
        Some(u64::MAX),
        1,
    );
    assert_range(
        FiniteEventQuery::tail(maximum).resolve(maximum),
        u64::MAX,
        0,
        u64::MAX,
        Some(1),
        Some(u64::MAX),
        u64::MAX,
    );
    assert_range(
        FiniteEventQuery::range(SchemaU64::new(u64::MAX - 1), maximum).resolve(maximum),
        u64::MAX,
        u64::MAX - 1,
        u64::MAX,
        Some(u64::MAX),
        Some(u64::MAX),
        1,
    );
    assert_range(
        FiniteEventQuery::after(maximum).resolve(SchemaU64::new(u64::MAX - 1)),
        u64::MAX - 1,
        u64::MAX - 1,
        u64::MAX - 1,
        None,
        None,
        0,
    );
}

#[test]
fn corrupt_and_non_dense_rows_fail_closed_before_projection() {
    let corrupt_directory = TestRunDirectory::new("corrupt-row");
    let corrupt_lease =
        ActiveArchiveLease::acquire(corrupt_directory.path()).expect("active lease");
    let mut corrupt_writer = TransactionalWriter::new(create_store(corrupt_directory.path()), ())
        .expect("construct writer");
    let hub = diagnostic_hub();
    commit_counters(&mut corrupt_writer, &hub, 2);
    drop(corrupt_writer);
    let corrupt_connection = rusqlite::Connection::open(corrupt_directory.database_path())
        .expect("open store for fault injection");
    let corrupt_guards = remove_event_write_guards(&corrupt_connection);
    corrupt_connection
        .execute(
            "UPDATE events SET canonical_json = ?1 WHERE sequence_key = ?2",
            params![
                b"{}".as_slice(),
                SortableU64Key::new(2).as_bytes().as_slice()
            ],
        )
        .expect("corrupt a captured row");
    restore_event_write_guards(&corrupt_connection, corrupt_guards);
    drop(corrupt_connection);
    let mut corrupt_reader =
        DiagnosticReader::open_active(run_id(), corrupt_lease.guard()).expect("open active reader");
    let corrupt = corrupt_reader
        .capture()
        .expect_err("corrupt row must prevent finite projection");
    assert_eq!(corrupt.class(), ReaderFailureClass::CoreFatal);
    assert_eq!(corrupt.code(), ReaderErrorCode::StoreValidation);
    assert_eq!(
        corrupt.store_code(),
        Some(StoreOpenErrorCode::EventIdentityMismatch)
    );

    let gap_directory = TestRunDirectory::new("non-dense-row");
    let gap_lease = ActiveArchiveLease::acquire(gap_directory.path()).expect("active lease");
    let mut gap_writer =
        TransactionalWriter::new(create_store(gap_directory.path()), ()).expect("construct writer");
    let gap_hub = diagnostic_hub();
    commit_counters(&mut gap_writer, &gap_hub, 3);
    drop(gap_writer);
    let gap_connection = rusqlite::Connection::open(gap_directory.database_path())
        .expect("open store for gap injection");
    let gap_guards = remove_event_write_guards(&gap_connection);
    gap_connection
        .execute(
            "DELETE FROM events WHERE sequence_key = ?1",
            [SortableU64Key::new(2).as_bytes().as_slice()],
        )
        .expect("remove middle event row");
    restore_event_write_guards(&gap_connection, gap_guards);
    drop(gap_connection);
    let mut gap_reader =
        DiagnosticReader::open_active(run_id(), gap_lease.guard()).expect("open active reader");
    let gap = gap_reader
        .capture()
        .expect_err("non-dense row must prevent finite projection");
    assert_eq!(gap.class(), ReaderFailureClass::CoreFatal);
    assert_eq!(gap.code(), ReaderErrorCode::StoreValidation);
    assert_eq!(
        gap.store_code(),
        Some(StoreOpenErrorCode::DensePrefixViolation)
    );
}
