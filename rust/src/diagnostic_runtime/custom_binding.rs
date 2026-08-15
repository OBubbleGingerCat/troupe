use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    sync::{Arc, Mutex, MutexGuard, OnceLock, Weak},
};

use pyo3::{
    exceptions::{PyRuntimeError, PyTypeError},
    prelude::*,
    types::{
        PyAny, PyAnyMethods, PyDict, PyDictMethods, PyList, PyListMethods, PyModule, PyType,
        PyWeakrefMethods, PyWeakrefReference,
    },
};
use troupe_diagnostics_core::{
    detail::{
        CanonicalInteger, CustomNumber, DiagnosticAttributeValue, DiagnosticAttributes,
        DiagnosticDimension, DiagnosticDimensions, DiagnosticScalar,
    },
    event::DiagnosticScope,
    kinds::{CustomSeverity, SpanOutcome},
    scalar::{DecimalString, SchemaU64},
};

use crate::{
    diagnostic_runtime::{
        load_producer::DiagnosticRunContext, runtime_producer,
        runtime_producer::RuntimeLifecycleProducer, scene_producer,
    },
    orchestration::{python_task::TaskLineage, scene_context::RunBinding},
};

#[cfg(not(test))]
use crate::diagnostic_runtime::cue_producer;

const CONTEXT_ERROR: &str = "diagnostic publication requires an active Runtime context";
const SPAN_CONTEXT_ERROR: &str = "custom span is not active in the current task";
const MULTIPLE_CONTEXTS_ERROR: &str = "custom publication matched multiple Runtime contexts";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn diagnostics_module(py: Python<'_>) -> PyResult<Bound<'_, PyModule>> {
    py.import("sys")?
        .getattr("modules")?
        .cast_into::<PyDict>()?
        .get_item("troupe.diagnostics")?
        .ok_or_else(|| PyRuntimeError::new_err("troupe.diagnostics is not installed"))?
        .cast_into::<PyModule>()
        .map_err(Into::into)
}

fn context_error(py: Python<'_>, message: &'static str) -> PyErr {
    diagnostics_module(py)
        .and_then(|diagnostics| diagnostics.getattr("DiagnosticContextError"))
        .and_then(|error_type| error_type.call1((message,)))
        .map_or_else(|error| error, PyErr::from_value)
}

static DOMAIN_EXTENSION: OnceLock<Arc<dyn CustomDomainExtension>> = OnceLock::new();

#[derive(Clone)]
pub(crate) struct CustomDomainSnapshot {
    runtime: Arc<RuntimeLifecycleProducer>,
    context: DiagnosticRunContext,
    scope: DiagnosticScope,
    containing_span_id: SchemaU64,
}

impl CustomDomainSnapshot {
    pub(crate) fn new(
        runtime: Arc<RuntimeLifecycleProducer>,
        context: DiagnosticRunContext,
        scope: DiagnosticScope,
        containing_span_id: SchemaU64,
    ) -> Self {
        Self {
            runtime,
            context,
            scope,
            containing_span_id,
        }
    }
}

pub(crate) trait CustomDomainExtension: Send + Sync {
    fn resolve(
        &self,
        py: Python<'_>,
        binding: &Arc<RunBinding>,
        lineage: &TaskLineage,
    ) -> PyResult<Option<CustomDomainSnapshot>>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CustomDomainExtensionInstallError;

impl fmt::Display for CustomDomainExtensionInstallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("custom diagnostic domain extension is already installed")
    }
}

impl std::error::Error for CustomDomainExtensionInstallError {}

fn domain_extension() -> Option<&'static Arc<dyn CustomDomainExtension>> {
    DOMAIN_EXTENSION.get()
}

pub(crate) fn install_domain_extension(
    extension: Arc<dyn CustomDomainExtension>,
) -> Result<(), CustomDomainExtensionInstallError> {
    DOMAIN_EXTENSION
        .set(extension)
        .map_err(|_| CustomDomainExtensionInstallError)
}

struct TaskSpanStack {
    identity: Py<PyWeakrefReference>,
    spans: Vec<SchemaU64>,
}

impl TaskSpanStack {
    fn new(task: &Bound<'_, PyAny>) -> PyResult<Self> {
        Ok(Self {
            identity: PyWeakrefReference::new(task)?.unbind(),
            spans: Vec::new(),
        })
    }

    fn matches(&self, task: &Bound<'_, PyAny>) -> bool {
        self.identity
            .bind(task.py())
            .upgrade()
            .is_some_and(|current| current.is(task))
    }
}

struct CustomRunBinding {
    binding: Weak<RunBinding>,
    task_spans: Mutex<HashMap<usize, TaskSpanStack>>,
}

impl CustomRunBinding {
    fn new(binding: &Arc<RunBinding>) -> Self {
        Self {
            binding: Arc::downgrade(binding),
            task_spans: Mutex::new(HashMap::new()),
        }
    }

