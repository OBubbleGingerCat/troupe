#![allow(dead_code)] // D07 wires the streaming client into the application entry point.

use std::{fmt, time::Duration};

use reqwest::{Response, header::ACCEPT};
use tokio_util::sync::CancellationToken;
use troupe_diagnostics_core::{event::DiagnosticEvent, id::CanonicalUuid, scalar::SchemaU64};
use troupe_diagnostics_runtime::server::{
    query::EVENTS_PATH,
    sse::{
        cursor::LAST_EVENT_ID_HEADER,
        frame::{DecodedSseControl, SSE_CONTENT_TYPE, SseFrameKind, decode_control_payload},
    },
};

use super::{
    args::{EventStart, EventsArgs, EventsFormat},
    events_finite::{self, EventsError},
    http_client::{DiagnosticHttpClient, HttpClientError, HttpClientErrorCode},
    resolver::{ResolvedDiagnosticTarget, ResolverError, resolve},
};

const MAX_SSE_FRAME_BYTES: usize = 64 * 1024 * 1024;
const RECONNECT_DELAY: Duration = Duration::from_millis(100);

pub(crate) trait FollowOutput {
    type Error: fmt::Display;

    /// Writes one complete event record. Implementations must not split a
    /// record around cancellation handling.
    fn write_stdout_record(&mut self, record: &str) -> Result<(), Self::Error>;

