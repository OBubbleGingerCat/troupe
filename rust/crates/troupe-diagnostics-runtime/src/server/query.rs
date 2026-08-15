use std::{fmt, path::PathBuf, sync::Arc};

use hyper::{
    StatusCode,
    header::{ACCEPT, CONTENT_TYPE, HeaderValue},
};
use serde::Serialize;
use troupe_diagnostics_core::{id::CanonicalUuid, scalar::SchemaU64};

use crate::{
    archive::lease::ActiveArchiveLease,
    query::{
        events::{EventQueryError, FiniteEventQuery, query_events},
        reader::{CapturedEventSource, DiagnosticReader, ReaderFailure},
        snapshot::{SnapshotQueryError, SnapshotQueryErrorCode, project_snapshot},
        status::{
            self, ActiveStatusObservation, DiagnosticStatus, Observation, StatusProjectionError,
            project_status,
        },
    },
    store::{connection::StoreOpenErrorCode, key::SortableU64Key},
};

use super::{
    error::RouteConfigurationError,
    routes::{RouteDefinition, RouteRequest, RouteResponse},
};

pub const API_SCHEMA_VERSION: u8 = 1;
pub const STATUS_PATH: &str = "/api/v1/status";
pub const SNAPSHOT_PATH: &str = "/api/v1/snapshot";
pub const EVENTS_PATH: &str = "/api/v1/events";
pub const DEFAULT_EVENT_TAIL: u64 = 100;

type ActiveStatusProvider = dyn Fn() -> Option<ActiveStatusObservation> + Send + Sync + 'static;

#[derive(Clone)]
enum QueryTarget {
    Active {
        lease: Arc<ActiveArchiveLease>,
        status_provider: Arc<ActiveStatusProvider>,
    },
    Archive {
        run_directory: Arc<PathBuf>,
    },
}

#[derive(Clone)]
pub struct QueryEndpoints {
    run_id: CanonicalUuid,
    target: QueryTarget,
}

impl QueryEndpoints {
    pub fn active<F>(
        run_id: CanonicalUuid,
        lease: Arc<ActiveArchiveLease>,
        status_provider: F,
    ) -> Self
    where
        F: Fn() -> Option<ActiveStatusObservation> + Send + Sync + 'static,
    {
        Self {
            run_id,
            target: QueryTarget::Active {
                lease,
                status_provider: Arc::new(status_provider),
            },
        }
    }

    pub fn active_unobserved(run_id: CanonicalUuid, lease: Arc<ActiveArchiveLease>) -> Self {
        Self::active(run_id, lease, || None)
    }

