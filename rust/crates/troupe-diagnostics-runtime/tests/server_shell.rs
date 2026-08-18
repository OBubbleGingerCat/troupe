use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use futures::stream;
use http_body_util::StreamBody;
use hyper::StatusCode;
use hyper::body::Frame;
use serde_json::{Value, json};
use troupe_diagnostics_core::id::CanonicalUuid;
use troupe_diagnostics_runtime::{
    registry::{model::WebBaseUrl, process_identity::ProcessIdentity},
    server::{
        error::{RequestError, ServerCoreFailureCode, ServerStartErrorCode},
        identity::OperationalLimits,
        routes::{RouteDefinition, RouteResponse},
        runtime::{DiagnosticServer, ServerConfig},
    },
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";

fn run_id() -> CanonicalUuid {
    CanonicalUuid::parse(RUN_ID).unwrap()
}

fn process_identity() -> ProcessIdentity {
    ProcessIdentity::new("test", "boot-a:4242").unwrap()
}

fn config() -> ServerConfig {
    ServerConfig::new(run_id(), 8123, process_identity())
}

fn test_route(hits: Arc<AtomicUsize>) -> RouteDefinition {
    RouteDefinition::read_only("/api/v1/test", move |request| {
        let hits = Arc::clone(&hits);
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            RouteResponse::json(
                StatusCode::OK,
                &json!({
                    "method": request.method().as_str(),
                    "path": request.uri().path(),
                    "query": request.uri().query(),
                    "server_pid": std::process::id(),
                    "server_thread": std::thread::current().name(),
                    "forwarded_visible": request.headers().contains_key("forwarded")
                        || request.headers().contains_key("x-forwarded-host")
                        || request.headers().contains_key("x-forwarded-proto")
                        || request.headers().contains_key("x-forwarded-prefix"),
                }),
            )
            .map_err(|error| RequestError::new("test_response_encode", error.to_string()))
        }
    })
    .unwrap()
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

fn request(
    server: &DiagnosticServer,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> HttpResponse {
    let mut stream = TcpStream::connect(server.connect_addr()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
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
        assert_eq!(&remaining[size..size + 2], b"\r\n");
        remaining = &remaining[size + 2..];
    }
    decoded
}

fn assert_common_headers(response: &HttpResponse) {
    assert_eq!(response.header("cache-control"), Some("no-store"));
    assert!(
        response
            .headers
            .keys()
            .all(|name| !name.starts_with("access-control-"))
    );
}

#[test]
fn default_listener_is_ephemeral_ready_and_reports_complete_identity() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = DiagnosticServer::start(config(), vec![test_route(Arc::clone(&hits))]).unwrap();

    assert_eq!(server.identity().bind_host(), "0.0.0.0");
    assert_ne!(server.identity().port(), 0);
    assert!(server.accepted_connections_at_ready() >= 1);
    assert_eq!(server.connect_addr().ip().to_string(), "127.0.0.1");

    let response = request(&server, "GET", "/api/v1/identity", &[]);
    assert_eq!(response.status, 200);
    assert_eq!(
        response.header("content-type"),
        Some("application/json; charset=utf-8")
    );
    assert_common_headers(&response);
    let identity = response.json();
    assert_eq!(identity["identity_schema_version"], 1);
    assert_eq!(identity["server_protocol_version"], 1);
    assert_eq!(identity["event_schema_version"], 1);
    assert_eq!(identity["api_schema_version"], 1);
    assert_eq!(identity["run_id"], RUN_ID);
    assert_eq!(identity["owner_pid"], 8123);
    assert_eq!(identity["process_identity"], "test:boot-a:4242");
    assert_eq!(identity["bind_host"], "0.0.0.0");
    assert_eq!(identity["port"], server.identity().port());
    assert_eq!(
        identity["local_endpoint"],
        format!("http://127.0.0.1:{}/", server.identity().port())
    );
    assert_eq!(identity["advertise_url"], Value::Null);
    assert_eq!(identity["base_path"], "/");
    assert_eq!(identity["api_base_path"], "/api/v1");
    assert_eq!(identity["identity_path"], "/api/v1/identity");
    assert_eq!(identity["security_scope"], "trusted_network");
    assert_eq!(
        identity["operational_limits"],
        json!({
            "max_batch_age_ms": "25",
            "max_batch_canonical_bytes": "1048576",
            "max_batch_events": "512",
            "max_page_rows": "500",
            "max_uncommitted_canonical_bytes": "67108864",
            "max_uncommitted_events": "32768",
            "shutdown_drain_timeout_ms": "30000",
            "writer_stall_timeout_ms": "10000",
        })
    );

    let injected = request(&server, "GET", "/api/v1/test?cue=2", &[]);
    assert_eq!(injected.status, 200);
    assert_eq!(injected.json()["query"], "cue=2");
    assert_eq!(injected.json()["server_pid"], std::process::id());
    assert_eq!(injected.json()["server_thread"], "troupe-diagnostic-http");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert!(server.try_core_failure().is_none());
    server.shutdown().unwrap();
}

#[test]
fn configured_base_path_and_explicit_port_are_authoritative() {
    let reservation = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let advertise = WebBaseUrl::parse("https://diagnostics.example/troupe/").unwrap();
    let limits = OperationalLimits::default()
        .with_limit("sse_heartbeat_interval_ms", 5_000)
        .unwrap();
    let server = DiagnosticServer::start(
        config()
            .with_bind("127.0.0.1", port)
            .with_advertise_url(Some(advertise))
            .with_operational_limits(limits),
        vec![test_route(Arc::new(AtomicUsize::new(0)))],
    )
    .unwrap();

    assert_eq!(server.identity().port(), port);
    assert_eq!(request(&server, "GET", "/api/v1/identity", &[]).status, 404);
    assert_eq!(request(&server, "GET", "/api/v1/test", &[]).status, 404);

    let response = request(&server, "GET", "/troupe/api/v1/identity", &[]);
    assert_eq!(response.status, 200);
    let identity = response.json();
    assert_eq!(
        identity["advertise_url"],
        "https://diagnostics.example/troupe/"
    );
    assert_eq!(identity["base_path"], "/troupe");
    assert_eq!(identity["api_base_path"], "/troupe/api/v1");
    assert_eq!(identity["identity_path"], "/troupe/api/v1/identity");
    assert_eq!(
        identity["operational_limits"]["sse_heartbeat_interval_ms"],
        "5000"
    );
    assert_eq!(
        request(&server, "GET", "/troupe/api/v1/test", &[]).status,
        200
    );
    server.shutdown().unwrap();
}

#[test]
fn methods_routes_cache_and_cors_form_a_read_only_shell() {
    let header_attempt = RouteDefinition::read_only("/api/v1/header-attempt", |_request| async {
        Ok(RouteResponse::empty(StatusCode::NO_CONTENT)
            .with_header(
                hyper::header::ACCESS_CONTROL_ALLOW_ORIGIN,
                hyper::header::HeaderValue::from_static("*"),
            )
            .with_header(
                hyper::header::CACHE_CONTROL,
                hyper::header::HeaderValue::from_static("public"),
            ))
    })
    .unwrap();
    let server = DiagnosticServer::start(
        config(),
        vec![test_route(Arc::new(AtomicUsize::new(0))), header_attempt],
    )
    .unwrap();
    let get = request(&server, "GET", "/api/v1/identity", &[]);
    let head = request(&server, "HEAD", "/api/v1/identity", &[]);
    assert_eq!(get.status, 200);
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());
    assert_eq!(
        head.header("content-length"),
        Some(get.body.len().to_string().as_str())
    );

    for method in ["POST", "PUT", "PATCH", "DELETE", "OPTIONS", "CONNECT"] {
        let response = request(&server, method, "/api/v1/identity", &[]);
        assert_eq!(response.status, 405, "method {method}");
        assert_eq!(response.header("allow"), Some("GET, HEAD"));
        assert_common_headers(&response);
    }
    for path in ["/status", "/api/v1/status", "/", "/api/v1/missing"] {
        let response = request(&server, "GET", path, &[]);
        assert_eq!(response.status, 404, "path {path}");
        assert_common_headers(&response);
    }

    let origin = request(
        &server,
        "GET",
        "/api/v1/identity",
        &[("Origin", "https://other.example")],
    );
    assert_common_headers(&origin);
    let header_attempt = request(&server, "GET", "/api/v1/header-attempt", &[]);
    assert_eq!(header_attempt.status, 204);
    assert_common_headers(&header_attempt);
    server.shutdown().unwrap();
}

