#![allow(dead_code)] // D07 wires this finite command into the CLI dispatcher.

use std::{
    fmt::{self, Write as _},
    time::Duration,
};

use reqwest::header::{ACCEPT, CONTENT_TYPE};
use troupe_diagnostics_core::{event::EVENT_SCHEMA_VERSION, id::CanonicalUuid};
use troupe_diagnostics_runtime::{
    server::query::{STATUS_PATH, encode_status_response},
    store::schema::{STORE_SCHEMA_IDENTITY, STORE_SCHEMA_VERSION},
};

use super::{
    args::{DocumentFormat, StatusArgs},
    http_client::{DiagnosticHttpClient, HttpClientError},
    resolver::{ResolvedDiagnosticTarget, ResolverError, resolve},
};

const STATUS_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_STATUS_RESPONSE_BYTES: usize = 1024 * 1024;
const STATUS_API_SCHEMA_VERSION: &str = "1";
const SECURITY_SCOPE: &str = "trusted_network";
const MAX_JSON_DEPTH: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StatusErrorCode {
    Resolve,
    Http,
    UnexpectedStatus,
    ResponseTooLarge,
    InvalidResponse,
    IncompatibleResponse,
    RunIdentityMismatch,
    SourceMismatch,
    Archive,
    TaskFailed,
}

impl StatusErrorCode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Resolve => "diagnostic_status.resolve",
            Self::Http => "diagnostic_status.http",
            Self::UnexpectedStatus => "diagnostic_status.unexpected_status",
            Self::ResponseTooLarge => "diagnostic_status.response_too_large",
            Self::InvalidResponse => "diagnostic_status.invalid_response",
            Self::IncompatibleResponse => "diagnostic_status.incompatible_response",
            Self::RunIdentityMismatch => "diagnostic_status.run_identity_mismatch",
            Self::SourceMismatch => "diagnostic_status.source_mismatch",
            Self::Archive => "diagnostic_status.archive",
            Self::TaskFailed => "diagnostic_status.task_failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StatusError {
    code: StatusErrorCode,
    detail: String,
}

impl StatusError {
    fn new(code: StatusErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    fn resolver(error: ResolverError) -> Self {
        Self::new(StatusErrorCode::Resolve, error.to_string())
    }

    fn http(error: HttpClientError) -> Self {
        Self::new(StatusErrorCode::Http, error.to_string())
    }

    pub(crate) const fn code(&self) -> StatusErrorCode {
        self.code
    }
}

impl fmt::Display for StatusError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.detail)
    }
}

impl std::error::Error for StatusError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StatusDocument {
    value: JsonValue,
}

impl StatusDocument {
    pub(crate) fn render(&self, format: DocumentFormat) -> String {
        match format {
            DocumentFormat::Human => self.render_human(),
            DocumentFormat::Json => self.render_json(),
        }
    }

    fn render_json(&self) -> String {
        let mut output = String::new();
        self.value.write_json(&mut output);
        output.push('\n');
        output
    }

    fn render_human(&self) -> String {
        let mut output = String::new();
        self.value.write_human(&mut output, 0);
        output
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExpectedSource {
    Active,
    Archive,
}

impl ExpectedSource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archive => "archive",
        }
    }
}

pub(crate) async fn execute(arguments: StatusArgs) -> Result<String, StatusError> {
    let (target, format) = arguments.into_parts();
    let target = resolve(target).await.map_err(StatusError::resolver)?;
    query(target).await.map(|status| status.render(format))
}

pub(crate) async fn query(target: ResolvedDiagnosticTarget) -> Result<StatusDocument, StatusError> {
    match target {
        ResolvedDiagnosticTarget::Live(client) => query_live(&client).await,
        ResolvedDiagnosticTarget::Archive(mut archive) => {
            let expected_run_id = archive.run_id();
            tokio::task::spawn_blocking(move || {
                let bytes = {
                    let captured = archive.capture().map_err(|error| {
                        StatusError::new(
                            StatusErrorCode::Archive,
                            format!("cannot capture archive status: {error}"),
                        )
                    })?;
                    encode_status_response(expected_run_id, &captured, None).map_err(|error| {
                        StatusError::new(
                            StatusErrorCode::Archive,
                            format!("cannot project archive status: {error}"),
                        )
                    })?
                };
                decode_status_response(&bytes, expected_run_id, ExpectedSource::Archive)
            })
            .await
            .map_err(|error| {
                StatusError::new(
                    StatusErrorCode::TaskFailed,
                    format!("archive status task failed: {error}"),
                )
            })?
        }
    }
}

