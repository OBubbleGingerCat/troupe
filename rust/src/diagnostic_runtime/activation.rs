use std::{
    cell::RefCell,
    collections::HashMap,
    fmt,
    path::Path,
    sync::{Arc, Mutex, MutexGuard, OnceLock, Weak},
    time::Duration,
};

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyAnyMethods, PyList, PyString};
use serde::Serialize;
use tokio::io::AsyncWrite;
use troupe_diagnostics_core::{
    hub::{AcceptedDiagnosticEvent, DeliveryFailure, LiveEventNotifier},
    id::CanonicalUuid,
    scalar::SchemaU64,
};
use troupe_diagnostics_perfetto::{
    collect::{PERFETTO_EXPORTER_SCHEMA_VERSION, TRACE_CONTENT_WARNING},
    dump::dump_captured_prefix,
};
use troupe_diagnostics_runtime::{
    query::{reader::CapturedEventSource, views::ViewQueryEngine},
    registry::model::SecurityScope,
    server::{
        assembly::ActiveRouteAssembly,
        dump::{
            CapturedPrefixDumpProducer, DumpEndpoints, DumpProducerError, DumpProducerFuture,
            DumpProducerMetadata,
        },
        query::QueryEndpoints,
        sse::{
            replay::{ActiveReplaySource, ReplayDriverConfig, SseEndpoint},
            subscriber::{CommitSignal, SubscriberLimits},
        },
        views::ViewEndpoints,
    },
    store::{
        progress::WriterDeadlines,
        watermark::{CommitNotification, CommitObserver},
    },
};

use crate::{
    application::{diagnostic_cli::RuntimeDiagnosticArgs, loader::prevalidate_production_root},
    diagnostic_runtime::{
        bootstrap::{
            self, BootstrapComponents, BootstrapConfig, BootstrapRouteAssemblyError,
            BootstrapRouteContext, DiagnosticRuntimeGuard, DiagnosticShutdownError,
        },
        custom_binding,
        load_producer::{
            DiagnosticRunContext, LoadProducerError, ProductionLoadProducer,
            current_production_construction,
        },
        runtime_producer,
        view_compile::{ViewStartupLifecycle, prepare_production_views},
    },
    orchestration::{
        actor_registry::ProductionState, production::Production, runtime::RuntimeCore,
        scene_context::RunBinding,
    },
};

pub(crate) const READY_PREFIX: &str = "troupe: diagnostic ready ";
const STARTUP_FAILURE_PREFIX: &str = "troupe: diagnostic startup failed: ";
const LOCATOR_SCHEMA_VERSION: u8 = 1;
const SSE_MAX_BUFFERED_EVENTS: usize = 1_024;
const SSE_MAX_BUFFERED_BYTES: usize = 1024 * 1024;
const SSE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub(crate) enum ActivationError {
    Python(PyErr),
    Diagnostic { code: String, detail: String },
}

impl ActivationError {
    fn diagnostic(code: impl Into<String>, detail: impl fmt::Display) -> Self {
        Self::Diagnostic {
            code: code.into(),
            detail: detail.to_string(),
        }
    }

    fn with_cleanup(self, cleanup: impl fmt::Display) -> Self {
        match self {
            Self::Python(error) => Self::Python(error),
            Self::Diagnostic { code, detail } => Self::Diagnostic {
                code,
                detail: format!("{detail}; diagnostic cleanup also failed: {cleanup}"),
            },
        }
    }

    pub(crate) fn into_python(self) -> Result<PyErr, Self> {
        match self {
            Self::Python(error) => Ok(error),
            diagnostic => Err(diagnostic),
        }
    }

    pub(crate) fn line(&self) -> String {
        format!("{STARTUP_FAILURE_PREFIX}{self}\n")
    }
}

impl fmt::Display for ActivationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Python(error) => fmt::Display::fmt(error, formatter),
            Self::Diagnostic { code, detail } => write!(formatter, "{code}: {detail}"),
        }
    }
}

impl std::error::Error for ActivationError {}

