use std::{
    error::Error,
    fmt,
    future::Future,
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

use bytes::Bytes;
use futures::executor::block_on;
use hyper::{
    HeaderMap, StatusCode,
    body::{Body, Frame, SizeHint},
    header::{ACCEPT, CONTENT_TYPE, HeaderValue},
};
use serde::Serialize;
use tokio::{io::AsyncWrite, sync::oneshot};
use troupe_diagnostics_core::{id::CanonicalUuid, scalar::SchemaU64};

use crate::{
    archive::lease::ActiveArchiveLease,
    query::reader::{CapturedEventSource, DiagnosticReader, ReaderFailure},
    store::key::SortableU64Key,
};

use super::{
    error::RouteConfigurationError,
    routes::{RouteDefinition, RouteRequest, RouteResponse},
};

pub const DUMP_PATH: &str = "/api/v1/dump";
pub const PERFETTO_TRACE_MIME: &str = "application/x-protobuf";
pub const DUMP_API_SCHEMA_VERSION: u8 = 1;
pub const DUMP_RUN_ID_HEADER: &str = "x-troupe-run-id";
pub const DUMP_CAPTURED_WATERMARK_HEADER: &str = "x-troupe-captured-watermark";
pub const DUMP_EXPORTED_THROUGH_HEADER: &str = "x-troupe-exported-through";
pub const DUMP_API_SCHEMA_VERSION_HEADER: &str = "x-troupe-api-schema-version";
pub const DUMP_EVENT_SCHEMA_VERSION_HEADER: &str = "x-troupe-event-schema-version";
pub const DUMP_EXPORTER_SCHEMA_VERSION_HEADER: &str = "x-troupe-perfetto-exporter-schema-version";
pub const DUMP_TROUPE_VERSION_HEADER: &str = "x-troupe-version";
pub const DUMP_PRODUCTION_OUTCOME_HEADER: &str = "x-troupe-production-outcome";
pub const DUMP_CLEAN_SHUTDOWN_HEADER: &str = "x-troupe-clean-shutdown";
pub const DUMP_CONTENT_WARNING_HEADER: &str = "x-troupe-content-warning";

const MAX_RESPONSE_CHUNK_BYTES: usize = 64 * 1024;
const UNAVAILABLE: &str = "unavailable";

pub type DumpProducerFuture<'operation> =
    Pin<Box<dyn Future<Output = Result<(), DumpProducerError>> + 'operation>>;

/// Dependency-inversion boundary implemented by the concrete Perfetto exporter.
///
/// The first call to `writer.poll_write` is the readiness signal. A producer must
/// perform its complete structural preflight before that call. The writer holds
/// that first poll pending until the HTTP response body is itself polled, so no
/// trace byte can precede successful response commitment.
pub trait CapturedPrefixDumpProducer: Send + Sync + 'static {
    fn metadata(&self) -> &DumpProducerMetadata;

    fn dump<'operation>(
        &'operation self,
        source: &'operation CapturedEventSource<'_>,
        writer: &'operation mut (dyn AsyncWrite + Unpin),
        through: Option<SchemaU64>,
    ) -> DumpProducerFuture<'operation>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DumpProducerMetadata {
    exporter_schema_version: u8,
    troupe_version: String,
    content_warning: String,
}

impl DumpProducerMetadata {
    pub fn new(
        exporter_schema_version: u8,
        troupe_version: impl Into<String>,
        content_warning: impl Into<String>,
    ) -> Result<Self, DumpProducerMetadataError> {
        if exporter_schema_version == 0 {
            return Err(DumpProducerMetadataError::new(
                "exporter schema version must be nonzero",
            ));
        }
        let troupe_version = troupe_version.into();
        let content_warning = content_warning.into();
        validate_metadata_header(&troupe_version, "Troupe version")?;
        validate_metadata_header(&content_warning, "content warning")?;
        Ok(Self {
            exporter_schema_version,
            troupe_version,
            content_warning,
        })
    }

