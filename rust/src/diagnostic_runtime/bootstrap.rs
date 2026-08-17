use std::{
    fmt,
    net::SocketAddr,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread::{self, JoinHandle, Thread},
    time::{Duration, Instant},
};

use pyo3::prelude::*;
use troupe_diagnostics_core::{
    hub::{
        AdmissionReserver, AdmissionSize, LiveEventNotifier, MandatoryDurableReserver,
        ProductionDiagnosticHub,
    },
    id::CanonicalUuid,
    scalar::SchemaU64,
};
use troupe_diagnostics_runtime::{
    archive::{
        layout::ArchiveLayout,
        lease::{ActiveArchiveLease, ActiveArchiveLeaseGuard},
    },
    registry::{
        model::{BindEndpoint, RegistryEntry, WebBaseUrl},
        process_identity::current_process_identity,
        publish::{
            ListenerState, RegistryPublication, RegistryPublicationReadiness,
            publish_registry_entry,
        },
    },
    server::{
        error::{ServerCoreFailure, ServerStartErrorCode},
        identity::{OperationalLimits, ServerIdentity},
        routes::RouteDefinition,
        runtime::{DiagnosticServer, ServerConfig},
    },
    store::{
        admission::{
            IngressAdmissionError, IngressFailureReceiver, IngressReservation, MandatoryIngress,
        },
        batch::{BatchAccumulator, TriggeredBatch},
        connection::{DiagnosticStore, InitialStoreMetadata},
        key::SortableU64Key,
        progress::{
            WriterCoreFailure, WriterDeadlines, WriterProgressSample, WriterProgressStatus,
            WriterProgressSupervisor,
        },
        quota::{QuotaError, QuotaFailure, QuotaFailureReceiver, QuotaStatus, RunQuota},
        watermark::{CommitNotification, CommitObserver},
        writer::{FinalProductionOutcome, TransactionalWriter},
    },
};
use uuid::Uuid;

use crate::application::loader::PrevalidatedProductionRoot;

pub(crate) use super::shutdown::{CleanupFailure, DiagnosticShutdownError};
use super::shutdown::{OrderedShutdownResources, ShutdownMetadata, run_ordered_shutdown};

const WRITER_POLL_INTERVAL: Duration = Duration::from_millis(1);

pub(crate) type DurableDiagnosticHub = ProductionDiagnosticHub<DurableAdmission>;
type RouteFactory = Box<
    dyn FnOnce(BootstrapRouteContext) -> Result<Vec<RouteDefinition>, BootstrapRouteAssemblyError>
        + Send,
>;
type RuntimeWriter = TransactionalWriter<BootstrapCommitObserver>;

#[derive(Clone)]
struct CoordinatedRunQuota {
    quota: RunQuota,
    origin: Instant,
    operation: Arc<Mutex<()>>,
}

impl CoordinatedRunQuota {
    fn new(quota: RunQuota) -> Self {
        Self {
            quota,
            origin: Instant::now(),
            operation: Arc::new(Mutex::new(())),
        }
    }

    fn precheck(&self, conservative_growth_bytes: u64) -> Result<(), QuotaError> {
        let _operation = self
            .operation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.quota
            .precheck(self.origin.elapsed(), conservative_growth_bytes)?;
        Ok(())
    }

    fn post_growth_measurement(&self) -> Result<(), QuotaError> {
        let _operation = self
            .operation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.quota.post_growth_measurement(self.origin.elapsed())?;
        Ok(())
    }
}

pub(crate) struct DurableAdmission {
    ingress: MandatoryIngress,
    quota: CoordinatedRunQuota,
}

impl DurableAdmission {
    fn new(ingress: MandatoryIngress, quota: CoordinatedRunQuota) -> Self {
        Self { ingress, quota }
    }

    fn quota_precheck(&self, size: AdmissionSize) -> Result<(), DurableAdmissionError> {
        let conservative_growth = u64::try_from(size.canonical_bytes()).unwrap_or(u64::MAX);
        if let Err(error) = self.quota.precheck(conservative_growth) {
            let _ = self.ingress.seal_for_external_core_failure();
            return Err(DurableAdmissionError::Quota(error));
        }
        Ok(())
    }
}

impl AdmissionReserver for DurableAdmission {
    type Error = DurableAdmissionError;
    type Reservation = IngressReservation;

    fn try_reserve(&mut self, size: AdmissionSize) -> Result<Self::Reservation, Self::Error> {
        self.quota_precheck(size)?;
        self.ingress
            .try_reserve(size)
            .map_err(DurableAdmissionError::Ingress)
    }

    fn try_reserve_fatal(&mut self, size: AdmissionSize) -> Result<Self::Reservation, Self::Error> {
        self.quota_precheck(size)?;
        self.ingress
            .try_reserve_fatal(size)
            .map_err(DurableAdmissionError::Ingress)
    }
}