#[derive(Serialize)]
struct RuntimeConfigurationIdentity<'config> {
    schema_version: u8,
    bind_host: &'config str,
    port: u16,
    advertise_url: Option<&'config str>,
    max_run_bytes: Option<u64>,
    writer_stall_timeout_ms: u64,
    shutdown_timeout_ms: u64,
}

#[derive(Serialize)]
struct ReadyLocator {
    locator_schema_version: u8,
    run_id: CanonicalUuid,
    local_url: String,
    advertise_url: Option<String>,
    archive_directory: String,
    security_scope: SecurityScope,
}

pub(crate) struct ActivatedProduction {
    production: Py<PyAny>,
    runtime: ActivatedRuntime,
}

impl ActivatedProduction {
    pub(crate) fn into_parts(self) -> (Py<PyAny>, ActivatedRuntime) {
        (self.production, self.runtime)
    }
}

pub(crate) struct ActivatedRuntime {
    guard: Option<DiagnosticRuntimeGuard>,
    run_id: CanonicalUuid,
}

impl ActivatedRuntime {
    fn new(guard: DiagnosticRuntimeGuard) -> Self {
        Self {
            run_id: guard.run_id(),
            guard: Some(guard),
        }
    }

    pub(crate) fn shutdown(mut self) -> Result<(), ActivationError> {
        remove_pending_run(self.run_id);
        self.guard
            .take()
            .expect("an activated Runtime owns its diagnostic guard")
            .shutdown()
            .map_err(|error| ActivationError::diagnostic("diagnostic_activation.shutdown", error))
    }

    fn guard(&self) -> &DiagnosticRuntimeGuard {
        self.guard
            .as_ref()
            .expect("an activated Runtime owns its diagnostic guard")
    }
}

struct StartupLifecycle {
    runtime: ActivatedRuntime,
}

impl StartupLifecycle {
    fn into_runtime(self) -> ActivatedRuntime {
        self.runtime
    }
}

impl ViewStartupLifecycle for StartupLifecycle {
    type Error = DiagnosticShutdownError;

    fn run_directory(&self) -> &Path {
        self.runtime.guard().layout().run_directory()
    }

    fn run_id(&self) -> CanonicalUuid {
        self.runtime.run_id
    }

    fn finalize_user_failure(mut self) -> Result<(), Self::Error> {
        remove_pending_run(self.runtime.run_id);
        self.runtime
            .guard
            .take()
            .expect("startup lifecycle owns its diagnostic guard")
            .shutdown()
    }

    fn abort_core_failure(mut self) -> Result<(), Self::Error> {
        remove_pending_run(self.runtime.run_id);
        self.runtime
            .guard
            .take()
            .expect("startup lifecycle owns its diagnostic guard")
            .shutdown()
    }
}

#[derive(Default)]
struct IgnoreLiveEvents;

impl LiveEventNotifier for IgnoreLiveEvents {
    fn notify(&mut self, _event: AcceptedDiagnosticEvent) -> Result<(), DeliveryFailure> {
        Ok(())
    }
}

#[derive(Clone, Default)]
struct DeferredCommitObserver {
    state: Arc<Mutex<DeferredCommitState>>,
}

#[derive(Default)]
struct DeferredCommitState {
    signal: Option<CommitSignal>,
    pending: Vec<CommitNotification>,
}

impl DeferredCommitObserver {
    fn bind(&self, run_id: CanonicalUuid) -> Result<CommitSignal, &'static str> {
        let mut state = lock(&self.state);
        if state.signal.is_some() {
            return Err("commit observer was bound more than once");
        }
        let mut signal = CommitSignal::new(run_id, SchemaU64::new(0));
        for notification in state.pending.drain(..) {
            CommitObserver::committed(&mut signal, notification);
        }
        state.signal = Some(signal.clone());
        Ok(signal)
    }
}

impl CommitObserver for DeferredCommitObserver {
    fn committed(&mut self, notification: CommitNotification) {
        let mut state = lock(&self.state);
        match state.signal.as_mut() {
            Some(signal) => CommitObserver::committed(signal, notification),
            None => state.pending.push(notification),
        }
    }
}

struct RuntimePerfettoDumpProducer {
    metadata: DumpProducerMetadata,
}

