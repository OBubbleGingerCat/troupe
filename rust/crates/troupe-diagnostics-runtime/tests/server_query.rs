use std::{
    collections::BTreeMap,
    convert::Infallible,
    fs,
    io::{Read, Write},
    net::{Shutdown, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use rusqlite::{Connection, params};
use serde_json::Value;
use troupe_diagnostics_core::{
    event::{
        ActTokenUsageFinalized, CounterSampled, DiagnosticEvent, DiagnosticEventHeader,
        DiagnosticScope,
    },
    hub::{
        AcceptedDiagnosticEvent, AdmissionReservation, AdmissionReserver, AdmissionSize,
        DeliveryFailure, EventIdentity, LiveEventNotifier, MandatoryDurableReserver,
        ProductionDiagnosticHub,
    },
    id::{CanonicalUuid, RunLocalId},
    kinds::{CounterKind, UsageAvailability, UsageSource},
    scalar::{SchemaU64, TokenCount},
    time::ElapsedNs,
};
use troupe_diagnostics_runtime::{
    archive::lease::ActiveArchiveLease,
    query::{
        events::FiniteEventQuery,
        reader::{DiagnosticReader, ReaderErrorCode, ReaderFailureClass, ReaderProfile},
        snapshot::SnapshotQueryErrorCode,
        status::{ActiveStatusObservation, StatusProjectionError},
    },
    registry::{model::WebBaseUrl, process_identity::ProcessIdentity},
    server::{
        query::{
            QueryCoreFailureSignal, QueryEndpointError, QueryEndpointKind, QueryEndpoints,
            QueryFailureCode, encode_events_response, encode_snapshot_response,
        },
        runtime::{DiagnosticServer, ServerConfig},
    },
    store::{
        admission::MandatoryIngress,
        batch::EventBatch,
        connection::{DiagnosticStore, InitialStoreMetadata, StoreOpenErrorCode},
        key::SortableU64Key,
        progress::WriterProgressSupervisor,
        quota::RunQuota,
        schema::DIAGNOSTIC_DATABASE_FILENAME,
        writer::TransactionalWriter,
    },
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
const OTHER_RUN_ID: &str = "87654321-4321-4321-8321-cba987654321";
const LARGE_TOKEN_COUNT: &str = "1234567890123456789012345678901234567890";
const BASE_PATH: &str = "/troupe";

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestRunDirectory(PathBuf);

impl TestRunDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "troupe-h01-server-query-{label}-{}-{sequence}",
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

fn other_run_id() -> CanonicalUuid {
    CanonicalUuid::parse(OTHER_RUN_ID).expect("canonical other Run UUID")
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

fn header(identity: EventIdentity) -> DiagnosticEventHeader {
    DiagnosticEventHeader::new(
        identity.run_id(),
        identity.sequence(),
        ElapsedNs::new(identity.sequence().get() * 10),
        act_scope(),
        Vec::new(),
    )
    .expect("valid event header")
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

fn admit(
    hub: &ProductionDiagnosticHub<AcceptAll>,
    candidate: impl FnOnce(EventIdentity) -> DiagnosticEvent,
) -> AcceptedDiagnosticEvent {
    hub.admit(candidate, None)
        .expect("admit diagnostic event")
        .accepted()
        .clone()
}

fn usage_event(hub: &ProductionDiagnosticHub<AcceptAll>) -> AcceptedDiagnosticEvent {
    admit(hub, |identity| {
        DiagnosticEvent::ActTokenUsageFinalized(
            ActTokenUsageFinalized::new(
                header(identity),
                UsageAvailability::Available,
                Some(UsageSource::AcpPromptResponseUsage),
                None,
                Some(TokenCount::parse(LARGE_TOKEN_COUNT).expect("large token count")),
                Some(TokenCount::parse("42").expect("input tokens")),
                Some(TokenCount::parse("7").expect("output tokens")),
                Some(TokenCount::parse("3").expect("thought tokens")),
                Some(TokenCount::parse("2").expect("cached read tokens")),
                Some(TokenCount::parse("1").expect("cached write tokens")),
            )
            .expect("valid terminal usage"),
        )
    })
}

fn counter_event(hub: &ProductionDiagnosticHub<AcceptAll>) -> AcceptedDiagnosticEvent {
    admit(hub, |identity| {
        DiagnosticEvent::CounterSampled(CounterSampled::new(
            header(identity),
            CounterKind::AgentTurnActive,
            SchemaU64::new(u64::MAX),
        ))
    })
}

fn create_store(directory: &Path) -> DiagnosticStore {
    DiagnosticStore::create(
        directory,
        &InitialStoreMetadata::new(run_id(), "2026-08-16T00:00:00Z", "configuration-sha256:h01"),
    )
    .expect("create diagnostic store")
}

struct TestArchive {
    directory: TestRunDirectory,
    canonical_events: Vec<Vec<u8>>,
    canonical_snapshot: Vec<u8>,
}

fn build_archive(
    label: &str,
    ended_at: Option<&str>,
    outcome: Option<&str>,
    clean_shutdown: bool,
) -> TestArchive {
    let directory = TestRunDirectory::new(label);
    let active_lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    let mut writer =
        TransactionalWriter::new(create_store(directory.path()), ()).expect("construct writer");
    let hub = diagnostic_hub();
    let events = vec![usage_event(&hub), counter_event(&hub)];
    let canonical_events = events
        .iter()
        .map(|event| event.canonical_bytes().to_vec())
        .collect();
    writer
        .commit_batch(&EventBatch::new(events).expect("nonempty event batch"))
        .expect("commit HTTP fixture events");
    let canonical_snapshot = writer
        .snapshot()
        .canonical_json()
        .expect("canonical materialized snapshot");
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
    TestArchive {
        directory,
        canonical_events,
        canonical_snapshot,
    }
}

fn process_identity() -> ProcessIdentity {
    ProcessIdentity::new("test", "h01:4242").expect("valid process identity")
}

fn ignore_core_failure(_failure: QueryCoreFailureSignal) {}

fn start_archive_server(expected_run_id: CanonicalUuid, directory: &Path) -> DiagnosticServer {
    let endpoint = QueryEndpoints::archive(expected_run_id, directory);
    DiagnosticServer::start(
        ServerConfig::new(expected_run_id, std::process::id(), process_identity())
            .with_bind("127.0.0.1", 0)
            .with_advertise_url(Some(
                WebBaseUrl::parse("https://diagnostics.example/troupe/")
                    .expect("valid advertised URL"),
            )),
        endpoint.route_definitions().expect("valid query routes"),
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
    let mut stream = TcpStream::connect(server.connect_addr()).expect("connect to HTTP server");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("set write timeout");
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
    stream.read_to_end(&mut bytes).expect("read response");
    parse_response(&bytes)
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
        "status-v1.json" => include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../tests/fixtures/diagnostics/http/status-v1.json"
        )),
        "snapshot-v1.json" => include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../tests/fixtures/diagnostics/http/snapshot-v1.json"
        )),
        "events-v1.json" => include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../tests/fixtures/diagnostics/http/events-v1.json"
        )),
        "error-v1.json" => include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../tests/fixtures/diagnostics/http/error-v1.json"
        )),
        _ => panic!("unknown HTTP fixture {name}"),
    }
}

