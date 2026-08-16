use std::{
    collections::BTreeMap,
    fmt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use hyper::{
    StatusCode,
    header::{ACCEPT, CONTENT_TYPE, HeaderValue},
};
use serde::Serialize;
use troupe_diagnostics_core::{
    event::DiagnosticScope,
    id::{CanonicalUuid, RunLocalId},
    scalar::SchemaU64,
    view_protocol::{
        API_SCHEMA_VERSION, Coverage, CoverageStatus, ExcludedCounts, IncompatibilityReason,
        IncompatibleView, MAX_PAGE_ROWS, OpaqueCursor, OperationalCapabilities, Pagination,
        QueryBinding, Renderer, ResultMetadata, ScopeMode, TableColumn, TimeRangeMode, ViewRecord,
        ViewResponse, expected_bucket_width_ns,
    },
};

use crate::{
    archive::lease::ActiveArchiveLease,
    query::{
        archive_views::{
            ArchiveViewLoadError, ArchiveViewLoadErrorCode, StoredViewAvailability,
            StoredViewRecord, load_stored_view_records,
        },
        reader::{
            CapturedEventSource, DiagnosticReader, ReaderErrorCode, ReaderFailure,
            ReaderFailureClass, ReaderProfile,
        },
        views::{
            ViewQueryEngine, ViewQueryError, ViewQueryErrorClass, ViewQueryErrorCode,
            ViewQueryRequest, Viewport,
        },
    },
    store::{connection::StoreOpenErrorCode, key::SortableU64Key},
};

use super::{
    error::RouteConfigurationError,
    routes::{RouteDefinition, RouteRequest, RouteResponse},
};

pub const VIEWS_PATH: &str = "/api/v1/views";
pub const DEFAULT_VIEW_QUERY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewLocalErrorCode {
    InvalidViewId,
    ViewNotFound,
    InvalidBinding,
    InvalidPagination,
    InvalidCursor,
    RequestCancelled,
    RequestTimedOut,
}

impl ViewLocalErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidViewId => "diagnostic_view.invalid_view_id",
            Self::ViewNotFound => "diagnostic_view.not_found",
            Self::InvalidBinding => "diagnostic_view.invalid_binding",
            Self::InvalidPagination => "diagnostic_view.invalid_pagination",
            Self::InvalidCursor => "diagnostic_view.invalid_cursor",
            Self::RequestCancelled => "diagnostic_view.request_cancelled",
            Self::RequestTimedOut => "diagnostic_view.request_timed_out",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewEndpointFailureCode {
    Local(ViewLocalErrorCode),
    Reader(ReaderErrorCode),
    ArchiveViews(ArchiveViewLoadErrorCode),
    Query(ViewQueryErrorCode),
    CapturedTimeOverflow,
    ProtocolInvariant,
    ResponseEncoding,
}

impl ViewEndpointFailureCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local(code) => code.as_str(),
            Self::Reader(code) => code.as_str(),
            Self::ArchiveViews(code) => code.as_str(),
            Self::Query(code) => code.as_str(),
            Self::CapturedTimeOverflow => "diagnostic_view.captured_time_overflow",
            Self::ProtocolInvariant => "diagnostic_view.protocol_invariant",
            Self::ResponseEncoding => "diagnostic_view.response_encoding",
        }
    }
}

#[derive(Debug)]
pub enum ViewEndpointError {
    Local(ViewLocalErrorCode),
    Reader(ReaderFailure),
    ArchiveViews(ArchiveViewLoadError),
    Query(ViewQueryError),
    CapturedTimeOverflow {
        profile: ReaderProfile,
    },
    ProtocolInvariant {
        profile: ReaderProfile,
    },
    ResponseEncoding {
        profile: ReaderProfile,
        source: serde_json::Error,
    },
}

impl ViewEndpointError {
    pub const fn class(&self) -> ViewQueryErrorClass {
        match self {
            Self::Local(_) => ViewQueryErrorClass::LocalQuery,
            Self::Reader(error) => class_for_reader(error.class()),
            Self::ArchiveViews(error) => class_for_reader(error.class()),
            Self::Query(error) => error.class(),
            Self::CapturedTimeOverflow { profile }
            | Self::ProtocolInvariant { profile }
            | Self::ResponseEncoding { profile, .. } => class_for_profile(*profile),
        }
    }

    pub const fn profile(&self) -> Option<ReaderProfile> {
        match self {
            Self::Local(_) => None,
            Self::Reader(error) => Some(error.profile()),
            Self::ArchiveViews(error) => Some(error.profile()),
            Self::Query(error) => error.profile(),
            Self::CapturedTimeOverflow { profile }
            | Self::ProtocolInvariant { profile }
            | Self::ResponseEncoding { profile, .. } => Some(*profile),
        }
    }

