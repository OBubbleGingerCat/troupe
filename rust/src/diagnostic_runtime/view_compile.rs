use std::{fmt, path::Path};

use pyo3::{
    prelude::*,
    types::{PyAny, PyBytes, PyBytesMethods, PyFunction, PyTuple, PyTupleMethods, PyType},
};
use troupe_diagnostics_core::id::CanonicalUuid;
use troupe_diagnostics_runtime::store::view_records::{
    CompiledViewSet, ViewManifest, persist_view_set,
};

use super::load_producer::ObservedProductionClass;

const DIAGNOSTIC_VIEWS_ATTRIBUTE: &str = "diagnostic_views";
const DIAGNOSTICS_MODULE: &str = "troupe.diagnostics";
const VIEW_CLASSES_ATTRIBUTE: &str = "_VIEW_CLASSES";
const VIEW_ENCODER_ATTRIBUTE: &str = "_view_to_json_bytes";
const BUILTIN_VIEW_CLASS_COUNT: usize = 4;

/// Owns the diagnostic runtime while Production startup is still fallible.
///
/// A user failure finalizer must durably record `outcome=failed` and
/// `clean_shutdown=true`, durably unpublish the registry entry, and attempt to
/// close every listener, reader, store, and lease. A core abort must attempt the
/// same resource cleanup without marking the archive clean. Implementations must
/// finish all cleanup attempts before returning an error.
pub(crate) trait ViewStartupLifecycle: Sized {
    type Error: fmt::Display;

    fn run_directory(&self) -> &Path;
    fn run_id(&self) -> CanonicalUuid;
    fn finalize_user_failure(self) -> Result<(), Self::Error>;
    fn abort_core_failure(self) -> Result<(), Self::Error>;
}

trait StaticProductionClass: Sized {
    fn inspect_static_attribute(&self, py: Python<'_>, name: &str) -> PyResult<Option<Py<PyAny>>>;

    fn rollback(self, py: Python<'_>) -> PyResult<()>;
}

impl StaticProductionClass for ObservedProductionClass {
    fn inspect_static_attribute(&self, py: Python<'_>, name: &str) -> PyResult<Option<Py<PyAny>>> {
        self.inspect_static_attribute(py, name)
    }

    fn rollback(self, py: Python<'_>) -> PyResult<()> {
        self.rollback(py)
    }
}

pub(crate) struct PreparedViewClass<C, L> {
    class: C,
    lifecycle: L,
    manifest: ViewManifest,
}

impl<C, L> fmt::Debug for PreparedViewClass<C, L> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedViewClass")
            .field("manifest", &self.manifest)
            .finish_non_exhaustive()
    }
}

impl<C, L> PreparedViewClass<C, L> {
    pub(crate) const fn manifest(&self) -> &ViewManifest {
        &self.manifest
    }

    pub(crate) fn into_parts(self) -> (C, L, ViewManifest) {
        (self.class, self.lifecycle, self.manifest)
    }
}

pub(crate) type PreparedProductionClass<L> = PreparedViewClass<ObservedProductionClass, L>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ViewStartupErrorKind {
    UserConfiguration,
    CoreFailure,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ViewStartupError {
    kind: ViewStartupErrorKind,
    code: &'static str,
    message: String,
}

impl ViewStartupError {
    fn user(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind: ViewStartupErrorKind::UserConfiguration,
            code,
            message: message.into(),
        }
    }

    fn core(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind: ViewStartupErrorKind::CoreFailure,
            code,
            message: message.into(),
        }
    }

    fn append(mut self, context: &str, detail: impl fmt::Display) -> Self {
        use fmt::Write as _;
        let _ = write!(self.message, "; {context}: {detail}");
        self
    }

    pub(crate) const fn kind(&self) -> ViewStartupErrorKind {
        self.kind
    }

    pub(crate) const fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for ViewStartupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "diagnostic view startup failed [{}]: {}",
            self.code, self.message
        )
    }
}