fn fixture_body(name: &str) -> &'static [u8] {
    fixture(name).strip_suffix(b"\n").unwrap_or(fixture(name))
}

fn assert_json_headers(response: &HttpResponse) {
    assert_eq!(
        response.header("content-type"),
        Some("application/json; charset=utf-8")
    );
    assert_eq!(response.header("cache-control"), Some("no-store"));
    assert!(
        response
            .headers
            .keys()
            .all(|name| !name.starts_with("access-control-"))
    );
}

fn assert_closed_error(response: &HttpResponse, status: u16, code: &str, expected_run: &str) {
    assert_eq!(response.status, status);
    assert_json_headers(response);
    let body = response.json();
    let mut keys = body
        .as_object()
        .expect("error envelope")
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(keys, ["api_schema_version", "error", "run_id"]);
    assert_eq!(body["api_schema_version"], 1);
    assert_eq!(body["run_id"], expected_run);
    assert_eq!(body["error"]["code"], code);
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|v| !v.is_empty())
    );
    assert_eq!(body["error"]["details"], Value::Null);
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

#[test]
fn archive_endpoints_match_goldens_and_preserve_canonical_payloads() {
    let archive = build_archive("golden", Some("2026-08-16T00:00:01Z"), Some("failed"), true);
    let server = start_archive_server(run_id(), archive.directory.path());

    let status = request(&server, "GET", &format!("{BASE_PATH}/api/v1/status"), &[]);
    let snapshot = request(&server, "GET", &format!("{BASE_PATH}/api/v1/snapshot"), &[]);
    let events = request(
        &server,
        "GET",
        &format!("{BASE_PATH}/api/v1/events?after=0"),
        &[],
    );
    let error = request(
        &server,
        "GET",
        &format!("{BASE_PATH}/api/v1/events?after=01"),
        &[],
    );

    for response in [&status, &snapshot, &events] {
        assert_eq!(response.status, 200);
        assert_json_headers(response);
    }
    assert_eq!(status.body, fixture_body("status-v1.json"));
    assert_eq!(snapshot.body, fixture_body("snapshot-v1.json"));
    assert_eq!(events.body, fixture_body("events-v1.json"));
    assert_eq!(error.body, fixture_body("error-v1.json"));
    assert_closed_error(&error, 400, "invalid_cursor", RUN_ID);

    assert!(contains_bytes(&snapshot.body, &archive.canonical_snapshot));
    for canonical in &archive.canonical_events {
        assert!(contains_bytes(&events.body, canonical));
    }
    let snapshot_json = snapshot.json();
    assert_eq!(snapshot_json["watermark_sequence"], "2");
    assert_eq!(snapshot_json["state"]["through_sequence"], "2");
    let events_json = events.json();
    assert_eq!(events_json["captured_watermark"], "2");
    assert_eq!(events_json["events"].as_array().unwrap().len(), 2);
    assert_eq!(events_json["next_after"], Value::Null);

    let head = request(&server, "HEAD", &format!("{BASE_PATH}/api/v1/status"), &[]);
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());
    let expected_content_length = status.body.len().to_string();
    assert_eq!(
        head.header("content-length"),
        Some(expected_content_length.as_str())
    );
    assert_json_headers(&head);
    assert_eq!(request(&server, "GET", "/api/v1/status", &[]).status, 404);
    assert!(server.try_core_failure().is_none());
    server.shutdown().expect("clean server shutdown");
}

