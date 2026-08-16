#![allow(dead_code)] // D07 wires this finite command into the CLI dispatcher.

use std::{
    fmt::{self, Write as _},
    time::Duration,
};

use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use troupe_diagnostics_core::{event::DiagnosticEvent, id::CanonicalUuid, scalar::SchemaU64};
use troupe_diagnostics_runtime::{
    query::events::FiniteEventQuery,
    server::query::{EVENTS_PATH, encode_events_response},
};

use super::{
    args::{EventStart, EventsArgs, EventsFormat},
    http_client::{DiagnosticHttpClient, HttpClientError},
    resolver::{ResolvedDiagnosticTarget, ResolverError, resolve},
};

const EVENTS_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_EVENTS_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const EVENTS_API_SCHEMA_VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EventsErrorCode {
    Resolve,
    FollowUnsupported,
    Http,
    UnexpectedStatus,
    ResponseTooLarge,
    InvalidResponse,
    IncompatibleResponse,
    RunIdentityMismatch,
    Archive,
    TaskFailed,
}

impl EventsErrorCode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Resolve => "diagnostic_events_cli.resolve",
            Self::FollowUnsupported => "diagnostic_events_cli.follow_requires_streaming",
            Self::Http => "diagnostic_events_cli.http",
            Self::UnexpectedStatus => "diagnostic_events_cli.unexpected_status",
            Self::ResponseTooLarge => "diagnostic_events_cli.response_too_large",
            Self::InvalidResponse => "diagnostic_events_cli.invalid_response",
            Self::IncompatibleResponse => "diagnostic_events_cli.incompatible_response",
            Self::RunIdentityMismatch => "diagnostic_events_cli.run_identity_mismatch",
            Self::Archive => "diagnostic_events_cli.archive",
            Self::TaskFailed => "diagnostic_events_cli.task_failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EventsError {
    code: EventsErrorCode,
    detail: String,
}

impl EventsError {
    fn new(code: EventsErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    fn resolver(error: ResolverError) -> Self {
        Self::new(EventsErrorCode::Resolve, error.to_string())
    }

    fn http(error: HttpClientError) -> Self {
        Self::new(EventsErrorCode::Http, error.to_string())
    }

    pub(crate) const fn code(&self) -> EventsErrorCode {
        self.code
    }
}

impl fmt::Display for EventsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.detail)
    }
}

impl std::error::Error for EventsError {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct EventsResponseV1 {
    api_schema_version: u8,
    run_id: CanonicalUuid,
    captured_watermark: SchemaU64,
    events: Vec<DiagnosticEvent>,
    next_after: Option<SchemaU64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EventsDocument {
    response: EventsResponseV1,
}

impl EventsDocument {
    pub(crate) fn render(&self, format: EventsFormat) -> String {
        match format {
            EventsFormat::Human => self.render_human(),
            EventsFormat::Jsonl => self.render_jsonl(),
        }
    }

    fn render_jsonl(&self) -> String {
        let mut output = String::new();
        for event in &self.response.events {
            let line = serde_json::to_string(event)
                .expect("a validated typed diagnostic event is serializable");
            output.push_str(&line);
            output.push('\n');
        }
        output
    }

