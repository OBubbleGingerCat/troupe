use std::sync::{Arc, Mutex, MutexGuard};

use pyo3::class::gc::{PyTraverseError, PyVisit};
use pyo3::exceptions::{PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict, PyString};

use crate::actor::{ActorCapability, ActorCapabilityNode};
use crate::cue::CueContextError;
use crate::cue_future::CueCall;
use crate::scene_context::CUE_CONTEXT_ERROR;

const HANDLE_DIRECT_ERROR: &str = "ActorHandle cannot be constructed directly";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[pyclass(name = "ActorHandle", module = "troupe")]
pub struct ActorHandle {
    capability: Mutex<Option<Py<ActorCapabilityNode>>>,
}

impl ActorHandle {
    pub(crate) fn from_node(capability: Py<ActorCapabilityNode>) -> Self {
        Self {
            capability: Mutex::new(Some(capability)),
        }
    }

    pub(crate) fn from_capability(
        py: Python<'_>,
        capability: Arc<ActorCapability>,
    ) -> PyResult<Option<Self>> {
        Ok(capability.node(py)?.map(Self::from_node))
    }

    fn clear_capability(&self) {
        let capability = lock(&self.capability).take();
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
            .filter(crate::python_task::TaskLineage::is_active)
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

    use crate::actor::{
        Actor, ActorCapability, ActorCapabilityNode, ActorIdentity, enter_actor_permit,
    };
    use crate::actor_registry::{NameKey, ProductionState};
    use crate::cue::CueContextError;
    use crate::scene_context::{
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