    pub const fn exporter_schema_version(&self) -> u8 {
        self.exporter_schema_version
    }

    pub fn troupe_version(&self) -> &str {
        &self.troupe_version
    }

    pub fn content_warning(&self) -> &str {
        &self.content_warning
    }
}

fn validate_metadata_header(
    value: &str,
    field: &'static str,
) -> Result<(), DumpProducerMetadataError> {
    if value.is_empty() || HeaderValue::from_str(value).is_err() {
        Err(DumpProducerMetadataError::new(match field {
            "Troupe version" => "Troupe version is not a valid HTTP header value",
            _ => "content warning is not a valid HTTP header value",
        }))
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DumpProducerMetadataError(&'static str);

impl DumpProducerMetadataError {
    const fn new(message: &'static str) -> Self {
        Self(message)
    }
}

impl fmt::Display for DumpProducerMetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for DumpProducerMetadataError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DumpProducerError {
    code: String,
    message: String,
}

impl DumpProducerError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        let code = code.into();
        Self {
            code: if code.is_empty() {
                "dump_producer_failed".to_owned()
            } else {
                code
            },
            message: message.into(),
        }
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for DumpProducerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for DumpProducerError {}

#[derive(Clone)]
enum DumpTarget {
    Active(Arc<ActiveArchiveLease>),
    Archive(Arc<PathBuf>),
}

#[derive(Clone)]
pub struct DumpEndpoints {
    run_id: CanonicalUuid,
    target: DumpTarget,
    producer: Arc<dyn CapturedPrefixDumpProducer>,
}

impl DumpEndpoints {
    pub fn active<P>(run_id: CanonicalUuid, lease: Arc<ActiveArchiveLease>, producer: P) -> Self
    where
        P: CapturedPrefixDumpProducer,
    {
        Self {
            run_id,
            target: DumpTarget::Active(lease),
            producer: Arc::new(producer),
        }
    }

    pub fn active_shared(
        run_id: CanonicalUuid,
        lease: Arc<ActiveArchiveLease>,
        producer: Arc<dyn CapturedPrefixDumpProducer>,
    ) -> Self {
        Self {
            run_id,
            target: DumpTarget::Active(lease),
            producer,
        }
    }

    pub fn archive<P>(run_id: CanonicalUuid, run_directory: impl Into<PathBuf>, producer: P) -> Self
    where
        P: CapturedPrefixDumpProducer,
    {
        Self {
            run_id,
            target: DumpTarget::Archive(Arc::new(run_directory.into())),
            producer: Arc::new(producer),
        }
    }

    pub fn archive_shared(
        run_id: CanonicalUuid,
        run_directory: impl Into<PathBuf>,
        producer: Arc<dyn CapturedPrefixDumpProducer>,
    ) -> Self {
        Self {
            run_id,
            target: DumpTarget::Archive(Arc::new(run_directory.into())),
            producer,
        }
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub fn route_definitions(&self) -> Result<Vec<RouteDefinition>, RouteConfigurationError> {
        let endpoint = self.clone();
        Ok(vec![RouteDefinition::get(DUMP_PATH, move |request| {
            let endpoint = endpoint.clone();
            async move { Ok(endpoint.handle(request).await) }
        })?])
    }

    async fn handle(&self, request: RouteRequest) -> RouteResponse {
        let requested_through = match validate_request(&request) {
            Ok(through) => through,
            Err(error) => return error.response(self.run_id),
        };
        match self.start_dump(requested_through).await {
            Ok((prepared, body)) => prepared.response(body),
            Err(error) => error.response(self.run_id),
        }
    }

    async fn start_dump(
        &self,
        requested_through: Option<SchemaU64>,
    ) -> Result<(PreparedResponse, DumpBody), ClientError> {
        let bridge = Arc::new(Bridge::new());
        let gate = Arc::new(StartupGate::new());
        let (startup_sender, startup_receiver) = oneshot::channel();
        gate.install(startup_sender);

        let target = self.target.clone();
        let producer = Arc::clone(&self.producer);
        let run_id = self.run_id;
        let worker_bridge = Arc::clone(&bridge);
        let worker_gate = Arc::clone(&gate);
        drop(tokio::task::spawn_blocking(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                run_worker(
                    run_id,
                    target,
                    producer,
                    requested_through,
                    Arc::clone(&worker_gate),
                    Arc::clone(&worker_bridge),
                );
            }));
            if result.is_err() {
                let failure = ClientError::dump_failed();
                if !worker_gate.fail(failure) {
                    worker_bridge.finish(Err(DumpBodyError::worker_failed()));
                }
            }
        }));

        let mut cancellation = PendingRequestCancellation::new(Arc::clone(&bridge));
        let prepared = startup_receiver
            .await
            .map_err(|_| ClientError::dump_failed())??;
        let body = DumpBody::new(bridge);
        cancellation.disarm();
        Ok((prepared, body))
    }
}