impl std::error::Error for ViewStartupError {}

/// Compiles and durably persists static views before the caller may construct
/// the Production. The returned value contains no Python view or callback.
pub(crate) fn prepare_production_views<L>(
    py: Python<'_>,
    class: ObservedProductionClass,
    lifecycle: L,
) -> Result<PreparedProductionClass<L>, ViewStartupError>
where
    L: ViewStartupLifecycle,
{
    prepare_views(py, class, lifecycle)
}

fn prepare_views<C, L>(
    py: Python<'_>,
    class: C,
    lifecycle: L,
) -> Result<PreparedViewClass<C, L>, ViewStartupError>
where
    C: StaticProductionClass,
    L: ViewStartupLifecycle,
{
    let run_directory = lifecycle.run_directory().to_path_buf();
    let run_id = lifecycle.run_id();
    let compiled = match compile_static_views(py, &class) {
        Ok(compiled) => compiled,
        Err(error) if error.kind() == ViewStartupErrorKind::UserConfiguration => {
            return Err(finalize_user_configuration_failure(
                py, class, lifecycle, error,
            ));
        }
        Err(error) => return Err(abort_core_failure(py, class, lifecycle, error)),
    };

    if let Err(error) = persist_view_set(&run_directory, run_id, &compiled) {
        let error = ViewStartupError::core(error.code().as_str(), error.to_string());
        return Err(abort_core_failure(py, class, lifecycle, error));
    }

    Ok(PreparedViewClass {
        class,
        lifecycle,
        manifest: compiled.manifest().clone(),
    })
}

fn compile_static_views<C>(py: Python<'_>, class: &C) -> Result<CompiledViewSet, ViewStartupError>
where
    C: StaticProductionClass,
{
    let Some(value) = class
        .inspect_static_attribute(py, DIAGNOSTIC_VIEWS_ATTRIBUTE)
        .map_err(|error| {
            ViewStartupError::core(
                "diagnostic_views.static_lookup_failed",
                format!("static class attribute lookup failed: {error}"),
            )
        })?
    else {
        return CompiledViewSet::from_json_records(std::iter::empty::<&[u8]>())
            .map_err(|error| ViewStartupError::core(error.code().as_str(), error.to_string()));
    };
    let value = value.bind(py);
    if !value.is_exact_instance_of::<PyTuple>() {
        return Err(ViewStartupError::user(
            "diagnostic_views.container_not_exact_tuple",
            "Production.diagnostic_views must be an exact tuple",
        ));
    }
    let tuple = value.cast::<PyTuple>().map_err(|error| {
        ViewStartupError::core(
            "diagnostic_views.container_cast_failed",
            format!("validated diagnostic view tuple could not be read: {error}"),
        )
    })?;
    if tuple.is_empty() {
        return CompiledViewSet::from_json_records(std::iter::empty::<&[u8]>())
            .map_err(|error| ViewStartupError::core(error.code().as_str(), error.to_string()));
    }

    let encoder = PythonViewEncoder::load(py)?;
    let mut records = Vec::with_capacity(tuple.len());
    for (ordinal, item) in tuple.iter().enumerate() {
        if !encoder.accepts_exact(&item) {
            return Err(ViewStartupError::user(
                "diagnostic_views.item_not_builtin_viewspec",
                format!(
                    "Production.diagnostic_views item {ordinal} is not an exact built-in ViewSpec"
                ),
            ));
        }
        records.push(encoder.encode(&item, ordinal)?);
    }

    CompiledViewSet::from_json_records(records)
        .map_err(|error| ViewStartupError::user(error.code().as_str(), error.to_string()))
}

struct PythonViewEncoder<'py> {
    classes: Bound<'py, PyTuple>,
    encode: Bound<'py, PyFunction>,
}