impl RuntimePerfettoDumpProducer {
    fn new() -> Self {
        Self {
            metadata: DumpProducerMetadata::new(
                PERFETTO_EXPORTER_SCHEMA_VERSION,
                env!("CARGO_PKG_VERSION"),
                TRACE_CONTENT_WARNING,
            )
            .expect("built-in Perfetto dump metadata is valid"),
        }
    }
}

impl CapturedPrefixDumpProducer for RuntimePerfettoDumpProducer {
    fn metadata(&self) -> &DumpProducerMetadata {
        &self.metadata
    }

    fn dump<'operation>(
        &'operation self,
        source: &'operation CapturedEventSource<'_>,
        writer: &'operation mut (dyn AsyncWrite + Unpin),
        through: Option<SchemaU64>,
    ) -> DumpProducerFuture<'operation> {
        Box::pin(async move {
            dump_captured_prefix(source, writer, through)
                .await
                .map(|_summary| ())
                .map_err(|error| DumpProducerError::new("perfetto_dump_failed", error.to_string()))
        })
    }
}

fn active_routes(
    context: BootstrapRouteContext,
    commits: &DeferredCommitObserver,
) -> Result<
    Vec<troupe_diagnostics_runtime::server::routes::RouteDefinition>,
    BootstrapRouteAssemblyError,
> {
    let run_id = context.run_id();
    let lease = Arc::clone(context.active_lease());
    let commit_signal = commits
        .bind(run_id)
        .map_err(BootstrapRouteAssemblyError::new)?;
    let subscriber_limits = SubscriberLimits::new(SSE_MAX_BUFFERED_EVENTS, SSE_MAX_BUFFERED_BYTES)
        .map_err(route_assembly_error)?;
    let driver_config =
        ReplayDriverConfig::new(SSE_HEARTBEAT_INTERVAL).map_err(route_assembly_error)?;
    let assembly = ActiveRouteAssembly::new(
        QueryEndpoints::active_unobserved(run_id, Arc::clone(&lease), |_failure| {}),
        SseEndpoint::active(
            ActiveReplaySource::new(run_id, Arc::clone(&lease)),
            commit_signal,
            subscriber_limits,
            driver_config,
            |_failure| {},
        )
        .map_err(route_assembly_error)?,
        ViewEndpoints::active(
            run_id,
            Arc::clone(&lease),
            ViewQueryEngine::default(),
            |_failure| {},
        ),
        DumpEndpoints::active(run_id, lease, RuntimePerfettoDumpProducer::new()),
    )
    .map_err(route_assembly_error)?;
    assembly.route_definitions().map_err(route_assembly_error)
}

fn route_assembly_error(error: impl fmt::Display) -> BootstrapRouteAssemblyError {
    BootstrapRouteAssemblyError::new(error.to_string())
}

fn bootstrap_components() -> BootstrapComponents {
    let commits = DeferredCommitObserver::default();
    let route_commits = commits.clone();
    BootstrapComponents::new(
        Box::new(IgnoreLiveEvents),
        Box::new(commits),
        move |context| active_routes(context, &route_commits),
    )
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).expect("CLI duration milliseconds originate in u64")
}

fn configuration_identity(arguments: &RuntimeDiagnosticArgs) -> Result<String, ActivationError> {
    let identity = RuntimeConfigurationIdentity {
        schema_version: 1,
        bind_host: arguments.bind_host.as_str(),
        port: arguments.port.get(),
        advertise_url: arguments.advertise_url.as_ref().map(|url| url.as_str()),
        max_run_bytes: arguments.max_run_bytes.map(|size| size.bytes()),
        writer_stall_timeout_ms: duration_millis(arguments.writer_stall_timeout.get()),
        shutdown_timeout_ms: duration_millis(arguments.shutdown_timeout.get()),
    };
    serde_json::to_string(&identity)
        .map(|encoded| format!("configuration-v1:{encoded}"))
        .map_err(|error| {
            ActivationError::diagnostic("diagnostic_activation.configuration_encode", error)
        })
}

fn started_at(py: Python<'_>) -> Result<String, ActivationError> {
    let time = py.import("time").map_err(ActivationError::Python)?;
    let utc = time
        .call_method0("gmtime")
        .map_err(ActivationError::Python)?;
    time.call_method1("strftime", ("%Y-%m-%dT%H:%M:%SZ", utc))
        .and_then(|value| value.extract::<String>())
        .map_err(ActivationError::Python)
}

