#![allow(dead_code)] // D07 wires this command into the application entry point.

use std::{fmt, path::Path};

use reqwest::header::HeaderMap;
use serde::Serialize;
use tokio::io::{AsyncWrite, AsyncWriteExt as _};
use tokio_util::sync::CancellationToken;
use troupe_diagnostics_core::{event::EVENT_SCHEMA_VERSION, id::CanonicalUuid, scalar::SchemaU64};
use troupe_diagnostics_perfetto::{
    atomic_file::{
        NodeKind, NodeObservation, PublicationCancellation, PublicationFailure, PublicationReport,
        PublicationState, TraceProducerFuture, TraceStreamProducer, publish_atomic_trace,
    },
    collect::{PERFETTO_EXPORTER_SCHEMA_VERSION, ProjectionMetadata, TRACE_CONTENT_WARNING},
    dump::{DumpError as PerfettoDumpError, TraceBodyValidator, dump_captured_prefix_with_version},
};
use troupe_diagnostics_runtime::{
    query::reader::CapturedEventSource,
    server::dump::{
        DUMP_API_SCHEMA_VERSION, DUMP_API_SCHEMA_VERSION_HEADER, DUMP_CAPTURED_WATERMARK_HEADER,
        DUMP_CLEAN_SHUTDOWN_HEADER, DUMP_CONTENT_WARNING_HEADER, DUMP_EVENT_SCHEMA_VERSION_HEADER,
        DUMP_EXPORTED_THROUGH_HEADER, DUMP_EXPORTER_SCHEMA_VERSION_HEADER, DUMP_PATH,
        DUMP_PRODUCTION_OUTCOME_HEADER, DUMP_RUN_ID_HEADER, DUMP_TROUPE_VERSION_HEADER,
        PERFETTO_TRACE_MIME,
    },
};

use super::{
    archive_target::ArchiveTarget,
    args::DumpArgs,
    http_client::DiagnosticHttpClient,
    resolver::{ResolvedDiagnosticTarget, ResolverError, resolve},
    values::CanonicalU64,
};

const REPORT_SCHEMA_VERSION: u8 = 1;
const MAX_REMOTE_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DumpErrorCode {
    Resolve,
    ArchiveRead,
    Output,
    PublicationFailed,
    PublicationIndeterminate,
    InternalInvariant,
}

impl DumpErrorCode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Resolve => "diagnostic_dump.resolve",
            Self::ArchiveRead => "diagnostic_dump.archive_read",
            Self::Output => "diagnostic_dump.output",
            Self::PublicationFailed => "diagnostic_dump.publication_failed",
            Self::PublicationIndeterminate => "diagnostic_dump.publication_indeterminate",
            Self::InternalInvariant => "diagnostic_dump.internal_invariant",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DumpCommandError {
    code: DumpErrorCode,
    detail: String,
}

impl DumpCommandError {
    fn new(code: DumpErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    fn resolver(error: ResolverError) -> Self {
        Self::new(DumpErrorCode::Resolve, error.to_string())
    }

    fn output(error: impl fmt::Display) -> Self {
        Self::new(DumpErrorCode::Output, error.to_string())
    }

    pub(crate) const fn code(&self) -> DumpErrorCode {
        self.code
    }
}

impl fmt::Display for DumpCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.detail)
    }
}

impl std::error::Error for DumpCommandError {}

pub(crate) trait DumpOutput {
    type Error: fmt::Display;

