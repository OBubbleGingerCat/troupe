use std::{
    collections::BTreeMap,
    convert::Infallible,
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use rusqlite::{Connection, params};
use serde_json::Value;
use troupe_diagnostics_core::{
    detail::{EmptyDetail, InstantDetail},
    event::{DiagnosticEvent, DiagnosticEventHeader, DiagnosticScope, InstantOccurred},
    hub::{
        AcceptedDiagnosticEvent, AdmissionReservation, AdmissionReserver, AdmissionSize,
        DeliveryFailure, EventIdentity, LiveEventNotifier, MandatoryDurableReserver,
        ProductionDiagnosticHub,
    },
    id::CanonicalUuid,
    time::ElapsedNs,
    view_protocol::{IncompatibilityReason, ViewResponse},
};
use troupe_diagnostics_runtime::{
    archive::lease::ActiveArchiveLease,
    query::views::{
        CursorKey, ViewQueryEngine, ViewQueryErrorClass, ViewQueryErrorCode, ViewQueryRequest,
    },
    registry::{model::WebBaseUrl, process_identity::ProcessIdentity},
    server::{
        query::QueryEndpoints,
        runtime::{DiagnosticServer, ServerConfig},
        views::{
            ViewCoreFailureSignal, ViewEndpointFailureCode, ViewEndpoints, ViewLocalErrorCode,
            ViewRequestControl,
        },
    },
    store::{
        batch::EventBatch,
        connection::{DiagnosticStore, InitialStoreMetadata},
        schema::DIAGNOSTIC_DATABASE_FILENAME,
        view_records::{CompiledViewSet, persist_view_set},
        writer::TransactionalWriter,
    },
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
const BASE_PATH: &str = "/troupe";
const CURSOR_KEY: [u8; 32] = [0x6d; 32];

const TIMELINE_RECORD: &[u8] = br#"{"renderer":"timeline","view_schema_version":1,"id":"timeline_view","title":"Timeline","time_range":"run","scope":"run","query":{"source":{"source":"instant","selector":{"selector":"built_in","kind":"cue.admitted"}},"filters":[],"group_by":null}}"#;
const METRIC_RECORD: &[u8] = br#"{"renderer":"metric","view_schema_version":1,"id":"metric_view","title":"Metric","time_range":"run","scope":"run","query":{"source":{"source":"instant_count","selector":{"selector":"built_in","kind":"cue.admitted"}},"filters":[],"group_by":null,"reducer":"count"}}"#;
const SELECTION_METRIC_RECORD: &[u8] = br#"{"renderer":"metric","view_schema_version":1,"id":"selection_metric_view","title":"Selection metric","time_range":"run","scope":"selection","query":{"source":{"source":"instant_count","selector":{"selector":"built_in","kind":"cue.admitted"}},"filters":[],"group_by":null,"reducer":"count"}}"#;
const TABLE_RECORD: &[u8] = br#"{"renderer":"table","view_schema_version":1,"id":"table_view","title":"Table","time_range":"run","scope":"run","query":{"source":{"source":"event","kind":"instant_occurred"},"filters":[],"columns":[{"column":"sequence"},{"column":"elapsed_ns"}],"page_size":1}}"#;
const TIME_SERIES_RECORD: &[u8] = br#"{"renderer":"time_series","view_schema_version":1,"id":"timeseries_view","title":"Time series","time_range":"run","scope":"run","query":{"source":{"source":"instant_count","selector":{"selector":"built_in","kind":"cue.admitted"}},"filters":[],"group_by":null,"reducer":"count"}}"#;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestRunDirectory(PathBuf);

impl TestRunDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "troupe-h03-server-views-{label}-{}-{sequence}",
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

fn header(identity: EventIdentity, elapsed_ns: u64) -> DiagnosticEventHeader {
    DiagnosticEventHeader::new(
        identity.run_id(),
        identity.sequence(),
        ElapsedNs::new(elapsed_ns),
        DiagnosticScope::new(None, None, None, None, None, None, None),
        Vec::new(),
    )
    .expect("valid diagnostic event header")
}

fn cue_admitted(
    hub: &ProductionDiagnosticHub<AcceptAll>,
    elapsed_ns: u64,
) -> AcceptedDiagnosticEvent {
    hub.admit(
        |identity| {
            DiagnosticEvent::InstantOccurred(InstantOccurred::new(
                header(identity, elapsed_ns),
                InstantDetail::CueAdmitted(EmptyDetail::new()),
                None,
            ))
        },
        None,
    )
    .expect("admit cue event")
    .accepted()
    .clone()
}

fn build_run(label: &str) -> (TestRunDirectory, Arc<ActiveArchiveLease>) {
    build_run_with_elapsed(label, &[1, 3])
}

fn build_run_with_elapsed(
    label: &str,
    elapsed_values: &[u64],
) -> (TestRunDirectory, Arc<ActiveArchiveLease>) {
    let directory = TestRunDirectory::new(label);
    let lease = Arc::new(ActiveArchiveLease::acquire(directory.path()).expect("active lease"));
    let store = DiagnosticStore::create(
        directory.path(),
        &InitialStoreMetadata::new(run_id(), "2026-08-16T00:00:00Z", "configuration-sha256:h03"),
    )
    .expect("create diagnostic store");
    let mut writer = TransactionalWriter::new(store, ()).expect("construct writer");
    let hub = diagnostic_hub();
    let events = elapsed_values
        .iter()
        .map(|elapsed_ns| cue_admitted(&hub, *elapsed_ns))
        .collect();
    writer
        .commit_batch(&EventBatch::new(events).expect("nonempty event batch"))
        .expect("commit view fixture events");
    drop(writer);

    let compiled = CompiledViewSet::from_json_records([
        TIMELINE_RECORD,
        METRIC_RECORD,
        SELECTION_METRIC_RECORD,
        TABLE_RECORD,
        TIME_SERIES_RECORD,
    ])
    .expect("compile canonical test view records");
    persist_view_set(directory.path(), run_id(), &compiled).expect("persist test views");
    (directory, lease)
}

fn engine() -> ViewQueryEngine {
    ViewQueryEngine::new(CursorKey::new(CURSOR_KEY))
}

fn test_active_endpoint(lease: Arc<ActiveArchiveLease>) -> ViewEndpoints {
    ViewEndpoints::active(run_id(), lease, engine(), |_| {})
}

fn process_identity() -> ProcessIdentity {
    ProcessIdentity::new("test", "h03:4242").expect("valid process identity")
}

fn start_server(
    routes: Vec<troupe_diagnostics_runtime::server::routes::RouteDefinition>,
) -> DiagnosticServer {
    DiagnosticServer::start(
        ServerConfig::new(run_id(), std::process::id(), process_identity())
            .with_bind("127.0.0.1", 0)
            .with_advertise_url(Some(
                WebBaseUrl::parse("https://diagnostics.example/troupe/")
                    .expect("valid advertised URL"),
            )),
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

    fn view(&self) -> ViewResponse {
        serde_json::from_slice(&self.body).expect("C05 view response")
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
        "view-timeline-v1.json" => include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../tests/fixtures/diagnostics/http/view-timeline-v1.json"
        )),
        "view-metric-v1.json" => include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../tests/fixtures/diagnostics/http/view-metric-v1.json"
        )),
        "view-table-v1.json" => include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../tests/fixtures/diagnostics/http/view-table-v1.json"
        )),
        "view-timeseries-v1.json" => include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../tests/fixtures/diagnostics/http/view-timeseries-v1.json"
        )),
        "view-error-v1.json" => include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../tests/fixtures/diagnostics/http/view-error-v1.json"
        )),
        _ => panic!("unknown H03 fixture {name}"),
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