fn bootstrap_config(
    py: Python<'_>,
    arguments: RuntimeDiagnosticArgs,
) -> Result<BootstrapConfig, ActivationError> {
    let identity = configuration_identity(&arguments)?;
    let started_at = started_at(py)?;
    let deadlines = WriterDeadlines::new(
        arguments.writer_stall_timeout.get(),
        arguments.shutdown_timeout.get(),
    )
    .map_err(|error| ActivationError::diagnostic("diagnostic_activation.deadlines", error))?;
    Ok(BootstrapConfig::new(started_at, identity)
        .with_bind(arguments.bind_host.as_str(), arguments.port.get())
        .with_advertise_url(arguments.advertise_url.map(|url| url.into_inner()))
        .with_max_run_bytes(arguments.max_run_bytes.map(|size| size.bytes()))
        .with_writer_deadlines(deadlines))
}

fn ready_line(runtime: &DiagnosticRuntimeGuard) -> Result<String, ActivationError> {
    let archive_directory = runtime
        .layout()
        .run_directory()
        .to_str()
        .ok_or_else(|| {
            ActivationError::diagnostic(
                "diagnostic_activation.archive_path",
                "archive directory is not valid UTF-8 and cannot be represented in the locator",
            )
        })?
        .to_owned();
    let identity = runtime.server_identity();
    let locator = ReadyLocator {
        locator_schema_version: LOCATOR_SCHEMA_VERSION,
        run_id: runtime.run_id(),
        local_url: identity.local_endpoint().as_str().to_owned(),
        advertise_url: identity.advertise_url().map(|url| url.as_str().to_owned()),
        archive_directory,
        security_scope: identity.security_scope(),
    };
    serde_json::to_string(&locator)
        .map(|encoded| format!("{READY_PREFIX}{encoded}\n"))
        .map_err(|error| ActivationError::diagnostic("diagnostic_activation.locator_encode", error))
}

fn write_ready(py: Python<'_>, runtime: &DiagnosticRuntimeGuard) -> Result<(), ActivationError> {
    let line = ready_line(runtime)?;
    let stderr = py
        .import("sys")
        .and_then(|sys| sys.getattr("stderr"))
        .map_err(ActivationError::Python)?;
    stderr
        .call_method1("write", (line,))
        .and_then(|_| stderr.call_method0("flush"))
        .map(|_| ())
        .map_err(ActivationError::Python)
}

fn load_error(error: LoadProducerError) -> ActivationError {
    match error {
        LoadProducerError::Python { error, .. } => ActivationError::Python(error),
        LoadProducerError::Diagnostic {
            error,
            cleanup_error,
        } => {
            let code = error.code().to_owned();
            let error = ActivationError::diagnostic(code, error);
            match cleanup_error {
                Some(cleanup) => error.with_cleanup(cleanup),
                None => error,
            }
        }
    }
}

fn shutdown_after_error(runtime: ActivatedRuntime, error: ActivationError) -> ActivationError {
    match runtime.shutdown() {
        Ok(()) => error,
        Err(cleanup) => error.with_cleanup(cleanup),
    }
}

