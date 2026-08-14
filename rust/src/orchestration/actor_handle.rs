use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use pyo3::class::gc::{PyTraverseError, PyVisit};
use pyo3::exceptions::{PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict, PyString};

use crate::diagnostic_runtime::actor_producer::{self, ActorHook};
use crate::orchestration::actor::{ActorCapability, ActorCapabilityNode};
use crate::orchestration::cue::CueContextError;
use crate::orchestration::cue_future::CueCall;
use crate::orchestration::scene_context::CUE_CONTEXT_ERROR;

const HANDLE_DIRECT_ERROR: &str = "ActorHandle cannot be constructed directly";

static NEXT_HANDLE_IDENTITY: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ActorHandleIdentity(u64);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[pyclass(name = "ActorHandle", module = "troupe")]
pub struct ActorHandle {
    capability: Mutex<Option<Py<ActorCapabilityNode>>>,
    diagnostic_identity: ActorHandleIdentity,
}

impl ActorHandle {
    pub(crate) fn from_node(capability: Py<ActorCapabilityNode>) -> Self {
        let mut handle = Self {
            capability: Mutex::new(Some(capability)),
            diagnostic_identity: ActorHandleIdentity(
                NEXT_HANDLE_IDENTITY.fetch_add(1, Ordering::Relaxed),
            ),
        };
        actor_producer::observe_handle(
            handle.diagnostic_identity,
            handle
                .capability
                .get_mut()
                .expect("a fresh handle capability mutex is not poisoned")
                .as_ref()
                .expect("a fresh handle retains its capability"),
            ActorHook::HandleCreated,
        );
        handle
    }

    pub(crate) fn from_capability(
        py: Python<'_>,
        capability: Arc<ActorCapability>,
    ) -> PyResult<Option<Self>> {
        Ok(capability.node(py)?.map(Self::from_node))
    }

    fn clear_capability(&self) {
        let capability = lock(&self.capability).take();
        if let Some(capability) = &capability {
            actor_producer::observe_handle(
                self.diagnostic_identity,
                capability,
                ActorHook::HandleCleared,
            );
        }
        drop(capability);
    }

    fn capability(&self, py: Python<'_>) -> PyResult<Arc<ActorCapability>> {
        let node = lock(&self.capability)
            .as_ref()
            .map(|node| node.clone_ref(py))
            .ok_or_else(|| PyRuntimeError::new_err("ActorHandle is no longer attached"))?;
        let capability = node.bind(py).borrow().capability();
        capability.ok_or_else(|| PyRuntimeError::new_err("ActorHandle is no longer attached"))
    }

    #[allow(dead_code)]
    pub(crate) fn diagnostic_identity(&self) -> ActorHandleIdentity {
        self.diagnostic_identity
    }
}

#[pymethods]
impl ActorHandle {
    #[new]
    fn new() -> PyResult<Self> {
        Err(PyTypeError::new_err(HANDLE_DIRECT_ERROR))
    }

    #[getter]
    fn name(&self, py: Python<'_>) -> PyResult<Py<PyString>> {
        Ok(self.capability(py)?.name(py))
    }

    fn cue(slf: PyRef<'_, Self>, instruction: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let py = slf.py();
        let capability = slf.capability(py)?;
        let state = capability
            .production_state()
            .ok_or_else(|| CueContextError::new_err(CUE_CONTEXT_ERROR))?;
        let binding = state.active_binding_for_cue()?;
        if !binding.production_matches(&state) {
            return Err(CueContextError::new_err(CUE_CONTEXT_ERROR));
        }
        let lineage = binding
            .current_lineage(py)?
            .filter(crate::orchestration::python_task::TaskLineage::is_active)
            .ok_or_else(|| CueContextError::new_err(CUE_CONTEXT_ERROR))?;
        let scene = lineage
            .scene()
            .filter(|scene| scene.is_open())
            .ok_or_else(|| CueContextError::new_err(CUE_CONTEXT_ERROR))?;
        let instruction = instruction.cast::<PyDict>()?;
        let handle = Py::<ActorHandle>::from(slf).into_any();
        Ok(Py::new(
            py,
            CueCall::new(
                handle,
                instruction.clone().unbind(),
                capability,
                &binding,
                &scene,
            ),
        )?
        .into_any())
    }

