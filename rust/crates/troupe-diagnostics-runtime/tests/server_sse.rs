use std::{
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

use http_body_util::BodyExt as _;
use hyper::{HeaderMap, StatusCode, header::HeaderValue};
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
    archive::{layout::ArchiveLayout, lease::ActiveArchiveLease},
    query::reader::ReaderProfile,
    registry::process_identity::ProcessIdentity,
    server::{
        routes::RouteDefinition,
        runtime::{DiagnosticServer, ServerConfig},
        sse::{
            cursor::{CursorErrorKind, CursorSource, EffectiveCursor, resolve_effective_cursor},
            frame::{
                CURSOR_INCONSISTENT_REASON, CommittedEvent, PRODUCTION_FINISHED_REASON, SseFrame,
                SseFrameKind, sse_response_headers, sse_route_response,
            },
            replay::{
                ActiveReplaySource, ReplayCoordinator, ReplayDriverConfig, ReplayErrorKind,
                ReplayPage, ReplayPhase, ReplayRange, ReplayStart, ReplayWindow, SseEndpoint,
                accepts_event_stream,
            },
            subscriber::{
                CommitSignal, CommitSignalErrorKind, CommitTailStatus, DeliveryStatus, SseBody,
                SubscriberLimits, open_subscriber, resync_body,
            },
        },
    },
    store::{
        batch::EventBatch,
        connection::{DiagnosticStore, InitialStoreMetadata},
        writer::TransactionalWriter,
    },
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestRoot(PathBuf);

impl TestRoot {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "troupe-h02-server-sse-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create test Production root");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Copy)]
struct AcceptAll;

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

struct IgnoreLive;

impl LiveEventNotifier for IgnoreLive {
    fn notify(&mut self, _event: AcceptedDiagnosticEvent) -> Result<(), DeliveryFailure> {
        Ok(())
    }
}

fn run_id() -> CanonicalUuid {
    CanonicalUuid::parse(RUN_ID).expect("canonical test Run UUID")
}

fn cursor(value: u64) -> EffectiveCursor {
    EffectiveCursor::new(SchemaU64::new(value), CursorSource::QueryAfter)
}

fn event(sequence: u64) -> CommittedEvent {
    let header = DiagnosticEventHeader::new(
        run_id(),
        SchemaU64::new(sequence),
        ElapsedNs::new(sequence * 10),
        DiagnosticScope::new(None, None, None, None, None, None, None),
        Vec::new(),
    )
    .expect("valid event header");
    let event = DiagnosticEvent::CounterSampled(CounterSampled::new(
        header,
        CounterKind::AgentTurnActive,
        SchemaU64::new(sequence),
    ));
    let canonical = serde_json::to_vec(&event).expect("canonical event JSON");
    CommittedEvent::try_new(event, canonical).expect("validated canonical event")
}

fn accepted_event(
    hub: &ProductionDiagnosticHub<AcceptAll>,
    elapsed_ns: u64,
) -> AcceptedDiagnosticEvent {
    hub.admit(
        |identity: EventIdentity| {
            let header = DiagnosticEventHeader::new(
                identity.run_id(),
                identity.sequence(),
                ElapsedNs::new(elapsed_ns),
                DiagnosticScope::new(None, None, None, None, None, None, None),
                Vec::new(),
            )
            .expect("valid accepted event header");
            DiagnosticEvent::CounterSampled(CounterSampled::new(
                header,
                CounterKind::AgentTurnActive,
                identity.sequence(),
            ))
        },
        None,
    )
    .expect("admit event")
    .accepted()
    .clone()
}

fn limits(events: usize) -> SubscriberLimits {
    SubscriberLimits::new(events, 64 * 1024).expect("valid subscriber limits")
}

fn active_window(head: u64) -> ReplayWindow {
    ReplayWindow::new(
        ReaderProfile::Active,
        run_id(),
        SchemaU64::new(head),
        (head != 0).then_some(SchemaU64::new(1)),
    )
    .expect("valid active replay window")
}

