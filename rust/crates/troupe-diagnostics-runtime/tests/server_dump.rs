use std::{
    collections::BTreeMap,
    convert::Infallible,
    fs,
    io::{Read, Write},
    net::{Shutdown, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use hyper::StatusCode;
use rusqlite::params;
use serde_json::{Value, json};
use tokio::io::{AsyncWrite, AsyncWriteExt as _};
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
    archive::lease::{ActiveArchiveLease, ArchiveLeaseErrorCode, CleanupArchiveLease},
    query::reader::CapturedEventSource,
    registry::process_identity::ProcessIdentity,
    server::{
        dump::{
            CapturedPrefixDumpProducer, DUMP_API_SCHEMA_VERSION_HEADER,
            DUMP_CAPTURED_WATERMARK_HEADER, DUMP_CLEAN_SHUTDOWN_HEADER,
            DUMP_CONTENT_WARNING_HEADER, DUMP_EVENT_SCHEMA_VERSION_HEADER,
            DUMP_EXPORTED_THROUGH_HEADER, DUMP_EXPORTER_SCHEMA_VERSION_HEADER,
            DUMP_PRODUCTION_OUTCOME_HEADER, DUMP_RUN_ID_HEADER, DUMP_TROUPE_VERSION_HEADER,
            DumpEndpoints, DumpProducerError, DumpProducerFuture, DumpProducerMetadata,
            PERFETTO_TRACE_MIME,
        },
        routes::{RouteDefinition, RouteResponse},
        runtime::{DiagnosticServer, ServerConfig},
    },
    store::{
        batch::EventBatch,
        connection::{DiagnosticStore, InitialStoreMetadata},
        writer::TransactionalWriter,
    },
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
const OTHER_RUN_ID: &str = "87654321-4321-4321-8321-cba987654321";
const TROUPE_VERSION: &str = "0.1.0";
const CONTENT_WARNING: &str =
    "trace may contain sensitive diagnostic metadata and user-provided attributes";
const WAIT: Duration = Duration::from_secs(3);

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestRunDirectory(PathBuf);

impl TestRunDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "troupe-h05-server-dump-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create test Run directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn entries(&self) -> Vec<String> {
        let mut entries = fs::read_dir(self.path())
            .expect("read Run directory")
            .map(|entry| {
                entry
                    .expect("read Run entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }
}

impl Drop for TestRunDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Copy, Debug, Default)]
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

fn run_id() -> CanonicalUuid {
    CanonicalUuid::parse(RUN_ID).expect("canonical test Run UUID")
}

fn other_run_id() -> CanonicalUuid {
    CanonicalUuid::parse(OTHER_RUN_ID).expect("canonical other Run UUID")
}

fn hub() -> ProductionDiagnosticHub<AcceptAll> {
    ProductionDiagnosticHub::production(run_id(), AcceptAll, Box::new(IgnoreLive))
}

fn accepted_event(hub: &ProductionDiagnosticHub<AcceptAll>) -> AcceptedDiagnosticEvent {
    hub.admit(
        |identity: EventIdentity| {
            let header = DiagnosticEventHeader::new(
                identity.run_id(),
                identity.sequence(),
                ElapsedNs::new(identity.sequence().get() * 10),
                DiagnosticScope::new(None, None, None, None, None, None, None),
                Vec::new(),
            )
            .expect("valid event header");
            DiagnosticEvent::CounterSampled(CounterSampled::new(
                header,
                CounterKind::AgentTurnActive,
                identity.sequence(),
            ))
        },
        None,
    )
    .expect("admit event")
    .accepted()
    .clone()
}

fn create_store(directory: &Path) -> DiagnosticStore {
    DiagnosticStore::create(
        directory,
        &InitialStoreMetadata::new(run_id(), "2026-08-16T00:00:00Z", "configuration-sha256:h05"),
    )
    .expect("create diagnostic store")
}

struct ActiveRun {
    directory: TestRunDirectory,
    lease: Arc<ActiveArchiveLease>,
    writer: TransactionalWriter<()>,
    hub: ProductionDiagnosticHub<AcceptAll>,
}

impl ActiveRun {
    fn new(label: &str, event_count: usize) -> Self {
        let directory = TestRunDirectory::new(label);
        let lease =
            Arc::new(ActiveArchiveLease::acquire(directory.path()).expect("active archive lease"));
        let writer =
            TransactionalWriter::new(create_store(directory.path()), ()).expect("construct writer");
        let hub = hub();
        let mut run = Self {
            directory,
            lease,
            writer,
            hub,
        };
        run.append(event_count);
        run
    }

    fn append(&mut self, event_count: usize) {
        if event_count == 0 {
            return;
        }
        let events = (0..event_count)
            .map(|_| accepted_event(&self.hub))
            .collect::<Vec<_>>();
        self.writer
            .commit_batch(&EventBatch::new(events).expect("nonempty batch"))
            .expect("commit events");
    }
}

fn build_archive(
    label: &str,
    event_count: usize,
    ended_at: Option<&str>,
    outcome: Option<&str>,
    clean_shutdown: bool,
) -> TestRunDirectory {
    let run = ActiveRun::new(label, event_count);
    let store = run.writer.into_store();
    store
        .connection()
        .execute(
            "UPDATE run_metadata SET ended_at = ?1, production_outcome = ?2, \
             clean_shutdown = ?3 WHERE singleton = 1",
            params![ended_at, outcome, i64::from(clean_shutdown)],
        )
        .expect("set archive completion metadata");
    drop(store);
    drop(run.lease);
    run.directory
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ProbeCounts {
    started: u64,
    finished: u64,
    writer_attempts: u64,
    writer_completions: u64,
    preflight_pages: u64,
    emit_pages: u64,
}

#[derive(Default)]
struct ProducerProbe {
    counts: Mutex<ProbeCounts>,
    changed: Condvar,
}

impl ProducerProbe {
    fn update(&self, update: impl FnOnce(&mut ProbeCounts)) {
        update(&mut self.counts.lock().expect("producer probe lock"));
        self.changed.notify_all();
    }

    fn counts(&self) -> ProbeCounts {
        *self.counts.lock().expect("producer probe lock")
    }

    fn wait_for(&self, predicate: impl Fn(ProbeCounts) -> bool) -> ProbeCounts {
        let deadline = Instant::now() + WAIT;
        let mut counts = self.counts.lock().expect("producer probe lock");
        while !predicate(*counts) {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("timed out waiting for producer state");
            let (next, timeout) = self
                .changed
                .wait_timeout(counts, remaining)
                .expect("producer probe wait");
            counts = next;
            assert!(!timeout.timed_out(), "timed out waiting for producer state");
        }
        *counts
    }
}

struct RequestFinished<'probe>(&'probe ProducerProbe);

impl Drop for RequestFinished<'_> {
    fn drop(&mut self) {
        self.0.update(|counts| counts.finished += 1);
    }
}

#[derive(Default)]
struct PreflightBarrier {
    state: Mutex<BarrierState>,
    changed: Condvar,
}

#[derive(Default)]
struct BarrierState {
    reached: bool,
    released: bool,
}

impl PreflightBarrier {
    fn reach_and_wait(&self) {
        let mut state = self.state.lock().expect("barrier lock");
        state.reached = true;
        self.changed.notify_all();
        while !state.released {
            state = self.changed.wait(state).expect("barrier wait");
        }
    }

    fn wait_until_reached(&self) {
        let deadline = Instant::now() + WAIT;
        let mut state = self.state.lock().expect("barrier lock");
        while !state.reached {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("timed out waiting for preflight barrier");
            let (next, timeout) = self
                .changed
                .wait_timeout(state, remaining)
                .expect("barrier wait");
            state = next;
            assert!(!timeout.timed_out(), "preflight barrier was not reached");
        }
    }

    fn release(&self) {
        let mut state = self.state.lock().expect("barrier lock");
        state.released = true;
        self.changed.notify_all();
    }
}

struct FakeProducer {
    metadata: DumpProducerMetadata,
    probe: Arc<ProducerProbe>,
    barrier: Option<Arc<PreflightBarrier>>,
    fail_preflight: bool,
    padding_bytes: usize,
    trailing_chunks: usize,
}

impl FakeProducer {
    fn new(probe: Arc<ProducerProbe>) -> Self {
        Self {
            metadata: DumpProducerMetadata::new(1, TROUPE_VERSION, CONTENT_WARNING)
                .expect("valid producer metadata"),
            probe,
            barrier: None,
            fail_preflight: false,
            padding_bytes: 0,
            trailing_chunks: 0,
        }
    }

    fn with_barrier(mut self, barrier: Arc<PreflightBarrier>) -> Self {
        self.barrier = Some(barrier);
        self
    }

    fn failing_preflight(mut self) -> Self {
        self.fail_preflight = true;
        self
    }

    fn with_padding(mut self, padding_bytes: usize) -> Self {
        self.padding_bytes = padding_bytes;
        self
    }

    fn with_trailing_chunks(mut self, trailing_chunks: usize) -> Self {
        self.trailing_chunks = trailing_chunks;
        self
    }
}

impl CapturedPrefixDumpProducer for FakeProducer {
    fn metadata(&self) -> &DumpProducerMetadata {
        &self.metadata
    }

    fn dump<'operation>(
        &'operation self,
        source: &'operation CapturedEventSource<'_>,
        writer: &'operation mut (dyn AsyncWrite + Unpin),
        through: Option<SchemaU64>,
    ) -> DumpProducerFuture<'operation> {
        Box::pin(async move {
            self.probe.update(|counts| counts.started += 1);
            let _finished = RequestFinished(self.probe.as_ref());
            let captured = source.captured_watermark();
            let exported = through.unwrap_or(captured);
            scan_prefix(source, exported, &self.probe)?;
            if let Some(barrier) = &self.barrier {
                barrier.reach_and_wait();
            }
            if self.fail_preflight {
                return Err(DumpProducerError::new(
                    "fake_preflight_failed",
                    "injected preflight failure",
                ));
            }

            let metadata = source.metadata();
            let outcome = metadata.production_outcome().unwrap_or("unavailable");
            let clean_shutdown = if metadata.ended_at().is_some() {
                if metadata.clean_shutdown() {
                    "true"
                } else {
                    "false"
                }
            } else {
                "unavailable"
            };
            let prelude = format!(
                "trace_metadata|exporter_schema={}|event_schema={}|run_id={}|captured_watermark={}|exported_through={}|troupe_version={}|outcome={}|clean_shutdown={}|content_warning={}\n",
                self.metadata.exporter_schema_version(),
                metadata.event_schema_version(),
                metadata.run_id(),
                captured.get(),
                exported.get(),
                self.metadata.troupe_version(),
                outcome,
                clean_shutdown,
                self.metadata.content_warning(),
            );
            write_bytes(writer, prelude.as_bytes(), &self.probe).await?;

            let mut after = SchemaU64::new(0);
            while after.get() < exported.get() {
                let page = source.read_event_page(after).map_err(reader_error)?;
                self.probe.update(|counts| counts.emit_pages += 1);
                let mut reached = false;
                for event in page.events() {
                    if event.sequence().get() > exported.get() {
                        reached = true;
                        break;
                    }
                    let mut line =
                        format!("event_sequence={}|", event.sequence().get()).into_bytes();
                    line.extend_from_slice(event.canonical_bytes());
                    line.extend(std::iter::repeat_n(b'x', self.padding_bytes));
                    line.push(b'\n');
                    write_bytes(writer, &line, &self.probe).await?;
                    after = event.sequence();
                    if after == exported {
                        reached = true;
                        break;
                    }
                }
                if reached || page.events().is_empty() {
                    break;
                }
            }

            let trailing = vec![b'z'; 64 * 1024];
            for _ in 0..self.trailing_chunks {
                write_bytes(writer, &trailing, &self.probe).await?;
            }
            writer.flush().await.map_err(writer_error)
        })
    }
}