    fn write_stderr(&mut self, text: &str) -> Result<(), Self::Error>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DumpTermination {
    Published,
    Interrupted,
}

impl DumpTermination {
    pub(crate) const fn exit_code(self) -> u8 {
        match self {
            Self::Published => 0,
            Self::Interrupted => 130,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DumpSuccess {
    run_id: CanonicalUuid,
    captured_watermark: SchemaU64,
    exported_through: SchemaU64,
    event_count: u64,
    content_warning: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TraceSourceError {
    pub(crate) code: &'static str,
    detail: String,
}

impl TraceSourceError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for TraceSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

struct LocalDumpProducer<'source, 'transaction> {
    source: &'source CapturedEventSource<'transaction>,
    through: Option<SchemaU64>,
}

impl TraceStreamProducer for LocalDumpProducer<'_, '_> {
    type Summary = DumpSuccess;
    type Error = TraceSourceError;

    fn produce<'operation>(
        &'operation mut self,
        writer: &'operation mut (dyn AsyncWrite + Unpin),
    ) -> TraceProducerFuture<'operation, Self::Summary, Self::Error> {
        Box::pin(async move {
            let summary = dump_captured_prefix_with_version(
                self.source,
                writer,
                self.through,
                env!("CARGO_PKG_VERSION"),
            )
            .await
            .map_err(local_dump_error)?;
            Ok(DumpSuccess {
                run_id: self.source.metadata().run_id(),
                captured_watermark: summary.captured_watermark(),
                exported_through: summary.exported_through(),
                event_count: summary.event_count(),
                content_warning: TRACE_CONTENT_WARNING.to_owned(),
            })
        })
    }
}

fn local_dump_error(error: PerfettoDumpError) -> TraceSourceError {
    TraceSourceError::new("local_export_failed", error.to_string())
}

struct RemoteDumpProducer {
    client: DiagnosticHttpClient,
    through: Option<SchemaU64>,
}

impl RemoteDumpProducer {
    fn new(client: DiagnosticHttpClient, through: Option<SchemaU64>) -> Self {
        Self { client, through }
    }

    async fn stream(
        &self,
        writer: &mut (dyn AsyncWrite + Unpin),
    ) -> Result<DumpSuccess, TraceSourceError> {
        self.client
            .revalidate_identity()
            .await
            .map_err(|error| remote_error("identity_before_request", error))?;
        let path = self.through.map_or_else(
            || DUMP_PATH.to_owned(),
            |through| format!("{DUMP_PATH}?through={}", through.get()),
        );
        let request = self
            .client
            .get(&path)
            .map_err(|error| remote_error("invalid_endpoint", error))?
            .header(reqwest::header::ACCEPT, PERFETTO_TRACE_MIME);
        let mut response = request
            .send()
            .await
            .map_err(|error| remote_error("request_failed", error))?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(TraceSourceError::new(
                "unexpected_status",
                format!("dump endpoint returned HTTP {}", response.status()),
            ));
        }
        let metadata =
            validate_remote_metadata(response.headers(), self.client.run_id(), self.through)?;
        let mut trace_validator = TraceBodyValidator::new(metadata.trace_metadata());
        let mut received_body = false;
        loop {
            let chunk = response
                .chunk()
                .await
                .map_err(|error| remote_error("body_failed", error))?;
            let Some(chunk) = chunk else {
                break;
            };
            if chunk.is_empty() {
                continue;
            }
            if chunk.len() > MAX_REMOTE_CHUNK_BYTES {
                return Err(TraceSourceError::new(
                    "body_chunk_too_large",
                    format!(
                        "dump response chunk has {} bytes, limit is {MAX_REMOTE_CHUNK_BYTES}",
                        chunk.len()
                    ),
                ));
            }
            trace_validator
                .push(&chunk)
                .map_err(remote_trace_validation_error)?;
            writer
                .write_all(&chunk)
                .await
                .map_err(|error| remote_error("output_stream_failed", error))?;
            received_body = true;
        }
        if !received_body {
            return Err(TraceSourceError::new(
                "empty_body",
                "dump endpoint returned an empty trace body",
            ));
        }
        trace_validator
            .finish()
            .map_err(remote_trace_validation_error)?;
        self.client
            .revalidate_identity()
            .await
            .map_err(|error| remote_error("identity_after_body", error))?;
        Ok(DumpSuccess {
            run_id: metadata.run_id,
            captured_watermark: metadata.captured_watermark,
            exported_through: metadata.exported_through,
            event_count: metadata.exported_through.get(),
            content_warning: metadata.content_warning,
        })
    }
}

impl TraceStreamProducer for RemoteDumpProducer {
    type Summary = DumpSuccess;
    type Error = TraceSourceError;