pub(crate) fn activate(
    py: Python<'_>,
    package_dir: &Bound<'_, PyString>,
    production_args: &Bound<'_, PyList>,
    arguments: RuntimeDiagnosticArgs,
) -> Result<ActivatedProduction, ActivationError> {
    let root = prevalidate_production_root(py, package_dir).map_err(ActivationError::Python)?;
    let config = bootstrap_config(py, arguments)?;
    let guard =
        bootstrap::bootstrap(py, &root, config, bootstrap_components()).map_err(|error| {
            let code = error.code().to_owned();
            ActivationError::diagnostic(code, error)
        })?;
    let runtime = ActivatedRuntime::new(guard);
    let loader = match ProductionLoadProducer::new(runtime.guard()) {
        Ok(loader) => loader,
        Err(error) => {
            let code = error.code().to_owned();
            return Err(shutdown_after_error(
                runtime,
                ActivationError::diagnostic(code, error),
            ));
        }
    };
    if let Err(error) = write_ready(py, runtime.guard()) {
        return Err(shutdown_after_error(runtime, error));
    }

    let path = match loader.resolve_path(py, root) {
        Ok(path) => path,
        Err(error) => return Err(shutdown_after_error(runtime, load_error(error))),
    };
    let class = match loader.resolve_class(py, path) {
        Ok(class) => class,
        Err(error) => return Err(shutdown_after_error(runtime, load_error(error))),
    };
    let prepared = prepare_production_views(py, class, StartupLifecycle { runtime })
        .map_err(|error| ActivationError::diagnostic(error.code(), error))?;
    let (class, lifecycle, _manifest) = prepared.into_parts();
    let runtime = lifecycle.into_runtime();
    let construction = ActivationConstructionGuard::enter(runtime.run_id);
    let production = match loader.construct(py, class, production_args) {
        Ok(production) => production,
        Err(error) => return Err(shutdown_after_error(runtime, load_error(error))),
    };
    drop(construction);
    let pending_matches = match pending_production_matches(py, &production, runtime.run_id) {
        Ok(matches) => matches,
        Err(error) => return Err(shutdown_after_error(runtime, error)),
    };
    if !pending_matches {
        return Err(shutdown_after_error(
            runtime,
            ActivationError::diagnostic(
                "diagnostic_activation.production_context_missing",
                "Production construction did not retain its mandatory diagnostic context",
            ),
        ));
    }
    Ok(ActivatedProduction {
        production,
        runtime,
    })
}

thread_local! {
    static ACTIVATION_CONSTRUCTION_STACK: RefCell<Vec<CanonicalUuid>> = const { RefCell::new(Vec::new()) };
}

struct ActivationConstructionGuard(CanonicalUuid);

impl ActivationConstructionGuard {
    fn enter(run_id: CanonicalUuid) -> Self {
        ACTIVATION_CONSTRUCTION_STACK.with(|stack| stack.borrow_mut().push(run_id));
        Self(run_id)
    }
}

impl Drop for ActivationConstructionGuard {
    fn drop(&mut self) {
        ACTIVATION_CONSTRUCTION_STACK.with(|stack| {
            let popped = stack
                .borrow_mut()
                .pop()
                .expect("activation construction context must remain installed");
            assert_eq!(
                popped, self.0,
                "activation contexts must leave in LIFO order"
            );
        });
    }
}

struct PendingProduction {
    production: Weak<ProductionState>,
    run_id: CanonicalUuid,
    context: DiagnosticRunContext,
}

fn pending_productions() -> &'static Mutex<HashMap<usize, PendingProduction>> {
    static PENDING: OnceLock<Mutex<HashMap<usize, PendingProduction>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

fn production_key(production: &ProductionState) -> usize {
    std::ptr::from_ref(production).addr()
}

fn remove_pending_run(run_id: CanonicalUuid) {
    lock(pending_productions())
        .retain(|_, pending| pending.run_id != run_id && pending.production.upgrade().is_some());
}

fn pending_production_matches(
    py: Python<'_>,
    production: &Py<PyAny>,
    run_id: CanonicalUuid,
) -> Result<bool, ActivationError> {
    let state = production
        .bind(py)
        .cast::<Production>()
        .map_err(|error| ActivationError::Python(error.into()))?
        .borrow()
        .state();
    let pending = lock(pending_productions());
    Ok(pending
        .get(&production_key(&state))
        .is_some_and(|pending| pending.run_id == run_id))
}

#[inline]
pub(crate) fn production_created(
    _py: Python<'_>,
    production: &Arc<ProductionState>,
) -> PyResult<()> {
    let Some(construction) = current_production_construction() else {
        return Ok(());
    };
    let Some(run_id) = ACTIVATION_CONSTRUCTION_STACK.with(|stack| stack.borrow().last().copied())
    else {
        return Ok(());
    };
    let key = production_key(production);
    let mut pending = lock(pending_productions());
    pending.retain(|_, value| value.production.upgrade().is_some());
    if pending.contains_key(&key) {
        return Err(PyRuntimeError::new_err(
            "Production diagnostics were initialized more than once",
        ));
    }
    pending.insert(
        key,
        PendingProduction {
            production: Arc::downgrade(production),
            run_id,
            context: construction.context(),
        },
    );
    Ok(())
}