fn scan_prefix(
    source: &CapturedEventSource<'_>,
    through: SchemaU64,
    probe: &ProducerProbe,
) -> Result<(), DumpProducerError> {
    let mut after = SchemaU64::new(0);
    let mut expected = 1_u64;
    while after.get() < through.get() {
        let page = source.read_event_page(after).map_err(reader_error)?;
        probe.update(|counts| counts.preflight_pages += 1);
        let mut reached = false;
        for event in page.events() {
            if event.sequence().get() > through.get() {
                reached = true;
                break;
            }
            if event.sequence().get() != expected {
                return Err(DumpProducerError::new(
                    "fake_dense_prefix",
                    "captured prefix is not dense",
                ));
            }
            expected += 1;
            after = event.sequence();
            if after == through {
                reached = true;
                break;
            }
        }
        if reached || page.events().is_empty() {
            break;
        }
    }
    if expected.saturating_sub(1) != through.get() {
        return Err(DumpProducerError::new(
            "fake_watermark_mismatch",
            "captured prefix ended before through",
        ));
    }
    Ok(())
}

async fn write_bytes(
    writer: &mut (dyn AsyncWrite + Unpin),
    bytes: &[u8],
    probe: &ProducerProbe,
) -> Result<(), DumpProducerError> {
    probe.update(|counts| counts.writer_attempts += 1);
    writer.write_all(bytes).await.map_err(writer_error)?;
    probe.update(|counts| counts.writer_completions += 1);
    Ok(())
}

