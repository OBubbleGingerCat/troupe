use std::{
    convert::Infallible,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use clap::Parser;
use troupe_diagnostics_core::{
    detail::{ActorDetail, EmptyDetail, SpanStartDetail},
    event::{
        ActTokenUsageFinalized, AgentMessageDelta, CounterSampled, DiagnosticEvent,
        DiagnosticEventHeader, DiagnosticScope, SpanStarted,
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
    query::reader::DiagnosticReader,
    registry::{model::WebBaseUrl, process_identity::ProcessIdentity},
    server::{
        query::{QueryCoreFailureSignal, QueryEndpoints, encode_snapshot_response},
        runtime::{DiagnosticServer, ServerConfig},
    },
    store::{
        batch::EventBatch,
        connection::{DiagnosticStore, InitialStoreMetadata},
        writer::TransactionalWriter,
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
#[path = "../src/application/diagnostic_cli/snapshot.rs"]
mod snapshot;
#[path = "../src/application/diagnostic_cli/target.rs"]
mod target;
#[path = "../src/application/diagnostic_cli/values.rs"]
mod values;

use args::{DiagnosticCommand, DocumentFormat, TroupeArgs, TroupeInvocation};
use http_client::DiagnosticHttpClient;
use resolver::ResolvedDiagnosticTarget;
use snapshot::{SnapshotErrorCode, decode_snapshot_response};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
const OTHER_RUN_ID: &str = "87654321-4321-4321-8321-cba987654321";
const STARTED_AT: &str = "2026-08-16T00:00:00Z";
const CONFIGURATION_IDENTITY: &str = "configuration-sha256:d09";
const LARGE_TOKEN_COUNT: &str = "1234567890123456789012345678901234567890";

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestRunDirectory(PathBuf);

impl TestRunDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "troupe-d09-snapshot-{label}-{}-{sequence}",
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

fn process_identity() -> ProcessIdentity {
    ProcessIdentity::new("test", "d09:4242").expect("valid process identity")
}

fn create_store(directory: &Path) -> DiagnosticStore {
    DiagnosticStore::create(
        directory,
        &InitialStoreMetadata::new(run_id(), STARTED_AT, CONFIGURATION_IDENTITY),
    )
    .expect("create diagnostic store")
}

fn fixture(name: &str) -> String {
    fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tests/fixtures/diagnostics/cli")
            .join(name),
    )
    .expect("read snapshot fixture")
}

fn parse_archive_snapshot(directory: &Path, format: Option<&str>) -> args::SnapshotArgs {
    let mut argv = vec![
        "troupe".to_owned(),
        "diagnostic".to_owned(),
        "snapshot".to_owned(),
        "--archive".to_owned(),
        directory.display().to_string(),
    ];
    if let Some(format) = format {
        argv.extend(["--format".to_owned(), format.to_owned()]);
    }
    let invocation = TroupeArgs::try_parse_from(argv)
        .expect("valid snapshot arguments")
        .into_invocation();
    match invocation {
        TroupeInvocation::Diagnostic(DiagnosticCommand::Snapshot(arguments)) => arguments,
        _ => panic!("expected diagnostic snapshot invocation"),
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

fn local_id(value: &str) -> RunLocalId {
    RunLocalId::parse(value).expect("valid Run-local ID")
}

fn scope(cue_id: Option<&str>, act_id: Option<&str>) -> DiagnosticScope {
    DiagnosticScope::new(
        Some(local_id("scene-1")),
        Some(local_id("actor-1")),
        cue_id.map(local_id),
        None,
        act_id.map(local_id),
        None,
        act_id.map(|_| SchemaU64::new(1)),
    )
}

fn scene_scope() -> DiagnosticScope {
    DiagnosticScope::new(
        Some(local_id("scene-1")),
        None,
        None,
        None,
        None,
        None,
        None,
    )
}

fn header(identity: EventIdentity, scope: DiagnosticScope) -> DiagnosticEventHeader {
    DiagnosticEventHeader::new(
        identity.run_id(),
        identity.sequence(),
        ElapsedNs::new(identity.sequence().get() * 10),
        scope,
        Vec::new(),
    )
    .expect("valid event header")
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

fn started(
    hub: &ProductionDiagnosticHub<AcceptAll>,
    scope: DiagnosticScope,
    detail: SpanStartDetail,
    parent_span_id: Option<u64>,
) -> AcceptedDiagnosticEvent {
    admit(hub, |identity| {
        DiagnosticEvent::SpanStarted(SpanStarted::new(
            header(identity, scope),
            detail,
            parent_span_id.map(SchemaU64::new),
        ))
    })
}

fn message(
    hub: &ProductionDiagnosticHub<AcceptAll>,
    cue: &str,
    act: &str,
) -> AcceptedDiagnosticEvent {
    admit(hub, |identity| {
        DiagnosticEvent::AgentMessageDelta(AgentMessageDelta::new(
            header(identity, scope(Some(cue), Some(act))),
            local_id(&format!("message-{cue}")),
            None,
            format!("output for {cue}"),
        ))
    })
}

fn counter(
    hub: &ProductionDiagnosticHub<AcceptAll>,
    cue: &str,
    act: &str,
    value: u64,
) -> AcceptedDiagnosticEvent {
    admit(hub, |identity| {
        DiagnosticEvent::CounterSampled(CounterSampled::new(
            header(identity, scope(Some(cue), Some(act))),
            CounterKind::AgentTurnActive,
            SchemaU64::new(value),
        ))
    })
}

fn usage(
    hub: &ProductionDiagnosticHub<AcceptAll>,
    cue: &str,
    act: &str,
    input: &str,
) -> AcceptedDiagnosticEvent {
    admit(hub, |identity| {
        DiagnosticEvent::ActTokenUsageFinalized(
            ActTokenUsageFinalized::new(
                header(identity, scope(Some(cue), Some(act))),
                UsageAvailability::Available,
                Some(UsageSource::AcpPromptResponseUsage),
                None,
                Some(TokenCount::parse(LARGE_TOKEN_COUNT).expect("large provider total")),
                Some(TokenCount::parse(input).expect("input tokens")),
                Some(TokenCount::parse("7").expect("output tokens")),
                None,
                None,
                None,
            )
            .expect("valid finalized usage"),
        )
    })
}

fn two_cue_batch(hub: &ProductionDiagnosticHub<AcceptAll>) -> EventBatch {
    EventBatch::new(vec![
        started(
            hub,
            scene_scope(),
            SpanStartDetail::SceneLifecycle(EmptyDetail::new()),
            None,
        ),
        started(
            hub,
            scope(None, None),
            SpanStartDetail::ActorHandleLifetime(ActorDetail::new(
                "Worker".to_owned(),
                "Worker".to_owned(),
            )),
            Some(1),
        ),
        started(
            hub,
            scope(Some("cue-1"), None),
            SpanStartDetail::CueExecution(EmptyDetail::new()),
            Some(2),
        ),
        message(hub, "cue-1", "act-1"),
        counter(hub, "cue-1", "act-1", 1),
        usage(hub, "cue-1", "act-1", "42"),
        started(
            hub,
            scope(Some("cue-2"), None),
            SpanStartDetail::CueExecution(EmptyDetail::new()),
            Some(2),
        ),
        message(hub, "cue-2", "act-2"),
        counter(hub, "cue-2", "act-2", 2),
        usage(hub, "cue-2", "act-2", "84"),
    ])
    .expect("valid two-Cue batch")
}

#[tokio::test]
async fn incomplete_archive_matches_human_and_versioned_json_fixtures() {
    let directory = TestRunDirectory::new("archive-golden");
    let lease = ActiveArchiveLease::acquire(directory.path()).expect("active archive lease");
    let store = create_store(directory.path());
    drop(store);
    drop(lease);

    let human = snapshot::execute(parse_archive_snapshot(directory.path(), None))
        .await
        .expect("incomplete archive snapshot is observable data");
    assert_eq!(human, fixture("snapshot-human.txt"));

    let json = snapshot::execute(parse_archive_snapshot(directory.path(), Some("json")))
        .await
        .expect("archive JSON snapshot");
    assert_eq!(json, fixture("snapshot-v1.json"));
    assert!(json.ends_with('\n'));
    assert!(!json[..json.len() - 1].contains('\n'));
    assert!(json.contains("\"earliest_available_sequence\":null"));
}

#[tokio::test]
async fn live_and_archive_share_one_two_cue_materialized_read_model() {
    let directory = TestRunDirectory::new("live-archive-two-cues");
    let lease =
        Arc::new(ActiveArchiveLease::acquire(directory.path()).expect("active archive lease"));
    let mut writer =
        TransactionalWriter::new(create_store(directory.path()), ()).expect("construct writer");
    let hub = diagnostic_hub();
    writer
        .commit_batch(&two_cue_batch(&hub))
        .expect("commit two-Cue read model");

    let endpoints = QueryEndpoints::active_unobserved(
        run_id(),
        Arc::clone(&lease),
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

    let live = snapshot::query(ResolvedDiagnosticTarget::Live(client))
        .await
        .expect("live diagnostic snapshot")
        .render(DocumentFormat::Json);
    server.shutdown().expect("clean server shutdown");
    drop(endpoints);
    let store = writer.into_store();
    drop(store);
    drop(lease);

    let archive = snapshot::execute(parse_archive_snapshot(directory.path(), Some("json")))
        .await
        .expect("archive snapshot");
    assert_eq!(live, archive);
    for required in [
        "\"span_kind\":\"scene.lifecycle\"",
        "\"span_kind\":\"actor.handle_lifetime\"",
        "\"message_id\":\"message-cue-1\"",
        "\"message_id\":\"message-cue-2\"",
        "\"act_id\":\"act-1\"",
        "\"act_id\":\"act-2\"",
        "\"input_tokens\":\"42\"",
        "\"input_tokens\":\"84\"",
        "\"finalized_acts\":\"2\"",
        "\"known_sum\":\"126\"",
    ] {
        assert!(
            archive.contains(required),
            "missing {required} from {archive}"
        );
    }
    assert_eq!(
        archive.matches("\"span_kind\":\"cue.execution\"").count(),
        2
    );
    assert!(archive.matches("\"cue_id\":\"cue-1\"").count() >= 4);
    assert!(archive.matches("\"cue_id\":\"cue-2\"").count() >= 4);
}

#[test]
fn a_later_active_commit_does_not_pollute_the_captured_snapshot() {
    let directory = TestRunDirectory::new("captured-head");
    let lease = ActiveArchiveLease::acquire(directory.path()).expect("active archive lease");
    let mut writer =
        TransactionalWriter::new(create_store(directory.path()), ()).expect("construct writer");
    let hub = diagnostic_hub();
    writer
        .commit_batch(&EventBatch::new(vec![message(&hub, "cue-1", "act-1")]).unwrap())
        .expect("commit W=1");
    let mut reader = DiagnosticReader::open_active(run_id(), lease.guard()).expect("open reader");
    let captured = reader.capture().expect("capture W=1");
    writer
        .commit_batch(&EventBatch::new(vec![message(&hub, "cue-2", "act-2")]).unwrap())
        .expect("advance active head to W=2");

    let bytes = encode_snapshot_response(run_id(), &captured).expect("encode captured snapshot");
    let json = decode_snapshot_response(&bytes, run_id())
        .expect("decode captured response")
        .render(DocumentFormat::Json);
    assert!(json.contains("\"watermark_sequence\":\"1\""));
    assert!(json.contains("\"message_id\":\"message-cue-1\""));
    assert!(!json.contains("message-cue-2"));
}

#[tokio::test]
async fn failed_and_incomplete_archives_are_readable_documents() {
    for (label, failed) in [("incomplete", false), ("failed", true)] {
        let directory = TestRunDirectory::new(label);
        let lease = ActiveArchiveLease::acquire(directory.path()).expect("active archive lease");
        let store = create_store(directory.path());
        if failed {
            store
                .connection()
                .execute(
                    "UPDATE run_metadata SET ended_at = '2026-08-16T00:00:01Z', \
                     production_outcome = 'failed', clean_shutdown = 1 WHERE singleton = 1",
                    [],
                )
                .expect("mark failed archive");
        }
        drop(store);
        drop(lease);

        let output = snapshot::execute(parse_archive_snapshot(directory.path(), Some("json")))
            .await
            .expect("terminal outcome is data, not a snapshot operation failure");
        assert!(output.contains("\"watermark_sequence\":\"0\""));
    }
}

#[test]
fn schema_identity_decimal_shape_and_size_errors_fail_closed() {
    let valid = fixture("snapshot-v1.json");
    let incompatible = valid.replacen("\"api_schema_version\":1", "\"api_schema_version\":2", 1);
    assert_eq!(
        decode_snapshot_response(incompatible.as_bytes(), run_id())
            .unwrap_err()
            .code(),
        SnapshotErrorCode::IncompatibleResponse
    );

    let wrong_run = valid.replacen(RUN_ID, OTHER_RUN_ID, 1);
    assert_eq!(
        decode_snapshot_response(wrong_run.as_bytes(), run_id())
            .unwrap_err()
            .code(),
        SnapshotErrorCode::RunIdentityMismatch
    );

    let incompatible_model = valid.replacen(
        "\"model_schema_version\":1",
        "\"model_schema_version\":2",
        1,
    );
    assert_eq!(
        decode_snapshot_response(incompatible_model.as_bytes(), run_id())
            .unwrap_err()
            .code(),
        SnapshotErrorCode::IncompatibleResponse
    );

    for invalid in [
        valid.replacen(
            "\"watermark_sequence\":\"0\"",
            "\"watermark_sequence\":\"00\"",
            1,
        ),
        valid.replacen(
            "\"through_sequence\":\"0\"",
            "\"through_sequence\":\"1\"",
            1,
        ),
        valid.replacen("\"spans\":[]", "\"spans\":[],\"unknown\":true", 1),
        valid.replacen("\"run_id\":", "\"run_id\":\"duplicate\",\"run_id\":", 1),
        "{\"api_schema_version\":1".to_owned(),
    ] {
        assert_eq!(
            decode_snapshot_response(invalid.as_bytes(), run_id())
                .unwrap_err()
                .code(),
            SnapshotErrorCode::InvalidResponse,
            "unexpectedly accepted {invalid}"
        );
    }

    let oversized = vec![b' '; 64 * 1024 * 1024 + 1];
    assert_eq!(
        decode_snapshot_response(&oversized, run_id())
            .unwrap_err()
            .code(),
        SnapshotErrorCode::ResponseTooLarge
    );
}

#[tokio::test]
async fn archive_store_and_resolver_failures_are_operation_errors() {
    let missing = TestRunDirectory::new("missing-store");
    let error = snapshot::execute(parse_archive_snapshot(missing.path(), Some("json")))
        .await
        .unwrap_err();
    assert_eq!(error.code(), SnapshotErrorCode::Resolve);

    let corrupt = TestRunDirectory::new("missing-materialized");
    let lease = ActiveArchiveLease::acquire(corrupt.path()).expect("active archive lease");
    let mut writer =
        TransactionalWriter::new(create_store(corrupt.path()), ()).expect("construct writer");
    let hub = diagnostic_hub();
    writer
        .commit_batch(&EventBatch::new(vec![message(&hub, "cue-1", "act-1")]).unwrap())
        .expect("commit W=1 materialized state");
    let store = writer.into_store();
    store
        .connection()
        .execute("DELETE FROM materialized_snapshot", [])
        .expect("remove materialized snapshot");
    drop(store);
    drop(lease);
    let error = snapshot::execute(parse_archive_snapshot(corrupt.path(), Some("json")))
        .await
        .unwrap_err();
    assert_eq!(error.code(), SnapshotErrorCode::Archive);
}
