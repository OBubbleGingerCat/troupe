use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use pyo3::class::gc::{PyTraverseError, PyVisit};
use pyo3::exceptions::{PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict, PyString, PyTuple, PyType, PyWeakrefReference};

use crate::actor_registry::{NameKey, ProductionState};
use crate::cue::Cue;
use crate::effect::{Effect, EffectContextError, construct_effect};
use crate::mailbox::{
    CueOperation, DispatchOutcome, Mailbox, MailboxTerminalTransition, TerminalAction,
};

pub(crate) const ACTOR_DIRECT_ERROR: &str =
    "Actor instances can only be created by Production.cast_actor()";

#[derive(Debug)]
pub(crate) struct ActorIdentity;

pub(crate) struct ActorConstruction {
    actor_type: Py<PyType>,
    name: Py<PyString>,
    production: Py<PyAny>,
    identity: Arc<ActorIdentity>,
    consumed: Cell<bool>,
}

impl ActorConstruction {
    fn new(
        actor_type: Py<PyType>,
        name: Py<PyString>,
        production: Py<PyAny>,
        identity: Arc<ActorIdentity>,
    ) -> Self {
        Self {
            actor_type,
            name,
            production,
            identity,
            consumed: Cell::new(false),
        }
    }

    pub(crate) fn was_consumed(&self) -> bool {
        self.consumed.get()
    }

    pub(crate) fn matches(&self, actor: &Bound<'_, Actor>) -> bool {
        Arc::ptr_eq(&self.identity, &actor.borrow().identity)
    }
}

thread_local! {
    static ACTOR_PERMITS: RefCell<Vec<Rc<ActorConstruction>>> = const { RefCell::new(Vec::new()) };
}

pub(crate) struct ActorPermitGuard {
    construction: Rc<ActorConstruction>,
}

impl Drop for ActorPermitGuard {
    fn drop(&mut self) {
        ACTOR_PERMITS.with(|permits| {
            let popped = permits
                .borrow_mut()
                .pop()
                .expect("Actor permit stack must contain its active guard");
            assert!(
                Rc::ptr_eq(&popped, &self.construction),
                "Actor permit guards must be dropped in LIFO order"
            );
        });
    }
}

pub(crate) fn enter_actor_permit(
    actor_type: &Bound<'_, PyType>,
    name: &Bound<'_, PyString>,
    production: &Bound<'_, PyAny>,
    identity: Arc<ActorIdentity>,
) -> (Rc<ActorConstruction>, ActorPermitGuard) {
    let construction = Rc::new(ActorConstruction::new(
        actor_type.clone().unbind(),
        name.clone().unbind(),
        production.clone().unbind(),
        identity,
    ));
    ACTOR_PERMITS.with(|permits| permits.borrow_mut().push(Rc::clone(&construction)));
    let guard = ActorPermitGuard {
        construction: Rc::clone(&construction),
    };
    (construction, guard)
}