fn reader_error(error: impl std::fmt::Display) -> DumpProducerError {
    DumpProducerError::new("fake_reader_failed", error.to_string())
}

fn writer_error(error: std::io::Error) -> DumpProducerError {
    DumpProducerError::new("fake_writer_failed", error.to_string())
}

fn process_identity() -> ProcessIdentity {
    ProcessIdentity::new("test", "h05:4242").expect("valid process identity")
}

fn start_server(endpoint: DumpEndpoints) -> DiagnosticServer {
    start_server_for(run_id(), endpoint)
}

fn start_server_for(identity: CanonicalUuid, endpoint: DumpEndpoints) -> DiagnosticServer {
    let mut routes = endpoint.route_definitions().expect("valid dump route");
    routes.push(
        RouteDefinition::get("/api/v1/ping", |_request| async {
            Ok(RouteResponse::bytes(StatusCode::OK, "pong"))
        })
        .expect("valid ping route"),
    );
    DiagnosticServer::start(
        ServerConfig::new(identity, std::process::id(), process_identity())
            .with_bind("127.0.0.1", 0),
        routes,
    )
    .expect("start diagnostic server")
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    headers: BTreeMap<String, Vec<String>>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .and_then(|values| values.first())
            .map(String::as_str)
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("HTTP response JSON")
    }
}

