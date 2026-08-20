use std::{fmt, panic::AssertUnwindSafe, sync::Arc, time::Duration};

use futures::FutureExt as _;
use hyper::{HeaderMap, StatusCode, header::ACCEPT};
use serde_json::json;
use troupe_diagnostics_core::{id::CanonicalUuid, scalar::SchemaU64};

use crate::archive::lease::ActiveArchiveLease;
use crate::query::reader::{
    CAPTURED_EVENT_PAGE_SIZE, CapturedEventSource, DiagnosticReader, ReaderErrorCode,
    ReaderFailure, ReaderProfile,
};
use crate::server::{
    query::API_SCHEMA_VERSION,
    routes::{RouteRequest, RouteResponse},
};

use super::{
    cursor::{CursorError, EffectiveCursor, resolve_effective_cursor},
    frame::{
        CURSOR_INCONSISTENT_REASON, CURSOR_UNAVAILABLE_REASON, CommittedEvent, SseFrame,
        sse_route_response,
    },
    subscriber::{
        CommitListener, CommitSignal, CommitTailStatus, DeliveryStatus, SseBody, SseSender,
        SubscriberError, SubscriberLimits, open_subscriber, resync_body,
    },
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayWindow {
    profile: ReaderProfile,
    run_id: CanonicalUuid,
    committed_head: SchemaU64,
    earliest_available_sequence: Option<SchemaU64>,
}

impl ReplayWindow {
    pub fn capture(source: &CapturedEventSource<'_>) -> Result<Self, ReplayError> {
        let committed_head = source.captured_watermark();
        Self::new(
            source.profile(),
            source.metadata().run_id(),
            committed_head,
            retained_earliest(committed_head),
        )
    }

    pub fn new(
        profile: ReaderProfile,
        run_id: CanonicalUuid,
        committed_head: SchemaU64,
        earliest_available_sequence: Option<SchemaU64>,
    ) -> Result<Self, ReplayError> {
        let retained_range_valid = match earliest_available_sequence {
            None => committed_head.get() == 0,
            Some(earliest) => {
                committed_head.get() != 0
                    && earliest.get() != 0
                    && earliest.get() <= committed_head.get()
            }
        };
        if !retained_range_valid {
            return Err(ReplayError::invalid(ReplayErrorKind::InvalidRetainedRange));
        }
        Ok(Self {
            profile,
            run_id,
            committed_head,
            earliest_available_sequence,
        })
    }

    pub const fn profile(&self) -> ReaderProfile {
        self.profile
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub const fn committed_head(&self) -> SchemaU64 {
        self.committed_head
    }

    pub const fn earliest_available_sequence(&self) -> Option<SchemaU64> {
        self.earliest_available_sequence
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplayRange {
    after_exclusive: SchemaU64,
    through_inclusive: SchemaU64,
}

impl ReplayRange {
    pub fn new(
        after_exclusive: SchemaU64,
        through_inclusive: SchemaU64,
    ) -> Result<Self, ReplayError> {
        if after_exclusive.get() > through_inclusive.get() {
            return Err(ReplayError::invalid(ReplayErrorKind::InvalidPageRange));
        }
        Ok(Self {
            after_exclusive,
            through_inclusive,
        })
    }

    pub const fn after_exclusive(self) -> SchemaU64 {
        self.after_exclusive
    }

    pub const fn through_inclusive(self) -> SchemaU64 {
        self.through_inclusive
    }

    pub const fn is_empty(self) -> bool {
        self.after_exclusive.get() == self.through_inclusive.get()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayPage {
    run_id: CanonicalUuid,
    range: ReplayRange,
    events: Vec<CommittedEvent>,
}

impl ReplayPage {
    pub fn capture(
        source: &CapturedEventSource<'_>,
        range: ReplayRange,
    ) -> Result<Self, ReplayError> {
        if range.through_inclusive().get() > source.captured_watermark().get() {
            return Err(ReplayError::invalid(
                ReplayErrorKind::CapturedHeadBehindRange,
            ));
        }
        let events = if range.is_empty() {
            Vec::new()
        } else {
            source
                .read_event_page(range.after_exclusive())
                .map_err(ReplayError::Reader)?
                .into_events()
                .into_iter()
                .take_while(|event| event.sequence().get() <= range.through_inclusive().get())
                .map(CommittedEvent::from)
                .collect()
        };
        Self::new(source.metadata().run_id(), range, events)
    }

    pub fn new(
        run_id: CanonicalUuid,
        range: ReplayRange,
        events: Vec<CommittedEvent>,
    ) -> Result<Self, ReplayError> {
        if events.len() > CAPTURED_EVENT_PAGE_SIZE {
            return Err(ReplayError::invalid(ReplayErrorKind::PageTooLarge));
        }
        let mut expected = range.after_exclusive().get().checked_add(1);
        for event in &events {
            if event.run_id() != run_id {
                return Err(ReplayError::invalid(ReplayErrorKind::RunIdentityMismatch));
            }
            if expected != Some(event.sequence().get())
                || event.sequence().get() > range.through_inclusive().get()
            {
                return Err(ReplayError::invalid(ReplayErrorKind::NonDensePage));
            }
            expected = event.sequence().get().checked_add(1);
        }
        if !range.is_empty() && events.is_empty() {
            return Err(ReplayError::invalid(ReplayErrorKind::NonDensePage));
        }
        Ok(Self {
            run_id,
            range,
            events,
        })
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub const fn range(&self) -> ReplayRange {
        self.range
    }

    pub fn events(&self) -> &[CommittedEvent] {
        &self.events
    }

    pub fn next_after(&self) -> SchemaU64 {
        match self.events.last() {
            Some(event) => event.sequence(),
            None => self.range.after_exclusive(),
        }
    }

    pub fn completes_range(&self) -> bool {
        self.next_after().get() == self.range.through_inclusive().get()
    }
}

#[derive(Clone)]
pub struct ReplayCoordinator {
    commit_signal: CommitSignal,
    subscriber_limits: SubscriberLimits,
}

impl ReplayCoordinator {
    pub const fn new(commit_signal: CommitSignal, subscriber_limits: SubscriberLimits) -> Self {
        Self {
            commit_signal,
            subscriber_limits,
        }
    }

    pub fn begin<F>(&self, cursor: EffectiveCursor, capture: F) -> Result<ReplayStart, ReplayError>
    where
        F: FnOnce() -> Result<ReplayWindow, ReplayError>,
    {
        // Subscribe before H is captured. A commit racing with the capture is
        // retained as a newer head and replayed from the store after H.
        let listener = self.commit_signal.subscribe();
        let window = capture()?;
        if window.profile() == ReaderProfile::Archive {
            return Err(ReplayError::invalid(
                ReplayErrorKind::ArchiveFollowUnsupported,
            ));
        }
        let tail_state = listener.current();
        if window.run_id() != tail_state.run_id() {
            return Err(ReplayError::invalid(ReplayErrorKind::RunIdentityMismatch));
        }
        if matches!(tail_state.status(), CommitTailStatus::Invalid { .. }) {
            return Err(ReplayError::invalid(ReplayErrorKind::CommitSignalInvalid));
        }
        if cursor.value().get() > window.committed_head().get() {
            return Err(ReplayError::future(cursor.value(), window.committed_head()));
        }
        if !cursor.is_recoverable_from(window.earliest_available_sequence()) {
            return Ok(ReplayStart::Resync(resync_body(
                window.run_id(),
                CURSOR_UNAVAILABLE_REASON,
                window.committed_head(),
                window.earliest_available_sequence(),
            )?));
        }

        let (sender, body) = open_subscriber(
            window.run_id(),
            cursor.value(),
            window.committed_head(),
            self.subscriber_limits,
        )?;
        Ok(ReplayStart::Ready(ReplaySession {
            run_id: window.run_id(),
            earliest_available_sequence: window.earliest_available_sequence(),
            replay_through: window.committed_head(),
            ingested_through: cursor.value(),
            sender,
            body: Some(body),
            listener,
            phase: ReplayPhase::Replaying,
        }))
    }
}

#[derive(Clone)]
pub struct ActiveReplaySource {
    run_id: CanonicalUuid,
    lease: Arc<ActiveArchiveLease>,
}

impl ActiveReplaySource {
    pub const fn new(run_id: CanonicalUuid, lease: Arc<ActiveArchiveLease>) -> Self {
        Self { run_id, lease }
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub fn capture_window(&self) -> Result<ReplayWindow, ReplayError> {
        let mut reader = DiagnosticReader::open_active(self.run_id, self.lease.guard())
            .map_err(ReplayError::Reader)?;
        let captured = reader.capture().map_err(ReplayError::Reader)?;
        ReplayWindow::capture(&captured)
    }

    pub fn capture_page(&self, range: ReplayRange) -> Result<ReplayPage, ReplayError> {
        let mut reader = DiagnosticReader::open_active(self.run_id, self.lease.guard())
            .map_err(ReplayError::Reader)?;
        let captured = reader.capture().map_err(ReplayError::Reader)?;
        ReplayPage::capture(&captured, range)
    }

    pub async fn drive(
        &self,
        mut session: ReplaySession,
        config: ReplayDriverConfig,
    ) -> Result<(), ReplayError> {
        if session.body.is_some() {
            return Err(ReplayError::invalid(ReplayErrorKind::BodyNotDetached));
        }

        if let Err(error) = self.drive_replay(&mut session).await {
            let _ = session.post_stream_resync(CURSOR_INCONSISTENT_REASON);
            return Err(error);
        }
        loop {
            if session.phase() == ReplayPhase::Closed {
                return Ok(());
            }
            match tokio::time::timeout(config.heartbeat_interval(), session.wait_for_live_range())
                .await
            {
                Ok(Ok(Some(range))) => {
                    if let Err(error) = self.drive_live_range(&mut session, range) {
                        let _ = session.post_stream_resync(CURSOR_INCONSISTENT_REASON);
                        return Err(error);
                    }
                }
                Ok(Ok(None)) => return Ok(()),
                Ok(Err(error)) => return Err(error),
                Err(_) => match session.heartbeat()? {
                    DeliveryStatus::Closed
                    | DeliveryStatus::Overflowed
                    | DeliveryStatus::ResyncRequired => return Ok(()),
                    DeliveryStatus::Enqueued
                    | DeliveryStatus::DroppedControl
                    | DeliveryStatus::Duplicate => {}
                },
            }
        }
    }

    async fn drive_replay(&self, session: &mut ReplaySession) -> Result<(), ReplayError> {
        while !session.replay_range().is_empty() {
            let page = self.capture_page(session.replay_range())?;
            match session.push_replay_page(&page).await? {
                DeliveryStatus::Closed
                | DeliveryStatus::Overflowed
                | DeliveryStatus::ResyncRequired => return Ok(()),
                DeliveryStatus::Enqueued | DeliveryStatus::Duplicate => {}
                DeliveryStatus::DroppedControl => unreachable!("replay pages contain facts"),
            }
        }
        session.finish_replay()
    }

    fn drive_live_range(
        &self,
        session: &mut ReplaySession,
        range: ReplayRange,
    ) -> Result<(), ReplayError> {
        let through = range.through_inclusive();
        while session.ingested_through().get() < through.get() {
            let page = self.capture_page(ReplayRange {
                after_exclusive: session.ingested_through(),
                through_inclusive: through,
            })?;
            match session.push_live_page(&page)? {
                DeliveryStatus::Closed
                | DeliveryStatus::Overflowed
                | DeliveryStatus::ResyncRequired => return Ok(()),
                DeliveryStatus::Enqueued | DeliveryStatus::Duplicate => {}
                DeliveryStatus::DroppedControl => unreachable!("live pages contain facts"),
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplayDriverConfig {
    heartbeat_interval: Duration,
}

impl ReplayDriverConfig {
    pub fn new(heartbeat_interval: Duration) -> Result<Self, ReplayError> {
        if heartbeat_interval.is_zero() {
            return Err(ReplayError::invalid(
                ReplayErrorKind::InvalidHeartbeatInterval,
            ));
        }
        Ok(Self { heartbeat_interval })
    }

    pub const fn heartbeat_interval(self) -> Duration {
        self.heartbeat_interval
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SseCoreFailureCode {
    Replay(ReplayErrorKind),
    DriverPanicked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SseCoreFailureSignal {
    run_id: CanonicalUuid,
    code: SseCoreFailureCode,
    reader_code: Option<ReaderErrorCode>,
}

impl SseCoreFailureSignal {
    pub const fn run_id(self) -> CanonicalUuid {
        self.run_id
    }

    pub const fn code(self) -> SseCoreFailureCode {
        self.code
    }

    pub const fn reader_code(self) -> Option<ReaderErrorCode> {
        self.reader_code
    }
}

pub trait SseCoreFailureReporter: Send + Sync + 'static {
    fn report(&self, failure: SseCoreFailureSignal);
}

impl<F> SseCoreFailureReporter for F
where
    F: Fn(SseCoreFailureSignal) + Send + Sync + 'static,
{
    fn report(&self, failure: SseCoreFailureSignal) {
        self(failure);
    }
}

#[derive(Clone)]
pub struct SseEndpoint {
    run_id: CanonicalUuid,
    source: ActiveReplaySource,
    coordinator: ReplayCoordinator,
    driver_config: ReplayDriverConfig,
    core_failure_reporter: Arc<dyn SseCoreFailureReporter>,
}

impl SseEndpoint {
    pub fn active<R>(
        source: ActiveReplaySource,
        commit_signal: CommitSignal,
        subscriber_limits: SubscriberLimits,
        driver_config: ReplayDriverConfig,
        core_failure_reporter: R,
    ) -> Result<Self, ReplayError>
    where
        R: SseCoreFailureReporter,
    {
        let run_id = source.run_id();
        if commit_signal.state().run_id() != run_id {
            return Err(ReplayError::invalid(ReplayErrorKind::RunIdentityMismatch));
        }
        let largest_ready =
            SseFrame::stream_ready(run_id, SchemaU64::new(u64::MAX), SchemaU64::new(u64::MAX))
                .map_err(SubscriberError::from)?;
        if largest_ready.byte_len() > subscriber_limits.max_buffered_bytes() {
            return Err(ReplayError::invalid(
                ReplayErrorKind::InvalidSubscriberLimits,
            ));
        }
        Ok(Self {
            run_id,
            source,
            coordinator: ReplayCoordinator::new(commit_signal, subscriber_limits),
            driver_config,
            core_failure_reporter: Arc::new(core_failure_reporter),
        })
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub fn handle_follow(&self, request: RouteRequest) -> RouteResponse {
        if !requests_event_stream(&request) {
            return versioned_error_response(
                self.run_id,
                StatusCode::NOT_ACCEPTABLE,
                "unsupported_format",
                "the live events endpoint requires Accept: text/event-stream",
            );
        }
        let cursor = match resolve_effective_cursor(request.uri().query(), request.headers()) {
            Ok(cursor) => cursor,
            Err(error) => return cursor_error_response(self.run_id, error),
        };
        match self
            .coordinator
            .begin(cursor, || self.source.capture_window())
        {
            Ok(ReplayStart::Resync(body)) => sse_route_response(body),
            Ok(ReplayStart::Ready(mut session)) => {
                let body = session
                    .take_body()
                    .expect("a new replay session owns exactly one response body");
                let source = self.source.clone();
                let config = self.driver_config;
                let reporter = Arc::clone(&self.core_failure_reporter);
                let run_id = self.run_id;
                tokio::spawn(async move {
                    match AssertUnwindSafe(source.drive(session, config))
                        .catch_unwind()
                        .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => reporter.report(core_failure(run_id, &error)),
                        Err(_) => reporter.report(SseCoreFailureSignal {
                            run_id,
                            code: SseCoreFailureCode::DriverPanicked,
                            reader_code: None,
                        }),
                    }
                });
                sse_route_response(body)
            }
            Err(error) => {
                if error.status().is_server_error() {
                    self.core_failure_reporter
                        .report(core_failure(self.run_id, &error));
                }
                replay_error_response(self.run_id, &error)
            }
        }
    }
}

pub fn requests_event_stream(request: &RouteRequest) -> bool {
    accepts_event_stream(request.headers())
}

pub fn accepts_event_stream(headers: &HeaderMap) -> bool {
    headers.get_all(ACCEPT).iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            value.split(',').any(|candidate| {
                let mut parts = candidate.split(';');
                let media_type = parts.next().unwrap_or_default().trim();
                let mut quality = Some(1.0_f32);
                let mut saw_quality = false;
                for parameter in parts {
                    let Some((name, value)) = parameter.trim().split_once('=') else {
                        continue;
                    };
                    if name.trim().eq_ignore_ascii_case("q") {
                        if saw_quality {
                            quality = None;
                            break;
                        }
                        saw_quality = true;
                        quality = value
                            .trim()
                            .parse::<f32>()
                            .ok()
                            .filter(|value| (0.0..=1.0).contains(value));
                    }
                }
                media_type.eq_ignore_ascii_case("text/event-stream")
                    && quality.is_some_and(|value| value > 0.0)
            })
        })
    })
}

fn cursor_error_response(run_id: CanonicalUuid, error: CursorError) -> RouteResponse {
    versioned_error_response(run_id, error.status(), error.code(), error.message())
}

fn replay_error_response(run_id: CanonicalUuid, error: &ReplayError) -> RouteResponse {
    versioned_error_response(
        run_id,
        error.status(),
        error.code(),
        "the live diagnostic event stream could not be started",
    )
}

fn versioned_error_response(
    run_id: CanonicalUuid,
    status: StatusCode,
    code: &str,
    message: &str,
) -> RouteResponse {
    RouteResponse::json(
        status,
        &json!({
            "api_schema_version": API_SCHEMA_VERSION,
            "run_id": run_id,
            "error": {
                "code": code,
                "message": message,
                "details": null,
            },
        }),
    )
    .expect("the closed SSE error envelope is serializable")
}

fn core_failure(run_id: CanonicalUuid, error: &ReplayError) -> SseCoreFailureSignal {
    SseCoreFailureSignal {
        run_id,
        code: SseCoreFailureCode::Replay(error.kind()),
        reader_code: error.reader_failure().map(ReaderFailure::code),
    }
}

pub enum ReplayStart {
    Ready(ReplaySession),
    Resync(SseBody),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplayPhase {
    Replaying,
    Live,
    Closed,
}

pub struct ReplaySession {
    run_id: CanonicalUuid,
    earliest_available_sequence: Option<SchemaU64>,
    replay_through: SchemaU64,
    ingested_through: SchemaU64,
    sender: SseSender,
    body: Option<SseBody>,
    listener: CommitListener,
    phase: ReplayPhase,
}

impl ReplaySession {
    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub const fn replay_through(&self) -> SchemaU64 {
        self.replay_through
    }

    pub const fn ingested_through(&self) -> SchemaU64 {
        self.ingested_through
    }

    pub const fn phase(&self) -> ReplayPhase {
        self.phase
    }

    pub fn take_body(&mut self) -> Option<SseBody> {
        self.body.take()
    }

    pub const fn replay_range(&self) -> ReplayRange {
        ReplayRange {
            after_exclusive: self.ingested_through,
            through_inclusive: self.replay_through,
        }
    }

    pub async fn push_replay_page(
        &mut self,
        page: &ReplayPage,
    ) -> Result<DeliveryStatus, ReplayError> {
        self.validate_page(page, self.replay_through, ReplayPhase::Replaying)?;
        let mut final_status = DeliveryStatus::Enqueued;
        for event in page.events() {
            let status = self
                .sender
                .send_replay_event(event, self.replay_through)
                .await?;
            match status {
                DeliveryStatus::Enqueued => self.ingested_through = event.sequence(),
                DeliveryStatus::Duplicate => {}
                DeliveryStatus::Overflowed
                | DeliveryStatus::ResyncRequired
                | DeliveryStatus::Closed => {
                    self.phase = ReplayPhase::Closed;
                    return Ok(status);
                }
                DeliveryStatus::DroppedControl => unreachable!("event delivery is not a control"),
            }
            final_status = status;
        }
        Ok(final_status)
    }

    pub fn finish_replay(&mut self) -> Result<(), ReplayError> {
        if self.phase != ReplayPhase::Replaying {
            return Err(ReplayError::invalid(ReplayErrorKind::InvalidPhase));
        }
        if self.ingested_through != self.replay_through {
            self.post_stream_resync(CURSOR_INCONSISTENT_REASON)?;
            return Err(ReplayError::invalid(ReplayErrorKind::IncompleteReplay));
        }
        self.phase = ReplayPhase::Live;
        let _ = self.next_live_range()?;
        Ok(())
    }

    pub fn next_live_range(&mut self) -> Result<Option<ReplayRange>, ReplayError> {
        if self.phase != ReplayPhase::Live {
            return Ok(None);
        }
        let state = self.listener.current();
        let head = state.committed_head();
        match state.status() {
            CommitTailStatus::Invalid { reason } => {
                let watermark = SchemaU64::new(head.get().max(self.ingested_through.get()));
                self.sender
                    .resync_required(reason, watermark, retained_earliest(watermark))?;
                self.phase = ReplayPhase::Closed;
                Ok(None)
            }
            CommitTailStatus::Open => {
                if head.get() > self.ingested_through.get() {
                    Ok(Some(ReplayRange {
                        after_exclusive: self.ingested_through,
                        through_inclusive: head,
                    }))
                } else {
                    // The observer may legitimately lag a captured H. Its
                    // next notification still wakes this listener.
                    Ok(None)
                }
            }
            CommitTailStatus::Closed { reason } => {
                if head.get() > self.ingested_through.get() {
                    Ok(Some(ReplayRange {
                        after_exclusive: self.ingested_through,
                        through_inclusive: head,
                    }))
                } else if head == self.ingested_through {
                    self.sender.close(reason, head)?;
                    self.phase = ReplayPhase::Closed;
                    Ok(None)
                } else {
                    self.post_stream_resync(CURSOR_INCONSISTENT_REASON)?;
                    Ok(None)
                }
            }
        }
    }

    pub async fn wait_for_live_range(&mut self) -> Result<Option<ReplayRange>, ReplayError> {
        loop {
            if let Some(range) = self.next_live_range()? {
                return Ok(Some(range));
            }
            if self.phase == ReplayPhase::Closed {
                return Ok(None);
            }
            let _ = self.listener.changed().await;
        }
    }

    pub fn push_live_page(&mut self, page: &ReplayPage) -> Result<DeliveryStatus, ReplayError> {
        let committed_head = self.listener.current().committed_head();
        if let Err(error) = self.validate_page(page, committed_head, ReplayPhase::Live) {
            self.post_stream_resync(CURSOR_INCONSISTENT_REASON)?;
            return Err(error);
        }
        let mut final_status = DeliveryStatus::Enqueued;
        for event in page.events() {
            let status = self
                .sender
                .try_send_event(event, page.range().through_inclusive())?;
            match status {
                DeliveryStatus::Enqueued => self.ingested_through = event.sequence(),
                DeliveryStatus::Duplicate => {}
                DeliveryStatus::Overflowed
                | DeliveryStatus::ResyncRequired
                | DeliveryStatus::Closed => {
                    self.phase = ReplayPhase::Closed;
                    return Ok(status);
                }
                DeliveryStatus::DroppedControl => unreachable!("event delivery is not a control"),
            }
            final_status = status;
        }
        Ok(final_status)
    }

    pub fn heartbeat(&self) -> Result<DeliveryStatus, ReplayError> {
        let observed = self.listener.current().committed_head();
        Ok(self.sender.try_send_heartbeat(SchemaU64::new(
            observed.get().max(self.ingested_through.get()),
        ))?)
    }

    pub fn post_stream_resync(&mut self, reason: &str) -> Result<(), ReplayError> {
        if self.phase == ReplayPhase::Closed {
            return Ok(());
        }
        let observed = self.listener.current().committed_head();
        let watermark = SchemaU64::new(observed.get().max(self.ingested_through.get()));
        self.sender.resync_required(
            reason,
            watermark,
            self.earliest_available_sequence
                .or_else(|| retained_earliest(watermark)),
        )?;
        self.phase = ReplayPhase::Closed;
        Ok(())
    }

    fn validate_page(
        &self,
        page: &ReplayPage,
        maximum_through: SchemaU64,
        expected_phase: ReplayPhase,
    ) -> Result<(), ReplayError> {
        if self.phase != expected_phase {
            return Err(ReplayError::invalid(ReplayErrorKind::InvalidPhase));
        }
        if page.run_id() != self.run_id {
            return Err(ReplayError::invalid(ReplayErrorKind::RunIdentityMismatch));
        }
        if page.range().after_exclusive() != self.ingested_through
            || page.range().through_inclusive().get() > maximum_through.get()
        {
            return Err(ReplayError::invalid(ReplayErrorKind::UnexpectedPage));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplayErrorKind {
    ArchiveFollowUnsupported,
    FutureCursor,
    RunIdentityMismatch,
    InvalidRetainedRange,
    InvalidPageRange,
    CapturedHeadBehindRange,
    PageTooLarge,
    NonDensePage,
    UnexpectedPage,
    IncompleteReplay,
    InvalidPhase,
    InvalidHeartbeatInterval,
    InvalidSubscriberLimits,
    CommitSignalInvalid,
    BodyNotDetached,
    Reader,
    Subscriber,
}

#[derive(Debug)]
pub enum ReplayError {
    Invalid {
        kind: ReplayErrorKind,
        requested: Option<SchemaU64>,
        committed_head: Option<SchemaU64>,
    },
    Reader(ReaderFailure),
    Subscriber(SubscriberError),
}

impl ReplayError {
    const fn invalid(kind: ReplayErrorKind) -> Self {
        Self::Invalid {
            kind,
            requested: None,
            committed_head: None,
        }
    }

    const fn future(requested: SchemaU64, committed_head: SchemaU64) -> Self {
        Self::Invalid {
            kind: ReplayErrorKind::FutureCursor,
            requested: Some(requested),
            committed_head: Some(committed_head),
        }
    }

    pub const fn kind(&self) -> ReplayErrorKind {
        match self {
            Self::Invalid { kind, .. } => *kind,
            Self::Reader(_) => ReplayErrorKind::Reader,
            Self::Subscriber(_) => ReplayErrorKind::Subscriber,
        }
    }

    pub const fn requested(&self) -> Option<SchemaU64> {
        match self {
            Self::Invalid { requested, .. } => *requested,
            Self::Reader(_) | Self::Subscriber(_) => None,
        }
    }

    pub const fn committed_head(&self) -> Option<SchemaU64> {
        match self {
            Self::Invalid { committed_head, .. } => *committed_head,
            Self::Reader(_) | Self::Subscriber(_) => None,
        }
    }

    pub const fn reader_failure(&self) -> Option<&ReaderFailure> {
        match self {
            Self::Reader(error) => Some(error),
            Self::Invalid { .. } | Self::Subscriber(_) => None,
        }
    }

    pub const fn status(&self) -> StatusCode {
        match self.kind() {
            ReplayErrorKind::FutureCursor => StatusCode::CONFLICT,
            ReplayErrorKind::ArchiveFollowUnsupported => StatusCode::METHOD_NOT_ALLOWED,
            ReplayErrorKind::RunIdentityMismatch
            | ReplayErrorKind::InvalidRetainedRange
            | ReplayErrorKind::InvalidPageRange
            | ReplayErrorKind::CapturedHeadBehindRange
            | ReplayErrorKind::PageTooLarge
            | ReplayErrorKind::NonDensePage
            | ReplayErrorKind::UnexpectedPage
            | ReplayErrorKind::IncompleteReplay
            | ReplayErrorKind::InvalidPhase
            | ReplayErrorKind::InvalidHeartbeatInterval
            | ReplayErrorKind::InvalidSubscriberLimits
            | ReplayErrorKind::CommitSignalInvalid
            | ReplayErrorKind::BodyNotDetached
            | ReplayErrorKind::Reader
            | ReplayErrorKind::Subscriber => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub const fn code(&self) -> &'static str {
        match self.kind() {
            ReplayErrorKind::ArchiveFollowUnsupported => "archive_follow_unsupported",
            ReplayErrorKind::FutureCursor => "cursor_ahead_of_head",
            ReplayErrorKind::RunIdentityMismatch => "sse_run_identity_mismatch",
            ReplayErrorKind::InvalidRetainedRange => "sse_retained_range_invalid",
            ReplayErrorKind::InvalidPageRange => "sse_page_range_invalid",
            ReplayErrorKind::CapturedHeadBehindRange => "sse_capture_behind_range",
            ReplayErrorKind::PageTooLarge => "sse_page_too_large",
            ReplayErrorKind::NonDensePage => "sse_page_not_dense",
            ReplayErrorKind::UnexpectedPage => "sse_page_unexpected",
            ReplayErrorKind::IncompleteReplay => "sse_replay_incomplete",
            ReplayErrorKind::InvalidPhase => "sse_phase_invalid",
            ReplayErrorKind::InvalidHeartbeatInterval => "sse_heartbeat_interval_invalid",
            ReplayErrorKind::InvalidSubscriberLimits => "sse_subscriber_limits_invalid",
            ReplayErrorKind::CommitSignalInvalid => "sse_commit_signal_invalid",
            ReplayErrorKind::BodyNotDetached => "sse_body_not_detached",
            ReplayErrorKind::Reader => "sse_reader_failed",
            ReplayErrorKind::Subscriber => "sse_subscriber_failed",
        }
    }
}

impl fmt::Display for ReplayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { .. } => formatter.write_str(self.code()),
            Self::Reader(error) => fmt::Display::fmt(error, formatter),
            Self::Subscriber(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for ReplayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Reader(error) => Some(error),
            Self::Subscriber(error) => Some(error),
            Self::Invalid { .. } => None,
        }
    }
}

impl From<SubscriberError> for ReplayError {
    fn from(error: SubscriberError) -> Self {
        Self::Subscriber(error)
    }
}

const fn retained_earliest(head: SchemaU64) -> Option<SchemaU64> {
    if head.get() == 0 {
        None
    } else {
        Some(SchemaU64::new(1))
    }
}