#[test]
fn finite_event_queries_are_bounded_and_reject_invalid_inputs() {
    let archive = build_archive("finite", None, None, false);
    let server = start_archive_server(run_id(), archive.directory.path());
    let path = |query: &str| format!("{BASE_PATH}/api/v1/events{query}");

    for (query, expected_sequences) in [
        ("", vec!["1", "2"]),
        ("?after=1", vec!["2"]),
        ("?after=0&through=1", vec!["1"]),
        ("?after=1&through=2", vec!["2"]),
        ("?after=2&through=2", vec![]),
        ("?tail=1", vec!["2"]),
        ("?tail=0", vec![]),
        ("?after=18446744073709551615", vec![]),
    ] {
        let response = request(&server, "GET", &path(query), &[]);
        assert_eq!(response.status, 200, "query {query}");
        let body = response.json();
        assert_eq!(body["captured_watermark"], "2");
        let actual = body["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["sequence"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected_sequences, "query {query}");
    }

    for (query, status, code) in [
        ("?after=01", 400, "invalid_cursor"),
        ("?after=-1", 400, "invalid_cursor"),
        ("?after=18446744073709551616", 400, "invalid_cursor"),
        ("?after=0&through=01", 400, "invalid_cursor"),
        ("?after=2&through=1", 400, "invalid_cursor"),
        ("?after=0&through=3", 400, "invalid_cursor"),
        ("?through=1", 400, "invalid_query"),
        ("?after=0&tail=1", 400, "conflicting_event_query"),
        ("?tail=1&through=2", 400, "conflicting_event_query"),
        ("?after=0&after=1", 400, "invalid_query"),
        ("?after=0&through=1&through=2", 400, "invalid_query"),
        ("?after=0&limit=1", 400, "invalid_query"),
        ("?unknown=1", 400, "invalid_query"),
        ("?after=%31", 400, "invalid_query"),
        ("?format=jsonl", 406, "unsupported_format"),
    ] {
        assert_closed_error(
            &request(&server, "GET", &path(query), &[]),
            status,
            code,
            RUN_ID,
        );
    }
    assert_closed_error(
        &request(
            &server,
            "GET",
            &path(""),
            &[("Accept", "text/event-stream")],
        ),
        406,
        "unsupported_format",
        RUN_ID,
    );

    for endpoint in ["status", "snapshot"] {
        assert_closed_error(
            &request(
                &server,
                "GET",
                &format!("{BASE_PATH}/api/v1/{endpoint}?unexpected=1"),
                &[],
            ),
            400,
            "invalid_query",
            RUN_ID,
        );
    }

    server.shutdown().expect("clean server shutdown");
}

#[test]
fn failed_and_incomplete_archives_are_explicit_successful_queries() {
    for (label, ended_at, outcome, clean, expected_state) in [
        (
            "failed-status",
            Some("2026-08-16T00:00:01Z"),
            Some("failed"),
            true,
            "failed",
        ),
        ("incomplete-status", None, None, false, "incomplete"),
    ] {
        let archive = build_archive(label, ended_at, outcome, clean);
        let server = start_archive_server(run_id(), archive.directory.path());
        let status = request(&server, "GET", &format!("{BASE_PATH}/api/v1/status"), &[]);
        let snapshot = request(&server, "GET", &format!("{BASE_PATH}/api/v1/snapshot"), &[]);
        assert_eq!(status.status, 200);
        assert_eq!(snapshot.status, 200);
        assert_eq!(status.json()["lifecycle"]["state"], expected_state);
        assert_eq!(status.json()["lifecycle"]["clean_shutdown"], clean);
        assert_eq!(snapshot.json()["watermark_sequence"], "2");
        server.shutdown().expect("clean server shutdown");
    }
}

#[test]
fn active_status_serializes_available_live_observations() {
    let directory = TestRunDirectory::new("active-status");
    let lease = Arc::new(ActiveArchiveLease::acquire(directory.path()).expect("active lease"));
    let _store = create_store(directory.path());
    let (ingress, _) = MandatoryIngress::new();
    let progress = WriterProgressSupervisor::default();
    let (quota, _) = RunQuota::new(directory.path(), None).expect("disabled quota");
    let observation = ActiveStatusObservation::available(
        ingress.status().expect("ingress status"),
        progress.status(),
        quota.status().expect("quota status"),
    );
    let endpoint = QueryEndpoints::active(
        run_id(),
        lease,
        move || Some(observation.clone()),
        ignore_core_failure,
    );
    let server = DiagnosticServer::start(
        ServerConfig::new(run_id(), std::process::id(), process_identity())
            .with_bind("127.0.0.1", 0),
        endpoint.route_definitions().expect("valid active routes"),
    )
    .expect("start active query server");

    let response = request(&server, "GET", "/api/v1/status", &[]);
    assert_eq!(response.status, 200);
    assert_json_headers(&response);
    let body = response.json();
    assert_eq!(body["source"], "active");
    assert_eq!(body["lifecycle"]["state"], "active");
    assert_eq!(body["writer"]["status"], "available");
    assert_eq!(body["writer"]["value"]["max_uncommitted_events"], "32768");
    assert_eq!(body["writer"]["value"]["queued_events"], "0");
    assert_eq!(body["quota"]["status"], "available");
    assert_eq!(body["quota"]["value"]["max_run_bytes"], Value::Null);
    server.shutdown().expect("clean active server shutdown");
}

fn recording_reporter(
    failures: &Arc<Mutex<Vec<QueryCoreFailureSignal>>>,
) -> impl Fn(QueryCoreFailureSignal) + Send + Sync + 'static {
    let failures = Arc::clone(failures);
    move |failure| {
        failures
            .lock()
            .expect("failure recorder lock")
            .push(failure)
    }
}