fn page(after: u64, through: u64, events: Vec<CommittedEvent>) -> ReplayPage {
    ReplayPage::new(
        run_id(),
        ReplayRange::new(SchemaU64::new(after), SchemaU64::new(through))
            .expect("valid replay range"),
        events,
    )
    .expect("valid replay page")
}

async fn next_frame(body: &mut SseBody) -> Vec<u8> {
    let frame = tokio::time::timeout(Duration::from_secs(1), body.frame())
        .await
        .expect("SSE frame arrived promptly")
        .expect("SSE body did not end")
        .expect("infallible SSE body");
    frame
        .into_data()
        .expect("SSE bodies emit data frames")
        .to_vec()
}

async fn assert_body_closed(body: &mut SseBody) {
    assert!(
        tokio::time::timeout(Duration::from_secs(1), body.frame())
            .await
            .expect("SSE body closure arrived promptly")
            .is_none()
    );
}

fn contains_line(frame: &[u8], line: &str) -> bool {
    std::str::from_utf8(frame)
        .expect("SSE is UTF-8")
        .lines()
        .any(|actual| actual == line)
}

fn raw_request(server: &DiagnosticServer, path: &str, accept: &str) -> Vec<u8> {
    let mut stream = TcpStream::connect(server.connect_addr()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: diagnostics.test\r\nAccept: {accept}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    stream.flush().unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    response
}

fn response_status_and_json(response: &[u8]) -> (u16, serde_json::Value) {
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let status = std::str::from_utf8(&response[..header_end])
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .split_ascii_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let body = serde_json::from_slice(&response[header_end + 4..]).unwrap();
    (status, body)
}

#[test]
fn cursor_is_mandatory_canonical_and_last_event_id_takes_precedence() {
    let headers = HeaderMap::new();
    let missing = resolve_effective_cursor(None, &headers).unwrap_err();
    assert_eq!(missing.kind(), CursorErrorKind::Missing);
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);

    for invalid in ["after=01", "after=-1", "after=18446744073709551616"] {
        assert_eq!(
            resolve_effective_cursor(Some(invalid), &headers)
                .unwrap_err()
                .kind(),
            CursorErrorKind::InvalidQuery
        );
    }

    let mut reconnect = HeaderMap::new();
    reconnect.insert("last-event-id", HeaderValue::from_static("7"));
    let effective = resolve_effective_cursor(Some("after=2"), &reconnect).unwrap();
    assert_eq!(effective.value(), SchemaU64::new(7));
    assert_eq!(effective.source(), CursorSource::LastEventId);
    assert_eq!(
        resolve_effective_cursor(Some("after=not-canonical"), &reconnect)
            .unwrap()
            .value(),
        SchemaU64::new(7)
    );
    assert_eq!(
        resolve_effective_cursor(Some("unknown=2"), &reconnect)
            .unwrap_err()
            .kind(),
        CursorErrorKind::InvalidQuery
    );

    let mut empty_reconnect = HeaderMap::new();
    empty_reconnect.insert("last-event-id", HeaderValue::from_static(""));
    assert_eq!(
        resolve_effective_cursor(Some("after=2"), &empty_reconnect)
            .unwrap()
            .value(),
        SchemaU64::new(2)
    );

    reconnect.insert("last-event-id", HeaderValue::from_static("01"));
    assert_eq!(
        resolve_effective_cursor(Some("after=2"), &reconnect)
            .unwrap_err()
            .kind(),
        CursorErrorKind::InvalidLastEventId
    );
    let mut ambiguous = HeaderMap::new();
    ambiguous.append("last-event-id", HeaderValue::from_static("1"));
    ambiguous.append("last-event-id", HeaderValue::from_static("2"));
    assert_eq!(
        resolve_effective_cursor(Some("after=0"), &ambiguous)
            .unwrap_err()
            .kind(),
        CursorErrorKind::AmbiguousLastEventId
    );

    let future = cursor(8).validate_head(SchemaU64::new(7)).unwrap_err();
    assert_eq!(future.kind(), CursorErrorKind::Future);
    assert_eq!(future.status(), StatusCode::CONFLICT);
    assert!(!cursor(3).is_recoverable_from(Some(SchemaU64::new(5))));
    assert!(cursor(4).is_recoverable_from(Some(SchemaU64::new(5))));
}