impl MandatoryDurableReserver for DurableAdmission {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DurableAdmissionError {
    Quota(QuotaError),
    Ingress(IngressAdmissionError),
}

impl fmt::Display for DurableAdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Quota(error) => fmt::Display::fmt(error, formatter),
            Self::Ingress(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for DurableAdmissionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Quota(error) => Some(error),
            Self::Ingress(error) => Some(error),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct BootstrapConfig {
    started_at: String,
    configuration_identity: String,
    bind_host: String,
    port: u16,
    advertise_url: Option<WebBaseUrl>,
    max_run_bytes: Option<u64>,
    writer_deadlines: WriterDeadlines,
}

impl BootstrapConfig {
    pub(crate) fn new(
        started_at: impl Into<String>,
        configuration_identity: impl Into<String>,
    ) -> Self {
        Self {
            started_at: started_at.into(),
            configuration_identity: configuration_identity.into(),
            bind_host: "0.0.0.0".to_owned(),
            port: 0,
            advertise_url: None,
            max_run_bytes: None,
            writer_deadlines: WriterDeadlines::default(),
        }
    }

    pub(crate) fn with_bind(mut self, host: impl Into<String>, port: u16) -> Self {
        self.bind_host = host.into();
        self.port = port;
        self
    }

    pub(crate) fn with_advertise_url(mut self, advertise_url: Option<WebBaseUrl>) -> Self {
        self.advertise_url = advertise_url;
        self
    }

    pub(crate) fn with_max_run_bytes(mut self, max_run_bytes: Option<u64>) -> Self {
        self.max_run_bytes = max_run_bytes;
        self
    }

    pub(crate) fn with_writer_deadlines(mut self, writer_deadlines: WriterDeadlines) -> Self {
        self.writer_deadlines = writer_deadlines;
        self
    }

    fn operational_limits(&self) -> Result<OperationalLimits, BootstrapError> {
        let mut limits = OperationalLimits::default();
        limits
            .set_limit(
                "writer_stall_timeout_ms",
                duration_millis(self.writer_deadlines.writer_stall_timeout())?,
            )
            .map_err(|error| {
                BootstrapError::new(
                    BootstrapPhase::ListenerReady,
                    "bootstrap.operational_limits_invalid",
                    error,
                )
            })?;
        limits
            .set_limit(
                "shutdown_drain_timeout_ms",
                duration_millis(self.writer_deadlines.shutdown_drain_timeout())?,
            )
            .map_err(|error| {
                BootstrapError::new(
                    BootstrapPhase::ListenerReady,
                    "bootstrap.operational_limits_invalid",
                    error,
                )
            })?;
        if let Some(max_run_bytes) = self.max_run_bytes {
            limits
                .set_limit("max_run_bytes", max_run_bytes)
                .map_err(|error| {
                    BootstrapError::new(
                        BootstrapPhase::ListenerReady,
                        "bootstrap.operational_limits_invalid",
                        error,
                    )
                })?;
        }
        Ok(limits)
    }
}

fn duration_millis(value: Duration) -> Result<u64, BootstrapError> {
    u64::try_from(value.as_millis()).map_err(|_| {
        BootstrapError::message(
            BootstrapPhase::ListenerReady,
            "bootstrap.operational_limit_overflow",
            "writer deadline does not fit the server identity",
        )
    })
}

pub(crate) struct BootstrapComponents {
    live_notifier: Box<dyn LiveEventNotifier>,
    commit_observer: Box<dyn CommitObserver + Send>,
    route_factory: RouteFactory,
    final_stream_closer: Box<dyn FinalStreamCloser>,
}

pub(crate) trait FinalStreamCloser: Send {
    fn close_stream(&self, reason: &str, final_watermark: SchemaU64) -> Result<(), String>;
}

impl FinalStreamCloser for () {
    fn close_stream(&self, _reason: &str, _final_watermark: SchemaU64) -> Result<(), String> {
        Ok(())
    }
}

impl BootstrapComponents {
    pub(crate) fn new<F>(
        live_notifier: Box<dyn LiveEventNotifier>,
        commit_observer: Box<dyn CommitObserver + Send>,
        route_factory: F,
    ) -> Self
    where
        F: FnOnce(
                BootstrapRouteContext,
            ) -> Result<Vec<RouteDefinition>, BootstrapRouteAssemblyError>
            + Send
            + 'static,
    {
        Self {
            live_notifier,
            commit_observer,
            route_factory: Box::new(route_factory),
            final_stream_closer: Box::new(()),
        }
    }

    pub(crate) fn with_final_stream_closer(
        mut self,
        final_stream_closer: Box<dyn FinalStreamCloser>,
    ) -> Self {
        self.final_stream_closer = final_stream_closer;
        self
    }
}

#[derive(Clone)]
pub(crate) struct BootstrapRouteContext {
    run_id: CanonicalUuid,
    run_directory: PathBuf,
    active_lease: Arc<ActiveArchiveLease>,
    hub: Arc<DurableDiagnosticHub>,
}

impl BootstrapRouteContext {
    pub(crate) const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub(crate) fn run_directory(&self) -> &Path {
        &self.run_directory
    }

    pub(crate) fn active_lease(&self) -> &Arc<ActiveArchiveLease> {
        &self.active_lease
    }

    pub(crate) fn hub(&self) -> &Arc<DurableDiagnosticHub> {
        &self.hub
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BootstrapRouteAssemblyError(String);

impl BootstrapRouteAssemblyError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for BootstrapRouteAssemblyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for BootstrapRouteAssemblyError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BootstrapPhase {
    RunIdentityAllocated,
    ArchivePrepared,
    ActiveLeaseAcquired,
    InitialStoreReady,
    WriterSupervisorReady,
    ListenerReady,
    RegistryPublished,
    ReadyResult,
}

impl BootstrapPhase {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::RunIdentityAllocated => "run_identity_allocated",
            Self::ArchivePrepared => "archive_prepared",
            Self::ActiveLeaseAcquired => "active_lease_acquired",
            Self::InitialStoreReady => "initial_store_ready",
            Self::WriterSupervisorReady => "writer_supervisor_ready",
            Self::ListenerReady => "listener_ready",
            Self::RegistryPublished => "registry_published",
            Self::ReadyResult => "ready_result",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BootstrapError {
    phase: BootstrapPhase,
    code: String,
    message: String,
    cleanup_failures: Vec<CleanupFailure>,
}

impl BootstrapError {
    fn new(phase: BootstrapPhase, code: impl Into<String>, error: impl fmt::Display) -> Self {
        Self::message(phase, code, error.to_string())
    }

    fn message(phase: BootstrapPhase, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            phase,
            code: code.into(),
            message: message.into(),
            cleanup_failures: Vec::new(),
        }
    }

    fn with_cleanup_failures(mut self, cleanup_failures: Vec<CleanupFailure>) -> Self {
        self.cleanup_failures = cleanup_failures;
        self
    }

    pub(crate) const fn phase(&self) -> BootstrapPhase {
        self.phase
    }

    pub(crate) fn code(&self) -> &str {
        &self.code
    }

    pub(crate) fn cleanup_failures(&self) -> &[CleanupFailure] {
        &self.cleanup_failures
    }
}

impl fmt::Display for BootstrapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "diagnostic bootstrap failed during {} [{}]: {}",
            self.phase.as_str(),
            self.code,
            self.message
        )?;
        if !self.cleanup_failures.is_empty() {
            write!(
                formatter,
                "; {} cleanup operation(s) also failed",
                self.cleanup_failures.len()
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for BootstrapError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiagnosticCoreFailure {
    component: &'static str,
    stage: &'static str,
    code: String,
    message: String,
}

impl DiagnosticCoreFailure {
    fn new(
        component: &'static str,
        stage: &'static str,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            component,
            stage,
            code: code.into(),
            message: message.into(),
        }
    }

    fn writer(error: impl fmt::Display, code: impl Into<String>) -> Self {
        Self::new("writer", "commit", code, error.to_string())
    }

    fn supervised(failure: WriterCoreFailure) -> Self {
        Self::new(
            failure.component(),
            failure.stage().as_str(),
            failure.code().as_str(),
            format!(
                "diagnostic writer supervisor reported {}",
                failure.code().as_str()
            ),
        )
    }

    fn server(failure: ServerCoreFailure) -> Self {
        Self::new(
            "server",
            "execution",
            server_core_error_code(failure.code()),
            failure.to_string(),
        )
    }

    pub(crate) const fn component(&self) -> &'static str {
        self.component
    }

    pub(crate) const fn stage(&self) -> &'static str {
        self.stage
    }

    pub(crate) fn code(&self) -> &str {
        &self.code
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

pub(crate) struct DiagnosticRuntimeGuard {
    layout: ArchiveLayout,
    active_lease: Option<Arc<ActiveArchiveLease>>,
    ingress: Option<MandatoryIngress>,
    quota: Option<RunQuota>,
    hub: Option<Arc<DurableDiagnosticHub>>,
    writer: Option<WriterSupervisor>,
    server: Option<DiagnosticServer>,
    publication: Option<RegistryPublication>,
    final_stream_closer: Option<Box<dyn FinalStreamCloser>>,
    shutdown_attempted: bool,
}

impl DiagnosticRuntimeGuard {
    pub(crate) const fn run_id(&self) -> CanonicalUuid {
        self.layout.run_id()
    }

    pub(crate) const fn layout(&self) -> &ArchiveLayout {
        &self.layout
    }

    pub(crate) fn hub(&self) -> &Arc<DurableDiagnosticHub> {
        self.hub
            .as_ref()
            .expect("a live diagnostic guard owns its hub")
    }

    pub(crate) fn active_lease_guard(&self) -> ActiveArchiveLeaseGuard<'_> {
        self.active_lease
            .as_ref()
            .expect("a live diagnostic guard owns its active lease")
            .guard()
    }

    pub(crate) fn server_identity(&self) -> &ServerIdentity {
        self.server
            .as_ref()
            .expect("a live diagnostic guard owns its server")
            .identity()
    }

    pub(crate) fn connect_addr(&self) -> SocketAddr {
        self.server
            .as_ref()
            .expect("a live diagnostic guard owns its server")
            .connect_addr()
    }

    pub(crate) fn locator_path(&self) -> &Path {
        self.publication
            .as_ref()
            .expect("a ready diagnostic guard owns its registry publication")
            .locator_path()
    }

    pub(crate) fn writer_progress(&self) -> Result<WriterProgressStatus, DiagnosticCoreFailure> {
        self.writer
            .as_ref()
            .expect("a live diagnostic guard owns its writer")
            .progress_status()
    }

    pub(crate) fn quota_status(&self) -> Result<QuotaStatus, DiagnosticCoreFailure> {
        self.quota
            .as_ref()
            .expect("a live diagnostic guard owns its quota")
            .status()
            .map_err(|error| {
                DiagnosticCoreFailure::new(
                    "quota",
                    "status",
                    "run_quota_state_unavailable",
                    error.to_string(),
                )
            })
    }

    pub(crate) fn try_core_failure(&self) -> Option<DiagnosticCoreFailure> {
        self.writer
            .as_ref()
            .and_then(WriterSupervisor::try_core_failure)
            .or_else(|| {
                self.server
                    .as_ref()
                    .and_then(DiagnosticServer::try_core_failure)
                    .map(DiagnosticCoreFailure::server)
            })
    }

    pub(crate) fn seal_for_core_failure(&self) -> Result<(), DiagnosticCoreFailure> {
        self.ingress
            .as_ref()
            .expect("a live diagnostic guard owns its ingress")
            .seal_for_external_core_failure()
            .map(|_| ())
            .map_err(|error| {
                DiagnosticCoreFailure::new(
                    "ingress",
                    "seal",
                    "diagnostic_ingress.seal_failed",
                    error.to_string(),
                )
            })
    }

    pub(crate) fn shutdown(mut self) -> Result<(), DiagnosticShutdownError> {
        self.shutdown_attempted = true;
        let failures = cleanup_resources(
            &mut self.publication,
            &mut self.server,
            &mut self.hub,
            &mut self.writer,
            &mut self.ingress,
            &mut self.quota,
            None,
            &mut self.active_lease,
        );
        if failures.is_empty() {
            Ok(())
        } else {
            Err(DiagnosticShutdownError::new(failures))
        }
    }

    pub(crate) fn shutdown_clean(
        mut self,
        metadata: ShutdownMetadata,
    ) -> Result<(), DiagnosticShutdownError> {
        self.shutdown_attempted = true;
        run_ordered_shutdown(&mut self, &metadata)
    }
}

impl fmt::Debug for DiagnosticRuntimeGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiagnosticRuntimeGuard")
            .field("run_id", &self.run_id())
            .field("run_directory", &self.layout.run_directory())
            .field("server", &self.server)
            .field("publication", &self.publication)
            .finish_non_exhaustive()
    }
}

impl Drop for DiagnosticRuntimeGuard {
    fn drop(&mut self) {
        if self.shutdown_attempted {
            return;
        }
        self.shutdown_attempted = true;
        let failures = cleanup_resources(
            &mut self.publication,
            &mut self.server,
            &mut self.hub,
            &mut self.writer,
            &mut self.ingress,
            &mut self.quota,
            None,
            &mut self.active_lease,
        );
        report_drop_failures("diagnostic Runtime guard drop failed", &failures);
    }
}

impl OrderedShutdownResources for DiagnosticRuntimeGuard {
    fn seal_ingress(&mut self) -> Result<(), CleanupFailure> {
        self.ingress
            .as_ref()
            .expect("a live diagnostic guard owns its ingress")
            .seal_normal_ingress()
            .map(|_| ())
            .map_err(|error| CleanupFailure::new("writer", "writer_ingress_seal_failed", error))
    }