    fn binding(&self) -> Option<Arc<RunBinding>> {
        self.binding.upgrade()
    }

    fn current_parent(&self, task: &Bound<'_, PyAny>, containing_span_id: SchemaU64) -> SchemaU64 {
        let key = task.as_ptr().addr();
        let mut task_spans = lock(&self.task_spans);
        let exact = task_spans
            .get(&key)
            .is_some_and(|stack| stack.matches(task));
        if !exact {
            task_spans.remove(&key);
            return containing_span_id;
        }
        task_spans
            .get(&key)
            .and_then(|stack| stack.spans.last().copied())
            .unwrap_or(containing_span_id)
    }

    fn prepare_span_task(
        &self,
        task: &Bound<'_, PyAny>,
        containing_span_id: SchemaU64,
    ) -> PyResult<SchemaU64> {
        let key = task.as_ptr().addr();
        let mut task_spans = lock(&self.task_spans);
        let exact = task_spans
            .get(&key)
            .is_some_and(|stack| stack.matches(task));
        if !exact {
            task_spans.insert(key, TaskSpanStack::new(task)?);
        }
        Ok(task_spans
            .get(&key)
            .and_then(|stack| stack.spans.last().copied())
            .unwrap_or(containing_span_id))
    }

    fn push_span(&self, task: &Bound<'_, PyAny>, span_id: SchemaU64) -> bool {
        let key = task.as_ptr().addr();
        let mut task_spans = lock(&self.task_spans);
        let Some(stack) = task_spans.get_mut(&key).filter(|stack| stack.matches(task)) else {
            return false;
        };
        stack.spans.push(span_id);
        true
    }

    fn pop_span(&self, task: &Bound<'_, PyAny>) -> Option<SchemaU64> {
        let key = task.as_ptr().addr();
        let mut task_spans = lock(&self.task_spans);
        let exact = task_spans
            .get(&key)
            .is_some_and(|stack| stack.matches(task));
        if !exact {
            task_spans.remove(&key);
            return None;
        }
        let span_id = task_spans.get_mut(&key).and_then(|stack| stack.spans.pop());
        if task_spans
            .get(&key)
            .is_some_and(|stack| stack.spans.is_empty())
        {
            task_spans.remove(&key);
        }
        span_id
    }
}

#[derive(Default)]
struct CustomBindingRegistry {
    runs: HashMap<usize, Arc<CustomRunBinding>>,
}

fn registry() -> &'static Mutex<CustomBindingRegistry> {
    static REGISTRY: OnceLock<Mutex<CustomBindingRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(CustomBindingRegistry::default()))
}

fn binding_key(binding: &RunBinding) -> usize {
    std::ptr::from_ref(binding).addr()
}

fn register_binding(binding: &Arc<RunBinding>) -> Result<Arc<CustomRunBinding>, PyErr> {
    let key = binding_key(binding);
    let mut registry = lock(registry());
    registry
        .runs
        .retain(|_, current| current.binding().is_some());
    if registry.runs.contains_key(&key) {
        return Err(PyRuntimeError::new_err(
            "custom diagnostic binding is already installed for this Runtime",
        ));
    }
    let custom = Arc::new(CustomRunBinding::new(binding));
    registry.runs.insert(key, Arc::clone(&custom));
    Ok(custom)
}

fn registered_bindings() -> Vec<Arc<CustomRunBinding>> {
    let mut registry = lock(registry());
    registry
        .runs
        .retain(|_, current| current.binding().is_some());
    registry.runs.values().cloned().collect()
}

fn snapshot_from_lineage(
    py: Python<'_>,
    binding: &Arc<RunBinding>,
    lineage: &TaskLineage,
) -> PyResult<Option<CustomDomainSnapshot>> {
    let Some(runtime) = runtime_producer::producer_for_binding(binding) else {
        return Ok(None);
    };
    if let Some(failure) = runtime.failure() {
        return Err(PyRuntimeError::new_err(format!(
            "diagnostic core is unavailable [{}]",
            failure.code()
        )));
    }

    if let Some(extension) = domain_extension()
        && let Some(snapshot) = extension.resolve(py, binding, lineage)?
    {
        if !Arc::ptr_eq(&snapshot.runtime, &runtime) {
            runtime.latch_state_failure("custom.domain-extension-runtime-mismatch");
            return Err(PyRuntimeError::new_err(
                "custom diagnostic domain extension returned another Runtime",
            ));
        }
        return Ok(Some(snapshot));
    }

    if let Some((runtime_binding, phase)) = lineage.runtime() {
        if !Arc::ptr_eq(&runtime_binding, binding) {
            return Ok(None);
        }
        return Ok(
            runtime_producer::lineage_snapshot(binding, phase).map(|snapshot| {
                CustomDomainSnapshot::new(
                    Arc::clone(snapshot.runtime()),
                    snapshot.context(),
                    snapshot.scope().clone(),
                    snapshot.containing_span_id(),
                )
            }),
        );
    }

    #[cfg(not(test))]
    if let Some(cued) = lineage.cued() {
        return Ok(cue_producer::lineage_snapshot(&cued).map(|snapshot| {
            CustomDomainSnapshot::new(
                Arc::clone(snapshot.runtime()),
                snapshot.context(),
                snapshot.cue_scope().clone(),
                snapshot.containing_span_id(),
            )
        }));
    }

    #[cfg(test)]
    if lineage.cued().is_some() {
        return Ok(None);
    }

    Ok(scene_producer::lineage_snapshot(lineage).map(|snapshot| {
        CustomDomainSnapshot::new(
            runtime,
            snapshot.context(),
            snapshot.scope().clone(),
            snapshot.scene_span_id(),
        )
    }))
}