    fn render_human(&self) -> String {
        let value = serde_json::to_value(&self.response)
            .expect("a validated typed event response is serializable");
        let mut output = String::new();
        write_human(&value, &mut output, 0);
        output
    }
}

pub(crate) async fn execute(arguments: EventsArgs) -> Result<String, EventsError> {
    let (target, start, follow, format) = arguments.into_parts();
    if follow {
        return Err(EventsError::new(
            EventsErrorCode::FollowUnsupported,
            "finite events cannot execute a follow request; D10 owns streaming",
        ));
    }
    let target = resolve(target).await.map_err(EventsError::resolver)?;
    query(target, start)
        .await
        .map(|events| events.render(format))
}

pub(crate) async fn query(
    target: ResolvedDiagnosticTarget,
    start: EventStart,
) -> Result<EventsDocument, EventsError> {
    match target {
        ResolvedDiagnosticTarget::Live(client) => query_live(&client, start).await,
        ResolvedDiagnosticTarget::Archive(mut archive) => {
            let expected_run_id = archive.run_id();
            tokio::task::spawn_blocking(move || {
                let bytes = {
                    let captured = archive.capture().map_err(|error| {
                        EventsError::new(
                            EventsErrorCode::Archive,
                            format!("cannot capture archive events: {error}"),
                        )
                    })?;
                    encode_events_response(expected_run_id, &captured, finite_query(start))
                        .map_err(|error| {
                            EventsError::new(
                                EventsErrorCode::Archive,
                                format!("cannot query archive events: {error}"),
                            )
                        })?
                };
                decode_events_response(&bytes, expected_run_id, start)
            })
            .await
            .map_err(|error| {
                EventsError::new(
                    EventsErrorCode::TaskFailed,
                    format!("archive events task failed: {error}"),
                )
            })?
        }
    }
}

async fn query_live(
    client: &DiagnosticHttpClient,
    start: EventStart,
) -> Result<EventsDocument, EventsError> {
    let expected_run_id = client.run_id();
    let bytes = fetch_events_bytes(client, start).await?;
    let events = decode_events_response(&bytes, expected_run_id, start)?;

    // Bind the finite response to the same resolved endpoint after the body is
    // decoded so an endpoint replacement cannot pass a mixed capture.
    client
        .revalidate_identity()
        .await
        .map_err(EventsError::http)?;
    Ok(events)
}

async fn fetch_events_bytes(
    client: &DiagnosticHttpClient,
    start: EventStart,
) -> Result<Vec<u8>, EventsError> {
    let path = request_path(start);
    let mut response = client
        .get(&path)
        .map_err(EventsError::http)?
        .header(ACCEPT, "application/json")
        .timeout(EVENTS_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|error| {
            EventsError::new(
                EventsErrorCode::Http,
                format!("events request failed: {error}"),
            )
        })?;

    if response.status().as_u16() != 200 {
        return Err(EventsError::new(
            EventsErrorCode::UnexpectedStatus,
            format!("events endpoint returned HTTP {}", response.status()),
        ));
    }
    let is_json = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|media_type| media_type.trim() == "application/json")
        });
    if !is_json {
        return Err(EventsError::new(
            EventsErrorCode::InvalidResponse,
            "events response is not application/json",
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_EVENTS_RESPONSE_BYTES as u64)
    {
        return Err(EventsError::new(
            EventsErrorCode::ResponseTooLarge,
            "events response exceeds the size limit",
        ));
    }

    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        EventsError::new(
            EventsErrorCode::Http,
            format!("events response body failed: {error}"),
        )
    })? {
        let next_length = bytes.len().checked_add(chunk.len()).ok_or_else(|| {
            EventsError::new(
                EventsErrorCode::ResponseTooLarge,
                "events response length overflowed",
            )
        })?;
        if next_length > MAX_EVENTS_RESPONSE_BYTES {
            return Err(EventsError::new(
                EventsErrorCode::ResponseTooLarge,
                "events response exceeds the size limit",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn request_path(start: EventStart) -> String {
    match start {
        EventStart::Tail(count) => format!("{EVENTS_PATH}?tail={}", count.get()),
        EventStart::After(after) => format!("{EVENTS_PATH}?after={}", after.get()),
    }
}

fn finite_query(start: EventStart) -> FiniteEventQuery {
    match start {
        EventStart::Tail(count) => FiniteEventQuery::tail(SchemaU64::new(count.get())),
        EventStart::After(after) => FiniteEventQuery::after(SchemaU64::new(after.get())),
    }
}

pub(crate) fn decode_events_response(
    bytes: &[u8],
    expected_run_id: CanonicalUuid,
    start: EventStart,
) -> Result<EventsDocument, EventsError> {
    if bytes.len() > MAX_EVENTS_RESPONSE_BYTES {
        return Err(EventsError::new(
            EventsErrorCode::ResponseTooLarge,
            "events response exceeds the size limit",
        ));
    }
    let response: EventsResponseV1 = serde_json::from_slice(bytes).map_err(|error| {
        EventsError::new(
            EventsErrorCode::InvalidResponse,
            format!("events response is invalid v1 JSON: {error}"),
        )
    })?;
    validate_events_response(&response, expected_run_id, start)?;
    Ok(EventsDocument { response })
}

fn validate_events_response(
    response: &EventsResponseV1,
    expected_run_id: CanonicalUuid,
    start: EventStart,
) -> Result<(), EventsError> {
    if response.api_schema_version != EVENTS_API_SCHEMA_VERSION {
        return Err(EventsError::new(
            EventsErrorCode::IncompatibleResponse,
            format!(
                "events API schema version {} is incompatible with {}",
                response.api_schema_version, EVENTS_API_SCHEMA_VERSION
            ),
        ));
    }
    ensure_run_id("run_id", response.run_id, expected_run_id)?;
    if response.next_after.is_some() {
        return invalid_field("next_after", "must be null for a finite response");
    }

    let watermark = response.captured_watermark.get();
    let (first, expected_count) = expected_event_range(start, watermark);
    let actual_count = u64::try_from(response.events.len()).map_err(|_| {
        invalid_response("events length exceeds the unsigned 64-bit integer domain")
    })?;
    if actual_count != expected_count {
        return invalid_field(
            "events",
            &format!(
                "contains {actual_count} records but the captured range requires {expected_count}"
            ),
        );
    }

    for (index, event) in response.events.iter().enumerate() {
        ensure_run_id(
            &format!("events[{index}].run_id"),
            event.header().run_id(),
            expected_run_id,
        )?;
        let offset = u64::try_from(index)
            .map_err(|_| invalid_response("event index exceeds the u64 domain"))?;
        let expected_sequence = first
            .and_then(|first| first.checked_add(offset))
            .ok_or_else(|| invalid_response("event sequence range overflowed"))?;
        if event.header().sequence().get() != expected_sequence {
            return invalid_field(
                &format!("events[{index}].sequence"),
                &format!(
                    "must be {expected_sequence} for a strictly increasing duplicate-free response"
                ),
            );
        }
        if event.header().sequence().get() > watermark {
            return invalid_field(
                &format!("events[{index}].sequence"),
                "must not exceed captured_watermark",
            );
        }
    }
    Ok(())
}

fn expected_event_range(start: EventStart, watermark: u64) -> (Option<u64>, u64) {
    let count = match start {
        EventStart::Tail(count) => count.get().min(watermark),
        EventStart::After(after) => watermark.saturating_sub(after.get().min(watermark)),
    };
    if count == 0 {
        (None, 0)
    } else {
        (Some(watermark - count + 1), count)
    }
}

fn ensure_run_id(
    path: &str,
    actual: CanonicalUuid,
    expected: CanonicalUuid,
) -> Result<(), EventsError> {
    if actual == expected {
        Ok(())
    } else {
        Err(EventsError::new(
            EventsErrorCode::RunIdentityMismatch,
            format!("{path} Run {actual} differs from resolved Run {expected}"),
        ))
    }
}

fn invalid_field<T>(path: &str, requirement: &str) -> Result<T, EventsError> {
    Err(invalid_response(format!("{path} {requirement}")))
}

fn invalid_response(detail: impl Into<String>) -> EventsError {
    EventsError::new(EventsErrorCode::InvalidResponse, detail)
}

fn write_human(value: &Value, output: &mut String, indent: usize) {
    match value {
        Value::Object(fields) if fields.is_empty() => output.push_str("{}"),
        Value::Object(fields) => {
            for (name, value) in fields {
                write_indent(output, indent);
                match value {
                    Value::Object(fields) if fields.is_empty() => {
                        writeln!(output, "{name}: {{}}").expect("write to String");
                    }
                    Value::Array(values) if values.is_empty() => {
                        writeln!(output, "{name}: []").expect("write to String");
                    }
                    Value::Object(_) | Value::Array(_) => {
                        writeln!(output, "{name}:").expect("write to String");
                        write_human(value, output, indent + 2);
                    }
                    _ => {
                        write!(output, "{name}: ").expect("write to String");
                        write_human_scalar(value, output);
                        output.push('\n');
                    }
                }
            }
        }
        Value::Array(values) if values.is_empty() => output.push_str("[]"),
        Value::Array(values) => {
            for value in values {
                write_indent(output, indent);
                match value {
                    Value::Object(_) | Value::Array(_) => {
                        output.push_str("-\n");
                        write_human(value, output, indent + 2);
                    }
                    _ => {
                        output.push_str("- ");
                        write_human_scalar(value, output);
                        output.push('\n');
                    }
                }
            }
        }
        _ => write_human_scalar(value, output),
    }
}

fn write_indent(output: &mut String, indent: usize) {
    for _ in 0..indent {
        output.push(' ');
    }
}

fn write_human_scalar(value: &Value, output: &mut String) {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => write!(output, "{value}").expect("write to String"),
        Value::String(value)
            if !value.is_empty() && value.chars().all(|character| !character.is_control()) =>
        {
            output.push_str(value);
        }
        Value::String(value) => {
            write!(output, "{}", Value::String(value.clone())).expect("write to String");
        }
        Value::Array(_) | Value::Object(_) => unreachable!("compound values are not scalars"),
    }
}