#[test]
fn frame_bytes_are_canonical_one_event_frames_and_controls_never_have_ids() {
    let committed = event(1);
    let diagnostic = SseFrame::diagnostic_event(&committed);
    let expected = format!(
        "event: diagnostic_event\nid: 1\ndata: {}\n\n",
        std::str::from_utf8(committed.canonical_json()).unwrap()
    );
    assert_eq!(diagnostic.bytes(), expected.as_bytes());
    assert_eq!(diagnostic.id(), Some(SchemaU64::new(1)));
    assert!(diagnostic.advances_cursor());

    let controls = [
        SseFrame::stream_ready(run_id(), SchemaU64::new(1), SchemaU64::new(2)).unwrap(),
        SseFrame::heartbeat(run_id(), SchemaU64::new(2)).unwrap(),
        SseFrame::delivery_gap(
            run_id(),
            "subscriber_buffer_overflow",
            SchemaU64::new(1),
            SchemaU64::new(2),
        )
        .unwrap(),
        SseFrame::resync_required(
            run_id(),
            "cursor_unavailable",
            SchemaU64::new(2),
            Some(SchemaU64::new(1)),
        )
        .unwrap(),
        SseFrame::stream_closed(run_id(), PRODUCTION_FINISHED_REASON, SchemaU64::new(2)).unwrap(),
    ];
    assert_eq!(
        controls.each_ref().map(|frame| frame.kind()),
        [
            SseFrameKind::StreamReady,
            SseFrameKind::Heartbeat,
            SseFrameKind::DeliveryGap,
            SseFrameKind::ResyncRequired,
            SseFrameKind::StreamClosed,
        ]
    );
    for control in controls {
        assert_eq!(control.id(), None);
        assert!(!control.advances_cursor());
        assert!(
            !std::str::from_utf8(control.bytes())
                .unwrap()
                .contains("\nid: ")
        );
        assert!(control.bytes().ends_with(b"\n\n"));
    }

    let ready = SseFrame::stream_ready(run_id(), SchemaU64::new(1), SchemaU64::new(2)).unwrap();
    assert_eq!(
        std::str::from_utf8(ready.bytes()).unwrap(),
        format!(
            "event: stream_ready\ndata: {{\"control_schema_version\":1,\"run_id\":\"{RUN_ID}\",\"resume_after\":\"1\",\"replay_through\":\"2\"}}\n\n"
        )
    );
}

#[test]
fn response_headers_disable_caching_transforms_and_proxy_buffering() {
    let headers = sse_response_headers();
    assert_eq!(
        headers.get("content-type").unwrap(),
        "text/event-stream; charset=utf-8"
    );
    assert_eq!(
        headers.get("cache-control").unwrap(),
        "no-cache, no-transform"
    );
    assert_eq!(headers.get("x-accel-buffering").unwrap(), "no");
    assert_eq!(headers.len(), 3);

    let mut accept = HeaderMap::new();
    accept.insert(
        "accept",
        HeaderValue::from_static("application/json, text/event-stream; q=0.5"),
    );
    assert!(accepts_event_stream(&accept));
    accept.insert("accept", HeaderValue::from_static("text/event-stream; q=0"));
    assert!(!accepts_event_stream(&accept));
    accept.insert(
        "accept",
        HeaderValue::from_static("text/event-stream; q=invalid"),
    );
    assert!(!accepts_event_stream(&accept));
}

