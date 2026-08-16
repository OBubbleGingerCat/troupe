#![allow(dead_code)] // D07 wires this finite command into the CLI dispatcher.

use std::{
    fmt::{self, Write as _},
    time::Duration,
};

use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use troupe_diagnostics_core::{id::CanonicalUuid, scalar::SchemaU64};
use troupe_diagnostics_runtime::{
    server::query::{SNAPSHOT_PATH, encode_snapshot_response},
    store::projector::{
        counters::COUNTER_READ_MODEL_SCHEMA_VERSION,
        messages::MESSAGE_READ_MODEL_SCHEMA_VERSION,
        plans::PLAN_READ_MODEL_SCHEMA_VERSION,
        snapshot::{SNAPSHOT_READ_MODEL_SCHEMA_VERSION, SnapshotReadModel},
        spans::SPAN_READ_MODEL_SCHEMA_VERSION,
        usage::USAGE_READ_MODEL_SCHEMA_VERSION,
    },
};

use super::{
    args::{DocumentFormat, SnapshotArgs},
    http_client::{DiagnosticHttpClient, HttpClientError},
    resolver::{ResolvedDiagnosticTarget, ResolverError, resolve},
};

const SNAPSHOT_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_SNAPSHOT_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const SNAPSHOT_API_SCHEMA_VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SnapshotErrorCode {
    Resolve,
    Http,
    UnexpectedStatus,
    ResponseTooLarge,
    InvalidResponse,
    IncompatibleResponse,
    RunIdentityMismatch,
    Archive,
    TaskFailed,
}

impl SnapshotErrorCode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Resolve => "diagnostic_snapshot_cli.resolve",
            Self::Http => "diagnostic_snapshot_cli.http",
            Self::UnexpectedStatus => "diagnostic_snapshot_cli.unexpected_status",
            Self::ResponseTooLarge => "diagnostic_snapshot_cli.response_too_large",
            Self::InvalidResponse => "diagnostic_snapshot_cli.invalid_response",
            Self::IncompatibleResponse => "diagnostic_snapshot_cli.incompatible_response",
            Self::RunIdentityMismatch => "diagnostic_snapshot_cli.run_identity_mismatch",
            Self::Archive => "diagnostic_snapshot_cli.archive",
            Self::TaskFailed => "diagnostic_snapshot_cli.task_failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SnapshotError {
    code: SnapshotErrorCode,
    detail: String,
}

impl SnapshotError {
    fn new(code: SnapshotErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    fn resolver(error: ResolverError) -> Self {
        Self::new(SnapshotErrorCode::Resolve, error.to_string())
    }

    fn http(error: HttpClientError) -> Self {
        Self::new(SnapshotErrorCode::Http, error.to_string())
    }

    pub(crate) const fn code(&self) -> SnapshotErrorCode {
        self.code
    }
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.detail)
    }
}

impl std::error::Error for SnapshotError {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct SnapshotResponseV1 {
    api_schema_version: u8,
    run_id: CanonicalUuid,
    watermark_sequence: SchemaU64,
    earliest_available_sequence: Option<SchemaU64>,
    state: SnapshotReadModel,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SnapshotDocument {
    response: SnapshotResponseV1,
}

impl SnapshotDocument {
    pub(crate) fn render(&self, format: DocumentFormat) -> String {
        match format {
            DocumentFormat::Human => self.render_human(),
            DocumentFormat::Json => self.render_json(),
        }
    }

    fn render_json(&self) -> String {
        let mut output = serde_json::to_string(&self.response)
            .expect("a validated typed snapshot response is serializable");
        output.push('\n');
        output
    }

