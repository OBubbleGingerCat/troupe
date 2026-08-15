use std::sync::{Arc, Weak};

use pyo3::class::gc::{PyTraverseError, PyVisit};
use pyo3::exceptions::{PyAttributeError, PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyBool, PyDict, PyModule, PyType};

use crate::agent::{
    AgentActError, AgentResultIssueData, AgentSessionFailure, AgentSessionSlot,
    AgentTurnCancelDecision, AgentTurnControl, AgentTurnOutcome, AgentTurnStop, CompiledActSchema,
    MAX_REPAIRABLE_INVALID_CALLS, PythonSchemaValidationBridge, SchemaValidationMode, busy_error,
    missing_result_error, result_error, session_broken_error, turn_error,
};
use crate::diagnostic_runtime::act_producer::{self, ActCallerExit, ActHook};
use crate::diagnostic_runtime::hooks::{DiagnosticActBinding, DiagnosticCaptureConfig};
use crate::orchestration::cue::CueContextError;
use crate::orchestration::scene_context::{CuedScope, RunBinding};

const ACT_CONTEXT_ERROR: &str =
    "Actor.act() must be called on the current actor within its active cued context";
const REUSE_ERROR: &str = "cannot reuse already awaited coroutine";
const DIAGNOSTIC_SINK_TYPE_ERROR: &str =
    "diagnostic_sink must be a DiagnosticSink instance or None";

fn diagnostics_module(py: Python<'_>) -> PyResult<Bound<'_, PyModule>> {
    let modules = py
        .import("sys")?
        .getattr("modules")?
        .cast_into::<PyDict>()?;
    modules
        .get_item("troupe.diagnostics")?
        .ok_or_else(|| PyRuntimeError::new_err("troupe.diagnostics is not installed"))?
        .cast_into::<PyModule>()
        .map_err(Into::into)
}

fn diagnostic_sink_state_error(diagnostics: &Bound<'_, PyModule>, code: &'static str) -> PyErr {
    let kwargs = PyDict::new(diagnostics.py());
    if let Err(error) = kwargs.set_item("code", code) {
        return error;
    }
    diagnostics
        .getattr("DiagnosticSinkStateError")
        .and_then(|error_type| error_type.call((), Some(&kwargs)))
        .map_or_else(|error| error, PyErr::from_value)
}

fn require_base_slot<'py>(
    diagnostics: &Bound<'py, PyModule>,
    base_type: &Bound<'py, PyType>,
    instance: &Bound<'py, PyAny>,
    slot: &str,
) -> PyResult<Bound<'py, PyAny>> {
    let descriptor = base_type.getattr("__dict__")?.get_item(slot)?;
    match descriptor.call_method1("__get__", (instance, instance.get_type())) {
        Ok(value) => Ok(value),
        Err(error) if error.is_instance_of::<PyAttributeError>(diagnostics.py()) => {
            Err(diagnostic_sink_state_error(diagnostics, "uninitialized"))
        }
        Err(error) => Err(error),
    }
}

fn capture_flag(
    diagnostics: &Bound<'_, PyModule>,
    capture_type: &Bound<'_, PyType>,
    capture: &Bound<'_, PyAny>,
    field: &str,
) -> PyResult<bool> {
    let value = require_base_slot(diagnostics, capture_type, capture, field)?;
    if !value.is_exact_instance_of::<PyBool>() {
        return Err(PyTypeError::new_err(
            "diagnostic sink capture fields must be exact bool values",
        ));
    }
    value.extract()
}

