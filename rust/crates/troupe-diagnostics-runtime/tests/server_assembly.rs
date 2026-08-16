use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};
use tokio::io::{AsyncWrite, AsyncWriteExt as _};
use troupe_diagnostics_core::{id::CanonicalUuid, scalar::SchemaU64};
use troupe_diagnostics_runtime::{
    archive::lease::ActiveArchiveLease,
    query::{
        reader::CapturedEventSource,
        views::{CursorKey, ViewQueryEngine},
    },
    registry::{model::WebBaseUrl, process_identity::ProcessIdentity},
    server::{
        assembly::{ActiveRouteAssembly, ArchiveRouteAssembly},
        dump::{
            CapturedPrefixDumpProducer, DumpEndpoints, DumpProducerError, DumpProducerFuture,
            DumpProducerMetadata,
        },
        query::QueryEndpoints,
        routes::{RouteDefinition, RouteMethods},
        runtime::{DiagnosticServer, ServerConfig},
        sse::{
            frame::PRODUCTION_FINISHED_REASON,
            replay::{ActiveReplaySource, ReplayDriverConfig, SseEndpoint},
            subscriber::{CommitSignal, SubscriberLimits},
        },
        views::ViewEndpoints,
    },
    store::{
        connection::{DiagnosticStore, InitialStoreMetadata},
        view_records::{CompiledViewSet, persist_view_set},
    },
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
const OTHER_RUN_ID: &str = "87654321-4321-4321-8321-cba987654321";
const BASE_PATH: &str = "/proxy/run";
const ROUTE_MATRIX: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../tests/fixtures/diagnostics/http/route-matrix.json"
));
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; font-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; object-src 'none'; worker-src 'self'; manifest-src 'self'";

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestRunDirectory(PathBuf);

impl TestRunDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "troupe-h04-server-assembly-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
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

struct TinyDump {
    metadata: DumpProducerMetadata,
}

impl TinyDump {
    fn new() -> Self {
        Self {
            metadata: DumpProducerMetadata::new(
                1,
                "0.1.0",
                "trace may contain diagnostic metadata",
            )
            .unwrap(),
        }
    }
}

impl CapturedPrefixDumpProducer for TinyDump {
    fn metadata(&self) -> &DumpProducerMetadata {
        &self.metadata
    }

    fn dump<'operation>(
        &'operation self,
        _source: &'operation CapturedEventSource<'_>,
        writer: &'operation mut (dyn AsyncWrite + Unpin),
        _through: Option<SchemaU64>,
    ) -> DumpProducerFuture<'operation> {
        Box::pin(async move {
            writer
                .write_all(b"trace")
                .await
                .map_err(|error| DumpProducerError::new("test_write", error.to_string()))?;
            writer
                .flush()
                .await
                .map_err(|error| DumpProducerError::new("test_flush", error.to_string()))
        })
    }
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
        serde_json::from_slice(&self.body).unwrap()
    }
}

struct ProfileServers {
    active: DiagnosticServer,
    archive: DiagnosticServer,
    _active_directory: TestRunDirectory,
    _archive_directory: TestRunDirectory,
}

impl ProfileServers {
    fn new() -> Self {
        let (active_directory, lease) = active_run("active");
        let archive_directory = archive_run("archive");
        let active = DiagnosticServer::start(
            server_config(),
            active_assembly(Arc::clone(&lease))
                .route_definitions()
                .unwrap(),
        )
        .unwrap();
        let archive = DiagnosticServer::start(
            server_config(),
            archive_assembly(archive_directory.path())
                .route_definitions()
                .unwrap(),
        )
        .unwrap();
        Self {
            active,
            archive,
            _active_directory: active_directory,
            _archive_directory: archive_directory,
        }
    }

    fn server(&self, profile: &str) -> &DiagnosticServer {
        match profile {
            "active" => &self.active,
            "archive" => &self.archive,
            _ => panic!("unknown profile {profile}"),
        }
    }
}

fn run_id() -> CanonicalUuid {
    CanonicalUuid::parse(RUN_ID).unwrap()
}

fn other_run_id() -> CanonicalUuid {
    CanonicalUuid::parse(OTHER_RUN_ID).unwrap()
}