    pub const fn code(&self) -> ViewEndpointFailureCode {
        match self {
            Self::Local(code) => ViewEndpointFailureCode::Local(*code),
            Self::Reader(error) => ViewEndpointFailureCode::Reader(error.code()),
            Self::ArchiveViews(error) => ViewEndpointFailureCode::ArchiveViews(error.code()),
            Self::Query(error) => ViewEndpointFailureCode::Query(error.code()),
            Self::CapturedTimeOverflow { .. } => ViewEndpointFailureCode::CapturedTimeOverflow,
            Self::ProtocolInvariant { .. } => ViewEndpointFailureCode::ProtocolInvariant,
            Self::ResponseEncoding { .. } => ViewEndpointFailureCode::ResponseEncoding,
        }
    }

    pub const fn store_code(&self) -> Option<StoreOpenErrorCode> {
        match self {
            Self::Reader(error) => error.store_code(),
            Self::ArchiveViews(error) => match error.reader_failure() {
                Some(error) => error.store_code(),
                None => None,
            },
            Self::Local(_)
            | Self::Query(_)
            | Self::CapturedTimeOverflow { .. }
            | Self::ProtocolInvariant { .. }
            | Self::ResponseEncoding { .. } => None,
        }
    }

    pub const fn core_failure_signal(
        &self,
        run_id: CanonicalUuid,
    ) -> Option<ViewCoreFailureSignal> {
        if matches!(self.class(), ViewQueryErrorClass::CoreFatal) {
            Some(ViewCoreFailureSignal {
                run_id,
                class: ViewQueryErrorClass::CoreFatal,
                code: self.code(),
                store_code: self.store_code(),
            })
        } else {
            None
        }
    }
}

impl fmt::Display for ViewEndpointError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "diagnostic view endpoint failed [{}]: ",
            self.code().as_str()
        )?;
        match self {
            Self::Local(code) => formatter.write_str(local_error_message(*code)),
            Self::Reader(error) => fmt::Display::fmt(error, formatter),
            Self::ArchiveViews(error) => fmt::Display::fmt(error, formatter),
            Self::Query(error) => fmt::Display::fmt(error, formatter),
            Self::CapturedTimeOverflow { .. } => {
                formatter.write_str("captured elapsed range exceeds the u64 schema")
            }
            Self::ProtocolInvariant { .. } => {
                formatter.write_str("view response violated the frozen C05 protocol")
            }
            Self::ResponseEncoding { source, .. } => fmt::Display::fmt(source, formatter),
        }
    }
}

impl std::error::Error for ViewEndpointError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Reader(error) => Some(error),
            Self::ArchiveViews(error) => Some(error),
            Self::Query(error) => Some(error),
            Self::ResponseEncoding { source, .. } => Some(source),
            Self::Local(_) | Self::CapturedTimeOverflow { .. } | Self::ProtocolInvariant { .. } => {
                None
            }
        }
    }
}

impl From<ReaderFailure> for ViewEndpointError {
    fn from(error: ReaderFailure) -> Self {
        Self::Reader(error)
    }
}

impl From<ArchiveViewLoadError> for ViewEndpointError {
    fn from(error: ArchiveViewLoadError) -> Self {
        Self::ArchiveViews(error)
    }
}

impl From<ViewQueryError> for ViewEndpointError {
    fn from(error: ViewQueryError) -> Self {
        Self::Query(error)
    }
}

const fn class_for_reader(class: ReaderFailureClass) -> ViewQueryErrorClass {
    match class {
        ReaderFailureClass::CoreFatal => ViewQueryErrorClass::CoreFatal,
        ReaderFailureClass::ArchiveOperation => ViewQueryErrorClass::ArchiveOperation,
    }
}

