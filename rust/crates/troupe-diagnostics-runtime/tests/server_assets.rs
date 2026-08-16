use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::TcpStream,
    time::Duration,
};

use bytes::Bytes;
use http_body_util::Full;
use hyper::StatusCode;
use serde_json::json;
use troupe_diagnostics_core::id::CanonicalUuid;
use troupe_diagnostics_runtime::{
    registry::{model::WebBaseUrl, process_identity::ProcessIdentity},
    server::{
        assets,
        error::RequestError,
        routes::{RouteDefinition, RouteResponse},
        runtime::{DiagnosticServer, ServerConfig},
        sse::frame::sse_route_response,
    },
};

#[allow(dead_code)]
mod generated {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/generated/assets.rs"
    ));
}

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";

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
}

fn run_id() -> CanonicalUuid {
    CanonicalUuid::parse(RUN_ID).unwrap()
}

fn config() -> ServerConfig {
    ServerConfig::new(
        run_id(),
        8123,
        ProcessIdentity::new("test", "boot-assets:4242").unwrap(),
    )
}

fn start(routes: Vec<RouteDefinition>) -> DiagnosticServer {
    DiagnosticServer::start(config().with_bind("127.0.0.1", 0), routes).unwrap()
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

fn representation(kind: &str, encoding: &str) -> &'static generated::GeneratedRepresentation {
    generated::REPRESENTATIONS
        .iter()
        .find(|candidate| candidate.kind == kind && candidate.encoding == encoding)
        .unwrap()
}

fn logical_path(kind: &str) -> String {
    representation(kind, "raw")
        .url
        .strip_prefix('.')
        .unwrap()
        .to_owned()
}

fn assert_security_headers(response: &HttpResponse) {
    let csp = response.header("content-security-policy").unwrap();
    assert!(csp.contains("default-src 'none'"));
    assert!(csp.contains("script-src 'self'"));
    assert!(csp.contains("connect-src 'self'"));
    assert!(csp.contains("frame-ancestors 'none'"));
    assert!(!csp.contains("script-src 'unsafe-inline'"));
    assert!(!csp.contains("'unsafe-eval'"));
    assert_eq!(response.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(response.header("referrer-policy"), Some("no-referrer"));
    assert_eq!(
        response.header("cross-origin-resource-policy"),
        Some("same-origin")
    );
    assert_eq!(
        response.header("cross-origin-opener-policy"),
        Some("same-origin")
    );
    assert!(
        response
            .headers
            .keys()
            .all(|name| !name.starts_with("access-control-"))
    );
}

#[test]
fn generated_route_inventory_is_relative_hashed_and_compile_time_only() {
    let routes = assets::route_definitions().unwrap();
    let mut paths: Vec<String> = routes
        .iter()
        .map(|route| route.relative_path().to_owned())
        .collect();
    paths.sort_unstable();
    let mut expected = vec!["/".to_owned(), logical_path("css"), logical_path("js")];
    expected.sort_unstable();
    assert_eq!(paths, expected);
    assert_eq!(assets::build_sha256(), generated::BUILD_SHA256);
    for path in paths.into_iter().filter(|path| *path != "/") {
        assert!(path.starts_with("/assets/diagnostics-"));
        assert!(path.contains(generated::BUILD_SHA256));
        assert!(path.ends_with(".js") || path.ends_with(".css"));
    }

    let html = std::str::from_utf8(generated::INDEX_HTML).unwrap();
    assert_eq!(html.matches("<script ").count(), 1);
    assert_eq!(html.matches("<link ").count(), 1);
    assert!(!html.contains("<script>") && !html.contains("<style"));
    assert!(!html.contains("http://") && !html.contains("https://") && !html.contains("//assets"));
    assert!(html.contains("src=\"./assets/diagnostics-"));
    assert!(html.contains("href=\"./assets/diagnostics-"));

    let source = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/server/assets.rs"));
    for forbidden in [
        "flate",
        "brotli::",
        "Compression",
        "std::fs",
        "Command::new",
    ] {
        assert!(!source.contains(forbidden));
    }
}

#[test]
fn html_get_head_conditional_and_security_contract_is_exact() {
    let server = start(assets::route_definitions().unwrap());

    let get = request(&server, "GET", "/", &[]);
    assert_eq!(get.status, 200);
    assert_eq!(get.body, generated::INDEX_HTML);
    assert_eq!(get.header("content-type"), Some(generated::INDEX_HTML_MIME));
    assert_eq!(get.header("cache-control"), Some("no-cache"));
    assert!(get.header("etag").unwrap().starts_with("\"sha256-"));
    assert_security_headers(&get);

    let head = request(&server, "HEAD", "/", &[]);
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());
    let html_length = generated::INDEX_HTML.len().to_string();
    assert_eq!(head.header("content-length"), Some(html_length.as_str()));
    assert_eq!(head.header("etag"), get.header("etag"));
    assert_security_headers(&head);

    let not_modified = request(
        &server,
        "GET",
        "/",
        &[("If-None-Match", get.header("etag").unwrap())],
    );
    assert_eq!(not_modified.status, 304);
    assert!(not_modified.body.is_empty());
    assert_eq!(not_modified.header("cache-control"), Some("no-cache"));
    assert_eq!(not_modified.header("etag"), get.header("etag"));
    assert_security_headers(&not_modified);

    server.shutdown().unwrap();
}