fn assert_golden(response: &HttpResponse, name: &str) {
    assert_eq!(response.status, 200);
    assert_json_headers(response);
    assert_eq!(
        response.body,
        fixture_body(name),
        "actual {}: {}",
        name,
        String::from_utf8_lossy(&response.body)
    );
}

fn assert_error(response: &HttpResponse, status: u16, code: &str) {
    assert_eq!(response.status, status);
    assert_json_headers(response);
    let value = response.json();
    assert_eq!(value["api_schema_version"], 1);
    assert_eq!(value["run_id"], RUN_ID);
    assert_eq!(value["error"]["code"], code);
    assert!(
        value["error"]["message"]
            .as_str()
            .is_some_and(|v| !v.is_empty())
    );
    assert!(value["error"]["details"].is_null());
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[test]
fn four_renderer_endpoints_match_goldens_and_frozen_metadata() {
    let (_directory, lease) = build_run("goldens");
    let endpoint = test_active_endpoint(lease);
    let server = start_server(endpoint.route_definitions().unwrap());
    let cases = [
        ("view_id=timeline_view&page_size=1", "view-timeline-v1.json"),
        ("view_id=metric_view", "view-metric-v1.json"),
        ("view_id=table_view", "view-table-v1.json"),
        ("view_id=timeseries_view", "view-timeseries-v1.json"),
    ];
    let mut golden_mismatches = Vec::new();
    for (query, golden) in cases {
        let response = request(
            &server,
            "GET",
            &format!("{BASE_PATH}/api/v1/views?{query}"),
            &[("Accept", "application/json")],
        );
        if response.body != fixture_body(golden) {
            golden_mismatches.push(format!(
                "{golden}={}",
                String::from_utf8_lossy(&response.body)
            ));
        } else {
            assert_golden(&response, golden);
        }
        assert_eq!(response.status, 200);
        assert_json_headers(&response);
        let view = response.view();
        view.validate().expect("valid frozen C05 response");
        assert_eq!(view.metadata().run_id(), run_id());
        assert_eq!(view.metadata().binding().captured_watermark().get(), 2);
        assert_eq!(view.metadata().binding().captured_elapsed_end_ns().get(), 4);
        view.metadata().capabilities().validate().unwrap();
    }
    assert!(
        golden_mismatches.is_empty(),
        "actual H03 goldens:\n{}",
        golden_mismatches.join("\n")
    );

    let timeseries = request(
        &server,
        "GET",
        &format!("{BASE_PATH}/api/v1/views?view_id=timeseries_view"),
        &[],
    )
    .view();
    let timeseries = timeseries.time_series().unwrap();
    assert_eq!(timeseries.bucket_width_ns().get(), 1);
    assert_eq!(timeseries.series().len(), 1);
    assert_eq!(timeseries.series()[0].points().len(), 4);
    assert!(timeseries.series()[0].points()[0].value().is_none());
    assert!(timeseries.series()[0].points()[1].value().is_some());
    server.shutdown().unwrap();
}

#[test]
fn timeseries_http_response_preserves_server_width_empty_and_partial_buckets() {
    let (_directory, lease) = build_run_with_elapsed("partial-buckets", &[1, 1024]);
    let endpoint = test_active_endpoint(lease);
    let server = start_server(endpoint.route_definitions().unwrap());
    let response = request(
        &server,
        "GET",
        &format!("{BASE_PATH}/api/v1/views?view_id=timeseries_view"),
        &[],
    );
    assert_eq!(response.status, 200);
    let response = response.view();
    let binding = response.metadata().binding();
    assert_eq!(binding.range_start_ns().get(), 0);
    assert_eq!(binding.range_end_ns().get(), 1025);
    let result = response.time_series().unwrap();
    assert_eq!(result.bucket_width_ns().get(), 2);
    let points = result.series()[0].points();
    assert_eq!(points.len(), 513);
    assert!(points[1].value().is_none(), "empty buckets stay explicit");
    assert_eq!(points.last().unwrap().bucket_start_ns().get(), 1024);
    assert_eq!(points.last().unwrap().bucket_end_ns().get(), 1026);
    assert!(points.last().unwrap().is_partial());
    server.shutdown().unwrap();
}

#[test]
fn selection_view_without_selected_scope_falls_back_to_the_whole_run_over_http() {
    let (_directory, lease) = build_run("selection-fallback");
    let endpoint = test_active_endpoint(lease);
    let server = start_server(endpoint.route_definitions().unwrap());
    let response = request(
        &server,
        "GET",
        &format!(
            "{BASE_PATH}/api/v1/views?view_id=selection_metric_view&captured_watermark=2&captured_elapsed_end_ns=4"
        ),
        &[],
    );
    assert_eq!(response.status, 200);
    let response = response.view();
    response.validate().unwrap();
    assert_eq!(
        response.metadata().binding().scope(),
        troupe_diagnostics_core::view_protocol::ScopeMode::Selection
    );
    assert!(response.metadata().binding().selected_scope().is_none());
    let result = response.metric().unwrap();
    assert_eq!(result.series().len(), 1);
    assert_eq!(result.series()[0].coverage().matched_count().get(), 2);
    assert_eq!(result.series()[0].coverage().contributing_count().get(), 2);
    server.shutdown().unwrap();
}

#[test]
fn pagination_binding_and_width_inputs_are_closed_and_tamper_resistant() {
    let (_directory, lease) = build_run("pagination");
    let endpoint = test_active_endpoint(lease);
    let server = start_server(endpoint.route_definitions().unwrap());

    let first = request(
        &server,
        "GET",
        &format!("{BASE_PATH}/api/v1/views?view_id=timeline_view&page_size=1"),
        &[],
    );
    let cursor = first
        .view()
        .metadata()
        .pagination()
        .unwrap()
        .next_cursor()
        .unwrap()
        .as_str()
        .to_owned();
    let second = request(
        &server,
        "GET",
        &format!(
            "{BASE_PATH}/api/v1/views?view_id=timeline_view&page_size=1&cursor={}",
            percent_encode(&cursor)
        ),
        &[],
    );
    assert_eq!(second.status, 200);
    assert_eq!(second.view().timeline().unwrap().rows().len(), 1);

    let cross_query = request(
        &server,
        "GET",
        &format!(
            "{BASE_PATH}/api/v1/views?view_id=table_view&cursor={}",
            percent_encode(&cursor)
        ),
        &[],
    );
    assert_error(&cross_query, 400, "invalid_view_cursor");

    let mut tampered = cursor.into_bytes();
    let final_byte = tampered.last_mut().unwrap();
    *final_byte = if *final_byte == b'0' { b'1' } else { b'0' };
    let tampered = String::from_utf8(tampered).unwrap();
    assert_error(
        &request(
            &server,
            "GET",
            &format!(
                "{BASE_PATH}/api/v1/views?view_id=timeline_view&page_size=1&cursor={}",
                percent_encode(&tampered)
            ),
            &[],
        ),
        400,
        "invalid_view_cursor",
    );
    assert_error(
        &request(
            &server,
            "GET",
            &format!("{BASE_PATH}/api/v1/views?view_id=timeline_view&page_size=501"),
            &[],
        ),
        400,
        "invalid_view_pagination",
    );
    assert_error(
        &request(
            &server,
            "GET",
            &format!("{BASE_PATH}/api/v1/views?view_id=metric_view&page_size=1"),
            &[],
        ),
        400,
        "invalid_view_pagination",
    );
    assert_error(
        &request(
            &server,
            "GET",
            &format!("{BASE_PATH}/api/v1/views?view_id=timeline_view&scene_id=scene-1"),
            &[],
        ),
        400,
        "invalid_view_binding",
    );
    assert_error(
        &request(
            &server,
            "GET",
            &format!(
                "{BASE_PATH}/api/v1/views?view_id=timeline_view&captured_watermark=1&captured_elapsed_end_ns=4"
            ),
            &[],
        ),
        409,
        "stale_view_binding",
    );
    assert_error(
        &request(
            &server,
            "GET",
            &format!("{BASE_PATH}/api/v1/views?view_id=missing"),
            &[],
        ),
        404,
        "view_not_found",
    );
    assert_error(
        &request(
            &server,
            "GET",
            &format!("{BASE_PATH}/api/v1/views?view_id=timeline_view"),
            &[("Accept", "text/plain")],
        ),
        406,
        "unsupported_format",
    );
    let width_override = request(
        &server,
        "GET",
        &format!("{BASE_PATH}/api/v1/views?view_id=timeseries_view&bucket_width_ns=1"),
        &[],
    );
    assert_error(&width_override, 400, "invalid_view_query");
    assert_eq!(
        width_override.body,
        fixture_body("view-error-v1.json"),
        "actual view-error-v1.json: {}",
        String::from_utf8_lossy(&width_override.body)
    );
    server.shutdown().unwrap();
}

fn replace_manifest_version(connection: &Connection, old: u8, new: u8) {
    let bytes = connection
        .query_row(
            "SELECT manifest_json FROM diagnostic_view_manifest WHERE singleton = 1",
            [],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let old = format!("\"view_schema_version\":{old}");
    let new = format!("\"view_schema_version\":{new}");
    let replaced = text.replacen(&old, &new, 1);
    assert_ne!(replaced, text);
    connection
        .execute(
            "UPDATE diagnostic_view_manifest SET manifest_json = ?1 WHERE singleton = 1",
            [replaced.as_bytes()],
        )
        .unwrap();
}

#[test]
fn archive_newer_and_corrupt_views_are_panel_local() {
    let (directory, lease) = build_run("archive-incompatible");
    let marker = directory.path().join("stored-content-executed");
    let connection = Connection::open(directory.database_path()).unwrap();
    replace_manifest_version(&connection, 1, 2);
    let future = serde_json::to_vec(&serde_json::json!({
        "renderer": "timeline",
        "view_schema_version": 2,
        "id": "timeline_view",
        "title": "<script>stored text only</script>",
        "time_range": "run",
        "scope": "run",
        "query": {
            "python": format!("write({:?})", marker.display().to_string()),
        },
    }))
    .unwrap();
    connection
        .execute(
            "UPDATE diagnostic_view_records SET view_schema_version = 2, record_json = ?1 \
             WHERE view_id = 'timeline_view'",
            params![future],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE diagnostic_view_records SET record_json = ?1 WHERE view_id = 'metric_view'",
            params![b"not-json".as_slice()],
        )
        .unwrap();
    drop(connection);
    drop(lease);

    let views = ViewEndpoints::archive(run_id(), directory.path(), engine());
    let queries = QueryEndpoints::archive(run_id(), directory.path());
    let mut routes = queries.route_definitions().unwrap();
    routes.extend(views.route_definitions().unwrap());
    let server = start_server(routes);

    let newer = request(
        &server,
        "GET",
        &format!("{BASE_PATH}/api/v1/views?view_id=timeline_view"),
        &[],
    );
    assert_eq!(newer.status, 200);
    let newer = newer.view();
    newer.validate().unwrap();
    assert_eq!(
        newer.metadata().incompatible().unwrap().reason(),
        IncompatibilityReason::NewerViewSchema
    );
    assert!(newer.timeline().unwrap().rows().is_empty());
    assert_error(
        &request(
            &server,
            "GET",
            &format!("{BASE_PATH}/api/v1/views?view_id=timeline_view&cursor=q1.invalid"),
            &[],
        ),
        400,
        "invalid_view_cursor",
    );

    let corrupt = request(
        &server,
        "GET",
        &format!("{BASE_PATH}/api/v1/views?view_id=metric_view"),
        &[],
    );
    assert_eq!(corrupt.status, 200);
    let corrupt = corrupt.view();
    corrupt.validate().unwrap();
    assert_eq!(
        corrupt.metadata().incompatible().unwrap().reason(),
        IncompatibilityReason::CorruptRecord
    );
    assert!(corrupt.metric().unwrap().series().is_empty());

    assert_eq!(
        request(
            &server,
            "GET",
            &format!("{BASE_PATH}/api/v1/views?view_id=table_view"),
            &[],
        )
        .status,
        200
    );
    assert_eq!(
        request(&server, "GET", &format!("{BASE_PATH}/api/v1/snapshot"), &[],).status,
        200
    );
    assert!(!marker.exists(), "stored strings must remain inert data");
    server.shutdown().unwrap();
}

fn recording_reporter() -> (
    Arc<Mutex<Vec<ViewCoreFailureSignal>>>,
    impl Fn(ViewCoreFailureSignal) + Send + Sync + 'static,
) {
    let failures = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&failures);
    (failures, move |failure| {
        recorded.lock().unwrap().push(failure);
    })
}

#[test]
fn active_system_failures_are_forwarded_but_local_errors_are_not() {
    let (_directory, lease) = build_run("active-fatal");
    let query_engine = engine();
    let execution = query_engine.execution_context();
    let (failures, reporter) = recording_reporter();
    let endpoint = ViewEndpoints::active(run_id(), lease, query_engine, reporter);
    let server = start_server(endpoint.route_definitions().unwrap());

    assert_error(
        &request(
            &server,
            "GET",
            &format!("{BASE_PATH}/api/v1/views?view_id=timeline_view&page_size=0"),
            &[],
        ),
        400,
        "invalid_view_pagination",
    );
    assert!(failures.lock().unwrap().is_empty());

    execution.mark_lost();
    assert_error(
        &request(
            &server,
            "GET",
            &format!("{BASE_PATH}/api/v1/views?view_id=timeline_view"),
            &[],
        ),
        500,
        "view_query_failed",
    );
    let failures = failures.lock().unwrap();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].run_id(), run_id());
    assert_eq!(failures[0].class(), ViewQueryErrorClass::CoreFatal);
    assert_eq!(
        failures[0].code(),
        ViewEndpointFailureCode::Query(ViewQueryErrorCode::ExecutionContextLost)
    );
    server.shutdown().unwrap();

    let (_directory, lease) = build_run("active-timeout");
    let (failures, reporter) = recording_reporter();
    let endpoint = ViewEndpoints::active(run_id(), lease, engine(), reporter)
        .with_request_timeout(Duration::ZERO);
    let server = start_server(endpoint.route_definitions().unwrap());
    assert_error(
        &request(
            &server,
            "GET",
            &format!("{BASE_PATH}/api/v1/views?view_id=timeline_view"),
            &[],
        ),
        408,
        "view_query_timeout",
    );
    assert!(failures.lock().unwrap().is_empty());
    server.shutdown().unwrap();
}