fn current_task<'py>(py: Python<'py>) -> PyResult<Option<Bound<'py, PyAny>>> {
    let task = py.import("asyncio")?.getattr("current_task")?.call0()?;
    Ok((!task.is_none()).then_some(task))
}

fn current_authorization<'py>(
    py: Python<'py>,
    custom: &Arc<CustomRunBinding>,
) -> PyResult<Option<(Bound<'py, PyAny>, CustomDomainSnapshot)>> {
    let Some(binding) = custom.binding() else {
        return Ok(None);
    };
    let Some(lineage) = binding.current_lineage(py)?.filter(TaskLineage::is_active) else {
        return Ok(None);
    };
    let Some(snapshot) = snapshot_from_lineage(py, &binding, &lineage)? else {
        return Ok(None);
    };
    Ok(current_task(py)?.map(|task| (task, snapshot)))
}

struct AuthorizedPublication<'py> {
    binding: Arc<CustomRunBinding>,
    task: Bound<'py, PyAny>,
    domain: CustomDomainSnapshot,
}

fn authorize_publication(py: Python<'_>) -> PyResult<AuthorizedPublication<'_>> {
    let mut authorized: Option<AuthorizedPublication<'_>> = None;
    for binding in registered_bindings() {
        let Some((task, domain)) = current_authorization(py, &binding)? else {
            continue;
        };
        if let Some(previous) = &authorized {
            previous
                .domain
                .runtime
                .latch_state_failure("custom.multiple-runtime-contexts");
            domain
                .runtime
                .latch_state_failure("custom.multiple-runtime-contexts");
            return Err(PyRuntimeError::new_err(MULTIPLE_CONTEXTS_ERROR));
        }
        authorized = Some(AuthorizedPublication {
            binding,
            task,
            domain,
        });
    }
    authorized.ok_or_else(|| context_error(py, CONTEXT_ERROR))
}

enum CustomCandidate {
    Instant {
        name: String,
        severity: CustomSeverity,
        attributes: DiagnosticAttributes,
    },
    Counter {
        name: String,
        value: CustomNumber,
        unit: Option<String>,
        dimensions: DiagnosticDimensions,
    },
    SpanStart {
        name: String,
        attributes: DiagnosticAttributes,
    },
    SpanFinish {
        outcome: SpanOutcome,
    },
}

fn required_item<'py>(mapping: &Bound<'py, PyDict>, key: &str) -> PyResult<Bound<'py, PyAny>> {
    mapping.get_item(key)?.ok_or_else(|| {
        PyRuntimeError::new_err(format!("normalized custom payload is missing {key}"))
    })
}

fn tagged_type(mapping: &Bound<'_, PyDict>) -> PyResult<String> {
    required_item(mapping, "type")?.extract()
}

fn wire_error(field: &str, error: impl fmt::Display) -> PyErr {
    PyRuntimeError::new_err(format!("normalized custom {field} is invalid: {error}"))
}

fn parse_integer(value: &Bound<'_, PyAny>, field: &str) -> PyResult<CanonicalInteger> {
    let text = value.extract::<String>()?;
    CanonicalInteger::parse(&text).map_err(|error| wire_error(field, error))
}

fn parse_decimal(value: &Bound<'_, PyAny>, field: &str) -> PyResult<DecimalString> {
    let text = value.extract::<String>()?;
    DecimalString::parse(&text).map_err(|error| wire_error(field, error))
}