    fn finalize_writer(
        &mut self,
        metadata: &ShutdownMetadata,
    ) -> Result<SchemaU64, CleanupFailure> {
        self.writer
            .as_mut()
            .expect("a live diagnostic guard owns its writer")
            .finalize(metadata.ended_at(), metadata.outcome())
            .map(|watermark| SchemaU64::new(watermark.get()))
    }

    fn close_live_stream(
        &mut self,
        reason: &str,
        final_watermark: SchemaU64,
    ) -> Result<(), CleanupFailure> {
        self.final_stream_closer
            .as_ref()
            .expect("a live diagnostic guard owns its stream closer")
            .close_stream(reason, final_watermark)
            .map_err(|error| CleanupFailure::new("stream", "stream_close_failed", error))
    }

    fn unpublish_registry(&mut self) -> Result<(), CleanupFailure> {
        self.publication
            .take()
            .expect("a live diagnostic guard owns its registry publication")
            .unpublish(ListenerState::Running)
            .map_err(|error| CleanupFailure::new("registry", error.code().as_str(), error))
    }

    fn close_listener_and_readers(&mut self) -> Result<(), CleanupFailure> {
        self.server
            .take()
            .expect("a live diagnostic guard owns its server")
            .shutdown()
            .map_err(|error| CleanupFailure::new("server", "server_shutdown_failed", error))
    }

    fn close_writer_and_store(&mut self) -> Vec<CleanupFailure> {
        self.hub.take();
        match self
            .writer
            .take()
            .expect("a live diagnostic guard owns its writer")
            .close()
        {
            Ok(()) => Vec::new(),
            Err(failures) => failures,
        }
    }