const fn class_for_profile(profile: ReaderProfile) -> ViewQueryErrorClass {
    match profile {
        ReaderProfile::Active => ViewQueryErrorClass::CoreFatal,
        ReaderProfile::Archive => ViewQueryErrorClass::ArchiveOperation,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ViewCoreFailureSignal {
    run_id: CanonicalUuid,
    class: ViewQueryErrorClass,
    code: ViewEndpointFailureCode,
    store_code: Option<StoreOpenErrorCode>,
}

impl ViewCoreFailureSignal {
    pub const fn run_id(self) -> CanonicalUuid {
        self.run_id
    }

    pub const fn class(self) -> ViewQueryErrorClass {
        self.class
    }

    pub const fn code(self) -> ViewEndpointFailureCode {
        self.code
    }

    pub const fn store_code(self) -> Option<StoreOpenErrorCode> {
        self.store_code
    }
}

pub trait ViewCoreFailureReporter: Send + Sync + 'static {
    fn report(&self, failure: ViewCoreFailureSignal);
}

impl<F> ViewCoreFailureReporter for F
where
    F: Fn(ViewCoreFailureSignal) + Send + Sync + 'static,
{
    fn report(&self, failure: ViewCoreFailureSignal) {
        self(failure);
    }
}

#[derive(Clone, Debug)]
pub struct ViewRequestControl {
    cancelled: Arc<AtomicBool>,
    deadline: Option<Instant>,
}

impl ViewRequestControl {
    pub fn without_deadline() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline: None,
        }
    }

    pub fn with_timeout(timeout: Duration) -> Self {
        let now = Instant::now();
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline: Some(now.checked_add(timeout).unwrap_or(now)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn check(&self) -> Result<(), ViewEndpointError> {
        if self.is_cancelled() {
            return Err(ViewEndpointError::Local(
                ViewLocalErrorCode::RequestCancelled,
            ));
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(ViewEndpointError::Local(
                ViewLocalErrorCode::RequestTimedOut,
            ));
        }
        Ok(())
    }
}

impl Default for ViewRequestControl {
    fn default() -> Self {
        Self::without_deadline()
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum HttpViewResponse {
    Catalog(ViewCatalogResponse),
    Query(Box<ViewResponse>),
}

#[derive(Serialize)]
struct ViewCatalogResponse {
    api_schema_version: u8,
    run_id: CanonicalUuid,
    capabilities: OperationalCapabilities,
    views: Vec<ViewCatalogEntry>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ViewCatalogEntry {
    Compatible(ViewRecord),
    Incompatible(IncompatibleCatalogEntry),
}

#[derive(Serialize)]
struct IncompatibleCatalogEntry {
    status: &'static str,
    view_id: String,
    renderer: Renderer,
    incompatible: IncompatibleView,
}

#[derive(Clone)]
enum ViewTarget {
    Active {
        lease: Arc<ActiveArchiveLease>,
        core_failure_reporter: Arc<dyn ViewCoreFailureReporter>,
    },
    Archive {
        run_directory: Arc<PathBuf>,
    },
}

#[derive(Clone)]
pub struct ViewEndpoints {
    run_id: CanonicalUuid,
    target: ViewTarget,
    engine: ViewQueryEngine,
    request_timeout: Duration,
}

impl ViewEndpoints {
    pub fn active<R>(
        run_id: CanonicalUuid,
        lease: Arc<ActiveArchiveLease>,
        engine: ViewQueryEngine,
        core_failure_reporter: R,
    ) -> Self
    where
        R: ViewCoreFailureReporter,
    {
        Self {
            run_id,
            target: ViewTarget::Active {
                lease,
                core_failure_reporter: Arc::new(core_failure_reporter),
            },
            engine,
            request_timeout: DEFAULT_VIEW_QUERY_TIMEOUT,
        }
    }

    pub fn archive(
        run_id: CanonicalUuid,
        run_directory: impl Into<PathBuf>,
        engine: ViewQueryEngine,
    ) -> Self {
        Self {
            run_id,
            target: ViewTarget::Archive {
                run_directory: Arc::new(run_directory.into()),
            },
            engine,
            request_timeout: DEFAULT_VIEW_QUERY_TIMEOUT,
        }
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub const fn engine(&self) -> &ViewQueryEngine {
        &self.engine
    }

    pub const fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    pub const fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    pub fn route_definitions(&self) -> Result<Vec<RouteDefinition>, RouteConfigurationError> {
        let views = self.clone();
        Ok(vec![RouteDefinition::read_only(
            VIEWS_PATH,
            move |request| {
                let endpoint = views.clone();
                async move { Ok(endpoint.handle_query(request)) }
            },
        )?])
    }

    pub fn handle_query(&self, request: RouteRequest) -> RouteResponse {
        let result = validate_json_request(&request)
            .map_err(HttpFailure::Client)
            .and_then(|()| {
                let control = ViewRequestControl::with_timeout(self.request_timeout);
                if request.uri().query().is_none() {
                    self.execute_catalog(&control)
                        .map(HttpViewResponse::Catalog)
                        .map_err(HttpFailure::Operation)
                } else {
                    parse_http_query(&request)
                        .map_err(HttpFailure::Client)
                        .and_then(|request| {
                            self.execute_http(&request, &control)
                                .map(Box::new)
                                .map(HttpViewResponse::Query)
                                .map_err(HttpFailure::Operation)
                        })
                }
            });
        match result {
            Ok(response) => match serde_json::to_vec(&response) {
                Ok(bytes) => json_bytes(StatusCode::OK, bytes),
                Err(source) => {
                    let error = ViewEndpointError::ResponseEncoding {
                        profile: self.profile(),
                        source,
                    };
                    self.observe_failure(&error);
                    ClientError::from_operation(&error).response(self.run_id)
                }
            },
            Err(HttpFailure::Client(error)) => error.response(self.run_id),
            Err(HttpFailure::Operation(error)) => {
                self.observe_failure(&error);
                ClientError::from_operation(&error).response(self.run_id)
            }
        }
    }

    pub fn execute(
        &self,
        view_id: &str,
        request: &ViewQueryRequest,
        control: &ViewRequestControl,
    ) -> Result<ViewResponse, ViewEndpointError> {
        let result = self.execute_inner(view_id, request, control);
        if let Err(error) = &result {
            self.observe_failure(error);
        }
        result
    }

    fn execute_http(
        &self,
        request: &ParsedHttpQuery,
        control: &ViewRequestControl,
    ) -> Result<ViewResponse, ViewEndpointError> {
        control.check()?;
        if !valid_view_id(&request.view_id) {
            return Err(ViewEndpointError::Local(ViewLocalErrorCode::InvalidViewId));
        }
        self.with_capture(|source| {
            control.check()?;
            let catalog = load_stored_view_records(source)?;
            let stored = catalog
                .get(&request.view_id)
                .ok_or(ViewEndpointError::Local(ViewLocalErrorCode::ViewNotFound))?;
            let query = match stored.compatible_record() {
                Some(record) => request.build_for(record)?,
                None => {
                    request.validate_unavailable()?;
                    ViewQueryRequest::new()
                }
            };
            self.execute_stored(source, stored, &query, control)
        })
    }

    fn execute_catalog(
        &self,
        control: &ViewRequestControl,
    ) -> Result<ViewCatalogResponse, ViewEndpointError> {
        control.check()?;
        self.with_capture(|source| {
            control.check()?;
            let catalog = load_stored_view_records(source)?;
            let mut views = Vec::with_capacity(catalog.views().len());
            for stored in catalog.views() {
                let entry = match stored.availability() {
                    StoredViewAvailability::Compatible(record) => {
                        ViewCatalogEntry::Compatible(record.clone())
                    }
                    StoredViewAvailability::Unavailable(reason) => {
                        ViewCatalogEntry::Incompatible(IncompatibleCatalogEntry {
                            status: "incompatible",
                            view_id: stored.id().to_owned(),
                            renderer: stored.renderer(),
                            incompatible: stored_incompatibility(source, stored, *reason)?,
                        })
                    }
                };
                views.push(entry);
            }
            control.check()?;
            Ok(ViewCatalogResponse {
                api_schema_version: API_SCHEMA_VERSION,
                run_id: source.metadata().run_id(),
                capabilities: OperationalCapabilities::default(),
                views,
            })
        })
    }

    fn execute_inner(
        &self,
        view_id: &str,
        request: &ViewQueryRequest,
        control: &ViewRequestControl,
    ) -> Result<ViewResponse, ViewEndpointError> {
        control.check()?;
        if !valid_view_id(view_id) {
            return Err(ViewEndpointError::Local(ViewLocalErrorCode::InvalidViewId));
        }
        self.with_capture(|source| {
            control.check()?;
            let catalog = load_stored_view_records(source)?;
            let stored = catalog
                .get(view_id)
                .ok_or(ViewEndpointError::Local(ViewLocalErrorCode::ViewNotFound))?;
            self.execute_stored(source, stored, request, control)
        })
    }

    fn execute_stored(
        &self,
        source: &CapturedEventSource<'_>,
        stored: &StoredViewRecord,
        request: &ViewQueryRequest,
        control: &ViewRequestControl,
    ) -> Result<ViewResponse, ViewEndpointError> {
        control.check()?;
        let response = match stored.availability() {
            StoredViewAvailability::Compatible(record) => {
                self.engine.query(source, record, request)?
            }
            StoredViewAvailability::Unavailable(reason) => {
                incompatible_response(source, stored, *reason)?
            }
        };
        control.check()?;
        Ok(response)
    }

    fn with_capture<T>(
        &self,
        operation: impl FnOnce(&CapturedEventSource<'_>) -> Result<T, ViewEndpointError>,
    ) -> Result<T, ViewEndpointError> {
        match &self.target {
            ViewTarget::Active { lease, .. } => {
                let mut reader = DiagnosticReader::open_active(self.run_id, lease.guard())?;
                let source = reader.capture()?;
                operation(&source)
            }
            ViewTarget::Archive { run_directory } => {
                let mut reader =
                    DiagnosticReader::open_archive(run_directory.as_ref(), self.run_id)?;
                let source = reader.capture()?;
                operation(&source)
            }
        }
    }

    const fn profile(&self) -> ReaderProfile {
        match self.target {
            ViewTarget::Active { .. } => ReaderProfile::Active,
            ViewTarget::Archive { .. } => ReaderProfile::Archive,
        }
    }

    fn observe_failure(&self, error: &ViewEndpointError) {
        if let ViewTarget::Active {
            core_failure_reporter,
            ..
        } = &self.target
            && let Some(failure) = error.core_failure_signal(self.run_id)
        {
            core_failure_reporter.report(failure);
        }
    }
}

impl fmt::Debug for ViewEndpoints {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ViewEndpoints")
            .field("run_id", &self.run_id)
            .field("profile", &self.profile())
            .field("request_timeout", &self.request_timeout)
            .finish_non_exhaustive()
    }
}

fn incompatible_response(
    source: &CapturedEventSource<'_>,
    stored: &StoredViewRecord,
    reason: IncompatibilityReason,
) -> Result<ViewResponse, ViewEndpointError> {
    let profile = source.profile();
    let captured_elapsed_end_ns = captured_elapsed_end_ns(source)?;
    let binding = QueryBinding::new(
        source.captured_watermark(),
        SchemaU64::new(captured_elapsed_end_ns),
        TimeRangeMode::Run,
        SchemaU64::new(0),
        SchemaU64::new(captured_elapsed_end_ns),
        ScopeMode::Run,
        None,
    )
    .map_err(|_| ViewEndpointError::ProtocolInvariant { profile })?;
    let coverage = Coverage::new(
        CoverageStatus::Unavailable,
        SchemaU64::new(0),
        SchemaU64::new(0),
        SchemaU64::new(0),
        ExcludedCounts::new(
            SchemaU64::new(0),
            SchemaU64::new(0),
            SchemaU64::new(0),
            SchemaU64::new(0),
            SchemaU64::new(0),
        ),
        SchemaU64::new(0),
    )
    .map_err(|_| ViewEndpointError::ProtocolInvariant { profile })?;
    let incompatible = stored_incompatibility(source, stored, reason)?;
    let pagination = matches!(stored.renderer(), Renderer::Timeline | Renderer::Table)
        .then(|| Pagination::new(MAX_PAGE_ROWS, None))
        .transpose()
        .map_err(|_| ViewEndpointError::ProtocolInvariant { profile })?;
    let metadata = ResultMetadata::new(
        source.metadata().run_id(),
        stored.id().to_owned(),
        binding,
        coverage,
        pagination,
        false,
        Some(incompatible),
    )
    .map_err(|_| ViewEndpointError::ProtocolInvariant { profile })?;
    let response = match stored.renderer() {
        Renderer::Timeline => ViewResponse::new_timeline(metadata, Vec::new()),
        Renderer::Metric => ViewResponse::new_metric(metadata, Vec::new()),
        Renderer::Table => {
            ViewResponse::new_table(metadata, vec![TableColumn::Sequence], Vec::new())
        }
        Renderer::TimeSeries => ViewResponse::new_time_series(
            metadata,
            expected_bucket_width_ns(0, captured_elapsed_end_ns)
                .map_err(|_| ViewEndpointError::ProtocolInvariant { profile })?,
            Vec::new(),
        ),
    }
    .map_err(|_| ViewEndpointError::ProtocolInvariant { profile })?;
    response
        .validate()
        .map_err(|_| ViewEndpointError::ProtocolInvariant { profile })?;
    Ok(response)
}

fn stored_incompatibility(
    source: &CapturedEventSource<'_>,
    stored: &StoredViewRecord,
    reason: IncompatibilityReason,
) -> Result<IncompatibleView, ViewEndpointError> {
    let incompatible = match reason {
        IncompatibilityReason::NewerViewSchema => {
            IncompatibleView::newer(stored.record_view_schema_version())
        }
        IncompatibilityReason::CorruptRecord => {
            IncompatibleView::corrupt(Some(stored.record_view_schema_version()))
        }
    }
    .map_err(|_| ViewEndpointError::ProtocolInvariant {
        profile: source.profile(),
    })?;
    Ok(incompatible)
}

fn captured_elapsed_end_ns(source: &CapturedEventSource<'_>) -> Result<u64, ViewEndpointError> {
    let mut maximum = None;
    for event in source.events() {
        let elapsed_ns = event?.event().header().elapsed_ns().get();
        maximum = Some(maximum.map_or(elapsed_ns, |current: u64| current.max(elapsed_ns)));
    }
    maximum.map_or(Ok(0), |elapsed_ns| {
        elapsed_ns
            .checked_add(1)
            .ok_or(ViewEndpointError::CapturedTimeOverflow {
                profile: source.profile(),
            })
    })
}

#[derive(Debug)]
enum HttpFailure {
    Client(ClientError),
    Operation(ViewEndpointError),
}

#[derive(Clone, Copy, Debug)]
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

    fn from_operation(error: &ViewEndpointError) -> Self {
        match error {
            ViewEndpointError::Local(ViewLocalErrorCode::InvalidViewId) => Self::new(
                StatusCode::BAD_REQUEST,
                "invalid_view_id",
                "view_id must be a canonical compiled view identifier",
            ),
            ViewEndpointError::Local(ViewLocalErrorCode::ViewNotFound) => Self::new(
                StatusCode::NOT_FOUND,
                "view_not_found",
                "the compiled diagnostic view does not exist",
            ),
            ViewEndpointError::Local(ViewLocalErrorCode::InvalidBinding) => Self::new(
                StatusCode::BAD_REQUEST,
                "invalid_view_binding",
                "view time or scope binding is invalid",
            ),
            ViewEndpointError::Query(error)
                if error.code() == ViewQueryErrorCode::InvalidBinding =>
            {
                Self::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_view_binding",
                    "view time or scope binding is invalid",
                )
            }
            ViewEndpointError::Local(ViewLocalErrorCode::InvalidPagination) => Self::new(
                StatusCode::BAD_REQUEST,
                "invalid_view_pagination",
                "view pagination parameters are invalid",
            ),
            ViewEndpointError::Query(error)
                if error.code() == ViewQueryErrorCode::InvalidPagination =>
            {
                Self::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_view_pagination",
                    "view pagination parameters are invalid",
                )
            }
            ViewEndpointError::Local(ViewLocalErrorCode::InvalidCursor) => Self::new(
                StatusCode::BAD_REQUEST,
                "invalid_view_cursor",
                "view cursor is invalid for this captured query",
            ),
            ViewEndpointError::Query(error)
                if error.code() == ViewQueryErrorCode::InvalidCursor =>
            {
                Self::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_view_cursor",
                    "view cursor is invalid for this captured query",
                )
            }
            ViewEndpointError::Query(error) if error.code() == ViewQueryErrorCode::StaleBinding => {
                Self::new(
                    StatusCode::CONFLICT,
                    "stale_view_binding",
                    "the captured view binding is stale",
                )
            }
            ViewEndpointError::Local(ViewLocalErrorCode::RequestCancelled) => Self::new(
                status_code(499),
                "view_query_cancelled",
                "the view query was cancelled",
            ),
            ViewEndpointError::Local(ViewLocalErrorCode::RequestTimedOut) => Self::new(
                StatusCode::REQUEST_TIMEOUT,
                "view_query_timeout",
                "the view query exceeded its request deadline",
            ),
            _ if error.class() == ViewQueryErrorClass::CoreFatal => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "view_query_failed",
                "the diagnostic view query could not be completed",
            ),
            _ if error.store_code() == Some(StoreOpenErrorCode::RunIdentityMismatch) => Self::new(
                StatusCode::CONFLICT,
                "run_identity_mismatch",
                "the view query source does not match this Run",
            ),
            _ if matches!(
                error.store_code(),
                Some(StoreOpenErrorCode::NewerSchema | StoreOpenErrorCode::SchemaMismatch)
            ) =>
            {
                Self::new(
                    StatusCode::CONFLICT,
                    "incompatible_schema",
                    "the view query source uses an incompatible schema",
                )
            }
            _ if error.class() == ViewQueryErrorClass::LocalQuery => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "view_query_rejected",
                "the diagnostic view query was rejected",
            ),
            _ => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "view_query_failed",
                "the diagnostic view query could not be completed",
            ),
        }
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
        .expect("closed view HTTP errors are JSON serializable");
        json_bytes(self.status, bytes)
    }
}

