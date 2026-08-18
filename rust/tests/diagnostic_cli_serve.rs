use std::{
    collections::BTreeMap,
    convert::Infallible,
    ffi::OsString,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use clap::Parser;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use troupe_diagnostics_core::id::CanonicalUuid;
use troupe_diagnostics_runtime::{
    archive::lease::{
        ActiveArchiveLease, ArchiveLeaseErrorCode, CleanupArchiveLease, SharedArchiveLease,
    },
    registry::{
        codec::encode_registry_entry,
        model::{BindEndpoint, RegistryEntry},
        process_identity::current_process_identity,
    },
    server::runtime::{DiagnosticServer, ServerConfig},
    store::{
        connection::{DiagnosticStore, InitialStoreMetadata},
    },
};

#[path = "../src/application/diagnostic_cli/archive_target.rs"]
mod archive_target;
#[path = "../src/application/diagnostic_cli/args.rs"]
mod args;
#[path = "../src/application/diagnostic_cli/http_client.rs"]
mod http_client;
#[path = "../src/application/diagnostic_cli/resolver.rs"]
mod resolver;
#[path = "../src/application/diagnostic_cli/serve.rs"]
mod serve;
#[path = "../src/application/diagnostic_cli/target.rs"]
mod target;
#[path = "../src/application/diagnostic_cli/values.rs"]
mod values;

use archive_target::ArchiveTarget;
use args::{DiagnosticCommand, ServeArgs, TroupeArgs, TroupeInvocation};
use serve::{
    ARCHIVE_READY_PREFIX, BrowserLauncher, ServeErrorCode, ServeOutput, ServeTermination,
    execute_with_launcher, start_archive,
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
const STARTED_AT: &str = "2026-08-16T00:00:00Z";
const ENDED_AT: &str = "2026-08-16T00:00:01Z";
const CONFIGURATION_IDENTITY: &str = "configuration-sha256:d04";

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "troupe-d04-serve-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_id() -> CanonicalUuid {
    CanonicalUuid::parse(RUN_ID).unwrap()
}

fn create_archive(label: &str, clean_shutdown: bool) -> TestDirectory {
    let directory = TestDirectory::new(label);
    let active = ActiveArchiveLease::acquire(directory.path()).unwrap();
    let store = DiagnosticStore::create(
        directory.path(),
        &InitialStoreMetadata::new(run_id(), STARTED_AT, CONFIGURATION_IDENTITY),
    )
    .unwrap();
    if clean_shutdown {
        store
            .connection()
            .execute(
                "UPDATE run_metadata SET ended_at = ?1, production_outcome = 'completed', \
                 clean_shutdown = 1 WHERE singleton = 1",
                [ENDED_AT],
            )
            .unwrap();
    }
    drop(store);
    drop(active);
    directory
}

fn open_archive(directory: &Path) -> ArchiveTarget {
    ArchiveTarget::open_identified(directory).unwrap()
}

fn archive_files(directory: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            files.insert(
                entry.file_name().to_string_lossy().into_owned(),
                fs::read(entry.path()).unwrap(),
            );
        }
    }
    files
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

fn connect_addr(local_url: &str) -> std::net::SocketAddr {
    local_url
        .strip_prefix("http://")
        .unwrap()
        .strip_suffix('/')
        .unwrap()
        .parse()
        .unwrap()
}

fn request(local_url: &str, path: &str, headers: &[(&str, &str)]) -> HttpResponse {
    let mut stream = TcpStream::connect(connect_addr(local_url)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: archive.test\r\nConnection: close\r\n"
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
    let headers = lines
        .map(|line| {
            let (name, value) = line.split_once(':').unwrap();
            (name.to_ascii_lowercase(), value.trim().to_owned())
        })
        .collect::<BTreeMap<_, _>>();
    let mut body = bytes[header_end + 4..].to_vec();
    if headers
        .get("transfer-encoding")
        .is_some_and(|value| value == "chunked")
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
    let mut output = Vec::new();
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
        output.extend_from_slice(&remaining[..size]);
        remaining = &remaining[size + 2..];
    }
    output
}

fn serve_args(arguments: Vec<OsString>) -> ServeArgs {
    match TroupeArgs::try_parse_from(arguments)
        .unwrap()
        .into_invocation()
    {
        TroupeInvocation::Diagnostic(DiagnosticCommand::Serve(arguments)) => arguments,
        other => panic!("expected diagnostic serve, got {other:?}"),
    }
}

fn archive_serve_args(directory: &Path, open: bool) -> ServeArgs {
    let mut arguments = vec![
        OsString::from("troupe"),
        OsString::from("diagnostic"),
        OsString::from("serve"),
        OsString::from("--archive"),
        directory.as_os_str().to_owned(),
    ];
    if open {
        arguments.push(OsString::from("--open"));
    }
    serve_args(arguments)
}

struct RecordingOutput {
    cancellation: CancellationToken,
    writes: Vec<String>,
}

impl RecordingOutput {
    fn new(cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            writes: Vec::new(),
        }
    }
}

impl ServeOutput for RecordingOutput {
    type Error = Infallible;