pub(crate) fn preflight_diagnostic_sink(
    py: Python<'_>,
    sink: Option<&Bound<'_, PyAny>>,
) -> PyResult<DiagnosticActBinding> {
    let Some(sink) = sink else {
        return Ok(DiagnosticActBinding::inactive());
    };

    let diagnostics = diagnostics_module(py)?;
    let sink_type = diagnostics
        .getattr("DiagnosticSink")?
        .cast_into::<PyType>()?;
    if !sink
        .get_type()
        .mro()
        .iter()
        .any(|candidate| candidate.is(&sink_type))
    {
        return Err(PyTypeError::new_err(DIAGNOSTIC_SINK_TYPE_ERROR));
    }

    let _lock = require_base_slot(&diagnostics, &sink_type, sink, "_DiagnosticSink__lock")?;
    let state = require_base_slot(&diagnostics, &sink_type, sink, "_DiagnosticSink__state")?
        .extract::<String>()?;
    if !matches!(state.as_str(), "UNBOUND" | "BOUND" | "SEALED" | "CLOSED") {
        return Err(PyRuntimeError::new_err(
            "diagnostic sink lifecycle state is corrupt",
        ));
    }
    let capture = require_base_slot(&diagnostics, &sink_type, sink, "_DiagnosticSink__capture")?;
    let _summary = require_base_slot(&diagnostics, &sink_type, sink, "_DiagnosticSink__summary")?;
    let _waiters = require_base_slot(&diagnostics, &sink_type, sink, "_DiagnosticSink__waiters")?;

    let capture_type = diagnostics
        .getattr("DiagnosticCapture")?
        .cast_into::<PyType>()?;
    if !capture.get_type().is(&capture_type) {
        return Err(PyTypeError::new_err(
            "diagnostic sink capture must be an exact DiagnosticCapture",
        ));
    }
    let capture = DiagnosticCaptureConfig::new(
        capture_flag(&diagnostics, &capture_type, &capture, "agent_messages")?,
        capture_flag(&diagnostics, &capture_type, &capture, "plans")?,
        capture_flag(&diagnostics, &capture_type, &capture, "tool_calls")?,
        capture_flag(&diagnostics, &capture_type, &capture, "result_validation")?,
        capture_flag(&diagnostics, &capture_type, &capture, "usage")?,
        capture_flag(&diagnostics, &capture_type, &capture, "custom_events")?,
        capture_flag(&diagnostics, &capture_type, &capture, "tool_inputs")?,
        capture_flag(&diagnostics, &capture_type, &capture, "tool_outputs")?,
    );
    Ok(DiagnosticActBinding::new(capture, sink.clone().unbind()))
}

#[cfg(test)]
mod preflight_tests {
    use pyo3::exceptions::PyTypeError;
    use pyo3::prelude::*;
    use pyo3::types::{PyAny, PyDict, PyModule};

    use super::preflight_diagnostic_sink;

    fn error_from_preflight(py: Python<'_>, sink: &Bound<'_, PyAny>) -> PyErr {
        match preflight_diagnostic_sink(py, Some(sink)) {
            Ok(_) => panic!("diagnostic sink preflight unexpectedly succeeded"),
            Err(error) => error,
        }
    }

    #[test]
    fn diagnostic_sink_preflight_is_nominal_initialized_and_non_binding() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let runtime = PyModule::new(py, "troupe._runtime")?;
            crate::diagnostic_python::install(&runtime)?;
            let diagnostics = runtime.getattr("diagnostics")?.cast_into::<PyModule>()?;
            let namespace = diagnostics.dict();
            py.run(
                cr#"
class _ValidSink(DiagnosticSink):
    def on_event(self, event, /):
        pass

class _MissingSuperSink(DiagnosticSink):
    def __init__(self):
        pass

    def on_event(self, event, /):
        pass

class _PoisonedOverridesSink(_ValidSink):
    @property
    def capture(self):
        raise AssertionError("public capture override was invoked")

    @property
    def state(self):
        raise AssertionError("public state override was invoked")

    def _diagnostic_require_lock(self):
        raise AssertionError("private override was invoked")

class _VirtualSink:
    pass

DiagnosticSink.register(_VirtualSink)
assert isinstance(_VirtualSink(), DiagnosticSink)

_valid = _ValidSink(capture=DiagnosticCapture(
    agent_messages=False,
    plans=True,
    tool_calls=True,
    result_validation=False,
    usage=True,
    custom_events=False,
    tool_inputs=True,
    tool_outputs=False,
))
_missing = _MissingSuperSink()
_poisoned = _PoisonedOverridesSink()
_virtual = _VirtualSink()
_bound = _ValidSink()
_bound._diagnostic_bind()
"#,
                Some(&namespace),
                Some(&namespace),
            )?;

            let inactive = preflight_diagnostic_sink(py, None)?;
            assert!(!inactive.is_active());

            let object = py.import("builtins")?.getattr("object")?.call0()?;
            let type_error = error_from_preflight(py, &object);
            assert!(type_error.is_instance_of::<PyTypeError>(py));

            let virtual_sink = diagnostics.getattr("_virtual")?;
            let virtual_error = error_from_preflight(py, &virtual_sink);
            assert!(virtual_error.is_instance_of::<PyTypeError>(py));

            let missing = diagnostics.getattr("_missing")?;
            let missing_error = error_from_preflight(py, &missing);
            assert!(
                missing_error.is_instance(py, &diagnostics.getattr("DiagnosticSinkStateError")?,)
            );
            assert_eq!(
                missing_error
                    .value(py)
                    .getattr("code")?
                    .extract::<String>()?,
                "uninitialized"
            );