fn only_failure(failures: &Arc<Mutex<Vec<QueryCoreFailureSignal>>>) -> QueryCoreFailureSignal {
    let failures = failures.lock().expect("failure recorder lock");
    assert_eq!(failures.len(), 1);
    failures[0]
}

#[test]
fn active_reader_and_projection_failures_report_typed_core_fatal_signals() {
    let identity_directory = TestRunDirectory::new("active-signal-identity");
    let identity_lease = Arc::new(
        ActiveArchiveLease::acquire(identity_directory.path()).expect("identity active lease"),
    );
    let _identity_store = create_store(identity_directory.path());
    let identity_failures = Arc::new(Mutex::new(Vec::new()));
    let identity_endpoint = QueryEndpoints::active_unobserved(
        other_run_id(),
        identity_lease,
        recording_reporter(&identity_failures),
    );
    let identity_server = DiagnosticServer::start(
        ServerConfig::new(other_run_id(), std::process::id(), process_identity())
            .with_bind("127.0.0.1", 0),
        identity_endpoint.route_definitions().unwrap(),
    )
    .unwrap();
    assert_closed_error(
        &request(&identity_server, "GET", "/api/v1/events?after=01", &[]),
        400,
        "invalid_cursor",
        OTHER_RUN_ID,
    );
    assert!(identity_failures.lock().unwrap().is_empty());
    assert_closed_error(
        &request(&identity_server, "GET", "/api/v1/status", &[]),
        409,
        "run_identity_mismatch",
        OTHER_RUN_ID,
    );
    let identity = only_failure(&identity_failures);
    assert_eq!(identity.run_id(), other_run_id());
    assert_eq!(identity.endpoint(), QueryEndpointKind::Status);
    assert_eq!(identity.class(), ReaderFailureClass::CoreFatal);
    assert_eq!(
        identity.code(),
        QueryFailureCode::Reader(ReaderErrorCode::StoreValidation)
    );
    assert_eq!(
        identity.store_code(),
        Some(StoreOpenErrorCode::RunIdentityMismatch)
    );
    identity_server.shutdown().unwrap();

    let dense_directory = TestRunDirectory::new("active-signal-dense");
    let dense_lease =
        Arc::new(ActiveArchiveLease::acquire(dense_directory.path()).expect("dense active lease"));
    let dense_store = create_store(dense_directory.path());
    let one = SortableU64Key::new(1);
    dense_store
        .connection()
        .execute(
            "UPDATE run_metadata SET committed_key = ?1, committed_sequence = '1', \
             read_model_key = ?1, read_model_sequence = '1' WHERE singleton = 1",
            params![one.as_bytes().as_slice()],
        )
        .expect("inject impossible dense prefix");
    let dense_failures = Arc::new(Mutex::new(Vec::new()));
    let dense_endpoint = QueryEndpoints::active_unobserved(
        run_id(),
        dense_lease,
        recording_reporter(&dense_failures),
    );
    let dense_server = DiagnosticServer::start(
        ServerConfig::new(run_id(), std::process::id(), process_identity())
            .with_bind("127.0.0.1", 0),
        dense_endpoint.route_definitions().unwrap(),
    )
    .unwrap();
    assert_closed_error(
        &request(&dense_server, "GET", "/api/v1/events", &[]),
        500,
        "query_failed",
        RUN_ID,
    );
    let dense = only_failure(&dense_failures);
    assert_eq!(dense.endpoint(), QueryEndpointKind::Events);
    assert_eq!(dense.class(), ReaderFailureClass::CoreFatal);
    assert_eq!(
        dense.code(),
        QueryFailureCode::Reader(ReaderErrorCode::StoreValidation)
    );
    assert_eq!(
        dense.store_code(),
        Some(StoreOpenErrorCode::DensePrefixViolation)
    );
    dense_server.shutdown().unwrap();

    let snapshot_directory = TestRunDirectory::new("active-signal-snapshot");
    let snapshot_lease = Arc::new(
        ActiveArchiveLease::acquire(snapshot_directory.path()).expect("snapshot active lease"),
    );
    let mut snapshot_writer = TransactionalWriter::new(create_store(snapshot_directory.path()), ())
        .expect("construct snapshot writer");
    let snapshot_hub = diagnostic_hub();
    snapshot_writer
        .commit_batch(&EventBatch::new(vec![counter_event(&snapshot_hub)]).unwrap())
        .expect("commit snapshot event");
    let snapshot_store = snapshot_writer.into_store();
    snapshot_store
        .connection()
        .execute("DELETE FROM materialized_snapshot", [])
        .expect("remove materialized snapshot");
    let snapshot_failures = Arc::new(Mutex::new(Vec::new()));
    let snapshot_endpoint = QueryEndpoints::active_unobserved(
        run_id(),
        Arc::clone(&snapshot_lease),
        recording_reporter(&snapshot_failures),
    );
    let snapshot_server = DiagnosticServer::start(
        ServerConfig::new(run_id(), std::process::id(), process_identity())
            .with_bind("127.0.0.1", 0),
        snapshot_endpoint.route_definitions().unwrap(),
    )
    .unwrap();
    drop(snapshot_endpoint);
    assert_closed_error(
        &request(&snapshot_server, "GET", "/api/v1/snapshot", &[]),
        500,
        "query_failed",
        RUN_ID,
    );
    let snapshot = only_failure(&snapshot_failures);
    assert_eq!(snapshot.endpoint(), QueryEndpointKind::Snapshot);
    assert_eq!(snapshot.class(), ReaderFailureClass::CoreFatal);
    assert_eq!(
        snapshot.code(),
        QueryFailureCode::Snapshot(SnapshotQueryErrorCode::MaterializedMissing)
    );
    assert_eq!(snapshot.store_code(), None);
    snapshot_server.shutdown().unwrap();

    drop(snapshot_store);
    drop(snapshot_lease);
    let mut archive_reader =
        DiagnosticReader::open_archive(snapshot_directory.path(), run_id()).unwrap();
    let archive_capture = archive_reader.capture().unwrap();
    let archive_error = encode_snapshot_response(run_id(), &archive_capture).unwrap_err();
    assert_eq!(archive_error.profile(), ReaderProfile::Archive);
    assert_eq!(archive_error.class(), ReaderFailureClass::ArchiveOperation);
    assert_eq!(
        archive_error.code(),
        QueryFailureCode::Snapshot(SnapshotQueryErrorCode::MaterializedMissing)
    );
    assert!(
        archive_error
            .core_failure_signal(run_id(), QueryEndpointKind::Snapshot)
            .is_none()
    );
    drop(archive_capture);
    drop(archive_reader);
    let archive_server = start_archive_server(run_id(), snapshot_directory.path());
    assert_closed_error(
        &request(
            &archive_server,
            "GET",
            &format!("{BASE_PATH}/api/v1/snapshot"),
            &[],
        ),
        500,
        "query_failed",
        RUN_ID,
    );
    assert_eq!(snapshot_failures.lock().unwrap().len(), 1);
    archive_server.shutdown().unwrap();
}