const fn status_code(value: u16) -> StatusCode {
    match StatusCode::from_u16(value) {
        Ok(status) => status,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
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

#[derive(Clone, Debug)]
struct ParsedHttpQuery {
    view_id: String,
    viewport: Option<Viewport>,
    selected_scope: Option<DiagnosticScope>,
    cursor: Option<OpaqueCursor>,
    page_size: Option<u16>,
    expected_watermark: Option<SchemaU64>,
    expected_elapsed_end_ns: Option<SchemaU64>,
}

impl ParsedHttpQuery {
    fn validate_unavailable(&self) -> Result<(), ViewEndpointError> {
        if self.cursor.is_some() {
            return Err(ViewEndpointError::Local(ViewLocalErrorCode::InvalidCursor));
        }
        if self.page_size.is_some() {
            return Err(ViewEndpointError::Local(
                ViewLocalErrorCode::InvalidPagination,
            ));
        }
        if self.viewport.is_some()
            || self.selected_scope.is_some()
            || self.expected_watermark.is_some()
            || self.expected_elapsed_end_ns.is_some()
        {
            return Err(ViewEndpointError::Local(ViewLocalErrorCode::InvalidBinding));
        }
        Ok(())
    }

    fn build_for(&self, record: &ViewRecord) -> Result<ViewQueryRequest, ViewEndpointError> {
        if matches!(record.renderer(), Renderer::Metric | Renderer::TimeSeries) {
            if self.cursor.is_some() {
                return Err(ViewEndpointError::Local(ViewLocalErrorCode::InvalidCursor));
            }
            if self.page_size.is_some() {
                return Err(ViewEndpointError::Local(
                    ViewLocalErrorCode::InvalidPagination,
                ));
            }
        }
        let mut request = ViewQueryRequest::new();
        if let Some(viewport) = self.viewport {
            request = request.with_viewport(viewport);
        }
        if let Some(scope) = &self.selected_scope {
            request = request.with_selected_scope(scope.clone());
        }
        if let Some(cursor) = &self.cursor {
            request = request.with_cursor(cursor.clone());
        }
        if let Some(page_size) = self.page_size {
            request = request.with_page_size(page_size);
        }
        match (self.expected_watermark, self.expected_elapsed_end_ns) {
            (None, None) => {}
            (Some(watermark), Some(elapsed_end)) => {
                let (range_start, range_end) = match (record.time_range(), self.viewport) {
                    (TimeRangeMode::Run, None) => (SchemaU64::new(0), elapsed_end),
                    (TimeRangeMode::Viewport, Some(viewport)) => {
                        (viewport.start_ns(), viewport.end_ns())
                    }
                    _ => {
                        return Err(ViewEndpointError::Local(ViewLocalErrorCode::InvalidBinding));
                    }
                };
                let selected_scope = match (record.scope(), &self.selected_scope) {
                    (ScopeMode::Run | ScopeMode::Selection, None) => None,
                    (ScopeMode::Selection, Some(scope)) => Some(scope.clone()),
                    _ => {
                        return Err(ViewEndpointError::Local(ViewLocalErrorCode::InvalidBinding));
                    }
                };
                let binding = QueryBinding::new(
                    watermark,
                    elapsed_end,
                    record.time_range(),
                    range_start,
                    range_end,
                    record.scope(),
                    selected_scope,
                )
                .map_err(|_| ViewEndpointError::Local(ViewLocalErrorCode::InvalidBinding))?;
                request = request.with_expected_binding(binding);
            }
            _ => {
                return Err(ViewEndpointError::Local(ViewLocalErrorCode::InvalidBinding));
            }
        }
        Ok(request)
    }
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
            "diagnostic views are returned as application/json",
        ))
    } else {
        Ok(())
    }
}