fn engine() -> ViewQueryEngine {
    ViewQueryEngine::new(CursorKey::new([0x44; 32]))
}

fn initialize(directory: &Path) {
    let store = DiagnosticStore::create(
        directory,
        &InitialStoreMetadata::new(run_id(), "2026-08-16T00:00:00Z", "configuration-sha256:h04"),
    )
    .unwrap();
    drop(store);
    let empty = CompiledViewSet::from_json_records(std::iter::empty::<&[u8]>()).unwrap();
    persist_view_set(directory, run_id(), &empty).unwrap();
}

fn active_run(label: &str) -> (TestRunDirectory, Arc<ActiveArchiveLease>) {
    let directory = TestRunDirectory::new(label);
    let lease = Arc::new(ActiveArchiveLease::acquire(directory.path()).unwrap());
    initialize(directory.path());
    (directory, lease)
}

fn archive_run(label: &str) -> TestRunDirectory {
    let (directory, lease) = active_run(label);
    drop(lease);
    directory
}

fn active_assembly(lease: Arc<ActiveArchiveLease>) -> ActiveRouteAssembly {
    let queries = QueryEndpoints::active_unobserved(run_id(), Arc::clone(&lease), |_| {});
    let signal = CommitSignal::new(run_id(), SchemaU64::new(0));
    signal
        .close(PRODUCTION_FINISHED_REASON, SchemaU64::new(0))
        .unwrap();
    let sse = SseEndpoint::active(
        ActiveReplaySource::new(run_id(), Arc::clone(&lease)),
        signal,
        SubscriberLimits::new(8, 64 * 1024).unwrap(),
        ReplayDriverConfig::new(Duration::from_secs(1)).unwrap(),
        |_| {},
    )
    .unwrap();
    let views = ViewEndpoints::active(run_id(), Arc::clone(&lease), engine(), |_| {});
    let dump = DumpEndpoints::active(run_id(), lease, TinyDump::new());
    ActiveRouteAssembly::new(queries, sse, views, dump).unwrap()
}

fn archive_assembly(directory: &Path) -> ArchiveRouteAssembly {
    ArchiveRouteAssembly::new(
        QueryEndpoints::archive(run_id(), directory),
        ViewEndpoints::archive(run_id(), directory, engine()),
        DumpEndpoints::archive(run_id(), directory, TinyDump::new()),
    )
    .unwrap()
}

fn server_config() -> ServerConfig {
    ServerConfig::new(
        run_id(),
        std::process::id(),
        ProcessIdentity::new("test", "h04:4242").unwrap(),
    )
    .with_bind("127.0.0.1", 0)
    .with_advertise_url(Some(
        WebBaseUrl::parse("https://diagnostics.example/proxy/run/").unwrap(),
    ))
}