fn request(
    server: &DiagnosticServer,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> HttpResponse {
    request_addr(server.connect_addr(), method, path, headers)
}

fn request_addr(
    address: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> HttpResponse {
    let mut stream = TcpStream::connect(address).expect("connect to HTTP server");
    stream.set_read_timeout(Some(WAIT)).expect("read timeout");
    stream.set_write_timeout(Some(WAIT)).expect("write timeout");
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: diagnostics.test\r\nConnection: close\r\n"
    )
    .expect("write request line");
    for (name, value) in headers {
        write!(stream, "{name}: {value}\r\n").expect("write request header");
    }
    write!(stream, "\r\n").expect("terminate request headers");
    stream.flush().expect("flush request");
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).expect("read HTTP response");
    parse_response(&bytes)
}

fn open_request(address: SocketAddr, path: &str) -> TcpStream {
    let mut stream = TcpStream::connect(address).expect("connect streaming request");
    stream.set_read_timeout(Some(WAIT)).expect("read timeout");
    stream.set_write_timeout(Some(WAIT)).expect("write timeout");
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: diagnostics.test\r\nConnection: close\r\n\r\n"
    )
    .expect("write streaming request");
    stream.flush().expect("flush streaming request");
    stream
}

fn parse_response(bytes: &[u8]) -> HttpResponse {
    let header_end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP header terminator");
    let head = std::str::from_utf8(&bytes[..header_end]).expect("ASCII HTTP head");
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .expect("HTTP status line")
        .split_ascii_whitespace()
        .nth(1)
        .expect("HTTP status code")
        .parse()
        .expect("numeric HTTP status code");
    let mut headers = BTreeMap::<String, Vec<String>>::new();
    for line in lines {
        let (name, value) = line.split_once(':').expect("HTTP response header");
        headers
            .entry(name.to_ascii_lowercase())
            .or_default()
            .push(value.trim().to_owned());
    }
    let mut body = bytes[header_end + 4..].to_vec();
    if headers
        .get("transfer-encoding")
        .is_some_and(|values| values.iter().any(|value| value == "chunked"))
    {
        body = decode_chunked(&body);
    }
    HttpResponse {
        status,
        headers,
        body,
    }
}