#[test]
fn forwarded_headers_are_ignored_before_routing_or_handler_dispatch() {
    let server = DiagnosticServer::start(
        config().with_advertise_url(Some(
            WebBaseUrl::parse("https://public.example/base").unwrap(),
        )),
        vec![test_route(Arc::new(AtomicUsize::new(0)))],
    )
    .unwrap();
    let headers = [
        ("Forwarded", "host=evil.example;proto=https"),
        ("FORWARDED", "host=other.example"),
        ("X-Forwarded-Host", "evil.example"),
        ("x-forwarded-host", "other.example"),
        ("X-FORWARDED-PROTO", "http"),
        ("X-Forwarded-Prefix", "/wrong"),
        ("x-forwarded-prefix", "/also-wrong"),
    ];

    let identity_path = "/base/api/v1/identity";
    let baseline = request(&server, "GET", identity_path, &[]);
    let forwarded = request(&server, "GET", identity_path, &headers);
    assert_eq!(forwarded.status, baseline.status);
    assert_eq!(forwarded.body, baseline.body);

    let route_path = "/base/api/v1/test?cue=3";
    let baseline = request(&server, "GET", route_path, &[]);
    let forwarded = request(&server, "GET", route_path, &headers);
    assert_eq!(forwarded.status, baseline.status);
    assert_eq!(forwarded.body, baseline.body);
    assert_eq!(forwarded.json()["forwarded_visible"], false);

    assert_eq!(
        request(&server, "GET", "/wrong/api/v1/identity", &headers).status,
        404
    );
    server.shutdown().unwrap();
}