impl fmt::Debug for DumpEndpoints {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let profile = match &self.target {
            DumpTarget::Active(_) => "active",
            DumpTarget::Archive(_) => "archive",
        };
        formatter
            .debug_struct("DumpEndpoints")
            .field("run_id", &self.run_id)
            .field("profile", &profile)
            .finish_non_exhaustive()
    }
}

fn run_worker(
    run_id: CanonicalUuid,
    target: DumpTarget,
    producer: Arc<dyn CapturedPrefixDumpProducer>,
    requested_through: Option<SchemaU64>,
    gate: Arc<StartupGate>,
    bridge: Arc<Bridge>,
) {
    match target {
        DumpTarget::Active(lease) => {
            let reader = DiagnosticReader::open_active(run_id, lease.guard());
            run_opened_reader(reader, producer, requested_through, gate, bridge);
        }
        DumpTarget::Archive(run_directory) => {
            let reader = DiagnosticReader::open_archive(run_directory.as_ref(), run_id);
            run_opened_reader(reader, producer, requested_through, gate, bridge);
        }
    }
}

fn run_opened_reader(
    opened: Result<DiagnosticReader<'_>, ReaderFailure>,
    producer: Arc<dyn CapturedPrefixDumpProducer>,
    requested_through: Option<SchemaU64>,
    gate: Arc<StartupGate>,
    bridge: Arc<Bridge>,
) {
    let mut reader = match opened {
        Ok(reader) => reader,
        Err(error) => {
            fail_reader_startup(&gate, error);
            return;
        }
    };
    let source = match reader.capture() {
        Ok(source) => source,
        Err(error) => {
            fail_reader_startup(&gate, error);
            return;
        }
    };
    let captured_watermark = source.captured_watermark();
    let exported_through = requested_through.unwrap_or(captured_watermark);
    if exported_through.get() > captured_watermark.get() {
        let _ = gate.fail(ClientError::future_through());
        return;
    }
    let prepared = match PreparedResponse::new(&source, exported_through, producer.metadata()) {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = gate.fail(error);
            return;
        }
    };
    let mut writer = GatedWriter::new(Arc::clone(&bridge), Arc::clone(&gate), prepared);
    let result = block_on(producer.dump(&source, &mut writer, requested_through));
    if gate.is_pending() {
        let _ = gate.fail(ClientError::dump_failed());
        return;
    }
    match result {
        Ok(()) => bridge.finish(Ok(())),
        Err(_) => bridge.finish(Err(DumpBodyError::producer_failed())),
    }
}

fn fail_reader_startup(gate: &StartupGate, _error: ReaderFailure) {
    let _ = gate.fail(ClientError::dump_failed());
}

fn validate_request(request: &RouteRequest) -> Result<Option<SchemaU64>, ClientError> {
    validate_accept(request)?;
    parse_through(request.uri().query())
}

fn validate_accept(request: &RouteRequest) -> Result<(), ClientError> {
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
            if !quality_is_zero
                && matches!(media_type, PERFETTO_TRACE_MIME | "application/*" | "*/*")
            {
                return Ok(());
            }
        }
    }
    if saw_accept {
        Err(ClientError::unsupported_format())
    } else {
        Ok(())
    }
}