    fn produce<'operation>(
        &'operation mut self,
        writer: &'operation mut (dyn AsyncWrite + Unpin),
    ) -> TraceProducerFuture<'operation, Self::Summary, Self::Error> {
        Box::pin(self.stream(writer))
    }
}

fn remote_error(code: &'static str, error: impl fmt::Display) -> TraceSourceError {
    TraceSourceError::new(code, error.to_string())
}

fn remote_trace_validation_error(
    error: troupe_diagnostics_perfetto::dump::TraceBodyValidationError,
) -> TraceSourceError {
    TraceSourceError::new(error.code(), error.to_string())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RemoteMetadata {
    pub(crate) run_id: CanonicalUuid,
    pub(crate) captured_watermark: SchemaU64,
    pub(crate) exported_through: SchemaU64,
    troupe_version: String,
    production_outcome: Option<String>,
    clean_shutdown: Option<bool>,
    content_warning: String,
}

impl RemoteMetadata {
    fn trace_metadata(&self) -> ProjectionMetadata {
        ProjectionMetadata::new(
            self.run_id,
            self.captured_watermark,
            self.exported_through,
            self.troupe_version.clone(),
        )
        .with_completion(self.production_outcome.clone(), self.clean_shutdown)
    }
}

pub(crate) fn validate_remote_metadata(
    headers: &HeaderMap,
    expected_run_id: CanonicalUuid,
    requested_through: Option<SchemaU64>,
) -> Result<RemoteMetadata, TraceSourceError> {
    let content_type = required_header(headers, reqwest::header::CONTENT_TYPE.as_str())?;
    if content_type != PERFETTO_TRACE_MIME {
        return metadata_error("content type is not application/x-protobuf");
    }
    if headers.contains_key(reqwest::header::CONTENT_ENCODING) {
        return metadata_error("content encoding is not allowed for a Perfetto dump");
    }
    let run_id = CanonicalUuid::parse(required_header(headers, DUMP_RUN_ID_HEADER)?)
        .map_err(|_| TraceSourceError::new("metadata_mismatch", "dump Run ID is not canonical"))?;
    if run_id != expected_run_id {
        return metadata_error("dump Run ID differs from the resolved server identity");
    }
    let captured_watermark = parse_header_u64(headers, DUMP_CAPTURED_WATERMARK_HEADER)?;
    let exported_through = parse_header_u64(headers, DUMP_EXPORTED_THROUGH_HEADER)?;
    if exported_through.get() > captured_watermark.get() {
        return metadata_error("exported watermark exceeds the captured watermark");
    }
    match requested_through {
        Some(requested) if requested != exported_through => {
            return metadata_error("exported watermark differs from requested through");
        }
        None if exported_through != captured_watermark => {
            return metadata_error("default dump did not export its captured head");
        }
        Some(_) | None => {}
    }
    require_exact_u8(
        headers,
        DUMP_API_SCHEMA_VERSION_HEADER,
        DUMP_API_SCHEMA_VERSION,
    )?;
    require_exact_u8(
        headers,
        DUMP_EVENT_SCHEMA_VERSION_HEADER,
        EVENT_SCHEMA_VERSION,
    )?;
    require_exact_u8(
        headers,
        DUMP_EXPORTER_SCHEMA_VERSION_HEADER,
        PERFETTO_EXPORTER_SCHEMA_VERSION,
    )?;
    let troupe_version = required_header(headers, DUMP_TROUPE_VERSION_HEADER)?;
    if troupe_version.is_empty() || troupe_version.chars().any(char::is_control) {
        return metadata_error("Troupe version metadata is invalid");
    }
    let production_outcome = required_header(headers, DUMP_PRODUCTION_OUTCOME_HEADER)?;
    let clean_shutdown = required_header(headers, DUMP_CLEAN_SHUTDOWN_HEADER)?;
    if !matches!(
        (production_outcome, clean_shutdown),
        ("unavailable", "unavailable") | ("completed" | "failed" | "cancelled", "true" | "false")
    ) {
        return metadata_error("production outcome and clean-shutdown metadata are inconsistent");
    }
    let content_warning = required_header(headers, DUMP_CONTENT_WARNING_HEADER)?;
    if content_warning != TRACE_CONTENT_WARNING {
        return metadata_error("content warning differs from the supported exporter contract");
    }
    let (production_outcome, clean_shutdown) = match (production_outcome, clean_shutdown) {
        ("unavailable", "unavailable") => (None, None),
        (outcome, clean) => (Some(outcome.to_owned()), Some(clean == "true")),
    };
    Ok(RemoteMetadata {
        run_id,
        captured_watermark,
        exported_through,
        troupe_version: troupe_version.to_owned(),
        production_outcome,
        clean_shutdown,
        content_warning: content_warning.to_owned(),
    })
}

fn required_header<'headers>(
    headers: &'headers HeaderMap,
    name: &'static str,
) -> Result<&'headers str, TraceSourceError> {
    let mut values = headers.get_all(name).iter();
    let value = values.next().ok_or_else(|| {
        TraceSourceError::new("metadata_mismatch", format!("missing dump header {name}"))
    })?;
    if values.next().is_some() {
        return metadata_error(format!("dump header {name} appeared more than once"));
    }
    value.to_str().map_err(|_| {
        TraceSourceError::new(
            "metadata_mismatch",
            format!("dump header {name} is not valid ASCII"),
        )
    })
}