async fn query_live(client: &DiagnosticHttpClient) -> Result<StatusDocument, StatusError> {
    let expected_run_id = client.run_id();
    let bytes = fetch_status_bytes(client).await?;
    let status = decode_status_response(&bytes, expected_run_id, ExpectedSource::Active)?;

    // The response identity is checked above; repeat the authoritative identity
    // probe afterwards so endpoint replacement cannot pass a mixed capture.
    client
        .revalidate_identity()
        .await
        .map_err(StatusError::http)?;
    Ok(status)
}

async fn fetch_status_bytes(client: &DiagnosticHttpClient) -> Result<Vec<u8>, StatusError> {
    let mut response = client
        .get(STATUS_PATH)
        .map_err(StatusError::http)?
        .header(ACCEPT, "application/json")
        .timeout(STATUS_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|error| {
            StatusError::new(
                StatusErrorCode::Http,
                format!("status request failed: {error}"),
            )
        })?;

    if response.status().as_u16() != 200 {
        return Err(StatusError::new(
            StatusErrorCode::UnexpectedStatus,
            format!("status endpoint returned HTTP {}", response.status()),
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
        return Err(StatusError::new(
            StatusErrorCode::InvalidResponse,
            "status response is not application/json",
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_STATUS_RESPONSE_BYTES as u64)
    {
        return Err(StatusError::new(
            StatusErrorCode::ResponseTooLarge,
            "status response exceeds the size limit",
        ));
    }

    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        StatusError::new(
            StatusErrorCode::Http,
            format!("status response body failed: {error}"),
        )
    })? {
        let next_length = bytes.len().checked_add(chunk.len()).ok_or_else(|| {
            StatusError::new(
                StatusErrorCode::ResponseTooLarge,
                "status response length overflowed",
            )
        })?;
        if next_length > MAX_STATUS_RESPONSE_BYTES {
            return Err(StatusError::new(
                StatusErrorCode::ResponseTooLarge,
                "status response exceeds the size limit",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub(crate) fn decode_status_response(
    bytes: &[u8],
    expected_run_id: CanonicalUuid,
    expected_source: ExpectedSource,
) -> Result<StatusDocument, StatusError> {
    if bytes.len() > MAX_STATUS_RESPONSE_BYTES {
        return Err(StatusError::new(
            StatusErrorCode::ResponseTooLarge,
            "status response exceeds the size limit",
        ));
    }
    let value = JsonParser::new(bytes).parse().map_err(|detail| {
        StatusError::new(
            StatusErrorCode::InvalidResponse,
            format!("status response is invalid JSON: {detail}"),
        )
    })?;
    validate_status_document(&value, expected_run_id, expected_source)
        .map(|value| StatusDocument { value })
}

fn validate_status_document(
    value: &JsonValue,
    expected_run_id: CanonicalUuid,
    expected_source: ExpectedSource,
) -> Result<JsonValue, StatusError> {
    const FIELDS: &[&str] = &[
        "api_schema_version",
        "run_id",
        "source",
        "store_schema_version",
        "store_schema_identity",
        "event_schema_version",
        "configuration_identity",
        "event_watermark",
        "read_model_watermark",
        "lifecycle",
        "writer",
        "quota",
    ];
    let object = exact_object(value, FIELDS, "status")?;
    let api_schema_version = object_member(object, "api_schema_version");
    match api_schema_version {
        JsonValue::Number(version) if version == STATUS_API_SCHEMA_VERSION => {}
        JsonValue::Number(version) if is_unsigned_decimal(version) => {
            return Err(StatusError::new(
                StatusErrorCode::IncompatibleResponse,
                format!("status API schema version {version} is incompatible"),
            ));
        }
        _ => return invalid_field("api_schema_version", "must be the number 1"),
    }

    let run_id = required_string(object_member(object, "run_id"), "run_id")?;
    let decoded_run_id = CanonicalUuid::parse(run_id)
        .map_err(|_| invalid_response("run_id is not a canonical UUID"))?;
    if decoded_run_id != expected_run_id {
        return Err(StatusError::new(
            StatusErrorCode::RunIdentityMismatch,
            format!(
                "status response Run {decoded_run_id} differs from resolved Run {expected_run_id}"
            ),
        ));
    }

    let source = required_string(object_member(object, "source"), "source")?;
    if source != expected_source.as_str() {
        return Err(StatusError::new(
            StatusErrorCode::SourceMismatch,
            format!(
                "status response source {source:?} differs from resolved {} target",
                expected_source.as_str()
            ),
        ));
    }

    Ok(JsonValue::Object(vec![
        field("api_schema_version", api_schema_version.clone()),
        field("run_id", JsonValue::String(run_id.to_owned())),
        field("source", JsonValue::String(source.to_owned())),
        // The v1 identity/registry protocol admits only this deployment scope;
        // archives retain that Production-level security contract.
        field(
            "security_scope",
            JsonValue::String(SECURITY_SCOPE.to_owned()),
        ),
        field(
            "store_schema_version",
            compatible_u64_string(
                object_member(object, "store_schema_version"),
                "store_schema_version",
                u64::from(STORE_SCHEMA_VERSION),
            )?,
        ),
        field(
            "store_schema_identity",
            compatible_string(
                object_member(object, "store_schema_identity"),
                "store_schema_identity",
                STORE_SCHEMA_IDENTITY,
            )?,
        ),
        field(
            "event_schema_version",
            compatible_u64_string(
                object_member(object, "event_schema_version"),
                "event_schema_version",
                u64::from(EVENT_SCHEMA_VERSION),
            )?,
        ),
        field(
            "configuration_identity",
            string_value(
                object_member(object, "configuration_identity"),
                "configuration_identity",
            )?,
        ),
        field(
            "event_watermark",
            canonical_u64_string(object_member(object, "event_watermark"), "event_watermark")?,
        ),
        field(
            "read_model_watermark",
            canonical_u64_string(
                object_member(object, "read_model_watermark"),
                "read_model_watermark",
            )?,
        ),
        field(
            "lifecycle",
            validate_lifecycle(object_member(object, "lifecycle"))?,
        ),
        field(
            "writer",
            validate_observation(
                object_member(object, "writer"),
                expected_source,
                ObservationKind::Writer,
            )?,
        ),
        field(
            "quota",
            validate_observation(
                object_member(object, "quota"),
                expected_source,
                ObservationKind::Quota,
            )?,
        ),
    ]))
}

fn validate_lifecycle(value: &JsonValue) -> Result<JsonValue, StatusError> {
    const FIELDS: &[&str] = &[
        "state",
        "started_at",
        "ended_at",
        "outcome",
        "clean_shutdown",
    ];
    let object = exact_object(value, FIELDS, "lifecycle")?;
    Ok(JsonValue::Object(vec![
        field(
            "state",
            enum_string(
                object_member(object, "state"),
                &["active", "completed", "failed", "incomplete"],
                "lifecycle.state",
            )?,
        ),
        field(
            "started_at",
            string_value(object_member(object, "started_at"), "lifecycle.started_at")?,
        ),
        field(
            "ended_at",
            optional_string(object_member(object, "ended_at"), "lifecycle.ended_at")?,
        ),
        field(
            "outcome",
            optional_enum_string(
                object_member(object, "outcome"),
                &["completed", "failed", "cancelled"],
                "lifecycle.outcome",
            )?,
        ),
        field(
            "clean_shutdown",
            bool_value(
                object_member(object, "clean_shutdown"),
                "lifecycle.clean_shutdown",
            )?,
        ),
    ]))
}

#[derive(Clone, Copy)]
enum ObservationKind {
    Writer,
    Quota,
}

impl ObservationKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Writer => "writer",
            Self::Quota => "quota",
        }
    }
}

fn validate_observation(
    value: &JsonValue,
    source: ExpectedSource,
    kind: ObservationKind,
) -> Result<JsonValue, StatusError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid_response(format!("{} must be an object", kind.as_str())))?;
    let status = required_string(
        object
            .iter()
            .find(|(name, _)| name == "status")
            .map(|(_, value)| value)
            .ok_or_else(|| invalid_response(format!("{}.status is required", kind.as_str())))?,
        &format!("{}.status", kind.as_str()),
    )?;
    match status {
        "available" => {
            exact_object(value, &["status", "value"], kind.as_str())?;
            if source == ExpectedSource::Archive {
                return invalid_field(kind.as_str(), "archive observations must be unavailable");
            }
            let projected = match kind {
                ObservationKind::Writer => validate_writer(object_member(object, "value"))?,
                ObservationKind::Quota => validate_quota(object_member(object, "value"))?,
            };
            Ok(JsonValue::Object(vec![
                field("status", JsonValue::String("available".to_owned())),
                field("value", projected),
            ]))
        }
        "unavailable" => {
            exact_object(value, &["status", "reason"], kind.as_str())?;
            let reason = required_string(
                object_member(object, "reason"),
                &format!("{}.reason", kind.as_str()),
            )?;
            let reason_is_valid = match source {
                ExpectedSource::Active => matches!(reason, "not_observed" | "state_unavailable"),
                ExpectedSource::Archive => reason == "archive",
            };
            if !reason_is_valid {
                return invalid_field(
                    &format!("{}.reason", kind.as_str()),
                    "does not match the target source",
                );
            }
            Ok(JsonValue::Object(vec![
                field("status", JsonValue::String("unavailable".to_owned())),
                field("reason", JsonValue::String(reason.to_owned())),
            ]))
        }
        _ => invalid_field(
            &format!("{}.status", kind.as_str()),
            "must be available or unavailable",
        ),
    }
}

fn validate_writer(value: &JsonValue) -> Result<JsonValue, StatusError> {
    const FIELDS: &[&str] = &[
        "max_uncommitted_events",
        "max_uncommitted_canonical_bytes",
        "max_batch_age",
        "max_batch_events",
        "max_batch_canonical_bytes",
        "accepted_uncommitted_events",
        "accepted_uncommitted_canonical_bytes",
        "queued_events",
        "in_flight_events",
        "ingress_committed_watermark",
        "normal_ingress_sealed",
        "ingress_failure",
        "writer_stall_timeout",
        "shutdown_drain_timeout",
        "progress_committed_watermark",
        "accepted_tail_events",
        "stalled_for",
        "drain_state",
        "progress_failure",
    ];
    let object = exact_object(value, FIELDS, "writer.value")?;
    let u64_field = |name: &'static str| {
        canonical_u64_string(object_member(object, name), &format!("writer.value.{name}"))
    };
    Ok(JsonValue::Object(vec![
        field(
            "max_uncommitted_events",
            u64_field("max_uncommitted_events")?,
        ),
        field(
            "max_uncommitted_canonical_bytes",
            u64_field("max_uncommitted_canonical_bytes")?,
        ),
        field(
            "max_batch_age",
            validate_duration(
                object_member(object, "max_batch_age"),
                "writer.value.max_batch_age",
            )?,
        ),
        field("max_batch_events", u64_field("max_batch_events")?),
        field(
            "max_batch_canonical_bytes",
            u64_field("max_batch_canonical_bytes")?,
        ),
        field(
            "accepted_uncommitted_events",
            u64_field("accepted_uncommitted_events")?,
        ),
        field(
            "accepted_uncommitted_canonical_bytes",
            u64_field("accepted_uncommitted_canonical_bytes")?,
        ),
        field("queued_events", u64_field("queued_events")?),
        field("in_flight_events", u64_field("in_flight_events")?),
        field(
            "ingress_committed_watermark",
            u64_field("ingress_committed_watermark")?,
        ),
        field(
            "normal_ingress_sealed",
            bool_value(
                object_member(object, "normal_ingress_sealed"),
                "writer.value.normal_ingress_sealed",
            )?,
        ),
        field(
            "ingress_failure",
            optional_object(
                object_member(object, "ingress_failure"),
                validate_ingress_failure,
            )?,
        ),
        field(
            "writer_stall_timeout",
            validate_duration(
                object_member(object, "writer_stall_timeout"),
                "writer.value.writer_stall_timeout",
            )?,
        ),
        field(
            "shutdown_drain_timeout",
            validate_duration(
                object_member(object, "shutdown_drain_timeout"),
                "writer.value.shutdown_drain_timeout",
            )?,
        ),
        field(
            "progress_committed_watermark",
            u64_field("progress_committed_watermark")?,
        ),
        field("accepted_tail_events", u64_field("accepted_tail_events")?),
        field(
            "stalled_for",
            optional_duration(
                object_member(object, "stalled_for"),
                "writer.value.stalled_for",
            )?,
        ),
        field(
            "drain_state",
            enum_string(
                object_member(object, "drain_state"),
                &["not_started", "draining", "drained", "timed_out"],
                "writer.value.drain_state",
            )?,
        ),
        field(
            "progress_failure",
            optional_object(
                object_member(object, "progress_failure"),
                validate_progress_failure,
            )?,
        ),
    ]))
}

fn validate_ingress_failure(value: &JsonValue) -> Result<JsonValue, StatusError> {
    const FIELDS: &[&str] = &[
        "code",
        "current_events",
        "current_canonical_bytes",
        "attempted_events",
        "attempted_canonical_bytes",
        "event_limit_exceeded",
        "byte_limit_exceeded",
    ];
    let object = exact_object(value, FIELDS, "writer.value.ingress_failure")?;
    Ok(JsonValue::Object(vec![
        field(
            "code",
            string_value(
                object_member(object, "code"),
                "writer.value.ingress_failure.code",
            )?,
        ),
        field(
            "current_events",
            canonical_u64_string(
                object_member(object, "current_events"),
                "writer.value.ingress_failure.current_events",
            )?,
        ),
        field(
            "current_canonical_bytes",
            canonical_u64_string(
                object_member(object, "current_canonical_bytes"),
                "writer.value.ingress_failure.current_canonical_bytes",
            )?,
        ),
        field(
            "attempted_events",
            canonical_u64_string(
                object_member(object, "attempted_events"),
                "writer.value.ingress_failure.attempted_events",
            )?,
        ),
        field(
            "attempted_canonical_bytes",
            canonical_u64_string(
                object_member(object, "attempted_canonical_bytes"),
                "writer.value.ingress_failure.attempted_canonical_bytes",
            )?,
        ),
        field(
            "event_limit_exceeded",
            bool_value(
                object_member(object, "event_limit_exceeded"),
                "writer.value.ingress_failure.event_limit_exceeded",
            )?,
        ),
        field(
            "byte_limit_exceeded",
            bool_value(
                object_member(object, "byte_limit_exceeded"),
                "writer.value.ingress_failure.byte_limit_exceeded",
            )?,
        ),
    ]))
}

fn validate_progress_failure(value: &JsonValue) -> Result<JsonValue, StatusError> {
    const FIELDS: &[&str] = &["component", "stage", "code"];
    let object = exact_object(value, FIELDS, "writer.value.progress_failure")?;
    Ok(JsonValue::Object(vec![
        field(
            "component",
            string_value(
                object_member(object, "component"),
                "writer.value.progress_failure.component",
            )?,
        ),
        field(
            "stage",
            string_value(
                object_member(object, "stage"),
                "writer.value.progress_failure.stage",
            )?,
        ),
        field(
            "code",
            string_value(
                object_member(object, "code"),
                "writer.value.progress_failure.code",
            )?,
        ),
    ]))
}

fn validate_quota(value: &JsonValue) -> Result<JsonValue, StatusError> {
    const FIELDS: &[&str] = &[
        "max_run_bytes",
        "current_measured_bytes",
        "last_measurement_at",
        "sealed",
        "failure",
    ];
    let object = exact_object(value, FIELDS, "quota.value")?;
    Ok(JsonValue::Object(vec![
        field(
            "max_run_bytes",
            optional_canonical_u64_string(
                object_member(object, "max_run_bytes"),
                "quota.value.max_run_bytes",
            )?,
        ),
        field(
            "current_measured_bytes",
            optional_canonical_u64_string(
                object_member(object, "current_measured_bytes"),
                "quota.value.current_measured_bytes",
            )?,
        ),
        field(
            "last_measurement_at",
            optional_duration(
                object_member(object, "last_measurement_at"),
                "quota.value.last_measurement_at",
            )?,
        ),
        field(
            "sealed",
            bool_value(object_member(object, "sealed"), "quota.value.sealed")?,
        ),
        field(
            "failure",
            optional_object(object_member(object, "failure"), validate_quota_failure)?,
        ),
    ]))
}

fn validate_quota_failure(value: &JsonValue) -> Result<JsonValue, StatusError> {
    const FIELDS: &[&str] = &[
        "code",
        "limit_bytes",
        "current_bytes",
        "predicted_growth_bytes",
    ];
    let object = exact_object(value, FIELDS, "quota.value.failure")?;
    Ok(JsonValue::Object(vec![
        field(
            "code",
            string_value(object_member(object, "code"), "quota.value.failure.code")?,
        ),
        field(
            "limit_bytes",
            canonical_u64_string(
                object_member(object, "limit_bytes"),
                "quota.value.failure.limit_bytes",
            )?,
        ),
        field(
            "current_bytes",
            optional_canonical_u64_string(
                object_member(object, "current_bytes"),
                "quota.value.failure.current_bytes",
            )?,
        ),
        field(
            "predicted_growth_bytes",
            optional_canonical_u64_string(
                object_member(object, "predicted_growth_bytes"),
                "quota.value.failure.predicted_growth_bytes",
            )?,
        ),
    ]))
}

fn validate_duration(value: &JsonValue, path: &str) -> Result<JsonValue, StatusError> {
    let object = exact_object(value, &["seconds", "subsecond_nanoseconds"], path)?;
    let nanos_path = format!("{path}.subsecond_nanoseconds");
    let nanos = canonical_u64_string(object_member(object, "subsecond_nanoseconds"), &nanos_path)?;
    if let JsonValue::String(value) = &nanos
        && value.parse::<u64>().expect("canonical u64 was checked") >= 1_000_000_000
    {
        return invalid_field(&nanos_path, "must be less than 1000000000");
    }
    Ok(JsonValue::Object(vec![
        field(
            "seconds",
            canonical_u64_string(object_member(object, "seconds"), &format!("{path}.seconds"))?,
        ),
        field("subsecond_nanoseconds", nanos),
    ]))
}

fn optional_duration(value: &JsonValue, path: &str) -> Result<JsonValue, StatusError> {
    match value {
        JsonValue::Null => Ok(JsonValue::Null),
        _ => validate_duration(value, path),
    }
}

fn optional_object(
    value: &JsonValue,
    validator: impl FnOnce(&JsonValue) -> Result<JsonValue, StatusError>,
) -> Result<JsonValue, StatusError> {
    match value {
        JsonValue::Null => Ok(JsonValue::Null),
        _ => validator(value),
    }
}

fn exact_object<'a>(
    value: &'a JsonValue,
    expected_fields: &[&str],
    path: &str,
) -> Result<&'a [(String, JsonValue)], StatusError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid_response(format!("{path} must be an object")))?;
    if object.len() != expected_fields.len()
        || expected_fields
            .iter()
            .any(|expected| !object.iter().any(|(name, _)| name == expected))
    {
        return invalid_field(path, "has missing or unknown fields");
    }
    Ok(object)
}