impl<'py> PythonViewEncoder<'py> {
    fn load(py: Python<'py>) -> Result<Self, ViewStartupError> {
        let module = py.import(DIAGNOSTICS_MODULE).map_err(|error| {
            ViewStartupError::core(
                "diagnostic_views.encoder_module_unavailable",
                format!("built-in diagnostic view module is unavailable: {error}"),
            )
        })?;
        let classes = module.getattr(VIEW_CLASSES_ATTRIBUTE).map_err(|error| {
            ViewStartupError::core(
                "diagnostic_views.encoder_contract_invalid",
                format!("built-in diagnostic view class registry is unavailable: {error}"),
            )
        })?;
        if !classes.is_exact_instance_of::<PyTuple>() {
            return Err(ViewStartupError::core(
                "diagnostic_views.encoder_contract_invalid",
                "built-in diagnostic view class registry is not an exact tuple",
            ));
        }
        let classes = classes.cast_into::<PyTuple>().map_err(|error| {
            ViewStartupError::core(
                "diagnostic_views.encoder_contract_invalid",
                format!("built-in diagnostic view class registry could not be read: {error}"),
            )
        })?;
        if classes.len() != BUILTIN_VIEW_CLASS_COUNT
            || classes.iter().any(|class| class.cast::<PyType>().is_err())
        {
            return Err(ViewStartupError::core(
                "diagnostic_views.encoder_contract_invalid",
                "built-in diagnostic view class registry has an invalid shape",
            ));
        }

        let encode = module.getattr(VIEW_ENCODER_ATTRIBUTE).map_err(|error| {
            ViewStartupError::core(
                "diagnostic_views.encoder_contract_invalid",
                format!("built-in diagnostic view encoder is unavailable: {error}"),
            )
        })?;
        if !encode.is_exact_instance_of::<PyFunction>() {
            return Err(ViewStartupError::core(
                "diagnostic_views.encoder_contract_invalid",
                "built-in diagnostic view encoder is not an exact Python function",
            ));
        }
        let encode = encode.cast_into::<PyFunction>().map_err(|error| {
            ViewStartupError::core(
                "diagnostic_views.encoder_contract_invalid",
                format!("built-in diagnostic view encoder could not be read: {error}"),
            )
        })?;
        Ok(Self { classes, encode })
    }

    fn accepts_exact(&self, item: &Bound<'_, PyAny>) -> bool {
        let item_type = item.get_type();
        self.classes
            .iter()
            .any(|class| item_type.as_any().is(&class))
    }

    fn encode(&self, item: &Bound<'_, PyAny>, ordinal: usize) -> Result<Vec<u8>, ViewStartupError> {
        let encoded = self.encode.call1((item,)).map_err(|error| {
            ViewStartupError::core(
                "diagnostic_views.encoder_failed",
                format!("built-in diagnostic view encoder failed at ordinal {ordinal}: {error}"),
            )
        })?;
        if !encoded.is_exact_instance_of::<PyBytes>() {
            return Err(ViewStartupError::core(
                "diagnostic_views.encoder_contract_invalid",
                format!("built-in diagnostic view encoder returned non-bytes at ordinal {ordinal}"),
            ));
        }
        encoded
            .cast_into::<PyBytes>()
            .map(|bytes| bytes.as_bytes().to_vec())
            .map_err(|error| {
                ViewStartupError::core(
                    "diagnostic_views.encoder_contract_invalid",
                    format!("built-in diagnostic view bytes could not be read: {error}"),
                )
            })
    }
}

fn finalize_user_configuration_failure<C, L>(
    py: Python<'_>,
    class: C,
    lifecycle: L,
    user_error: ViewStartupError,
) -> ViewStartupError
where
    C: StaticProductionClass,
    L: ViewStartupLifecycle,
{
    if let Err(rollback_error) = class.rollback(py) {
        let error = ViewStartupError::core(
            "diagnostic_views.import_rollback_failed",
            format!(
                "could not roll back Production import after user error [{}]: {rollback_error}",
                user_error.code()
            ),
        );
        return match lifecycle.abort_core_failure() {
            Ok(()) => error,
            Err(cleanup_error) => error.append("core cleanup also failed", cleanup_error),
        };
    }

    match lifecycle.finalize_user_failure() {
        Ok(()) => user_error,
        Err(error) => ViewStartupError::core(
            "diagnostic_views.user_failure_finalization_failed",
            format!(
                "user configuration failed [{}], then clean failure finalization failed: {error}",
                user_error.code()
            ),
        ),
    }
}