fn parse_scalar(value: &Bound<'_, PyAny>) -> PyResult<DiagnosticScalar> {
    let mapping = value.cast::<PyDict>()?;
    match tagged_type(mapping)?.as_str() {
        "null" => Ok(DiagnosticScalar::Null),
        "boolean" => Ok(DiagnosticScalar::Boolean(
            required_item(mapping, "value")?.extract()?,
        )),
        "integer" => Ok(DiagnosticScalar::Integer(parse_integer(
            &required_item(mapping, "value")?,
            "integer",
        )?)),
        "decimal" => Ok(DiagnosticScalar::Decimal(parse_decimal(
            &required_item(mapping, "value")?,
            "decimal",
        )?)),
        "string" => Ok(DiagnosticScalar::String(
            required_item(mapping, "value")?.extract()?,
        )),
        _ => Err(PyRuntimeError::new_err(
            "normalized custom scalar has an unknown type",
        )),
    }
}

fn parse_attribute(value: &Bound<'_, PyAny>) -> PyResult<DiagnosticAttributeValue> {
    let mapping = value.cast::<PyDict>()?;
    match tagged_type(mapping)?.as_str() {
        "null" => Ok(DiagnosticAttributeValue::Null),
        "boolean" => Ok(DiagnosticAttributeValue::Boolean(
            required_item(mapping, "value")?.extract()?,
        )),
        "integer" => Ok(DiagnosticAttributeValue::Integer(parse_integer(
            &required_item(mapping, "value")?,
            "attribute integer",
        )?)),
        "decimal" => Ok(DiagnosticAttributeValue::Decimal(parse_decimal(
            &required_item(mapping, "value")?,
            "attribute decimal",
        )?)),
        "string" => Ok(DiagnosticAttributeValue::String(
            required_item(mapping, "value")?.extract()?,
        )),
        "list" => {
            let values = required_item(mapping, "value")?.cast_into::<PyList>()?;
            Ok(DiagnosticAttributeValue::List(
                values
                    .iter()
                    .map(|value| parse_scalar(&value))
                    .collect::<PyResult<Vec<_>>>()?,
            ))
        }
        _ => Err(PyRuntimeError::new_err(
            "normalized custom attribute has an unknown type",
        )),
    }
}

fn parse_dimension(value: &Bound<'_, PyAny>) -> PyResult<DiagnosticDimension> {
    let mapping = value.cast::<PyDict>()?;
    match tagged_type(mapping)?.as_str() {
        "boolean" => Ok(DiagnosticDimension::Boolean(
            required_item(mapping, "value")?.extract()?,
        )),
        "integer" => Ok(DiagnosticDimension::Integer(parse_integer(
            &required_item(mapping, "value")?,
            "dimension integer",
        )?)),
        "decimal" => Ok(DiagnosticDimension::Decimal(parse_decimal(
            &required_item(mapping, "value")?,
            "dimension decimal",
        )?)),
        "string" => Ok(DiagnosticDimension::String(
            required_item(mapping, "value")?.extract()?,
        )),
        _ => Err(PyRuntimeError::new_err(
            "normalized custom dimension has an unknown type",
        )),
    }
}

fn parse_attributes(value: &Bound<'_, PyAny>) -> PyResult<DiagnosticAttributes> {
    let mapping = value.cast::<PyDict>()?;
    mapping
        .iter()
        .map(|(key, value)| Ok((key.extract::<String>()?, parse_attribute(&value)?)))
        .collect::<PyResult<BTreeMap<_, _>>>()
}

fn parse_dimensions(value: &Bound<'_, PyAny>) -> PyResult<DiagnosticDimensions> {
    let mapping = value.cast::<PyDict>()?;
    mapping
        .iter()
        .map(|(key, value)| Ok((key.extract::<String>()?, parse_dimension(&value)?)))
        .collect::<PyResult<BTreeMap<_, _>>>()
}

fn parse_number(value: &Bound<'_, PyAny>) -> PyResult<CustomNumber> {
    let mapping = value.cast::<PyDict>()?;
    match tagged_type(mapping)?.as_str() {
        "integer" => Ok(CustomNumber::Integer(parse_integer(
            &required_item(mapping, "value")?,
            "counter integer",
        )?)),
        "decimal" => Ok(CustomNumber::Decimal(parse_decimal(
            &required_item(mapping, "value")?,
            "counter decimal",
        )?)),
        _ => Err(PyRuntimeError::new_err(
            "normalized custom counter has an unknown number type",
        )),
    }
}

fn parse_severity(value: &Bound<'_, PyAny>) -> PyResult<CustomSeverity> {
    match value.extract::<String>()?.as_str() {
        "debug" => Ok(CustomSeverity::Debug),
        "info" => Ok(CustomSeverity::Info),
        "warning" => Ok(CustomSeverity::Warning),
        "error" => Ok(CustomSeverity::Error),
        _ => Err(PyRuntimeError::new_err(
            "normalized custom severity is invalid",
        )),
    }
}

fn parse_outcome(value: &Bound<'_, PyAny>) -> PyResult<SpanOutcome> {
    match value.extract::<String>()?.as_str() {
        "completed" => Ok(SpanOutcome::Completed),
        "cancelled" => Ok(SpanOutcome::Cancelled),
        "failed" => Ok(SpanOutcome::Failed),
        _ => Err(PyRuntimeError::new_err(
            "normalized custom span outcome is invalid",
        )),
    }
}