fn parse_through(query: Option<&str>) -> Result<Option<SchemaU64>, ClientError> {
    let Some(query) = query.filter(|query| !query.is_empty()) else {
        return Ok(None);
    };
    let Some((name, value)) = query.split_once('=') else {
        return Err(ClientError::invalid_query());
    };
    if name != "through"
        || value.is_empty()
        || query.contains('&')
        || name.bytes().any(|byte| matches!(byte, b'%' | b'+'))
        || value.bytes().any(|byte| matches!(byte, b'%' | b'+'))
    {
        return Err(ClientError::invalid_query());
    }
    SortableU64Key::parse_canonical_decimal(value)
        .map(|value| Some(SchemaU64::new(value.get())))
        .map_err(|_| ClientError::invalid_through())
}

struct PreparedResponse {
    headers: HeaderMap,
}

impl PreparedResponse {
    fn new(
        source: &CapturedEventSource<'_>,
        exported_through: SchemaU64,
        producer: &DumpProducerMetadata,
    ) -> Result<Self, ClientError> {
        let metadata = source.metadata();
        let outcome = metadata.production_outcome().unwrap_or(UNAVAILABLE);
        let clean_shutdown = if metadata.ended_at().is_some() {
            if metadata.clean_shutdown() {
                "true"
            } else {
                "false"
            }
        } else {
            UNAVAILABLE
        };
        let mut headers = HeaderMap::new();
        insert_static(&mut headers, CONTENT_TYPE.as_str(), PERFETTO_TRACE_MIME)?;
        insert_header(
            &mut headers,
            DUMP_RUN_ID_HEADER,
            &metadata.run_id().to_string(),
        )?;
        insert_header(
            &mut headers,
            DUMP_CAPTURED_WATERMARK_HEADER,
            &source.captured_watermark().get().to_string(),
        )?;
        insert_header(
            &mut headers,
            DUMP_EXPORTED_THROUGH_HEADER,
            &exported_through.get().to_string(),
        )?;
        insert_header(
            &mut headers,
            DUMP_API_SCHEMA_VERSION_HEADER,
            &DUMP_API_SCHEMA_VERSION.to_string(),
        )?;
        insert_header(
            &mut headers,
            DUMP_EVENT_SCHEMA_VERSION_HEADER,
            &metadata.event_schema_version().to_string(),
        )?;
        insert_header(
            &mut headers,
            DUMP_EXPORTER_SCHEMA_VERSION_HEADER,
            &producer.exporter_schema_version().to_string(),
        )?;
        insert_header(
            &mut headers,
            DUMP_TROUPE_VERSION_HEADER,
            producer.troupe_version(),
        )?;
        insert_header(&mut headers, DUMP_PRODUCTION_OUTCOME_HEADER, outcome)?;
        insert_header(&mut headers, DUMP_CLEAN_SHUTDOWN_HEADER, clean_shutdown)?;
        insert_header(
            &mut headers,
            DUMP_CONTENT_WARNING_HEADER,
            producer.content_warning(),
        )?;
        Ok(Self { headers })
    }

    fn response(self, body: DumpBody) -> RouteResponse {
        let mut response = RouteResponse::stream(StatusCode::OK, body);
        *response.headers_mut() = self.headers;
        response
    }
}

fn insert_static(
    headers: &mut HeaderMap,
    name: &'static str,
    value: &'static str,
) -> Result<(), ClientError> {
    insert_header(headers, name, value)
}

fn insert_header(
    headers: &mut HeaderMap,
    name: &'static str,
    value: &str,
) -> Result<(), ClientError> {
    let value = HeaderValue::from_str(value).map_err(|_| ClientError::dump_failed())?;
    headers.insert(name, value);
    Ok(())
}

struct StartupGate {
    sender: Mutex<Option<oneshot::Sender<Result<PreparedResponse, ClientError>>>>,
}