    pub fn archive(run_id: CanonicalUuid, run_directory: impl Into<PathBuf>) -> Self {
        Self {
            run_id,
            target: QueryTarget::Archive {
                run_directory: Arc::new(run_directory.into()),
            },
        }
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub fn route_definitions(&self) -> Result<Vec<RouteDefinition>, RouteConfigurationError> {
        let status = self.clone();
        let snapshot = self.clone();
        let events = self.clone();
        Ok(vec![
            RouteDefinition::read_only(STATUS_PATH, move |request| {
                let endpoint = status.clone();
                async move { Ok(endpoint.handle_status(request)) }
            })?,
            RouteDefinition::read_only(SNAPSHOT_PATH, move |request| {
                let endpoint = snapshot.clone();
                async move { Ok(endpoint.handle_snapshot(request)) }
            })?,
            RouteDefinition::read_only(EVENTS_PATH, move |request| {
                let endpoint = events.clone();
                async move { Ok(endpoint.handle_finite_events(request)) }
            })?,
        ])
    }

    pub fn handle_status(&self, request: RouteRequest) -> RouteResponse {
        let result = validate_json_request(&request)
            .and_then(|()| validate_no_query(&request))
            .and_then(|()| {
                self.with_capture(|source, active| {
                    encode_status_response(self.run_id, source, active)
                })
                .map_err(ClientError::from_operation)
            });
        self.finish(result)
    }

    pub fn handle_snapshot(&self, request: RouteRequest) -> RouteResponse {
        let result = validate_json_request(&request)
            .and_then(|()| validate_no_query(&request))
            .and_then(|()| {
                self.with_capture(|source, _active| encode_snapshot_response(self.run_id, source))
                    .map_err(ClientError::from_operation)
            });
        self.finish(result)
    }

    pub fn handle_finite_events(&self, request: RouteRequest) -> RouteResponse {
        let result = validate_json_request(&request)
            .and_then(|()| parse_event_query(&request))
            .and_then(|query| {
                self.with_capture(|source, _active| {
                    encode_events_response(self.run_id, source, query)
                })
                .map_err(ClientError::from_operation)
            });
        self.finish(result)
    }

    fn finish(&self, result: Result<Vec<u8>, ClientError>) -> RouteResponse {
        match result {
            Ok(bytes) => json_bytes(StatusCode::OK, bytes),
            Err(error) => error.response(self.run_id),
        }
    }

    fn with_capture<T>(
        &self,
        operation: impl FnOnce(
            &CapturedEventSource<'_>,
            Option<&ActiveStatusObservation>,
        ) -> Result<T, QueryEndpointError>,
    ) -> Result<T, QueryEndpointError> {
        match &self.target {
            QueryTarget::Active {
                lease,
                status_provider,
            } => {
                let mut reader = DiagnosticReader::open_active(self.run_id, lease.guard())?;
                let captured = reader.capture()?;
                let active = status_provider();
                operation(&captured, active.as_ref())
            }
            QueryTarget::Archive { run_directory } => {
                let mut reader =
                    DiagnosticReader::open_archive(run_directory.as_ref(), self.run_id)?;
                let captured = reader.capture()?;
                operation(&captured, None)
            }
        }
    }
}

impl fmt::Debug for QueryEndpoints {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let profile = match &self.target {
            QueryTarget::Active { .. } => "active",
            QueryTarget::Archive { .. } => "archive",
        };
        formatter
            .debug_struct("QueryEndpoints")
            .field("run_id", &self.run_id)
            .field("profile", &profile)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum QueryEndpointError {
    Reader(ReaderFailure),
    Status(StatusProjectionError),
    Snapshot(SnapshotQueryError),
    Events(EventQueryError),
    IdentityMismatch {
        expected: CanonicalUuid,
        actual: CanonicalUuid,
    },
    SnapshotWatermarkMismatch,
    Json(serde_json::Error),
}

impl QueryEndpointError {
    fn store_code(&self) -> Option<StoreOpenErrorCode> {
        match self {
            Self::Reader(error) => error.store_code(),
            Self::Events(error) => error.reader_failure().and_then(ReaderFailure::store_code),
            _ => None,
        }
    }

    fn snapshot_code(&self) -> Option<SnapshotQueryErrorCode> {
        match self {
            Self::Snapshot(error) => Some(error.code()),
            _ => None,
        }
    }
}

impl fmt::Display for QueryEndpointError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reader(error) => fmt::Display::fmt(error, formatter),
            Self::Status(error) => fmt::Display::fmt(error, formatter),
            Self::Snapshot(error) => fmt::Display::fmt(error, formatter),
            Self::Events(error) => fmt::Display::fmt(error, formatter),
            Self::IdentityMismatch { expected, actual } => {
                write!(
                    formatter,
                    "query response belongs to Run {actual}, expected {expected}"
                )
            }
            Self::SnapshotWatermarkMismatch => formatter
                .write_str("snapshot state watermark does not match the response watermark"),
            Self::Json(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for QueryEndpointError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Reader(error) => Some(error),
            Self::Status(error) => Some(error),
            Self::Snapshot(error) => Some(error),
            Self::Events(error) => Some(error),
            Self::IdentityMismatch { .. } => None,
            Self::SnapshotWatermarkMismatch => None,
            Self::Json(error) => Some(error),
        }
    }
}

impl From<ReaderFailure> for QueryEndpointError {
    fn from(error: ReaderFailure) -> Self {
        Self::Reader(error)
    }
}

impl From<StatusProjectionError> for QueryEndpointError {
    fn from(error: StatusProjectionError) -> Self {
        Self::Status(error)
    }
}

