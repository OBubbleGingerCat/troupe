use std::{
    convert::Infallible,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex},
    time::Duration,
};

use rusqlite::Connection;
use troupe_diagnostics_core::{
    detail::{EmptyDetail, PlanEntry, SpanStartDetail},
    event::{
        ActTokenUsageFinalized, AffectedElapsedInterval, AgentMessageCompleted, AgentMessageDelta,
        AgentPlanSnapshot, CausalLink, CounterSampled, DiagnosticEvent, DiagnosticEventHeader,
        DiagnosticEventKind, DiagnosticScope, ObservationGap, SpanFinished, SpanStarted,
    },
    hub::{
        AcceptedDiagnosticEvent, AdmissionReservation, AdmissionReserver, AdmissionSize,
        DeliveryFailure, DiagnosticEventCandidate, EventIdentity, LiveEventNotifier,
        MandatoryDurableReserver, ProductionDiagnosticHub,
    },
    id::{CanonicalUuid, RunLocalId},
    kinds::{
        CausalRelation, CounterKind, PlanEntryPriority, PlanEntryStatus, SpanOutcome,
        UsageAvailability, UsageUnavailableReason,
    },
    scalar::SchemaU64,
    time::ElapsedNs,
};
use troupe_diagnostics_runtime::store::{
    batch::{
        BatchAccumulator, BatchError, BatchTrigger, EventBatch, MAX_BATCH_AGE,
        MAX_BATCH_CANONICAL_BYTES, MAX_BATCH_EVENTS,
    },
    connection::{DiagnosticStore, InitialStoreMetadata},
    schema::DIAGNOSTIC_DATABASE_FILENAME,
    watermark::{CommitNotification, CommitObserver},
    writer::{TransactionalWriter, WriteStatement, WriterErrorCode, WriterTransactionHook},
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
const CRASH_MODE_ENV: &str = "TROUPE_S03_CRASH_MODE";
const CRASH_DIRECTORY_ENV: &str = "TROUPE_S03_CRASH_DIRECTORY";

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestRunDirectory(PathBuf);

impl TestRunDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "troupe-s03-writer-{label}-{}-{sequence}",
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

fn local_id(value: &str) -> RunLocalId {
    RunLocalId::parse(value).expect("valid Run-local ID")
}

fn empty_scope() -> DiagnosticScope {
    DiagnosticScope::new(None, None, None, None, None, None, None)
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
    .expect("valid test event header")
}

enum TestFact {
    SpanStarted,
    MessageDelta(String),
    PlanSnapshot { truncated: bool },
    Counter,
    Usage,
    Gap,
    MessageCompleted,
    SpanFinished,
}

impl DiagnosticEventCandidate for TestFact {
    fn materialize(self, identity: EventIdentity) -> DiagnosticEvent {
        match self {
            Self::SpanStarted => DiagnosticEvent::SpanStarted(SpanStarted::new(
                header(identity, empty_scope(), Vec::new()),
                SpanStartDetail::RunLifecycle(EmptyDetail::new()),
                None,
            )),
            Self::MessageDelta(text) => DiagnosticEvent::AgentMessageDelta(AgentMessageDelta::new(
                header(identity, act_scope(), Vec::new()),
                local_id("message-1"),
                None,
                text,
            )),
            Self::PlanSnapshot { truncated } => {
                DiagnosticEvent::AgentPlanSnapshot(AgentPlanSnapshot::new(
                    header(identity, act_scope(), Vec::new()),
                    vec![PlanEntry::new(
                        "inspect".to_owned(),
                        PlanEntryPriority::High,
                        PlanEntryStatus::InProgress,
                    )],
                    truncated,
                ))
            }
            Self::Counter => DiagnosticEvent::CounterSampled(CounterSampled::new(
                header(identity, act_scope(), Vec::new()),
                CounterKind::AgentTurnActive,
                identity.sequence(),
            )),
            Self::Usage => DiagnosticEvent::ActTokenUsageFinalized(
                ActTokenUsageFinalized::new(
                    header(identity, act_scope(), Vec::new()),
                    UsageAvailability::Unavailable,
                    None,
                    Some(UsageUnavailableReason::UsageNotReported),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .expect("valid unavailable usage"),
            ),
            Self::Gap => DiagnosticEvent::ObservationGap(ObservationGap::new(
                header(
                    identity,
                    act_scope(),
                    vec![CausalLink::new(
                        SchemaU64::new(2),
                        CausalRelation::FollowsFrom,
                    )],
                ),
                "agent-observer".to_owned(),
                Some("message-stream".to_owned()),
                "provider_sequence_gap".to_owned(),
                Some(SchemaU64::new(0)),
                Some(AffectedElapsedInterval::new(
                    ElapsedNs::new(15),
                    ElapsedNs::new(55),
                )),
                Some(DiagnosticEventKind::AgentMessageDelta),
                Some(act_scope()),
            )),
            Self::MessageCompleted => {
                DiagnosticEvent::AgentMessageCompleted(AgentMessageCompleted::new(
                    header(identity, act_scope(), Vec::new()),
                    local_id("message-1"),
                    SchemaU64::new(5),
                    SchemaU64::new(5),
                    true,
                ))
            }
            Self::SpanFinished => DiagnosticEvent::SpanFinished(SpanFinished::new(
                header(identity, empty_scope(), Vec::new()),
                SchemaU64::new(1),
                SpanOutcome::Completed,
                None,
            )),
        }
    }
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

fn accept(hub: &ProductionDiagnosticHub<AcceptAll>, fact: TestFact) -> AcceptedDiagnosticEvent {
    hub.admit(fact, None)
        .expect("accept canonical test fact")
        .accepted()
        .clone()
}

fn full_batch() -> EventBatch {
    let hub = diagnostic_hub();
    EventBatch::new(vec![
        accept(&hub, TestFact::SpanStarted),
        accept(&hub, TestFact::MessageDelta("hello".to_owned())),
        accept(&hub, TestFact::PlanSnapshot { truncated: false }),
        accept(&hub, TestFact::Counter),
        accept(&hub, TestFact::Usage),
        accept(&hub, TestFact::Gap),
        accept(&hub, TestFact::MessageCompleted),
        accept(&hub, TestFact::PlanSnapshot { truncated: true }),
        accept(&hub, TestFact::SpanFinished),
    ])
    .expect("valid full test batch")
}

fn span_batch() -> EventBatch {
    let hub = diagnostic_hub();
    EventBatch::new(vec![
        accept(&hub, TestFact::SpanStarted),
        accept(&hub, TestFact::SpanFinished),
    ])
    .expect("valid span batch")
}

fn create_store(directory: &Path) -> DiagnosticStore {
    DiagnosticStore::create(
        directory,
        &InitialStoreMetadata::new(
            run_id(),
            "2026-08-14T00:00:00Z",
            "configuration-sha256:test",
        ),
    )
    .expect("create diagnostic store")
}

#[derive(Clone, Default)]
struct RecordingObserver(Arc<Mutex<Vec<CommitNotification>>>);

impl RecordingObserver {
    fn notifications(&self) -> Vec<CommitNotification> {
        self.0.lock().expect("lock observer").clone()
    }
}

impl CommitObserver for RecordingObserver {
    fn committed(&mut self, notification: CommitNotification) {
        self.0.lock().expect("lock observer").push(notification);
    }
}

fn row_count(connection: &Connection, table: &str) -> u64 {
    let count: i64 = connection
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count table rows");
    count.try_into().expect("non-negative row count")
}

fn assert_empty_prefix(connection: &Connection) {
    assert_eq!(row_count(connection, "events"), 0);
    for table in [
        "materialized_spans",
        "materialized_messages",
        "materialized_plans",
        "materialized_counters",
        "materialized_usage",
        "materialized_snapshot",
    ] {
        assert_eq!(row_count(connection, table), 0, "unexpected row in {table}");
    }
    let watermarks: (String, String) = connection
        .query_row(
            "SELECT committed_sequence, read_model_sequence FROM run_metadata WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read persisted watermarks");
    assert_eq!(watermarks, ("0".to_owned(), "0".to_owned()));
}

#[test]
fn paused_clock_exercises_all_three_exact_batch_triggers() {
    assert_eq!(MAX_BATCH_AGE, Duration::from_millis(25));
    assert_eq!(MAX_BATCH_EVENTS, 512);
    assert_eq!(MAX_BATCH_CANONICAL_BYTES, 1024 * 1024);

    let hub = diagnostic_hub();
    let mut age = BatchAccumulator::new();
    assert!(
        age.push(
            accept(&hub, TestFact::MessageDelta("age".to_owned())),
            Duration::ZERO,
        )
        .expect("push age event")
        .is_none()
    );
    assert!(
        age.poll(MAX_BATCH_AGE - Duration::from_nanos(1))
            .expect("poll before age boundary")
            .is_none()
    );
    let aged = age
        .poll(MAX_BATCH_AGE)
        .expect("poll exact age boundary")
        .expect("age-triggered batch");
    assert_eq!(aged.trigger(), BatchTrigger::OldestAge);
    assert_eq!(aged.batch().event_count(), 1);

    let hub = diagnostic_hub();
    let mut count = BatchAccumulator::new();
    for index in 0..MAX_BATCH_EVENTS - 1 {
        assert!(
            count
                .push(accept(&hub, TestFact::Counter), Duration::ZERO)
                .unwrap_or_else(|error| panic!("push count event {index}: {error}"))
                .is_none()
        );
    }
    let counted = count
        .push(accept(&hub, TestFact::Counter), Duration::ZERO)
        .expect("push exact count boundary")
        .expect("count-triggered batch");
    assert_eq!(counted.trigger(), BatchTrigger::EventCount);
    assert_eq!(counted.batch().event_count(), MAX_BATCH_EVENTS);

    let baseline_hub = diagnostic_hub();
    let baseline = accept(&baseline_hub, TestFact::MessageDelta(String::new()));
    let exact_text_bytes = MAX_BATCH_CANONICAL_BYTES
        .checked_sub(baseline.canonical_bytes().len())
        .expect("batch threshold exceeds event envelope");

    let below_hub = diagnostic_hub();
    let below = accept(
        &below_hub,
        TestFact::MessageDelta("x".repeat(exact_text_bytes - 1)),
    );
    assert_eq!(below.canonical_bytes().len(), MAX_BATCH_CANONICAL_BYTES - 1);
    let mut below_bytes = BatchAccumulator::new();
    assert!(
        below_bytes
            .push(below, Duration::ZERO)
            .expect("push below byte boundary")
            .is_none()
    );

    let exact_hub = diagnostic_hub();
    let byte_event = accept(
        &exact_hub,
        TestFact::MessageDelta("x".repeat(exact_text_bytes)),
    );
    assert_eq!(
        byte_event.canonical_bytes().len(),
        MAX_BATCH_CANONICAL_BYTES
    );
    let mut bytes = BatchAccumulator::new();
    let byte_batch = bytes
        .push(byte_event, Duration::ZERO)
        .expect("push byte boundary")
        .expect("byte-triggered batch");
    assert_eq!(byte_batch.trigger(), BatchTrigger::CanonicalBytes);
    assert_eq!(
        byte_batch.batch().canonical_bytes(),
        MAX_BATCH_CANONICAL_BYTES
    );

    assert!(bytes.poll(Duration::ZERO).unwrap().is_none());
    let mut regressing = BatchAccumulator::new();
    regressing.poll(Duration::from_millis(2)).unwrap();
    assert!(matches!(
        regressing.poll(Duration::from_millis(1)),
        Err(BatchError::ClockRegressed)
    ));

    let hub = diagnostic_hub();
    let first = accept(&hub, TestFact::Counter);
    let second = accept(&hub, TestFact::Counter);
    let third = accept(&hub, TestFact::Counter);
    let mut retryable = BatchAccumulator::new();
    retryable.push(first, Duration::ZERO).unwrap();
    assert!(matches!(
        retryable.push(third, Duration::from_millis(2)),
        Err(BatchError::NonCanonicalSequence {
            expected: 2,
            actual: 3
        })
    ));
    assert!(
        retryable
            .push(second, Duration::from_millis(1))
            .expect("invalid event must not advance the batch clock")
            .is_none()
    );
}

#[test]
fn one_full_transaction_aligns_events_all_read_models_and_observer() {
    let directory = TestRunDirectory::new("full-commit");
    let observer = RecordingObserver::default();
    let mut writer = TransactionalWriter::new(create_store(directory.path()), observer.clone())
        .expect("construct fresh writer");
    let batch = full_batch();

    let notification = writer.commit_batch(&batch).expect("commit full batch");
    assert_eq!(notification.previous().get(), 0);
    assert_eq!(notification.committed().get(), 9);
    assert_eq!(notification.event_count(), 9);
    assert_eq!(notification.canonical_bytes(), batch.canonical_bytes());
    assert_eq!(writer.watermark().value().get(), 9);
    assert_eq!(writer.snapshot().through_sequence().get(), 9);
    assert_eq!(observer.notifications(), vec![notification]);

    let connection = writer.store().connection();
    assert_eq!(row_count(connection, "events"), 9);
    assert_eq!(row_count(connection, "materialized_spans"), 1);
    assert_eq!(row_count(connection, "materialized_messages"), 1);
    assert_eq!(row_count(connection, "materialized_plans"), 1);
    assert_eq!(row_count(connection, "materialized_counters"), 1);
    assert_eq!(row_count(connection, "materialized_usage"), 1);
    assert_eq!(row_count(connection, "materialized_snapshot"), 1);
    let watermarks: (String, String) = connection
        .query_row(
            "SELECT committed_sequence, read_model_sequence FROM run_metadata WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read committed watermarks");
    assert_eq!(watermarks, ("9".to_owned(), "9".to_owned()));

    let snapshot_bytes: Vec<u8> = connection
        .query_row(
            "SELECT payload_json FROM materialized_snapshot WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .expect("read materialized snapshot");
    assert_eq!(
        snapshot_bytes,
        writer.snapshot().canonical_json().expect("encode snapshot")
    );
    for (sequence, accepted) in batch.events().iter().enumerate() {
        let stored: Vec<u8> = connection
            .query_row(
                "SELECT canonical_json FROM events WHERE sequence = ?1",
                [(sequence + 1).to_string()],
                |row| row.get(0),
            )
            .expect("read canonical event");
        assert_eq!(stored, accepted.canonical_bytes());
    }

    drop(writer);
    let reopened = DiagnosticStore::open_validated(directory.path(), run_id())
        .expect("reopen FULL-committed store");
    assert_eq!(reopened.metadata().committed_watermark().get(), 9);
    assert_eq!(reopened.metadata().read_model_watermark().get(), 9);
    let reopened_snapshot: Vec<u8> = reopened
        .connection()
        .query_row(
            "SELECT payload_json FROM materialized_snapshot WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .expect("read reopened snapshot");
    assert_eq!(reopened_snapshot, snapshot_bytes);
}

struct InvisibleBeforeCommit {
    database_path: PathBuf,
}

impl WriterTransactionHook for InvisibleBeforeCommit {
    fn before_commit(&mut self, _transaction: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
        let reader = Connection::open(&self.database_path).expect("open independent reader");
        assert_empty_prefix(&reader);
        Err(rusqlite::Error::InvalidQuery)
    }
}

#[test]
fn accepted_uncommitted_tail_is_invisible_and_retryable_after_rollback() {
    let directory = TestRunDirectory::new("uncommitted");
    let observer = RecordingObserver::default();
    let mut writer = TransactionalWriter::new(create_store(directory.path()), observer.clone())
        .expect("construct writer");
    let batch = span_batch();
    let mut hook = InvisibleBeforeCommit {
        database_path: directory.database_path(),
    };

    let error = writer
        .commit_batch_with_hook(&batch, &mut hook)
        .expect_err("inject before-commit failure");
    assert_eq!(error.code(), WriterErrorCode::BeforeCommit);
    assert_eq!(writer.watermark().value().get(), 0);
    assert_eq!(writer.snapshot().through_sequence().get(), 0);
    assert!(observer.notifications().is_empty());
    assert_empty_prefix(writer.store().connection());

    writer.commit_batch(&batch).expect("retry identical tail");
    assert_eq!(writer.watermark().value().get(), 2);
    assert_eq!(observer.notifications().len(), 1);
}

#[derive(Default)]
struct CountStatements {
    count: usize,
}

impl WriterTransactionHook for CountStatements {
    fn after_statement(
        &mut self,
        ordinal: usize,
        _statement: WriteStatement,
        _transaction: &rusqlite::Transaction<'_>,
    ) -> rusqlite::Result<()> {
        self.count = ordinal;
        Ok(())
    }
}

struct FailStatement {
    target: usize,
}

impl WriterTransactionHook for FailStatement {
    fn after_statement(
        &mut self,
        ordinal: usize,
        _statement: WriteStatement,
        _transaction: &rusqlite::Transaction<'_>,
    ) -> rusqlite::Result<()> {
        if ordinal == self.target {
            Err(rusqlite::Error::InvalidQuery)
        } else {
            Ok(())
        }
    }
}

#[test]
fn every_statement_failure_rolls_back_to_the_original_dense_prefix() {
    let batch = span_batch();
    let count_directory = TestRunDirectory::new("statement-count");
    let mut counter = CountStatements::default();
    let mut counting_writer = TransactionalWriter::new(create_store(count_directory.path()), ())
        .expect("construct counting writer");
    counting_writer
        .commit_batch_with_hook(&batch, &mut counter)
        .expect("count successful statements");
    assert!(counter.count >= 10);

    for target in 1..=counter.count {
        let directory = TestRunDirectory::new(&format!("statement-{target}"));
        let observer = RecordingObserver::default();
        let mut writer = TransactionalWriter::new(create_store(directory.path()), observer.clone())
            .expect("construct fault writer");
        let mut hook = FailStatement { target };
        let error = writer
            .commit_batch_with_hook(&batch, &mut hook)
            .expect_err("inject statement failure");
        assert_eq!(error.code(), WriterErrorCode::Statement);
        assert_eq!(error.statement_ordinal(), Some(target));
        assert_eq!(writer.watermark().value().get(), 0);
        assert_eq!(writer.snapshot().through_sequence().get(), 0);
        assert!(observer.notifications().is_empty());
        assert_empty_prefix(writer.store().connection());

        writer
            .commit_batch(&batch)
            .unwrap_or_else(|error| panic!("retry after statement {target}: {error}"));
        assert_eq!(writer.watermark().value().get(), 2);
    }
}

struct RollBackBeforeCommit;

impl WriterTransactionHook for RollBackBeforeCommit {
    fn before_commit(&mut self, transaction: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
        transaction.execute_batch("ROLLBACK")
    }
}

#[test]
fn actual_commit_error_does_not_advance_memory_disk_or_observer() {
    let directory = TestRunDirectory::new("commit-error");
    let observer = RecordingObserver::default();
    let mut writer = TransactionalWriter::new(create_store(directory.path()), observer.clone())
        .expect("construct writer");
    let batch = span_batch();
    let mut hook = RollBackBeforeCommit;

    let error = writer
        .commit_batch_with_hook(&batch, &mut hook)
        .expect_err("force COMMIT after rollback error");
    assert_eq!(error.code(), WriterErrorCode::Commit);
    assert_eq!(writer.watermark().value().get(), 0);
    assert_eq!(writer.snapshot().through_sequence().get(), 0);
    assert!(observer.notifications().is_empty());
    assert_empty_prefix(writer.store().connection());
}

#[test]
fn writer_rejects_gaps_and_never_resumes_an_existing_run() {
    let gap_directory = TestRunDirectory::new("gap");
    let hub = diagnostic_hub();
    let _discarded = accept(&hub, TestFact::Counter);
    let sequence_two = accept(&hub, TestFact::Counter);
    let gap_batch = EventBatch::new(vec![sequence_two]).expect("single event batch");
    let mut gap_writer = TransactionalWriter::new(create_store(gap_directory.path()), ())
        .expect("construct gap writer");
    let error = gap_writer
        .commit_batch(&gap_batch)
        .expect_err("reject missing sequence one");
    assert_eq!(error.code(), WriterErrorCode::Watermark);
    assert_empty_prefix(gap_writer.store().connection());

    let resume_directory = TestRunDirectory::new("resume");
    let mut writer = TransactionalWriter::new(create_store(resume_directory.path()), ())
        .expect("construct writer");
    writer
        .commit_batch(&span_batch())
        .expect("commit existing prefix");
    let store = writer.into_store();
    let error = TransactionalWriter::new(store, ()).expect_err("writer must not resume old Run");
    assert_eq!(error.code(), WriterErrorCode::NonFreshStore);
}

struct CrashAfterFirstStatement;

impl WriterTransactionHook for CrashAfterFirstStatement {
    fn after_statement(
        &mut self,
        ordinal: usize,
        _statement: WriteStatement,
        _transaction: &rusqlite::Transaction<'_>,
    ) -> rusqlite::Result<()> {
        if ordinal == 1 {
            std::process::exit(72);
        }
        Ok(())
    }
}

#[test]
fn crash_reopen_helper() {
    let Ok(mode) = std::env::var(CRASH_MODE_ENV) else {
        return;
    };
    let directory = PathBuf::from(
        std::env::var_os(CRASH_DIRECTORY_ENV).expect("crash helper directory environment"),
    );
    let mut writer =
        TransactionalWriter::new(create_store(&directory), ()).expect("construct crash writer");
    let batch = span_batch();
    match mode.as_str() {
        "queued" => std::process::exit(71),
        "transaction" => {
            let mut hook = CrashAfterFirstStatement;
            let _ = writer.commit_batch_with_hook(&batch, &mut hook);
            panic!("transaction crash hook did not exit")
        }
        "committed" => {
            writer.commit_batch(&batch).expect("commit before crash");
            std::process::exit(73);
        }
        _ => panic!("unknown crash helper mode"),
    }
}

#[test]
fn crash_reopen_matrix_exposes_only_a_dense_durable_prefix() {
    for (mode, expected_watermark) in [("queued", 0), ("transaction", 0), ("committed", 2)] {
        let directory = TestRunDirectory::new(&format!("crash-{mode}"));
        let status = Command::new(std::env::current_exe().expect("current test executable"))
            .arg("--exact")
            .arg("crash_reopen_helper")
            .arg("--test-threads=1")
            .env(CRASH_MODE_ENV, mode)
            .env(CRASH_DIRECTORY_ENV, directory.path())
            .status()
            .expect("run crash helper process");
        assert!(
            !status.success(),
            "crash helper {mode} unexpectedly succeeded"
        );

        let reopened = DiagnosticStore::open_validated(directory.path(), run_id())
            .unwrap_or_else(|error| panic!("reopen {mode} crash store: {error}"));
        assert_eq!(
            reopened.metadata().committed_watermark().get(),
            expected_watermark
        );
        assert_eq!(
            reopened.metadata().read_model_watermark().get(),
            expected_watermark
        );
        assert_eq!(
            row_count(reopened.connection(), "events"),
            expected_watermark
        );
    }
}