#[test]
fn bind_failure_is_a_synchronous_start_failure() {
    let occupied = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = occupied.local_addr().unwrap().port();
    let error =
        DiagnosticServer::start(config().with_bind("127.0.0.1", port), Vec::new()).unwrap_err();
    assert_eq!(error.code(), ServerStartErrorCode::BindFailed);
}

#[test]
fn request_errors_and_client_disconnects_stay_local() {
    let failing = RouteDefinition::read_only("/api/v1/fail", |_request| async {
        Err(RequestError::new(
            "injected_request_failure",
            "expected test failure",
        ))
    })
    .unwrap();
    let server = DiagnosticServer::start(config(), vec![failing]).unwrap();

    let mut disconnected = TcpStream::connect(server.connect_addr()).unwrap();
    disconnected
        .write_all(b"GET /api/v1/identity HTTP/1.1\r\nHost:")
        .unwrap();
    drop(disconnected);

    let failed = request(&server, "GET", "/api/v1/fail", &[]);
    assert_eq!(failed.status, 500);
    assert_eq!(failed.json()["error"]["code"], "injected_request_failure");
    assert_common_headers(&failed);
    assert_eq!(request(&server, "GET", "/api/v1/identity", &[]).status, 200);
    assert!(server.try_core_failure().is_none());
    server.shutdown().unwrap();
}

#[test]
fn injected_routes_can_stream_without_buffering_the_response() {
    let streaming = RouteDefinition::get("/api/v1/stream", |_request| async {
        let frames = stream::iter([
            Ok::<_, io::Error>(Frame::data(Bytes::from_static(b"part-one"))),
            Ok(Frame::data(Bytes::from_static(b"part-two"))),
        ]);
        Ok(RouteResponse::stream(
            StatusCode::OK,
            StreamBody::new(frames),
        ))
    })
    .unwrap();
    let server = DiagnosticServer::start(config(), vec![streaming]).unwrap();

    let response = request(&server, "GET", "/api/v1/stream", &[]);
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"part-onepart-two");
    assert_eq!(response.header("transfer-encoding"), Some("chunked"));
    assert_common_headers(&response);
    let head = request(&server, "HEAD", "/api/v1/stream", &[]);
    assert_eq!(head.status, 405);
    assert_eq!(head.header("allow"), Some("GET"));
    assert!(head.body.is_empty());
    assert!(server.try_core_failure().is_none());
    server.shutdown().unwrap();
}

#[test]
fn unexpected_execution_context_exit_is_reported_as_core_fatal() {
    let server = DiagnosticServer::start(config(), Vec::new()).unwrap();
    server.trigger_context_exit_for_test();
    let failure = server
        .wait_for_core_failure(Duration::from_secs(2))
        .unwrap();
    assert_eq!(
        failure.code(),
        ServerCoreFailureCode::ExecutionContextExited
    );
    server.shutdown().unwrap();
}

#[test]
fn invalid_injected_routes_fail_before_listener_readiness() {
    for path in ["api/v1/test", "/api/v1/identity", "/a/../b", "/a?query=1"] {
        let result = RouteDefinition::read_only(path, |_request| async {
            RouteResponse::json(StatusCode::OK, &json!({"ok": true}))
                .map_err(|error| RequestError::new("encode", error.to_string()))
        });
        assert!(result.is_err(), "accepted route {path}");
    }

    let duplicate_a = RouteDefinition::read_only("/api/v1/test", |_request| async {
        Ok(RouteResponse::empty(StatusCode::NO_CONTENT))
    })
    .unwrap();
    let duplicate_b = RouteDefinition::read_only("/api/v1/test", |_request| async {
        Ok(RouteResponse::empty(StatusCode::NO_CONTENT))
    })
    .unwrap();
    let error = DiagnosticServer::start(config(), vec![duplicate_a, duplicate_b]).unwrap_err();
    assert_eq!(error.code(), ServerStartErrorCode::InvalidRoutes);
}