impl From<SnapshotQueryError> for QueryEndpointError {
    fn from(error: SnapshotQueryError) -> Self {
        Self::Snapshot(error)
    }
}

impl From<EventQueryError> for QueryEndpointError {
    fn from(error: EventQueryError) -> Self {
        Self::Events(error)
    }
}

impl From<serde_json::Error> for QueryEndpointError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub fn encode_status_response(
    expected_run_id: CanonicalUuid,
    source: &CapturedEventSource<'_>,
    active: Option<&ActiveStatusObservation>,
) -> Result<Vec<u8>, QueryEndpointError> {
    let status = project_status(source, active)?;
    ensure_run_id(expected_run_id, status.identity().run_id())?;
    serde_json::to_vec(&StatusResponse::from_status(&status)).map_err(Into::into)
}

pub fn encode_snapshot_response(
    expected_run_id: CanonicalUuid,
    source: &CapturedEventSource<'_>,
) -> Result<Vec<u8>, QueryEndpointError> {
    let snapshot = project_snapshot(source)?;
    ensure_run_id(expected_run_id, snapshot.run_id())?;
    ensure_run_id(expected_run_id, snapshot.state().run_id())?;
    if snapshot.state().through_sequence() != snapshot.watermark_sequence() {
        return Err(QueryEndpointError::SnapshotWatermarkMismatch);
    }

    let earliest = snapshot
        .earliest_available_sequence()
        .map(|value| format!("\"{}\"", value.get()))
        .unwrap_or_else(|| "null".to_owned());
    let mut bytes = format!(
        "{{\"api_schema_version\":{API_SCHEMA_VERSION},\"run_id\":\"{}\",\"watermark_sequence\":\"{}\",\"earliest_available_sequence\":{earliest},\"state\":",
        snapshot.run_id(),
        snapshot.watermark_sequence().get(),
    )
    .into_bytes();
    bytes.extend_from_slice(snapshot.canonical_state());
    bytes.push(b'}');
    Ok(bytes)
}

pub fn encode_events_response(
    expected_run_id: CanonicalUuid,
    source: &CapturedEventSource<'_>,
    query: FiniteEventQuery,
) -> Result<Vec<u8>, QueryEndpointError> {
    ensure_run_id(expected_run_id, source.metadata().run_id())?;
    let mut events = query_events(source, query);
    let captured_watermark = events.range().captured_watermark();
    let mut bytes = format!(
        "{{\"api_schema_version\":{API_SCHEMA_VERSION},\"run_id\":\"{expected_run_id}\",\"captured_watermark\":\"{}\",\"events\":[",
        captured_watermark.get(),
    )
    .into_bytes();
    let mut first = true;
    for captured in &mut events {
        let captured = captured?;
        ensure_run_id(expected_run_id, captured.event().header().run_id())?;
        if !first {
            bytes.push(b',');
        }
        first = false;
        bytes.extend_from_slice(captured.canonical_bytes());
    }
    bytes.extend_from_slice(b"],\"next_after\":null}");
    Ok(bytes)
}

fn ensure_run_id(expected: CanonicalUuid, actual: CanonicalUuid) -> Result<(), QueryEndpointError> {
    if expected == actual {
        Ok(())
    } else {
        Err(QueryEndpointError::IdentityMismatch { expected, actual })
    }
}

#[derive(Serialize)]
struct StatusResponse<'a> {
    api_schema_version: u8,
    run_id: CanonicalUuid,
    source: &'static str,
    store_schema_version: SchemaU64,
    store_schema_identity: &'static str,
    event_schema_version: SchemaU64,
    configuration_identity: &'a str,
    event_watermark: SchemaU64,
    read_model_watermark: SchemaU64,
    lifecycle: LifecycleResponse<'a>,
    writer: ObservationResponse<WriterResponse>,
    quota: ObservationResponse<QuotaResponse>,
}