fn parse_header_u64(
    headers: &HeaderMap,
    name: &'static str,
) -> Result<SchemaU64, TraceSourceError> {
    required_header(headers, name)?
        .parse::<CanonicalU64>()
        .map(|value| SchemaU64::new(value.get()))
        .map_err(|_| {
            TraceSourceError::new(
                "metadata_mismatch",
                format!("dump header {name} is not a canonical decimal u64"),
            )
        })
}

fn require_exact_u8(
    headers: &HeaderMap,
    name: &'static str,
    expected: u8,
) -> Result<(), TraceSourceError> {
    if required_header(headers, name)? == expected.to_string() {
        Ok(())
    } else {
        metadata_error(format!("dump header {name} is incompatible"))
    }
}

fn metadata_error<T>(detail: impl Into<String>) -> Result<T, TraceSourceError> {
    Err(TraceSourceError::new("metadata_mismatch", detail))
}

pub(crate) async fn execute<O>(
    arguments: DumpArgs,
    output: &mut O,
    cancellation: CancellationToken,
) -> Result<DumpTermination, DumpCommandError>
where
    O: DumpOutput,
{
    let (target, output_path, through, force) = arguments.into_parts();
    let through = through.map(|value| SchemaU64::new(value.get()));
    let target = tokio::select! {
        () = cancellation.cancelled() => {
            write_prepublication_cancellation(output, &output_path)?;
            return Ok(DumpTermination::Interrupted);
        }
        target = resolve(target) => target.map_err(DumpCommandError::resolver)?,
    };
    let report = match target {
        ResolvedDiagnosticTarget::Archive(mut archive) => {
            publish_archive(&mut archive, &output_path, through, force, &cancellation).await?
        }
        ResolvedDiagnosticTarget::Live(client) => {
            let mut producer = RemoteDumpProducer::new(client, through);
            publish(&output_path, force, &cancellation, &mut producer).await
        }
    };
    finish_report(output, &report, cancellation.is_cancelled())
}

async fn publish_archive(
    archive: &mut ArchiveTarget,
    output: &Path,
    through: Option<SchemaU64>,
    force: bool,
    cancellation: &CancellationToken,
) -> Result<PublicationReport<DumpSuccess, TraceSourceError>, DumpCommandError> {
    let source = archive
        .capture()
        .map_err(|error| DumpCommandError::new(DumpErrorCode::ArchiveRead, error.to_string()))?;
    let mut producer = LocalDumpProducer {
        source: &source,
        through,
    };
    Ok(publish(output, force, cancellation, &mut producer).await)
}