    fn write_stderr(&mut self, text: &str) -> Result<(), Self::Error> {
        self.writes.push(text.to_owned());
        if text.starts_with(ARCHIVE_READY_PREFIX) {
            self.cancellation.cancel();
        }
        Ok(())
    }
}

struct RecordingBrowser {
    calls: Mutex<Vec<String>>,
    failure: Option<String>,
}

impl RecordingBrowser {
    fn succeeding() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            failure: None,
        }
    }

    fn failing(detail: &str) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            failure: Some(detail.to_owned()),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl BrowserLauncher for RecordingBrowser {
    fn launch(&self, url: &str) -> Result<(), String> {
        self.calls.lock().unwrap().push(url.to_owned());
        match &self.failure {
            Some(detail) => Err(detail.clone()),
            None => Ok(()),
        }
    }
}

#[test]
fn archive_server_reuses_full_read_only_routes_and_reports_incomplete_locator() {
    let directory = create_archive("routes with spaces", false);
    let before = archive_files(directory.path());
    let session = start_archive(open_archive(directory.path()), 0).unwrap();
    let locator = session.locator();

    assert_eq!(locator.run_id(), run_id());
    assert!(!locator.clean_shutdown());
    assert_eq!(
        locator.archive_directory(),
        fs::canonicalize(directory.path()).unwrap()
    );
    assert!(locator.local_url().starts_with("http://127.0.0.1:"));
    assert_ne!(connect_addr(locator.local_url()).port(), 0);
    let ready = locator.ready_line().unwrap();
    assert!(ready.starts_with(ARCHIVE_READY_PREFIX));
    assert!(ready.ends_with('\n'));
    let encoded: Value =
        serde_json::from_str(ready.strip_prefix(ARCHIVE_READY_PREFIX).unwrap().trim_end()).unwrap();
    assert_eq!(encoded["locator_schema_version"], 1);
    assert_eq!(encoded["run_id"], RUN_ID);
    assert_eq!(encoded["local_url"], locator.local_url());
    assert_eq!(encoded["clean_shutdown"], false);

    let identity = request(locator.local_url(), "/api/v1/identity", &[]);
    assert_eq!(identity.status, 200);
    assert_eq!(identity.json()["run_id"], RUN_ID);

    let status = request(
        locator.local_url(),
        "/api/v1/status",
        &[("Accept", "application/json")],
    );
    assert_eq!(status.status, 200);
    assert_eq!(status.json()["source"], "archive");
    assert_eq!(status.json()["lifecycle"]["clean_shutdown"], false);

    let snapshot = request(
        locator.local_url(),
        "/api/v1/snapshot",
        &[("Accept", "application/json")],
    );
    assert_eq!(snapshot.status, 200);
    assert_eq!(snapshot.json()["run_id"], RUN_ID);

    let events = request(
        locator.local_url(),
        "/api/v1/events?after=0",
        &[("Accept", "application/json")],
    );
    assert_eq!(events.status, 200);
    assert_eq!(events.json()["events"], serde_json::json!([]));
    let rejected_sse = request(
        locator.local_url(),
        "/api/v1/events?after=0",
        &[("Accept", "text/event-stream")],
    );
    assert_eq!(rejected_sse.status, 406);

    let html = request(locator.local_url(), "/", &[]);
    assert_eq!(html.status, 200);
    assert_eq!(
        html.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert!(!html.body.is_empty());
    assert!(
        html.headers
            .keys()
            .all(|name| !name.starts_with("access-control-"))
    );

    let dump = request(locator.local_url(), "/api/v1/dump", &[]);
    assert_eq!(dump.status, 200);
    assert_eq!(dump.header("content-type"), Some("application/x-protobuf"));
    assert_eq!(dump.header("x-troupe-run-id"), Some(RUN_ID));
    assert!(!dump.body.is_empty());

    session.shutdown().unwrap();
    assert_eq!(archive_files(directory.path()), before);
    assert!(!directory.path().join("instances").exists());
}

#[test]
fn archive_server_holds_one_lifetime_shared_lease_and_releases_every_normal_exit() {
    let directory = create_archive("lease", true);
    let session = start_archive(open_archive(directory.path()), 0).unwrap();
    assert!(session.locator().clean_shutdown());
    assert_eq!(
        CleanupArchiveLease::acquire(directory.path())
            .unwrap_err()
            .code(),
        ArchiveLeaseErrorCode::Contended
    );
    let concurrent_reader = SharedArchiveLease::acquire(directory.path()).unwrap();
    drop(concurrent_reader);
    session.shutdown().unwrap();
    CleanupArchiveLease::acquire(directory.path()).unwrap();
}

#[tokio::test]
async fn open_is_the_only_browser_side_effect_and_launch_failure_is_nonfatal() {
    let directory = create_archive("browser", false);

    let cancellation = CancellationToken::new();
    let mut closed_output = RecordingOutput::new(cancellation.clone());
    let unopened = RecordingBrowser::succeeding();
    let termination = execute_with_launcher(
        archive_serve_args(directory.path(), false),
        &mut closed_output,
        cancellation,
        &unopened,
    )
    .await
    .unwrap();
    assert_eq!(termination, ServeTermination::Interrupted);
    assert_eq!(termination.exit_code(), 130);
    assert!(unopened.calls().is_empty());
    assert_eq!(closed_output.writes.len(), 1);
    assert!(closed_output.writes[0].starts_with(ARCHIVE_READY_PREFIX));

    let cancellation = CancellationToken::new();
    let mut warning_output = RecordingOutput::new(cancellation.clone());
    let failing = RecordingBrowser::failing("browser unavailable\nwithout leaking a line");
    let termination = execute_with_launcher(
        archive_serve_args(directory.path(), true),
        &mut warning_output,
        cancellation,
        &failing,
    )
    .await
    .unwrap();
    assert_eq!(termination, ServeTermination::Interrupted);
    assert_eq!(failing.calls().len(), 1);
    assert_eq!(warning_output.writes.len(), 2);
    assert!(warning_output.writes[0].starts_with(ARCHIVE_READY_PREFIX));
    assert!(warning_output.writes[1].starts_with("troupe: diagnostic archive warning "));
    assert_eq!(warning_output.writes[1].matches('\n').count(), 1);
    let warning: Value = serde_json::from_str(
        warning_output.writes[1]
            .strip_prefix("troupe: diagnostic archive warning ")
            .unwrap()
            .trim_end(),
    )
    .unwrap();
    assert_eq!(warning["warning_schema_version"], 1);
    assert_eq!(warning["code"], "browser_launch_failed");
    assert_eq!(
        warning["detail"],
        "browser unavailable\nwithout leaking a line"
    );
    CleanupArchiveLease::acquire(directory.path()).unwrap();
}

#[tokio::test]
async fn server_core_failure_stops_foreground_serve_and_releases_the_lease() {
    let directory = create_archive("core-failure", false);
    let session = start_archive(open_archive(directory.path()), 0).unwrap();
    session.trigger_server_exit_for_test();
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        session.run_until_cancelled(&CancellationToken::new()),
    )
    .await
    .expect("server failure must stop foreground serve")
    .unwrap_err();
    assert_eq!(error.code(), ServeErrorCode::ServerCore);
    CleanupArchiveLease::acquire(directory.path()).unwrap();
}