#[test]
fn h01_owned_projection_errors_have_profile_aware_typed_signals() {
    let active = QueryEndpointError::Status {
        profile: ReaderProfile::Active,
        source: StatusProjectionError::UnknownProductionOutcome("future".to_owned()),
    };
    let signal = active
        .core_failure_signal(run_id(), QueryEndpointKind::Status)
        .expect("active status projection must be core-fatal");
    assert_eq!(signal.class(), ReaderFailureClass::CoreFatal);
    assert_eq!(
        signal.code(),
        QueryFailureCode::StatusUnknownProductionOutcome
    );
    assert_eq!(
        signal.code().as_str(),
        "diagnostic_status.unknown_production_outcome"
    );

    let archive = QueryEndpointError::Status {
        profile: ReaderProfile::Archive,
        source: StatusProjectionError::UnknownProductionOutcome("future".to_owned()),
    };
    assert_eq!(archive.class(), ReaderFailureClass::ArchiveOperation);
    assert!(
        archive
            .core_failure_signal(run_id(), QueryEndpointKind::Status)
            .is_none()
    );
}

#[test]
fn response_encoding_never_chases_beyond_one_captured_head() {
    let directory = TestRunDirectory::new("captured-head");
    let lease = ActiveArchiveLease::acquire(directory.path()).expect("active lease");
    let mut writer =
        TransactionalWriter::new(create_store(directory.path()), ()).expect("construct writer");
    let hub = diagnostic_hub();
    let first = usage_event(&hub);
    let second = counter_event(&hub);
    writer
        .commit_batch(&EventBatch::new(vec![first.clone(), second.clone()]).unwrap())
        .expect("commit W=2");
    let expected_snapshot = writer.snapshot().canonical_json().unwrap();
    let mut reader = DiagnosticReader::open_active(run_id(), lease.guard()).unwrap();
    let captured = reader.capture().expect("capture W=2");
    let third = counter_event(&hub);
    writer
        .commit_batch(&EventBatch::new(vec![third.clone()]).unwrap())
        .expect("advance live head to W=3");

    let events = encode_events_response(
        run_id(),
        &captured,
        FiniteEventQuery::after(SchemaU64::new(0)),
    )
    .expect("encode captured events");
    let events_json: Value = serde_json::from_slice(&events).unwrap();
    assert_eq!(events_json["captured_watermark"], "2");
    assert_eq!(events_json["events"].as_array().unwrap().len(), 2);
    assert!(contains_bytes(&events, first.canonical_bytes()));
    assert!(contains_bytes(&events, second.canonical_bytes()));
    assert!(!contains_bytes(&events, third.canonical_bytes()));

    let snapshot = encode_snapshot_response(run_id(), &captured).expect("encode captured snapshot");
    let snapshot_json: Value = serde_json::from_slice(&snapshot).unwrap();
    assert_eq!(snapshot_json["watermark_sequence"], "2");
    assert_eq!(snapshot_json["state"]["through_sequence"], "2");
    assert!(contains_bytes(&snapshot, &expected_snapshot));
}