#[test]
fn encoding_negotiation_etag_and_conditional_are_representation_specific() {
    let server = start(assets::route_definitions().unwrap());
    let path = logical_path("js");
    let cases = [
        (None, "raw", None),
        (Some("gzip"), "gzip", Some("gzip")),
        (Some("gzip, br"), "br", Some("br")),
        (
            Some("br;q=0.5, gzip;q=0.8, identity;q=0.1"),
            "gzip",
            Some("gzip"),
        ),
        (Some("*;q=1"), "br", Some("br")),
        (Some("br;q=bogus, gzip;q=0"), "raw", None),
    ];
    let mut etags = Vec::new();
    for (accept, expected_encoding, content_encoding) in cases {
        let headers: Vec<(&str, &str)> = accept
            .map(|value| vec![("Accept-Encoding", value)])
            .unwrap_or_default();
        let response = request(&server, "GET", &path, &headers);
        let expected = representation("js", expected_encoding);
        assert_eq!(response.status, 200);
        assert_eq!(response.body, expected.bytes);
        assert_eq!(response.header("content-type"), Some(expected.mime));
        assert_eq!(response.header("content-encoding"), content_encoding);
        assert_eq!(response.header("vary"), Some("Accept-Encoding"));
        assert_eq!(
            response.header("cache-control"),
            Some("public, max-age=31536000, immutable")
        );
        assert_security_headers(&response);
        etags.push(response.header("etag").unwrap().to_owned());
    }
    etags.sort();
    etags.dedup();
    assert_eq!(etags.len(), 3);

    let brotli = request(&server, "GET", &path, &[("Accept-Encoding", "br")]);
    let weak = format!("W/{}", brotli.header("etag").unwrap());
    let not_modified = request(
        &server,
        "GET",
        &path,
        &[("Accept-Encoding", "br"), ("If-None-Match", &weak)],
    );
    assert_eq!(not_modified.status, 304);
    assert!(not_modified.body.is_empty());
    assert_eq!(not_modified.header("etag"), brotli.header("etag"));
    assert_eq!(not_modified.header("content-encoding"), Some("br"));

    let unacceptable = request(
        &server,
        "GET",
        &path,
        &[("Accept-Encoding", "br;q=0, gzip;q=0, identity;q=0")],
    );
    assert_eq!(unacceptable.status, 406);
    assert!(unacceptable.body.is_empty());
    assert_eq!(unacceptable.header("cache-control"), Some("no-store"));
    assert_eq!(unacceptable.header("vary"), Some("Accept-Encoding"));
    assert_security_headers(&unacceptable);

    server.shutdown().unwrap();
}

#[test]
fn reverse_proxy_base_api_no_store_and_sse_policy_remain_distinct() {
    let api = RouteDefinition::read_only("/api/v1/test", |_request| async move {
        RouteResponse::json(StatusCode::OK, &json!({"ok": true}))
            .map_err(|error| RequestError::new("test_json", error.to_string()))
    })
    .unwrap();
    let sse = RouteDefinition::get("/api/v1/events", |_request| async move {
        Ok(sse_route_response(Full::new(Bytes::from_static(
            b": heartbeat\n\n",
        ))))
    })
    .unwrap();
    let mut routes = assets::route_definitions().unwrap();
    routes.extend([api, sse]);
    let advertise = WebBaseUrl::parse("https://diagnostics.example/proxy/run/").unwrap();
    let server = DiagnosticServer::start(
        config()
            .with_bind("127.0.0.1", 0)
            .with_advertise_url(Some(advertise)),
        routes,
    )
    .unwrap();

    assert_eq!(request(&server, "GET", "/", &[]).status, 404);
    assert_eq!(
        request(&server, "GET", &logical_path("css"), &[]).status,
        404
    );
    let html = request(&server, "GET", "/proxy/run/", &[]);
    assert_eq!(html.status, 200);
    assert_eq!(html.body, generated::INDEX_HTML);
    let css = request(
        &server,
        "GET",
        &format!("/proxy/run{}", logical_path("css")),
        &[("Accept-Encoding", "gzip")],
    );
    assert_eq!(css.status, 200);
    assert_eq!(css.header("content-encoding"), Some("gzip"));

    let api = request(&server, "GET", "/proxy/run/api/v1/test", &[]);
    assert_eq!(api.status, 200);
    assert_eq!(api.header("cache-control"), Some("no-store"));
    assert!(api.header("content-security-policy").is_none());
    let sse = request(&server, "GET", "/proxy/run/api/v1/events", &[]);
    assert_eq!(sse.status, 200);
    assert_eq!(sse.header("cache-control"), Some("no-cache, no-transform"));
    assert_eq!(
        sse.header("content-type"),
        Some("text/event-stream; charset=utf-8")
    );

    server.shutdown().unwrap();
}

#[test]
fn methods_unknown_assets_and_forwarded_headers_cannot_weaken_the_surface() {
    let server = start(assets::route_definitions().unwrap());
    let js = logical_path("js");
    assert_eq!(request(&server, "POST", "/", &[]).status, 405);
    assert_eq!(request(&server, "POST", &js, &[]).status, 405);
    assert_eq!(
        request(&server, "GET", "/assets/diagnostics-unhashed.js", &[]).status,
        404
    );
    let forwarded = request(
        &server,
        "GET",
        &js,
        &[
            ("Accept-Encoding", "br"),
            ("Forwarded", "host=evil.invalid"),
            ("X-Forwarded-Prefix", "/evil"),
            ("Origin", "https://evil.invalid"),
        ],
    );
    assert_eq!(forwarded.status, 200);
    assert_eq!(forwarded.header("content-encoding"), Some("br"));
    assert_security_headers(&forwarded);
    assert!(
        forwarded
            .headers
            .keys()
            .all(|name| !name.starts_with("access-control-"))
    );
    server.shutdown().unwrap();
}