fn abort_core_failure<C, L>(
    py: Python<'_>,
    class: C,
    lifecycle: L,
    mut error: ViewStartupError,
) -> ViewStartupError
where
    C: StaticProductionClass,
    L: ViewStartupLifecycle,
{
    if let Err(rollback_error) = class.rollback(py) {
        error = error.append("Production import rollback failed", rollback_error);
    }
    if let Err(cleanup_error) = lifecycle.abort_core_failure() {
        error = error.append("core cleanup also failed", cleanup_error);
    }
    error
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
    };

    use pyo3::types::{PyDict, PyList, PyModule};
    use troupe_diagnostics_runtime::store::{
        connection::{DiagnosticStore, InitialStoreMetadata},
        view_records::{CompiledViewSet, persist_view_set},
    };

    use super::*;

    const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestRunDirectory(PathBuf);

    impl TestRunDirectory {
        fn new(label: &str) -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "troupe-b08-view-compile-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create test Run directory");
            drop(
                DiagnosticStore::create(
                    &path,
                    &InitialStoreMetadata::new(
                        run_id(),
                        "2026-08-16T00:00:00Z",
                        "configuration-sha256:b08",
                    ),
                )
                .expect("create diagnostic store"),
            );
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestRunDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn run_id() -> CanonicalUuid {
        CanonicalUuid::parse(RUN_ID).expect("canonical test Run UUID")
    }

    #[derive(Clone)]
    struct LifecycleLog(Arc<Mutex<Vec<&'static str>>>);

    impl LifecycleLog {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(Vec::new())))
        }

        fn push(&self, value: &'static str) {
            self.0.lock().expect("lock lifecycle log").push(value);
        }

        fn values(&self) -> Vec<&'static str> {
            self.0.lock().expect("lock lifecycle log").clone()
        }
    }

    struct TestLifecycle {
        directory: PathBuf,
        log: LifecycleLog,
        fail_finalize: bool,
        fail_abort: bool,
    }

    impl TestLifecycle {
        fn new(directory: &Path, log: LifecycleLog) -> Self {
            Self {
                directory: directory.to_path_buf(),
                log,
                fail_finalize: false,
                fail_abort: false,
            }
        }

        fn failing_finalize(mut self) -> Self {
            self.fail_finalize = true;
            self
        }
    }

    impl ViewStartupLifecycle for TestLifecycle {
        type Error = &'static str;

        fn run_directory(&self) -> &Path {
            &self.directory
        }

        fn run_id(&self) -> CanonicalUuid {
            run_id()
        }

        fn finalize_user_failure(self) -> Result<(), Self::Error> {
            self.log.push("finalize_user_failure");
            if self.fail_finalize {
                Err("forced finalization failure")
            } else {
                Ok(())
            }
        }

        fn abort_core_failure(self) -> Result<(), Self::Error> {
            self.log.push("abort_core_failure");
            if self.fail_abort {
                Err("forced abort failure")
            } else {
                Ok(())
            }
        }
    }

    struct TestClass {
        value: Option<Py<PyAny>>,
        log: LifecycleLog,
        fail_rollback: bool,
    }

    impl TestClass {
        fn new(value: Option<Py<PyAny>>, log: LifecycleLog) -> Self {
            Self {
                value,
                log,
                fail_rollback: false,
            }
        }

        fn construct(self) {
            self.log.push("construct");
        }
    }

    impl StaticProductionClass for TestClass {
        fn inspect_static_attribute(
            &self,
            py: Python<'_>,
            name: &str,
        ) -> PyResult<Option<Py<PyAny>>> {
            assert_eq!(name, DIAGNOSTIC_VIEWS_ATTRIBUTE);
            Ok(self.value.as_ref().map(|value| value.clone_ref(py)))
        }

        fn rollback(self, _py: Python<'_>) -> PyResult<()> {
            self.log.push("rollback");
            if self.fail_rollback {
                Err(pyo3::exceptions::PyRuntimeError::new_err(
                    "forced rollback failure",
                ))
            } else {
                Ok(())
            }
        }
    }

    fn install_diagnostics(py: Python<'_>) -> Bound<'_, PyModule> {
        let package = PyModule::new(py, "troupe").expect("create troupe package");
        package
            .add("__path__", PyList::empty(py))
            .expect("mark troupe as a package");
        let runtime = PyModule::new(py, "troupe._runtime").expect("create runtime module");
        crate::diagnostic_python::install(&runtime).expect("install diagnostic Python module");
        let modules = py
            .import("sys")
            .and_then(|module| module.getattr("modules"))
            .and_then(|modules| Ok(modules.cast_into::<PyDict>()?))
            .expect("read sys.modules");
        modules
            .set_item("troupe", &package)
            .expect("register troupe package");
        modules
            .set_item("troupe._runtime", &runtime)
            .expect("register runtime module");
        runtime
            .getattr("diagnostics")
            .expect("runtime exposes diagnostics")
            .cast_into::<PyModule>()
            .expect("diagnostics is a module")
    }

    fn remove_diagnostics(py: Python<'_>) {
        let modules = py
            .import("sys")
            .and_then(|module| module.getattr("modules"))
            .and_then(|modules| Ok(modules.cast_into::<PyDict>()?))
            .expect("read sys.modules");
        let _ = modules.del_item(DIAGNOSTICS_MODULE);
        let _ = modules.del_item("troupe._runtime");
        let _ = modules.del_item("troupe");
    }

    fn valid_views(module: &Bound<'_, PyModule>) -> Py<PyAny> {
        module
            .py()
            .eval(
                cr#"(
                    TimelineView(
                        id="timeline", title="Timeline", time_range="run", scope="run",
                        query=TimelineQuery(source=SpanSource(kind="cue.execution")),
                    ),
                    MetricView(
                        id="metric", title="Metric", time_range="run", scope="run",
                        query=MetricQuery(
                            source=ActTokenMetric(metric="input_tokens"), reducer="sum"
                        ),
                    ),
                    TableView(
                        id="table", title="Table", time_range="run", scope="run",
                        query=TableQuery(
                            source=EventRows(kind="agent_message_completed"),
                            columns=(TableColumn(column="sequence"),), page_size=100,
                        ),
                    ),
                    TimeSeriesView(
                        id="series", title="Series", time_range="viewport", scope="selection",
                        query=TimeSeriesQuery(
                            source=CounterValue(selector=CounterSource(name="example.depth")),
                            reducer="max",
                        ),
                    ),
                )"#,
                Some(&module.dict()),
                None,
            )
            .expect("construct all built-in view values")
            .unbind()
    }

    fn persisted_counts(directory: &Path) -> (i64, i64) {
        let store = DiagnosticStore::open_validated(directory, run_id()).expect("open store");
        store
            .connection()
            .query_row(
                "SELECT (SELECT count(*) FROM diagnostic_view_manifest), \
                        (SELECT count(*) FROM diagnostic_view_records)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read view persistence counts")
    }

    #[test]
    fn missing_attribute_is_an_empty_tuple_and_valid_views_persist_before_construction() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let module = install_diagnostics(py);

            let missing_directory = TestRunDirectory::new("missing");
            let missing_log = LifecycleLog::new();
            let missing = prepare_views(
                py,
                TestClass::new(None, missing_log.clone()),
                TestLifecycle::new(missing_directory.path(), missing_log.clone()),
            )
            .expect("missing diagnostic_views compiles as empty");
            assert!(missing.manifest().views().is_empty());
            assert_eq!(persisted_counts(missing_directory.path()), (1, 0));
            let (class, _lifecycle, manifest) = missing.into_parts();
            assert!(manifest.views().is_empty());
            class.construct();
            assert_eq!(missing_log.values(), ["construct"]);

            let valid_directory = TestRunDirectory::new("valid");
            let valid_log = LifecycleLog::new();
            let prepared = prepare_views(
                py,
                TestClass::new(Some(valid_views(&module)), valid_log.clone()),
                TestLifecycle::new(valid_directory.path(), valid_log.clone()),
            )
            .expect("compile all four built-in view types");
            assert_eq!(prepared.manifest().views().len(), 4);
            assert_eq!(persisted_counts(valid_directory.path()), (1, 4));
            assert!(valid_log.values().is_empty());
            let (class, _lifecycle, manifest) = prepared.into_parts();
            assert_eq!(
                manifest
                    .views()
                    .iter()
                    .map(|view| view.renderer().as_str())
                    .collect::<Vec<_>>(),
                ["timeline", "metric", "table", "time_series"]
            );
            class.construct();
            assert_eq!(valid_log.values(), ["construct"]);

            remove_diagnostics(py);
        });
    }

    #[test]
    fn rejects_non_tuple_lazy_tuple_subclass_and_descriptor_without_invoking_it() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let module = install_diagnostics(py);
            let namespace = module.dict();
            py.run(
                cr#"
_view = TimelineView(
    id="one", title="One", time_range="run", scope="run",
    query=TimelineQuery(source=SpanSource(kind="cue.execution")),
)
class _TupleSubclass(tuple):
    pass
_descriptor_calls = 0
def _descriptor_getter(_instance):
    global _descriptor_calls
    _descriptor_calls += 1
    return (_view,)
_descriptor = property(_descriptor_getter)
"#,
                Some(&namespace),
                Some(&namespace),
            )
            .expect("install invalid containers");

            for (label, expression) in [
                ("list", "[_view]"),
                ("generator", "(_item for _item in (_view,))"),
                ("subclass", "_TupleSubclass((_view,))"),
                ("descriptor", "_descriptor"),
            ] {
                let directory = TestRunDirectory::new(label);
                let log = LifecycleLog::new();
                let value = py
                    .eval(
                        &std::ffi::CString::new(expression).expect("valid expression"),
                        Some(&namespace),
                        None,
                    )
                    .expect("evaluate invalid container")
                    .unbind();
                let error = prepare_views(
                    py,
                    TestClass::new(Some(value), log.clone()),
                    TestLifecycle::new(directory.path(), log.clone()),
                )
                .expect_err("invalid container must prevent startup");
                assert_eq!(error.kind(), ViewStartupErrorKind::UserConfiguration);
                assert_eq!(error.code(), "diagnostic_views.container_not_exact_tuple");
                assert_eq!(log.values(), ["rollback", "finalize_user_failure"]);
                assert_eq!(persisted_counts(directory.path()), (0, 0));
            }
            assert_eq!(
                namespace
                    .get_item("_descriptor_calls")
                    .expect("read descriptor counter")
                    .expect("descriptor counter exists")
                    .extract::<usize>()
                    .expect("descriptor counter is an integer"),
                0
            );
            remove_diagnostics(py);
        });
    }

    #[test]
    fn invalid_or_duplicate_items_are_atomic_clean_user_failures() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let module = install_diagnostics(py);
            let namespace = module.dict();
            let valid = valid_views(&module);
            namespace
                .set_item("_valid_views", valid.bind(py))
                .expect("publish valid tuple");

            for (label, expression, code) in [
                (
                    "invalid-item",
                    "(_valid_views[0], object())",
                    "diagnostic_views.item_not_builtin_viewspec",
                ),
                (
                    "duplicate",
                    "(_valid_views[0], _valid_views[0])",
                    "diagnostic_views.duplicate_id",
                ),
            ] {
                let directory = TestRunDirectory::new(label);
                let log = LifecycleLog::new();
                let value = py
                    .eval(
                        &std::ffi::CString::new(expression).expect("valid expression"),
                        Some(&namespace),
                        None,
                    )
                    .expect("evaluate invalid declaration")
                    .unbind();
                let error = prepare_views(
                    py,
                    TestClass::new(Some(value), log.clone()),
                    TestLifecycle::new(directory.path(), log.clone()),
                )
                .expect_err("invalid declaration must prevent startup");
                assert_eq!(error.kind(), ViewStartupErrorKind::UserConfiguration);
                assert_eq!(error.code(), code);
                assert_eq!(log.values(), ["rollback", "finalize_user_failure"]);
                assert_eq!(persisted_counts(directory.path()), (0, 0));
            }
            remove_diagnostics(py);
        });
    }

    #[test]
    fn persistence_failure_is_an_incomplete_core_abort() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let module = install_diagnostics(py);
            let directory = TestRunDirectory::new("persistence-core");
            let already_persisted = CompiledViewSet::from_json_records(std::iter::empty::<&[u8]>())
                .expect("compile empty manifest");
            persist_view_set(directory.path(), run_id(), &already_persisted)
                .expect("seed one-shot view persistence");
            let log = LifecycleLog::new();

            let error = prepare_views(
                py,
                TestClass::new(Some(valid_views(&module)), log.clone()),
                TestLifecycle::new(directory.path(), log.clone()),
            )
            .expect_err("persistence failure must prevent startup");
            assert_eq!(error.kind(), ViewStartupErrorKind::CoreFailure);
            assert_eq!(error.code(), "diagnostic_views.already_persisted");
            assert_eq!(log.values(), ["rollback", "abort_core_failure"]);
            let store = DiagnosticStore::open_validated(directory.path(), run_id())
                .expect("read incomplete store");
            assert!(!store.metadata().clean_shutdown());
            remove_diagnostics(py);
        });
    }

    #[test]
    fn finalization_or_import_rollback_failure_upgrades_user_error_to_core() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let module = install_diagnostics(py);
            let invalid = module
                .py()
                .eval(c"(object(),)", Some(&module.dict()), None)
                .expect("make invalid tuple")
                .unbind();

            let finalization_directory = TestRunDirectory::new("finalization-core");
            let finalization_log = LifecycleLog::new();
            let finalization_error = prepare_views(
                py,
                TestClass::new(Some(invalid.clone_ref(py)), finalization_log.clone()),
                TestLifecycle::new(finalization_directory.path(), finalization_log.clone())
                    .failing_finalize(),
            )
            .expect_err("failed clean finalization becomes core failure");
            assert_eq!(finalization_error.kind(), ViewStartupErrorKind::CoreFailure);
            assert_eq!(
                finalization_error.code(),
                "diagnostic_views.user_failure_finalization_failed"
            );
            assert_eq!(
                finalization_log.values(),
                ["rollback", "finalize_user_failure"]
            );

            let rollback_directory = TestRunDirectory::new("rollback-core");
            let rollback_log = LifecycleLog::new();
            let mut class = TestClass::new(Some(invalid), rollback_log.clone());
            class.fail_rollback = true;
            let rollback_error = prepare_views(
                py,
                class,
                TestLifecycle::new(rollback_directory.path(), rollback_log.clone()),
            )
            .expect_err("failed import rollback becomes core failure");
            assert_eq!(rollback_error.kind(), ViewStartupErrorKind::CoreFailure);
            assert_eq!(
                rollback_error.code(),
                "diagnostic_views.import_rollback_failed"
            );
            assert_eq!(rollback_log.values(), ["rollback", "abort_core_failure"]);
            remove_diagnostics(py);
        });
    }
}