#[test]
fn active_q00_corruption_is_fatal_while_archive_and_request_controls_are_local() {
    let (active_directory, lease) = build_run("active-corrupt");
    let (failures, reporter) = recording_reporter();
    let active = ViewEndpoints::active(run_id(), lease, engine(), reporter);
    Connection::open(active_directory.database_path())
        .unwrap()
        .execute_batch("DROP TABLE events")
        .unwrap();
    let server = start_server(active.route_definitions().unwrap());
    assert_error(
        &request(
            &server,
            "GET",
            &format!("{BASE_PATH}/api/v1/views?view_id=timeline_view"),
            &[],
        ),
        500,
        "view_query_failed",
    );
    let failure = failures.lock().unwrap()[0];
    assert_eq!(failure.class(), ViewQueryErrorClass::CoreFatal);
    assert!(matches!(
        failure.code(),
        ViewEndpointFailureCode::ArchiveViews(_) | ViewEndpointFailureCode::Reader(_)
    ));
    server.shutdown().unwrap();

    let (archive_directory, archive_lease) = build_run("archive-local");
    drop(archive_lease);
    let archive_engine = engine();
    let execution = archive_engine.execution_context();
    execution.mark_lost();
    let archive = ViewEndpoints::archive(run_id(), archive_directory.path(), archive_engine);
    let error = archive
        .execute(
            "timeline_view",
            &ViewQueryRequest::new(),
            &ViewRequestControl::without_deadline(),
        )
        .unwrap_err();
    assert_eq!(error.class(), ViewQueryErrorClass::ArchiveOperation);
    assert_eq!(
        error.code(),
        ViewEndpointFailureCode::Query(ViewQueryErrorCode::ExecutionContextLost)
    );

    let archive = ViewEndpoints::archive(run_id(), archive_directory.path(), engine());
    let cancelled = ViewRequestControl::without_deadline();
    cancelled.cancel();
    let error = archive
        .execute("timeline_view", &ViewQueryRequest::new(), &cancelled)
        .unwrap_err();
    assert_eq!(error.class(), ViewQueryErrorClass::LocalQuery);
    assert_eq!(
        error.code(),
        ViewEndpointFailureCode::Local(ViewLocalErrorCode::RequestCancelled)
    );

    let error = archive
        .execute(
            "timeline_view",
            &ViewQueryRequest::new(),
            &ViewRequestControl::with_timeout(Duration::ZERO),
        )
        .unwrap_err();
    assert_eq!(error.class(), ViewQueryErrorClass::LocalQuery);
    assert_eq!(
        error.code(),
        ViewEndpointFailureCode::Local(ViewLocalErrorCode::RequestTimedOut)
    );
}