#[test]
fn identity_and_schema_drift_are_closed_versioned_errors() {
    let identity_archive = build_archive("wrong-run", None, None, false);
    let identity_server = start_archive_server(other_run_id(), identity_archive.directory.path());
    for endpoint in ["status", "snapshot", "events"] {
        assert_closed_error(
            &request(
                &identity_server,
                "GET",
                &format!("{BASE_PATH}/api/v1/{endpoint}"),
                &[],
            ),
            409,
            "run_identity_mismatch",
            OTHER_RUN_ID,
        );
    }
    identity_server
        .shutdown()
        .expect("clean identity server shutdown");

    let schema_archive = build_archive("newer-schema", None, None, false);
    let connection = Connection::open(schema_archive.directory.database_path()).unwrap();
    connection.pragma_update(None, "user_version", 2).unwrap();
    drop(connection);
    let schema_server = start_archive_server(run_id(), schema_archive.directory.path());
    for endpoint in ["status", "snapshot", "events"] {
        assert_closed_error(
            &request(
                &schema_server,
                "GET",
                &format!("{BASE_PATH}/api/v1/{endpoint}"),
                &[],
            ),
            409,
            "incompatible_schema",
            RUN_ID,
        );
    }
    schema_server
        .shutdown()
        .expect("clean schema server shutdown");
}