impl StartupGate {
    fn new() -> Self {
        Self {
            sender: Mutex::new(None),
        }
    }

    fn install(&self, sender: oneshot::Sender<Result<PreparedResponse, ClientError>>) {
        *lock(&self.sender) = Some(sender);
    }

    fn ready(&self, prepared: PreparedResponse) -> bool {
        lock(&self.sender)
            .take()
            .is_some_and(|sender| sender.send(Ok(prepared)).is_ok())
    }

    fn fail(&self, error: ClientError) -> bool {
        lock(&self.sender)
            .take()
            .is_some_and(|sender| sender.send(Err(error)).is_ok())
    }

    fn is_pending(&self) -> bool {
        lock(&self.sender).is_some()
    }
}

struct GatedWriter {
    bridge: Arc<Bridge>,
    gate: Arc<StartupGate>,
    prepared: Option<PreparedResponse>,
}

impl GatedWriter {
    fn new(bridge: Arc<Bridge>, gate: Arc<StartupGate>, prepared: PreparedResponse) -> Self {
        Self {
            bridge,
            gate,
            prepared: Some(prepared),
        }
    }
}

impl AsyncWrite for GatedWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if let Some(prepared) = self.prepared.take() {
            if !self.gate.ready(prepared) {
                self.bridge.cancel();
                return Poll::Ready(Err(disconnected()));
            }
            if self.bridge.wait_for_body(context.waker()) {
                context.waker().wake_by_ref();
            }
            return Poll::Pending;
        }
        self.bridge.poll_write(context, bytes)
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.bridge.poll_flush(context)
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.bridge.poll_flush(context)
    }
}

struct Bridge {
    state: Mutex<BridgeState>,
}

struct BridgeState {
    body_started: bool,
    cancelled: bool,
    chunk: Option<Bytes>,
    terminal: BridgeTerminal,
    producer_waker: Option<Waker>,
    body_waker: Option<Waker>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BridgeTerminal {
    Open,
    Complete,
    Failed(DumpBodyError),
}

impl Bridge {
    fn new() -> Self {
        Self {
            state: Mutex::new(BridgeState {
                body_started: false,
                cancelled: false,
                chunk: None,
                terminal: BridgeTerminal::Open,
                producer_waker: None,
                body_waker: None,
            }),
        }
    }

    fn wait_for_body(&self, waker: &Waker) -> bool {
        let mut state = lock(&self.state);
        if state.body_started {
            return true;
        }
        state.producer_waker = Some(waker.clone());
        false
    }

    fn poll_write(&self, context: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        let mut state = lock(&self.state);
        state.producer_waker = Some(context.waker().clone());
        if state.cancelled {
            return Poll::Ready(Err(disconnected()));
        }
        if !state.body_started || state.chunk.is_some() {
            return Poll::Pending;
        }
        if bytes.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let accepted = bytes.len().min(MAX_RESPONSE_CHUNK_BYTES);
        state.chunk = Some(Bytes::copy_from_slice(&bytes[..accepted]));
        state.producer_waker = None;
        let body_waker = state.body_waker.take();
        drop(state);
        if let Some(waker) = body_waker {
            waker.wake();
        }
        Poll::Ready(Ok(accepted))
    }

    fn poll_flush(&self, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut state = lock(&self.state);
        state.producer_waker = Some(context.waker().clone());
        if state.cancelled {
            return Poll::Ready(Err(disconnected()));
        }
        if state.chunk.is_some() {
            Poll::Pending
        } else {
            state.producer_waker = None;
            Poll::Ready(Ok(()))
        }
    }

    fn finish(&self, result: Result<(), DumpBodyError>) {
        let mut state = lock(&self.state);
        if state.cancelled {
            return;
        }
        state.terminal = match result {
            Ok(()) => BridgeTerminal::Complete,
            Err(error) => BridgeTerminal::Failed(error),
        };
        let body_waker = state.body_waker.take();
        drop(state);
        if let Some(waker) = body_waker {
            waker.wake();
        }
    }