    fn write_stderr_line(&mut self, line: &str) -> Result<(), Self::Error>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FollowTermination {
    StreamClosed,
    Interrupted,
}

impl FollowTermination {
    pub(crate) const fn exit_code(self) -> u8 {
        match self {
            Self::StreamClosed => 0,
            Self::Interrupted => 130,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FollowErrorCode {
    FollowRequired,
    Resolve,
    ArchiveUnsupported,
    Finite,
    Http,
    UnexpectedStatus,
    InvalidContentType,
    InvalidStream,
    RunIdentityMismatch,
    SequenceGap,
    ResyncRequired,
    Output,
}

impl FollowErrorCode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::FollowRequired => "diagnostic_events_follow.follow_required",
            Self::Resolve => "diagnostic_events_follow.resolve",
            Self::ArchiveUnsupported => "diagnostic_events_follow.archive_unsupported",
            Self::Finite => "diagnostic_events_follow.finite",
            Self::Http => "diagnostic_events_follow.http",
            Self::UnexpectedStatus => "diagnostic_events_follow.unexpected_status",
            Self::InvalidContentType => "diagnostic_events_follow.invalid_content_type",
            Self::InvalidStream => "diagnostic_events_follow.invalid_stream",
            Self::RunIdentityMismatch => "diagnostic_events_follow.run_identity_mismatch",
            Self::SequenceGap => "diagnostic_events_follow.sequence_gap",
            Self::ResyncRequired => "diagnostic_events_follow.resync_required",
            Self::Output => "diagnostic_events_follow.output",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FollowError {
    code: FollowErrorCode,
    detail: String,
}

impl FollowError {
    fn new(code: FollowErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    fn resolver(error: ResolverError) -> Self {
        Self::new(FollowErrorCode::Resolve, error.to_string())
    }

    fn finite(error: EventsError) -> Self {
        Self::new(FollowErrorCode::Finite, error.to_string())
    }

    fn http(error: impl fmt::Display) -> Self {
        Self::new(FollowErrorCode::Http, error.to_string())
    }

    fn output(error: impl fmt::Display) -> Self {
        Self::new(FollowErrorCode::Output, error.to_string())
    }

    pub(crate) const fn code(&self) -> FollowErrorCode {
        self.code
    }
}

impl fmt::Display for FollowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.detail)
    }
}

impl std::error::Error for FollowError {}

pub(crate) async fn execute<O>(
    arguments: EventsArgs,
    output: &mut O,
    cancellation: CancellationToken,
) -> Result<FollowTermination, FollowError>
where
    O: FollowOutput,
{
    let (target, start, follow, format) = arguments.into_parts();
    if !follow {
        return Err(FollowError::new(
            FollowErrorCode::FollowRequired,
            "the streaming client requires --follow",
        ));
    }

    let target = tokio::select! {
        () = cancellation.cancelled() => return Ok(FollowTermination::Interrupted),
        target = resolve(target) => target.map_err(FollowError::resolver)?,
    };
    let ResolvedDiagnosticTarget::Live(client) = target else {
        return Err(FollowError::new(
            FollowErrorCode::ArchiveUnsupported,
            "archived Runs do not provide a live event stream",
        ));
    };
    let run_id = client.run_id();
    let initial = tokio::select! {
        () = cancellation.cancelled() => return Ok(FollowTermination::Interrupted),
        initial = events_finite::query(ResolvedDiagnosticTarget::Live(client.clone()), start) => {
            initial.map_err(FollowError::finite)?
        }
    };
    let mut last_output = None;
    for event in initial.events() {
        if cancellation.is_cancelled() {
            return Ok(FollowTermination::Interrupted);
        }
        ensure_initial_identity(event, run_id, last_output)?;
        write_event(output, event, format)?;
        last_output = Some(EventCursor::from_event(event));
        if cancellation.is_cancelled() {
            return Ok(FollowTermination::Interrupted);
        }
    }

    let cursor = EventCursor::new(run_id, initial.captured_watermark());
    let mut state = FollowState {
        cursor,
        adopt_connection_head: is_tail_zero(start),
    };
    follow_stream(&client, format, output, &cancellation, &mut state).await
}

async fn follow_stream<O>(
    client: &DiagnosticHttpClient,
    format: EventsFormat,
    output: &mut O,
    cancellation: &CancellationToken,
    state: &mut FollowState,
) -> Result<FollowTermination, FollowError>
where
    O: FollowOutput,
{
    let mut reconnect = false;
    loop {
        if cancellation.is_cancelled() {
            return Ok(FollowTermination::Interrupted);
        }

        let identity = tokio::select! {
            () = cancellation.cancelled() => return Ok(FollowTermination::Interrupted),
            identity = client.revalidate_identity() => identity,
        };
        match identity {
            Ok(()) => {}
            Err(error) if is_temporary_identity_failure(&error) => {
                reconnect_notice(output, state.cursor, "identity probe failed")?;
                if wait_to_reconnect(cancellation).await {
                    return Ok(FollowTermination::Interrupted);
                }
                reconnect = true;
                continue;
            }
            Err(error) => {
                return Err(FollowError::new(
                    FollowErrorCode::RunIdentityMismatch,
                    format!("live endpoint identity changed or became invalid: {error}"),
                ));
            }
        }

        let requested = state.cursor;
        let response = match connect(client, requested, reconnect, cancellation).await? {
            ConnectResult::Connected(response) => response,
            ConnectResult::Retry(reason) => {
                reconnect_notice(output, requested, reason)?;
                if wait_to_reconnect(cancellation).await {
                    return Ok(FollowTermination::Interrupted);
                }
                reconnect = true;
                continue;
            }
            ConnectResult::Interrupted => return Ok(FollowTermination::Interrupted),
        };

        match consume_connection(response, requested, format, output, cancellation, state).await? {
            ConnectionOutcome::Closed => return Ok(FollowTermination::StreamClosed),
            ConnectionOutcome::Interrupted => return Ok(FollowTermination::Interrupted),
            ConnectionOutcome::Reconnect(reason) => {
                reconnect_notice(output, state.cursor, &reason)?;
                if wait_to_reconnect(cancellation).await {
                    return Ok(FollowTermination::Interrupted);
                }
                reconnect = true;
            }
        }
    }
}

async fn connect(
    client: &DiagnosticHttpClient,
    cursor: EventCursor,
    reconnect: bool,
    cancellation: &CancellationToken,
) -> Result<ConnectResult, FollowError> {
    let path = format!("{EVENTS_PATH}?after={}", cursor.sequence.get());
    let mut request = client
        .get(&path)
        .map_err(FollowError::http)?
        .header(ACCEPT, SSE_CONTENT_TYPE);
    if reconnect {
        request = request.header(LAST_EVENT_ID_HEADER, cursor.sequence.get().to_string());
    }
    let response = tokio::select! {
        () = cancellation.cancelled() => return Ok(ConnectResult::Interrupted),
        response = request.send() => response,
    };
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            return Ok(ConnectResult::Retry(format!(
                "stream request failed: {error}"
            )));
        }
    };
    let status = response.status();
    if status.is_server_error() {
        return Ok(ConnectResult::Retry(format!(
            "stream endpoint returned temporary HTTP {status}"
        )));
    }
    if status.as_u16() != 200 {
        return Err(FollowError::new(
            FollowErrorCode::UnexpectedStatus,
            format!("stream endpoint returned HTTP {status}"),
        ));
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    if !content_type.is_some_and(|value| value.eq_ignore_ascii_case(SSE_CONTENT_TYPE)) {
        return Err(FollowError::new(
            FollowErrorCode::InvalidContentType,
            "stream response is not text/event-stream; charset=utf-8",
        ));
    }
    Ok(ConnectResult::Connected(response))
}

async fn consume_connection<O>(
    mut response: Response,
    requested: EventCursor,
    format: EventsFormat,
    output: &mut O,
    cancellation: &CancellationToken,
    state: &mut FollowState,
) -> Result<ConnectionOutcome, FollowError>
where
    O: FollowOutput,
{
    let mut decoder = SseEnvelopeDecoder::default();
    let mut ready = false;
    loop {
        let chunk = tokio::select! {
            () = cancellation.cancelled() => return Ok(ConnectionOutcome::Interrupted),
            chunk = response.chunk() => chunk,
        };
        let chunk = match chunk {
            Ok(Some(chunk)) => chunk,
            Ok(None) => {
                return Ok(ConnectionOutcome::Reconnect(
                    "stream ended without stream_closed".to_owned(),
                ));
            }
            Err(error) => {
                return Ok(ConnectionOutcome::Reconnect(format!(
                    "stream body failed: {error}"
                )));
            }
        };
        decoder.push(&chunk)?;
        while let Some(frame) = decoder.next_frame()? {
            match frame.kind {
                SseFrameKind::DiagnosticEvent => {
                    if !ready {
                        return invalid_stream("diagnostic event arrived before stream_ready");
                    }
                    let event = frame.decode_event()?;
                    state.consume_event(event, format, output)?;
                }
                kind => {
                    if frame.id.is_some() {
                        return invalid_stream("SSE controls must not carry an event ID");
                    }
                    let control = decode_control_payload(kind, &frame.data).map_err(|error| {
                        FollowError::new(
                            FollowErrorCode::InvalidStream,
                            format!("invalid {} control: {error}", kind.event_name()),
                        )
                    })?;
                    if control.run_id() != state.cursor.run_id {
                        return Err(FollowError::new(
                            FollowErrorCode::RunIdentityMismatch,
                            format!(
                                "stream control Run {} differs from resolved Run {}",
                                control.run_id(),
                                state.cursor.run_id
                            ),
                        ));
                    }
                    match control {
                        DecodedSseControl::StreamReady(control) => {
                            if ready {
                                return invalid_stream("stream_ready appeared more than once");
                            }
                            if control.resume_after() != requested.sequence {
                                return invalid_stream(
                                    "stream_ready resume cursor differs from the request cursor",
                                );
                            }
                            if state.adopt_connection_head {
                                state.cursor.sequence = control.replay_through();
                                state.adopt_connection_head = false;
                            }
                            ready = true;
                        }
                        DecodedSseControl::Heartbeat(control) => {
                            require_ready(ready, "heartbeat")?;
                            if control.committed_watermark().get() < state.cursor.sequence.get() {
                                return invalid_stream(
                                    "heartbeat watermark is behind the client cursor",
                                );
                            }
                        }
                        DecodedSseControl::DeliveryGap(control) => {
                            require_ready(ready, "delivery_gap")?;
                            if control.committed_watermark().get() < state.cursor.sequence.get() {
                                return invalid_stream(
                                    "delivery_gap watermark is behind the client cursor",
                                );
                            }
                            return Ok(ConnectionOutcome::Reconnect(format!(
                                "delivery_gap ({})",
                                control.reason()
                            )));
                        }
                        DecodedSseControl::ResyncRequired(control) => {
                            return Err(FollowError::new(
                                FollowErrorCode::ResyncRequired,
                                format!(
                                    "server cannot replay cursor {} at watermark {}: {}",
                                    state.cursor.sequence.get(),
                                    control.committed_watermark().get(),
                                    control.reason()
                                ),
                            ));
                        }
                        DecodedSseControl::StreamClosed(control) => {
                            require_ready(ready, "stream_closed")?;
                            if control.committed_watermark() != state.cursor.sequence {
                                return Err(FollowError::new(
                                    FollowErrorCode::SequenceGap,
                                    format!(
                                        "stream closed at watermark {} after client cursor {}",
                                        control.committed_watermark().get(),
                                        state.cursor.sequence.get()
                                    ),
                                ));
                            }
                            return Ok(ConnectionOutcome::Closed);
                        }
                    }
                }
            }
            if cancellation.is_cancelled() {
                return Ok(ConnectionOutcome::Interrupted);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EventCursor {
    run_id: CanonicalUuid,
    sequence: SchemaU64,
}

impl EventCursor {
    const fn new(run_id: CanonicalUuid, sequence: SchemaU64) -> Self {
        Self { run_id, sequence }
    }

    fn from_event(event: &DiagnosticEvent) -> Self {
        Self::new(event.header().run_id(), event.header().sequence())
    }
}

struct FollowState {
    cursor: EventCursor,
    adopt_connection_head: bool,
}

impl FollowState {
    fn consume_event<O>(
        &mut self,
        event: DiagnosticEvent,
        format: EventsFormat,
        output: &mut O,
    ) -> Result<(), FollowError>
    where
        O: FollowOutput,
    {
        let identity = EventCursor::from_event(&event);
        if identity.run_id != self.cursor.run_id {
            return Err(FollowError::new(
                FollowErrorCode::RunIdentityMismatch,
                format!(
                    "stream event Run {} differs from resolved Run {}",
                    identity.run_id, self.cursor.run_id
                ),
            ));
        }
        if identity.sequence.get() <= self.cursor.sequence.get() {
            return Ok(());
        }
        let expected = self.cursor.sequence.get().checked_add(1).ok_or_else(|| {
            FollowError::new(
                FollowErrorCode::SequenceGap,
                "an event followed the maximum u64 cursor",
            )
        })?;
        if identity.sequence.get() != expected {
            return Err(FollowError::new(
                FollowErrorCode::SequenceGap,
                format!(
                    "stream event sequence {} followed cursor {}",
                    identity.sequence.get(),
                    self.cursor.sequence.get()
                ),
            ));
        }
        write_event(output, &event, format)?;
        self.cursor = identity;
        Ok(())
    }
}

enum ConnectResult {
    Connected(Response),
    Retry(String),
    Interrupted,
}

enum ConnectionOutcome {
    Closed,
    Reconnect(String),
    Interrupted,
}

#[derive(Default)]
struct SseEnvelopeDecoder {
    buffer: Vec<u8>,
}

impl SseEnvelopeDecoder {
    fn push(&mut self, chunk: &[u8]) -> Result<(), FollowError> {
        self.buffer.len().checked_add(chunk.len()).ok_or_else(|| {
            FollowError::new(
                FollowErrorCode::InvalidStream,
                "SSE frame length overflowed",
            )
        })?;
        self.buffer.extend_from_slice(chunk);
        if find_frame_end(&self.buffer).is_none() && self.buffer.len() > MAX_SSE_FRAME_BYTES {
            return invalid_stream("SSE frame exceeds the size limit");
        }
        Ok(())
    }

    fn next_frame(&mut self) -> Result<Option<RawSseFrame>, FollowError> {
        let Some((content_end, framed_end)) = find_frame_end(&self.buffer) else {
            if self.buffer.len() > MAX_SSE_FRAME_BYTES {
                return invalid_stream("SSE frame exceeds the size limit");
            }
            return Ok(None);
        };
        if content_end > MAX_SSE_FRAME_BYTES {
            return invalid_stream("SSE frame exceeds the size limit");
        }
        let tail = self.buffer.split_off(framed_end);
        let mut frame = std::mem::replace(&mut self.buffer, tail);
        frame.truncate(content_end);
        parse_sse_frame(&frame).map(Some)
    }
}

struct RawSseFrame {
    kind: SseFrameKind,
    id: Option<SchemaU64>,
    data: Vec<u8>,
}

impl RawSseFrame {
    fn decode_event(self) -> Result<DiagnosticEvent, FollowError> {
        let Some(id) = self.id else {
            return invalid_stream("diagnostic event is missing its SSE ID");
        };
        let event: DiagnosticEvent = serde_json::from_slice(&self.data).map_err(|error| {
            FollowError::new(
                FollowErrorCode::InvalidStream,
                format!("diagnostic event payload is invalid: {error}"),
            )
        })?;
        let canonical = serde_json::to_vec(&event).map_err(|error| {
            FollowError::new(
                FollowErrorCode::InvalidStream,
                format!("diagnostic event could not be re-encoded: {error}"),
            )
        })?;
        if canonical != self.data {
            return invalid_stream("diagnostic event payload is not canonical JSON");
        }
        if id != event.header().sequence() {
            return invalid_stream("SSE event ID differs from canonical event sequence");
        }
        Ok(event)
    }
}

fn find_frame_end(bytes: &[u8]) -> Option<(usize, usize)> {
    let lf = bytes
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (index, index + 2));
    let crlf = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, index + 4));
    match (lf, crlf) {
        (Some(left), Some(right)) => Some(if left.0 <= right.0 { left } else { right }),
        (Some(found), None) | (None, Some(found)) => Some(found),
        (None, None) => None,
    }
}

fn parse_sse_frame(bytes: &[u8]) -> Result<RawSseFrame, FollowError> {
    let mut lines = bytes
        .split(|byte| *byte == b'\n')
        .map(strip_carriage_return);
    let event_line = lines
        .next()
        .ok_or_else(|| FollowError::new(FollowErrorCode::InvalidStream, "empty SSE frame"))?;
    let event_name = event_line.strip_prefix(b"event: ").ok_or_else(|| {
        FollowError::new(
            FollowErrorCode::InvalidStream,
            "SSE frame must begin with an event field",
        )
    })?;
    let event_name = std::str::from_utf8(event_name).map_err(|_| {
        FollowError::new(FollowErrorCode::InvalidStream, "non-UTF-8 SSE event name")
    })?;
    let kind = SseFrameKind::from_event_name(event_name).ok_or_else(|| {
        FollowError::new(
            FollowErrorCode::InvalidStream,
            format!("unknown SSE event name {event_name:?}"),
        )
    })?;

    let second = lines.next().ok_or_else(|| {
        FollowError::new(
            FollowErrorCode::InvalidStream,
            "SSE frame is missing its data field",
        )
    })?;
    let (id, data_line) = if let Some(id) = second.strip_prefix(b"id: ") {
        let id = parse_sse_id(id)?;
        let data = lines.next().ok_or_else(|| {
            FollowError::new(
                FollowErrorCode::InvalidStream,
                "SSE frame is missing its data field",
            )
        })?;
        (Some(id), data)
    } else {
        (None, second)
    };
    let data = data_line.strip_prefix(b"data: ").ok_or_else(|| {
        FollowError::new(
            FollowErrorCode::InvalidStream,
            "SSE frame has an invalid data field",
        )
    })?;
    if lines.next().is_some() {
        return invalid_stream("SSE frame contains unsupported extra fields");
    }
    if matches!(kind, SseFrameKind::DiagnosticEvent) != id.is_some() {
        return invalid_stream("only diagnostic events may carry an SSE ID");
    }
    Ok(RawSseFrame {
        kind,
        id,
        data: data.to_vec(),
    })
}

fn strip_carriage_return(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn parse_sse_id(bytes: &[u8]) -> Result<SchemaU64, FollowError> {
    let value = std::str::from_utf8(bytes)
        .map_err(|_| FollowError::new(FollowErrorCode::InvalidStream, "non-UTF-8 SSE ID"))?;
    if value.is_empty() || (value.len() > 1 && value.starts_with('0')) {
        return invalid_stream("SSE ID is not a canonical decimal u64");
    }
    value.parse::<u64>().map(SchemaU64::new).map_err(|_| {
        FollowError::new(
            FollowErrorCode::InvalidStream,
            "SSE ID is not a canonical decimal u64",
        )
    })
}

fn ensure_initial_identity(
    event: &DiagnosticEvent,
    run_id: CanonicalUuid,
    previous: Option<EventCursor>,
) -> Result<(), FollowError> {
    let current = EventCursor::from_event(event);
    if current.run_id != run_id {
        return Err(FollowError::new(
            FollowErrorCode::RunIdentityMismatch,
            "finite event belongs to a different Run",
        ));
    }
    if previous.is_some_and(|previous| previous.sequence.get() >= current.sequence.get()) {
        return Err(FollowError::new(
            FollowErrorCode::SequenceGap,
            "finite events are not strictly increasing",
        ));
    }
    Ok(())
}

fn write_event<O>(
    output: &mut O,
    event: &DiagnosticEvent,
    format: EventsFormat,
) -> Result<(), FollowError>
where
    O: FollowOutput,
{
    let mut record = match format {
        EventsFormat::Jsonl => serde_json::to_string(event),
        EventsFormat::Human => serde_json::to_string_pretty(event),
    }
    .map_err(|error| {
        FollowError::new(
            FollowErrorCode::Output,
            format!("could not render diagnostic event: {error}"),
        )
    })?;
    record.push('\n');
    output
        .write_stdout_record(&record)
        .map_err(FollowError::output)
}

fn reconnect_notice<O>(
    output: &mut O,
    cursor: EventCursor,
    reason: impl fmt::Display,
) -> Result<(), FollowError>
where
    O: FollowOutput,
{
    output
        .write_stderr_line(&format!(
            "diagnostic events stream reconnecting after {}: {reason}",
            cursor.sequence.get()
        ))
        .map_err(FollowError::output)
}

async fn wait_to_reconnect(cancellation: &CancellationToken) -> bool {
    tokio::select! {
        () = cancellation.cancelled() => true,
        () = tokio::time::sleep(RECONNECT_DELAY) => false,
    }
}

fn is_tail_zero(start: EventStart) -> bool {
    matches!(start, EventStart::Tail(count) if count.get() == 0)
}

fn is_temporary_identity_failure(error: &HttpClientError) -> bool {
    matches!(error.code(), HttpClientErrorCode::Transport)
}

fn require_ready(ready: bool, name: &str) -> Result<(), FollowError> {
    if ready {
        Ok(())
    } else {
        invalid_stream(format!("{name} arrived before stream_ready"))
    }
}

fn invalid_stream<T>(detail: impl Into<String>) -> Result<T, FollowError> {
    Err(FollowError::new(FollowErrorCode::InvalidStream, detail))
}