    fn render_human(&self) -> String {
        let value = serde_json::to_value(&self.response)
            .expect("a validated typed snapshot response is serializable");
        let mut output = String::new();
        write_human(&value, &mut output, 0);
        output
    }
}

pub(crate) async fn execute(arguments: SnapshotArgs) -> Result<String, SnapshotError> {
    let (target, format) = arguments.into_parts();
    let target = resolve(target).await.map_err(SnapshotError::resolver)?;
    query(target).await.map(|snapshot| snapshot.render(format))
}

pub(crate) async fn query(
    target: ResolvedDiagnosticTarget,
) -> Result<SnapshotDocument, SnapshotError> {
    match target {
        ResolvedDiagnosticTarget::Live(client) => query_live(&client).await,
        ResolvedDiagnosticTarget::Archive(mut archive) => {
            let expected_run_id = archive.run_id();
            tokio::task::spawn_blocking(move || {
                let bytes = {
                    let captured = archive.capture().map_err(|error| {
                        SnapshotError::new(
                            SnapshotErrorCode::Archive,
                            format!("cannot capture archive snapshot: {error}"),
                        )
                    })?;
                    encode_snapshot_response(expected_run_id, &captured).map_err(|error| {
                        SnapshotError::new(
                            SnapshotErrorCode::Archive,
                            format!("cannot project archive snapshot: {error}"),
                        )
                    })?
                };
                decode_snapshot_response(&bytes, expected_run_id)
            })
            .await
            .map_err(|error| {
                SnapshotError::new(
                    SnapshotErrorCode::TaskFailed,
                    format!("archive snapshot task failed: {error}"),
                )
            })?
        }
    }
}

async fn query_live(client: &DiagnosticHttpClient) -> Result<SnapshotDocument, SnapshotError> {
    let expected_run_id = client.run_id();
    let bytes = fetch_snapshot_bytes(client).await?;
    let snapshot = decode_snapshot_response(&bytes, expected_run_id)?;

    // The response identity is checked above; repeat the authoritative identity
    // probe afterwards so endpoint replacement cannot pass a mixed capture.
    client
        .revalidate_identity()
        .await
        .map_err(SnapshotError::http)?;
    Ok(snapshot)
}

async fn fetch_snapshot_bytes(client: &DiagnosticHttpClient) -> Result<Vec<u8>, SnapshotError> {
    let mut response = client
        .get(SNAPSHOT_PATH)
        .map_err(SnapshotError::http)?
        .header(ACCEPT, "application/json")
        .timeout(SNAPSHOT_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|error| {
            SnapshotError::new(
                SnapshotErrorCode::Http,
                format!("snapshot request failed: {error}"),
            )
        })?;

    if response.status().as_u16() != 200 {
        return Err(SnapshotError::new(
            SnapshotErrorCode::UnexpectedStatus,
            format!("snapshot endpoint returned HTTP {}", response.status()),
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
        return Err(SnapshotError::new(
            SnapshotErrorCode::InvalidResponse,
            "snapshot response is not application/json",
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_SNAPSHOT_RESPONSE_BYTES as u64)
    {
        return Err(SnapshotError::new(
            SnapshotErrorCode::ResponseTooLarge,
            "snapshot response exceeds the size limit",
        ));
    }

    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        SnapshotError::new(
            SnapshotErrorCode::Http,
            format!("snapshot response body failed: {error}"),
        )
    })? {
        let next_length = bytes.len().checked_add(chunk.len()).ok_or_else(|| {
            SnapshotError::new(
                SnapshotErrorCode::ResponseTooLarge,
                "snapshot response length overflowed",
            )
        })?;
        if next_length > MAX_SNAPSHOT_RESPONSE_BYTES {
            return Err(SnapshotError::new(
                SnapshotErrorCode::ResponseTooLarge,
                "snapshot response exceeds the size limit",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub(crate) fn decode_snapshot_response(
    bytes: &[u8],
    expected_run_id: CanonicalUuid,
) -> Result<SnapshotDocument, SnapshotError> {
    if bytes.len() > MAX_SNAPSHOT_RESPONSE_BYTES {
        return Err(SnapshotError::new(
            SnapshotErrorCode::ResponseTooLarge,
            "snapshot response exceeds the size limit",
        ));
    }
    let response: SnapshotResponseV1 = serde_json::from_slice(bytes).map_err(|error| {
        SnapshotError::new(
            SnapshotErrorCode::InvalidResponse,
            format!("snapshot response is invalid v1 JSON: {error}"),
        )
    })?;
    validate_snapshot_response(&response, expected_run_id)?;
    Ok(SnapshotDocument { response })
}

fn validate_snapshot_response(
    response: &SnapshotResponseV1,
    expected_run_id: CanonicalUuid,
) -> Result<(), SnapshotError> {
    if response.api_schema_version != SNAPSHOT_API_SCHEMA_VERSION {
        return Err(SnapshotError::new(
            SnapshotErrorCode::IncompatibleResponse,
            format!(
                "snapshot API schema version {} is incompatible with {}",
                response.api_schema_version, SNAPSHOT_API_SCHEMA_VERSION
            ),
        ));
    }
    ensure_run_id("run_id", response.run_id, expected_run_id)?;

    let watermark = response.watermark_sequence.get();
    match response.earliest_available_sequence {
        None if watermark == 0 => {}
        None => {
            return invalid_field(
                "earliest_available_sequence",
                "must be present when watermark_sequence is nonzero",
            );
        }
        Some(_) if watermark == 0 => {
            return invalid_field(
                "earliest_available_sequence",
                "must be null when watermark_sequence is zero",
            );
        }
        Some(earliest) if earliest.get() == 0 || earliest.get() > watermark => {
            return invalid_field(
                "earliest_available_sequence",
                "must be between 1 and watermark_sequence",
            );
        }
        Some(_) => {}
    }

    validate_snapshot_state(
        &response.state,
        expected_run_id,
        response.watermark_sequence,
    )
}

fn validate_snapshot_state(
    state: &SnapshotReadModel,
    expected_run_id: CanonicalUuid,
    watermark: SchemaU64,
) -> Result<(), SnapshotError> {
    ensure_model_version(
        "state.model_schema_version",
        state.model_schema_version(),
        SNAPSHOT_READ_MODEL_SCHEMA_VERSION,
    )?;
    ensure_run_id("state.run_id", state.run_id(), expected_run_id)?;
    ensure_watermark(
        "state.through_sequence",
        state.through_sequence(),
        watermark,
    )?;
    let elapsed = state.through_elapsed_ns();

    ensure_model_version(
        "state.spans.model_schema_version",
        state.spans().model_schema_version(),
        SPAN_READ_MODEL_SCHEMA_VERSION,
    )?;
    ensure_run_id(
        "state.spans.run_id",
        state.spans().run_id(),
        expected_run_id,
    )?;
    ensure_watermark(
        "state.spans.through_sequence",
        state.spans().through_sequence(),
        watermark,
    )?;
    ensure_elapsed(
        "state.spans.through_elapsed_ns",
        state.spans().through_elapsed_ns(),
        elapsed,
    )?;

    ensure_model_version(
        "state.messages.model_schema_version",
        state.messages().model_schema_version(),
        MESSAGE_READ_MODEL_SCHEMA_VERSION,
    )?;
    ensure_run_id(
        "state.messages.run_id",
        state.messages().run_id(),
        expected_run_id,
    )?;
    ensure_watermark(
        "state.messages.through_sequence",
        state.messages().through_sequence(),
        watermark,
    )?;
    ensure_elapsed(
        "state.messages.through_elapsed_ns",
        state.messages().through_elapsed_ns(),
        elapsed,
    )?;

    ensure_model_version(
        "state.plans.model_schema_version",
        state.plans().model_schema_version(),
        PLAN_READ_MODEL_SCHEMA_VERSION,
    )?;
    ensure_run_id(
        "state.plans.run_id",
        state.plans().run_id(),
        expected_run_id,
    )?;
    ensure_watermark(
        "state.plans.through_sequence",
        state.plans().through_sequence(),
        watermark,
    )?;
    ensure_elapsed(
        "state.plans.through_elapsed_ns",
        state.plans().through_elapsed_ns(),
        elapsed,
    )?;

    ensure_model_version(
        "state.counters.model_schema_version",
        state.counters().model_schema_version(),
        COUNTER_READ_MODEL_SCHEMA_VERSION,
    )?;
    ensure_run_id(
        "state.counters.run_id",
        state.counters().run_id(),
        expected_run_id,
    )?;
    ensure_watermark(
        "state.counters.through_sequence",
        state.counters().through_sequence(),
        watermark,
    )?;
    ensure_elapsed(
        "state.counters.through_elapsed_ns",
        state.counters().through_elapsed_ns(),
        elapsed,
    )?;

    ensure_model_version(
        "state.usage.model_schema_version",
        state.usage().model_schema_version(),
        USAGE_READ_MODEL_SCHEMA_VERSION,
    )?;
    ensure_run_id(
        "state.usage.run_id",
        state.usage().run_id(),
        expected_run_id,
    )?;
    ensure_watermark(
        "state.usage.through_sequence",
        state.usage().through_sequence(),
        watermark,
    )?;
    ensure_elapsed(
        "state.usage.through_elapsed_ns",
        state.usage().through_elapsed_ns(),
        elapsed,
    )?;

    for (index, span) in state.spans().spans().iter().enumerate() {
        ensure_run_id(
            &format!("state.spans.spans[{index}].run_id"),
            span.run_id(),
            expected_run_id,
        )?;
    }
    for (index, message) in state.messages().messages().iter().enumerate() {
        ensure_run_id(
            &format!("state.messages.messages[{index}].run_id"),
            message.run_id(),
            expected_run_id,
        )?;
    }
    for (index, plan) in state.plans().plans().iter().enumerate() {
        ensure_run_id(
            &format!("state.plans.plans[{index}].run_id"),
            plan.run_id(),
            expected_run_id,
        )?;
    }
    for (index, counter) in state.counters().series().iter().enumerate() {
        ensure_run_id(
            &format!("state.counters.series[{index}].run_id"),
            counter.run_id(),
            expected_run_id,
        )?;
    }
    for (index, usage) in state.usage().usages().iter().enumerate() {
        ensure_run_id(
            &format!("state.usage.usages[{index}].run_id"),
            usage.run_id(),
            expected_run_id,
        )?;
    }
    for (index, gap) in state.gaps().iter().enumerate() {
        ensure_run_id(
            &format!("state.gaps[{index}].run_id"),
            gap.header().run_id(),
            expected_run_id,
        )?;
    }

    state
        .counters()
        .validate()
        .map_err(|error| invalid_response(format!("state.counters is inconsistent: {error}")))?;
    state
        .usage()
        .validate()
        .map_err(|error| invalid_response(format!("state.usage is inconsistent: {error}")))?;
    Ok(())
}

fn ensure_model_version(path: &str, actual: u8, expected: u8) -> Result<(), SnapshotError> {
    if actual == expected {
        Ok(())
    } else {
        Err(SnapshotError::new(
            SnapshotErrorCode::IncompatibleResponse,
            format!("{path} {actual} is incompatible with {expected}"),
        ))
    }
}

fn ensure_run_id(
    path: &str,
    actual: CanonicalUuid,
    expected: CanonicalUuid,
) -> Result<(), SnapshotError> {
    if actual == expected {
        Ok(())
    } else {
        Err(SnapshotError::new(
            SnapshotErrorCode::RunIdentityMismatch,
            format!("{path} Run {actual} differs from resolved Run {expected}"),
        ))
    }
}

fn ensure_watermark(
    path: &str,
    actual: SchemaU64,
    expected: SchemaU64,
) -> Result<(), SnapshotError> {
    if actual == expected {
        Ok(())
    } else {
        invalid_field(path, "must equal watermark_sequence")
    }
}

fn ensure_elapsed(
    path: &str,
    actual: troupe_diagnostics_core::time::ElapsedNs,
    expected: troupe_diagnostics_core::time::ElapsedNs,
) -> Result<(), SnapshotError> {
    if actual == expected {
        Ok(())
    } else {
        invalid_field(path, "must equal state.through_elapsed_ns")
    }
}

fn invalid_field<T>(path: &str, requirement: &str) -> Result<T, SnapshotError> {
    Err(invalid_response(format!("{path} {requirement}")))
}

fn invalid_response(detail: impl Into<String>) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::InvalidResponse, detail)
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
                output.push_str("- ");
                match value {
                    Value::Object(_) | Value::Array(_) => {
                        output.push('\n');
                        write_human(value, output, indent + 2);
                    }
                    _ => {
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