    fn release_runtime_resources(&mut self) {
        self.final_stream_closer.take();
        self.quota.take();
        self.ingress.take();
        self.active_lease.take();
    }
}

pub(crate) fn bootstrap(
    py: Python<'_>,
    production_root: &PrevalidatedProductionRoot,
    config: BootstrapConfig,
    components: BootstrapComponents,
) -> Result<DiagnosticRuntimeGuard, BootstrapError> {
    let production_root = production_root
        .production_root(py)
        .extract::<PathBuf>()
        .map_err(|error| {
            BootstrapError::new(
                BootstrapPhase::RunIdentityAllocated,
                "bootstrap.production_root_extract_failed",
                error,
            )
        })?;
    let run_id = CanonicalUuid::new(Uuid::new_v4());
    bootstrap_root(
        &production_root,
        run_id,
        config,
        components,
        &mut NoopCheckpoint,
    )
}

trait BootstrapCheckpoint {
    fn reached(
        &mut self,
        phase: BootstrapPhase,
        snapshot: &BootstrapSnapshot,
    ) -> Result<(), BootstrapError>;
}

struct NoopCheckpoint;

impl BootstrapCheckpoint for NoopCheckpoint {
    fn reached(
        &mut self,
        _phase: BootstrapPhase,
        _snapshot: &BootstrapSnapshot,
    ) -> Result<(), BootstrapError> {
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct BootstrapSnapshot {
    run_id: CanonicalUuid,
    run_directory: Option<PathBuf>,
    connect_addr: Option<SocketAddr>,
    locator_path: Option<PathBuf>,
}

#[derive(Default)]
struct StartupState {
    layout: Option<ArchiveLayout>,
    active_lease: Option<Arc<ActiveArchiveLease>>,
    store: Option<DiagnosticStore>,
    ingress: Option<MandatoryIngress>,
    quota: Option<RunQuota>,
    hub: Option<Arc<DurableDiagnosticHub>>,
    writer: Option<WriterSupervisor>,
    server: Option<DiagnosticServer>,
    publication: Option<RegistryPublication>,
    final_stream_closer: Option<Box<dyn FinalStreamCloser>>,
}

impl StartupState {
    fn snapshot(&self, run_id: CanonicalUuid) -> BootstrapSnapshot {
        BootstrapSnapshot {
            run_id,
            run_directory: self
                .layout
                .as_ref()
                .map(|layout| layout.run_directory().to_path_buf()),
            connect_addr: self.server.as_ref().map(DiagnosticServer::connect_addr),
            locator_path: self
                .publication
                .as_ref()
                .map(|publication| publication.locator_path().to_path_buf()),
        }
    }

    fn rollback(mut self, error: BootstrapError) -> BootstrapError {
        let failures = cleanup_resources(
            &mut self.publication,
            &mut self.server,
            &mut self.hub,
            &mut self.writer,
            &mut self.ingress,
            &mut self.quota,
            self.store.take(),
            &mut self.active_lease,
        );
        error.with_cleanup_failures(failures)
    }
}

fn bootstrap_root<C: BootstrapCheckpoint>(
    production_root: &Path,
    run_id: CanonicalUuid,
    config: BootstrapConfig,
    components: BootstrapComponents,
    checkpoint: &mut C,
) -> Result<DiagnosticRuntimeGuard, BootstrapError> {
    let mut state = StartupState::default();
    if let Err(error) = checkpoint.reached(
        BootstrapPhase::RunIdentityAllocated,
        &state.snapshot(run_id),
    ) {
        return Err(state.rollback(error));
    }

    let layout = ArchiveLayout::prepare(production_root, run_id).map_err(|error| {
        BootstrapError::new(
            BootstrapPhase::ArchivePrepared,
            error.code().as_str(),
            error,
        )
    })?;
    state.layout = Some(layout);
    if let Err(error) = checkpoint.reached(BootstrapPhase::ArchivePrepared, &state.snapshot(run_id))
    {
        return Err(state.rollback(error));
    }

    let active_lease = ActiveArchiveLease::acquire(
        state
            .layout
            .as_ref()
            .expect("archive layout was installed")
            .run_directory(),
    )
    .map_err(|error| {
        BootstrapError::new(
            BootstrapPhase::ActiveLeaseAcquired,
            error.code().as_str(),
            error,
        )
    });
    let active_lease = match active_lease {
        Ok(active_lease) => active_lease,
        Err(error) => return Err(state.rollback(error)),
    };
    state.active_lease = Some(Arc::new(active_lease));
    if let Err(error) =
        checkpoint.reached(BootstrapPhase::ActiveLeaseAcquired, &state.snapshot(run_id))
    {
        return Err(state.rollback(error));
    }

    let initial = InitialStoreMetadata::new(
        run_id,
        config.started_at.clone(),
        config.configuration_identity.clone(),
    );
    let store = DiagnosticStore::create(
        state
            .layout
            .as_ref()
            .expect("archive layout was installed")
            .run_directory(),
        &initial,
    )
    .map_err(|error| {
        BootstrapError::new(
            BootstrapPhase::InitialStoreReady,
            error.code().as_str(),
            error,
        )
    });
    let store = match store {
        Ok(store) => store,
        Err(error) => return Err(state.rollback(error)),
    };
    state.store = Some(store);
    if let Err(error) =
        checkpoint.reached(BootstrapPhase::InitialStoreReady, &state.snapshot(run_id))
    {
        return Err(state.rollback(error));
    }

    let (ingress, ingress_failures) = MandatoryIngress::new();
    let quota = RunQuota::new(
        state
            .layout
            .as_ref()
            .expect("archive layout was installed")
            .run_directory(),
        config.max_run_bytes,
    )
    .map_err(|error| {
        BootstrapError::new(
            BootstrapPhase::WriterSupervisorReady,
            quota_configuration_code(error),
            error,
        )
    });
    let (quota, quota_failures) = match quota {
        Ok(quota) => quota,
        Err(error) => return Err(state.rollback(error)),
    };
    let coordinated_quota = CoordinatedRunQuota::new(quota.clone());
    if let Err(error) = coordinated_quota
        .post_growth_measurement()
        .map_err(|error| {
            BootstrapError::new(
                BootstrapPhase::WriterSupervisorReady,
                quota_error_code(&error),
                error,
            )
        })
    {
        return Err(state.rollback(error));
    }
    let BootstrapComponents {
        live_notifier,
        commit_observer,
        route_factory,
        final_stream_closer,
    } = components;
    state.final_stream_closer = Some(final_stream_closer);
    let writer = WriterSupervisor::start(
        state.store.take().expect("initial store was installed"),
        ingress.clone(),
        ingress_failures,
        coordinated_quota.clone(),
        quota_failures,
        config.writer_deadlines,
        commit_observer,
    );
    let writer = match writer {
        Ok(writer) => writer,
        Err(error) => return Err(state.rollback(error)),
    };
    let hub = Arc::new(ProductionDiagnosticHub::production(
        run_id,
        DurableAdmission::new(ingress.clone(), coordinated_quota),
        live_notifier,
    ));
    state.ingress = Some(ingress);
    state.quota = Some(quota);
    state.writer = Some(writer);
    state.hub = Some(hub);
    if let Err(error) = checkpoint.reached(
        BootstrapPhase::WriterSupervisorReady,
        &state.snapshot(run_id),
    ) {
        return Err(state.rollback(error));
    }

    let route_context = BootstrapRouteContext {
        run_id,
        run_directory: state
            .layout
            .as_ref()
            .expect("archive layout was installed")
            .run_directory()
            .to_path_buf(),
        active_lease: Arc::clone(
            state
                .active_lease
                .as_ref()
                .expect("active lease was installed"),
        ),
        hub: Arc::clone(state.hub.as_ref().expect("diagnostic hub was installed")),
    };
    let routes = match route_factory(route_context) {
        Ok(routes) => routes,
        Err(error) => {
            return Err(state.rollback(BootstrapError::new(
                BootstrapPhase::ListenerReady,
                "bootstrap.route_assembly_failed",
                error,
            )));
        }
    };
    let process_identity = match current_process_identity() {
        Ok(identity) => identity,
        Err(error) => {
            return Err(state.rollback(BootstrapError::new(
                BootstrapPhase::ListenerReady,
                "bootstrap.process_identity_unavailable",
                error,
            )));
        }
    };
    let operational_limits = match config.operational_limits() {
        Ok(limits) => limits,
        Err(error) => return Err(state.rollback(error)),
    };
    let server_config = ServerConfig::new(run_id, std::process::id(), process_identity)
        .with_bind(config.bind_host.clone(), config.port)
        .with_advertise_url(config.advertise_url.clone())
        .with_operational_limits(operational_limits);
    let server = DiagnosticServer::start(server_config, routes).map_err(|error| {
        BootstrapError::new(
            BootstrapPhase::ListenerReady,
            server_start_error_code(error.code()),
            error,
        )
    });
    let server = match server {
        Ok(server) => server,
        Err(error) => return Err(state.rollback(error)),
    };
    state.server = Some(server);
    if let Err(error) = checkpoint.reached(BootstrapPhase::ListenerReady, &state.snapshot(run_id)) {
        return Err(state.rollback(error));
    }

    let identity = state
        .server
        .as_ref()
        .expect("diagnostic server was installed")
        .identity();
    let bind = match BindEndpoint::new(identity.bind_host(), identity.port()) {
        Ok(bind) => bind,
        Err(error) => {
            return Err(state.rollback(BootstrapError::new(
                BootstrapPhase::RegistryPublished,
                "bootstrap.registry_entry_invalid",
                error,
            )));
        }
    };
    let entry = RegistryEntry::new(
        run_id,
        state
            .layout
            .as_ref()
            .expect("archive layout was installed")
            .run_directory(),
        identity.owner_pid(),
        identity.process_identity().clone(),
        bind,
        identity.advertise_url().cloned(),
        &config.started_at,
    );
    let entry = match entry {
        Ok(entry) => entry,
        Err(error) => {
            return Err(state.rollback(BootstrapError::new(
                BootstrapPhase::RegistryPublished,
                "bootstrap.registry_entry_invalid",
                error,
            )));
        }
    };
    let publication = publish_registry_entry(
        state.layout.as_ref().expect("archive layout was installed"),
        &entry,
        RegistryPublicationReadiness::new(true, true),
    );
    let publication = match publication {
        Ok(publication) => publication,
        Err(error) => {
            let bootstrap_error = BootstrapError::new(
                BootstrapPhase::RegistryPublished,
                error.code().as_str(),
                error,
            );
            return Err(state.rollback(bootstrap_error));
        }
    };
    state.publication = Some(publication);
    if let Err(error) =
        checkpoint.reached(BootstrapPhase::RegistryPublished, &state.snapshot(run_id))
    {
        return Err(state.rollback(error));
    }
    if let Err(error) = checkpoint.reached(BootstrapPhase::ReadyResult, &state.snapshot(run_id)) {
        return Err(state.rollback(error));
    }

    Ok(DiagnosticRuntimeGuard {
        layout: state.layout.take().expect("archive layout was installed"),
        active_lease: state.active_lease.take(),
        ingress: state.ingress.take(),
        quota: state.quota.take(),
        hub: state.hub.take(),
        writer: state.writer.take(),
        server: state.server.take(),
        publication: state.publication.take(),
        final_stream_closer: state.final_stream_closer.take(),
        shutdown_attempted: false,
    })
}

struct BootstrapCommitObserver {
    ingress: MandatoryIngress,
    downstream: Box<dyn CommitObserver + Send>,
}

impl CommitObserver for BootstrapCommitObserver {
    fn committed(&mut self, notification: CommitNotification) {
        self.ingress.committed(notification);
        self.downstream.committed(notification);
    }
}

struct WriterSupervisor {
    ingress: MandatoryIngress,
    progress: Arc<Mutex<WriterProgressStatus>>,
    failure_receiver: Receiver<DiagnosticCoreFailure>,
    command_sender: Option<SyncSender<WriterCommand>>,
    thread: Option<JoinHandle<Result<(), DiagnosticCoreFailure>>>,
    thread_handle: Thread,
}

enum WriterCommand {
    Finalize {
        ended_at: String,
        outcome: FinalProductionOutcome,
        reply: SyncSender<Result<SortableU64Key, DiagnosticCoreFailure>>,
    },
    Abort,
    Close,
}

enum WriterDrainIntent {
    Finalize {
        ended_at: String,
        outcome: FinalProductionOutcome,
        reply: SyncSender<Result<SortableU64Key, DiagnosticCoreFailure>>,
    },
    Abort,
}

impl WriterSupervisor {
    #[allow(clippy::too_many_arguments)]
    fn start(
        store: DiagnosticStore,
        ingress: MandatoryIngress,
        ingress_failures: IngressFailureReceiver,
        quota: CoordinatedRunQuota,
        quota_failures: QuotaFailureReceiver,
        deadlines: WriterDeadlines,
        downstream: Box<dyn CommitObserver + Send>,
    ) -> Result<Self, BootstrapError> {
        let observer = BootstrapCommitObserver {
            ingress: ingress.clone(),
            downstream,
        };
        let writer = TransactionalWriter::new(store, observer).map_err(|error| {
            BootstrapError::new(
                BootstrapPhase::WriterSupervisorReady,
                error.code().as_str(),
                error,
            )
        })?;
        let initial_supervisor = WriterProgressSupervisor::new(deadlines);
        let progress = Arc::new(Mutex::new(initial_supervisor.status()));
        let thread_progress = Arc::clone(&progress);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let (failure_sender, failure_receiver) = mpsc::sync_channel(1);
        let (command_sender, command_receiver) = mpsc::sync_channel(1);
        let thread_ingress = ingress.clone();
        let thread = thread::Builder::new()
            .name("troupe-diagnostic-writer".to_owned())
            .spawn(move || {
                let _activity = WriterThreadActivity::started();
                let failure_ingress = thread_ingress.clone();
                let result = catch_unwind(AssertUnwindSafe(|| {
                    run_writer(
                        writer,
                        thread_ingress,
                        ingress_failures,
                        quota,
                        quota_failures,
                        initial_supervisor,
                        thread_progress,
                        command_receiver,
                        ready_sender,
                    )
                }))
                .unwrap_or_else(|_| {
                    Err(DiagnosticCoreFailure::new(
                        "writer",
                        "task_exit",
                        "writer_panicked",
                        "diagnostic writer task panicked",
                    ))
                });
                if let Err(failure) = &result {
                    let _ = failure_ingress.seal_for_external_core_failure();
                    let _ = failure_sender.try_send(failure.clone());
                }
                result
            })
            .map_err(|error| {
                BootstrapError::new(
                    BootstrapPhase::WriterSupervisorReady,
                    "bootstrap.writer_thread_spawn_failed",
                    error,
                )
            })?;
        let thread_handle = thread.thread().clone();
        if ready_receiver.recv().is_err() {
            let result = thread.join();
            let message = match result {
                Ok(Ok(())) => "diagnostic writer exited before readiness".to_owned(),
                Ok(Err(error)) => error.message().to_owned(),
                Err(_) => "diagnostic writer panicked before readiness".to_owned(),
            };
            return Err(BootstrapError::message(
                BootstrapPhase::WriterSupervisorReady,
                "bootstrap.writer_exited_before_ready",
                message,
            ));
        }
        Ok(Self {
            ingress,
            progress,
            failure_receiver,
            command_sender: Some(command_sender),
            thread: Some(thread),
            thread_handle,
        })
    }

    fn progress_status(&self) -> Result<WriterProgressStatus, DiagnosticCoreFailure> {
        self.progress.lock().map(|status| *status).map_err(|_| {
            DiagnosticCoreFailure::new(
                "writer",
                "progress",
                "writer_progress_state_poisoned",
                "diagnostic writer progress state is poisoned",
            )
        })
    }

    fn try_core_failure(&self) -> Option<DiagnosticCoreFailure> {
        self.failure_receiver.try_recv().ok()
    }

    fn finalize(
        &mut self,
        ended_at: &str,
        outcome: FinalProductionOutcome,
    ) -> Result<SortableU64Key, CleanupFailure> {
        let (reply, result) = mpsc::sync_channel(1);
        let command = WriterCommand::Finalize {
            ended_at: ended_at.to_owned(),
            outcome,
            reply,
        };
        self.command_sender
            .as_ref()
            .expect("a live writer supervisor owns its command sender")
            .send(command)
            .map_err(|_| {
                CleanupFailure::new(
                    "writer",
                    "writer_finalize_command_failed",
                    "diagnostic writer is not accepting finalization",
                )
            })?;
        self.thread_handle.unpark();
        match result.recv() {
            Ok(Ok(watermark)) => Ok(watermark),
            Ok(Err(error)) => Err(CleanupFailure::new("writer", error.code(), error.message())),
            Err(_) => Err(CleanupFailure::new(
                "writer",
                "writer_finalize_result_unavailable",
                "diagnostic writer exited without a finalization result",
            )),
        }
    }

    fn shutdown(mut self) -> Result<(), Vec<CleanupFailure>> {
        let failures = self.shutdown_inner();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures)
        }
    }