#[test]
fn finalized_http_response_preserves_the_exact_sse_transport_headers() {
    let route = RouteDefinition::get("/api/v1/events", |_request| async {
        let body = resync_body(run_id(), "cursor_unavailable", SchemaU64::new(0), None).unwrap();
        Ok(sse_route_response(body))
    })
    .unwrap();
    let server = DiagnosticServer::start(
        ServerConfig::new(
            run_id(),
            std::process::id(),
            ProcessIdentity::new("test", "sse-header-owner").unwrap(),
        ),
        vec![route],
    )
    .unwrap();
    let response = raw_request(&server, "/api/v1/events?after=0", "text/event-stream");
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let headers = std::str::from_utf8(&response[..header_end])
        .unwrap()
        .to_ascii_lowercase();
    assert!(headers.starts_with("http/1.1 200"));
    assert!(headers.contains("content-type: text/event-stream; charset=utf-8\r\n"));
    assert!(headers.contains("cache-control: no-cache, no-transform\r\n"));
    assert!(headers.contains("x-accel-buffering: no\r\n"));
    assert!(headers.contains("transfer-encoding: chunked\r\n"));
    assert!(!headers.contains("access-control-"));
    assert!(
        response[header_end + 4..]
            .windows(b"event: resync_required".len())
            .any(|window| window == b"event: resync_required")
    );
    assert!(server.try_core_failure().is_none());
    server.shutdown().unwrap();
}

#[test]
fn endpoint_returns_versioned_http_errors_before_establishing_a_stream() {
    let root = TestRoot::new();
    let layout = ArchiveLayout::prepare(root.path(), run_id()).unwrap();
    let lease = Arc::new(ActiveArchiveLease::acquire(layout.run_directory()).unwrap());
    let _store = DiagnosticStore::create(
        layout.run_directory(),
        &InitialStoreMetadata::new(
            run_id(),
            "2026-08-16T00:00:00Z",
            "configuration-sha256:h02-http-errors",
        ),
    )
    .unwrap();
    let signal = CommitSignal::new(run_id(), SchemaU64::new(0));
    let failures = Arc::new(Mutex::new(Vec::new()));
    let reported = Arc::clone(&failures);
    let endpoint = SseEndpoint::active(
        ActiveReplaySource::new(run_id(), lease),
        signal,
        limits(4),
        ReplayDriverConfig::new(Duration::from_secs(5)).unwrap(),
        move |failure| reported.lock().unwrap().push(failure),
    )
    .unwrap();
    let route = RouteDefinition::get("/api/v1/events", move |request| {
        let endpoint = endpoint.clone();
        async move { Ok(endpoint.handle_follow(request)) }
    })
    .unwrap();
    let server = DiagnosticServer::start(
        ServerConfig::new(
            run_id(),
            std::process::id(),
            ProcessIdentity::new("test", "sse-error-owner").unwrap(),
        ),
        vec![route],
    )
    .unwrap();

    for (path, accept, expected_status, expected_code) in [
        ("/api/v1/events", "text/event-stream", 400, "missing_cursor"),
        (
            "/api/v1/events?after=01",
            "text/event-stream",
            400,
            "invalid_cursor",
        ),
        (
            "/api/v1/events?after=1",
            "text/event-stream",
            409,
            "cursor_ahead_of_head",
        ),
        (
            "/api/v1/events?after=0",
            "application/json",
            406,
            "unsupported_format",
        ),
    ] {
        let response = raw_request(&server, path, accept);
        let (status, body) = response_status_and_json(&response);
        assert_eq!(status, expected_status, "{path}");
        assert_eq!(body["api_schema_version"], 1);
        assert_eq!(body["run_id"], RUN_ID);
        assert_eq!(body["error"]["code"], expected_code);
        assert_eq!(body["error"]["details"], serde_json::Value::Null);
    }
    assert!(failures.lock().unwrap().is_empty());
    assert!(server.try_core_failure().is_none());
    server.shutdown().unwrap();
}

