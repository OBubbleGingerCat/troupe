use std::{error::Error, fmt};

use bytes::{Bytes, BytesMut};
use hyper::{
    HeaderMap, StatusCode,
    body::{Body, Frame},
    header::{CACHE_CONTROL, CONTENT_TYPE, HeaderName, HeaderValue},
};
use serde::Serialize;
use troupe_diagnostics_core::{event::DiagnosticEvent, id::CanonicalUuid, scalar::SchemaU64};

use crate::{
    query::reader::CapturedEvent,
    server::routes::{CachePolicy, RouteResponse},
};

pub const CONTROL_SCHEMA_VERSION: u8 = 1;
pub const SSE_CONTENT_TYPE: &str = "text/event-stream; charset=utf-8";
pub const SSE_CACHE_CONTROL: &str = "no-cache, no-transform";
pub const SSE_PROXY_BUFFERING_HEADER: &str = "x-accel-buffering";
pub const SSE_PROXY_BUFFERING_DISABLED: &str = "no";
pub const SSE_CONTROL_EVENT_NAMES: [&str; 5] = [
    "stream_ready",
    "heartbeat",
    "delivery_gap",
    "resync_required",
    "stream_closed",
];

pub const BUFFER_OVERFLOW_REASON: &str = "subscriber_buffer_overflow";
pub const CURSOR_UNAVAILABLE_REASON: &str = "cursor_unavailable";
pub const CURSOR_INCONSISTENT_REASON: &str = "cursor_inconsistent";
pub const PRODUCTION_FINISHED_REASON: &str = "production_finished";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedEvent {
    event: DiagnosticEvent,
    canonical_json: Bytes,
}

impl CommittedEvent {
    pub fn try_new(
        event: DiagnosticEvent,
        canonical_json: impl Into<Bytes>,
    ) -> Result<Self, FrameError> {
        let canonical_json = canonical_json.into();
        let encoded = serde_json::to_vec(&event).map_err(FrameError::Serialization)?;
        if encoded.as_slice() != canonical_json.as_ref() {
            return Err(FrameError::NonCanonicalEvent);
        }
        Ok(Self {
            event,
            canonical_json,
        })
    }

    pub const fn event(&self) -> &DiagnosticEvent {
        &self.event
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.event.header().run_id()
    }

    pub const fn sequence(&self) -> SchemaU64 {
        self.event.header().sequence()
    }

    pub fn canonical_json(&self) -> &[u8] {
        &self.canonical_json
    }
}