fn object_member<'a>(object: &'a [(String, JsonValue)], name: &str) -> &'a JsonValue {
    object
        .iter()
        .find(|(candidate, _)| candidate == name)
        .map(|(_, value)| value)
        .expect("exact_object proved the field exists")
}

fn required_string<'a>(value: &'a JsonValue, path: &str) -> Result<&'a str, StatusError> {
    match value {
        JsonValue::String(value) => Ok(value),
        _ => invalid_field(path, "must be a string"),
    }
}

fn string_value(value: &JsonValue, path: &str) -> Result<JsonValue, StatusError> {
    required_string(value, path).map(|value| JsonValue::String(value.to_owned()))
}

fn optional_string(value: &JsonValue, path: &str) -> Result<JsonValue, StatusError> {
    match value {
        JsonValue::Null => Ok(JsonValue::Null),
        _ => string_value(value, path),
    }
}

fn enum_string(value: &JsonValue, allowed: &[&str], path: &str) -> Result<JsonValue, StatusError> {
    let value = required_string(value, path)?;
    if allowed.contains(&value) {
        Ok(JsonValue::String(value.to_owned()))
    } else {
        invalid_field(path, "contains an unknown value")
    }
}

fn optional_enum_string(
    value: &JsonValue,
    allowed: &[&str],
    path: &str,
) -> Result<JsonValue, StatusError> {
    match value {
        JsonValue::Null => Ok(JsonValue::Null),
        _ => enum_string(value, allowed, path),
    }
}