#[tokio::test]
async fn replay_handoff_captures_racing_commits_without_loss_or_duplicates() {
    let signal = CommitSignal::new(run_id(), SchemaU64::new(1));
    let coordinator = ReplayCoordinator::new(signal.clone(), limits(8));
    let start = coordinator
        .begin(cursor(0), || {
            // The snapshot transaction captured H=1 before this post-COMMIT
            // wake. Registration already happened inside begin().
            signal
                .advance(run_id(), SchemaU64::new(1), SchemaU64::new(2))
                .unwrap();
            Ok(active_window(1))
        })
        .unwrap();
    let ReplayStart::Ready(mut session) = start else {
        panic!("recoverable cursor must start a stream")
    };
    let mut body = session.take_body().expect("one response body");

    let ready = next_frame(&mut body).await;
    assert!(contains_line(&ready, "event: stream_ready"));
    assert!(
        std::str::from_utf8(&ready)
            .unwrap()
            .contains("\"replay_through\":\"1\"")
    );

    assert_eq!(
        session
            .push_replay_page(&page(0, 1, vec![event(1)]))
            .await
            .unwrap(),
        DeliveryStatus::Enqueued
    );
    session.finish_replay().unwrap();
    assert_eq!(session.phase(), ReplayPhase::Live);
    assert_eq!(
        session.next_live_range().unwrap().unwrap(),
        ReplayRange::new(SchemaU64::new(1), SchemaU64::new(2)).unwrap()
    );
    assert_eq!(
        session.push_live_page(&page(1, 2, vec![event(2)])).unwrap(),
        DeliveryStatus::Enqueued
    );
    signal
        .close(PRODUCTION_FINISHED_REASON, SchemaU64::new(2))
        .unwrap();
    assert!(session.next_live_range().unwrap().is_none());
    assert_eq!(session.phase(), ReplayPhase::Closed);

    let first = next_frame(&mut body).await;
    let second = next_frame(&mut body).await;
    let closed = next_frame(&mut body).await;
    assert!(contains_line(&first, "id: 1"));
    assert!(contains_line(&second, "id: 2"));
    assert!(contains_line(&closed, "event: stream_closed"));
    assert!(!contains_line(&closed, "id: 2"));
    assert_body_closed(&mut body).await;
}

#[tokio::test]
async fn active_driver_exposes_only_post_commit_store_events_and_drains_on_close() {
    let root = TestRoot::new();
    let layout = ArchiveLayout::prepare(root.path(), run_id()).unwrap();
    let lease = Arc::new(ActiveArchiveLease::acquire(layout.run_directory()).unwrap());
    let store = DiagnosticStore::create(
        layout.run_directory(),
        &InitialStoreMetadata::new(run_id(), "2026-08-16T00:00:00Z", "configuration-sha256:h02"),
    )
    .unwrap();
    let signal = CommitSignal::new(run_id(), SchemaU64::new(0));
    let mut writer = TransactionalWriter::new(store, signal.clone()).unwrap();
    let hub = ProductionDiagnosticHub::production(run_id(), AcceptAll, Box::new(IgnoreLive));

    let uncommitted = accepted_event(&hub, 10);
    let source = ActiveReplaySource::new(run_id(), Arc::clone(&lease));
    assert_eq!(
        source.capture_window().unwrap().committed_head(),
        SchemaU64::new(0)
    );
    writer
        .commit_batch(&EventBatch::new(vec![uncommitted]).unwrap())
        .unwrap();

    let coordinator = ReplayCoordinator::new(signal.clone(), limits(8));
    let ReplayStart::Ready(mut session) = coordinator
        .begin(cursor(0), || source.capture_window())
        .unwrap()
    else {
        panic!("committed active prefix is replayable")
    };
    let mut body = session.take_body().unwrap();
    let driver_source = source.clone();
    let driver = tokio::spawn(async move {
        driver_source
            .drive(
                session,
                ReplayDriverConfig::new(Duration::from_secs(5)).unwrap(),
            )
            .await
    });

    assert!(contains_line(
        &next_frame(&mut body).await,
        "event: stream_ready"
    ));
    assert!(contains_line(&next_frame(&mut body).await, "id: 1"));

    let second = accepted_event(&hub, 20);
    assert_eq!(
        source.capture_window().unwrap().committed_head(),
        SchemaU64::new(1)
    );
    writer
        .commit_batch(&EventBatch::new(vec![second]).unwrap())
        .unwrap();
    signal
        .close(PRODUCTION_FINISHED_REASON, SchemaU64::new(2))
        .unwrap();

    assert!(contains_line(&next_frame(&mut body).await, "id: 2"));
    assert!(contains_line(
        &next_frame(&mut body).await,
        "event: stream_closed"
    ));
    assert_body_closed(&mut body).await;
    driver.await.unwrap().unwrap();
}