fn custom_payload<'py>(
    diagnostics: &Bound<'py, PyModule>,
    candidate: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyDict>> {
    diagnostics
        .getattr("_custom_candidate_payload")?
        .call1((candidate,))?
        .cast_into::<PyDict>()
        .map_err(Into::into)
}

fn exact_candidate_type(
    diagnostics: &Bound<'_, PyModule>,
    candidate: &Bound<'_, PyAny>,
    name: &str,
) -> PyResult<bool> {
    let expected = diagnostics.getattr(name)?.cast_into::<PyType>()?;
    Ok(candidate.get_type().is(&expected))
}

fn parse_candidate(
    diagnostics: &Bound<'_, PyModule>,
    candidate: &Bound<'_, PyAny>,
) -> PyResult<CustomCandidate> {
    if exact_candidate_type(diagnostics, candidate, "_CustomInstantCandidate")? {
        let payload = custom_payload(diagnostics, candidate)?;
        return Ok(CustomCandidate::Instant {
            name: required_item(&payload, "name")?.extract()?,
            severity: parse_severity(&required_item(&payload, "severity")?)?,
            attributes: parse_attributes(&required_item(&payload, "attributes")?)?,
        });
    }
    if exact_candidate_type(diagnostics, candidate, "_CustomCounterCandidate")? {
        let payload = custom_payload(diagnostics, candidate)?;
        let unit = required_item(&payload, "unit")?;
        return Ok(CustomCandidate::Counter {
            name: required_item(&payload, "name")?.extract()?,
            value: parse_number(&required_item(&payload, "value")?)?,
            unit: (!unit.is_none()).then(|| unit.extract()).transpose()?,
            dimensions: parse_dimensions(&required_item(&payload, "dimensions")?)?,
        });
    }
    if exact_candidate_type(diagnostics, candidate, "_CustomSpanStartCandidate")? {
        let payload = custom_payload(diagnostics, candidate)?;
        return Ok(CustomCandidate::SpanStart {
            name: required_item(&payload, "name")?.extract()?,
            attributes: parse_attributes(&required_item(&payload, "attributes")?)?,
        });
    }
    if exact_candidate_type(diagnostics, candidate, "_CustomSpanFinishCandidate")? {
        return Ok(CustomCandidate::SpanFinish {
            outcome: parse_outcome(&candidate.getattr("outcome")?)?,
        });
    }
    Err(PyTypeError::new_err(
        "custom admission requires an exact normalized candidate",
    ))
}

fn core_failure(
    runtime: &Arc<RuntimeLifecycleProducer>,
    error: crate::diagnostic_runtime::load_producer::DiagnosticProducerError,
) -> PyErr {
    let code = error.code().to_owned();
    runtime.latch_diagnostic_failure(error);
    PyRuntimeError::new_err(format!("diagnostic core admission failed [{code}]"))
}

fn ensure_runtime_healthy(runtime: &Arc<RuntimeLifecycleProducer>) -> PyResult<()> {
    match runtime.failure() {
        Some(error) => Err(PyRuntimeError::new_err(format!(
            "diagnostic core is unavailable [{}]",
            error.code()
        ))),
        None => Ok(()),
    }
}

fn admit_candidate(
    py: Python<'_>,
    authorized: AuthorizedPublication<'_>,
    candidate: CustomCandidate,
) -> PyResult<()> {
    let AuthorizedPublication {
        binding,
        task,
        domain,
    } = authorized;
    ensure_runtime_healthy(&domain.runtime)?;
    match candidate {
        CustomCandidate::Instant {
            name,
            severity,
            attributes,
        } => {
            let containing_span_id = binding.current_parent(&task, domain.containing_span_id);
            domain
                .context
                .emit_custom_instant(
                    domain.scope,
                    name,
                    Some(containing_span_id),
                    Some(severity),
                    attributes,
                )
                .map_err(|error| core_failure(&domain.runtime, error))
        }
        CustomCandidate::Counter {
            name,
            value,
            unit,
            dimensions,
        } => domain
            .context
            .emit_custom_counter(domain.scope, name, value, unit, dimensions)
            .map_err(|error| core_failure(&domain.runtime, error)),
        CustomCandidate::SpanStart { name, attributes } => {
            let parent_span_id = binding.prepare_span_task(&task, domain.containing_span_id)?;
            let span_id = domain
                .context
                .start_custom_span(domain.scope, name, Some(parent_span_id), attributes)
                .map_err(|error| core_failure(&domain.runtime, error))?;
            if !binding.push_span(&task, span_id) {
                domain
                    .runtime
                    .latch_state_failure("custom.task-span-registration-failed");
                return Err(PyRuntimeError::new_err(
                    "custom task span registration failed",
                ));
            }
            Ok(())
        }
        CustomCandidate::SpanFinish { outcome } => {
            let span_id = binding
                .pop_span(&task)
                .ok_or_else(|| context_error(py, SPAN_CONTEXT_ERROR))?;
            domain
                .context
                .finish_custom_span(domain.scope, span_id, outcome)
                .map_err(|error| core_failure(&domain.runtime, error))
        }
    }
}