            let valid = diagnostics.getattr("_valid")?;
            let binding = preflight_diagnostic_sink(py, Some(&valid))?;
            assert!(binding.is_active());
            assert!(
                binding
                    .request(py)
                    .expect("active request")
                    .bind(py)
                    .is(&valid)
            );
            let capture = binding.capture();
            assert!(!capture.agent_messages);
            assert!(capture.plans);
            assert!(capture.tool_calls);
            assert!(!capture.result_validation);
            assert!(capture.usage);
            assert!(!capture.custom_events);
            assert!(capture.tool_inputs);
            assert!(!capture.tool_outputs);
            assert_eq!(
                diagnostics
                    .getattr("DiagnosticSink")?
                    .getattr("__dict__")?
                    .get_item("_DiagnosticSink__state")?
                    .call_method1("__get__", (&valid, valid.get_type()))?
                    .extract::<String>()?,
                "UNBOUND"
            );

            let poisoned = diagnostics.getattr("_poisoned")?;
            let poisoned_binding = preflight_diagnostic_sink(py, Some(&poisoned))?;
            assert!(poisoned_binding.is_active());

            let bound = diagnostics.getattr("_bound")?;
            let bound_binding = preflight_diagnostic_sink(py, Some(&bound))?;
            assert!(bound_binding.is_active());
            assert_eq!(
                diagnostics
                    .getattr("DiagnosticSink")?
                    .getattr("__dict__")?
                    .get_item("_DiagnosticSink__state")?
                    .call_method1("__get__", (&bound, bound.get_type()))?
                    .extract::<String>()?,
                "BOUND"
            );

            py.import("sys")?
                .getattr("modules")?
                .cast_into::<PyDict>()?
                .del_item("troupe.diagnostics")?;
            Ok::<_, PyErr>(())
        })
        .expect("diagnostic sink preflight contract must hold");
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ActCallPhase {
    Created,
    Running,
    Done,
}

#[pyclass(name = "_ActCall", module = "troupe._runtime")]
pub(crate) struct ActCall {
    session: Option<Arc<AgentSessionSlot>>,
    prompt: Option<String>,
    schema: Option<Arc<CompiledActSchema>>,
    binding: Weak<RunBinding>,
    cued: Weak<CuedScope>,
    diagnostics: DiagnosticActBinding,
    control: Option<Arc<AgentTurnControl>>,
    signal: Option<Py<PyAny>>,
    driver: Option<Py<PyAny>>,
    phase: ActCallPhase,
}

impl ActCall {
    pub(crate) fn new(
        session: Arc<AgentSessionSlot>,
        prompt: String,
        schema: Arc<CompiledActSchema>,
        binding: &Arc<RunBinding>,
        cued: &Arc<CuedScope>,
        diagnostics: DiagnosticActBinding,
    ) -> Self {
        let control = AgentTurnControl::new(Arc::clone(&session));
        Self {
            session: Some(session),
            prompt: Some(prompt),
            schema: Some(schema),
            binding: Arc::downgrade(binding),
            cued: Arc::downgrade(cued),
            diagnostics,
            control: Some(control),
            signal: None,
            driver: None,
            phase: ActCallPhase::Created,
        }
    }

    fn validate_context(&self, py: Python<'_>) -> PyResult<Arc<RunBinding>> {
        let binding = self
            .binding
            .upgrade()
            .ok_or_else(|| CueContextError::new_err(ACT_CONTEXT_ERROR))?;
        let expected = self
            .cued
            .upgrade()
            .ok_or_else(|| CueContextError::new_err(ACT_CONTEXT_ERROR))?;
        let current = binding
            .current_lineage(py)?
            .filter(crate::orchestration::python_task::TaskLineage::is_active)
            .and_then(|lineage| lineage.cued())
            .filter(|cued| Arc::ptr_eq(cued, &expected));
        if current.is_none() {
            return Err(CueContextError::new_err(ACT_CONTEXT_ERROR));
        }
        Ok(binding)
    }