fn parse_http_query(request: &RouteRequest) -> Result<ParsedHttpQuery, ClientError> {
    let query = request
        .uri()
        .query()
        .filter(|query| !query.is_empty())
        .ok_or_else(invalid_query)?;
    let parameters = parse_query_parameters(query)?;
    let view_id = required(&parameters, "view_id")?.to_owned();
    let viewport_start = optional_u64(&parameters, "viewport_start_ns")?;
    let viewport_end = optional_u64(&parameters, "viewport_end_ns")?;
    let viewport = match (viewport_start, viewport_end) {
        (None, None) => None,
        (Some(start), Some(end)) if start.get() <= end.get() => Some(Viewport::new(start, end)),
        _ => return Err(invalid_binding()),
    };
    let scene_id = optional_local_id(&parameters, "scene_id")?;
    let actor_id = optional_local_id(&parameters, "actor_id")?;
    let cue_id = optional_local_id(&parameters, "cue_id")?;
    let act_id = optional_local_id(&parameters, "act_id")?;
    let session_generation = optional_u64(&parameters, "session_generation")?;
    let selected_scope = if scene_id.is_some()
        || actor_id.is_some()
        || cue_id.is_some()
        || act_id.is_some()
        || session_generation.is_some()
    {
        if session_generation.is_some_and(|value| value.get() == 0)
            || (scene_id.is_none() && actor_id.is_none() && cue_id.is_none() && act_id.is_none())
        {
            return Err(invalid_binding());
        }
        Some(DiagnosticScope::new(
            scene_id,
            actor_id,
            cue_id,
            None,
            act_id,
            None,
            session_generation,
        ))
    } else {
        None
    };
    let cursor = parameters
        .get("cursor")
        .map(|value| OpaqueCursor::parse(value).map_err(|_| invalid_cursor()))
        .transpose()?;
    let page_size = parameters
        .get("page_size")
        .map(|value| parse_page_size(value))
        .transpose()?;
    Ok(ParsedHttpQuery {
        view_id,
        viewport,
        selected_scope,
        cursor,
        page_size,
        expected_watermark: optional_u64(&parameters, "captured_watermark")?,
        expected_elapsed_end_ns: optional_u64(&parameters, "captured_elapsed_end_ns")?,
    })
}