#[pyclass(frozen, module = "troupe.diagnostics")]
struct CustomAdmissionHook;

#[pymethods]
impl CustomAdmissionHook {
    fn __call__(&self, py: Python<'_>, candidate: &Bound<'_, PyAny>) -> PyResult<()> {
        let authorized = authorize_publication(py)?;
        let diagnostics = diagnostics_module(py)?;
        let candidate = parse_candidate(&diagnostics, candidate)?;
        admit_candidate(py, authorized, candidate)
    }
}

pub(crate) fn install(diagnostics: &Bound<'_, PyModule>) -> PyResult<()> {
    let hook = Py::new(diagnostics.py(), CustomAdmissionHook)?;
    diagnostics
        .getattr("_set_custom_admission_hook")?
        .call1((hook,))
        .map(|_| ())
}

pub(crate) fn bind_run(_py: Python<'_>, binding: &Arc<RunBinding>) -> PyResult<()> {
    register_binding(binding).map(|_| ())
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Instant};

    use pyo3::{
        exceptions::PyRuntimeError,
        prelude::*,
        types::{PyAnyMethods, PyModule, PyModuleMethods},
    };
    use troupe_diagnostics_core::{
        detail::{CustomNumber, DiagnosticAttributeValue, DiagnosticDimension},
        event::{DiagnosticEvent, DiagnosticScope},
        hub::{
            AcceptedDiagnosticEvent, AdmissionReservation, AdmissionReserver, AdmissionSize,
            DeliveryFailure, LiveEventNotifier, MandatoryDurableReserver, ProductionDiagnosticHub,
        },
        id::CanonicalUuid,
        kinds::{CustomSeverity, SpanOutcome},
        scalar::SchemaU64,
        time::RunClock,
    };
    use uuid::Uuid;

    use crate::{
        diagnostic_python,
        diagnostic_runtime::{load_producer::DiagnosticRunContext, runtime_producer},
        orchestration::{runtime::RuntimeCore, scene_context::RunBinding},
    };

    use super::*;

    #[derive(Clone, Default)]
    struct EventLog(Arc<Mutex<Vec<AcceptedDiagnosticEvent>>>);

    impl EventLog {
        fn events(&self) -> Vec<AcceptedDiagnosticEvent> {
            lock(&self.0).clone()
        }
    }

    struct RecordingReservation(EventLog);

    impl AdmissionReservation for RecordingReservation {
        fn commit(self, event: AcceptedDiagnosticEvent) {
            lock(&self.0.0).push(event);
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct InjectedAdmissionError;

    impl fmt::Display for InjectedAdmissionError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("injected custom admission failure")
        }
    }

    impl std::error::Error for InjectedAdmissionError {}

    struct RecordingReserver {
        log: EventLog,
        attempts: usize,
        fail_on_attempt: Option<usize>,
    }

    impl AdmissionReserver for RecordingReserver {
        type Error = InjectedAdmissionError;
        type Reservation = RecordingReservation;

        fn try_reserve(&mut self, _size: AdmissionSize) -> Result<Self::Reservation, Self::Error> {
            self.attempts += 1;
            if self.fail_on_attempt == Some(self.attempts) {
                return Err(InjectedAdmissionError);
            }
            Ok(RecordingReservation(self.log.clone()))
        }
    }

    impl MandatoryDurableReserver for RecordingReserver {}

    struct IgnoreLive;

    impl LiveEventNotifier for IgnoreLive {
        fn notify(&mut self, _event: AcceptedDiagnosticEvent) -> Result<(), DeliveryFailure> {
            Ok(())
        }
    }

    fn context(fail_on_attempt: Option<usize>) -> (DiagnosticRunContext, EventLog) {
        let log = EventLog::default();
        let hub = Arc::new(ProductionDiagnosticHub::production(
            CanonicalUuid::new(Uuid::parse_str("12345678-1234-4234-9234-123456789abc").unwrap()),
            RecordingReserver {
                log: log.clone(),
                attempts: 0,
                fail_on_attempt,
            },
            Box::new(IgnoreLive),
        ));
        (
            DiagnosticRunContext::with_hub(hub, RunClock::from_origin(Instant::now())),
            log,
        )
    }

    fn install_diagnostics(py: Python<'_>) -> PyResult<Bound<'_, PyModule>> {
        let runtime = PyModule::new(py, "troupe._runtime")?;
        diagnostic_python::install(&runtime)?;
        Ok(runtime.getattr("diagnostics")?.cast_into::<PyModule>()?)
    }

    fn task_probes(py: Python<'_>) -> PyResult<(Bound<'_, PyAny>, Bound<'_, PyAny>)> {
        let module = PyModule::from_code(
            py,
            c"class TaskProbe:\n    pass\n\ncurrent = TaskProbe()\nchild = TaskProbe()\n",
            c"custom-task-probe.py",
            c"_troupe_custom_task_probe",
        )?;
        Ok((module.getattr("current")?, module.getattr("child")?))
    }

    #[test]
    fn native_hook_is_installed_once_and_rejects_publication_without_runtime_authority() {
        let _guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let diagnostics = install_diagnostics(py)?;
            install(&diagnostics)?;

            let error = diagnostics
                .getattr("event")?
                .call1(("outside.runtime",))
                .expect_err("publication without a Runtime must fail");
            let expected = diagnostics.getattr("DiagnosticContextError")?;
            assert!(error.is_instance(py, &expected));
            assert_eq!(error.value(py).str()?.to_str()?, CONTEXT_ERROR);
            Ok::<_, PyErr>(())
        })
        .expect("native custom hook context contract");
    }

    #[test]
    fn normalized_python_candidates_convert_to_closed_core_values() {
        let _guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let diagnostics = install_diagnostics(py)?;
            py.run(
                cr#"
_instant_probe = _CustomInstantCandidate(
    name="orders.ready",
    severity="warning",
    attributes=(
        ("attempt", 7),
        ("labels", (None, False, _Decimal("0.25"), "east")),
    ),
)
_counter_probe = _CustomCounterCandidate(
    name="orders.pending",
    value=_Decimal("12.5"),
    unit="items",
    dimensions=(("attempt", 2), ("region", "east")),
)
_start_probe = _CustomSpanStartCandidate(
    name="orders.batch",
    attributes=(("enabled", True),),
)
_finish_probe = _CustomSpanFinishCandidate(outcome="cancelled")
"#,
                Some(&diagnostics.dict()),
                Some(&diagnostics.dict()),
            )?;

            let CustomCandidate::Instant {
                name,
                severity,
                attributes,
            } = parse_candidate(&diagnostics, &diagnostics.getattr("_instant_probe")?)?
            else {
                panic!("expected instant candidate")
            };
            assert_eq!(name, "orders.ready");
            assert_eq!(severity, CustomSeverity::Warning);
            assert!(matches!(
                attributes.get("attempt"),
                Some(DiagnosticAttributeValue::Integer(value)) if value.as_str() == "7"
            ));
            assert!(matches!(
                attributes.get("labels"),
                Some(DiagnosticAttributeValue::List(values)) if values.len() == 4
            ));

            let CustomCandidate::Counter {
                name,
                value,
                unit,
                dimensions,
            } = parse_candidate(&diagnostics, &diagnostics.getattr("_counter_probe")?)?
            else {
                panic!("expected counter candidate")
            };
            assert_eq!(name, "orders.pending");
            assert!(matches!(value, CustomNumber::Decimal(value) if value.as_str() == "12.5"));
            assert_eq!(unit.as_deref(), Some("items"));
            assert!(matches!(
                dimensions.get("attempt"),
                Some(DiagnosticDimension::Integer(value)) if value.as_str() == "2"
            ));

            assert!(matches!(
                parse_candidate(&diagnostics, &diagnostics.getattr("_start_probe")?)?,
                CustomCandidate::SpanStart { name, .. } if name == "orders.batch"
            ));
            assert!(matches!(
                parse_candidate(&diagnostics, &diagnostics.getattr("_finish_probe")?)?,
                CustomCandidate::SpanFinish {
                    outcome: SpanOutcome::Cancelled
                }
            ));
            Ok::<_, PyErr>(())
        })
        .expect("normalized candidates must convert without reinterpretation");
    }

    #[test]
    fn custom_temporal_parent_is_task_local_and_all_facts_use_the_shared_hub() {
        let _guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let (context, log) = context(None);
            let core = Arc::new(RuntimeCore::new());
            let permit = core.begin().expect("begin test Runtime");
            let binding = Arc::new(RunBinding::new_for_test(py)?);
            let runtime = runtime_producer::install(&core, &binding, context.clone())
                .expect("install Runtime producer");
            let custom = Arc::new(CustomRunBinding::new(&binding));
            let (current, child) = task_probes(py)?;
            let domain = CustomDomainSnapshot::new(
                Arc::clone(&runtime),
                context,
                DiagnosticScope::new(None, None, None, None, None, None, None),
                runtime.run_span_id(),
            );

            let publish = |task: &Bound<'_, PyAny>, candidate| {
                admit_candidate(
                    py,
                    AuthorizedPublication {
                        binding: Arc::clone(&custom),
                        task: task.clone(),
                        domain: domain.clone(),
                    },
                    candidate,
                )
            };
            publish(
                &current,
                CustomCandidate::SpanStart {
                    name: "orders.outer".to_owned(),
                    attributes: Default::default(),
                },
            )?;
            publish(
                &current,
                CustomCandidate::Instant {
                    name: "orders.inside".to_owned(),
                    severity: CustomSeverity::Info,
                    attributes: Default::default(),
                },
            )?;
            publish(
                &current,
                CustomCandidate::SpanStart {
                    name: "orders.inner".to_owned(),
                    attributes: Default::default(),
                },
            )?;
            publish(
                &child,
                CustomCandidate::Instant {
                    name: "orders.child".to_owned(),
                    severity: CustomSeverity::Debug,
                    attributes: Default::default(),
                },
            )?;
            publish(
                &current,
                CustomCandidate::SpanFinish {
                    outcome: SpanOutcome::Failed,
                },
            )?;
            publish(
                &current,
                CustomCandidate::SpanFinish {
                    outcome: SpanOutcome::Completed,
                },
            )?;

            let events = log.events();
            assert_eq!(events.len(), 7);
            let DiagnosticEvent::CustomSpanStarted(outer) = events[1].event() else {
                panic!("expected outer custom span")
            };
            assert_eq!(outer.parent_span_id(), Some(SchemaU64::new(1)));
            let DiagnosticEvent::CustomInstantOccurred(inside) = events[2].event() else {
                panic!("expected inner instant")
            };
            assert_eq!(inside.containing_span_id(), Some(SchemaU64::new(2)));
            let DiagnosticEvent::CustomSpanStarted(inner) = events[3].event() else {
                panic!("expected nested custom span")
            };
            assert_eq!(inner.parent_span_id(), Some(SchemaU64::new(2)));
            let DiagnosticEvent::CustomInstantOccurred(child_event) = events[4].event() else {
                panic!("expected child instant")
            };
            assert_eq!(child_event.containing_span_id(), Some(SchemaU64::new(1)));
            let DiagnosticEvent::CustomSpanFinished(inner_finish) = events[5].event() else {
                panic!("expected nested finish")
            };
            assert_eq!(inner_finish.span_id(), SchemaU64::new(4));
            assert_eq!(inner_finish.outcome(), SpanOutcome::Failed);
            let DiagnosticEvent::CustomSpanFinished(outer_finish) = events[6].event() else {
                panic!("expected outer finish")
            };
            assert_eq!(outer_finish.span_id(), SchemaU64::new(2));
            assert_eq!(outer_finish.outcome(), SpanOutcome::Completed);
            assert!(
                events
                    .iter()
                    .all(|event| event.event().header().caused_by().is_empty())
            );
            drop(permit);
            Ok::<_, PyErr>(())
        })
        .expect("custom temporal lineage must remain task-local");
    }

    #[test]
    fn admission_failure_latches_runtime_and_blocks_caught_continuation() {
        let _guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let (context, log) = context(Some(2));
            let core = Arc::new(RuntimeCore::new());
            let permit = core.begin().expect("begin test Runtime");
            let binding = Arc::new(RunBinding::new_for_test(py)?);
            let runtime = runtime_producer::install(&core, &binding, context.clone())
                .expect("install Runtime producer");
            let custom = Arc::new(CustomRunBinding::new(&binding));
            let (task, _) = task_probes(py)?;
            let authorized = || AuthorizedPublication {
                binding: Arc::clone(&custom),
                task: task.clone(),
                domain: CustomDomainSnapshot::new(
                    Arc::clone(&runtime),
                    context.clone(),
                    DiagnosticScope::new(None, None, None, None, None, None, None),
                    runtime.run_span_id(),
                ),
            };

            let first = admit_candidate(
                py,
                authorized(),
                CustomCandidate::Instant {
                    name: "orders.failure".to_owned(),
                    severity: CustomSeverity::Error,
                    attributes: Default::default(),
                },
            )
            .expect_err("injected mandatory admission failure");
            assert!(first.is_instance_of::<PyRuntimeError>(py));
            assert_eq!(
                runtime.failure().as_ref().map(|failure| failure.code()),
                Some("diagnostic.admission-failed")
            );
            assert_eq!(log.events().len(), 1);

            admit_candidate(
                py,
                authorized(),
                CustomCandidate::Instant {
                    name: "orders.after_failure".to_owned(),
                    severity: CustomSeverity::Info,
                    attributes: Default::default(),
                },
            )
            .expect_err("a caught Python error cannot recover the Runtime");
            assert_eq!(log.events().len(), 1);
            drop(permit);
            Ok::<_, PyErr>(())
        })
        .expect("mandatory custom admission failure must remain fatal");
    }
}
