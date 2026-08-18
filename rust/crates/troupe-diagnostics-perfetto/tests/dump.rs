use std::{
    convert::Infallible,
    fs,
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

use futures::{executor::block_on, task::noop_waker};
use tokio::io::AsyncWrite;
use troupe_diagnostics_core::{
    detail::{
        ActorDetail, CanonicalInteger, CustomNumber, DiagnosticDimensions, EmptyDetail,
        SpanStartDetail,
    },
    event::{
        CounterSampled, CustomCounterSampled, DiagnosticEvent, DiagnosticEventHeader,
        DiagnosticScope, SpanFinished, SpanStarted,
    },
    hub::{
        AcceptedDiagnosticEvent, AdmissionReservation, AdmissionReserver, AdmissionSize,
        DeliveryFailure, EventIdentity, LiveEventNotifier, MandatoryDurableReserver,
        ProductionDiagnosticHub,
    },
    id::{CanonicalUuid, RunLocalId},
    kinds::{CounterKind, SpanOutcome},
    scalar::{DecimalString, SchemaU64},
    time::ElapsedNs,
};
use troupe_diagnostics_perfetto::{
    collect::{ProjectionError, ProjectionMetadata},
    dump::{TraceBodyValidator, dump_captured_prefix},
    project::project_prefix,
};
use troupe_diagnostics_runtime::{
    archive::lease::ActiveArchiveLease,
    query::reader::{
        CAPTURED_EVENT_PAGE_SIZE, CapturedEventSource, DiagnosticReader, ReaderErrorCode,
    },
    store::{
        batch::{EventBatch, MAX_BATCH_EVENTS},
        connection::{DiagnosticStore, InitialStoreMetadata},
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
            "troupe-t03-dump-{label}-{}-{sequence}",
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

#[derive(Default)]
struct MemoryWriter(Vec<u8>);

impl AsyncWrite for MemoryWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.0.extend_from_slice(bytes);
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn run_id() -> CanonicalUuid {
    CanonicalUuid::parse(RUN_ID).expect("canonical test Run UUID")
}

#[derive(Clone, Copy)]
struct AcceptAll;

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

struct IgnoreLive;

impl LiveEventNotifier for IgnoreLive {
    fn notify(&mut self, _event: AcceptedDiagnosticEvent) -> Result<(), DeliveryFailure> {
        Ok(())
    }
}

fn create_store(directory: &Path) -> DiagnosticStore {
    DiagnosticStore::create(
        directory,
        &InitialStoreMetadata::new(run_id(), "2026-08-15T00:00:00Z", "configuration-sha256:t03"),
    )
    .expect("create diagnostic store")
}

fn persist_events(writer: &mut TransactionalWriter<()>, events: Vec<DiagnosticEvent>) {
    let hub = ProductionDiagnosticHub::production(run_id(), AcceptAll, Box::new(IgnoreLive));
    let accepted = events
        .into_iter()
        .map(|event| {
            let expected_sequence = event.header().sequence();
            hub.admit(
                move |identity: EventIdentity| {
                    assert_eq!(identity.run_id(), run_id());
                    assert_eq!(identity.sequence(), expected_sequence);
                    event
                },
                None,
            )
            .expect("admit fixture event")
            .accepted()
            .clone()
        })
        .collect::<Vec<_>>();
    let mut accepted = accepted.into_iter();
    loop {
        let events = accepted.by_ref().take(MAX_BATCH_EVENTS).collect::<Vec<_>>();
        if events.is_empty() {
            break;
        }
        writer
            .commit_batch(&EventBatch::new(events).expect("canonical fixture batch"))
            .expect("commit fixture batch");
    }
}

fn with_active_capture<T>(
    label: &str,
    events: Vec<DiagnosticEvent>,
    inspect: impl FnOnce(&CapturedEventSource<'_>) -> T,
) -> T {
    let directory = TestRunDirectory::new(label);
    let active_lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    let mut writer =
        TransactionalWriter::new(create_store(directory.path()), ()).expect("construct writer");
    persist_events(&mut writer, events);
    let mut reader =
        DiagnosticReader::open_active(run_id(), active_lease.guard()).expect("open active reader");
    let captured = reader.capture().expect("capture active prefix");
    inspect(&captured)
}

fn with_archive_capture<T>(
    label: &str,
    events: Vec<DiagnosticEvent>,
    inspect: impl FnOnce(&CapturedEventSource<'_>) -> T,
) -> T {
    let directory = TestRunDirectory::new(label);
    let active_lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    let mut writer =
        TransactionalWriter::new(create_store(directory.path()), ()).expect("construct writer");
    persist_events(&mut writer, events);
    let store = writer.into_store();
    store
        .connection()
        .execute(
            "UPDATE run_metadata SET ended_at = '2026-08-15T00:00:01Z', \
             production_outcome = 'completed', clean_shutdown = 1 WHERE singleton = 1",
            (),
        )
        .expect("record terminal Production metadata");
    drop(store);
    drop(active_lease);
    let mut reader =
        DiagnosticReader::open_archive(directory.path(), run_id()).expect("open archive reader");
    let captured = reader.capture().expect("capture archive prefix");
    inspect(&captured)
}

fn dump_bytes(source: &CapturedEventSource<'_>, through: Option<u64>) -> (Vec<u8>, u64) {
    let mut writer = MemoryWriter::default();
    let summary = block_on(dump_captured_prefix(
        source,
        &mut writer,
        through.map(SchemaU64::new),
    ))
    .expect("dump captured prefix");
    assert_eq!(summary.bytes_written(), writer.0.len() as u64);
    (writer.0, summary.packet_count())
}

#[test]
fn streamed_body_validator_accepts_the_t03_dump_contract() {
    with_active_capture("body-validator-valid", vec![counter(1, 1)], |captured| {
        let (bytes, _) = dump_bytes(captured, None);
        let metadata = ProjectionMetadata::new(
            run_id(),
            captured.captured_watermark(),
            captured.captured_watermark(),
            env!("CARGO_PKG_VERSION"),
        );
        let mut validator = TraceBodyValidator::new(metadata);
        for chunk in bytes.chunks(3) {
            validator.push(chunk).expect("valid Perfetto trace chunk");
        }
        validator.finish().expect("complete Perfetto trace");
    });
}

#[test]
fn streamed_body_validator_rejects_header_body_metadata_mismatch() {
    with_active_capture("body-validator-mismatch", vec![counter(1, 1)], |captured| {
        let (bytes, _) = dump_bytes(captured, None);
        let metadata = ProjectionMetadata::new(
            run_id(),
            captured.captured_watermark(),
            SchemaU64::new(captured.captured_watermark().get() - 1),
            env!("CARGO_PKG_VERSION"),
        );
        let mut validator = TraceBodyValidator::new(metadata);
        let error = validator
            .push(&bytes)
            .expect_err("metadata must match headers");
        assert_eq!(error.code(), "body_metadata_mismatch");
    });
}

#[test]
fn streamed_body_validator_rejects_incomplete_or_malformed_trace_body() {
    let metadata = ProjectionMetadata::new(
        run_id(),
        SchemaU64::new(0),
        SchemaU64::new(0),
        env!("CARGO_PKG_VERSION"),
    );
    let mut validator = TraceBodyValidator::new(metadata);
    let error = validator
        .push(&[0x0a, 0x01, 0xff])
        .expect_err("malformed packet must be rejected");
    assert_eq!(error.code(), "body_invalid");

    let metadata = ProjectionMetadata::new(
        run_id(),
        SchemaU64::new(0),
        SchemaU64::new(0),
        env!("CARGO_PKG_VERSION"),
    );
    let mut validator = TraceBodyValidator::new(metadata);
    let error = validator
        .push(&[0x0a, 0x00])
        .expect_err("empty TracePacket must be rejected");
    assert_eq!(error.code(), "body_invalid");
}

fn local(value: &str) -> RunLocalId {
    RunLocalId::parse(value).expect("valid Run-local ID")
}

fn scope(scene: Option<&str>, actor: Option<&str>, cue: Option<&str>) -> DiagnosticScope {
    DiagnosticScope::new(
        scene.map(local),
        actor.map(local),
        cue.map(local),
        None,
        None,
        None,
        None,
    )
}

fn header(sequence: u64, elapsed_ns: u64, scope: DiagnosticScope) -> DiagnosticEventHeader {
    DiagnosticEventHeader::new(
        run_id(),
        SchemaU64::new(sequence),
        ElapsedNs::new(elapsed_ns),
        scope,
        Vec::new(),
    )
    .expect("valid diagnostic event header")
}

fn counter(sequence: u64, elapsed_ns: u64) -> DiagnosticEvent {
    DiagnosticEvent::CounterSampled(CounterSampled::new(
        header(sequence, elapsed_ns, scope(None, None, None)),
        CounterKind::DiagnosticDroppedEvents,
        SchemaU64::new(sequence),
    ))
}

fn start(
    sequence: u64,
    elapsed_ns: u64,
    event_scope: DiagnosticScope,
    detail: SpanStartDetail,
    parent: Option<u64>,
) -> DiagnosticEvent {
    DiagnosticEvent::SpanStarted(SpanStarted::new(
        header(sequence, elapsed_ns, event_scope),
        detail,
        parent.map(SchemaU64::new),
    ))
}

fn finish(
    sequence: u64,
    elapsed_ns: u64,
    event_scope: DiagnosticScope,
    span_id: u64,
) -> DiagnosticEvent {
    DiagnosticEvent::SpanFinished(SpanFinished::new(
        header(sequence, elapsed_ns, event_scope),
        SchemaU64::new(span_id),
        SpanOutcome::Completed,
        None,
    ))
}

fn open_events() -> Vec<DiagnosticEvent> {
    vec![start(
        1,
        10,
        scope(None, None, None),
        SpanStartDetail::ProductionStart(EmptyDetail::new()),
        None,
    )]
}

fn nested_events() -> Vec<DiagnosticEvent> {
    let root = scope(None, None, None);
    let scene = scope(Some("scene-1"), None, None);
    let actor = scope(Some("scene-1"), Some("actor-1"), None);
    vec![
        start(
            1,
            0,
            root.clone(),
            SpanStartDetail::RunLifecycle(EmptyDetail::new()),
            None,
        ),
        start(
            2,
            10,
            scene.clone(),
            SpanStartDetail::SceneLifecycle(EmptyDetail::new()),
            Some(1),
        ),
        start(
            3,
            20,
            actor.clone(),
            SpanStartDetail::ActorHandleLifetime(ActorDetail::new(
                "Worker".to_owned(),
                "Worker".to_owned(),
            )),
            Some(2),
        ),
        finish(4, 30, actor, 3),
        finish(5, 40, scene, 2),
        finish(6, 50, root, 1),
    ]
}

fn multi_cue_events() -> Vec<DiagnosticEvent> {
    let actor = scope(Some("scene-1"), Some("actor-1"), None);
    let cue_one = scope(Some("scene-1"), Some("actor-1"), Some("cue-1"));
    let cue_two = scope(Some("scene-1"), Some("actor-1"), Some("cue-2"));
    vec![
        start(
            1,
            0,
            actor.clone(),
            SpanStartDetail::ActorHandleLifetime(ActorDetail::new(
                "Worker".to_owned(),
                "Worker".to_owned(),
            )),
            None,
        ),
        start(
            2,
            10,
            cue_one.clone(),
            SpanStartDetail::CueExecution(EmptyDetail::new()),
            Some(1),
        ),
        finish(3, 20, cue_one, 2),
        start(
            4,
            30,
            cue_two.clone(),
            SpanStartDetail::CueExecution(EmptyDetail::new()),
            Some(1),
        ),
        finish(5, 40, cue_two, 4),
        finish(6, 50, actor, 1),
    ]
}

fn overlap_events() -> Vec<DiagnosticEvent> {
    let actor = scope(Some("scene-1"), Some("actor-1"), None);
    let cue_one = scope(Some("scene-1"), Some("actor-1"), Some("cue-1"));
    let cue_two = scope(Some("scene-1"), Some("actor-1"), Some("cue-2"));
    vec![
        start(
            1,
            0,
            actor.clone(),
            SpanStartDetail::ActorHandleLifetime(ActorDetail::new(
                "Worker".to_owned(),
                "Worker".to_owned(),
            )),
            None,
        ),
        start(
            2,
            10,
            cue_one.clone(),
            SpanStartDetail::CueExecution(EmptyDetail::new()),
            Some(1),
        ),
        start(
            3,
            20,
            cue_two.clone(),
            SpanStartDetail::CueExecution(EmptyDetail::new()),
            Some(1),
        ),
        finish(4, 30, cue_one, 2),
        finish(5, 40, cue_two, 3),
        finish(6, 50, actor, 1),
    ]
}

fn numeric_boundary_events() -> Vec<DiagnosticEvent> {
    let root = scope(None, None, None);
    let exact_integer = CustomCounterSampled::new(
        header(1, i64::MAX as u64, root.clone()),
        "numeric.i64_max".to_owned(),
        CustomNumber::Integer(CanonicalInteger::parse("9223372036854775807").unwrap()),
        Some("items".to_owned()),
        DiagnosticDimensions::new(),
    )
    .unwrap();
    let decimal_fallback = CustomCounterSampled::new(
        header(2, i64::MAX as u64, root),
        "numeric.not_exact".to_owned(),
        CustomNumber::Decimal(DecimalString::parse("0.1").unwrap()),
        None,
        DiagnosticDimensions::new(),
    )
    .unwrap();
    vec![
        DiagnosticEvent::CustomCounterSampled(exact_integer),
        DiagnosticEvent::CustomCounterSampled(decimal_fallback),
    ]
}

#[test]
fn through_zero_is_a_valid_descriptor_only_trace() {
    let directory = TestRunDirectory::new("empty");
    let active_lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    let _store = DiagnosticStore::create(
        directory.path(),
        &InitialStoreMetadata::new(run_id(), "2026-08-15T00:00:00Z", "config:t03"),
    )
    .expect("create diagnostic store");
    let mut reader =
        DiagnosticReader::open_active(run_id(), active_lease.guard()).expect("open active reader");
    let captured = reader.capture().expect("capture empty Run");
    let mut output = MemoryWriter::default();

    let summary = block_on(dump_captured_prefix(&captured, &mut output, None))
        .expect("dump descriptor-only trace");

    assert_eq!(summary.captured_watermark().get(), 0);
    assert_eq!(summary.exported_through().get(), 0);
    assert_eq!(summary.event_count(), 0);
    assert!(summary.descriptor_count() >= 2);
    assert_eq!(summary.packet_count(), summary.descriptor_count());
    assert_eq!(summary.bytes_written(), output.0.len() as u64);
    assert!(!output.0.is_empty());

    with_active_capture("explicit-zero", vec![counter(1, 1)], |captured| {
        let mut output = MemoryWriter::default();
        let summary = block_on(dump_captured_prefix(
            captured,
            &mut output,
            Some(SchemaU64::new(0)),
        ))
        .expect("dump explicit empty prefix from non-empty capture");
        assert_eq!(summary.captured_watermark().get(), 1);
        assert_eq!(summary.exported_through().get(), 0);
        assert_eq!(summary.event_count(), 0);
        assert_eq!(summary.event_packet_count(), 0);
        assert_eq!(summary.source_page_reads(), 0);
    });
}

struct ShortWriter {
    bytes: Vec<u8>,
    maximum_write: usize,
}

impl AsyncWrite for ShortWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let written = bytes.len().min(self.maximum_write);
        self.bytes.extend_from_slice(&bytes[..written]);
        Poll::Ready(Ok(written))
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

struct ErrorWriter {
    calls: Arc<AtomicUsize>,
    kind: io::ErrorKind,
}

impl AsyncWrite for ErrorWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        _bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Poll::Ready(Err(io::Error::new(self.kind, "injected writer error")))
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

struct PendingWriter(Arc<AtomicUsize>);

impl AsyncWrite for PendingWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        _bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Poll::Pending
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[derive(Default)]
struct DiscardWriter {
    calls: u64,
    bytes: u64,
}

impl AsyncWrite for DiscardWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.calls += 1;
        self.bytes += bytes.len() as u64;
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[test]
fn two_pass_dump_matches_the_typed_projector_and_honors_short_writes() {
    let events = nested_events();
    with_active_capture("short-writes", events.clone(), |captured| {
        let expected = project_prefix(
            ProjectionMetadata::new(
                run_id(),
                SchemaU64::new(events.len() as u64),
                SchemaU64::new(events.len() as u64),
                env!("CARGO_PKG_VERSION"),
            ),
            &events,
        )
        .unwrap()
        .trace_bytes()
        .unwrap();
        let mut writer = ShortWriter {
            bytes: Vec::new(),
            maximum_write: 3,
        };
        let summary = block_on(dump_captured_prefix(captured, &mut writer, None)).unwrap();

        assert_eq!(writer.bytes, expected);
        assert_eq!(summary.event_count(), events.len() as u64);
        assert_eq!(summary.bytes_written(), expected.len() as u64);
        assert!(summary.event_packet_count() >= events.len() as u64);
    });
}

#[test]
fn writer_errors_are_returned_once_without_retry() {
    with_active_capture("writer-error", open_events(), |captured| {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut writer = ErrorWriter {
            calls: Arc::clone(&calls),
            kind: io::ErrorKind::Interrupted,
        };
        let error = block_on(dump_captured_prefix(captured, &mut writer, None)).unwrap_err();

        assert_eq!(
            error.writer_error().unwrap().kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(
            error.writer_error().unwrap().to_string(),
            "injected writer error"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        let mut zero_writer = ShortWriter {
            bytes: Vec::new(),
            maximum_write: 0,
        };
        let error = block_on(dump_captured_prefix(captured, &mut zero_writer, None)).unwrap_err();
        assert_eq!(
            error.writer_error().unwrap().kind(),
            io::ErrorKind::WriteZero
        );
    });
}

#[test]
fn cancelling_a_pending_write_leaves_the_captured_source_reusable() {
    with_active_capture("cancel", nested_events(), |captured| {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut pending_writer = PendingWriter(Arc::clone(&calls));
        let mut future = Box::pin(dump_captured_prefix(captured, &mut pending_writer, None));
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);
        assert!(Future::poll(future.as_mut(), &mut context).is_pending());
        drop(future);
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        let first = dump_bytes(captured, None).0;
        let second = dump_bytes(captured, None).0;
        assert_eq!(first, second);
    });
}

#[test]
fn invalid_watermarks_and_projection_faults_emit_no_bytes() {
    with_active_capture("watermark-error", vec![counter(1, 1)], |captured| {
        let mut writer = MemoryWriter::default();
        let error = block_on(dump_captured_prefix(
            captured,
            &mut writer,
            Some(SchemaU64::new(2)),
        ))
        .unwrap_err();
        assert!(matches!(
            error.projection_error(),
            Some(ProjectionError::WatermarkMismatch {
                captured: 1,
                exported: 2,
                observed: 0
            })
        ));
        assert!(writer.0.is_empty());
    });

    with_active_capture(
        "timestamp-error",
        vec![counter(1, i64::MAX as u64 + 1)],
        |captured| {
            let mut writer = MemoryWriter::default();
            let error = block_on(dump_captured_prefix(captured, &mut writer, None)).unwrap_err();
            assert!(matches!(
                error.projection_error(),
                Some(ProjectionError::TimestampOutOfRange { sequence: 1, .. })
            ));
            assert!(writer.0.is_empty());
        },
    );
}

#[test]
fn source_read_errors_are_preserved_and_emit_no_bytes() {
    let directory = TestRunDirectory::new("source-error");
    let active_lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    let mut writer =
        TransactionalWriter::new(create_store(directory.path()), ()).expect("construct writer");
    persist_events(
        &mut writer,
        (1..=CAPTURED_EVENT_PAGE_SIZE * 32)
            .map(|sequence| counter(sequence as u64, sequence as u64))
            .collect(),
    );
    writer
        .store()
        .connection()
        .pragma_update(None, "default_cache_size", 8)
        .expect("bound the fault-injection reader page cache");
    let database_path = writer.store().database_path().to_path_buf();
    drop(writer);
    let mut reader = DiagnosticReader::open_active(run_id(), active_lease.guard()).unwrap();
    let captured = reader.capture().unwrap();
    fs::write(&database_path, b"injected storage failure")
        .expect("invalidate captured backing file");
    let mut wal_path = database_path.as_os_str().to_os_string();
    wal_path.push("-wal");
    let wal_path = PathBuf::from(wal_path);
    if wal_path.exists() {
        fs::write(wal_path, b"injected WAL failure").expect("invalidate captured WAL");
    }
    let mut output = MemoryWriter::default();

    let error = block_on(dump_captured_prefix(&captured, &mut output, None)).unwrap_err();

    assert_eq!(
        error.source_error().unwrap().code(),
        ReaderErrorCode::EventRead
    );
    assert!(output.0.is_empty());
}

#[test]
fn large_prefix_uses_fixed_source_pages_and_a_single_packet_buffer() {
    const EVENT_COUNT: usize = CAPTURED_EVENT_PAGE_SIZE * 8 + 1;
    let events = (1..=EVENT_COUNT)
        .map(|sequence| counter(sequence as u64, sequence as u64))
        .collect();
    with_active_capture("large-prefix", events, |captured| {
        let mut writer = DiscardWriter::default();
        let summary = block_on(dump_captured_prefix(captured, &mut writer, None)).unwrap();

        assert_eq!(summary.event_count(), EVENT_COUNT as u64);
        assert_eq!(summary.source_page_reads(), 18);
        assert_eq!(summary.peak_page_events(), CAPTURED_EVENT_PAGE_SIZE);
        assert!(summary.peak_packet_bytes() < 4 * 1024);
        assert_eq!(summary.bytes_written(), writer.bytes);
        assert_eq!(summary.packet_count(), writer.calls);
    });
}

#[test]
fn typed_captured_sources_are_deterministic_and_cover_boundaries() {
    let empty = with_active_capture("fixture-empty", Vec::new(), |source| {
        dump_bytes(source, None).0
    });
    let open = with_active_capture("fixture-open", open_events(), |source| {
        let (bytes, packets) = dump_bytes(source, None);
        assert!(
            bytes
                .windows(b"troupe.capture_boundary".len())
                .any(|window| { window == b"troupe.capture_boundary" })
        );
        assert_eq!(packets, 5);
        bytes
    });
    let nested = with_active_capture("fixture-nested", nested_events(), |source| {
        dump_bytes(source, None).0
    });
    let multi_cue = with_active_capture("fixture-multi-cue", multi_cue_events(), |source| {
        dump_bytes(source, None).0
    });
    let overlap = with_active_capture("fixture-overlap", overlap_events(), |source| {
        dump_bytes(source, None).0
    });
    let numeric = with_active_capture(
        "fixture-numeric-boundary",
        numeric_boundary_events(),
        |source| dump_bytes(source, None).0,
    );
    let watermark_events = vec![counter(1, 1), counter(2, 2), counter(3, 3)];
    let active_watermark = with_active_capture(
        "fixture-active-watermark",
        watermark_events.clone(),
        |source| dump_bytes(source, Some(2)).0,
    );
    let archive_watermark =
        with_archive_capture("fixture-archive-watermark", watermark_events, |source| {
            dump_bytes(source, Some(2)).0
        });
    let repeated_dump =
        with_active_capture("fixture-repeated-dump", multi_cue_events(), |source| {
            let first = dump_bytes(source, None).0;
            let second = dump_bytes(source, None).0;
            assert_eq!(first, second);
            first
        });

    let active_metadata = String::from_utf8_lossy(&active_watermark);
    assert!(active_metadata.contains("captured_watermark=3 | exported_through=2"));
    assert!(active_metadata.contains("outcome=unavailable | clean_shutdown=unavailable"));
    assert!(active_metadata.contains("exporter_schema=1 | event_schema=1"));
    assert!(active_metadata.contains("content_warning=trace may contain sensitive"));
    let archive_metadata = String::from_utf8_lossy(&archive_watermark);
    assert!(archive_metadata.contains("captured_watermark=3 | exported_through=2"));
    assert!(archive_metadata.contains("outcome=completed | clean_shutdown=true"));

    let traces = [
        active_watermark.as_slice(),
        archive_watermark.as_slice(),
        empty.as_slice(),
        multi_cue.as_slice(),
        nested.as_slice(),
        numeric.as_slice(),
        open.as_slice(),
        overlap.as_slice(),
        repeated_dump.as_slice(),
    ];
    assert!(traces.iter().all(|trace| !trace.is_empty()));
    assert_eq!(multi_cue, repeated_dump);
    assert_ne!(empty, open);
}