fn parse_query_parameters(query: &str) -> Result<BTreeMap<String, String>, ClientError> {
    const ALLOWED: [&str; 12] = [
        "view_id",
        "viewport_start_ns",
        "viewport_end_ns",
        "scene_id",
        "actor_id",
        "cue_id",
        "act_id",
        "session_generation",
        "cursor",
        "page_size",
        "captured_watermark",
        "captured_elapsed_end_ns",
    ];
    let mut parameters = BTreeMap::new();
    for parameter in query.split('&') {
        let (raw_name, raw_value) = parameter.split_once('=').ok_or_else(invalid_query)?;
        let name = decode_query_component(raw_name)?;
        let value = decode_query_component(raw_value)?;
        if name.is_empty()
            || value.is_empty()
            || !ALLOWED.contains(&name.as_str())
            || parameters.insert(name, value).is_some()
        {
            return Err(invalid_query());
        }
    }
    Ok(parameters)
}

fn decode_query_component(value: &str) -> Result<String, ClientError> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let high = decode_hex(bytes[index + 1]).ok_or_else(invalid_query)?;
                let low = decode_hex(bytes[index + 2]).ok_or_else(invalid_query)?;
                decoded.push((high << 4) | low);
                index += 3;
            }
            b'%' => return Err(invalid_query()),
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            byte if byte.is_ascii() => {
                decoded.push(byte);
                index += 1;
            }
            _ => return Err(invalid_query()),
        }
    }
    String::from_utf8(decoded).map_err(|_| invalid_query())
}