    fn close(mut self) -> Result<(), Vec<CleanupFailure>> {
        let failures = self.join_after(WriterCommand::Close, false);
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures)
        }
    }

    fn shutdown_inner(&mut self) -> Vec<CleanupFailure> {
        self.join_after(WriterCommand::Abort, true)
    }

    fn join_after(&mut self, command: WriterCommand, seal_ingress: bool) -> Vec<CleanupFailure> {
        let Some(thread) = self.thread.take() else {
            return Vec::new();
        };
        let mut failures = Vec::new();
        if seal_ingress && let Err(error) = self.ingress.seal_normal_ingress() {
            failures.push(CleanupFailure::new(
                "writer",
                "writer_ingress_seal_failed",
                error,
            ));
        }
        if let Some(sender) = self.command_sender.take() {
            let _ = sender.send(command);
        }
        self.thread_handle.unpark();
        match thread.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                failures.push(CleanupFailure::new("writer", error.code(), error.message()))
            }
            Err(_) => failures.push(CleanupFailure::new(
                "writer",
                "writer_join_panicked",
                "diagnostic writer thread panicked while joining",
            )),
        }
        failures
    }
}

impl Drop for WriterSupervisor {
    fn drop(&mut self) {
        let failures = self.shutdown_inner();
        report_drop_failures("diagnostic writer supervisor drop failed", &failures);
    }
}

struct WriterThreadActivity;

impl WriterThreadActivity {
    fn started() -> Self {
        #[cfg(test)]
        ACTIVE_WRITER_THREADS.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Self
    }
}