fn bool_value(value: &JsonValue, path: &str) -> Result<JsonValue, StatusError> {
    match value {
        JsonValue::Bool(value) => Ok(JsonValue::Bool(*value)),
        _ => invalid_field(path, "must be a boolean"),
    }
}

fn canonical_u64_string(value: &JsonValue, path: &str) -> Result<JsonValue, StatusError> {
    let value = required_string(value, path)?;
    if !is_canonical_u64(value) {
        return invalid_field(path, "must be a canonical decimal u64 string");
    }
    Ok(JsonValue::String(value.to_owned()))
}

fn optional_canonical_u64_string(value: &JsonValue, path: &str) -> Result<JsonValue, StatusError> {
    match value {
        JsonValue::Null => Ok(JsonValue::Null),
        _ => canonical_u64_string(value, path),
    }
}

fn compatible_u64_string(
    value: &JsonValue,
    path: &str,
    expected: u64,
) -> Result<JsonValue, StatusError> {
    let value = canonical_u64_string(value, path)?;
    let JsonValue::String(actual) = &value else {
        unreachable!("canonical_u64_string returns a string")
    };
    if actual.parse::<u64>() == Ok(expected) {
        Ok(value)
    } else {
        Err(StatusError::new(
            StatusErrorCode::IncompatibleResponse,
            format!("{path} {actual} is incompatible with {expected}"),
        ))
    }
}