#[tokio::test]
async fn serve_rejects_a_revalidated_active_production_without_opening_a_browser() {
    let production = TestDirectory::new("active-production");
    let instances = production.path().join(".troupe/diagnostics/instances");
    let runs = production.path().join(".troupe/diagnostics/runs");
    fs::create_dir_all(&instances).unwrap();
    fs::create_dir_all(&runs).unwrap();
    let run_directory = runs.join(RUN_ID);
    fs::create_dir(&run_directory).unwrap();
    let active_lease = ActiveArchiveLease::acquire(&run_directory).unwrap();
    let store = DiagnosticStore::create(
        &run_directory,
        &InitialStoreMetadata::new(run_id(), STARTED_AT, CONFIGURATION_IDENTITY),
    )
    .unwrap();
    let process_identity = current_process_identity().unwrap();
    let server = DiagnosticServer::start(
        ServerConfig::new(run_id(), std::process::id(), process_identity.clone())
            .with_bind("127.0.0.1", 0),
        Vec::new(),
    )
    .unwrap();
    let entry = RegistryEntry::new(
        run_id(),
        &run_directory,
        std::process::id(),
        process_identity,
        BindEndpoint::new("127.0.0.1", server.local_addr().port()).unwrap(),
        None,
        STARTED_AT,
    )
    .unwrap();
    fs::write(
        instances.join(format!("{}.json", run_id())),
        encode_registry_entry(&entry).unwrap(),
    )
    .unwrap();

    let arguments = serve_args(vec![
        OsString::from("troupe"),
        OsString::from("diagnostic"),
        OsString::from("serve"),
        OsString::from("--production"),
        production.path().as_os_str().to_owned(),
        OsString::from("--run"),
        OsString::from(RUN_ID),
        OsString::from("--open"),
    ]);
    let cancellation = CancellationToken::new();
    let mut output = RecordingOutput::new(cancellation.clone());
    let browser = RecordingBrowser::succeeding();
    let error = execute_with_launcher(arguments, &mut output, cancellation, &browser)
        .await
        .unwrap_err();
    assert_eq!(error.code(), ServeErrorCode::ActiveTarget);
    assert!(output.writes.is_empty());
    assert!(browser.calls().is_empty());
    assert_eq!(
        SharedArchiveLease::acquire(&run_directory)
            .unwrap_err()
            .code(),
        ArchiveLeaseErrorCode::Contended
    );

    server.shutdown().unwrap();
    drop(store);
    drop(active_lease);
}

#[test]
fn occupied_explicit_port_fails_before_ready_and_releases_the_archive_lease() {
    let directory = create_archive("occupied-port", false);
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let error = start_archive(open_archive(directory.path()), port).unwrap_err();
    assert_eq!(error.code(), ServeErrorCode::ServerStart);
    CleanupArchiveLease::acquire(directory.path()).unwrap();
}