fn request(
    server: &DiagnosticServer,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> HttpResponse {
    let mut stream = TcpStream::connect(server.connect_addr()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: diagnostics.test\r\nConnection: close\r\n"
    )
    .unwrap();
    for (name, value) in headers {
        write!(stream, "{name}: {value}\r\n").unwrap();
    }
    write!(stream, "\r\n").unwrap();
    stream.flush().unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    parse_response(&bytes)
}

fn parse_response(bytes: &[u8]) -> HttpResponse {
    let header_end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let head = std::str::from_utf8(&bytes[..header_end]).unwrap();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split_ascii_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let mut headers = BTreeMap::<String, Vec<String>>::new();
    for line in lines {
        let (name, value) = line.split_once(':').unwrap();
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
            .unwrap();
        let size = usize::from_str_radix(
            std::str::from_utf8(&remaining[..line_end])
                .unwrap()
                .split(';')
                .next()
                .unwrap(),
            16,
        )
        .unwrap();
        remaining = &remaining[line_end + 2..];
        if size == 0 {
            break;
        }
        decoded.extend_from_slice(&remaining[..size]);
        remaining = &remaining[size + 2..];
    }
    decoded
}

fn route_inventory(profile: &str, events_mode: &str, routes: &[RouteDefinition]) -> Value {
    let mut inventory = vec![json!({
        "path": "/api/v1/identity",
        "methods": ["GET", "HEAD"]
    })];
    inventory.extend(routes.iter().map(|route| {
        let methods = match route.methods() {
            RouteMethods::GetOnly => json!(["GET"]),
            RouteMethods::GetAndHead => json!(["GET", "HEAD"]),
        };
        json!({"path": route.relative_path(), "methods": methods})
    }));
    inventory.sort_by(|left, right| {
        left["path"]
            .as_str()
            .unwrap()
            .cmp(right["path"].as_str().unwrap())
    });
    json!({
        "profile": profile,
        "events_mode": events_mode,
        "routes": inventory,
    })
}

fn response_contract(name: &str, profile: &str, response: &HttpResponse) -> Value {
    json!({
        "name": name,
        "profile": profile,
        "status": response.status,
        "headers": {
            "cache-control": response.header("cache-control"),
            "content-type": response.header("content-type"),
            "content-encoding": response.header("content-encoding"),
            "vary": response.header("vary"),
            "x-accel-buffering": response.header("x-accel-buffering"),
            "content-security-policy": response.header("content-security-policy"),
        },
        "cors": response.headers.keys().any(|name| name.starts_with("access-control-")),
    })
}

fn matrix() -> Value {
    serde_json::from_slice(ROUTE_MATRIX).unwrap()
}

#[test]
fn route_inventory_matches_the_closed_active_archive_matrix() {
    let (active_directory, lease) = active_run("inventory-active");
    let archive_directory = archive_run("inventory-archive");
    let active = active_assembly(lease);
    let archive = archive_assembly(archive_directory.path());
    let actual = json!({
        "schema_version": 1,
        "profiles": [
            route_inventory(
                "active",
                "finite_json_and_sse",
                &active.route_definitions().unwrap(),
            ),
            route_inventory(
                "archive",
                "finite_json_only",
                &archive.route_definitions().unwrap(),
            ),
        ],
        "response_contracts": matrix()["response_contracts"].clone(),
    });
    assert_eq!(actual, matrix());
    assert_eq!(active.run_id(), run_id());
    assert_eq!(archive.run_id(), run_id());

    let mismatch = ArchiveRouteAssembly::new(
        QueryEndpoints::archive(other_run_id(), "/unused"),
        ViewEndpoints::archive(run_id(), "/unused", engine()),
        DumpEndpoints::archive(run_id(), "/unused", TinyDump::new()),
    )
    .err()
    .unwrap();
    assert_eq!(
        mismatch.to_string(),
        "assembled diagnostic endpoints belong to different Runs"
    );
    drop(active_directory);
}

#[test]
fn response_headers_and_profile_dispatch_match_the_closed_matrix() {
    let servers = ProfileServers::new();
    let js_path = matrix()["profiles"][0]["routes"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|route| route["path"].as_str().filter(|path| path.ends_with(".js")))
        .unwrap()
        .to_owned();
    let cases = [
        (
            "active_finite_events",
            "active",
            "GET",
            "/api/v1/events",
            vec![("Accept", "application/json")],
        ),
        (
            "active_sse_events",
            "active",
            "GET",
            "/api/v1/events?after=0",
            vec![("Accept", "text/event-stream")],
        ),
        (
            "active_head_sse_rejected",
            "active",
            "HEAD",
            "/api/v1/events?after=0",
            vec![("Accept", "text/event-stream")],
        ),
        (
            "archive_finite_events",
            "archive",
            "GET",
            "/api/v1/events",
            vec![("Accept", "application/json")],
        ),
        (
            "archive_sse_rejected",
            "archive",
            "GET",
            "/api/v1/events?after=0",
            vec![("Accept", "text/event-stream")],
        ),
        ("active_html", "active", "GET", "/", vec![]),
        (
            "archive_asset",
            "archive",
            "GET",
            js_path.as_str(),
            vec![("Accept-Encoding", "br")],
        ),
        ("active_dump", "active", "GET", "/api/v1/dump", vec![]),
        (
            "active_views",
            "active",
            "GET",
            "/api/v1/views",
            vec![("Accept", "application/json")],
        ),
        (
            "active_unknown_api",
            "active",
            "GET",
            "/api/v1/unknown",
            vec![("Accept", "text/html")],
        ),
    ];
    let mut contracts = Vec::new();
    for (name, profile, method, relative, headers) in cases {
        let response = request(
            servers.server(profile),
            method,
            &format!("{BASE_PATH}{relative}"),
            &headers,
        );
        match name {
            "active_finite_events" | "archive_finite_events" => {
                assert_eq!(response.json()["api_schema_version"], 1);
            }
            "active_sse_events" => {
                let text = std::str::from_utf8(&response.body).unwrap();
                assert!(text.contains("event: stream_ready"));
                assert!(text.contains("event: stream_closed"));
            }
            "active_head_sse_rejected" => assert!(response.body.is_empty()),
            "archive_sse_rejected" => {
                assert_eq!(response.json()["error"]["code"], "unsupported_format");
            }
            "active_html" => {
                let html = std::str::from_utf8(&response.body).unwrap();
                assert!(html.contains("src=\"./assets/"));
                let archive = request(&servers.archive, "GET", &format!("{BASE_PATH}/"), &[]);
                assert_eq!(response.body, archive.body);
            }
            "archive_asset" => {
                let active = request(
                    &servers.active,
                    "GET",
                    &format!("{BASE_PATH}{js_path}"),
                    &[("Accept-Encoding", "br")],
                );
                assert_eq!(response.body, active.body);
                assert_eq!(response.header("etag"), active.header("etag"));
            }
            "active_dump" => assert_eq!(response.body, b"trace"),
            "active_views" => assert_eq!(response.json()["views"], json!([])),
            "active_unknown_api" => {
                assert_eq!(response.json()["error"]["code"], "not_found");
                assert!(
                    !response
                        .body
                        .windows(b"<!DOCTYPE".len())
                        .any(|part| part == b"<!DOCTYPE")
                );
            }
            _ => unreachable!(),
        }
        contracts.push(response_contract(name, profile, &response));
    }
    assert_eq!(json!(contracts), matrix()["response_contracts"]);

    assert_eq!(request(&servers.active, "GET", "/", &[]).status, 404);
    assert_eq!(
        request(
            &servers.active,
            "GET",
            &format!("{BASE_PATH}/assets/not-registered.js"),
            &[],
        )
        .status,
        404
    );
}

#[test]
fn method_matrix_is_read_only_and_assembly_contains_no_handler_copy() {
    let servers = ProfileServers::new();
    for profile in matrix()["profiles"].as_array().unwrap() {
        let profile_name = profile["profile"].as_str().unwrap();
        let server = servers.server(profile_name);
        for route in profile["routes"].as_array().unwrap() {
            let path = route["path"].as_str().unwrap();
            let target = format!("{BASE_PATH}{path}");
            let methods = route["methods"].as_array().unwrap();
            let post = request(server, "POST", &target, &[]);
            assert_eq!(post.status, 405, "{profile_name} {path}");
            let allow = if methods.len() == 2 {
                "GET, HEAD"
            } else {
                "GET"
            };
            assert_eq!(post.header("allow"), Some(allow));

            let head = request(server, "HEAD", &target, &[("Accept", "application/json")]);
            assert_eq!(
                head.status != 405,
                methods.len() == 2,
                "{profile_name} {path}"
            );
            assert!(head.body.is_empty());
        }
    }

    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/server/assembly.rs"
    ));
    for forbidden in [
        "DiagnosticReader",
        "CapturedEventSource",
        "ViewQueryEngine",
        "serde_json",
        "PERFETTO_TRACE_MIME",
    ] {
        assert!(
            !source.contains(forbidden),
            "handler logic copied: {forbidden}"
        );
    }
    for delegated in [
        "handle_status",
        "handle_snapshot",
        "handle_finite_events",
        "handle_follow",
        "views.route_definitions",
        "dump.route_definitions",
        "assets::route_definitions",
    ] {
        assert!(
            source.contains(delegated),
            "missing owner delegation: {delegated}"
        );
    }
    assert!(!source.contains("fallback"));
    assert_eq!(
        CSP,
        matrix()["response_contracts"][5]["headers"]["content-security-policy"]
    );
}
