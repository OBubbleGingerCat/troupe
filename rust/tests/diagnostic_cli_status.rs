use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use clap::Parser;
use troupe_diagnostics_core::id::CanonicalUuid;
use troupe_diagnostics_runtime::{
    archive::lease::ActiveArchiveLease,
    query::status::ActiveStatusObservation,
    registry::{model::WebBaseUrl, process_identity::ProcessIdentity},
    server::{
        query::{QueryCoreFailureSignal, QueryEndpoints},
        runtime::{DiagnosticServer, ServerConfig},
    },
    store::{
        admission::MandatoryIngress,
        connection::{DiagnosticStore, InitialStoreMetadata},
        progress::WriterProgressSupervisor,
        quota::RunQuota,
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
#[path = "../src/application/diagnostic_cli/status.rs"]
mod status;
#[path = "../src/application/diagnostic_cli/target.rs"]
mod target;
#[path = "../src/application/diagnostic_cli/values.rs"]
mod values;

use args::{DiagnosticCommand, DocumentFormat, TroupeArgs, TroupeInvocation};
use http_client::DiagnosticHttpClient;
use resolver::ResolvedDiagnosticTarget;
use status::{ExpectedSource, StatusErrorCode, decode_status_response};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
const OTHER_RUN_ID: &str = "87654321-4321-4321-8321-cba987654321";
const STARTED_AT: &str = "2026-08-16T00:00:00Z";
const CONFIGURATION_IDENTITY: &str = "configuration-sha256:d02";

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestRunDirectory(PathBuf);

impl TestRunDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "troupe-d02-status-{label}-{}-{sequence}",
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

fn run_id() -> CanonicalUuid {
    CanonicalUuid::parse(RUN_ID).expect("canonical test Run UUID")
}

fn other_run_id() -> CanonicalUuid {
    CanonicalUuid::parse(OTHER_RUN_ID).expect("canonical other Run UUID")
}

fn process_identity() -> ProcessIdentity {
    ProcessIdentity::new("test", "d02:4242").expect("valid process identity")
}

fn create_store(directory: &Path, run_id: CanonicalUuid) -> DiagnosticStore {
    DiagnosticStore::create(
        directory,
        &InitialStoreMetadata::new(run_id, STARTED_AT, CONFIGURATION_IDENTITY),
    )
    .expect("create diagnostic store")
}

fn fixture(name: &str) -> String {
    fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tests/fixtures/diagnostics/cli")
            .join(name),
    )
    .expect("read status fixture")
}

fn parse_archive_status(directory: &Path, format: Option<&str>) -> args::StatusArgs {
    let mut argv = vec![
        "troupe".to_owned(),
        "diagnostic".to_owned(),
        "status".to_owned(),
        "--archive".to_owned(),
        directory.display().to_string(),
    ];
    if let Some(format) = format {
        argv.extend(["--format".to_owned(), format.to_owned()]);
    }
    let invocation = TroupeArgs::try_parse_from(argv)
        .expect("valid status arguments")
        .into_invocation();
    match invocation {
        TroupeInvocation::Diagnostic(DiagnosticCommand::Status(arguments)) => arguments,
        _ => panic!("expected diagnostic status invocation"),
    }
}

fn archived_status_bytes(
    run_id: &str,
    api_schema_version: u8,
    source: &str,
    lifecycle: &str,
) -> Vec<u8> {
    format!(
        concat!(
            "{{",
            "\"api_schema_version\":{api_schema_version},",
            "\"run_id\":\"{run_id}\",",
            "\"source\":\"{source}\",",
            "\"store_schema_version\":\"1\",",
            "\"store_schema_identity\":\"troupe.diagnostics.store.v1\",",
            "\"event_schema_version\":\"1\",",
            "\"configuration_identity\":\"configuration-sha256:d02\",",
            "\"event_watermark\":\"2\",",
            "\"read_model_watermark\":\"2\",",
            "\"lifecycle\":{lifecycle},",
            "\"writer\":{{\"status\":\"unavailable\",\"reason\":\"archive\"}},",
            "\"quota\":{{\"status\":\"unavailable\",\"reason\":\"archive\"}}",
            "}}"
        ),
        api_schema_version = api_schema_version,
        run_id = run_id,
        source = source,
        lifecycle = lifecycle,
    )
    .into_bytes()
}

#[tokio::test]
async fn archive_status_uses_q00_semantics_and_matches_human_and_json_fixtures() {
    let directory = TestRunDirectory::new("archive-golden");
    let lease = ActiveArchiveLease::acquire(directory.path()).expect("active archive lease");
    let store = create_store(directory.path(), run_id());
    drop(store);
    drop(lease);

    let human = status::execute(parse_archive_status(directory.path(), None))
        .await
        .expect("incomplete archive status is observable data");
    assert_eq!(human, fixture("status-human.txt"));

    let json = status::execute(parse_archive_status(directory.path(), Some("json")))
        .await
        .expect("archive JSON status");
    assert_eq!(json, fixture("status-v1.json"));
    assert!(json.ends_with('\n'));
    assert!(!json[..json.len() - 1].contains('\n'));
    assert!(json.contains("\"ended_at\":null"));
    assert!(json.contains("\"outcome\":null"));
    assert_eq!(json.matches("\"reason\":\"archive\"").count(), 2);
}

#[tokio::test]
async fn live_status_queries_h01_and_preserves_available_writer_quota_and_limits() {
    let directory = TestRunDirectory::new("live-available");
    let lease =
        Arc::new(ActiveArchiveLease::acquire(directory.path()).expect("active archive lease"));
    let _store = create_store(directory.path(), run_id());
    let (ingress, _ingress_failures) = MandatoryIngress::new();
    let progress = WriterProgressSupervisor::default();
    let (quota, _quota_failures) = RunQuota::new(directory.path(), None).expect("disabled quota");
    let observation = ActiveStatusObservation::available(
        ingress.status().expect("ingress status"),
        progress.status(),
        quota.status().expect("quota status"),
    );
    let endpoints = QueryEndpoints::active(
        run_id(),
        lease,
        move || Some(observation.clone()),
        |_failure: QueryCoreFailureSignal| {},
    );
    let server = DiagnosticServer::start(
        ServerConfig::new(run_id(), std::process::id(), process_identity())
            .with_bind("127.0.0.1", 0),
        endpoints.route_definitions().expect("valid query routes"),
    )
    .expect("start diagnostic server");
    let base_url = WebBaseUrl::parse(&format!("http://{}/", server.connect_addr()))
        .expect("valid local server URL");
    let client = DiagnosticHttpClient::connect(base_url)
        .await
        .expect("validated diagnostic HTTP client");

    let document = status::query(ResolvedDiagnosticTarget::Live(client))
        .await
        .expect("live diagnostic status");
    let json = document.render(DocumentFormat::Json);
    for required in [
        "\"source\":\"active\"",
        "\"security_scope\":\"trusted_network\"",
        "\"state\":\"active\"",
        "\"writer\":{\"status\":\"available\"",
        "\"max_uncommitted_events\":\"32768\"",
        "\"queued_events\":\"0\"",
        "\"progress_committed_watermark\":\"0\"",
        "\"quota\":{\"status\":\"available\"",
        "\"max_run_bytes\":null",
        "\"sealed\":false",
    ] {
        assert!(json.contains(required), "missing {required} from {json}");
    }
    assert!(json.ends_with('\n'));
    server.shutdown().expect("clean server shutdown");
}

#[test]
fn failed_and_incomplete_outcomes_are_documents_not_operation_errors() {
    let failed_lifecycle = concat!(
        "{",
        "\"state\":\"failed\",",
        "\"started_at\":\"2026-08-16T00:00:00Z\",",
        "\"ended_at\":\"2026-08-16T00:00:01Z\",",
        "\"outcome\":\"failed\",",
        "\"clean_shutdown\":true",
        "}"
    );
    let failed = decode_status_response(
        &archived_status_bytes(RUN_ID, 1, "archive", failed_lifecycle),
        run_id(),
        ExpectedSource::Archive,
    )
    .expect("failed Production is observable status data")
    .render(DocumentFormat::Json);
    assert!(failed.contains("\"state\":\"failed\""));
    assert!(failed.contains("\"outcome\":\"failed\""));

    let incomplete = fixture("status-v1.json");
    assert!(incomplete.contains("\"state\":\"incomplete\""));
    assert!(incomplete.contains("\"clean_shutdown\":false"));
}

#[test]
fn protocol_identity_source_and_shape_errors_fail_closed() {
    let lifecycle = concat!(
        "{",
        "\"state\":\"incomplete\",",
        "\"started_at\":\"2026-08-16T00:00:00Z\",",
        "\"ended_at\":null,",
        "\"outcome\":null,",
        "\"clean_shutdown\":false",
        "}"
    );
    let incompatible = decode_status_response(
        &archived_status_bytes(RUN_ID, 2, "archive", lifecycle),
        run_id(),
        ExpectedSource::Archive,
    )
    .unwrap_err();
    assert_eq!(incompatible.code(), StatusErrorCode::IncompatibleResponse);

    let wrong_run = decode_status_response(
        &archived_status_bytes(OTHER_RUN_ID, 1, "archive", lifecycle),
        run_id(),
        ExpectedSource::Archive,
    )
    .unwrap_err();
    assert_eq!(wrong_run.code(), StatusErrorCode::RunIdentityMismatch);

    let wrong_source = decode_status_response(
        &archived_status_bytes(RUN_ID, 1, "active", lifecycle),
        run_id(),
        ExpectedSource::Archive,
    )
    .unwrap_err();
    assert_eq!(wrong_source.code(), StatusErrorCode::SourceMismatch);

    let incompatible_store =
        String::from_utf8(archived_status_bytes(RUN_ID, 1, "archive", lifecycle))
            .unwrap()
            .replace(
                "\"store_schema_version\":\"1\"",
                "\"store_schema_version\":\"2\"",
            );
    let incompatible_store = decode_status_response(
        incompatible_store.as_bytes(),
        run_id(),
        ExpectedSource::Archive,
    )
    .unwrap_err();
    assert_eq!(
        incompatible_store.code(),
        StatusErrorCode::IncompatibleResponse
    );

    let noncanonical_watermark =
        String::from_utf8(archived_status_bytes(RUN_ID, 1, "archive", lifecycle))
            .unwrap()
            .replace("\"event_watermark\":\"2\"", "\"event_watermark\":\"02\"");
    let noncanonical_watermark = decode_status_response(
        noncanonical_watermark.as_bytes(),
        run_id(),
        ExpectedSource::Archive,
    )
    .unwrap_err();
    assert_eq!(
        noncanonical_watermark.code(),
        StatusErrorCode::InvalidResponse
    );

    let malformed = decode_status_response(
        b"{\"api_schema_version\":1",
        run_id(),
        ExpectedSource::Archive,
    )
    .unwrap_err();
    assert_eq!(malformed.code(), StatusErrorCode::InvalidResponse);

    let oversized = vec![b' '; 1024 * 1024 + 1];
    let oversized =
        decode_status_response(&oversized, run_id(), ExpectedSource::Archive).unwrap_err();
    assert_eq!(oversized.code(), StatusErrorCode::ResponseTooLarge);
}

#[tokio::test]
async fn captured_h01_response_run_identity_is_checked_against_the_resolved_server() {
    let directory = TestRunDirectory::new("response-identity");
    let lease =
        Arc::new(ActiveArchiveLease::acquire(directory.path()).expect("active archive lease"));
    let _store = create_store(directory.path(), other_run_id());
    let endpoints = QueryEndpoints::active_unobserved(
        other_run_id(),
        lease,
        |_failure: QueryCoreFailureSignal| {},
    );
    let server = DiagnosticServer::start(
        ServerConfig::new(run_id(), std::process::id(), process_identity())
            .with_bind("127.0.0.1", 0),
        endpoints
            .route_definitions()
            .expect("valid mismatched query routes"),
    )
    .expect("start identity-mismatch server");
    let base_url = WebBaseUrl::parse(&format!("http://{}/", server.connect_addr()))
        .expect("valid local server URL");
    let client = DiagnosticHttpClient::connect(base_url)
        .await
        .expect("server identity resolves the configured Run");

    let error = status::query(ResolvedDiagnosticTarget::Live(client))
        .await
        .unwrap_err();
    assert_eq!(error.code(), StatusErrorCode::RunIdentityMismatch);
    server.shutdown().expect("clean server shutdown");
}

#[tokio::test]
async fn archive_store_and_resolver_failures_are_operation_errors() {
    let missing = TestRunDirectory::new("missing-store");
    let error = status::execute(parse_archive_status(missing.path(), Some("json")))
        .await
        .unwrap_err();
    assert_eq!(error.code(), StatusErrorCode::Resolve);
}