#[tokio::test]
async fn reconnect_replays_at_least_once_from_the_last_processed_id() {
    let signal = CommitSignal::new(run_id(), SchemaU64::new(2));
    let coordinator = ReplayCoordinator::new(signal, limits(8));
    let ReplayStart::Ready(mut first) = coordinator
        .begin(cursor(0), || Ok(active_window(2)))
        .unwrap()
    else {
        panic!("initial replay must be ready")
    };
    let mut first_body = first.take_body().unwrap();
    let _ready = next_frame(&mut first_body).await;
    first
        .push_replay_page(&page(0, 2, vec![event(1), event(2)]))
        .await
        .unwrap();
    first.finish_replay().unwrap();
    assert!(contains_line(&next_frame(&mut first_body).await, "id: 1"));
    drop(first_body); // Event 2 may have been queued but was not processed.

    let ReplayStart::Ready(mut reconnect) = coordinator
        .begin(cursor(1), || Ok(active_window(2)))
        .unwrap()
    else {
        panic!("reconnect replay must be ready")
    };
    let mut reconnect_body = reconnect.take_body().unwrap();
    let _ready = next_frame(&mut reconnect_body).await;
    reconnect
        .push_replay_page(&page(1, 2, vec![event(2)]))
        .await
        .unwrap();
    reconnect.finish_replay().unwrap();
    let repeated = next_frame(&mut reconnect_body).await;
    assert!(contains_line(&repeated, "id: 2"));
    assert_eq!(
        repeated
            .windows(b"id: 2".len())
            .filter(|v| *v == b"id: 2")
            .count(),
        1
    );
}

#[tokio::test]
async fn slow_subscriber_overflow_is_isolated_and_ends_with_delivery_gap() {
    let (slow_sender, mut slow_body) =
        open_subscriber(run_id(), SchemaU64::new(0), SchemaU64::new(0), limits(1)).unwrap();
    let (fast_sender, mut fast_body) =
        open_subscriber(run_id(), SchemaU64::new(0), SchemaU64::new(0), limits(1)).unwrap();
    let _fast_ready = next_frame(&mut fast_body).await;

    assert_eq!(
        slow_sender
            .try_send_event(&event(1), SchemaU64::new(1))
            .unwrap(),
        DeliveryStatus::Enqueued
    );
    assert_eq!(
        fast_sender
            .try_send_event(&event(1), SchemaU64::new(1))
            .unwrap(),
        DeliveryStatus::Enqueued
    );
    assert!(contains_line(&next_frame(&mut fast_body).await, "id: 1"));

    assert_eq!(
        slow_sender
            .try_send_event(&event(2), SchemaU64::new(2))
            .unwrap(),
        DeliveryStatus::Overflowed
    );
    assert_eq!(
        fast_sender
            .try_send_event(&event(2), SchemaU64::new(2))
            .unwrap(),
        DeliveryStatus::Enqueued
    );

    let slow_ready = next_frame(&mut slow_body).await;
    assert!(contains_line(&slow_ready, "event: stream_ready"));
    let gap = next_frame(&mut slow_body).await;
    assert!(contains_line(&gap, "event: delivery_gap"));
    assert!(
        std::str::from_utf8(&gap)
            .unwrap()
            .contains("subscriber_buffer_overflow")
    );
    assert!(
        std::str::from_utf8(&gap)
            .unwrap()
            .contains("\"last_delivered_sequence\":\"0\"")
    );
    assert!(!std::str::from_utf8(&gap).unwrap().contains("\nid: "));
    assert_body_closed(&mut slow_body).await;
    assert!(contains_line(&next_frame(&mut fast_body).await, "id: 2"));
    assert!(!fast_sender.is_closed());
}