impl<'a> StatusResponse<'a> {
    fn from_status(status: &'a DiagnosticStatus) -> Self {
        let identity = status.identity();
        Self {
            api_schema_version: API_SCHEMA_VERSION,
            run_id: identity.run_id(),
            source: identity.source().as_str(),
            store_schema_version: identity.store_schema_version(),
            store_schema_identity: identity.store_schema_identity(),
            event_schema_version: identity.event_schema_version(),
            configuration_identity: status.configuration_identity(),
            event_watermark: status.event_watermark(),
            read_model_watermark: status.read_model_watermark(),
            lifecycle: LifecycleResponse::from_status(status),
            writer: ObservationResponse::new(status.writer(), WriterResponse::from_status),
            quota: ObservationResponse::new(status.quota(), QuotaResponse::from_status),
        }
    }
}

#[derive(Serialize)]
struct LifecycleResponse<'a> {
    state: &'static str,
    started_at: &'a str,
    ended_at: Option<&'a str>,
    outcome: Option<&'static str>,
    clean_shutdown: bool,
}

impl<'a> LifecycleResponse<'a> {
    fn from_status(status: &'a DiagnosticStatus) -> Self {
        let lifecycle = status.lifecycle();
        Self {
            state: lifecycle.state().as_str(),
            started_at: lifecycle.started_at(),
            ended_at: lifecycle.ended_at(),
            outcome: lifecycle.outcome().map(status::ProductionOutcome::as_str),
            clean_shutdown: lifecycle.clean_shutdown(),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ObservationResponse<T> {
    Available { value: T },
    Unavailable { reason: &'static str },
}

impl<T> ObservationResponse<T> {
    fn new<U>(observation: &Observation<U>, project: impl FnOnce(&U) -> T) -> Self {
        match observation {
            Observation::Available(value) => Self::Available {
                value: project(value),
            },
            Observation::Unavailable(reason) => Self::Unavailable {
                reason: reason.as_str(),
            },
        }
    }
}

#[derive(Clone, Copy, Serialize)]
struct DurationResponse {
    seconds: SchemaU64,
    subsecond_nanoseconds: SchemaU64,
}

impl From<status::CanonicalDuration> for DurationResponse {
    fn from(value: status::CanonicalDuration) -> Self {
        Self {
            seconds: value.seconds(),
            subsecond_nanoseconds: value.subsecond_nanoseconds(),
        }
    }
}

#[derive(Serialize)]
struct WriterResponse {
    max_uncommitted_events: SchemaU64,
    max_uncommitted_canonical_bytes: SchemaU64,
    max_batch_age: DurationResponse,
    max_batch_events: SchemaU64,
    max_batch_canonical_bytes: SchemaU64,
    accepted_uncommitted_events: SchemaU64,
    accepted_uncommitted_canonical_bytes: SchemaU64,
    queued_events: SchemaU64,
    in_flight_events: SchemaU64,
    ingress_committed_watermark: SchemaU64,
    normal_ingress_sealed: bool,
    ingress_failure: Option<IngressFailureResponse>,
    writer_stall_timeout: DurationResponse,
    shutdown_drain_timeout: DurationResponse,
    progress_committed_watermark: SchemaU64,
    accepted_tail_events: SchemaU64,
    stalled_for: Option<DurationResponse>,
    drain_state: &'static str,
    progress_failure: Option<WriterFailureResponse>,
}

impl WriterResponse {
    fn from_status(value: &status::WriterStatus) -> Self {
        Self {
            max_uncommitted_events: value.max_uncommitted_events(),
            max_uncommitted_canonical_bytes: value.max_uncommitted_canonical_bytes(),
            max_batch_age: value.max_batch_age().into(),
            max_batch_events: value.max_batch_events(),
            max_batch_canonical_bytes: value.max_batch_canonical_bytes(),
            accepted_uncommitted_events: value.accepted_uncommitted_events(),
            accepted_uncommitted_canonical_bytes: value.accepted_uncommitted_canonical_bytes(),
            queued_events: value.queued_events(),
            in_flight_events: value.in_flight_events(),
            ingress_committed_watermark: value.ingress_committed_watermark(),
            normal_ingress_sealed: value.normal_ingress_sealed(),
            ingress_failure: value
                .ingress_failure()
                .map(IngressFailureResponse::from_status),
            writer_stall_timeout: value.writer_stall_timeout().into(),
            shutdown_drain_timeout: value.shutdown_drain_timeout().into(),
            progress_committed_watermark: value.progress_committed_watermark(),
            accepted_tail_events: value.accepted_tail_events(),
            stalled_for: value.stalled_for().map(Into::into),
            drain_state: value.drain_state().as_str(),
            progress_failure: value
                .progress_failure()
                .map(WriterFailureResponse::from_status),
        }
    }
}

#[derive(Serialize)]
struct IngressFailureResponse {
    code: &'static str,
    current_events: SchemaU64,
    current_canonical_bytes: SchemaU64,
    attempted_events: SchemaU64,
    attempted_canonical_bytes: SchemaU64,
    event_limit_exceeded: bool,
    byte_limit_exceeded: bool,
}

impl IngressFailureResponse {
    fn from_status(value: &status::IngressFailureStatus) -> Self {
        Self {
            code: value.code(),
            current_events: value.current_events(),
            current_canonical_bytes: value.current_canonical_bytes(),
            attempted_events: value.attempted_events(),
            attempted_canonical_bytes: value.attempted_canonical_bytes(),
            event_limit_exceeded: value.event_limit_exceeded(),
            byte_limit_exceeded: value.byte_limit_exceeded(),
        }
    }
}

#[derive(Serialize)]
struct WriterFailureResponse {
    component: &'static str,
    stage: &'static str,
    code: &'static str,
}

impl WriterFailureResponse {
    fn from_status(value: status::WriterFailureStatus) -> Self {
        Self {
            component: value.component(),
            stage: value.stage(),
            code: value.code(),
        }
    }
}

#[derive(Serialize)]
struct QuotaResponse {
    max_run_bytes: Option<SchemaU64>,
    current_measured_bytes: Option<SchemaU64>,
    last_measurement_at: Option<DurationResponse>,
    sealed: bool,
    failure: Option<QuotaFailureResponse>,
}

impl QuotaResponse {
    fn from_status(value: &status::QuotaProjection) -> Self {
        Self {
            max_run_bytes: value.max_run_bytes(),
            current_measured_bytes: value.current_measured_bytes(),
            last_measurement_at: value.last_measurement_at().map(Into::into),
            sealed: value.sealed(),
            failure: value.failure().map(QuotaFailureResponse::from_status),
        }
    }
}

#[derive(Serialize)]
struct QuotaFailureResponse {
    code: &'static str,
    limit_bytes: SchemaU64,
    current_bytes: Option<SchemaU64>,
    predicted_growth_bytes: Option<SchemaU64>,
}

impl QuotaFailureResponse {
    fn from_status(value: &status::QuotaFailureStatus) -> Self {
        Self {
            code: value.code(),
            limit_bytes: value.limit_bytes(),
            current_bytes: value.current_bytes(),
            predicted_growth_bytes: value.predicted_growth_bytes(),
        }
    }
}

#[derive(Clone, Copy)]
struct ClientError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
}

impl ClientError {
    const fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
        }
    }

    fn from_operation(error: QueryEndpointError) -> Self {
        if error.store_code() == Some(StoreOpenErrorCode::RunIdentityMismatch)
            || error.snapshot_code() == Some(SnapshotQueryErrorCode::ModelIdentityMismatch)
            || matches!(error, QueryEndpointError::IdentityMismatch { .. })
        {
            return Self::new(
                StatusCode::CONFLICT,
                "run_identity_mismatch",
                "the query source does not match this Run",
            );
        }
        if matches!(
            error.store_code(),
            Some(StoreOpenErrorCode::NewerSchema | StoreOpenErrorCode::SchemaMismatch)
        ) || error.snapshot_code() == Some(SnapshotQueryErrorCode::ModelSchemaMismatch)
        {
            return Self::new(
                StatusCode::CONFLICT,
                "incompatible_schema",
                "the query source uses an incompatible schema",
            );
        }
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "query_failed",
            "the diagnostic query could not be completed",
        )
    }