fn compatible_string(
    value: &JsonValue,
    path: &str,
    expected: &str,
) -> Result<JsonValue, StatusError> {
    let actual = required_string(value, path)?;
    if actual == expected {
        Ok(JsonValue::String(actual.to_owned()))
    } else {
        Err(StatusError::new(
            StatusErrorCode::IncompatibleResponse,
            format!("{path} {actual:?} is incompatible with {expected:?}"),
        ))
    }
}

fn is_canonical_u64(value: &str) -> bool {
    is_unsigned_decimal(value)
        && (value == "0" || !value.starts_with('0'))
        && value.parse::<u64>().is_ok()
}

fn is_unsigned_decimal(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn field(name: &str, value: JsonValue) -> (String, JsonValue) {
    (name.to_owned(), value)
}

fn invalid_field<T>(path: &str, requirement: &str) -> Result<T, StatusError> {
    Err(invalid_response(format!("{path} {requirement}")))
}

fn invalid_response(detail: impl Into<String>) -> StatusError {
    StatusError::new(StatusErrorCode::InvalidResponse, detail)
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum JsonValue {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Self>),
    Object(Vec<(String, Self)>),
}

impl JsonValue {
    fn as_object(&self) -> Option<&[(String, Self)]> {
        match self {
            Self::Object(fields) => Some(fields),
            _ => None,
        }
    }

    fn write_json(&self, output: &mut String) {
        match self {
            Self::Null => output.push_str("null"),
            Self::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
            Self::Number(value) => output.push_str(value),
            Self::String(value) => push_json_string(output, value),
            Self::Array(values) => {
                output.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        output.push(',');
                    }
                    value.write_json(output);
                }
                output.push(']');
            }
            Self::Object(fields) => {
                output.push('{');
                for (index, (name, value)) in fields.iter().enumerate() {
                    if index != 0 {
                        output.push(',');
                    }
                    push_json_string(output, name);
                    output.push(':');
                    value.write_json(output);
                }
                output.push('}');
            }
        }
    }

    fn write_human(&self, output: &mut String, indent: usize) {
        match self {
            Self::Object(fields) => {
                for (name, value) in fields {
                    for _ in 0..indent {
                        output.push(' ');
                    }
                    match value {
                        Self::Object(_) | Self::Array(_) => {
                            writeln!(output, "{name}:").expect("write to String");
                            value.write_human(output, indent + 2);
                        }
                        _ => {
                            write!(output, "{name}: ").expect("write to String");
                            value.write_human_scalar(output);
                            output.push('\n');
                        }
                    }
                }
            }
            Self::Array(values) => {
                for value in values {
                    for _ in 0..indent {
                        output.push(' ');
                    }
                    output.push_str("- ");
                    match value {
                        Self::Object(_) | Self::Array(_) => {
                            output.push('\n');
                            value.write_human(output, indent + 2);
                        }
                        _ => {
                            value.write_human_scalar(output);
                            output.push('\n');
                        }
                    }
                }
            }
            _ => self.write_human_scalar(output),
        }
    }

    fn write_human_scalar(&self, output: &mut String) {
        match self {
            Self::Null => output.push_str("null"),
            Self::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
            Self::Number(value) | Self::String(value)
                if !value.is_empty() && value.chars().all(|character| !character.is_control()) =>
            {
                output.push_str(value);
            }
            Self::String(value) => push_json_string(output, value),
            Self::Array(_) | Self::Object(_) => unreachable!("compound values are not scalars"),
            Self::Number(value) => output.push_str(value),
        }
    }
}