fn decode_chunked(bytes: &[u8]) -> Vec<u8> {
    let mut remaining = bytes;
    let mut decoded = Vec::new();
    loop {
        let line_end = remaining
            .windows(2)
            .position(|window| window == b"\r\n")
            .expect("chunk size line");
        let size = usize::from_str_radix(
            std::str::from_utf8(&remaining[..line_end])
                .expect("ASCII chunk size")
                .split(';')
                .next()
                .expect("chunk size"),
            16,
        )
        .expect("hexadecimal chunk size");
        remaining = &remaining[line_end + 2..];
        if size == 0 {
            break;
        }
        decoded.extend_from_slice(&remaining[..size]);
        assert_eq!(&remaining[size..size + 2], b"\r\n");
        remaining = &remaining[size + 2..];
    }
    decoded
}

fn fixture(name: &str) -> &'static [u8] {
    match name {
        "dump-metadata-v1.json" => include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../tests/fixtures/diagnostics/http/dump-metadata-v1.json"
        )),
        "dump-error-v1.json" => include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../tests/fixtures/diagnostics/http/dump-error-v1.json"
        )),
        _ => panic!("unknown dump fixture {name}"),
    }
}

fn fixture_body(name: &str) -> &'static [u8] {
    fixture(name).strip_suffix(b"\n").unwrap_or(fixture(name))
}

fn metadata_json(response: &HttpResponse) -> Value {
    json!({
        "content_type": response.header("content-type"),
        "run_id": response.header(DUMP_RUN_ID_HEADER),
        "captured_watermark": response.header(DUMP_CAPTURED_WATERMARK_HEADER),
        "exported_through": response.header(DUMP_EXPORTED_THROUGH_HEADER),
        "api_schema_version": response.header(DUMP_API_SCHEMA_VERSION_HEADER),
        "event_schema_version": response.header(DUMP_EVENT_SCHEMA_VERSION_HEADER),
        "exporter_schema_version": response.header(DUMP_EXPORTER_SCHEMA_VERSION_HEADER),
        "troupe_version": response.header(DUMP_TROUPE_VERSION_HEADER),
        "production_outcome": response.header(DUMP_PRODUCTION_OUTCOME_HEADER),
        "clean_shutdown": response.header(DUMP_CLEAN_SHUTDOWN_HEADER),
        "content_warning": response.header(DUMP_CONTENT_WARNING_HEADER),
    })
}

fn assert_dump_response(response: &HttpResponse) {
    assert_eq!(response.status, 200);
    assert_eq!(response.header("content-type"), Some(PERFETTO_TRACE_MIME));
    assert_eq!(response.header("cache-control"), Some("no-store"));
    assert!(
        response
            .headers
            .keys()
            .all(|name| !name.starts_with("access-control-"))
    );
}

fn assert_error(response: &HttpResponse, status: u16, code: &str) {
    assert_error_for(response, status, code, RUN_ID);
}

fn assert_error_for(response: &HttpResponse, status: u16, code: &str, expected_run_id: &str) {
    assert_eq!(response.status, status);
    assert_eq!(
        response.header("content-type"),
        Some("application/json; charset=utf-8")
    );
    assert_eq!(response.header("cache-control"), Some("no-store"));
    let body = response.json();
    assert_eq!(body["api_schema_version"], 1);
    assert_eq!(body["run_id"], expected_run_id);
    assert_eq!(body["error"]["code"], code);
    assert_eq!(body["error"]["details"], Value::Null);
}