    #[cfg(feature = "agent-test-support")]
    fn _agent_state_for_test(&self, py: Python<'_>) -> PyResult<&'static str> {
        self.capability(py)?
            .agent_session()
            .map(|session| session.state_name())
            .ok_or_else(|| PyRuntimeError::new_err("Actor has no agent session"))
    }

    #[cfg(feature = "agent-test-support")]
    fn _agent_mode_transition_for_test(&self, py: Python<'_>) -> PyResult<&'static str> {
        self.capability(py)?
            .agent_session()
            .map(|session| session.mode_transition_name_for_test())
            .ok_or_else(|| PyRuntimeError::new_err("Actor has no agent session"))
    }

    #[cfg(feature = "agent-test-support")]
    fn _agent_has_queued_turn_for_test(&self, py: Python<'_>) -> PyResult<bool> {
        self.capability(py)?
            .agent_session()
            .map(|session| session.has_queued_turn_for_test())
            .ok_or_else(|| PyRuntimeError::new_err("Actor has no agent session"))
    }

    #[cfg(feature = "agent-test-support")]
    fn _agent_fail_transport_for_test(&self, py: Python<'_>) -> PyResult<()> {
        let session = self
            .capability(py)?
            .agent_session()
            .ok_or_else(|| PyRuntimeError::new_err("Actor has no agent session"))?;
        py.detach(move || session.commit_transport_loss());
        Ok(())
    }

    #[cfg(feature = "agent-test-support")]
    fn _agent_ready_for_test<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let session = self
            .capability(py)?
            .agent_session()
            .ok_or_else(|| PyRuntimeError::new_err("Actor has no agent session"))?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let snapshot = session
                .readiness_for_test()
                .await
                .map_err(|failure| Python::attach(|py| failure.to_pyerr(py)))?;
            Python::attach(|py| {
                let value = PyDict::new(py);
                value.set_item("state", "ready")?;
                value.set_item("pid", snapshot.pid)?;
                value.set_item("session_id", &snapshot.session_id)?;
                let agent_info = match &snapshot.agent_info {
                    Some(agent_info) => {
                        let info = PyDict::new(py);
                        info.set_item("name", &agent_info.name)?;
                        info.set_item("title", &agent_info.title)?;
                        info.set_item("version", &agent_info.version)?;
                        info.into_any()
                    }
                    None => py.None().into_bound(py),
                };
                value.set_item("agent_info", agent_info)?;
                let capabilities = PyDict::new(py);
                capabilities.set_item("load_session", snapshot.load_session)?;
                capabilities.set_item("mcp_http", snapshot.mcp_http)?;
                value.set_item("capabilities", capabilities)?;
                value.set_item("generation", snapshot.generation)?;
                value.set_item("server_name", &snapshot.server_name)?;
                value.set_item("endpoint", &snapshot.endpoint)?;
                value.set_item("model", &snapshot.effective_model)?;
                value.set_item("effort", &snapshot.effective_effort)?;
                Ok(value.into_any().unbind())
            })
        })
    }

    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&*lock(&self.capability))
    }

    fn __clear__(&self) {
        self.clear_capability();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Weak};

    use pyo3::prelude::*;
    use pyo3::types::{PyDict, PyString, PyWeakrefMethods, PyWeakrefReference};

    use crate::orchestration::actor::{
        Actor, ActorCapability, ActorCapabilityNode, ActorIdentity, enter_actor_permit,
    };
    use crate::orchestration::actor_registry::{NameKey, ProductionState};
    use crate::orchestration::cue::CueContextError;
    use crate::orchestration::scene_context::{
        AdmissionMetrics, admission_metrics_for_test, reset_admission_metrics_for_test,
    };

    use super::ActorHandle;

    #[test]
    fn handle_clear_is_idempotent() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let actor_type = py.get_type::<Actor>();
            let name = PyString::new(py, "handle-clear");
            let production = py.None().into_bound(py);
            let identity = Arc::new(ActorIdentity);
            let (_, permit) =
                enter_actor_permit(&actor_type, &name, &production, Arc::clone(&identity));
            let actor = actor_type.call0()?.cast_into::<Actor>()?.unbind();
            drop(permit);
            let capability = Arc::new(ActorCapability::new(
                actor,
                &name,
                NameKey::from_python(&name)?,
                identity,
                Weak::<ProductionState>::new(),
            ));
            let node = Py::new(py, ActorCapabilityNode::new(Arc::clone(&capability)))?;
            capability.attach_node(node.bind(py))?;
            let node_reference = PyWeakrefReference::new(node.bind(py).as_any())?.unbind();
            let handle = ActorHandle::from_node(node);
            assert_eq!(Arc::strong_count(&capability), 2);
            handle.clear_capability();
            handle.clear_capability();
            assert_eq!(Arc::strong_count(&capability), 1);
            assert!(node_reference.bind(py).upgrade().is_none());
            Ok::<_, PyErr>(())
        })
        .expect("private ActorHandle construction must succeed");
    }

    #[test]
    fn invalid_context_preflight_has_zero_native_side_effects() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            reset_admission_metrics_for_test();
            let actor_type = py.get_type::<Actor>();
            let name = PyString::new(py, "invalid-preflight");
            let production = py.None().into_bound(py);
            let identity = Arc::new(ActorIdentity);
            let (_, permit) =
                enter_actor_permit(&actor_type, &name, &production, Arc::clone(&identity));
            let actor = actor_type.call0()?.cast_into::<Actor>()?.unbind();
            drop(permit);
            let capability = Arc::new(ActorCapability::new(
                actor,
                &name,
                NameKey::from_python(&name)?,
                identity,
                Weak::<ProductionState>::new(),
            ));
            let node = Py::new(py, ActorCapabilityNode::new(Arc::clone(&capability)))?;
            capability.attach_node(node.bind(py))?;
            let handle = Py::new(py, ActorHandle::from_node(node))?;

            let error = handle
                .bind(py)
                .call_method1("cue", (PyDict::new(py),))
                .expect_err("runtime-external cue must fail synchronously");
            assert!(error.is_instance_of::<CueContextError>(py));
            assert_eq!(admission_metrics_for_test(), AdmissionMetrics::default());
            Ok::<_, PyErr>(())
        })
        .expect("invalid cue preflight must remain side-effect free");
    }
}