const fn decode_hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn required<'a>(
    parameters: &'a BTreeMap<String, String>,
    name: &str,
) -> Result<&'a str, ClientError> {
    parameters
        .get(name)
        .map(String::as_str)
        .ok_or_else(invalid_query)
}

fn optional_u64(
    parameters: &BTreeMap<String, String>,
    name: &str,
) -> Result<Option<SchemaU64>, ClientError> {
    parameters
        .get(name)
        .map(|value| parse_u64(value))
        .transpose()
}

fn parse_u64(value: &str) -> Result<SchemaU64, ClientError> {
    SortableU64Key::parse_canonical_decimal(value)
        .map(|value| SchemaU64::new(value.get()))
        .map_err(|_| invalid_binding())
}

fn parse_page_size(value: &str) -> Result<u16, ClientError> {
    let value = SortableU64Key::parse_canonical_decimal(value)
        .map_err(|_| invalid_pagination())?
        .get();
    let page_size = u16::try_from(value).map_err(|_| invalid_pagination())?;
    if page_size == 0 || page_size > MAX_PAGE_ROWS {
        return Err(invalid_pagination());
    }
    Ok(page_size)
}

fn optional_local_id(
    parameters: &BTreeMap<String, String>,
    name: &str,
) -> Result<Option<RunLocalId>, ClientError> {
    parameters
        .get(name)
        .map(|value| RunLocalId::parse(value).map_err(|_| invalid_binding()))
        .transpose()
}