    fn cancel(&self) {
        let mut state = lock(&self.state);
        state.cancelled = true;
        state.chunk = None;
        let producer_waker = state.producer_waker.take();
        let body_waker = state.body_waker.take();
        drop(state);
        if let Some(waker) = producer_waker {
            waker.wake();
        }
        if let Some(waker) = body_waker {
            waker.wake();
        }
    }
}

struct PendingRequestCancellation {
    bridge: Arc<Bridge>,
    armed: bool,
}

impl PendingRequestCancellation {
    fn new(bridge: Arc<Bridge>) -> Self {
        Self {
            bridge,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingRequestCancellation {
    fn drop(&mut self) {
        if self.armed {
            self.bridge.cancel();
        }
    }
}

struct DumpBody {
    bridge: Arc<Bridge>,
}

impl DumpBody {
    fn new(bridge: Arc<Bridge>) -> Self {
        Self { bridge }
    }
}

impl Body for DumpBody {
    type Data = Bytes;
    type Error = DumpBodyError;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let mut state = lock(&self.bridge.state);
        state.body_waker = Some(context.waker().clone());
        let producer_waker = if !state.body_started {
            state.body_started = true;
            state.producer_waker.take()
        } else {
            None
        };
        if let Some(chunk) = state.chunk.take() {
            let producer_waker = state.producer_waker.take().or(producer_waker);
            drop(state);
            if let Some(waker) = producer_waker {
                waker.wake();
            }
            return Poll::Ready(Some(Ok(Frame::data(chunk))));
        }
        if state.cancelled {
            drop(state);
            if let Some(waker) = producer_waker {
                waker.wake();
            }
            return Poll::Ready(None);
        }
        let result = match state.terminal {
            BridgeTerminal::Open => Poll::Pending,
            BridgeTerminal::Complete => Poll::Ready(None),
            BridgeTerminal::Failed(error) => {
                state.terminal = BridgeTerminal::Complete;
                Poll::Ready(Some(Err(error)))
            }
        };
        drop(state);
        if let Some(waker) = producer_waker {
            waker.wake();
        }
        result
    }

    fn is_end_stream(&self) -> bool {
        let state = lock(&self.bridge.state);
        state.chunk.is_none()
            && (state.cancelled || matches!(state.terminal, BridgeTerminal::Complete))
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}

impl Drop for DumpBody {
    fn drop(&mut self) {
        self.bridge.cancel();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DumpBodyError(&'static str);

impl DumpBodyError {
    const fn producer_failed() -> Self {
        Self("Perfetto response stream failed")
    }

    const fn worker_failed() -> Self {
        Self("Perfetto response worker failed")
    }
}

impl fmt::Display for DumpBodyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for DumpBodyError {}

fn disconnected() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "Perfetto response body disconnected",
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

    const fn invalid_query() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            "only one optional through parameter is accepted",
        )
    }

    const fn invalid_through() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_through",
            "through must be a canonical decimal u64",
        )
    }

    const fn future_through() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "through_not_captured",
            "through exceeds the captured watermark",
        )
    }

    const fn unsupported_format() -> Self {
        Self::new(
            StatusCode::NOT_ACCEPTABLE,
            "unsupported_format",
            "the Perfetto trace is available only as application/x-protobuf",
        )
    }

    const fn dump_failed() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "dump_failed",
            "the Perfetto trace could not be generated",
        )
    }

    fn response(self, run_id: CanonicalUuid) -> RouteResponse {
        let bytes = serde_json::to_vec(&ErrorResponse {
            api_schema_version: DUMP_API_SCHEMA_VERSION,
            run_id,
            error: ErrorBody {
                code: self.code,
                message: self.message,
                details: None,
            },
        })
        .expect("closed dump errors are JSON serializable");
        RouteResponse::bytes(self.status, bytes).with_header(
            CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        )
    }
}

#[derive(Serialize)]
struct ErrorResponse {
    api_schema_version: u8,
    run_id: CanonicalUuid,
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: &'static str,
    details: Option<()>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
