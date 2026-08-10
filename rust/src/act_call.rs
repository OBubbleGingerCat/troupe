use std::sync::{Arc, Weak};

use pyo3::class::gc::{PyTraverseError, PyVisit};
use pyo3::exceptions::{PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::PyAny;

use crate::act_schema::{CompiledActSchema, SchemaValidationMode};
use crate::agent_error::{
    AgentResultIssueData, AgentSessionFailure, busy_error, missing_result_error, result_error,
    session_broken_error, turn_error,
};
use crate::agent_session::{AgentActError, AgentSessionSlot};
use crate::agent_turn::{AgentTurnOutcome, AgentTurnStop};
use crate::cue::CueContextError;
use crate::result_mcp::MAX_REPAIRABLE_INVALID_CALLS;
use crate::scene_context::{CuedScope, RunBinding};
use crate::schema_validation_bridge::PythonSchemaValidationBridge;

const ACT_CONTEXT_ERROR: &str =
    "Actor.act() must be called on the current actor within its active cued context";
const REUSE_ERROR: &str = "cannot reuse already awaited coroutine";

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
    ) -> Self {
        Self {
            session: Some(session),
            prompt: Some(prompt),
            schema: Some(schema),
            binding: Arc::downgrade(binding),
            cued: Arc::downgrade(cued),
            driver: None,
            phase: ActCallPhase::Created,
        }
    }

    fn validate_context(&self, py: Python<'_>) -> PyResult<()> {
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
            .filter(crate::python_task::TaskLineage::is_active)
            .and_then(|lineage| lineage.cued())
            .filter(|cued| Arc::ptr_eq(cued, &expected));
        if current.is_none() {
            return Err(CueContextError::new_err(ACT_CONTEXT_ERROR));
        }
        Ok(())
    }

    fn start_driver(&mut self, py: Python<'_>) -> PyResult<()> {
        self.validate_context(py)?;
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
        let validation_bridge = match schema.validation_mode() {
            SchemaValidationMode::NativeOnly => None,
            SchemaValidationMode::Hybrid => Some(PythonSchemaValidationBridge::new(py)?),
        };
        let future = pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let _admission = admission;
            let outcome = session
                .run_turn(prompt, schema, validation_bridge)
                .await
                .map_err(|error| Python::attach(|py| act_error(py, error)))?;
            Python::attach(|py| turn_outcome(py, outcome))
        })?;
        self.driver = Some(future.call_method0("__await__")?.unbind());
        self.phase = ActCallPhase::Running;
        Ok(())
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
                self.finish();
                Err(error)
            }
        }
    }

    fn finish(&mut self) {
        self.phase = ActCallPhase::Done;
        self.session = None;
        self.prompt = None;
        self.schema = None;
        self.driver = None;
    }
}

fn act_error(py: Python<'_>, error: AgentActError) -> PyErr {
    match error {
        AgentActError::Startup(failure) => failure.to_pyerr(py),
        AgentActError::SessionBroken(failure) => session_broken_error(py, &failure),
        AgentActError::SessionClosed => {
            session_broken_error(py, &AgentSessionFailure::transport_lost())
        }
    }
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
                    self.finish();
                    return Err(PyTypeError::new_err(
                        "can't send non-None value to a just-started coroutine",
                    ));
                }
                if let Err(error) = self.start_driver(py) {
                    self.finish();
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
                self.finish();
                Err(PyErr::from_value(exc.into_bound(py)))
            }
            ActCallPhase::Running => self.drive(py, "throw", exc.bind(py)),
            ActCallPhase::Done => Err(PyRuntimeError::new_err(REUSE_ERROR)),
        }
    }

    fn close(&mut self, py: Python<'_>) -> PyResult<()> {
        let result = self
            .driver
            .as_ref()
            .map(|driver| driver.bind(py).call_method0("close"))
            .transpose();
        self.finish();
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
        visit.call(&self.driver)?;
        if let Some(schema) = &self.schema {
            schema.traverse(&visit)?;
        }
        Ok(())
    }

    fn __clear__(&mut self) {
        self.finish();
    }
}
