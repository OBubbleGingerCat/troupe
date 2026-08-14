use std::sync::{Arc, Weak};

use pyo3::class::gc::{PyTraverseError, PyVisit};
use pyo3::exceptions::{PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::PyAny;

use crate::agent::{
    AgentActError, AgentResultIssueData, AgentSessionFailure, AgentSessionSlot,
    AgentTurnCancelDecision, AgentTurnControl, AgentTurnOutcome, AgentTurnStop, CompiledActSchema,
    MAX_REPAIRABLE_INVALID_CALLS, PythonSchemaValidationBridge, SchemaValidationMode, busy_error,
    missing_result_error, result_error, session_broken_error, turn_error,
};
use crate::diagnostic_runtime::act_producer::{self, ActCallerExit, ActHook};
use crate::diagnostic_runtime::hooks::DiagnosticActBinding;
use crate::orchestration::cue::CueContextError;
use crate::orchestration::scene_context::{CuedScope, RunBinding};

const ACT_CONTEXT_ERROR: &str =
    "Actor.act() must be called on the current actor within its active cued context";
const REUSE_ERROR: &str = "cannot reuse already awaited coroutine";

pub(crate) fn preflight_diagnostic_sink(
    _py: Python<'_>,
    _sink: Option<&Bound<'_, PyAny>>,
) -> PyResult<DiagnosticActBinding> {
    Ok(DiagnosticActBinding::inactive())
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