impl From<CapturedEvent> for CommittedEvent {
    fn from(value: CapturedEvent) -> Self {
        let (event, canonical_json) = value.into_parts();
        Self {
            event,
            canonical_json: Bytes::from(canonical_json),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SseFrameKind {
    DiagnosticEvent,
    StreamReady,
    Heartbeat,
    DeliveryGap,
    ResyncRequired,
    StreamClosed,
}

impl SseFrameKind {
    pub const fn event_name(self) -> &'static str {
        match self {
            Self::DiagnosticEvent => "diagnostic_event",
            Self::StreamReady => "stream_ready",
            Self::Heartbeat => "heartbeat",
            Self::DeliveryGap => "delivery_gap",
            Self::ResyncRequired => "resync_required",
            Self::StreamClosed => "stream_closed",
        }
    }

    pub const fn is_control(self) -> bool {
        !matches!(self, Self::DiagnosticEvent)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SseFrame {
    kind: SseFrameKind,
    id: Option<SchemaU64>,
    bytes: Bytes,
}

impl SseFrame {
    pub fn diagnostic_event(event: &CommittedEvent) -> Self {
        let sequence = event.sequence();
        Self {
            kind: SseFrameKind::DiagnosticEvent,
            id: Some(sequence),
            bytes: encode_frame(
                SseFrameKind::DiagnosticEvent.event_name(),
                Some(sequence),
                event.canonical_json(),
            ),
        }
    }

    pub fn stream_ready(
        run_id: CanonicalUuid,
        resume_after: SchemaU64,
        replay_through: SchemaU64,
    ) -> Result<Self, FrameError> {
        if resume_after.get() > replay_through.get() {
            return Err(FrameError::InvalidControl(
                "resume cursor is ahead of replay watermark",
            ));
        }
        control_frame(
            SseFrameKind::StreamReady,
            &StreamReadyPayload {
                control_schema_version: CONTROL_SCHEMA_VERSION,
                run_id,
                resume_after,
                replay_through,
            },
        )
    }

    pub fn heartbeat(
        run_id: CanonicalUuid,
        committed_watermark: SchemaU64,
    ) -> Result<Self, FrameError> {
        control_frame(
            SseFrameKind::Heartbeat,
            &HeartbeatPayload {
                control_schema_version: CONTROL_SCHEMA_VERSION,
                run_id,
                committed_watermark,
            },
        )
    }

    pub fn delivery_gap(
        run_id: CanonicalUuid,
        reason: &str,
        last_delivered_sequence: SchemaU64,
        committed_watermark: SchemaU64,
    ) -> Result<Self, FrameError> {
        validate_reason(reason)?;
        if last_delivered_sequence.get() > committed_watermark.get() {
            return Err(FrameError::InvalidControl(
                "last delivered sequence exceeds committed watermark",
            ));
        }
        control_frame(
            SseFrameKind::DeliveryGap,
            &DeliveryGapPayload {
                control_schema_version: CONTROL_SCHEMA_VERSION,
                run_id,
                reason,
                last_delivered_sequence,
                committed_watermark,
            },
        )
    }

    pub fn resync_required(
        run_id: CanonicalUuid,
        reason: &str,
        committed_watermark: SchemaU64,
        earliest_available_sequence: Option<SchemaU64>,
    ) -> Result<Self, FrameError> {
        validate_reason(reason)?;
        if earliest_available_sequence.is_some_and(|earliest| {
            earliest.get() == 0 || earliest.get() > committed_watermark.get()
        }) {
            return Err(FrameError::InvalidControl(
                "earliest available sequence is outside retained history",
            ));
        }
        control_frame(
            SseFrameKind::ResyncRequired,
            &ResyncRequiredPayload {
                control_schema_version: CONTROL_SCHEMA_VERSION,
                run_id,
                reason,
                committed_watermark,
                earliest_available_sequence,
            },
        )
    }

    pub fn stream_closed(
        run_id: CanonicalUuid,
        reason: &str,
        committed_watermark: SchemaU64,
    ) -> Result<Self, FrameError> {
        validate_reason(reason)?;
        control_frame(
            SseFrameKind::StreamClosed,
            &StreamClosedPayload {
                control_schema_version: CONTROL_SCHEMA_VERSION,
                run_id,
                reason,
                committed_watermark,
            },
        )
    }

    pub const fn kind(&self) -> SseFrameKind {
        self.kind
    }

    pub const fn event_name(&self) -> &'static str {
        self.kind.event_name()
    }

    pub const fn id(&self) -> Option<SchemaU64> {
        self.id
    }

    pub const fn advances_cursor(&self) -> bool {
        self.id.is_some()
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub const fn byte_len(&self) -> usize {
        self.bytes.len()
    }

    pub fn into_bytes(self) -> Bytes {
        self.bytes
    }

    pub fn into_http_frame(self) -> Frame<Bytes> {
        Frame::data(self.into_bytes())
    }
}

#[derive(Debug)]
pub enum FrameError {
    NonCanonicalEvent,
    InvalidControl(&'static str),
    Serialization(serde_json::Error),
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonCanonicalEvent => {
                formatter.write_str("diagnostic event bytes are not canonical JSON")
            }
            Self::InvalidControl(message) => formatter.write_str(message),
            Self::Serialization(error) => {
                write!(formatter, "could not encode SSE payload: {error}")
            }
        }
    }
}

impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Serialization(error) => Some(error),
            Self::NonCanonicalEvent | Self::InvalidControl(_) => None,
        }
    }
}