fn consume_actor_permit(cls: &Bound<'_, PyType>) -> PyResult<Actor> {
    let construction = ACTOR_PERMITS.with(|permits| permits.borrow().last().cloned());
    let Some(construction) = construction else {
        return Err(PyTypeError::new_err(ACTOR_DIRECT_ERROR));
    };
    if !construction.actor_type.bind(cls.py()).is(cls) || construction.consumed.replace(true) {
        return Err(PyTypeError::new_err(ACTOR_DIRECT_ERROR));
    }

    Ok(Actor {
        name: construction.name.clone_ref(cls.py()),
        production: Mutex::new(Some(construction.production.clone_ref(cls.py()))),
        identity: Arc::clone(&construction.identity),
        capability: Mutex::new(Weak::new()),
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[pyclass(name = "Actor", module = "troupe", subclass)]
pub struct Actor {
    name: Py<PyString>,
    production: Mutex<Option<Py<PyAny>>>,
    identity: Arc<ActorIdentity>,
    capability: Mutex<Weak<ActorCapability>>,
}

impl Actor {
    pub(crate) fn attach_capability(&self, capability: &Arc<ActorCapability>) {
        *lock(&self.capability) = Arc::downgrade(capability);
    }

    fn clear_runtime_edges(&self) {
        let production = lock(&self.production).take();
        *lock(&self.capability) = Weak::new();
        drop(production);
    }
}

#[pymethods]
impl Actor {
    #[new]
    #[classmethod]
    #[pyo3(signature = (*args, **kwargs))]
    fn new(
        cls: &Bound<'_, PyType>,
        args: &Bound<'_, PyTuple>,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Self> {
        let _ = (args, kwargs);
        consume_actor_permit(cls)
    }

    #[getter]
    fn name(&self, py: Python<'_>) -> Py<PyString> {
        self.name.clone_ref(py)
    }

    #[getter]
    fn production(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        lock(&self.production)
            .as_ref()
            .map(|production| production.clone_ref(py))
            .ok_or_else(|| PyRuntimeError::new_err("Actor is no longer attached"))
    }

    #[pyo3(signature = (effect_type, *, effect_args, effect_kwargs))]
    fn make_effect(
        &self,
        effect_type: &Bound<'_, PyAny>,
        effect_args: &Bound<'_, PyAny>,
        effect_kwargs: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        const CONTEXT_ERROR: &str = "Actor.make_effect() must be called on the current actor within its active cued context";
        const TYPE_ERROR: &str = "effect_type must be a subclass of Effect";
        let context_error = || EffectContextError::new_err(CONTEXT_ERROR);
        let capability = lock(&self.capability).upgrade().ok_or_else(context_error)?;
        if !Arc::ptr_eq(&capability.identity, &self.identity) {
            return Err(context_error());
        }
        let state = capability.production_state().ok_or_else(context_error)?;
        let binding = state
            .active_binding_for_cue()
            .map_err(|_| context_error())?;
        let lineage = binding
            .current_lineage(effect_type.py())
            .map_err(|_| context_error())?;
        let cued = lineage
            .filter(crate::python_task::TaskLineage::is_active)
            .and_then(|lineage| lineage.cued())
            .filter(|cued| {
                cued.is_active() && cued.actor_identity() == capability.identity_address()
            })
            .ok_or_else(context_error)?;

        let effect_type = effect_type
            .cast::<PyType>()
            .map_err(|_| PyTypeError::new_err(TYPE_ERROR))?;
        if !effect_type.is_subclass(&effect_type.py().get_type::<Effect>())? {
            return Err(PyTypeError::new_err(TYPE_ERROR));
        }
        let effect_args = effect_args.cast::<PyTuple>()?;
        let effect_kwargs = effect_kwargs.cast::<PyDict>()?;
        let id = cued.next_effect_id(effect_type.py())?;
        construct_effect(
            effect_type,
            effect_args,
            effect_kwargs,
            id,
            cued.source(effect_type.py()),
        )
    }

    async fn cued(_self: Py<Self>, cue: Py<Cue>) -> PyResult<Py<PyTuple>> {
        let _ = cue;
        Python::attach(|py| Ok(PyTuple::empty(py).unbind()))
    }

    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.name)?;
        visit.call(&*lock(&self.production))
    }

    fn __clear__(&self) {
        self.clear_runtime_edges();
    }
}

pub(crate) struct ActorCapability {
    actor: Py<Actor>,
    name: Py<PyString>,
    key: NameKey,
    identity: Arc<ActorIdentity>,
    production_state: Weak<ProductionState>,
    node: Mutex<Option<Py<PyWeakrefReference>>>,
    mailbox: Mutex<Mailbox>,
}

impl ActorCapability {
    pub(crate) fn new(
        actor: Py<Actor>,
        name: &Bound<'_, PyString>,
        key: NameKey,
        identity: Arc<ActorIdentity>,
        production_state: Weak<ProductionState>,
    ) -> Self {
        Self {
            actor,
            name: name.clone().unbind(),
            key,
            identity,
            production_state,
            node: Mutex::new(None),
            mailbox: Mutex::new(Mailbox::default()),
        }
    }

    pub(crate) fn attach_node(&self, node: &Bound<'_, ActorCapabilityNode>) -> PyResult<()> {
        let reference = PyWeakrefReference::new(node.as_any())?.unbind();
        let previous = lock(&self.node).replace(reference);
        assert!(
            previous.is_none(),
            "an Actor capability node is attached once"
        );
        drop(previous);
        Ok(())
    }

    pub(crate) fn node(&self, py: Python<'_>) -> PyResult<Option<Py<ActorCapabilityNode>>> {
        let reference = lock(&self.node)
            .as_ref()
            .map(|reference| reference.clone_ref(py));
        let Some(reference) = reference else {
            return Ok(None);
        };
        Ok(reference
            .bind(py)
            .upgrade_as::<ActorCapabilityNode>()?
            .map(|node| node.unbind()))
    }

    pub(crate) fn name(&self, py: Python<'_>) -> Py<PyString> {
        self.name.clone_ref(py)
    }

    pub(crate) fn actor(&self, py: Python<'_>) -> Py<Actor> {
        self.actor.clone_ref(py)
    }

    pub(crate) fn production_state(&self) -> Option<Arc<ProductionState>> {
        self.production_state.upgrade()
    }

    pub(crate) fn identity_address(&self) -> usize {
        Arc::as_ptr(&self.identity) as usize
    }

    pub(crate) fn source_name_snapshot(&self, py: Python<'_>) -> PyResult<Py<PyString>> {
        let length = unsafe { pyo3::ffi::PyUnicode_GetLength(self.name.as_ptr()) };
        if length < 0 {
            return Err(PyErr::fetch(py));
        }
        let value = unsafe {
            Bound::<PyAny>::from_owned_ptr_or_err(
                py,
                pyo3::ffi::PyUnicode_Substring(self.name.as_ptr(), 0, length),
            )
        }?
        .cast_into::<PyString>()?;
        Ok(value.unbind())
    }

    pub(crate) fn enqueue_operation(&self, operation: CueOperation) -> PyResult<()> {
        let start = {
            let mut mailbox = lock(&self.mailbox);
            mailbox.enqueue(operation);
            mailbox.claim_next_if_idle()
        };
        if let Some(operation) = start {
            self.drain_from(operation);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn finish_operation(&self, operation: &CueOperation) {
        if let Some(next) = self.complete_and_take_next(operation) {
            self.drain_from(next);
        }
    }

    fn drain_from(&self, operation: CueOperation) {
        match operation.dispatch() {
            DispatchOutcome::Scheduled => {}
            DispatchOutcome::Terminal(action) => {
                self.finish_terminal_action(operation, action);
            }
        }
    }

    pub(crate) fn unlink_terminal_operation(
        &self,
        operation: &CueOperation,
    ) -> MailboxTerminalTransition {
        lock(&self.mailbox).terminal_transition(operation)
    }

    pub(crate) fn finish_terminal_action(
        &self,
        mut operation: CueOperation,
        mut action: TerminalAction,
    ) {
        loop {
            let delivery = operation.complete_terminal_action(action, Some(self));
            let Some(next) = delivery.next else {
                CueOperation::resolve_terminal_signal(delivery.signal);
                return;
            };
            match next.dispatch() {
                DispatchOutcome::Scheduled => {
                    CueOperation::resolve_terminal_signal(delivery.signal);
                    return;
                }
                DispatchOutcome::Terminal(next_action) => {
                    CueOperation::resolve_terminal_signal(delivery.signal);
                    operation = next;
                    action = next_action;
                }
            }
        }
    }

    #[cfg(test)]
    fn complete_and_take_next(&self, operation: &CueOperation) -> Option<CueOperation> {
        let transition = {
            let mut mailbox = lock(&self.mailbox);
            mailbox.complete_running(operation)
        };
        let (retired, next) = transition?;
        drop(retired);
        next
    }

    pub(crate) fn traverse(&self, visit: &PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.actor)?;
        visit.call(&self.name)?;
        visit.call(&*lock(&self.node))?;
        let mailbox = lock(&self.mailbox);
        for operation in &mailbox.queue {
            operation.traverse(visit)?;
        }
        if let Some(running) = &mailbox.running {
            running.operation.traverse(visit)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn install_running_for_test(&self, operation: CueOperation) {
        lock(&self.mailbox).running = Some(crate::mailbox::Running::new(operation));
    }

    #[cfg(test)]
    pub(crate) fn queued_contains_for_test(&self, operation: &CueOperation) -> bool {
        lock(&self.mailbox)
            .queue
            .iter()
            .any(|queued| queued.ptr_eq(operation))
    }

    #[cfg(test)]
    pub(crate) fn queue_empty_and_running_exact_for_test(&self, operation: &CueOperation) -> bool {
        let mailbox = lock(&self.mailbox);
        mailbox.queue.is_empty()
            && mailbox
                .running
                .as_ref()
                .is_some_and(|current| current.operation.ptr_eq(operation))
    }

    #[cfg(test)]
    pub(crate) fn queue_only_contains_and_running_exact_for_test(
        &self,
        running: &CueOperation,
        queued: &CueOperation,
    ) -> bool {
        let mailbox = lock(&self.mailbox);
        mailbox.queue.len() == 1
            && mailbox.queue[0].ptr_eq(queued)
            && mailbox
                .running
                .as_ref()
                .is_some_and(|current| current.operation.ptr_eq(running))
    }

    #[cfg(test)]
    pub(crate) fn mailbox_lock_available_for_test(&self) -> bool {
        self.mailbox.try_lock().is_ok()
    }

    #[cfg(test)]
    pub(crate) fn operation_absent_for_test(&self, operation: &CueOperation) -> bool {
        let mailbox = lock(&self.mailbox);
        !mailbox.queue.iter().any(|queued| queued.ptr_eq(operation))
            && !mailbox
                .running
                .as_ref()
                .is_some_and(|running| running.operation.ptr_eq(operation))
    }

    #[cfg(test)]
    pub(crate) fn retire_running_for_test(&self, operation: &CueOperation) -> bool {
        let transition = lock(&self.mailbox).complete_running(operation);
        drop(transition);
        lock(&self.mailbox).running.is_none()
    }
}

impl Drop for ActorCapability {
    fn drop(&mut self) {
        if let Some(state) = self.production_state.upgrade() {
            state.detach(&self.key, &self.identity);
        }
    }
}

#[pyclass(module = "troupe._runtime", weakref)]
pub(crate) struct ActorCapabilityNode {
    capability: Mutex<Option<Arc<ActorCapability>>>,
}

impl ActorCapabilityNode {
    pub(crate) fn new(capability: Arc<ActorCapability>) -> Self {
        Self {
            capability: Mutex::new(Some(capability)),
        }
    }

    pub(crate) fn capability(&self) -> Option<Arc<ActorCapability>> {
        lock(&self.capability).as_ref().map(Arc::clone)
    }

    fn clear_capability(&self) {
        let capability = lock(&self.capability).take();
        drop(capability);
    }
}

#[pymethods]
impl ActorCapabilityNode {
    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        if let Some(capability) = lock(&self.capability).as_ref() {
            capability.traverse(&visit)?;
        }
        Ok(())
    }

    fn __clear__(&self) {
        self.clear_capability();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Barrier, Mutex, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    };
    use std::thread;

    use pyo3::exceptions::PyRuntimeError;
    use pyo3::prelude::*;
    use pyo3::types::{
        PyCFunction, PyCapsule, PyDict, PyDictMethods, PyList, PyModule, PyString, PyTuple, PyType,
        PyWeakrefReference,
    };

    use crate::actor_handle::ActorHandle;
    use crate::actor_registry::{NameKey, ProductionState};
    use crate::cue::Cue;
    use crate::mailbox::{CueOperation, CueOperationInner, Mailbox, Running};
    use crate::scene_context::{CuedScope, RunBinding, SceneScope};

    use super::{
        ACTOR_DIRECT_ERROR, Actor, ActorCapability, ActorCapabilityNode, ActorIdentity,
        consume_actor_permit, enter_actor_permit, lock,
    };

    fn type_named<'py>(py: Python<'py>, name: &str) -> Bound<'py, PyType> {
        py.import("builtins")
            .expect("builtins must import")
            .getattr(name)
            .expect("builtin type must exist")
            .cast_into::<PyType>()
            .expect("builtin object must be a type")
    }

    fn capability_for_type(
        py: Python<'_>,
        actor_type: &Bound<'_, PyType>,
        name: &str,
    ) -> PyResult<Arc<ActorCapability>> {
        let name = PyString::new(py, name);
        let production = py.None().into_bound(py);
        let identity = Arc::new(ActorIdentity);
        let (_, permit) = enter_actor_permit(actor_type, &name, &production, Arc::clone(&identity));
        let actor = actor_type.call0()?.cast_into::<Actor>()?.unbind();
        drop(permit);
        let capability = Arc::new(ActorCapability::new(
            actor,
            &name,
            NameKey::from_python(&name)?,
            identity,
            Weak::new(),
        ));
        capability
            .actor(py)
            .bind(py)
            .borrow()
            .attach_capability(&capability);
        Ok(capability)
    }

    #[test]
    fn permit_stack_is_lifo_exact_and_single_use() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let outer_type = type_named(py, "int");
            let inner_type = type_named(py, "str");
            let outer_name = PyString::new(py, "outer");
            let inner_name = PyString::new(py, "inner");
            let production = py.None().into_bound(py);
            let (_, outer_guard) = enter_actor_permit(
                &outer_type,
                &outer_name,
                &production,
                Arc::new(ActorIdentity),
            );
            let (_, inner_guard) = enter_actor_permit(
                &inner_type,
                &inner_name,
                &production,
                Arc::new(ActorIdentity),
            );

            let mismatch = match consume_actor_permit(&outer_type) {
                Ok(_) => panic!("top permit must be exact"),
                Err(error) => error,
            };
            assert_eq!(
                mismatch.to_string(),
                format!("TypeError: {ACTOR_DIRECT_ERROR}")
            );
            consume_actor_permit(&inner_type).expect("matching inner permit must be consumed");
            assert!(consume_actor_permit(&inner_type).is_err());
            drop(inner_guard);
            consume_actor_permit(&outer_type).expect("outer permit must be restored");
            drop(outer_guard);
            assert!(consume_actor_permit(&outer_type).is_err());
        });
    }

    #[test]
    fn base_actor_cued_awaits_to_empty_tuple() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let actor_type = py.get_type::<Actor>();
            let name = PyString::new(py, "base");
            let production = py.None().into_bound(py);
            let (_, guard) =
                enter_actor_permit(&actor_type, &name, &production, Arc::new(ActorIdentity));
            let actor = Py::new(
                py,
                consume_actor_permit(&actor_type).expect("private permit must construct Actor"),
            )?;
            drop(guard);
            let cue = Py::new(py, Cue::new_runtime_for_test(py))?;
            let awaitable = actor.bind(py).call_method1("cued", (cue,))?;
            let event_loop = py.import("asyncio")?.call_method0("new_event_loop")?;
            let result = event_loop.call_method1("run_until_complete", (awaitable,))?;
            event_loop.call_method0("close")?;
            let tuple = result.cast::<PyTuple>()?;
            assert!(tuple.is_exact_instance_of::<PyTuple>());
            assert!(tuple.is_empty());
            Ok::<_, PyErr>(())
        })
        .expect("Python-visible base cued coroutine must succeed");
    }

    #[test]
    fn actor_clear_is_idempotent() {
        struct LockProbe {
            mutex_address: usize,
            dropped: Arc<AtomicBool>,
            observed_unlocked: Arc<AtomicBool>,
        }

        impl Drop for LockProbe {
            fn drop(&mut self) {
                self.dropped.store(true, Ordering::SeqCst);
                // The Actor remains boxed until after this capsule destructor runs.
                let mutex = unsafe { &*(self.mutex_address as *const Mutex<Option<Py<PyAny>>>) };
                self.observed_unlocked
                    .store(mutex.try_lock().is_ok(), Ordering::SeqCst);
            }
        }

        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let dropped = Arc::new(AtomicBool::new(false));
            let observed_unlocked = Arc::new(AtomicBool::new(false));
            let actor = Box::new(Actor {
                name: PyString::new(py, "clear").unbind(),
                production: Mutex::new(None),
                identity: Arc::new(ActorIdentity),
                capability: Mutex::new(Weak::new()),
            });
            let mutex_address = std::ptr::from_ref(&actor.production) as usize;
            let capsule = PyCapsule::new_with_value(
                py,
                LockProbe {
                    mutex_address,
                    dropped: Arc::clone(&dropped),
                    observed_unlocked: Arc::clone(&observed_unlocked),
                },
                c"troupe.actor.clear_probe",
            )?
            .into_any()
            .unbind();
            *actor.production.lock().expect("lock") = Some(capsule);
            assert!(actor.production.lock().expect("lock").is_some());
            actor.clear_runtime_edges();
            actor.clear_runtime_edges();
            assert!(actor.production.lock().expect("lock").is_none());
            assert!(dropped.load(Ordering::SeqCst));
            assert!(observed_unlocked.load(Ordering::SeqCst));
            Ok::<_, PyErr>(())
        })
        .expect("clear probe must run");
    }

    #[test]
    fn capability_mailbox_operation_python_cycle_is_collectable() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let actor_type = py.get_type::<Actor>();
            let name = PyString::new(py, "operation-cycle");
            let production = py.None().into_bound(py);
            let identity = Arc::new(ActorIdentity);
            let (_, permit) =
                enter_actor_permit(&actor_type, &name, &production, Arc::clone(&identity));
            let actor = actor_type.call0()?.cast_into::<Actor>()?.unbind();
            drop(permit);

            let state = Arc::new(ProductionState::new());
            let capability = Arc::new(ActorCapability::new(
                actor,
                &name,
                NameKey::from_python(&name)?,
                identity,
                Arc::downgrade(&state),
            ));
            let node = Py::new(py, ActorCapabilityNode::new(Arc::clone(&capability)))?;
            capability.attach_node(node.bind(py))?;
            let handle = Py::new(py, ActorHandle::from_node(node.clone_ref(py)))?;

            let marker_type = PyModule::from_code(
                py,
                c"class Marker: pass\n",
                c"operation_cycle_test.py",
                c"operation_cycle_test",
            )?
            .getattr("Marker")?;
            let marker = marker_type.call0()?;
            let marker_ref = PyWeakrefReference::new(&marker)?.unbind();
            let node_ref = PyWeakrefReference::new(node.bind(py).as_any())?.unbind();
            let instruction = PyDict::new(py);
            instruction.set_item("handle", handle.bind(py))?;
            instruction.set_item("marker", &marker)?;
            let cue = Py::new(
                py,
                Cue::new_runtime(
                    PyString::new(py, "scene-cycle-cue0").unbind(),
                    instruction.clone().unbind().into_any(),
                    PyString::new(py, "scene-cycle").unbind(),
                ),
            )?;
            let binding = Arc::new(RunBinding::new_for_test(py)?);
            let scene = SceneScope::zero_for_binding_for_test(py, "scene-cycle", &binding)?;
            let cued = CuedScope::new(
                py,
                Arc::clone(&scene),
                PyString::new(py, "operation-cycle").unbind(),
                capability.identity_address(),
                PyString::new(py, "scene-cycle-cue0").unbind(),
            )?;
            let operation =
                CueOperation::new_runtime(&scene, &capability, &binding, cued, cue, py.None());
            lock(&capability.mailbox).enqueue(operation.clone());

            drop((operation, scene, binding, capability, state));
            drop((handle, node, instruction, marker));
            py.import("gc")?.call_method0("collect")?;

            assert!(marker_ref.bind(py).call0()?.is_none());
            assert!(node_ref.bind(py).call0()?.is_none());
            Ok::<_, PyErr>(())
        })
        .expect("mailbox operation Python edges must participate in cyclic GC");
    }

    #[test]
    fn actor_iteratively_drains_a_long_synchronous_failure_queue() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let capability = capability_for_type(py, &py.get_type::<Actor>(), "iterative-drain")?;
            let scene = SceneScope::zero_for_test(py, "scene-iterative-drain")?;
            let current = CueOperation::new_for_actor_for_test(0, &capability, &scene);
            current.finish_cancel();
            let queued: Vec<_> = (1..=10_000)
                .map(|index| CueOperation::new_for_actor_for_test(index, &capability, &scene))
                .collect();
            {
                let mut mailbox = lock(&capability.mailbox);
                mailbox.running = Some(Running::new(current.clone()));
                for operation in &queued {
                    mailbox.enqueue(operation.clone());
                }
            }

            capability.finish_operation(&current);

            assert!(queued.iter().all(CueOperation::is_terminal));
            let mailbox = lock(&capability.mailbox);
            assert!(mailbox.queue.is_empty());
            assert!(mailbox.running.is_none());
            Ok::<_, PyErr>(())
        })
        .expect("synchronous dispatch failures must drain without recursive growth");
    }

    #[test]
    fn running_terminal_action_drops_unlocked_dispatches_successor_then_signals() {
        struct MailboxDropProbe {
            mailbox_address: Arc<AtomicUsize>,
            scene_slot: Arc<Mutex<Option<Weak<SceneScope>>>>,
            current_slot: Arc<Mutex<Option<Weak<CueOperationInner>>>>,
            successor_slot: Arc<Mutex<Option<Weak<CueOperationInner>>>>,
            dropped: Arc<AtomicBool>,
            observed_unlocked: Arc<AtomicBool>,
            observed_mailbox_retired: Arc<AtomicBool>,
            observed_scene_deregistered: Arc<AtomicBool>,
        }

        impl Drop for MailboxDropProbe {
            fn drop(&mut self) {
                let address = self.mailbox_address.load(Ordering::SeqCst);
                assert_ne!(address, 0);
                let mailbox = unsafe { &*(address as *const Mutex<Mailbox>) };
                let successor = lock(&self.successor_slot)
                    .as_ref()
                    .cloned()
                    .expect("successor identity must be installed before owner drop");
                let (mailbox_unlocked, mailbox_retired) = match mailbox.try_lock() {
                    Ok(mailbox) => (
                        true,
                        mailbox.queue.is_empty()
                            && mailbox.running.as_ref().is_some_and(|running| {
                                running.operation.downgrade_for_test().ptr_eq(&successor)
                            }),
                    ),
                    Err(_) => (false, false),
                };
                let current = lock(&self.current_slot)
                    .as_ref()
                    .cloned()
                    .expect("current identity must be installed before owner drop");
                let scene = lock(&self.scene_slot)
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .expect("Scene must remain available during owner drop");
                let scene_deregistered = !scene.operation_registered_weak_for_test(&current)
                    && scene.operation_registered_weak_for_test(&successor);
                self.dropped.store(true, Ordering::SeqCst);
                self.observed_unlocked
                    .store(mailbox_unlocked, Ordering::SeqCst);
                self.observed_mailbox_retired
                    .store(mailbox_retired, Ordering::SeqCst);
                self.observed_scene_deregistered
                    .store(scene_deregistered, Ordering::SeqCst);
            }
        }

        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let mailbox_address = Arc::new(AtomicUsize::new(0));
            let lookup_observed_unlocked = Arc::new(AtomicBool::new(false));
            let dropped = Arc::new(AtomicBool::new(false));
            let drop_observed_unlocked = Arc::new(AtomicBool::new(false));
            let drop_observed_mailbox_retired = Arc::new(AtomicBool::new(false));
            let drop_observed_scene_deregistered = Arc::new(AtomicBool::new(false));
            let signal_slot = Arc::new(Mutex::new(None::<Py<PyAny>>));
            let scene_slot = Arc::new(Mutex::new(None::<Weak<SceneScope>>));
            let current_slot = Arc::new(Mutex::new(None::<Weak<CueOperationInner>>));
            let successor_slot = Arc::new(Mutex::new(None::<Weak<CueOperationInner>>));
            let successor_observed_signal_pending = Arc::new(AtomicBool::new(false));
            let successor_observed_scene_deregistered = Arc::new(AtomicBool::new(false));
            let successor_observed_owner_dropped = Arc::new(AtomicBool::new(false));
            let callback_address = Arc::clone(&mailbox_address);
            let callback_observed = Arc::clone(&lookup_observed_unlocked);
            let callback_signal_slot = Arc::clone(&signal_slot);
            let callback_scene_slot = Arc::clone(&scene_slot);
            let callback_current_slot = Arc::clone(&current_slot);
            let callback_successor_slot = Arc::clone(&successor_slot);
            let callback_dropped = Arc::clone(&dropped);
            let callback_signal_pending = Arc::clone(&successor_observed_signal_pending);
            let callback_scene_deregistered =
                Arc::clone(&successor_observed_scene_deregistered);
            let callback_owner_dropped = Arc::clone(&successor_observed_owner_dropped);
            let probe = PyCFunction::new_closure(
                py,
                None,
                None,
                move |args: &Bound<'_, PyTuple>,
                      _kwargs: Option<&Bound<'_, PyDict>>|
                      -> PyResult<()> {
                    let py = args.py();
                    let address = callback_address.load(Ordering::SeqCst);
                    assert_ne!(address, 0);
                    let mailbox = unsafe { &*(address as *const Mutex<Mailbox>) };
                    callback_observed.store(mailbox.try_lock().is_ok(), Ordering::SeqCst);
                    let signal = lock(&callback_signal_slot)
                        .as_ref()
                        .map(|signal| signal.clone_ref(py))
                        .expect("current signal must be installed before successor dispatch");
                    callback_signal_pending.store(
                        !signal.bind(py).call_method0("done")?.is_truthy()?,
                        Ordering::SeqCst,
                    );
                    let scene = lock(&callback_scene_slot)
                        .as_ref()
                        .and_then(Weak::upgrade)
                        .expect("Scene must remain available during successor dispatch");
                    let current = lock(&callback_current_slot)
                        .as_ref()
                        .cloned()
                        .expect("current identity must be installed before successor dispatch");
                    let successor = lock(&callback_successor_slot)
                        .as_ref()
                        .cloned()
                        .expect("successor identity must be installed before dispatch");
                    callback_scene_deregistered.store(
                        !scene.operation_registered_weak_for_test(&current)
                            && scene.operation_registered_weak_for_test(&successor),
                        Ordering::SeqCst,
                    );
                    callback_owner_dropped
                        .store(callback_dropped.load(Ordering::SeqCst), Ordering::SeqCst);
                    Ok(())
                },
            )?;
            let globals = PyDict::new(py);
            globals.set_item("Actor", py.get_type::<Actor>())?;
            globals.set_item("probe", probe)?;
            globals.set_item("events", PyList::empty(py))?;
            globals.set_item(
                "probe_boom",
                PyRuntimeError::new_err("mailbox probe complete").into_value(py),
            )?;
            py.run(
                c"class MailboxProbeActor(Actor):\n    def cued(self, cue):\n        events.append('successor')\n        probe()\n        raise probe_boom\n\ndef signal_done(_future):\n    events.append('signal')\n\nasync def barrier():\n    import asyncio\n    reached = asyncio.get_running_loop().create_future()\n    asyncio.get_running_loop().call_soon(reached.set_result, None)\n    await reached\n",
                Some(&globals),
                None,
            )?;
            let actor_type = globals
                .get_item("MailboxProbeActor")?
                .expect("probe actor type must exist")
                .cast_into::<PyType>()?;
            let capability = capability_for_type(py, &actor_type, "mailbox-lock-probe")?;
            mailbox_address.store(
                std::ptr::from_ref(&capability.mailbox) as usize,
                Ordering::SeqCst,
            );

            let asyncio = py.import("asyncio")?;
            let event_loop = asyncio.call_method0("new_event_loop")?;
            asyncio.call_method1("set_event_loop", (&event_loop,))?;
            let binding_state = Arc::new(ProductionState::new());
            let binding = RunBinding::new(py, &binding_state, &event_loop)?;
            let scene = SceneScope::zero_for_binding_for_test(
                py,
                "scene-mailbox-lock-probe",
                &binding,
            )?;
            *lock(&scene_slot) = Some(Arc::downgrade(&scene));
            let placeholder =
                CueOperation::new_for_actor_for_test(usize::MAX, &capability, &scene);
            lock(&capability.mailbox).running = Some(Running::new(placeholder.clone()));

            let current_prepared = scene
                .begin_admission()
                .expect("current operation must register")
                .prepare(py)?;
            let current_id = PyString::new(py, current_prepared.id()).unbind();
            let capsule = PyCapsule::new_with_value(
                py,
                MailboxDropProbe {
                    mailbox_address: Arc::clone(&mailbox_address),
                    scene_slot: Arc::clone(&scene_slot),
                    current_slot: Arc::clone(&current_slot),
                    successor_slot: Arc::clone(&successor_slot),
                    dropped: Arc::clone(&dropped),
                    observed_unlocked: Arc::clone(&drop_observed_unlocked),
                    observed_mailbox_retired: Arc::clone(&drop_observed_mailbox_retired),
                    observed_scene_deregistered: Arc::clone(
                        &drop_observed_scene_deregistered,
                    ),
                },
                c"troupe.actor.mailbox_drop_probe",
            )?;
            let current_instruction = PyDict::new(py);
            current_instruction.set_item("drop_probe", &capsule)?;
            let current_cue = Py::new(
                py,
                Cue::new_runtime(
                    current_id.clone_ref(py),
                    current_instruction.unbind().into_any(),
                    PyString::new(py, "scene-mailbox-lock-probe").unbind(),
                ),
            )?;
            let current_cued = CuedScope::new(
                py,
                Arc::clone(&scene),
                PyString::new(py, "mailbox-lock-probe").unbind(),
                capability.identity_address(),
                current_id,
            )?;
            let current_signal = event_loop.call_method0("create_future")?;
            *lock(&signal_slot) = Some(current_signal.clone().unbind());
            current_signal.call_method1(
                "add_done_callback",
                (globals
                    .get_item("signal_done")?
                    .expect("signal callback must exist"),),
            )?;
            let current = CueOperation::new_runtime(
                &scene,
                &capability,
                &binding,
                Arc::clone(&current_cued),
                current_cue,
                current_signal.clone().unbind(),
            );
            *lock(&current_slot) = Some(current.downgrade_for_test());
            current_prepared.commit(current.clone())?;
            drop(capsule);
            {
                let mut mailbox = lock(&capability.mailbox);
                let (_, claimed) = mailbox
                    .complete_running(&placeholder)
                    .expect("placeholder must release the current operation");
                assert!(claimed.is_some_and(|operation| operation.ptr_eq(&current)));
            }
            current.force_running_for_test(py.None());

            let successor_prepared = scene
                .begin_admission()
                .expect("successor operation must register")
                .prepare(py)?;
            let successor_id = PyString::new(py, successor_prepared.id()).unbind();
            let successor_cue = Py::new(
                py,
                Cue::new_runtime(
                    successor_id.clone_ref(py),
                    PyDict::new(py).unbind().into_any(),
                    PyString::new(py, "scene-mailbox-lock-probe").unbind(),
                ),
            )?;
            let successor_cued = CuedScope::new(
                py,
                Arc::clone(&scene),
                PyString::new(py, "mailbox-lock-probe").unbind(),
                capability.identity_address(),
                successor_id,
            )?;
            let successor_signal = event_loop.call_method0("create_future")?;
            let successor = CueOperation::new_runtime(
                &scene,
                &capability,
                &binding,
                successor_cued,
                successor_cue,
                successor_signal.clone().unbind(),
            );
            *lock(&successor_slot) = Some(successor.downgrade_for_test());
            successor_prepared.commit(successor.clone())?;
            assert_eq!(scene.operation_count_for_test(), 2);

            current_cued.close_inline();
            assert!(current.finish_for_test(
                py,
                Ok(PyTuple::empty(py).unbind().into_any()),
            ));

            assert!(lookup_observed_unlocked.load(Ordering::SeqCst));
            assert!(dropped.load(Ordering::SeqCst));
            assert!(drop_observed_unlocked.load(Ordering::SeqCst));
            assert!(drop_observed_mailbox_retired.load(Ordering::SeqCst));
            assert!(drop_observed_scene_deregistered.load(Ordering::SeqCst));
            assert!(successor_observed_signal_pending.load(Ordering::SeqCst));
            assert!(successor_observed_scene_deregistered.load(Ordering::SeqCst));
            assert!(successor_observed_owner_dropped.load(Ordering::SeqCst));
            assert!(current_signal.call_method0("done")?.is_truthy()?);
            assert!(current_signal.call_method0("result")?.is_none());
            assert!(successor_signal.call_method0("done")?.is_truthy()?);
            assert!(successor_signal.call_method0("result")?.is_none());
            assert!(current.terminal_success_for_test(py).is_some());
            assert!(successor.is_terminal());
            assert_eq!(scene.operation_count_for_test(), 0);
            assert_eq!(
                globals
                    .get_item("events")?
                    .expect("event log must exist")
                    .extract::<Vec<String>>()?,
                ["successor"]
            );
            let barrier = globals
                .get_item("barrier")?
                .expect("loop barrier must exist")
                .call0()?;
            event_loop.call_method1("run_until_complete", (barrier,))?;
            assert_eq!(
                globals
                    .get_item("events")?
                    .expect("event log must exist")
                    .extract::<Vec<String>>()?,
                ["successor", "signal"]
            );
            let mailbox = lock(&capability.mailbox);
            assert!(mailbox.queue.is_empty());
            assert!(mailbox.running.is_none());
            drop(mailbox);
            event_loop.call_method0("close")?;
            asyncio.call_method1("set_event_loop", (py.None(),))?;
            Ok::<_, PyErr>(())
        })
        .expect("running terminal actions must release all owners before the caller wakes");
    }

    #[test]
    fn admission_ids_and_execution_follow_gated_commit_not_caller_creation() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let capability = capability_for_type(py, &py.get_type::<Actor>(), "admission-order")?;
            let asyncio = py.import("asyncio")?;
            let event_loop = asyncio.call_method0("new_event_loop")?;
            let state = Arc::new(ProductionState::new());
            let binding = RunBinding::new(py, &state, &event_loop)?;
            state.bind(&binding)?;
            let scene = SceneScope::zero_for_binding_for_test(
                py,
                "scene-admission-order",
                &binding,
            )?;
            let current =
                CueOperation::new_for_actor_for_test(usize::MAX, &capability, &scene);
            current.finish_cancel();
            lock(&capability.mailbox).running = Some(Running::new(current.clone()));

            let probes = PyModule::from_code(
                py,
                c"execution = []\ndef callback(label):\n    return lambda _future: execution.append(label)\n\nasync def barrier():\n    import asyncio\n    reached = asyncio.get_running_loop().create_future()\n    asyncio.get_running_loop().call_soon(reached.set_result, None)\n    await reached\n",
                c"admission_order_test.py",
                c"admission_order_test",
            )?;
            let callback = probes.getattr("callback")?.unbind();
            let admissions = Arc::new(Mutex::new(Vec::<(usize, String)>::new()));
            let barrier = Arc::new(Barrier::new(4));
            let mut gate_senders = Vec::new();
            let mut ack_receivers = Vec::new();
            let mut callers = Vec::new();

            for label in 0..3 {
                let (gate_sender, gate_receiver) = mpsc::channel();
                let (ack_sender, ack_receiver) = mpsc::channel();
                gate_senders.push(gate_sender);
                ack_receivers.push(ack_receiver);
                let caller_barrier = Arc::clone(&barrier);
                let caller_scene = Arc::clone(&scene);
                let caller_capability = Arc::clone(&capability);
                let caller_binding = Arc::clone(&binding);
                let caller_callback = callback.clone_ref(py);
                let caller_event_loop = event_loop.clone().unbind();
                let caller_admissions = Arc::clone(&admissions);
                callers.push(thread::spawn(move || {
                    caller_barrier.wait();
                    gate_receiver
                        .recv()
                        .map_err(|error| error.to_string())?;
                    let result = Python::attach(|thread_py| {
                        let transaction = caller_scene
                            .begin_admission()
                            .expect("gated caller must observe an open scene");
                        let prepared = transaction.prepare(thread_py)?;
                        let id = prepared.id().to_owned();
                        let cue_id = PyString::new(thread_py, &id).unbind();
                        let instruction = PyDict::new(thread_py);
                        instruction.set_item("caller", label)?;
                        let cue = Py::new(
                            thread_py,
                            Cue::new_runtime(
                                cue_id.clone_ref(thread_py),
                                instruction.unbind().into_any(),
                                PyString::new(thread_py, "scene-admission-order").unbind(),
                            ),
                        )?;
                        let cued = CuedScope::new(
                            thread_py,
                            Arc::clone(&caller_scene),
                            PyString::new(thread_py, "admission-order").unbind(),
                            caller_capability.identity_address(),
                            cue_id,
                        )?;
                        let signal = caller_event_loop
                            .bind(thread_py)
                            .call_method0("create_future")?;
                        signal.call_method1(
                            "add_done_callback",
                            (caller_callback.bind(thread_py).call1((label,))?,),
                        )?;
                        let operation = CueOperation::new_runtime(
                            &caller_scene,
                            &caller_capability,
                            &caller_binding,
                            cued,
                            cue,
                            signal.unbind(),
                        );
                        prepared.commit(operation)?;
                        lock(&caller_admissions).push((label, id));
                        Ok::<_, PyErr>(())
                    })
                    .map_err(|error| error.to_string());
                    let _ = ack_sender.send(());
                    result
                }));
            }

            let caller_results = py.detach(move || {
                barrier.wait();
                for label in [2, 0, 1] {
                    gate_senders[label]
                        .send(())
                        .expect("admission gate receiver must remain alive");
                    ack_receivers[label]
                        .recv()
                        .expect("admission caller must acknowledge commit");
                }
                callers
                    .into_iter()
                    .map(|caller| caller.join().expect("admission caller must not panic"))
                    .collect::<Vec<_>>()
            });
            for result in caller_results {
                result.map_err(PyRuntimeError::new_err)?;
            }

            let caller_creation_order = [0, 1, 2];
            assert_eq!(caller_creation_order, [0, 1, 2]);
            assert_eq!(
                lock(&admissions).as_slice(),
                &[
                    (2, "scene-admission-order-cue0".to_owned()),
                    (0, "scene-admission-order-cue1".to_owned()),
                    (1, "scene-admission-order-cue2".to_owned()),
                ]
            );
            capability.finish_operation(&current);

            let barrier = probes.getattr("barrier")?.call0()?;
            event_loop.call_method1("run_until_complete", (barrier,))?;
            assert_eq!(
                probes.getattr("execution")?.extract::<Vec<usize>>()?,
                [2, 0, 1]
            );
            let mailbox = lock(&capability.mailbox);
            assert!(mailbox.queue.is_empty());
            assert!(mailbox.running.is_none());
            drop(mailbox);
            event_loop.call_method0("close")?;
            Ok::<_, PyErr>(())
        })
        .expect("admission index and execution must follow gated commit order");
    }

    #[test]
    fn attachment_failure_quarantine_ignores_later_cancellation_matrix() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let asyncio = py.import("asyncio")?;
            let event_loop = asyncio.call_method0("new_event_loop")?;
            asyncio.call_method1("set_event_loop", (&event_loop,))?;
            let globals = PyDict::new(py);
            globals.set_item("Actor", py.get_type::<Actor>())?;
            py.run(
                c"import asyncio\nfrom collections.abc import Coroutine\n\noverride_calls = []\nunhandled = []\nevents = []\ntrapped_task_created = False\ntask_outcome = 'normal'\n\ndef exception_handler(loop, context):\n    unhandled.append(context)\n\ndef signal_done(_future):\n    events.append('signal')\n\nclass CallbackTrapTask(asyncio.Task):\n    def add_done_callback(self, *args, **kwargs):\n        override_calls.append('add_done_callback')\n        raise AssertionError('dynamic add_done_callback called')\n    def cancel(self, *args, **kwargs):\n        override_calls.append('cancel')\n        raise AssertionError('dynamic cancel called')\n    def done(self):\n        override_calls.append('done')\n        raise AssertionError('dynamic done called')\n    def result(self):\n        override_calls.append('result')\n        raise AssertionError('dynamic result called')\n\ndef task_factory(loop, coroutine, **kwargs):\n    global trapped_task_created\n    if type(coroutine).__name__ == '_ScopeDriver' and not trapped_task_created:\n        trapped_task_created = True\n        return CallbackTrapTask(coroutine, loop=loop, **kwargs)\n    return asyncio.Task(coroutine, loop=loop, **kwargs)\n\nclass PreStartCleanup(Coroutine):\n    def __init__(self, entered, release):\n        self.entered = entered\n        self.release = release\n        self.waiter = None\n        self.closed = False\n    def send(self, value):\n        try:\n            return self.waiter.send(value)\n        except StopIteration:\n            if task_outcome == 'normal':\n                raise StopIteration(())\n            if task_outcome == 'error':\n                raise task_boom\n            raise asyncio.CancelledError('attachment task cancelled')\n    def throw(self, exc):\n        if self.waiter is None:\n            self.entered.set()\n            self.waiter = self.release.wait().__await__()\n            return self.waiter.send(None)\n        return self.waiter.throw(exc)\n    def close(self):\n        if not self.closed:\n            self.closed = True\n            if self.waiter is not None:\n                self.waiter.close()\n    def __await__(self):\n        return self\n\ndef make_error(message):\n    try:\n        raise RuntimeError(message)\n    except RuntimeError as error:\n        return error\n\nasync def succeed():\n    events.append('successor')\n    successor_entered.set()\n    return ()\n\nclass AttachmentActor(Actor):\n    def cued(self, cue):\n        if cue.instruction['label'] == 'first':\n            return PreStartCleanup(cleanup_entered, cleanup_release)\n        return succeed()\n\nasync def wait_for_cleanup_entry():\n    await asyncio.wait_for(cleanup_entered.wait(), 5.0)\n\nasync def wait_for_signals(first, second):\n    return await asyncio.wait_for(asyncio.gather(first, second), 5.0)\n\nasync def loop_barrier():\n    reached = asyncio.get_running_loop().create_future()\n    asyncio.get_running_loop().call_soon(reached.set_result, None)\n    await reached\n",
                Some(&globals),
                None,
            )?;
            event_loop.call_method1(
                "set_exception_handler",
                (globals
                    .get_item("exception_handler")?
                    .expect("exception handler must exist"),),
            )?;
            event_loop.call_method1(
                "set_task_factory",
                (globals
                    .get_item("task_factory")?
                    .expect("task factory must exist"),),
            )?;

            let state = Arc::new(ProductionState::new());
            let binding = RunBinding::new(py, &state, &event_loop)?;
            binding.install_wrapper(py)?;
            let actor_type = globals
                .get_item("AttachmentActor")?
                .expect("attachment Actor type must exist")
                .cast_into::<PyType>()?;
            let capability =
                capability_for_type(py, &actor_type, "attachment-failure")?;

            for (case_index, task_outcome, cancel_order) in [
                (0, "normal", "caller-first"),
                (1, "error", "caller-first"),
                (2, "cancelled", "caller-first"),
                (3, "normal", "scene-first"),
                (4, "error", "scene-first"),
                (5, "cancelled", "scene-first"),
            ] {
                let override_calls = PyList::empty(py);
                let unhandled = PyList::empty(py);
                let events = PyList::empty(py);
                let cleanup_entered = asyncio.getattr("Event")?.call0()?;
                let cleanup_release = asyncio.getattr("Event")?.call0()?;
                let successor_entered = asyncio.getattr("Event")?.call0()?;
                let attachment_boom = globals
                    .get_item("make_error")?
                    .expect("error factory must exist")
                    .call1((format!("attachment-{case_index}"),))?
                    .unbind();
                let attachment_traceback = attachment_boom
                    .bind(py)
                    .getattr("__traceback__")?
                    .unbind();
                let task_boom = globals
                    .get_item("make_error")?
                    .expect("error factory must exist")
                    .call1((format!("task-{case_index}"),))?
                    .unbind();
                globals.set_item("override_calls", &override_calls)?;
                globals.set_item("unhandled", &unhandled)?;
                globals.set_item("events", &events)?;
                globals.set_item("trapped_task_created", false)?;
                globals.set_item("task_outcome", task_outcome)?;
                globals.set_item("cleanup_entered", &cleanup_entered)?;
                globals.set_item("cleanup_release", &cleanup_release)?;
                globals.set_item("successor_entered", &successor_entered)?;
                globals.set_item("attachment_boom", attachment_boom.bind(py))?;
                globals.set_item("task_boom", task_boom.bind(py))?;

                let scene_name = format!("scene-attachment-{case_index}");
                let scene = SceneScope::zero_for_binding_for_test(py, &scene_name, &binding)?;
                let prepared = scene
                    .begin_admission()
                    .expect("attachment case must start Open")
                    .prepare(py)?;
                let first_id = PyString::new(py, prepared.id()).unbind();
                let first_instruction = PyDict::new(py);
                first_instruction.set_item("label", "first")?;
                let first_cue = Py::new(
                    py,
                    Cue::new_runtime(
                        first_id.clone_ref(py),
                        first_instruction.unbind().into_any(),
                        PyString::new(py, &scene_name).unbind(),
                    ),
                )?;
                let first_cued = CuedScope::new(
                    py,
                    Arc::clone(&scene),
                    PyString::new(py, "attachment-failure").unbind(),
                    capability.identity_address(),
                    first_id,
                )?;
                let first_signal = event_loop.call_method0("create_future")?;
                first_signal.call_method1(
                    "add_done_callback",
                    (globals
                        .get_item("signal_done")?
                        .expect("signal callback must exist"),),
                )?;
                let first = CueOperation::new_runtime(
                    &scene,
                    &capability,
                    &binding,
                    Arc::clone(&first_cued),
                    first_cue,
                    first_signal.clone().unbind(),
                );
                first.force_callback_attachment_error_for_test(attachment_boom.clone_ref(py));

                let second_id_text = format!("{scene_name}-cue1");
                let second_id = PyString::new(py, &second_id_text).unbind();
                let second_instruction = PyDict::new(py);
                second_instruction.set_item("label", "second")?;
                let second_cue = Py::new(
                    py,
                    Cue::new_runtime(
                        second_id.clone_ref(py),
                        second_instruction.unbind().into_any(),
                        PyString::new(py, &scene_name).unbind(),
                    ),
                )?;
                let second_cued = CuedScope::new(
                    py,
                    Arc::clone(&scene),
                    PyString::new(py, "attachment-failure").unbind(),
                    capability.identity_address(),
                    second_id,
                )?;
                let second_signal = event_loop.call_method0("create_future")?;
                let second = CueOperation::new_runtime(
                    &scene,
                    &capability,
                    &binding,
                    second_cued,
                    second_cue,
                    second_signal.clone().unbind(),
                );

                let trigger_prepared = Arc::new(Mutex::new(Some(prepared)));
                let trigger_capability = Arc::clone(&capability);
                let trigger_first = first.clone();
                let trigger_second = second.clone();
                let trigger = PyCFunction::new_closure(
                    py,
                    None,
                    None,
                    move |_args: &Bound<'_, PyTuple>,
                          _kwargs: Option<&Bound<'_, PyDict>>|
                          -> PyResult<()> {
                        let prepared = lock(&trigger_prepared)
                            .take()
                            .expect("attachment admission trigger runs once");
                        prepared.commit(trigger_first.clone())?;
                        trigger_capability.enqueue_operation(trigger_second.clone())
                    },
                )?;
                event_loop.call_method1("call_soon", (trigger,))?;

                let wait_for_entry = globals
                    .get_item("wait_for_cleanup_entry")?
                    .expect("cleanup waiter must exist")
                    .call0()?;
                event_loop.call_method1("run_until_complete", (wait_for_entry,))?;

                match cancel_order {
                    "caller-first" => {
                        assert!(!first.request_cancel());
                        scene.close();
                    }
                    "scene-first" => {
                        scene.close();
                        assert!(!first.request_cancel());
                    }
                    _ => unreachable!(),
                }
                let early_signal_done = first_signal.call_method0("done")?.is_truthy()?;
                let early_successor = successor_entered.call_method0("is_set")?.is_truthy()?;
                let early_cued_active = first_cued.is_active();
                let early_operation_count = scene.operation_count_for_test();
                let early_slot_exact = lock(&capability.mailbox)
                    .running
                    .as_ref()
                    .is_some_and(|running| running.operation.ptr_eq(&first));

                cleanup_release.call_method0("set")?;
                let wait_for_signals = globals
                    .get_item("wait_for_signals")?
                    .expect("signal waiter must exist")
                    .call1((&first_signal, &second_signal))?;
                let signal_results = event_loop
                    .call_method1("run_until_complete", (wait_for_signals,))?
                    .cast_into::<PyList>()?;
                assert!(signal_results.iter().all(|result| result.is_none()));
                py.import("gc")?.call_method0("collect")?;
                let barrier = globals
                    .get_item("loop_barrier")?
                    .expect("loop barrier must exist")
                    .call0()?;
                event_loop.call_method1("run_until_complete", (barrier,))?;
                py.import("gc")?.call_method0("collect")?;

                assert!(!early_signal_done, "{task_outcome}/{cancel_order}");
                assert!(!early_successor, "{task_outcome}/{cancel_order}");
                assert!(early_cued_active, "{task_outcome}/{cancel_order}");
                assert_eq!(early_operation_count, 1, "{task_outcome}/{cancel_order}");
                assert!(early_slot_exact, "{task_outcome}/{cancel_order}");
                assert!(first
                    .terminal_error_for_test(py)
                    .is_some_and(|error| error.bind(py).is(attachment_boom.bind(py))));
                assert!(attachment_boom
                    .bind(py)
                    .getattr("__traceback__")?
                    .is(attachment_traceback.bind(py)));
                assert!(attachment_boom.bind(py).getattr("__context__")?.is_none());
                assert!(attachment_boom.bind(py).getattr("__cause__")?.is_none());
                assert!(second.terminal_success_for_test(py).is_some_and(|result| {
                    result.bind(py).is_empty()
                }));
                assert!(successor_entered.call_method0("is_set")?.is_truthy()?);
                assert_eq!(events.extract::<Vec<String>>()?, ["successor", "signal"]);
                assert!(!first_cued.is_active());
                assert!(first.terminal_owns_no_task_or_intent_for_test());
                assert_eq!(first.cancel_requests_for_test(), 0);
                assert_eq!(scene.operation_count_for_test(), 0);
                assert!(override_calls.is_empty(), "{task_outcome}/{cancel_order}");
                assert!(unhandled.is_empty(), "{task_outcome}/{cancel_order}");
                let mailbox = lock(&capability.mailbox);
                assert!(mailbox.queue.is_empty());
                assert!(mailbox.running.is_none());
                drop(mailbox);
            }

            binding.restore_factory(py)?;
            event_loop.call_method1("set_task_factory", (py.None(),))?;
            event_loop.call_method0("close")?;
            asyncio.call_method1("set_event_loop", (py.None(),))?;
            Ok::<_, PyErr>(())
        })
        .expect("attachment failure must quarantine every later cancellation source");
    }
}