#[tokio::test]
async fn replay_waits_for_its_finite_buffer_but_live_delivery_never_blocks_writer() {
    let (sender, mut body) =
        open_subscriber(run_id(), SchemaU64::new(0), SchemaU64::new(2), limits(1)).unwrap();
    let _ready = next_frame(&mut body).await;
    assert_eq!(
        sender
            .send_replay_event(&event(1), SchemaU64::new(2))
            .await
            .unwrap(),
        DeliveryStatus::Enqueued
    );

    let waiting_sender = sender.clone();
    let waiter = tokio::spawn(async move {
        waiting_sender
            .send_replay_event(&event(2), SchemaU64::new(2))
            .await
    });
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());
    assert!(contains_line(&next_frame(&mut body).await, "id: 1"));
    assert_eq!(waiter.await.unwrap().unwrap(), DeliveryStatus::Enqueued);
    assert!(contains_line(&next_frame(&mut body).await, "id: 2"));

    assert_eq!(
        sender.try_send_event(&event(2), SchemaU64::new(2)).unwrap(),
        DeliveryStatus::Duplicate
    );
}

#[tokio::test]
async fn invalid_post_stream_tail_emits_resync_and_closes_without_advancing_cursor() {
    let signal = CommitSignal::new(run_id(), SchemaU64::new(1));
    let coordinator = ReplayCoordinator::new(signal.clone(), limits(4));
    let ReplayStart::Ready(mut session) = coordinator
        .begin(cursor(1), || Ok(active_window(1)))
        .unwrap()
    else {
        panic!("head cursor is recoverable")
    };
    let mut body = session.take_body().unwrap();
    let ready = next_frame(&mut body).await;
    assert!(contains_line(&ready, "event: stream_ready"));
    session.finish_replay().unwrap();

    signal
        .advance(run_id(), SchemaU64::new(0), SchemaU64::new(2))
        .unwrap_err();
    assert!(matches!(
        signal.state().status(),
        CommitTailStatus::Invalid { .. }
    ));
    assert!(session.next_live_range().unwrap().is_none());
    let resync = next_frame(&mut body).await;
    assert!(contains_line(&resync, "event: resync_required"));
    assert!(!std::str::from_utf8(&resync).unwrap().contains("\nid: "));
    assert_eq!(body.last_delivered_sequence(), SchemaU64::new(1));
    assert_body_closed(&mut body).await;
}

#[test]
fn commit_signal_requires_an_exact_close_watermark_and_invalidates_late_commits() {
    let signal = CommitSignal::new(run_id(), SchemaU64::new(1));
    assert_eq!(
        signal
            .close(PRODUCTION_FINISHED_REASON, SchemaU64::new(2))
            .unwrap_err()
            .kind(),
        CommitSignalErrorKind::FinalWatermarkMismatch
    );
    assert!(matches!(signal.state().status(), CommitTailStatus::Open));

    signal
        .close(PRODUCTION_FINISHED_REASON, SchemaU64::new(1))
        .unwrap();
    assert!(matches!(
        signal.state().status(),
        CommitTailStatus::Closed { .. }
    ));
    assert_eq!(
        signal
            .advance(run_id(), SchemaU64::new(1), SchemaU64::new(2))
            .unwrap_err()
            .kind(),
        CommitSignalErrorKind::NotOpen
    );
    assert!(matches!(
        signal.state().status(),
        CommitTailStatus::Invalid { .. }
    ));
}