fn wait_for_cleanup_lease(run_directory: &Path) -> CleanupArchiveLease {
    let deadline = Instant::now() + WAIT;
    loop {
        match CleanupArchiveLease::acquire(run_directory) {
            Ok(lease) => return lease,
            Err(error)
                if error.code() == ArchiveLeaseErrorCode::Contended
                    && Instant::now() < deadline =>
            {
                std::thread::yield_now();
            }
            Err(error) => panic!("archive request did not release its lease: {error:?}"),
        }
    }
}

#[test]
fn completed_archive_metadata_and_stable_through_match_the_golden() {
    let archive = build_archive(
        "metadata",
        2,
        Some("2026-08-16T00:01:00Z"),
        Some("failed"),
        true,
    );
    let before = archive.entries();
    let probe = Arc::new(ProducerProbe::default());
    let endpoint = DumpEndpoints::archive(
        run_id(),
        archive.path(),
        FakeProducer::new(Arc::clone(&probe)),
    );
    let server = start_server(endpoint);

    let first = request(
        &server,
        "GET",
        "/api/v1/dump?through=1",
        &[("Accept", PERFETTO_TRACE_MIME)],
    );
    let second = request(&server, "GET", "/api/v1/dump?through=1", &[]);
    assert_dump_response(&first);
    assert_dump_response(&second);
    assert_eq!(first.body, second.body);
    assert_eq!(
        metadata_json(&first),
        serde_json::from_slice::<Value>(fixture("dump-metadata-v1.json"))
            .expect("metadata fixture JSON")
    );
    let trace = std::str::from_utf8(&first.body).expect("fake trace UTF-8");
    assert!(trace.contains("captured_watermark=2|exported_through=1"));
    assert!(trace.contains("outcome=failed|clean_shutdown=true"));
    assert!(trace.contains(&format!("content_warning={CONTENT_WARNING}")));
    assert_eq!(trace.matches("event_sequence=").count(), 1);
    assert_eq!(archive.entries(), before, "dump created a Run file");
    assert_eq!(probe.counts().finished, 2);

    server.shutdown().expect("clean server shutdown");
}

#[test]
fn archive_shared_lease_is_held_for_the_request_and_incomplete_is_explicit() {
    let archive = build_archive("archive-lease", 1, None, None, false);
    let probe = Arc::new(ProducerProbe::default());
    let barrier = Arc::new(PreflightBarrier::default());
    let endpoint = DumpEndpoints::archive(
        run_id(),
        archive.path(),
        FakeProducer::new(Arc::clone(&probe)).with_barrier(Arc::clone(&barrier)),
    );
    let server = start_server(endpoint);
    let address = server.connect_addr();
    let request_thread =
        std::thread::spawn(move || request_addr(address, "GET", "/api/v1/dump", &[]));
    barrier.wait_until_reached();

    let contended = CleanupArchiveLease::acquire(archive.path()).unwrap_err();
    assert_eq!(contended.code(), ArchiveLeaseErrorCode::Contended);
    barrier.release();
    let response = request_thread.join().expect("join archive request");
    assert_dump_response(&response);
    assert_eq!(
        response.header(DUMP_PRODUCTION_OUTCOME_HEADER),
        Some("unavailable")
    );
    assert_eq!(
        response.header(DUMP_CLEAN_SHUTDOWN_HEADER),
        Some("unavailable")
    );
    drop(wait_for_cleanup_lease(archive.path()));
    assert_eq!(probe.counts().finished, 1);

    server.shutdown().expect("clean server shutdown");
}

