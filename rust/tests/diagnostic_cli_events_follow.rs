use std::{
    collections::VecDeque,
    convert::Infallible,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use clap::Parser;
use clap::error::ErrorKind;
use tokio_util::sync::CancellationToken;
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
    archive::lease::ActiveArchiveLease,
    registry::process_identity::ProcessIdentity,
    server::{
        query::{EVENTS_PATH, QueryEndpoints},
        routes::{RouteDefinition, RouteResponse},
        runtime::{DiagnosticServer, ServerConfig},
        sse::{
            cursor::{CursorSource, resolve_effective_cursor},
            frame::{
                CURSOR_UNAVAILABLE_REASON, CommittedEvent, PRODUCTION_FINISHED_REASON, SseFrame,
                sse_response_headers,
            },
            replay::requests_event_stream,
        },
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
#[path = "../src/application/diagnostic_cli/events_follow.rs"]
mod events_follow;
#[path = "../src/application/diagnostic_cli/http_client.rs"]
mod http_client;
#[path = "../src/application/diagnostic_cli/resolver.rs"]
mod resolver;
#[path = "../src/application/diagnostic_cli/target.rs"]
mod target;
#[path = "../src/application/diagnostic_cli/values.rs"]
mod values;

use args::{DiagnosticCommand, TroupeArgs, TroupeInvocation};
use events_follow::{FollowErrorCode, FollowOutput, FollowTermination};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
const OTHER_RUN_ID: &str = "87654321-4321-4321-8321-cba987654321";
const STARTED_AT: &str = "2026-08-16T00:00:00Z";
const CONFIGURATION_IDENTITY: &str = "configuration-sha256:d10";

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestRunDirectory(PathBuf);

impl TestRunDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "troupe-d10-events-{label}-{}-{sequence}",
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

fn run_id() -> CanonicalUuid {
    CanonicalUuid::parse(RUN_ID).expect("canonical Run UUID")
}

fn other_run_id() -> CanonicalUuid {
    CanonicalUuid::parse(OTHER_RUN_ID).expect("canonical other Run UUID")
}

fn event_at(sequence: u64, run_id: CanonicalUuid) -> DiagnosticEvent {
    DiagnosticEvent::CounterSampled(CounterSampled::new(
        DiagnosticEventHeader::new(
            run_id,
            SchemaU64::new(sequence),
            ElapsedNs::new(sequence * 10),
            DiagnosticScope::new(None, None, None, None, None, None, None),
            Vec::new(),
        )
        .expect("positive event sequence"),
        CounterKind::DiagnosticDroppedEvents,
        SchemaU64::new(sequence),
    ))
}

fn committed_event(sequence: u64, run_id: CanonicalUuid) -> CommittedEvent {
    let event = event_at(sequence, run_id);
    let canonical = serde_json::to_vec(&event).expect("canonical event JSON");
    CommittedEvent::try_new(event, canonical).expect("committed event")
}

fn accepted_event(hub: &ProductionDiagnosticHub<AcceptAll>, value: u64) -> AcceptedDiagnosticEvent {
    hub.admit(
        |identity: EventIdentity| {
            DiagnosticEvent::CounterSampled(CounterSampled::new(
                DiagnosticEventHeader::new(
                    identity.run_id(),
                    identity.sequence(),
                    ElapsedNs::new(identity.sequence().get() * 10),
                    DiagnosticScope::new(None, None, None, None, None, None, None),
                    Vec::new(),
                )
                .expect("canonical event header"),
                CounterKind::DiagnosticDroppedEvents,
                SchemaU64::new(value),
            ))
        },
        None,
    )
    .expect("admit initial event")
    .accepted()
    .clone()
}

fn event_frame(sequence: u64) -> SseFrame {
    SseFrame::diagnostic_event(&committed_event(sequence, run_id()))
}

fn frames_bytes(frames: impl IntoIterator<Item = SseFrame>) -> Vec<u8> {
    let mut bytes = Vec::new();
    for frame in frames {
        bytes.extend_from_slice(frame.bytes());
    }
    bytes
}

#[derive(Clone)]
struct ScriptedResponse {
    bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CursorObservation {
    value: u64,
    source: CursorSource,
}

struct TestServer {
    _server: DiagnosticServer,
    _directory: TestRunDirectory,
    base_url: String,
    observations: Arc<Mutex<Vec<CursorObservation>>>,
}

impl TestServer {
    fn start(label: &str, initial_events: u64, scripts: Vec<ScriptedResponse>) -> Self {
        let directory = TestRunDirectory::new(label);
        let lease =
            Arc::new(ActiveArchiveLease::acquire(directory.path()).expect("active archive lease"));
        let store = DiagnosticStore::create(
            directory.path(),
            &InitialStoreMetadata::new(run_id(), STARTED_AT, CONFIGURATION_IDENTITY),
        )
        .expect("create diagnostic store");
        let mut writer = TransactionalWriter::new(store, ()).expect("construct store writer");
        let hub = ProductionDiagnosticHub::production(run_id(), AcceptAll, Box::new(IgnoreLive));
        let events = (1..=initial_events)
            .map(|value| accepted_event(&hub, value))
            .collect::<Vec<_>>();
        if !events.is_empty() {
            writer
                .commit_batch(&EventBatch::new(events).expect("nonempty initial batch"))
                .expect("commit initial events");
        }
        drop(writer.into_store());

        let query = QueryEndpoints::active_unobserved(run_id(), Arc::clone(&lease), |_failure| {});
        let scripts = Arc::new(Mutex::new(VecDeque::from(scripts)));
        let observations = Arc::new(Mutex::new(Vec::new()));
        let route_scripts = Arc::clone(&scripts);
        let route_observations = Arc::clone(&observations);
        let route = RouteDefinition::get(EVENTS_PATH, move |request| {
            let query = query.clone();
            let scripts = Arc::clone(&route_scripts);
            let observations = Arc::clone(&route_observations);
            async move {
                if !requests_event_stream(&request) {
                    return Ok(query.handle_finite_events(request));
                }
                let cursor = resolve_effective_cursor(request.uri().query(), request.headers())
                    .expect("D10 sends a valid H02 cursor");
                observations.lock().unwrap().push(CursorObservation {
                    value: cursor.value().get(),
                    source: cursor.source(),
                });
                let script =
                    scripts
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or_else(|| ScriptedResponse {
                            bytes: frames_bytes([
                                SseFrame::stream_ready(run_id(), cursor.value(), cursor.value())
                                    .unwrap(),
                                SseFrame::stream_closed(
                                    run_id(),
                                    PRODUCTION_FINISHED_REASON,
                                    cursor.value(),
                                )
                                .unwrap(),
                            ]),
                        });
                Ok(sse_bytes_response(script.bytes))
            }
        })
        .expect("combined finite/SSE route");
        let server = DiagnosticServer::start(
            ServerConfig::new(
                run_id(),
                std::process::id(),
                ProcessIdentity::new("test", &format!("d10:{label}")).unwrap(),
            )
            .with_bind("127.0.0.1", 0),
            vec![route],
        )
        .expect("start diagnostic server");
        let base_url = format!("http://{}/", server.connect_addr());
        Self {
            _server: server,
            _directory: directory,
            base_url,
            observations,
        }
    }

    fn observations(&self) -> Vec<CursorObservation> {
        self.observations.lock().unwrap().clone()
    }
}

fn sse_bytes_response(bytes: Vec<u8>) -> RouteResponse {
    let mut response = RouteResponse::bytes("200".parse().expect("HTTP 200"), bytes);
    let headers = sse_response_headers();
    for name in headers.keys() {
        response = response.with_header(name.clone(), headers.get(name).unwrap().clone());
    }
    response
}

fn parse_url_events(
    base_url: &str,
    tail: Option<&str>,
    after: Option<&str>,
    format: &str,
) -> args::EventsArgs {
    let mut argv = vec![
        "troupe".to_owned(),
        "diagnostic".to_owned(),
        "events".to_owned(),
        "--url".to_owned(),
        base_url.to_owned(),
        "--follow".to_owned(),
        "--format".to_owned(),
        format.to_owned(),
    ];
    if let Some(tail) = tail {
        argv.extend(["--tail".to_owned(), tail.to_owned()]);
    }
    if let Some(after) = after {
        argv.extend(["--after".to_owned(), after.to_owned()]);
    }
    match TroupeArgs::try_parse_from(argv)
        .expect("valid follow arguments")
        .into_invocation()
    {
        TroupeInvocation::Diagnostic(DiagnosticCommand::Events(arguments)) => arguments,
        _ => panic!("expected diagnostic events invocation"),
    }
}

#[derive(Default)]
struct CapturedOutput {
    stdout: String,
    stderr: Vec<String>,
}

impl FollowOutput for CapturedOutput {
    type Error = Infallible;

    fn write_stdout_record(&mut self, record: &str) -> Result<(), Self::Error> {
        self.stdout.push_str(record);
        Ok(())
    }

    fn write_stderr_line(&mut self, line: &str) -> Result<(), Self::Error> {
        self.stderr.push(line.to_owned());
        Ok(())
    }
}

struct CancellingOutput {
    captured: CapturedOutput,
    cancellation: CancellationToken,
    records: usize,
}

impl FollowOutput for CancellingOutput {
    type Error = Infallible;

    fn write_stdout_record(&mut self, record: &str) -> Result<(), Self::Error> {
        self.captured.write_stdout_record(record)?;
        self.records += 1;
        if self.records == 1 {
            self.cancellation.cancel();
        }
        Ok(())
    }

    fn write_stderr_line(&mut self, line: &str) -> Result<(), Self::Error> {
        self.captured.write_stderr_line(line)
    }
}

fn jsonl_sequences(output: &str) -> Vec<u64> {
    output
        .lines()
        .map(|line| {
            let event: DiagnosticEvent = serde_json::from_str(line).expect("valid JSONL event");
            assert_eq!(serde_json::to_string(&event).unwrap(), line);
            event.header().sequence().get()
        })
        .collect()
}

async fn run_follow(
    arguments: args::EventsArgs,
    output: &mut impl FollowOutput,
    cancellation: CancellationToken,
) -> Result<FollowTermination, events_follow::FollowError> {
    tokio::time::timeout(
        Duration::from_secs(5),
        events_follow::execute(arguments, output, cancellation),
    )
    .await
    .expect("follow execution did not hang")
}

#[tokio::test]
async fn finite_prefix_temporary_disconnect_reconnect_and_dedupe_are_seamless() {
    let first = ScriptedResponse {
        bytes: frames_bytes([
            SseFrame::stream_ready(run_id(), SchemaU64::new(2), SchemaU64::new(2)).unwrap(),
            event_frame(3),
        ]),
    };
    let second = ScriptedResponse {
        bytes: frames_bytes([
            SseFrame::stream_ready(run_id(), SchemaU64::new(3), SchemaU64::new(4)).unwrap(),
            SseFrame::heartbeat(run_id(), SchemaU64::new(4)).unwrap(),
            event_frame(3),
            event_frame(4),
            SseFrame::stream_closed(run_id(), PRODUCTION_FINISHED_REASON, SchemaU64::new(4))
                .unwrap(),
        ]),
    };
    let server = TestServer::start("reconnect", 2, vec![first, second]);
    let mut output = CapturedOutput::default();

    let termination = run_follow(
        parse_url_events(&server.base_url, None, None, "jsonl"),
        &mut output,
        CancellationToken::new(),
    )
    .await
    .expect("recoverable stream");

    assert_eq!(termination, FollowTermination::StreamClosed);
    assert_eq!(termination.exit_code(), 0);
    assert_eq!(jsonl_sequences(&output.stdout), [1, 2, 3, 4]);
    assert!(!output.stdout.contains("stream_ready"));
    assert!(!output.stdout.contains("heartbeat"));
    assert!(!output.stdout.contains("stream_closed"));
    assert_eq!(output.stderr.len(), 1);
    assert!(output.stderr[0].contains("stream ended without stream_closed"));
    assert_eq!(
        server.observations(),
        [
            CursorObservation {
                value: 2,
                source: CursorSource::QueryAfter,
            },
            CursorObservation {
                value: 3,
                source: CursorSource::LastEventId,
            },
        ]
    );
}

#[tokio::test]
async fn tail_zero_empty_finite_at_nonzero_head_adopts_connection_head() {
    let stream = ScriptedResponse {
        bytes: frames_bytes([
            SseFrame::stream_ready(run_id(), SchemaU64::new(2), SchemaU64::new(3)).unwrap(),
            event_frame(3),
            event_frame(4),
            SseFrame::stream_closed(run_id(), PRODUCTION_FINISHED_REASON, SchemaU64::new(4))
                .unwrap(),
        ]),
    };
    let server = TestServer::start("tail-zero", 2, vec![stream]);
    let mut output = CapturedOutput::default();

    let termination = run_follow(
        parse_url_events(&server.base_url, Some("0"), None, "jsonl"),
        &mut output,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(termination, FollowTermination::StreamClosed);
    assert_eq!(jsonl_sequences(&output.stdout), [4]);
    assert_eq!(
        server.observations(),
        [CursorObservation {
            value: 2,
            source: CursorSource::QueryAfter,
        }]
    );
}

#[tokio::test]
async fn delivery_gap_reconnects_from_last_output_and_replays_missing_range() {
    let gap = ScriptedResponse {
        bytes: frames_bytes([
            SseFrame::stream_ready(run_id(), SchemaU64::new(2), SchemaU64::new(5)).unwrap(),
            SseFrame::delivery_gap(
                run_id(),
                "subscriber_buffer_overflow",
                SchemaU64::new(2),
                SchemaU64::new(5),
            )
            .unwrap(),
        ]),
    };
    let recovery = ScriptedResponse {
        bytes: frames_bytes([
            SseFrame::stream_ready(run_id(), SchemaU64::new(2), SchemaU64::new(5)).unwrap(),
            event_frame(3),
            event_frame(4),
            event_frame(5),
            SseFrame::stream_closed(run_id(), PRODUCTION_FINISHED_REASON, SchemaU64::new(5))
                .unwrap(),
        ]),
    };
    let server = TestServer::start("gap", 2, vec![gap, recovery]);
    let mut output = CapturedOutput::default();

    let termination = run_follow(
        parse_url_events(&server.base_url, None, None, "jsonl"),
        &mut output,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(termination, FollowTermination::StreamClosed);
    assert_eq!(jsonl_sequences(&output.stdout), [1, 2, 3, 4, 5]);
    assert!(
        output
            .stderr
            .iter()
            .any(|line| line.contains("delivery_gap"))
    );
    assert_eq!(
        server.observations(),
        [
            CursorObservation {
                value: 2,
                source: CursorSource::QueryAfter,
            },
            CursorObservation {
                value: 2,
                source: CursorSource::LastEventId,
            },
        ]
    );
}

#[tokio::test]
async fn resync_and_stream_identity_change_fail_closed() {
    let resync = TestServer::start(
        "resync",
        2,
        vec![
            ScriptedResponse {
                bytes: frames_bytes([
                    SseFrame::stream_ready(run_id(), SchemaU64::new(2), SchemaU64::new(5)).unwrap(),
                    SseFrame::delivery_gap(
                        run_id(),
                        "subscriber_buffer_overflow",
                        SchemaU64::new(2),
                        SchemaU64::new(5),
                    )
                    .unwrap(),
                ]),
            },
            ScriptedResponse {
                bytes: frames_bytes([SseFrame::resync_required(
                    run_id(),
                    CURSOR_UNAVAILABLE_REASON,
                    SchemaU64::new(5),
                    Some(SchemaU64::new(3)),
                )
                .unwrap()]),
            },
        ],
    );
    let mut output = CapturedOutput::default();
    let error = run_follow(
        parse_url_events(&resync.base_url, None, None, "jsonl"),
        &mut output,
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code(), FollowErrorCode::ResyncRequired);
    assert_eq!(jsonl_sequences(&output.stdout), [1, 2]);
    assert_eq!(
        resync.observations(),
        [
            CursorObservation {
                value: 2,
                source: CursorSource::QueryAfter,
            },
            CursorObservation {
                value: 2,
                source: CursorSource::LastEventId,
            },
        ]
    );

    let identity = TestServer::start(
        "identity",
        2,
        vec![ScriptedResponse {
            bytes: frames_bytes([SseFrame::stream_ready(
                other_run_id(),
                SchemaU64::new(2),
                SchemaU64::new(2),
            )
            .unwrap()]),
        }],
    );
    let mut output = CapturedOutput::default();
    let error = run_follow(
        parse_url_events(&identity.base_url, None, None, "jsonl"),
        &mut output,
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code(), FollowErrorCode::RunIdentityMismatch);
    assert_eq!(jsonl_sequences(&output.stdout), [1, 2]);
}

#[tokio::test]
async fn cancellation_is_exit_130_and_leaves_a_valid_jsonl_prefix() {
    let server = TestServer::start("interrupt", 2, Vec::new());
    let cancellation = CancellationToken::new();
    let mut output = CancellingOutput {
        captured: CapturedOutput::default(),
        cancellation: cancellation.clone(),
        records: 0,
    };

    let termination = run_follow(
        parse_url_events(&server.base_url, None, None, "jsonl"),
        &mut output,
        cancellation,
    )
    .await
    .unwrap();

    assert_eq!(termination, FollowTermination::Interrupted);
    assert_eq!(termination.exit_code(), 130);
    assert_eq!(jsonl_sequences(&output.captured.stdout), [1]);
    assert!(output.captured.stdout.ends_with('\n'));
    assert!(server.observations().is_empty());
}

#[tokio::test]
async fn human_records_are_complete_and_strictly_increasing() {
    let server = TestServer::start(
        "human",
        2,
        vec![ScriptedResponse {
            bytes: frames_bytes([
                SseFrame::stream_ready(run_id(), SchemaU64::new(2), SchemaU64::new(3)).unwrap(),
                event_frame(3),
                SseFrame::stream_closed(run_id(), PRODUCTION_FINISHED_REASON, SchemaU64::new(3))
                    .unwrap(),
            ]),
        }],
    );
    let mut output = CapturedOutput::default();

    let termination = run_follow(
        parse_url_events(&server.base_url, None, None, "human"),
        &mut output,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(termination, FollowTermination::StreamClosed);
    let sequences = serde_json::Deserializer::from_str(&output.stdout)
        .into_iter::<DiagnosticEvent>()
        .map(|event| event.unwrap().header().sequence().get())
        .collect::<Vec<_>>();
    assert_eq!(sequences, [1, 2, 3]);
}

#[test]
fn archive_follow_is_rejected_by_the_shared_cli_grammar() {
    let error = TroupeArgs::try_parse_from([
        "troupe",
        "diagnostic",
        "events",
        "--archive",
        "/tmp/archive",
        "--follow",
    ])
    .unwrap_err();
    assert_eq!(error.exit_code(), 2);
    assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
}