#[test]
fn a_disconnected_request_does_not_poison_query_service() {
    let archive = build_archive("disconnect", None, None, false);
    let server = start_archive_server(run_id(), archive.directory.path());
    let mut partial = TcpStream::connect(server.connect_addr()).expect("connect partial request");
    partial
        .write_all(
            b"GET /troupe/api/v1/events?after=0 HTTP/1.1\r\nHost: diagnostics.test\r\nConnection: close\r\n\r\n",
        )
        .expect("write disconnected request");
    partial
        .shutdown(Shutdown::Both)
        .expect("disconnect request socket");
    drop(partial);

    let response = request(&server, "GET", &format!("{BASE_PATH}/api/v1/status"), &[]);
    assert_eq!(response.status, 200);
    assert!(server.try_core_failure().is_none());
    server.shutdown().expect("clean server shutdown");
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("runtime crate is under rust/crates")
        .to_path_buf()
}

#[test]
fn stdlib_verifier_accepts_all_http_goldens() {
    let script = r#"
import importlib.util
import json
import pathlib
import re
import sys
import uuid

root = pathlib.Path.cwd()
spec = importlib.util.spec_from_file_location(
    "troupe_fixture_verifier", root / "scripts/verify_diagnostic_fixtures.py"
)
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)
fixture_dir = root / "tests/fixtures/diagnostics/http"
load = lambda name: json.loads((fixture_dir / name).read_text(encoding="utf-8"))
status = load("status-v1.json")
snapshot = load("snapshot-v1.json")
events = load("events-v1.json")
error = load("error-v1.json")