#[test]
fn preflight_failure_is_closed_before_any_writer_poll_and_routes_survive() {
    let archive = build_archive("preflight-error", 2, None, None, false);
    let probe = Arc::new(ProducerProbe::default());
    let endpoint = DumpEndpoints::archive(
        run_id(),
        archive.path(),
        FakeProducer::new(Arc::clone(&probe)).failing_preflight(),
    );
    let server = start_server(endpoint);

    let response = request(&server, "GET", "/api/v1/dump", &[]);
    assert_eq!(response.body, fixture_body("dump-error-v1.json"));
    assert_error(&response, 500, "dump_failed");
    let counts = probe.counts();
    assert_eq!(counts.started, 1);
    assert_eq!(counts.finished, 1);
    assert_eq!(counts.writer_attempts, 0);
    assert_eq!(counts.writer_completions, 0);

    let ping = request(&server, "GET", "/api/v1/ping", &[]);
    assert_eq!(ping.status, 200);
    assert_eq!(ping.body, b"pong");
    assert!(server.try_core_failure().is_none());
    server.shutdown().expect("clean server shutdown");
}

#[test]
fn run_identity_mismatch_is_closed_before_the_producer_starts() {
    let archive = build_archive("identity-mismatch", 1, None, None, false);
    let probe = Arc::new(ProducerProbe::default());
    let endpoint = DumpEndpoints::archive(
        other_run_id(),
        archive.path(),
        FakeProducer::new(Arc::clone(&probe)),
    );
    let server = start_server_for(other_run_id(), endpoint);

    let response = request(&server, "GET", "/api/v1/dump", &[]);
    assert_error_for(&response, 500, "dump_failed", OTHER_RUN_ID);
    assert_eq!(probe.counts().started, 0);
    let ping = request(&server, "GET", "/api/v1/ping", &[]);
    assert_eq!(ping.status, 200);
    assert_eq!(ping.body, b"pong");

    server.shutdown().expect("clean server shutdown");
}

#[test]
fn request_surface_rejects_noncanonical_future_and_filesystem_controls() {
    let archive = build_archive("validation", 2, None, None, false);
    let probe = Arc::new(ProducerProbe::default());
    let endpoint = DumpEndpoints::archive(
        run_id(),
        archive.path(),
        FakeProducer::new(Arc::clone(&probe)),
    );
    let server = start_server(endpoint);

    for path in [
        "/api/v1/dump?path=/tmp/trace.pftrace",
        "/api/v1/dump?output=trace.pftrace",
        "/api/v1/dump?force=true",
        "/api/v1/dump?overwrite=1",
        "/api/v1/dump?through=1&path=trace.pftrace",
        "/api/v1/dump?through=1&through=1",
    ] {
        assert_error(&request(&server, "GET", path, &[]), 400, "invalid_query");
    }
    for value in ["01", "-1", "+1", "18446744073709551616"] {
        assert_error(
            &request(
                &server,
                "GET",
                &format!("/api/v1/dump?through={value}"),
                &[],
            ),
            400,
            if value == "+1" {
                "invalid_query"
            } else {
                "invalid_through"
            },
        );
    }
    assert_error(
        &request(&server, "GET", "/api/v1/dump?through=3", &[]),
        409,
        "through_not_captured",
    );
    assert_error(
        &request(&server, "GET", "/api/v1/dump", &[("Accept", "text/plain")]),
        406,
        "unsupported_format",
    );
    assert_eq!(request(&server, "HEAD", "/api/v1/dump", &[]).status, 405);
    assert_eq!(request(&server, "POST", "/api/v1/dump", &[]).status, 405);
    assert_eq!(probe.counts().started, 0);

    let zero = request(&server, "GET", "/api/v1/dump?through=0", &[]);
    assert_dump_response(&zero);
    assert_eq!(zero.header(DUMP_EXPORTED_THROUGH_HEADER), Some("0"));
    assert_eq!(
        std::str::from_utf8(&zero.body)
            .expect("fake trace UTF-8")
            .matches("event_sequence=")
            .count(),
        0
    );
    server.shutdown().expect("clean server shutdown");
}