fn valid_view_id(value: &str) -> bool {
    let mut bytes = value.bytes();
    !value.is_empty()
        && value.len() <= troupe_diagnostics_core::view_protocol::MAX_VIEW_ID_BYTES
        && bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

const fn local_error_message(code: ViewLocalErrorCode) -> &'static str {
    match code {
        ViewLocalErrorCode::InvalidViewId => "compiled view ID is invalid",
        ViewLocalErrorCode::ViewNotFound => "compiled view ID was not found",
        ViewLocalErrorCode::InvalidBinding => "view binding is invalid",
        ViewLocalErrorCode::InvalidPagination => "view pagination is invalid",
        ViewLocalErrorCode::InvalidCursor => "view cursor is invalid",
        ViewLocalErrorCode::RequestCancelled => "view request was cancelled",
        ViewLocalErrorCode::RequestTimedOut => "view request timed out",
    }
}

const fn invalid_query() -> ClientError {
    ClientError::new(
        StatusCode::BAD_REQUEST,
        "invalid_view_query",
        "view query parameters are invalid",
    )
}

const fn invalid_binding() -> ClientError {
    ClientError::new(
        StatusCode::BAD_REQUEST,
        "invalid_view_binding",
        "view time or scope binding is invalid",
    )
}

const fn invalid_pagination() -> ClientError {
    ClientError::new(
        StatusCode::BAD_REQUEST,
        "invalid_view_pagination",
        "view pagination parameters are invalid",
    )
}

const fn invalid_cursor() -> ClientError {
    ClientError::new(
        StatusCode::BAD_REQUEST,
        "invalid_view_cursor",
        "view cursor is invalid for this captured query",
    )
}

fn json_bytes(status: StatusCode, bytes: Vec<u8>) -> RouteResponse {
    RouteResponse::bytes(status, bytes).with_header(
        CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    )
}