    fn start_driver(&mut self, py: Python<'_>) -> PyResult<()> {
        let binding = self.validate_context(py)?;
        let cued = self
            .cued
            .upgrade()
            .ok_or_else(|| CueContextError::new_err(ACT_CONTEXT_ERROR))?;
        let control = self
            .control
            .as_ref()
            .map(Arc::clone)
            .ok_or_else(|| PyRuntimeError::new_err(REUSE_ERROR))?;
        if !cued.register_agent_turn(&control) {
            return Err(CueContextError::new_err(ACT_CONTEXT_ERROR));
        }
        let session = self
            .session
            .take()
            .ok_or_else(|| PyRuntimeError::new_err(REUSE_ERROR))?;
        let prompt = self
            .prompt
            .take()
            .ok_or_else(|| PyRuntimeError::new_err(REUSE_ERROR))?;
        let schema = self
            .schema
            .take()
            .ok_or_else(|| PyRuntimeError::new_err(REUSE_ERROR))?;
        let admission = session
            .try_claim_admission()
            .ok_or_else(|| busy_error(py))?;
        if !control.install_admission(admission) {
            return Err(cancelled_error(py));
        }
        act_producer::admitted(&binding, &cued, &control);
        let diagnostics =
            std::mem::replace(&mut self.diagnostics, DiagnosticActBinding::inactive());
        crate::diagnostic_runtime::sink_binding::admit_act(py, &binding, &control, diagnostics)?;
        let validation_bridge = match schema.validation_mode() {
            SchemaValidationMode::NativeOnly => None,
            SchemaValidationMode::Hybrid => Some(PythonSchemaValidationBridge::new(py)?),
        };
        let future = pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let outcome = session
                .run_turn(prompt, schema, validation_bridge, control)
                .await
                .map_err(|error| Python::attach(|py| act_error(py, error)))?;
            Python::attach(|py| turn_outcome(py, outcome))
        })?;
        let signal = future.unbind();
        self.driver = Some(Self::fresh_waiter(py, &signal)?);
        self.signal = Some(signal);
        self.phase = ActCallPhase::Running;
        if let Some(control) = &self.control {
            act_producer::observe(control, ActHook::DriverStarted);
        }
        Ok(())
    }

    fn fresh_waiter(py: Python<'_>, signal: &Py<PyAny>) -> PyResult<Py<PyAny>> {
        py.import("asyncio")?
            .call_method1("shield", (signal.bind(py),))?
            .call_method0("__await__")
            .map(Bound::unbind)
    }

    fn replace_shield_and_wait(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let signal = self
            .signal
            .as_ref()
            .map(|signal| signal.clone_ref(py))
            .ok_or_else(|| PyRuntimeError::new_err(REUSE_ERROR))?;
        let waiter = Self::fresh_waiter(py, &signal)?;
        let previous = self.driver.replace(waiter);
        drop(previous);
        let none = py.None();
        self.drive(py, "send", none.bind(py))
    }

    fn is_cancelled_error(py: Python<'_>, exc: &Bound<'_, PyAny>) -> PyResult<bool> {
        exc.is_instance(&py.import("asyncio")?.getattr("CancelledError")?)
    }

    fn request_cancel(&self) -> AgentTurnCancelDecision {
        self.control
            .as_ref()
            .map_or(AgentTurnCancelDecision::Accepted, |control| {
                act_producer::observe(control, ActHook::CancelRequested);
                control.request_cancel()
            })
    }

    fn cancel_signal(&self, py: Python<'_>) {
        if let Some(signal) = &self.signal {
            let _ = signal.bind(py).call_method0("cancel");
        }
    }

    fn drive(
        &mut self,
        py: Python<'_>,
        method: &str,
        value: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let driver = self
            .driver
            .as_ref()
            .map(|driver| driver.clone_ref(py))
            .ok_or_else(|| PyRuntimeError::new_err(REUSE_ERROR))?;
        match driver.bind(py).call_method1(method, (value,)) {
            Ok(yielded) => Ok(yielded.unbind()),
            Err(error) => {
                self.finish(ActCallerExit::Returned, Some(&error));
                Err(error)
            }
        }
    }

    fn finish(&mut self, exit: ActCallerExit, error: Option<&PyErr>) {
        if let Some(control) = &self.control {
            if self.phase != ActCallPhase::Done {
                act_producer::caller_finished(control, exit, error);
            }
            control.request_cancel();
            control.finish_caller();
        }
        self.phase = ActCallPhase::Done;
        self.session = None;
        self.prompt = None;
        self.schema = None;
        self.control = None;
        self.signal = None;
        self.driver = None;
        self.diagnostics.clear();
    }
}

fn act_error(py: Python<'_>, error: AgentActError) -> PyErr {
    match error {
        AgentActError::Startup(failure) => failure.to_pyerr(py),
        AgentActError::SessionBroken(failure) => session_broken_error(py, &failure),
        AgentActError::SessionClosed => {
            session_broken_error(py, &AgentSessionFailure::transport_lost())
        }
        AgentActError::CallerCancelled => cancelled_error(py),
    }
}

fn cancelled_error(py: Python<'_>) -> PyErr {
    py.import("asyncio")
        .and_then(|module| module.getattr("CancelledError"))
        .and_then(|error_type| error_type.call0())
        .map_or_else(|error| error, PyErr::from_value)
}

