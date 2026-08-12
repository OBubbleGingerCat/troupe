use std::rc::Rc;
use std::sync::Arc;

use pyo3::exceptions::{PyNotImplementedError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{
    PyAny, PyAnyMethods, PyDict, PyDictMethods, PyList, PyListMethods, PyString, PyTuple, PyType,
    PyTypeMethods,
};

use crate::agent::resolve_agent_profile;
use crate::orchestration::actor::{
    Actor, ActorCapability, ActorCapabilityNode, ActorConstruction, enter_actor_permit,
};
use crate::orchestration::actor_handle::ActorHandle;
use crate::orchestration::actor_registry::{NameKey, ProductionState};

const ACTOR_TYPE_ERROR: &str = "actor_type must be a subclass of Actor";
const ACTOR_RESULT_ERROR: &str = "actor_type did not construct the requested Actor instance";

fn validate_actor_factory_result<'py>(
    result: PyResult<Bound<'py, PyAny>>,
    construction: &Rc<ActorConstruction>,
) -> PyResult<Bound<'py, Actor>> {
    let result = result?;
    if !construction.was_consumed() {
        return Err(PyTypeError::new_err(ACTOR_RESULT_ERROR));
    }
    let actor = result
        .cast_into::<Actor>()
        .map_err(|_| PyTypeError::new_err(ACTOR_RESULT_ERROR))?;
    if !construction.matches(&actor) {
        return Err(PyTypeError::new_err(ACTOR_RESULT_ERROR));
    }
    Ok(actor)
}

fn is_pattern(value: &Bound<'_, PyAny>) -> PyResult<bool> {
    let pattern_type = value.py().import("re")?.getattr("Pattern")?;
    value.is_instance(&pattern_type)
}

fn handles_as_sorted_list(
    py: Python<'_>,
    nodes: impl IntoIterator<Item = Py<ActorCapabilityNode>>,
) -> PyResult<Py<PyAny>> {
    let handles = PyList::empty(py);
    for node in nodes {
        handles.append(Py::new(py, ActorHandle::from_node(node))?)?;
    }
    let key = py
        .import("operator")?
        .getattr("attrgetter")?
        .call1(("name",))?;
    let kwargs = PyDict::new(py);
    kwargs.set_item("key", key)?;
    Ok(py
        .import("builtins")?
        .getattr("sorted")?
        .call((handles,), Some(&kwargs))?
        .unbind())
}

fn pin_capability_nodes(
    py: Python<'_>,
    capabilities: impl IntoIterator<Item = Arc<ActorCapability>>,
) -> PyResult<Vec<Py<ActorCapabilityNode>>> {
    let mut nodes = Vec::new();
    for capability in capabilities {
        if let Some(node) = capability.node(py)? {
            nodes.push(node);
        }
    }
    Ok(nodes)
}

/// Native Production base with a synchronous constructor over raw argument tokens.
#[pyclass(name = "Production", module = "troupe", subclass)]
pub struct Production {
    state: Arc<ProductionState>,
}

#[pymethods]
impl Production {
    #[new]
    #[pyo3(signature = (args, /))]
    fn new(args: &Bound<'_, PyList>) -> PyResult<Self> {
        for arg in args.iter() {
            arg.cast::<PyString>()?;
        }
        Ok(Self {
            state: Arc::new(ProductionState::new()),
        })
    }

    #[pyo3(signature = (actor_type, *, name, agent_profile, actor_args, actor_kwargs))]
    fn cast_actor(
        slf: PyRef<'_, Self>,
        actor_type: &Bound<'_, PyAny>,
        name: &Bound<'_, PyAny>,
        agent_profile: &Bound<'_, PyAny>,
        actor_args: &Bound<'_, PyAny>,
        actor_kwargs: &Bound<'_, PyAny>,
    ) -> PyResult<Py<ActorHandle>> {
        let py = slf.py();
        slf.state.ensure_owner_process()?;
        let actor_type = actor_type
            .cast::<PyType>()
            .map_err(|_| PyTypeError::new_err(ACTOR_TYPE_ERROR))?;
        if !actor_type.is_subclass(&py.get_type::<Actor>())? {
            return Err(PyTypeError::new_err(ACTOR_TYPE_ERROR));
        }
        let name = name.cast::<PyString>()?;
        let actor_args = actor_args.cast::<PyTuple>()?;
        let actor_kwargs = actor_kwargs.cast::<PyDict>()?;
        let resolved_profile = Arc::new(resolve_agent_profile(agent_profile)?);

        let state = Arc::clone(&slf.state);
        let cast_permit = state
            .begin_agent_cast()
            .map_err(|failure| failure.to_pyerr(py))?;
        let reservation = state.reserve_name(name)?;
        let agent_launch = state
            .resolve_agent_launch(&resolved_profile)
            .map_err(|failure| failure.to_pyerr(py))?;
        let production = Py::<Production>::from(slf).into_any();
        let (construction, permit) = enter_actor_permit(
            actor_type,
            name,
            production.bind(py),
            Arc::clone(reservation.identity()),
        );
        let class_result = actor_type.call(actor_args, Some(actor_kwargs));
        drop(permit);
        let actor = validate_actor_factory_result(class_result, &construction)?;
        let actor_object = actor.clone().unbind();
        let agent_session =
            state.start_agent_session(&cast_permit, Arc::clone(&resolved_profile), agent_launch);
        let capability = Arc::new(ActorCapability::new(
            actor_object,
            name,
            reservation.key().clone(),
            Arc::clone(reservation.identity()),
            Arc::downgrade(&state),
        ));
        capability.attach_agent_session(agent_session);
        let capability_node = Py::new(py, ActorCapabilityNode::new(Arc::clone(&capability)))?;
        capability.attach_node(capability_node.bind(py))?;
        actor.borrow().attach_capability(&capability);
        let handle = Py::new(py, ActorHandle::from_node(capability_node))?;
        reservation.commit(&capability);
        Ok(handle)
    }

