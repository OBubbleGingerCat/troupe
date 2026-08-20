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
    event::{
        ActTokenUsageFinalized, CounterSampled, DiagnosticEvent, DiagnosticEventHeader,
        DiagnosticScope,
    },
    hub::{
        AcceptedDiagnosticEvent, AdmissionReservation, AdmissionReserver, AdmissionSize,
        DeliveryFailure, EventIdentity, LiveEventNotifier, MandatoryDurableReserver,
        ProductionDiagnosticHub,
    },
    id::CanonicalUuid,
    kinds::{CounterKind, UsageAvailability, UsageSource},
    scalar::{SchemaU64, TokenCount},
    time::ElapsedNs,
};
use troupe_diagnostics_runtime::{
    archive::lease::ActiveArchiveLease,
    query::{events::FiniteEventQuery, reader::DiagnosticReader},
    registry::{model::WebBaseUrl, process_identity::ProcessIdentity},
    server::{
        query::{QueryCoreFailureSignal, QueryEndpoints, encode_events_response},
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
#[path = "../src/application/diagnostic_cli/events_finite.rs"]
mod events_finite;
#[path = "../src/application/diagnostic_cli/http_client.rs"]
mod http_client;
#[path = "../src/application/diagnostic_cli/resolver.rs"]
mod resolver;
#[path = "../src/application/diagnostic_cli/target.rs"]
mod target;
#[path = "../src/application/diagnostic_cli/values.rs"]
mod values;

use args::{DiagnosticCommand, EventStart, EventsFormat, TroupeArgs, TroupeInvocation};
use events_finite::{EventsErrorCode, decode_events_response};
use http_client::DiagnosticHttpClient;
use resolver::ResolvedDiagnosticTarget;
use values::{CanonicalU64, Count};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
const OTHER_RUN_ID: &str = "87654321-4321-4321-8321-cba987654321";
const STARTED_AT: &str = "2026-08-16T00:00:00Z";
const CONFIGURATION_IDENTITY: &str = "configuration-sha256:d03";
const LARGE_TOKEN_COUNT: &str = "1234567890123456789012345678901234567890";

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestRunDirectory(PathBuf);

impl TestRunDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "troupe-d03-events-{label}-{}-{sequence}",
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
    ProcessIdentity::new("test", "d03:4242").expect("valid process identity")
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
    .expect("read events fixture")
}

fn parse_archive_events(
    directory: &Path,
    tail: Option<&str>,
    after: Option<&str>,
    format: Option<&str>,
    follow: bool,
) -> args::EventsArgs {
    let mut argv = vec![
        "troupe".to_owned(),
        "diagnostic".to_owned(),
        "events".to_owned(),
        "--archive".to_owned(),
        directory.display().to_string(),
    ];
    if let Some(tail) = tail {
        argv.extend(["--tail".to_owned(), tail.to_owned()]);
    }
    if let Some(after) = after {
        argv.extend(["--after".to_owned(), after.to_owned()]);
    }
    if let Some(format) = format {
        argv.extend(["--format".to_owned(), format.to_owned()]);
    }
    if follow {
        argv.push("--follow".to_owned());
    }
    let invocation = TroupeArgs::try_parse_from(argv)
        .expect("valid events arguments")
        .into_invocation();
    match invocation {
        TroupeInvocation::Diagnostic(DiagnosticCommand::Events(arguments)) => arguments,
        _ => panic!("expected diagnostic events invocation"),
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

fn act_scope() -> DiagnosticScope {
    use troupe_diagnostics_core::id::RunLocalId;

    let local = |value| RunLocalId::parse(value).expect("valid Run-local ID");
    DiagnosticScope::new(
        Some(local("scene-1")),
        Some(local("actor-1")),
        Some(local("cue-1")),
        None,
        Some(local("act-1")),
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

fn rich_batch(hub: &ProductionDiagnosticHub<AcceptAll>) -> EventBatch {
    EventBatch::new(vec![usage_event(hub), counter_event(hub)]).expect("valid event batch")
}

fn event_at(sequence: u64, run_id: CanonicalUuid) -> DiagnosticEvent {
    DiagnosticEvent::CounterSampled(CounterSampled::new(
        DiagnosticEventHeader::new(
            run_id,
            SchemaU64::new(sequence),
            ElapsedNs::new(sequence),
            DiagnosticScope::new(None, None, None, None, None, None, None),
            Vec::new(),
        )
        .expect("positive event sequence"),
        CounterKind::DiagnosticDroppedEvents,
        SchemaU64::new(sequence),
    ))
}

fn response_bytes(
    api_schema_version: u8,
    response_run_id: CanonicalUuid,
    watermark: u64,
    events: &[DiagnosticEvent],
    next_after: Option<u64>,
) -> Vec<u8> {
    let events = events
        .iter()
        .map(|event| serde_json::to_string(event).expect("serialize event"))
        .collect::<Vec<_>>()
        .join(",");
    let next_after = next_after
        .map(|value| format!("\"{value}\""))
        .unwrap_or_else(|| "null".to_owned());
    format!(
        "{{\"api_schema_version\":{api_schema_version},\"run_id\":\"{response_run_id}\",\"captured_watermark\":\"{watermark}\",\"events\":[{events}],\"next_after\":{next_after}}}"
    )
    .into_bytes()
}

#[tokio::test]
async fn archive_default_tail_matches_human_and_canonical_jsonl_fixtures() {
    let directory = TestRunDirectory::new("archive-golden");
    let lease = ActiveArchiveLease::acquire(directory.path()).expect("active archive lease");
    let mut writer =
        TransactionalWriter::new(create_store(directory.path()), ()).expect("construct writer");
    let hub = diagnostic_hub();
    writer
        .commit_batch(&rich_batch(&hub))
        .expect("commit fixture events");
    drop(writer.into_store());
    drop(lease);

    let default = parse_archive_events(directory.path(), None, None, None, false);
    let (_, start, follow, format) = default.clone().into_parts();
    assert!(matches!(start, EventStart::Tail(count) if count.get() == 100));
    assert!(!follow);
    assert_eq!(format, EventsFormat::Human);
    let human = events_finite::execute(default)
        .await
        .expect("incomplete archive events are observable data");
    assert_eq!(human, fixture("events-human.txt"));

    let jsonl = events_finite::execute(parse_archive_events(
        directory.path(),
        None,
        None,
        Some("jsonl"),
        false,
    ))
    .await
    .expect("archive JSONL events");
    assert_eq!(jsonl, fixture("events-v1.jsonl"));
    assert!(jsonl.ends_with('\n'));
    assert_eq!(jsonl.lines().count(), 2);
    for line in jsonl.lines() {
        assert!(!line.is_empty());
        let event: DiagnosticEvent = serde_json::from_str(line).expect("canonical event line");
        assert_eq!(serde_json::to_string(&event).unwrap(), line);
    }
}

#[tokio::test]
async fn live_and_archive_share_tail_after_zero_and_explicit_query_semantics() {
    let directory = TestRunDirectory::new("live-archive");
    let lease =
        Arc::new(ActiveArchiveLease::acquire(directory.path()).expect("active archive lease"));
    let mut writer =
        TransactionalWriter::new(create_store(directory.path()), ()).expect("construct writer");
    let hub = diagnostic_hub();
    writer
        .commit_batch(&rich_batch(&hub))
        .expect("commit fixture events");
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

    let starts = [
        EventStart::Tail(Count::new(0)),
        EventStart::Tail(Count::new(1)),
        EventStart::After(CanonicalU64::new(0)),
        EventStart::After(CanonicalU64::new(1)),
        EventStart::After(CanonicalU64::new(u64::MAX)),
    ];
    let mut live = Vec::new();
    for start in starts {
        live.push(
            events_finite::query(ResolvedDiagnosticTarget::Live(client.clone()), start)
                .await
                .expect("live finite events")
                .render(EventsFormat::Jsonl),
        );
    }
    server.shutdown().expect("clean server shutdown");
    drop(endpoints);
    drop(writer.into_store());
    drop(lease);

    let archive_arguments = [
        (Some("0"), None),
        (Some("1"), None),
        (None, Some("0")),
        (None, Some("1")),
        (None, Some("18446744073709551615")),
    ];
    for (index, (tail, after)) in archive_arguments.into_iter().enumerate() {
        let archive = events_finite::execute(parse_archive_events(
            directory.path(),
            tail,
            after,
            Some("jsonl"),
            false,
        ))
        .await
        .expect("archive finite events");
        assert_eq!(live[index], archive);
    }
    assert!(live[0].is_empty());
    assert_eq!(live[1].lines().count(), 1);
    assert_eq!(live[2], fixture("events-v1.jsonl"));
    assert_eq!(live[3], live[1]);
    assert!(live[4].is_empty());
}

#[test]
fn one_captured_head_excludes_later_commits_and_preserves_canonical_bytes() {
    let directory = TestRunDirectory::new("captured-head");
    let lease = ActiveArchiveLease::acquire(directory.path()).expect("active archive lease");
    let mut writer =
        TransactionalWriter::new(create_store(directory.path()), ()).expect("construct writer");
    let hub = diagnostic_hub();
    writer.commit_batch(&rich_batch(&hub)).expect("commit W=2");
    let mut reader = DiagnosticReader::open_active(run_id(), lease.guard()).expect("open reader");
    let captured = reader.capture().expect("capture W=2");
    let later = counter_event(&hub);
    writer
        .commit_batch(&EventBatch::new(vec![later.clone()]).unwrap())
        .expect("advance active head to W=3");

    let bytes = encode_events_response(
        run_id(),
        &captured,
        FiniteEventQuery::after(SchemaU64::new(0)),
    )
    .expect("encode captured events");
    let jsonl = decode_events_response(&bytes, run_id(), EventStart::After(CanonicalU64::new(0)))
        .expect("decode captured events")
        .render(EventsFormat::Jsonl);
    assert_eq!(jsonl, fixture("events-v1.jsonl"));
    assert!(
        !jsonl
            .as_bytes()
            .windows(later.canonical_bytes().len())
            .any(|window| { window == later.canonical_bytes() })
    );
}

#[test]
fn full_u64_cursor_domain_is_total_without_overflow() {
    let maximum = event_at(u64::MAX, run_id());
    let with_maximum = response_bytes(1, run_id(), u64::MAX, &[maximum], None);
    let line = decode_events_response(&with_maximum, run_id(), EventStart::Tail(Count::new(1)))
        .expect("tail one at u64::MAX")
        .render(EventsFormat::Jsonl);
    assert!(line.contains("\"sequence\":\"18446744073709551615\""));
    assert!(line.ends_with('\n'));

    let empty = response_bytes(1, run_id(), u64::MAX, &[], None);
    for start in [
        EventStart::Tail(Count::new(0)),
        EventStart::After(CanonicalU64::new(u64::MAX)),
    ] {
        assert!(
            decode_events_response(&empty, run_id(), start)
                .expect("empty maximum cursor query")
                .render(EventsFormat::Jsonl)
                .is_empty()
        );
    }
}

#[tokio::test]
async fn failed_and_incomplete_archives_remain_readable_and_follow_is_not_finite() {
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

        let output = events_finite::execute(parse_archive_events(
            directory.path(),
            None,
            Some("0"),
            Some("jsonl"),
            false,
        ))
        .await
        .expect("terminal outcome is data, not an events operation failure");
        assert!(output.is_empty());
    }

    let invocation = TroupeArgs::try_parse_from([
        "troupe",
        "diagnostic",
        "events",
        "--url",
        "http://127.0.0.1:9/",
        "--follow",
    ])
    .expect("live follow arguments are syntactically valid")
    .into_invocation();
    let arguments = match invocation {
        TroupeInvocation::Diagnostic(DiagnosticCommand::Events(arguments)) => arguments,
        _ => panic!("expected diagnostic events invocation"),
    };
    let error = events_finite::execute(arguments).await.unwrap_err();
    assert_eq!(error.code(), EventsErrorCode::FollowUnsupported);
}

#[test]
fn identity_schema_cursor_order_duplicate_shape_and_size_errors_fail_closed() {
    let events = [event_at(1, run_id()), event_at(2, run_id())];
    let valid = response_bytes(1, run_id(), 2, &events, None);
    let start = EventStart::After(CanonicalU64::new(0));

    let incompatible = response_bytes(2, run_id(), 2, &events, None);
    assert_eq!(
        decode_events_response(&incompatible, run_id(), start)
            .unwrap_err()
            .code(),
        EventsErrorCode::IncompatibleResponse
    );

    let wrong_run = response_bytes(1, other_run_id(), 2, &events, None);
    assert_eq!(
        decode_events_response(&wrong_run, run_id(), start)
            .unwrap_err()
            .code(),
        EventsErrorCode::RunIdentityMismatch
    );
    let wrong_event = response_bytes(1, run_id(), 1, &[event_at(1, other_run_id())], None);
    assert_eq!(
        decode_events_response(&wrong_event, run_id(), start)
            .unwrap_err()
            .code(),
        EventsErrorCode::RunIdentityMismatch
    );

    for invalid in [
        String::from_utf8(valid.clone()).unwrap().replacen(
            "\"captured_watermark\":\"2\"",
            "\"captured_watermark\":\"02\"",
            1,
        ),
        String::from_utf8(response_bytes(
            1,
            run_id(),
            2,
            &[event_at(2, run_id()), event_at(1, run_id())],
            None,
        ))
        .unwrap(),
        String::from_utf8(response_bytes(
            1,
            run_id(),
            2,
            &[event_at(1, run_id()), event_at(1, run_id())],
            None,
        ))
        .unwrap(),
        String::from_utf8(response_bytes(
            1,
            run_id(),
            2,
            &[event_at(1, run_id())],
            None,
        ))
        .unwrap(),
        String::from_utf8(response_bytes(1, run_id(), 2, &events, Some(2))).unwrap(),
        String::from_utf8(valid.clone()).unwrap().replacen(
            "\"next_after\":null",
            "\"next_after\":null,\"unknown\":true",
            1,
        ),
        String::from_utf8(valid.clone()).unwrap().replacen(
            "\"run_id\":",
            "\"run_id\":\"duplicate\",\"run_id\":",
            1,
        ),
        "{\"api_schema_version\":1".to_owned(),
    ] {
        assert_eq!(
            decode_events_response(invalid.as_bytes(), run_id(), start)
                .unwrap_err()
                .code(),
            EventsErrorCode::InvalidResponse,
            "unexpectedly accepted {invalid}"
        );
    }

    let oversized = vec![b' '; 64 * 1024 * 1024 + 1];
    assert_eq!(
        decode_events_response(&oversized, run_id(), start)
            .unwrap_err()
            .code(),
        EventsErrorCode::ResponseTooLarge
    );
}

#[tokio::test]
async fn archive_resolver_and_corrupt_store_fail_as_typed_operations() {
    let missing = TestRunDirectory::new("missing-store");
    let error = events_finite::execute(parse_archive_events(
        missing.path(),
        None,
        None,
        Some("jsonl"),
        false,
    ))
    .await
    .unwrap_err();
    assert_eq!(error.code(), EventsErrorCode::Resolve);

    let corrupt = TestRunDirectory::new("corrupt-event");
    let lease = ActiveArchiveLease::acquire(corrupt.path()).expect("active archive lease");
    let mut writer =
        TransactionalWriter::new(create_store(corrupt.path()), ()).expect("construct writer");
    let hub = diagnostic_hub();
    writer
        .commit_batch(&rich_batch(&hub))
        .expect("commit fixture events");
    let store = writer.into_store();
    store
        .connection()
        .execute_batch("DROP TRIGGER events_no_update")
        .expect("remove update guard for fault injection");
    store
        .connection()
        .execute(
            "UPDATE events SET canonical_json = CAST('{}' AS BLOB) WHERE sequence = '1'",
            [],
        )
        .expect("corrupt canonical event");
    drop(store);
    drop(lease);

    let error = events_finite::execute(parse_archive_events(
        corrupt.path(),
        None,
        Some("0"),
        Some("jsonl"),
        false,
    ))
    .await
    .unwrap_err();
    assert_eq!(error.code(), EventsErrorCode::Resolve);
}