#[tokio::test]
async fn unrecoverable_cursor_resyncs_first_while_future_and_archive_fail_before_streaming() {
    let signal = CommitSignal::new(run_id(), SchemaU64::new(10));
    let coordinator = ReplayCoordinator::new(signal, limits(4));
    let truncated = ReplayWindow::new(
        ReaderProfile::Active,
        run_id(),
        SchemaU64::new(10),
        Some(SchemaU64::new(5)),
    )
    .unwrap();
    let ReplayStart::Resync(mut body) = coordinator.begin(cursor(2), || Ok(truncated)).unwrap()
    else {
        panic!("unavailable history must request resync")
    };
    let first = next_frame(&mut body).await;
    assert!(contains_line(&first, "event: resync_required"));
    assert!(!contains_line(&first, "event: stream_ready"));
    assert_body_closed(&mut body).await;

    let future = coordinator
        .begin(cursor(11), || Ok(active_window(10)))
        .err()
        .expect("future cursor rejected before a body exists");
    assert_eq!(future.kind(), ReplayErrorKind::FutureCursor);
    assert_eq!(future.status(), StatusCode::CONFLICT);

    let archive = coordinator
        .begin(cursor(10), || {
            ReplayWindow::new(
                ReaderProfile::Archive,
                run_id(),
                SchemaU64::new(10),
                Some(SchemaU64::new(1)),
            )
        })
        .err()
        .expect("archive follow rejected before a body exists");
    assert_eq!(archive.kind(), ReplayErrorKind::ArchiveFollowUnsupported);
    assert_eq!(archive.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn heartbeat_and_shutdown_are_prompt_separate_no_id_frames() {
    let (sender, mut body) =
        open_subscriber(run_id(), SchemaU64::new(0), SchemaU64::new(0), limits(4)).unwrap();
    let ready = next_frame(&mut body).await;
    assert!(contains_line(&ready, "event: stream_ready"));
    assert_eq!(body.last_delivered_sequence(), SchemaU64::new(0));

    assert_eq!(
        sender.try_send_heartbeat(SchemaU64::new(0)).unwrap(),
        DeliveryStatus::Enqueued
    );
    assert_eq!(
        sender
            .close(PRODUCTION_FINISHED_REASON, SchemaU64::new(0))
            .unwrap(),
        DeliveryStatus::Enqueued
    );
    let heartbeat = next_frame(&mut body).await;
    let closed = next_frame(&mut body).await;
    assert!(contains_line(&heartbeat, "event: heartbeat"));
    assert!(contains_line(&closed, "event: stream_closed"));
    for control in [&heartbeat, &closed] {
        assert!(!std::str::from_utf8(control).unwrap().contains("\nid: "));
    }
    assert_eq!(body.last_delivered_sequence(), SchemaU64::new(0));
    assert_body_closed(&mut body).await;
}

#[tokio::test]
async fn out_of_order_live_event_resyncs_instead_of_silently_skipping() {
    let (sender, mut body) =
        open_subscriber(run_id(), SchemaU64::new(1), SchemaU64::new(1), limits(4)).unwrap();
    let _ready = next_frame(&mut body).await;
    assert_eq!(
        sender.try_send_event(&event(3), SchemaU64::new(3)).unwrap(),
        DeliveryStatus::ResyncRequired
    );
    let resync = next_frame(&mut body).await;
    assert!(contains_line(&resync, "event: resync_required"));
    assert!(
        std::str::from_utf8(&resync)
            .unwrap()
            .contains(CURSOR_INCONSISTENT_REASON)
    );
    assert_body_closed(&mut body).await;
}

#[tokio::test]
async fn event_beyond_the_committed_watermark_forces_resync() {
    let (sender, mut body) =
        open_subscriber(run_id(), SchemaU64::new(1), SchemaU64::new(1), limits(4)).unwrap();
    let _ready = next_frame(&mut body).await;
    assert_eq!(
        sender.try_send_event(&event(2), SchemaU64::new(1)).unwrap(),
        DeliveryStatus::ResyncRequired
    );
    let resync = next_frame(&mut body).await;
    assert!(contains_line(&resync, "event: resync_required"));
    assert!(
        std::str::from_utf8(&resync)
            .unwrap()
            .contains(CURSOR_INCONSISTENT_REASON)
    );
    assert_eq!(body.last_delivered_sequence(), SchemaU64::new(1));
    assert_body_closed(&mut body).await;
}