async fn publish<Producer>(
    output: &Path,
    force: bool,
    cancellation: &CancellationToken,
    producer: &mut Producer,
) -> PublicationReport<Producer::Summary, Producer::Error>
where
    Producer: TraceStreamProducer,
{
    let publication_cancellation = PublicationCancellation::default();
    if cancellation.is_cancelled() {
        publication_cancellation.cancel();
    }
    let operation = publish_atomic_trace(output, force, &publication_cancellation, producer);
    tokio::pin!(operation);
    tokio::select! {
        report = &mut operation => report,
        () = cancellation.cancelled() => {
            publication_cancellation.cancel();
            operation.await
        }
    }
}

fn finish_report<O>(
    output: &mut O,
    report: &PublicationReport<DumpSuccess, TraceSourceError>,
    interrupted: bool,
) -> Result<DumpTermination, DumpCommandError>
where
    O: DumpOutput,
{
    let line = publication_line(report)?;
    output
        .write_stderr(&line)
        .map_err(DumpCommandError::output)?;
    match report.state() {
        PublicationState::Published if interrupted => Ok(DumpTermination::Interrupted),
        PublicationState::Published => Ok(DumpTermination::Published),
        PublicationState::NotPublished | PublicationState::PublicationIndeterminate
            if interrupted
                || matches!(report.failure(), Some(PublicationFailure::Cancelled { .. })) =>
        {
            Ok(DumpTermination::Interrupted)
        }
        PublicationState::NotPublished => Err(DumpCommandError::new(
            DumpErrorCode::PublicationFailed,
            failure_detail(report),
        )),
        PublicationState::PublicationIndeterminate => Err(DumpCommandError::new(
            DumpErrorCode::PublicationIndeterminate,
            failure_detail(report),
        )),
    }
}

fn failure_detail(report: &PublicationReport<DumpSuccess, TraceSourceError>) -> String {
    report.failure().map_or_else(
        || format!("trace publication stopped at {}", report.phase().as_str()),
        |failure| failure.to_string(),
    )
}

#[derive(Serialize)]
struct PublicationRecord {
    report_schema_version: u8,
    publication: &'static str,
    phase: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    captured_watermark: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exported_through: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_count: Option<String>,
    output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uncertainty: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    manual_check_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    observations: Option<PublicationObservationRecord>,
}

#[derive(Serialize)]
struct PublicationObservationRecord {
    target: NodeObservationRecord,
    temp: NodeObservationRecord,
    backup: NodeObservationRecord,
}

#[derive(Serialize)]
struct NodeObservationRecord {
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inode: Option<String>,
}

impl NodeObservationRecord {
    fn from_observation(observation: NodeObservation) -> Self {
        match observation {
            NodeObservation::Absent => Self {
                state: "absent",
                kind: None,
                device: None,
                inode: None,
            },
            NodeObservation::Unknown => Self {
                state: "unknown",
                kind: None,
                device: None,
                inode: None,
            },
            NodeObservation::Present(metadata) => Self {
                state: "present",
                kind: Some(match metadata.kind() {
                    NodeKind::Directory => "directory",
                    NodeKind::RegularFile => "regular_file",
                    NodeKind::Symlink => "symlink",
                    NodeKind::Other => "other",
                }),
                device: Some(metadata.identity().device().to_string()),
                inode: Some(metadata.identity().inode().to_string()),
            },
        }
    }
}