    fn response(self, run_id: CanonicalUuid) -> RouteResponse {
        let bytes = serde_json::to_vec(&ErrorResponse {
            api_schema_version: API_SCHEMA_VERSION,
            run_id: Some(run_id),
            error: ErrorBody {
                code: self.code,
                message: self.message,
                details: None,
            },
        })
        .expect("closed HTTP errors are JSON serializable");
        json_bytes(self.status, bytes)
    }
}

#[derive(Serialize)]
struct ErrorResponse {
    api_schema_version: u8,
    run_id: Option<CanonicalUuid>,
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: &'static str,
    details: Option<()>,
}

fn validate_json_request(request: &RouteRequest) -> Result<(), ClientError> {
    let mut saw_accept = false;
    for value in request.headers().get_all(ACCEPT) {
        saw_accept = true;
        let Ok(value) = value.to_str() else {
            continue;
        };
        for item in value.split(',') {
            let mut parts = item.split(';');
            let media_type = parts.next().unwrap_or_default().trim();
            let quality_is_zero = parts.any(|parameter| {
                parameter
                    .trim()
                    .strip_prefix("q=")
                    .and_then(|quality| quality.parse::<f32>().ok())
                    == Some(0.0)
            });
            if !quality_is_zero && matches!(media_type, "application/json" | "*/*") {
                return Ok(());
            }
        }
    }
    if saw_accept {
        Err(ClientError::new(
            StatusCode::NOT_ACCEPTABLE,
            "unsupported_format",
            "only application/json is available for finite queries",
        ))
    } else {
        Ok(())
    }
}