fn turn_outcome(py: Python<'_>, outcome: AgentTurnOutcome) -> PyResult<Py<PyAny>> {
    match outcome {
        AgentTurnOutcome::Success(value) => value.into_py(py),
        AgentTurnOutcome::MissingResult => Err(missing_result_error(py)),
        AgentTurnOutcome::ResultRejected {
            issues,
            invalid_calls,
            truncated,
        } => {
            let (code, message) = if invalid_calls > MAX_REPAIRABLE_INVALID_CALLS {
                (
                    "too_many_invalid_results",
                    "agent submitted too many invalid results",
                )
            } else {
                ("invalid_result", "agent turn ended with an invalid result")
            };
            Err(result_error(
                py,
                code,
                message,
                issues
                    .into_iter()
                    .map(|issue| AgentResultIssueData {
                        path: issue.path,
                        code: issue.code,
                        message: issue.message,
                    })
                    .collect(),
                invalid_calls,
                truncated,
            ))
        }
        AgentTurnOutcome::Stopped(stop) => {
            let (code, message) = match stop {
                AgentTurnStop::MaxTokens => ("max_tokens", "agent turn reached its token limit"),
                AgentTurnStop::MaxTurnRequests => {
                    ("max_turn_requests", "agent turn reached its request limit")
                }
                AgentTurnStop::Refusal => ("refused", "agent refused the turn"),
                AgentTurnStop::Cancelled => ("remote_cancelled", "agent turn was cancelled"),
                AgentTurnStop::RequestFailed => ("request_failed", "agent turn request failed"),
            };
            Err(turn_error(py, code, message))
        }
        AgentTurnOutcome::SchemaCallbackFailed(error) => Err(error),
        AgentTurnOutcome::SessionBroken(failure) => Err(session_broken_error(py, &failure)),
    }
}

#[pymethods]
impl ActCall {
    fn send(&mut self, py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        match self.phase {
            ActCallPhase::Created => {
                if !value.is_none() {
                    let error = PyTypeError::new_err(
                        "can't send non-None value to a just-started coroutine",
                    );
                    self.finish(ActCallerExit::AdmissionFailed, Some(&error));
                    return Err(error);
                }
                if let Err(error) = self.start_driver(py) {
                    self.finish(ActCallerExit::AdmissionFailed, Some(&error));
                    return Err(error);
                }
                self.drive(py, "send", value)
            }
            ActCallPhase::Running => self.drive(py, "send", value),
            ActCallPhase::Done => Err(PyRuntimeError::new_err(REUSE_ERROR)),
        }
    }

    fn throw(&mut self, py: Python<'_>, exc: Py<PyAny>) -> PyResult<Py<PyAny>> {
        match self.phase {
            ActCallPhase::Created => {
                let error = PyErr::from_value(exc.into_bound(py));
                self.finish(ActCallerExit::AdmissionFailed, Some(&error));
                Err(error)
            }
            ActCallPhase::Running if Self::is_cancelled_error(py, exc.bind(py))? => {
                if self.request_cancel() == AgentTurnCancelDecision::Rejected {
                    return self.replace_shield_and_wait(py);
                }
                self.cancel_signal(py);
                let error = PyErr::from_value(exc.into_bound(py));
                self.finish(ActCallerExit::Cancelled, Some(&error));
                Err(error)
            }
            ActCallPhase::Running => self.drive(py, "throw", exc.bind(py)),
            ActCallPhase::Done => Err(PyRuntimeError::new_err(REUSE_ERROR)),
        }
    }

    fn close(&mut self, py: Python<'_>) -> PyResult<()> {
        self.request_cancel();
        self.cancel_signal(py);
        let result = self
            .driver
            .as_ref()
            .map(|driver| driver.bind(py).call_method0("close"))
            .transpose();
        self.finish(ActCallerExit::Closed, result.as_ref().err());
        result.map(|_| ())
    }

    fn __await__(self_: Py<Self>) -> Py<Self> {
        self_
    }

    fn __iter__(self_: Py<Self>) -> Py<Self> {
        self_
    }

    fn __next__(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let none = py.None();
        self.send(py, none.bind(py))
    }

    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.signal)?;
        visit.call(&self.driver)?;
        if let Some(schema) = &self.schema {
            schema.traverse(&visit)?;
        }
        self.diagnostics.traverse(&visit)?;
        Ok(())
    }

    fn __clear__(&mut self) {
        self.finish(ActCallerExit::Cleared, None);
    }
}