fn publication_line(
    report: &PublicationReport<DumpSuccess, TraceSourceError>,
) -> Result<String, DumpCommandError> {
    if report.state() == PublicationState::Published && report.summary().is_none() {
        return Err(DumpCommandError::new(
            DumpErrorCode::InternalInvariant,
            "published trace has no producer summary",
        ));
    }
    let summary = report.summary();
    let indeterminate = report.state() == PublicationState::PublicationIndeterminate;
    let observations = report.observations();
    if report.state() == PublicationState::Published
        && (!matches!(observations.target(), NodeObservation::Present(metadata) if metadata.kind() == NodeKind::RegularFile)
            || observations.temp() != NodeObservation::Absent
            || observations.backup() != NodeObservation::Absent)
    {
        return Err(DumpCommandError::new(
            DumpErrorCode::InternalInvariant,
            "published trace did not reach a residue-free regular-file state",
        ));
    }
    let record = PublicationRecord {
        report_schema_version: REPORT_SCHEMA_VERSION,
        publication: report.state().as_str(),
        phase: report.phase().as_str(),
        run_id: summary.map(|summary| summary.run_id.to_string()),
        captured_watermark: summary.map(|summary| summary.captured_watermark.get().to_string()),
        exported_through: summary.map(|summary| summary.exported_through.get().to_string()),
        event_count: summary.map(|summary| summary.event_count.to_string()),
        output: path_text(report.paths().target()),
        content_warning: summary.map(|summary| summary.content_warning.clone()),
        failure_code: report.failure().map(publication_failure_code),
        failure: report.failure().map(ToString::to_string),
        uncertainty: report
            .uncertainty()
            .map(|uncertainty| uncertainty.detail().to_owned()),
        manual_check_paths: if indeterminate {
            manual_check_paths(report)
        } else {
            Vec::new()
        },
        observations: indeterminate.then(|| PublicationObservationRecord {
            target: NodeObservationRecord::from_observation(observations.target()),
            temp: NodeObservationRecord::from_observation(observations.temp()),
            backup: NodeObservationRecord::from_observation(observations.backup()),
        }),
    };
    serde_json::to_string(&record)
        .map(|encoded| format!("troupe: diagnostic dump {encoded}\n"))
        .map_err(|error| {
            DumpCommandError::new(
                DumpErrorCode::Output,
                format!("dump report encoding failed: {error}"),
            )
        })
}

fn publication_failure_code(failure: &PublicationFailure<TraceSourceError>) -> &'static str {
    match failure {
        PublicationFailure::InvalidOutputPath(_) => "invalid_output_path",
        PublicationFailure::TargetAlreadyExists(_) => "target_already_exists",
        PublicationFailure::TargetTypeRejected(_) => "target_type_rejected",
        PublicationFailure::TemporaryTypeRejected(_) => "temporary_type_rejected",
        PublicationFailure::Cancelled { .. } => "cancelled",
        PublicationFailure::Producer(error) => error.code,
        PublicationFailure::Io { .. } => "io_failed",
        PublicationFailure::IdentityChanged { .. } => "identity_changed",
    }
}

fn manual_check_paths(report: &PublicationReport<DumpSuccess, TraceSourceError>) -> Vec<String> {
    let mut paths = vec![path_text(report.paths().target())];
    if let Some(path) = report.paths().temp() {
        paths.push(path_text(path));
    }
    if let Some(path) = report.paths().backup() {
        paths.push(path_text(path));
    }
    paths
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn write_prepublication_cancellation<O>(
    output: &mut O,
    output_path: &Path,
) -> Result<(), DumpCommandError>
where
    O: DumpOutput,
{
    #[derive(Serialize)]
    struct PrepublicationRecord {
        report_schema_version: u8,
        publication: &'static str,
        phase: &'static str,
        output: String,
        failure_code: &'static str,
        failure: &'static str,
    }

    let record = PrepublicationRecord {
        report_schema_version: REPORT_SCHEMA_VERSION,
        publication: PublicationState::NotPublished.as_str(),
        phase: "resolve_target",
        output: path_text(output_path),
        failure_code: "cancelled",
        failure: "dump command was interrupted before target resolution",
    };
    let line = serde_json::to_string(&record)
        .map(|encoded| format!("troupe: diagnostic dump {encoded}\n"))
        .map_err(|error| {
            DumpCommandError::new(
                DumpErrorCode::Output,
                format!("dump cancellation report encoding failed: {error}"),
            )
        })?;
    output.write_stderr(&line).map_err(DumpCommandError::output)
}