impl Drop for WriterThreadActivity {
    fn drop(&mut self) {
        #[cfg(test)]
        ACTIVE_WRITER_THREADS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

#[cfg(test)]
static ACTIVE_WRITER_THREADS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[allow(clippy::too_many_arguments)]
fn run_writer(
    mut writer: RuntimeWriter,
    ingress: MandatoryIngress,
    ingress_failures: IngressFailureReceiver,
    quota: CoordinatedRunQuota,
    quota_failures: QuotaFailureReceiver,
    mut supervisor: WriterProgressSupervisor,
    progress: Arc<Mutex<WriterProgressStatus>>,
    command_receiver: Receiver<WriterCommand>,
    ready_sender: SyncSender<()>,
) -> Result<(), DiagnosticCoreFailure> {
    let origin = Instant::now();
    let mut accumulator = BatchAccumulator::new();
    let mut drain_intent = None;
    observe_progress(&ingress, &mut supervisor, &progress, origin.elapsed())?;
    ready_sender.send(()).map_err(|_| {
        DiagnosticCoreFailure::new(
            "writer",
            "readiness",
            "writer_readiness_receiver_dropped",
            "diagnostic writer readiness receiver was dropped",
        )
    })?;

    loop {
        if let Some(failure) = ingress_failures.try_recv() {
            return Err(DiagnosticCoreFailure::new(
                "writer",
                "admission",
                failure.code(),
                format!("mandatory diagnostic ingress failed [{}]", failure.code()),
            ));
        }
        if let Some(failure) = quota_failures.try_recv() {
            return Err(quota_core_failure(&failure));
        }
        if drain_intent.is_none() {
            match command_receiver.try_recv() {
                Ok(command) => {
                    drain_intent = Some(match command {
                        WriterCommand::Finalize {
                            ended_at,
                            outcome,
                            reply,
                        } => WriterDrainIntent::Finalize {
                            ended_at,
                            outcome,
                            reply,
                        },
                        WriterCommand::Abort | WriterCommand::Close => WriterDrainIntent::Abort,
                    });
                    let now = origin.elapsed();
                    observe_progress(&ingress, &mut supervisor, &progress, now)?;
                    supervisor.begin_shutdown(now).map_err(|error| {
                        DiagnosticCoreFailure::new(
                            "writer",
                            "drain",
                            "writer_shutdown_begin_failed",
                            error.to_string(),
                        )
                    })?;
                    record_progress(&progress, supervisor.status())?;
                }
                Err(TryRecvError::Disconnected) => {
                    drain_intent = Some(WriterDrainIntent::Abort);
                    let now = origin.elapsed();
                    observe_progress(&ingress, &mut supervisor, &progress, now)?;
                    supervisor.begin_shutdown(now).map_err(|error| {
                        DiagnosticCoreFailure::new(
                            "writer",
                            "drain",
                            "writer_shutdown_begin_failed",
                            error.to_string(),
                        )
                    })?;
                    record_progress(&progress, supervisor.status())?;
                }
                Err(TryRecvError::Empty) => {}
            }
        }

        let mut worked = false;
        while let Some(event) = ingress.try_dequeue().map_err(|error| {
            DiagnosticCoreFailure::new(
                "writer",
                "admission",
                "writer_ingress_unavailable",
                error.to_string(),
            )
        })? {
            worked = true;
            if let Some(batch) = accumulator.push(event, origin.elapsed()).map_err(|error| {
                DiagnosticCoreFailure::new(
                    "writer",
                    "batch",
                    "writer_batch_invalid",
                    error.to_string(),
                )
            })? {
                commit_batch(&mut writer, &quota, batch)?;
            }
        }

        let triggered = if drain_intent.is_some() {
            accumulator.flush()
        } else {
            accumulator.poll(origin.elapsed())
        }
        .map_err(|error| {
            DiagnosticCoreFailure::new(
                "writer",
                "batch",
                "writer_batch_clock_failed",
                error.to_string(),
            )
        })?;
        if let Some(batch) = triggered {
            worked = true;
            commit_batch(&mut writer, &quota, batch)?;
        }

        let status = observe_progress(&ingress, &mut supervisor, &progress, origin.elapsed())?;
        if drain_intent.is_some()
            && status.accepted_uncommitted_events() == 0
            && accumulator.pending_event_count() == 0
        {
            match drain_intent
                .take()
                .expect("a completed drain has a shutdown intent")
            {
                WriterDrainIntent::Finalize {
                    ended_at,
                    outcome,
                    reply,
                } => {
                    let final_watermark = match writer.finalize_run(&ended_at, outcome) {
                        Ok(watermark) => watermark,
                        Err(error) => {
                            let failure = DiagnosticCoreFailure::writer(
                                error.to_string(),
                                error.code().as_str(),
                            );
                            let _ = reply.send(Err(failure.clone()));
                            return Err(failure);
                        }
                    };
                    if let Err(error) = quota.post_growth_measurement() {
                        let failure = DiagnosticCoreFailure::new(
                            "quota",
                            "finalize",
                            quota_error_code(&error),
                            error.to_string(),
                        );
                        let _ = reply.send(Err(failure.clone()));
                        return Err(failure);
                    }
                    reply.send(Ok(final_watermark)).map_err(|_| {
                        DiagnosticCoreFailure::new(
                            "writer",
                            "finalize",
                            "writer_finalize_reply_receiver_dropped",
                            "diagnostic writer finalization receiver was dropped",
                        )
                    })?;
                    wait_for_writer_close(&command_receiver)?;
                }
                WriterDrainIntent::Abort => {}
            }
            return checkpoint_and_close_writer(writer, &quota);
        }
        if !worked {
            thread::park_timeout(WRITER_POLL_INTERVAL);
        }
    }
}

fn wait_for_writer_close(
    command_receiver: &Receiver<WriterCommand>,
) -> Result<(), DiagnosticCoreFailure> {
    match command_receiver.recv() {
        Ok(WriterCommand::Close | WriterCommand::Abort) | Err(_) => Ok(()),
        Ok(WriterCommand::Finalize { reply, .. }) => {
            let failure = DiagnosticCoreFailure::new(
                "writer",
                "close",
                "writer_finalized_twice",
                "diagnostic writer received a second finalization command",
            );
            let _ = reply.send(Err(failure.clone()));
            Err(failure)
        }
    }
}

fn checkpoint_and_close_writer(
    writer: RuntimeWriter,
    quota: &CoordinatedRunQuota,
) -> Result<(), DiagnosticCoreFailure> {
    let store = writer.into_store();
    store
        .checkpoint_and_validate_files()
        .map_err(|error| DiagnosticCoreFailure::writer(error.to_string(), error.code().as_str()))?;
    quota.post_growth_measurement().map_err(|error| {
        DiagnosticCoreFailure::new(
            "quota",
            "checkpoint",
            quota_error_code(&error),
            error.to_string(),
        )
    })?;
    Ok(())
}

fn observe_progress(
    ingress: &MandatoryIngress,
    supervisor: &mut WriterProgressSupervisor,
    progress: &Mutex<WriterProgressStatus>,
    now: Duration,
) -> Result<troupe_diagnostics_runtime::store::admission::IngressStatus, DiagnosticCoreFailure> {
    let status = ingress.status().map_err(|error| {
        DiagnosticCoreFailure::new(
            "writer",
            "progress",
            "writer_progress_sample_unavailable",
            error.to_string(),
        )
    })?;
    if let Some(failure) = supervisor
        .observe(now, WriterProgressSample::from(status))
        .map_err(|error| {
            DiagnosticCoreFailure::new(
                "writer",
                "progress",
                "writer_progress_observation_failed",
                error.to_string(),
            )
        })?
    {
        return Err(DiagnosticCoreFailure::supervised(failure));
    }
    record_progress(progress, supervisor.status())?;
    Ok(status)
}

fn record_progress(
    progress: &Mutex<WriterProgressStatus>,
    status: WriterProgressStatus,
) -> Result<(), DiagnosticCoreFailure> {
    let mut current = progress.lock().map_err(|_| {
        DiagnosticCoreFailure::new(
            "writer",
            "progress",
            "writer_progress_state_poisoned",
            "diagnostic writer progress state is poisoned",
        )
    })?;
    *current = status;
    Ok(())
}

fn commit_batch(
    writer: &mut RuntimeWriter,
    quota: &CoordinatedRunQuota,
    triggered: TriggeredBatch,
) -> Result<(), DiagnosticCoreFailure> {
    let batch = triggered.into_batch();
    let conservative_growth = u64::try_from(batch.canonical_bytes()).unwrap_or(u64::MAX);
    quota.precheck(conservative_growth).map_err(|error| {
        DiagnosticCoreFailure::new(
            "quota",
            "precheck",
            quota_error_code(&error),
            error.to_string(),
        )
    })?;
    writer
        .commit_batch(&batch)
        .map_err(|error| DiagnosticCoreFailure::writer(error.to_string(), error.code().as_str()))?;
    quota.post_growth_measurement().map_err(|error| {
        DiagnosticCoreFailure::new(
            "quota",
            "commit",
            quota_error_code(&error),
            error.to_string(),
        )
    })?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cleanup_resources(
    publication: &mut Option<RegistryPublication>,
    server: &mut Option<DiagnosticServer>,
    hub: &mut Option<Arc<DurableDiagnosticHub>>,
    writer: &mut Option<WriterSupervisor>,
    ingress: &mut Option<MandatoryIngress>,
    quota: &mut Option<RunQuota>,
    store: Option<DiagnosticStore>,
    active_lease: &mut Option<Arc<ActiveArchiveLease>>,
) -> Vec<CleanupFailure> {
    let mut failures = Vec::new();
    if let Some(ingress) = ingress.as_ref()
        && let Err(error) = ingress.seal_normal_ingress()
    {
        failures.push(CleanupFailure::new(
            "writer",
            "writer_ingress_seal_failed",
            error,
        ));
    }
    if let Some(mut publication) = publication.take()
        && let Err(error) = publication.unpublish(ListenerState::Running)
    {
        failures.push(CleanupFailure::new(
            "registry",
            error.code().as_str(),
            error,
        ));
    }
    if let Some(server) = server.take()
        && let Err(error) = server.shutdown()
    {
        failures.push(CleanupFailure::new(
            "server",
            "server_shutdown_failed",
            error,
        ));
    }
    hub.take();
    if let Some(writer) = writer.take()
        && let Err(writer_failures) = writer.shutdown()
    {
        failures.extend(writer_failures);
    }
    drop(store);
    quota.take();
    ingress.take();
    active_lease.take();
    failures
}

fn report_drop_failures(context: &str, failures: &[CleanupFailure]) {
    if failures.is_empty() {
        return;
    }
    let details = failures
        .iter()
        .map(|failure| {
            format!(
                "{} [{}]: {}",
                failure.component(),
                failure.code(),
                failure.message()
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    if thread::panicking() {
        eprintln!("{context}: {details}");
    } else {
        panic!("{context}: {details}");
    }
}

fn quota_configuration_code(
    error: troupe_diagnostics_runtime::store::quota::QuotaConfigurationError,
) -> &'static str {
    use troupe_diagnostics_runtime::store::quota::QuotaConfigurationError;
    match error {
        QuotaConfigurationError::RunDirectoryNotAbsolute => "run_quota.invalid_run_directory",
        QuotaConfigurationError::LimitNotPositive => "run_quota.limit_not_positive",
    }
}

fn quota_error_code(error: &QuotaError) -> &'static str {
    match error {
        QuotaError::StatePoisoned => "run_quota_state_poisoned",
        QuotaError::Sealed(failure)
        | QuotaError::LimitReached(failure)
        | QuotaError::MeasurementFailed(failure) => failure.code().as_str(),
    }
}

fn quota_core_failure(failure: &QuotaFailure) -> DiagnosticCoreFailure {
    DiagnosticCoreFailure::new(
        "quota",
        "measurement",
        failure.code().as_str(),
        format!("diagnostic Run quota failed [{}]", failure.code().as_str()),
    )
}

fn server_start_error_code(code: ServerStartErrorCode) -> &'static str {
    match code {
        ServerStartErrorCode::InvalidConfiguration => "server.invalid_configuration",
        ServerStartErrorCode::InvalidRoutes => "server.invalid_routes",
        ServerStartErrorCode::BindFailed => "server.bind_failed",
        ServerStartErrorCode::ReadinessProbeFailed => "server.readiness_probe_failed",
        ServerStartErrorCode::ContextSpawnFailed => "server.context_spawn_failed",
        ServerStartErrorCode::ContextInitializationFailed => "server.context_initialization_failed",
        ServerStartErrorCode::ContextExitedBeforeReady => "server.context_exited_before_ready",
    }
}

fn server_core_error_code(
    code: troupe_diagnostics_runtime::server::error::ServerCoreFailureCode,
) -> &'static str {
    use troupe_diagnostics_runtime::server::error::ServerCoreFailureCode;
    match code {
        ServerCoreFailureCode::ListenerFailed => "server.listener_failed",
        ServerCoreFailureCode::ExecutionContextExited => "server.execution_context_exited",
        ServerCoreFailureCode::ExecutionContextPanicked => "server.execution_context_panicked",
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        net::TcpStream,
        sync::atomic::Ordering,
        time::Duration,
    };

    use troupe_diagnostics_core::{
        event::{CounterSampled, DiagnosticEvent, DiagnosticEventHeader, DiagnosticScope},
        hub::{
            AcceptedDiagnosticEvent, DeliveryFailure, DiagnosticEventCandidate, EventIdentity,
            HubAdmissionError,
        },
        kinds::CounterKind,
        scalar::SchemaU64,
        time::ElapsedNs,
    };
    use troupe_diagnostics_runtime::{
        archive::lease::{ArchiveLeaseErrorCode, SharedArchiveLease},
        store::connection::DiagnosticStore,
    };

    use super::*;

    const STARTED_AT: &str = "2026-08-14T09:30:00.123456789Z";
    const PHASES: [BootstrapPhase; 8] = [
        BootstrapPhase::RunIdentityAllocated,
        BootstrapPhase::ArchivePrepared,
        BootstrapPhase::ActiveLeaseAcquired,
        BootstrapPhase::InitialStoreReady,
        BootstrapPhase::WriterSupervisorReady,
        BootstrapPhase::ListenerReady,
        BootstrapPhase::RegistryPublished,
        BootstrapPhase::ReadyResult,
    ];
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    struct IgnoreLive;

    impl LiveEventNotifier for IgnoreLive {
        fn notify(&mut self, _event: AcceptedDiagnosticEvent) -> Result<(), DeliveryFailure> {
            Ok(())
        }
    }

    fn components() -> BootstrapComponents {
        BootstrapComponents::new(Box::new(IgnoreLive), Box::new(()), |_| Ok(Vec::new()))
    }

    fn counter_candidate(elapsed_ns: u64) -> impl DiagnosticEventCandidate {
        move |identity: EventIdentity| {
            let header = DiagnosticEventHeader::new(
                identity.run_id(),
                identity.sequence(),
                ElapsedNs::new(elapsed_ns),
                DiagnosticScope::new(None, None, None, None, None, None, None),
                Vec::new(),
            )
            .expect("construct test event header");
            DiagnosticEvent::CounterSampled(CounterSampled::new(
                header,
                CounterKind::CueActive,
                SchemaU64::new(1),
            ))
        }
    }

    fn config() -> BootstrapConfig {
        BootstrapConfig::new(STARTED_AT, "test-bootstrap-config").with_bind("127.0.0.1", 0)
    }

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("troupe-b00-{}", Uuid::new_v4()));
            fs::create_dir(&path).expect("create test Production root");
            Self(path.canonicalize().expect("canonical test Production root"))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            if self.0.exists() {
                fs::remove_dir_all(&self.0).expect("remove test Production root");
            }
        }
    }

    struct FaultCheckpoint {
        fail_after: BootstrapPhase,
        reached: Vec<BootstrapPhase>,
        snapshot: Option<BootstrapSnapshot>,
    }

    impl FaultCheckpoint {
        fn new(fail_after: BootstrapPhase) -> Self {
            Self {
                fail_after,
                reached: Vec::new(),
                snapshot: None,
            }
        }
    }

    impl BootstrapCheckpoint for FaultCheckpoint {
        fn reached(
            &mut self,
            phase: BootstrapPhase,
            snapshot: &BootstrapSnapshot,
        ) -> Result<(), BootstrapError> {
            self.reached.push(phase);
            self.snapshot = Some(snapshot.clone());
            if phase == self.fail_after {
                Err(BootstrapError::message(
                    phase,
                    "bootstrap.injected_failure",
                    "injected bootstrap phase failure",
                ))
            } else {
                Ok(())
            }
        }
    }

    fn assert_listener_stopped(address: Option<SocketAddr>) {
        if let Some(address) = address {
            assert!(
                TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_err(),
                "diagnostic listener remained reachable at {address}"
            );
        }
    }

    fn assert_registry_empty(root: &Path) {
        let instances = root.join(".troupe/diagnostics/instances");
        if !instances.exists() {
            return;
        }
        let entries = fs::read_dir(instances)
            .expect("read registry")
            .collect::<Result<Vec<_>, _>>()
            .expect("read registry entries");
        assert!(entries.is_empty(), "bootstrap left a registry entry");
    }

    #[test]
    fn bootstrap_failure_matrix_cleans_real_resources() {
        let _test_guard = test_lock();
        for (index, fail_after) in PHASES.into_iter().enumerate() {
            let root = TestRoot::new();
            let run_id = CanonicalUuid::new(Uuid::new_v4());
            let run_directory = root
                .path()
                .join(".troupe/diagnostics/runs")
                .join(run_id.to_string());
            let mut checkpoint = FaultCheckpoint::new(fail_after);
            let error =
                bootstrap_root(root.path(), run_id, config(), components(), &mut checkpoint)
                    .expect_err("inject bootstrap failure");

            assert_eq!(error.phase(), fail_after);
            assert_eq!(error.code(), "bootstrap.injected_failure");
            assert!(error.cleanup_failures().is_empty());
            assert_eq!(checkpoint.reached, PHASES[..=index]);
            let snapshot = checkpoint.snapshot.expect("capture failing phase state");
            assert_eq!(snapshot.run_id, run_id);
            if index == 0 {
                assert!(snapshot.run_directory.is_none());
            } else {
                assert_eq!(
                    snapshot.run_directory.as_deref(),
                    Some(run_directory.as_path())
                );
            }
            assert_eq!(snapshot.locator_path.is_some(), index >= 6);
            assert_listener_stopped(snapshot.connect_addr);
            assert_registry_empty(root.path());
            assert_eq!(ACTIVE_WRITER_THREADS.load(Ordering::Acquire), 0);

            if index >= 2 {
                let shared = SharedArchiveLease::acquire(&run_directory)
                    .expect("startup rollback released the real active lease");
                drop(shared);
            }
            if index >= 3 {
                let store = DiagnosticStore::open_validated(&run_directory, run_id)
                    .expect("startup archive remains structurally readable");
                assert!(!store.metadata().clean_shutdown());
            }
        }
    }

    #[test]
    fn successful_bootstrap_is_ready_and_explicit_shutdown_is_ordered() {
        let _test_guard = test_lock();
        let root = TestRoot::new();
        let run_id = CanonicalUuid::new(Uuid::new_v4());
        let run_directory = root
            .path()
            .join(".troupe/diagnostics/runs")
            .join(run_id.to_string());
        let mut checkpoint = NoopCheckpoint;
        let guard = bootstrap_root(root.path(), run_id, config(), components(), &mut checkpoint)
            .expect("bootstrap diagnostics");

        assert_eq!(guard.run_id(), run_id);
        assert!(guard.locator_path().is_file());
        assert_eq!(guard.server_identity().run_id(), run_id);
        assert_eq!(guard.writer_progress().unwrap().committed_sequence(), 0);
        assert_eq!(ACTIVE_WRITER_THREADS.load(Ordering::Acquire), 1);
        let address = guard.connect_addr();
        let error = SharedArchiveLease::acquire(&run_directory)
            .expect_err("active Runtime lease excludes archive readers");
        assert_eq!(error.code(), ArchiveLeaseErrorCode::Contended);
        let receipt = guard
            .hub()
            .admit(counter_candidate(1), None)
            .expect("admit through the coordinated durable path");
        assert_eq!(receipt.accepted().identity().sequence().get(), 1);

        guard.shutdown().expect("shutdown diagnostics");
        assert_listener_stopped(Some(address));
        assert_registry_empty(root.path());
        assert_eq!(ACTIVE_WRITER_THREADS.load(Ordering::Acquire), 0);
        drop(
            SharedArchiveLease::acquire(&run_directory)
                .expect("shutdown released the active archive lease"),
        );
        let store = DiagnosticStore::open_validated(&run_directory, run_id)
            .expect("shutdown retained a readable incomplete archive");
        assert!(!store.metadata().clean_shutdown());
        assert_eq!(store.metadata().committed_watermark().get(), 1);
    }

    #[test]
    fn ordered_clean_shutdown_drains_and_reopens_terminal_archive() {
        let _test_guard = test_lock();
        let root = TestRoot::new();
        let run_id = CanonicalUuid::new(Uuid::new_v4());
        let run_directory = root
            .path()
            .join(".troupe/diagnostics/runs")
            .join(run_id.to_string());
        let mut checkpoint = NoopCheckpoint;
        let guard = bootstrap_root(root.path(), run_id, config(), components(), &mut checkpoint)
            .expect("bootstrap diagnostics");
        let address = guard.connect_addr();
        guard
            .hub()
            .admit(counter_candidate(1), None)
            .expect("admit terminal test event");

        guard
            .shutdown_clean(ShutdownMetadata::new(
                "2026-08-16T09:31:00Z",
                FinalProductionOutcome::Completed,
            ))
            .expect("complete ordered shutdown");

        assert_listener_stopped(Some(address));
        assert_registry_empty(root.path());
        assert_eq!(ACTIVE_WRITER_THREADS.load(Ordering::Acquire), 0);
        let store = DiagnosticStore::open_validated(&run_directory, run_id)
            .expect("reopen clean terminal archive");
        assert!(store.metadata().clean_shutdown());
        assert_eq!(store.metadata().committed_watermark().get(), 1);
        let terminal = store
            .connection()
            .query_row(
                "SELECT ended_at, production_outcome FROM run_metadata WHERE singleton = 1",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .expect("read final metadata");
        assert_eq!(
            terminal,
            ("2026-08-16T09:31:00Z".to_owned(), "completed".to_owned(),)
        );
        drop(
            SharedArchiveLease::acquire(&run_directory)
                .expect("ordered shutdown released the active archive lease"),
        );
    }

    #[test]
    fn explicit_shutdown_reports_registry_failure_and_closes_other_resources() {
        let _test_guard = test_lock();
        let root = TestRoot::new();
        let run_id = CanonicalUuid::new(Uuid::new_v4());
        let run_directory = root
            .path()
            .join(".troupe/diagnostics/runs")
            .join(run_id.to_string());
        let mut checkpoint = NoopCheckpoint;
        let guard = bootstrap_root(root.path(), run_id, config(), components(), &mut checkpoint)
            .expect("bootstrap diagnostics");
        let address = guard.connect_addr();
        let locator = guard.locator_path().to_path_buf();
        let mut file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(locator)
            .expect("open registry entry");
        file.write_all(b"{}\n").expect("replace registry entry");
        file.sync_all().expect("sync replacement");

        let error = guard
            .shutdown()
            .expect_err("surface registry cleanup failure");
        assert!(
            error
                .failures()
                .iter()
                .any(|failure| failure.component() == "registry")
        );
        assert_listener_stopped(Some(address));
        assert_eq!(ACTIVE_WRITER_THREADS.load(Ordering::Acquire), 0);
        drop(
            SharedArchiveLease::acquire(&run_directory)
                .expect("failed unpublish does not retain the active lease"),
        );
    }

    #[test]
    #[should_panic(expected = "diagnostic Runtime guard drop failed")]
    fn implicit_drop_does_not_silently_ignore_shutdown_failure() {
        let _test_guard = test_lock();
        let root = TestRoot::new();
        let run_id = CanonicalUuid::new(Uuid::new_v4());
        let mut checkpoint = NoopCheckpoint;
        let guard = bootstrap_root(root.path(), run_id, config(), components(), &mut checkpoint)
            .expect("bootstrap diagnostics");
        fs::write(guard.locator_path(), b"{}\n").expect("replace registry entry");
        drop(guard);
    }

    #[test]
    fn real_write_probe_failure_stops_before_user_or_background_work() {
        let _test_guard = test_lock();
        let root = TestRoot::new();
        fs::write(root.path().join(".troupe"), b"not a directory")
            .expect("create invalid state root");
        let mut checkpoint = NoopCheckpoint;
        let error = bootstrap_root(
            root.path(),
            CanonicalUuid::new(Uuid::new_v4()),
            config(),
            components(),
            &mut checkpoint,
        )
        .expect_err("reject unwritable state-root shape");

        assert_eq!(error.phase(), BootstrapPhase::ArchivePrepared);
        assert_eq!(ACTIVE_WRITER_THREADS.load(Ordering::Acquire), 0);
    }

    #[test]
    fn quota_startup_failure_rolls_back_store_and_active_lease() {
        let _test_guard = test_lock();
        let root = TestRoot::new();
        let run_id = CanonicalUuid::new(Uuid::new_v4());
        let run_directory = root
            .path()
            .join(".troupe/diagnostics/runs")
            .join(run_id.to_string());
        let mut checkpoint = NoopCheckpoint;
        let error = bootstrap_root(
            root.path(),
            run_id,
            config().with_max_run_bytes(Some(1)),
            components(),
            &mut checkpoint,
        )
        .expect_err("reject an initial archive larger than its Run quota");

        assert_eq!(error.phase(), BootstrapPhase::WriterSupervisorReady);
        assert!(error.cleanup_failures().is_empty());
        assert_registry_empty(root.path());
        assert_eq!(ACTIVE_WRITER_THREADS.load(Ordering::Acquire), 0);
        drop(
            SharedArchiveLease::acquire(&run_directory)
                .expect("quota failure released the active archive lease"),
        );
        let store = DiagnosticStore::open_validated(&run_directory, run_id)
            .expect("quota failure retained a readable incomplete archive");
        assert!(!store.metadata().clean_shutdown());
    }

    #[test]
    fn durable_admission_prechecks_quota_and_seals_before_ingress_reservation() {
        let _test_guard = test_lock();
        let root = TestRoot::new();
        fs::write(root.path().join("seed"), b"x").expect("seed quota directory");
        let (measuring_quota, _) = RunQuota::new(root.path(), Some(u64::MAX)).unwrap();
        measuring_quota
            .post_growth_measurement(Duration::ZERO)
            .unwrap();
        let measured = measuring_quota
            .status()
            .unwrap()
            .current_measured_bytes()
            .expect("capture measured bytes");
        let limit = measured.checked_add(1).expect("test quota limit");
        let (quota, quota_failures) = RunQuota::new(root.path(), Some(limit)).unwrap();
        let quota = CoordinatedRunQuota::new(quota);
        quota.post_growth_measurement().unwrap();
        let (ingress, ingress_failures) = MandatoryIngress::new();
        let hub = ProductionDiagnosticHub::production(
            CanonicalUuid::new(Uuid::new_v4()),
            DurableAdmission::new(ingress.clone(), quota),
            Box::new(IgnoreLive),
        );

        assert!(matches!(
            hub.admit(counter_candidate(1), None),
            Err(HubAdmissionError::Reservation(
                DurableAdmissionError::Quota(_)
            ))
        ));
        assert!(ingress.status().unwrap().normal_ingress_sealed());
        assert_eq!(ingress.status().unwrap().accepted_uncommitted_events(), 0);
        assert!(quota_failures.try_recv().is_some());
        assert!(ingress_failures.try_recv().is_none());
    }
}