fn push_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character <= '\u{1f}' => {
                write!(output, "\\u{:04x}", u32::from(character)).expect("write to String");
            }
            character => output.push(character),
        }
    }
    output.push('"');
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> JsonParser<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn parse(mut self) -> Result<JsonValue, String> {
        let value = self.parse_value(0)?;
        self.skip_whitespace();
        if self.position != self.bytes.len() {
            return Err(format!("trailing content at byte {}", self.position));
        }
        Ok(value)
    }

    fn parse_value(&mut self, depth: usize) -> Result<JsonValue, String> {
        if depth > MAX_JSON_DEPTH {
            return Err("JSON nesting exceeds the depth limit".to_owned());
        }
        self.skip_whitespace();
        match self.peek() {
            Some(b'n') => self.parse_literal(b"null", JsonValue::Null),
            Some(b't') => self.parse_literal(b"true", JsonValue::Bool(true)),
            Some(b'f') => self.parse_literal(b"false", JsonValue::Bool(false)),
            Some(b'"') => self.parse_string().map(JsonValue::String),
            Some(b'[') => self.parse_array(depth + 1),
            Some(b'{') => self.parse_object(depth + 1),
            Some(b'-' | b'0'..=b'9') => self.parse_number().map(JsonValue::Number),
            Some(_) => Err(format!("unexpected token at byte {}", self.position)),
            None => Err("unexpected end of input".to_owned()),
        }
    }

    fn parse_literal(&mut self, literal: &[u8], value: JsonValue) -> Result<JsonValue, String> {
        if self.bytes.get(self.position..self.position + literal.len()) == Some(literal) {
            self.position += literal.len();
            Ok(value)
        } else {
            Err(format!("invalid literal at byte {}", self.position))
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<JsonValue, String> {
        self.position += 1;
        self.skip_whitespace();
        let mut values = Vec::new();
        if self.consume(b']') {
            return Ok(JsonValue::Array(values));
        }
        loop {
            values.push(self.parse_value(depth)?);
            self.skip_whitespace();
            if self.consume(b']') {
                return Ok(JsonValue::Array(values));
            }
            self.expect(b',')?;
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<JsonValue, String> {
        self.position += 1;
        self.skip_whitespace();
        let mut fields = Vec::new();
        if self.consume(b'}') {
            return Ok(JsonValue::Object(fields));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return Err(format!(
                    "object key must be a string at byte {}",
                    self.position
                ));
            }
            let name = self.parse_string()?;
            if fields
                .iter()
                .any(|(existing, _): &(String, JsonValue)| existing == &name)
            {
                return Err(format!("duplicate object key {name:?}"));
            }
            self.skip_whitespace();
            self.expect(b':')?;
            let value = self.parse_value(depth)?;
            fields.push((name, value));
            self.skip_whitespace();
            if self.consume(b'}') {
                return Ok(JsonValue::Object(fields));
            }
            self.expect(b',')?;
        }
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut output = String::new();
        let mut segment_start = self.position;
        loop {
            let Some(byte) = self.peek() else {
                return Err("unterminated string".to_owned());
            };
            match byte {
                b'"' => {
                    self.push_utf8_segment(&mut output, segment_start, self.position)?;
                    self.position += 1;
                    return Ok(output);
                }
                b'\\' => {
                    self.push_utf8_segment(&mut output, segment_start, self.position)?;
                    self.position += 1;
                    self.parse_escape(&mut output)?;
                    segment_start = self.position;
                }
                0x00..=0x1f => {
                    return Err(format!(
                        "unescaped control character at byte {}",
                        self.position
                    ));
                }
                _ => self.position += 1,
            }
        }
    }

    fn push_utf8_segment(
        &self,
        output: &mut String,
        start: usize,
        end: usize,
    ) -> Result<(), String> {
        let segment = std::str::from_utf8(&self.bytes[start..end])
            .map_err(|_| format!("invalid UTF-8 string at byte {start}"))?;
        output.push_str(segment);
        Ok(())
    }

    fn parse_escape(&mut self, output: &mut String) -> Result<(), String> {
        let Some(escape) = self.peek() else {
            return Err("unterminated escape".to_owned());
        };
        self.position += 1;
        match escape {
            b'"' => output.push('"'),
            b'\\' => output.push('\\'),
            b'/' => output.push('/'),
            b'b' => output.push('\u{08}'),
            b'f' => output.push('\u{0c}'),
            b'n' => output.push('\n'),
            b'r' => output.push('\r'),
            b't' => output.push('\t'),
            b'u' => {
                let first = self.parse_hex_quad()?;
                let scalar = if (0xd800..=0xdbff).contains(&first) {
                    if !self.consume(b'\\') || !self.consume(b'u') {
                        return Err("high surrogate is not followed by a low surrogate".to_owned());
                    }
                    let second = self.parse_hex_quad()?;
                    if !(0xdc00..=0xdfff).contains(&second) {
                        return Err("invalid low surrogate".to_owned());
                    }
                    0x1_0000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00)
                } else if (0xdc00..=0xdfff).contains(&first) {
                    return Err("lone low surrogate".to_owned());
                } else {
                    u32::from(first)
                };
                output.push(
                    char::from_u32(scalar).ok_or_else(|| "invalid Unicode scalar".to_owned())?,
                );
            }
            _ => return Err(format!("invalid escape at byte {}", self.position - 1)),
        }
        Ok(())
    }

    fn parse_hex_quad(&mut self) -> Result<u16, String> {
        let start = self.position;
        let end = start
            .checked_add(4)
            .ok_or_else(|| "Unicode escape position overflowed".to_owned())?;
        let Some(bytes) = self.bytes.get(start..end) else {
            return Err("truncated Unicode escape".to_owned());
        };
        let mut value = 0_u16;
        for byte in bytes {
            let digit = hex_digit(*byte)?;
            value = value
                .checked_mul(16)
                .and_then(|value| value.checked_add(u16::from(digit)))
                .expect("four hexadecimal digits fit in u16");
        }
        self.position = end;
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<String, String> {
        let start = self.position;
        self.consume(b'-');
        match self.peek() {
            Some(b'0') => {
                self.position += 1;
                if self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                    return Err(format!("number has a leading zero at byte {start}"));
                }
            }
            Some(b'1'..=b'9') => {
                self.position += 1;
                while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                    self.position += 1;
                }
            }
            _ => return Err(format!("invalid number at byte {start}")),
        }
        if self.consume(b'.') {
            if !self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                return Err(format!("invalid number fraction at byte {}", self.position));
            }
            while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                self.position += 1;
            }
        }
        if self.peek().is_some_and(|byte| matches!(byte, b'e' | b'E')) {
            self.position += 1;
            if self.peek().is_some_and(|byte| matches!(byte, b'+' | b'-')) {
                self.position += 1;
            }
            if !self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                return Err(format!("invalid number exponent at byte {}", self.position));
            }
            while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                self.position += 1;
            }
        }
        std::str::from_utf8(&self.bytes[start..self.position])
            .map(str::to_owned)
            .map_err(|_| format!("invalid number at byte {start}"))
    }

    fn skip_whitespace(&mut self) {
        while self
            .peek()
            .is_some_and(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.position += 1;
        }
    }

    fn expect(&mut self, expected: u8) -> Result<(), String> {
        if self.consume(expected) {
            Ok(())
        } else {
            Err(format!(
                "expected {:?} at byte {}",
                char::from(expected),
                self.position
            ))
        }
    }

    fn consume(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }
}

fn hex_digit(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err("Unicode escape contains a non-hexadecimal digit".to_owned()),
    }
}