#[inline]
pub(crate) fn bind_run(
    py: Python<'_>,
    core: &RuntimeCore,
    production: &ProductionState,
    binding: &Arc<RunBinding>,
) -> PyResult<()> {
    let pending = {
        let mut productions = lock(pending_productions());
        let Some(pending) = productions.remove(&production_key(production)) else {
            return Ok(());
        };
        if pending
            .production
            .upgrade()
            .is_none_or(|owner| !std::ptr::eq(owner.as_ref(), production))
        {
            return Ok(());
        }
        pending
    };
    let _producer = runtime_producer::install(core, binding, pending.context.clone())
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;

    #[cfg(not(test))]
    {
        use crate::diagnostic_runtime::{
            act_producer::ActDiagnosticFailureOwner, hooks::DiagnosticAdmissionCapability,
            sink_binding,
        };

        let failure_owner: Arc<dyn ActDiagnosticFailureOwner> = _producer.clone();
        let capability =
            sink_binding::production_capability(pending.run_id, pending.context, failure_owner)
                .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
        let capability: Arc<dyn DiagnosticAdmissionCapability> = capability;
        binding
            .diagnostic_admission()
            .install(capability)
            .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
    }

    custom_binding::bind_run(py, binding)?;
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use clap::Parser;
    use pyo3::types::{PyDict, PyDictMethods};
    use serde_json::Value;
    use troupe_diagnostics_runtime::store::connection::DiagnosticStore;
    use uuid::Uuid;

    use crate::{
        application::diagnostic_cli::{TroupeInvocation, args::TroupeArgs},
        orchestration::production::Production,
    };

    use super::*;

    const BASE_NAME: &str = "_troupe_x00_base";
    const CALLS_NAME: &str = "_troupe_x00_calls";

    struct TestPackage {
        parent: PathBuf,
        root: PathBuf,
        name: String,
    }

    impl TestPackage {
        fn new() -> Self {
            let name = format!("troupe_x00_{}", Uuid::new_v4().simple());
            let parent = std::env::temp_dir().join(format!("troupe-x00-{}", Uuid::new_v4()));
            let root = parent.join(&name);
            fs::create_dir_all(&root).expect("create test Production package");
            fs::write(root.join("__init__.py"), b"").expect("write test package init");
            fs::write(
                root.join("production.py"),
                format!(
                    r#"import builtins
import sys
builtins.{CALLS_NAME}.append('import|' + sys.stderr.getvalue())
class Production(builtins.{BASE_NAME}):
    def __init__(self, args):
        builtins.{CALLS_NAME}.append(
            'construct|' + repr(list(args)) + '|' + sys.stderr.getvalue()
        )
"#
                ),
            )
            .expect("write test Production module");
            Self {
                parent,
                root: root.canonicalize().expect("canonicalize test package"),
                name,
            }
        }

        fn root(&self) -> &Path {
            &self.root
        }
    }

    impl Drop for TestPackage {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.parent);
        }
    }

    struct PythonStreams {
        stdout: Py<PyAny>,
        stderr: Py<PyAny>,
        captured_stdout: Py<PyAny>,
        captured_stderr: Py<PyAny>,
        calls: Py<PyList>,
        package: String,
    }

    impl PythonStreams {
        fn install(py: Python<'_>, package: &TestPackage) -> PyResult<Self> {
            let sys = py.import("sys")?;
            let io = py.import("io")?;
            let captured_stdout = io.getattr("StringIO")?.call0()?;
            let captured_stderr = io.getattr("StringIO")?.call0()?;
            let stdout = sys.getattr("stdout")?.unbind();
            let stderr = sys.getattr("stderr")?.unbind();
            sys.setattr("stdout", &captured_stdout)?;
            sys.setattr("stderr", &captured_stderr)?;

            let calls = PyList::empty(py);
            let builtins = py.import("builtins")?;
            builtins.setattr(BASE_NAME, py.get_type::<Production>())?;
            builtins.setattr(CALLS_NAME, &calls)?;
            Ok(Self {
                stdout,
                stderr,
                captured_stdout: captured_stdout.unbind(),
                captured_stderr: captured_stderr.unbind(),
                calls: calls.unbind(),
                package: package.name.clone(),
            })
        }
    }

    impl Drop for PythonStreams {
        fn drop(&mut self) {
            Python::attach(|py| {
                if let Ok(sys) = py.import("sys") {
                    let _ = sys.setattr("stdout", self.stdout.bind(py));
                    let _ = sys.setattr("stderr", self.stderr.bind(py));
                    if let Ok(modules) = sys.getattr("modules")
                        && let Ok(modules) = modules.cast_into::<PyDict>()
                    {
                        let _ = modules.del_item(format!("{}.production", self.package));
                        let _ = modules.del_item(&self.package);
                    }
                }
                if let Ok(builtins) = py.import("builtins") {
                    let _ = builtins.delattr(BASE_NAME);
                    let _ = builtins.delattr(CALLS_NAME);
                }
            });
        }
    }

    fn runtime_arguments(package: &TestPackage) -> RuntimeDiagnosticArgs {
        let parsed = TroupeArgs::try_parse_from([
            "troupe",
            "--production",
            package.root().to_str().expect("test path is UTF-8"),
            "--diagnostic-bind-host",
            "127.0.0.1",
        ])
        .expect("parse test Runtime arguments")
        .into_invocation();
        let TroupeInvocation::Production(arguments) = parsed else {
            panic!("test arguments must select a Production run")
        };
        arguments.diagnostics
    }

    #[test]
    fn real_activation_is_ready_before_import_and_persists_construction_events() {
        let _python_test_guard = crate::initialize_python_for_test();
        let package = TestPackage::new();
        let arguments = runtime_arguments(&package);

        let (ready, stdout, calls, run_id, archive_directory) = Python::attach(|py| {
            let streams = PythonStreams::install(py, &package)?;
            let package_dir =
                PyString::new(py, package.root().to_str().expect("test path is UTF-8"));
            let production_args = PyList::new(py, ["--alpha", "beta"])?;
            let activated = activate(py, &package_dir, &production_args, arguments)
                .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
            let ready = streams
                .captured_stderr
                .bind(py)
                .call_method0("getvalue")?
                .extract::<String>()?;
            let stdout = streams
                .captured_stdout
                .bind(py)
                .call_method0("getvalue")?
                .extract::<String>()?;
            let calls = streams.calls.bind(py).extract::<Vec<String>>()?;
            let (production, runtime) = activated.into_parts();
            let run_id = runtime.run_id;
            let archive_directory = runtime.guard().layout().run_directory().to_path_buf();
            drop(production);
            runtime
                .shutdown()
                .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
            drop(streams);
            Ok::<_, PyErr>((ready, stdout, calls, run_id, archive_directory))
        })
        .expect("activate and shut down real diagnostics");

        assert!(stdout.is_empty());
        assert_eq!(ready.lines().count(), 1);
        assert!(ready.ends_with('\n'));
        let locator: Value = serde_json::from_str(
            ready
                .strip_prefix(READY_PREFIX)
                .expect("ready line uses the stable prefix")
                .trim_end(),
        )
        .expect("ready line carries JSON");
        assert_eq!(locator["locator_schema_version"], 1);
        assert_eq!(locator["run_id"], run_id.to_string());
        assert_eq!(locator["advertise_url"], Value::Null);
        assert_eq!(locator["security_scope"], "trusted_network");
        assert!(
            locator["local_url"]
                .as_str()
                .is_some_and(|url| url.starts_with("http://127.0.0.1:"))
        );
        assert_eq!(
            locator["archive_directory"].as_str(),
            archive_directory.to_str()
        );
        assert!(archive_directory.is_absolute());
        assert_eq!(
            calls,
            [
                format!("import|{ready}"),
                format!("construct|['--alpha', 'beta']|{ready}"),
            ]
        );

        let store = DiagnosticStore::open_validated(&archive_directory, run_id)
            .expect("reopen the persisted startup archive");
        assert!(store.metadata().committed_watermark().get() >= 6);
    }
}