for name, value in (("status", status), ("snapshot", snapshot), ("events", events), ("error", error)):
    assert value["api_schema_version"] == 1, name
    assert str(uuid.UUID(value["run_id"])) == value["run_id"], name

assert set(snapshot) == {
    "api_schema_version", "run_id", "watermark_sequence",
    "earliest_available_sequence", "state"
}
assert snapshot["watermark_sequence"] == snapshot["state"]["through_sequence"]
assert (snapshot["watermark_sequence"] == "0") == (snapshot["earliest_available_sequence"] is None)
assert set(events) == {
    "api_schema_version", "run_id", "captured_watermark", "events", "next_after"
}
assert events["next_after"] is None
for index, event in enumerate(events["events"]):
    module.validate_event(event, f"events.events[{index}]")
    assert event["run_id"] == events["run_id"]
    assert int(event["sequence"]) <= int(events["captured_watermark"])
assert status["lifecycle"]["state"] == "failed"
assert set(error) == {"api_schema_version", "run_id", "error"}
assert set(error["error"]) == {"code", "message", "details"}
assert error["error"]["details"] is None

def reject_untyped_integers(value, path="response"):
    if isinstance(value, bool) or value is None or isinstance(value, str):
        return
    if isinstance(value, int):
        assert path.endswith("schema_version"), path
        return
    if isinstance(value, list):
        for index, item in enumerate(value):
            reject_untyped_integers(item, f"{path}[{index}]")
        return
    assert isinstance(value, dict), path
    for key, item in value.items():
        reject_untyped_integers(item, f"{path}.{key}")

for value in (status, snapshot, events, error):
    reject_untyped_integers(value)
"#;
    let output = Command::new("python3")
        .args(["-c", script])
        .current_dir(repository_root())
        .output()
        .expect("run stdlib Python verifier");
    assert!(
        output.status.success(),
        "Python verifier failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn existing_w01_decoder_accepts_all_http_goldens() {
    let script = r#"
import { readFileSync } from "node:fs";
import {
  decodeEventsResponse,
  decodeHttpErrorResponse,
  decodeSnapshotResponse,
  decodeVersionedApiObject,
} from "./frontend/diagnostics/src/protocol/http.ts";

const load = (name) => JSON.parse(readFileSync(`tests/fixtures/diagnostics/http/${name}`, "utf8"));
const status = decodeVersionedApiObject(load("status-v1.json"), "status");
const snapshot = decodeSnapshotResponse(load("snapshot-v1.json"), "snapshot");
const events = decodeEventsResponse(load("events-v1.json"), "events");
const error = decodeHttpErrorResponse(load("error-v1.json"), "error");
if (status.lifecycle.state !== "failed") throw new Error("status lifecycle drift");
if (snapshot.watermark_sequence !== snapshot.state.through_sequence) {
  throw new Error("snapshot watermark drift");
}
if (events.events[0].provider_total_tokens !== "1234567890123456789012345678901234567890") {
  throw new Error("token integer lost precision");
}
if (error.error.code !== "invalid_cursor") throw new Error("error code drift");
"#;
    let output = Command::new("node")
        .args([
            "--no-warnings",
            "--experimental-strip-types",
            "--input-type=module",
            "-e",
            script,
        ])
        .current_dir(repository_root())
        .output()
        .expect("run existing W01 TypeScript decoder");
    assert!(
        output.status.success(),
        "W01 decoder failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn server_query_source_only_composes_frozen_query_surfaces() {
    let source = include_str!("../src/server/query.rs");
    assert_eq!(source.matches("project_status(").count(), 1);
    assert_eq!(source.matches("project_snapshot(").count(), 1);
    assert_eq!(source.matches("query_events(").count(), 1);
    for forbidden in [
        "SnapshotProjector",
        "SELECT ",
        "FROM events",
        "DiagnosticEvent::",
        "query_views",
    ] {
        assert!(
            !source.contains(forbidden),
            "unexpected server query path: {forbidden}"
        );
    }
    assert_eq!(source.matches("RouteDefinition::read_only(").count(), 3);
    assert!(!source.contains("text/event-stream"));
}