pub fn sse_response_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(SSE_CONTENT_TYPE));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static(SSE_CACHE_CONTROL));
    headers.insert(
        SSE_PROXY_BUFFERING_HEADER,
        HeaderValue::from_static(SSE_PROXY_BUFFERING_DISABLED),
    );
    headers
}

pub fn sse_route_response<B>(body: B) -> RouteResponse
where
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Error + Send + Sync + 'static,
{
    RouteResponse::stream(StatusCode::OK, body)
        .with_header(CONTENT_TYPE, HeaderValue::from_static(SSE_CONTENT_TYPE))
        .with_header(
            HeaderName::from_static(SSE_PROXY_BUFFERING_HEADER),
            HeaderValue::from_static(SSE_PROXY_BUFFERING_DISABLED),
        )
        .with_cache_policy(CachePolicy::NoCacheNoTransform)
}

#[derive(Serialize)]
struct StreamReadyPayload {
    control_schema_version: u8,
    run_id: CanonicalUuid,
    resume_after: SchemaU64,
    replay_through: SchemaU64,
}

#[derive(Serialize)]
struct HeartbeatPayload {
    control_schema_version: u8,
    run_id: CanonicalUuid,
    committed_watermark: SchemaU64,
}

#[derive(Serialize)]
struct DeliveryGapPayload<'reason> {
    control_schema_version: u8,
    run_id: CanonicalUuid,
    reason: &'reason str,
    last_delivered_sequence: SchemaU64,
    committed_watermark: SchemaU64,
}

#[derive(Serialize)]
struct ResyncRequiredPayload<'reason> {
    control_schema_version: u8,
    run_id: CanonicalUuid,
    reason: &'reason str,
    committed_watermark: SchemaU64,
    earliest_available_sequence: Option<SchemaU64>,
}

#[derive(Serialize)]
struct StreamClosedPayload<'reason> {
    control_schema_version: u8,
    run_id: CanonicalUuid,
    reason: &'reason str,
    committed_watermark: SchemaU64,
}

fn validate_reason(reason: &str) -> Result<(), FrameError> {
    if reason.is_empty() {
        Err(FrameError::InvalidControl(
            "control reason must be nonempty",
        ))
    } else {
        Ok(())
    }
}

fn control_frame<T>(kind: SseFrameKind, payload: &T) -> Result<SseFrame, FrameError>
where
    T: Serialize + ?Sized,
{
    debug_assert!(kind.is_control());
    let data = serde_json::to_vec(payload).map_err(FrameError::Serialization)?;
    Ok(SseFrame {
        kind,
        id: None,
        bytes: encode_frame(kind.event_name(), None, &data),
    })
}

fn encode_frame(event_name: &str, id: Option<SchemaU64>, data: &[u8]) -> Bytes {
    let id_bytes = id.map(|value| value.get().to_string());
    let capacity = "event: ".len()
        + event_name.len()
        + 1
        + id_bytes
            .as_ref()
            .map_or(0, |value| "id: ".len() + value.len() + 1)
        + "data: ".len()
        + data.len()
        + 2;
    let mut frame = BytesMut::with_capacity(capacity);
    frame.extend_from_slice(b"event: ");
    frame.extend_from_slice(event_name.as_bytes());
    frame.extend_from_slice(b"\n");
    if let Some(id) = id_bytes {
        frame.extend_from_slice(b"id: ");
        frame.extend_from_slice(id.as_bytes());
        frame.extend_from_slice(b"\n");
    }
    frame.extend_from_slice(b"data: ");
    frame.extend_from_slice(data);
    frame.extend_from_slice(b"\n\n");
    frame.freeze()
}