    #[cfg(feature = "agent-test-support")]
    fn _agent_shutdown_for_test<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let state = Arc::clone(&self.state);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            state.shutdown_agent_sessions().await;
            Ok(())
        })
    }

    #[cfg(feature = "agent-test-support")]
    fn _agent_is_shutting_down_for_test(&self) -> bool {
        self.state.agent_sessions_are_shutting_down()
    }

    #[cfg(feature = "agent-test-support")]
    fn _agent_fail_result_listener_for_test(&self) {
        self.state.fail_agent_result_listener_for_test();
    }

    #[cfg(feature = "agent-test-support")]
    fn _agent_tracked_sessions_for_test(&self) -> usize {
        self.state.tracked_agent_session_count()
    }

    #[pyo3(
        signature = (query=None, /, *, name=None, pattern=None),
        text_signature = None
    )]
    fn get_actor(
        &self,
        py: Python<'_>,
        query: Option<&Bound<'_, PyAny>>,
        name: Option<&Bound<'_, PyAny>>,
        pattern: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        match (query, name, pattern) {
            (Some(query), None, None) => {
                if let Ok(name) = query.cast::<PyString>() {
                    return self.get_actor_by_name(py, name);
                }
                if is_pattern(query)? {
                    return self.get_actor_by_pattern(py, query);
                }
                Err(PyTypeError::new_err(
                    "get_actor() expected a str or compiled re.Pattern",
                ))
            }
            (None, Some(name), None) => self.get_actor_by_name(py, name.cast::<PyString>()?),
            (None, None, Some(pattern)) if is_pattern(pattern)? => {
                self.get_actor_by_pattern(py, pattern)
            }
            (None, None, Some(_)) => Err(PyTypeError::new_err(
                "get_actor() pattern must be a compiled re.Pattern",
            )),
            _ => Err(PyTypeError::new_err(
                "get_actor() requires exactly one name or pattern",
            )),
        }
    }

    fn get_actors(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let nodes = pin_capability_nodes(py, self.state.snapshot())?;
        handles_as_sorted_list(py, nodes)
    }

    /// Acquire asynchronous resources before any scene starts.
    async fn start(_self: Py<Self>) {}

    /// Run one scene as the runtime-owned top-level asynchronous task.
    async fn scene(_self: Py<Self>) -> PyResult<()> {
        Err(PyNotImplementedError::new_err(
            "Production.scene() is not implemented",
        ))
    }

    /// Finish asynchronous work, await cleanup, and release resources after the scene.
    async fn stop(_self: Py<Self>) {}
}

impl Production {
    pub(crate) fn state(&self) -> Arc<ProductionState> {
        Arc::clone(&self.state)
    }

    fn get_actor_by_name(&self, py: Python<'_>, name: &Bound<'_, PyString>) -> PyResult<Py<PyAny>> {
        let key = NameKey::from_python(name)?;
        match self.state.get(&key) {
            Some(capability) => match ActorHandle::from_capability(py, capability)? {
                Some(handle) => Ok(Py::new(py, handle)?.into_any()),
                None => Ok(py.None()),
            },
            None => Ok(py.None()),
        }
    }

    fn get_actor_by_pattern(
        &self,
        py: Python<'_>,
        pattern: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let nodes = pin_capability_nodes(py, self.state.snapshot())?;
        let mut matches = Vec::new();
        for node in nodes {
            let Some(capability) = node.bind(py).borrow().capability() else {
                continue;
            };
            let name = capability.name(py);
            if pattern
                .call_method1("fullmatch", (name.bind(py),))?
                .is_truthy()?
            {
                matches.push(node);
            }
        }
        handles_as_sorted_list(py, matches)
    }
}

#[cfg(test)]
mod actor_factory_tests {
    use std::sync::Arc;

    use pyo3::exceptions::{PyTypeError, PyValueError};
    use pyo3::prelude::*;
    use pyo3::types::PyString;

    use crate::orchestration::actor::{Actor, ActorIdentity, enter_actor_permit};

    use super::{ACTOR_RESULT_ERROR, validate_actor_factory_result};

    #[test]
    fn factory_validates_result_only_after_successful_class_call() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let actor_type = py.get_type::<Actor>();
            let name = PyString::new(py, "transaction");
            let production = py.None().into_bound(py);
            let (construction, permit) =
                enter_actor_permit(&actor_type, &name, &production, Arc::new(ActorIdentity));
            let boom = PyValueError::new_err("class call failed");
            let boom_value = boom.value(py).clone().unbind();
            let propagated = validate_actor_factory_result(Err(boom), &construction)
                .expect_err("class-call error must propagate");
            assert!(propagated.value(py).is(boom_value.bind(py)));
            drop(permit);

            let (construction, permit) =
                enter_actor_permit(&actor_type, &name, &production, Arc::new(ActorIdentity));
            let error = validate_actor_factory_result(Ok(py.None().into_bound(py)), &construction)
                .expect_err("normal wrong return must be validated");
            assert!(error.is_instance_of::<PyTypeError>(py));
            assert_eq!(error.value(py).to_string(), ACTOR_RESULT_ERROR);
            drop(permit);
        });
    }
}