fn validate_no_query(request: &RouteRequest) -> Result<(), ClientError> {
    if request.uri().query().is_none_or(str::is_empty) {
        Ok(())
    } else {
        Err(invalid_query())
    }
}

fn parse_event_query(request: &RouteRequest) -> Result<FiniteEventQuery, ClientError> {
    let mut after = None;
    let mut tail = None;
    let Some(query) = request.uri().query().filter(|query| !query.is_empty()) else {
        return Ok(FiniteEventQuery::tail(SchemaU64::new(DEFAULT_EVENT_TAIL)));
    };
    for parameter in query.split('&') {
        let Some((name, value)) = parameter.split_once('=') else {
            return Err(invalid_query());
        };
        if name.is_empty()
            || value.is_empty()
            || name.bytes().any(|byte| matches!(byte, b'%' | b'+'))
            || value.bytes().any(|byte| matches!(byte, b'%' | b'+'))
        {
            return Err(invalid_query());
        }
        match name {
            "after" if after.is_none() => after = Some(parse_cursor(value)?),
            "tail" if tail.is_none() => tail = Some(parse_cursor(value)?),
            "after" | "tail" => return Err(invalid_query()),
            "format" => {
                return Err(ClientError::new(
                    StatusCode::NOT_ACCEPTABLE,
                    "unsupported_format",
                    "finite events are returned as application/json",
                ));
            }
            _ => return Err(invalid_query()),
        }
    }
    match (after, tail) {
        (Some(_), Some(_)) => Err(ClientError::new(
            StatusCode::BAD_REQUEST,
            "conflicting_event_query",
            "after and tail are mutually exclusive",
        )),
        (Some(after), None) => Ok(FiniteEventQuery::after(after)),
        (None, Some(tail)) => Ok(FiniteEventQuery::tail(tail)),
        (None, None) => Ok(FiniteEventQuery::tail(SchemaU64::new(DEFAULT_EVENT_TAIL))),
    }
}

fn parse_cursor(value: &str) -> Result<SchemaU64, ClientError> {
    SortableU64Key::parse_canonical_decimal(value)
        .map(|value| SchemaU64::new(value.get()))
        .map_err(|_| {
            ClientError::new(
                StatusCode::BAD_REQUEST,
                "invalid_cursor",
                "event cursor must be a canonical decimal u64",
            )
        })
}

const fn invalid_query() -> ClientError {
    ClientError::new(
        StatusCode::BAD_REQUEST,
        "invalid_query",
        "query parameters are invalid",
    )
}

fn json_bytes(status: StatusCode, bytes: Vec<u8>) -> RouteResponse {
    RouteResponse::bytes(status, bytes).with_header(
        CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    )
}