#[test]
fn active_capture_is_stable_across_concurrent_commit_and_never_reacquires() {
    let mut run = ActiveRun::new("active-stable", 2);
    let probe = Arc::new(ProducerProbe::default());
    let barrier = Arc::new(PreflightBarrier::default());
    let endpoint = DumpEndpoints::active(
        run_id(),
        Arc::clone(&run.lease),
        FakeProducer::new(Arc::clone(&probe)).with_barrier(Arc::clone(&barrier)),
    );
    let server = start_server(endpoint);
    let address = server.connect_addr();
    let request_thread =
        std::thread::spawn(move || request_addr(address, "GET", "/api/v1/dump", &[]));
    barrier.wait_until_reached();

    run.append(1);
    barrier.release();
    let captured = request_thread.join().expect("join active request");
    assert_dump_response(&captured);
    assert_eq!(captured.header(DUMP_CAPTURED_WATERMARK_HEADER), Some("2"));
    let captured_trace = std::str::from_utf8(&captured.body).expect("fake trace UTF-8");
    assert_eq!(captured_trace.matches("event_sequence=").count(), 2);
    assert!(!captured_trace.contains("event_sequence=3|"));

    let later = request(&server, "GET", "/api/v1/dump", &[]);
    assert_dump_response(&later);
    assert_eq!(later.header(DUMP_CAPTURED_WATERMARK_HEADER), Some("3"));
    assert_eq!(
        std::str::from_utf8(&later.body)
            .expect("fake trace UTF-8")
            .matches("event_sequence=")
            .count(),
        3
    );

    let contended = CleanupArchiveLease::acquire(run.directory.path()).unwrap_err();
    assert_eq!(contended.code(), ArchiveLeaseErrorCode::Contended);
    assert!(server.try_core_failure().is_none());
    server.shutdown().expect("clean server shutdown");
}

#[test]
fn large_stream_is_paged_and_disconnect_releases_only_the_archive_request() {
    let archive = build_archive("large", 1_025, None, None, false);
    let large_probe = Arc::new(ProducerProbe::default());
    let large_server = start_server(DumpEndpoints::archive(
        run_id(),
        archive.path(),
        FakeProducer::new(Arc::clone(&large_probe)).with_padding(96),
    ));
    let large = request(&large_server, "GET", "/api/v1/dump", &[]);
    assert_dump_response(&large);
    assert_eq!(
        std::str::from_utf8(&large.body)
            .expect("fake trace UTF-8")
            .matches("event_sequence=")
            .count(),
        1_025
    );
    assert!(large.body.len() > 64 * 1024);
    let counts = large_probe.counts();
    assert!(counts.preflight_pages >= 3);
    assert!(counts.emit_pages >= 3);
    large_server
        .shutdown()
        .expect("clean large server shutdown");

    let disconnect_probe = Arc::new(ProducerProbe::default());
    let disconnect_barrier = Arc::new(PreflightBarrier::default());
    let disconnect_server = start_server(DumpEndpoints::archive(
        run_id(),
        archive.path(),
        FakeProducer::new(Arc::clone(&disconnect_probe))
            .with_barrier(Arc::clone(&disconnect_barrier))
            .with_trailing_chunks(100_000),
    ));
    let socket = open_request(disconnect_server.connect_addr(), "/api/v1/dump");
    disconnect_barrier.wait_until_reached();
    let contended = CleanupArchiveLease::acquire(archive.path()).unwrap_err();
    assert_eq!(contended.code(), ArchiveLeaseErrorCode::Contended);
    socket.shutdown(Shutdown::Both).expect("disconnect client");
    drop(socket);
    disconnect_barrier.release();
    disconnect_probe.wait_for(|counts| counts.finished == 1);
    drop(wait_for_cleanup_lease(archive.path()));

    let ping = request(&disconnect_server, "GET", "/api/v1/ping", &[]);
    assert_eq!(ping.status, 200);
    assert_eq!(ping.body, b"pong");
    assert!(disconnect_server.try_core_failure().is_none());
    disconnect_server
        .shutdown()
        .expect("clean disconnect server shutdown");
}

#[test]
fn metadata_constructor_rejects_values_that_cannot_be_response_headers() {
    assert!(DumpProducerMetadata::new(0, TROUPE_VERSION, CONTENT_WARNING).is_err());
    assert!(DumpProducerMetadata::new(1, "", CONTENT_WARNING).is_err());
    assert!(DumpProducerMetadata::new(1, TROUPE_VERSION, "bad\nwarning").is_err());
}
