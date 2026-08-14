use std::sync::{Arc, Weak};

use pyo3::class::gc::{PyTraverseError, PyVisit};
use pyo3::exceptions::{PyRuntimeError, PyStopIteration, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict, PyString};

use crate::diagnostic_runtime::cue_producer::{self, CueHook};
use crate::orchestration::actor::ActorCapability;
use crate::orchestration::cue::{Cue, CueContextError};
use crate::orchestration::mailbox::CueOperation;
#[cfg(test)]
use crate::orchestration::scene_context::{AdmissionMetric, record_admission_metric_for_test};
use crate::orchestration::scene_context::{CUE_CONTEXT_ERROR, CuedScope, RunBinding, SceneScope};

const REUSE_ERROR: &str = "cannot reuse already awaited coroutine";

#[derive(Clone, Copy, Eq, PartialEq)]
enum CueCallPhase {
    Created,
    Waiting,
    Cancelling,
    Done,
}

#[pyclass(name = "_CueCall", module = "troupe._runtime")]
pub(crate) struct CueCall {
    handle: Option<Py<PyAny>>,
    instruction: Option<Py<PyDict>>,
    signal: Option<Py<PyAny>>,
    waiter: Option<Py<PyAny>>,
    cancellation: Option<Py<PyAny>>,
    target: Option<Arc<ActorCapability>>,
    binding: Weak<RunBinding>,
    scene: Weak<SceneScope>,
    operation: Option<CueOperation>,
    phase: CueCallPhase,
}

impl CueCall {
    pub(crate) fn new(
        handle: Py<PyAny>,
        instruction: Py<PyDict>,
        target: Arc<ActorCapability>,
        binding: &Arc<RunBinding>,
        scene: &Arc<SceneScope>,
    ) -> Self {
        Self {
            handle: Some(handle),
            instruction: Some(instruction),
            signal: None,
            waiter: None,
            cancellation: None,
            target: Some(target),
            binding: Arc::downgrade(binding),
            scene: Arc::downgrade(scene),
            operation: None,
            phase: CueCallPhase::Created,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        handle: Py<PyAny>,
        instruction: Py<PyDict>,
        signal: Py<PyAny>,
        waiter: Py<PyAny>,
        cancellation: Option<Py<PyAny>>,
        operation: CueOperation,
    ) -> Self {
        Self {
            handle: Some(handle),
            instruction: Some(instruction),
            signal: Some(signal),
            waiter: Some(waiter),
            cancellation,
            target: None,
            binding: Weak::new(),
            scene: Weak::new(),
            operation: Some(operation),
            phase: CueCallPhase::Waiting,
        }
    }

    fn clear(&mut self) {
        self.handle = None;
        self.instruction = None;
        self.signal = None;
        self.waiter = None;
        self.cancellation = None;
        self.target = None;
        self.operation = None;
    }

    fn finish(&mut self) {
        if self.phase != CueCallPhase::Done
            && let Some(operation) = &self.operation
        {
            cue_producer::observe(operation, CueHook::CallerFinished);
        }
        self.phase = CueCallPhase::Done;
        self.clear();
    }

    fn source_for_lineage(
        py: Python<'_>,
        lineage: &crate::orchestration::python_task::TaskLineage,
        scene: &Arc<SceneScope>,
    ) -> Py<PyString> {
        lineage
            .cued()
            .map(|cued| cued.source(py))
            .unwrap_or_else(|| scene.name(py))
    }

    fn fresh_waiter(py: Python<'_>, signal: &Py<PyAny>) -> PyResult<Py<PyAny>> {
        py.import("asyncio")?
            .call_method1("shield", (signal.bind(py),))?
            .call_method0("__await__")
            .map(Bound::unbind)
    }

    fn admit(&self, py: Python<'_>) -> PyResult<(Py<PyAny>, Py<PyAny>, CueOperation)> {
        let binding = self
            .binding
            .upgrade()
            .ok_or_else(|| CueContextError::new_err(CUE_CONTEXT_ERROR))?;
        let scene = self
            .scene
            .upgrade()
            .ok_or_else(|| CueContextError::new_err(CUE_CONTEXT_ERROR))?;
        let lineage = binding.validate_lineage_for_scene(py, &scene)?;
        let transaction = scene
            .begin_admission()
            .ok_or_else(|| CueContextError::new_err(CUE_CONTEXT_ERROR))?;
        cue_producer::admission_started(&binding, &scene);
        let instruction = self
            .instruction
            .as_ref()
            .expect("created CueCall must retain its instruction")
            .bind(py)
            .copy()?;
        #[cfg(test)]
        record_admission_metric_for_test(AdmissionMetric::Copy);
        let snapshot = py
            .import("types")?
            .getattr("MappingProxyType")?
            .call1((instruction,))?
            .unbind();
        let prepared = transaction.prepare(py)?;
        let id = PyString::new(py, prepared.id()).unbind();
        let cued_id = id.clone_ref(py);
        let source = Self::source_for_lineage(py, &lineage, &scene);
        let cue = Py::new(py, Cue::new_runtime(id, snapshot, source))?;
        #[cfg(test)]
        record_admission_metric_for_test(AdmissionMetric::Cue);
        let target = self
            .target
            .as_ref()
            .map(Arc::clone)
            .ok_or_else(|| PyRuntimeError::new_err("Actor is no longer alive"))?;
        let cued = CuedScope::new(
            py,
            Arc::clone(&scene),
            target.source_name_snapshot(py)?,
            target.identity_address(),
            cued_id,
        )?;
        let loop_ = binding.event_loop(py);
        let signal = loop_.bind(py).call_method0("create_future")?.unbind();
        let operation =
            CueOperation::new_runtime(&scene, &target, &binding, cued, cue, signal.clone_ref(py));
        let waiter = Self::fresh_waiter(py, &signal)?;
        prepared.commit(operation.clone())?;
        cue_producer::observe(&operation, CueHook::Admitted);
        Ok((signal, waiter, operation))
    }

    fn drive_waiter(&mut self, py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let waiter = self
            .waiter
            .as_ref()
            .map(|waiter| waiter.clone_ref(py))
            .ok_or_else(|| PyRuntimeError::new_err(REUSE_ERROR))?;
        match waiter.bind(py).call_method1("send", (value,)) {
            Ok(yielded) => Ok(yielded.unbind()),
            Err(error) if error.is_instance_of::<PyStopIteration>(py) => {
                self.finish_from_operation(py)
            }
            Err(error) => {
                self.finish();
                Err(error)
            }
        }
    }

    fn throw_into_waiter(&mut self, py: Python<'_>, exc: Py<PyAny>) -> PyResult<Py<PyAny>> {
        let waiter = self
            .waiter
            .as_ref()
            .map(|waiter| waiter.clone_ref(py))
            .ok_or_else(|| PyRuntimeError::new_err(REUSE_ERROR))?;
        match waiter.bind(py).call_method1("throw", (exc,)) {
            Ok(yielded) => Ok(yielded.unbind()),
            Err(error) if error.is_instance_of::<PyStopIteration>(py) => {
                self.finish_from_operation(py)
            }
            Err(error) => {
                self.finish();
                Err(error)
            }
        }
    }

    fn finish_from_operation(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let result = self
            .operation
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err(REUSE_ERROR))?
            .result_for_caller(py, self.cancellation.as_ref());
        self.finish();
        match result {
            Ok(value) => Err(PyStopIteration::new_err((value,))),
            Err(error) => Err(error),
        }
    }

    fn replace_shield_and_wait(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let signal = self
            .signal
            .as_ref()
            .map(|signal| signal.clone_ref(py))
            .ok_or_else(|| PyRuntimeError::new_err(REUSE_ERROR))?;
        let waiter = Self::fresh_waiter(py, &signal)?;
        let previous = self.waiter.replace(waiter);
        drop(previous);
        self.phase = CueCallPhase::Cancelling;
        let none = py.None();
        self.drive_waiter(py, none.bind(py))
    }

    fn is_cancelled_error(py: Python<'_>, exc: &Bound<'_, PyAny>) -> PyResult<bool> {
        exc.is_instance(&py.import("asyncio")?.getattr("CancelledError")?)
    }
}

#[pymethods]
impl CueCall {
    fn send(&mut self, py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        match self.phase {
            CueCallPhase::Created => {
                if !value.is_none() {
                    self.finish();
                    return Err(PyTypeError::new_err(
                        "can't send non-None value to a just-started coroutine",
                    ));
                }
                let (signal, waiter, operation) = match self.admit(py) {
                    Ok(admission) => admission,
                    Err(error) => {
                        self.finish();
                        return Err(error);
                    }
                };
                self.signal = Some(signal);
                self.waiter = Some(waiter);
                self.operation = Some(operation);
                self.phase = CueCallPhase::Waiting;
                self.drive_waiter(py, value)
            }
            CueCallPhase::Waiting => self.drive_waiter(py, value),
            CueCallPhase::Cancelling => self.drive_waiter(py, value),
            CueCallPhase::Done => Err(PyRuntimeError::new_err(REUSE_ERROR)),
        }
    }

    fn throw(&mut self, py: Python<'_>, exc: Py<PyAny>) -> PyResult<Py<PyAny>> {
        match self.phase {
            CueCallPhase::Created => {
                self.finish();
                Err(PyErr::from_value(exc.into_bound(py)))
            }
            CueCallPhase::Waiting if Self::is_cancelled_error(py, exc.bind(py))? => {
                let operation = self
                    .operation
                    .as_ref()
                    .cloned()
                    .ok_or_else(|| PyRuntimeError::new_err(REUSE_ERROR))?;
                let committed = operation.request_cancel();
                if !committed && operation.is_terminal() {
                    return self.finish_from_operation(py);
                }
                if self.cancellation.is_none() {
                    self.cancellation = Some(exc);
                }
                if operation.is_terminal() {
                    self.finish_from_operation(py)
                } else {
                    self.replace_shield_and_wait(py)
                }
            }
            CueCallPhase::Waiting => self.throw_into_waiter(py, exc),
            CueCallPhase::Cancelling if Self::is_cancelled_error(py, exc.bind(py))? => {
                if self.cancellation.is_none() {
                    self.cancellation = Some(exc);
                }
                self.replace_shield_and_wait(py)
            }
            CueCallPhase::Cancelling => self.throw_into_waiter(py, exc),
            CueCallPhase::Done => Err(PyRuntimeError::new_err(REUSE_ERROR)),
        }
    }

    fn close(&mut self) {
        if let Some(operation) = &self.operation {
            operation.request_cancel();
        }
        self.finish();
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
        visit.call(&self.handle)?;
        visit.call(&self.instruction)?;
        visit.call(&self.signal)?;
        visit.call(&self.waiter)?;
        visit.call(&self.cancellation)?;
        if let Some(operation) = &self.operation {
            operation.traverse(&visit)?;
        }
        Ok(())
    }

    fn __clear__(&mut self) {
        if let Some(operation) = &self.operation {
            operation.request_cancel();
        }
        self.phase = CueCallPhase::Done;
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pyo3::prelude::*;
    use pyo3::types::{PyAnyMethods, PyDict, PyDictMethods, PyModule, PyString, PyType};

    use crate::orchestration::actor::{
        Actor, ActorCapability, ActorCapabilityNode, ActorIdentity, enter_actor_permit,
    };
    use crate::orchestration::actor_handle::ActorHandle;
    use crate::orchestration::actor_registry::{NameKey, ProductionState};
    use crate::orchestration::mailbox::CueOperation;
    use crate::orchestration::python_task::TaskLineage;
    use crate::orchestration::scene_context::{
        AdmissionMetrics, RunBinding, admission_metrics_for_test, reset_admission_metrics_for_test,
    };

    use super::CueCall;

    #[test]
    fn cue_call_has_one_signal_and_no_second_cleanup_future() {
        fn assert_shape(call: CueCall) {
            let CueCall {
                handle: _,
                instruction: _,
                signal: _,
                waiter: _,
                cancellation: _,
                target: _,
                binding: _,
                scene: _,
                operation: _,
                phase: _,
            } = call;
        }

        let _: fn(CueCall) = assert_shape;
    }

    #[test]
    fn cue_call_traverses_every_direct_and_operation_python_edge() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let module = PyModule::from_code(
                py,
                c"class Marker: pass\n",
                c"cue_call_gc_test.py",
                c"cue_call_gc_test",
            )?;
            for (index, edge_name) in [
                "handle",
                "instruction",
                "call_signal",
                "operation_signal",
                "waiter",
                "cancellation",
                "cue",
                "task",
                "intent",
                "outcome",
            ]
            .into_iter()
            .enumerate()
            {
                let handle = PyDict::new(py);
                let instruction = PyDict::new(py);
                let signal = PyDict::new(py);
                let waiter = PyDict::new(py);
                let cancellation = PyDict::new(py);
                let operation_edge = PyDict::new(py);
                let operation = CueOperation::new_for_test(index);
                if matches!(
                    edge_name,
                    "operation_signal" | "cue" | "task" | "intent" | "outcome"
                ) {
                    operation.install_traversal_edge_for_test(
                        py,
                        if edge_name == "operation_signal" {
                            "signal"
                        } else {
                            edge_name
                        },
                        operation_edge.clone().unbind().into_any(),
                    )?;
                }
                let call = Py::new(
                    py,
                    CueCall::new_for_test(
                        handle.clone().unbind().into_any(),
                        instruction.clone().unbind(),
                        signal.clone().unbind().into_any(),
                        waiter.clone().unbind().into_any(),
                        Some(cancellation.clone().unbind().into_any()),
                        operation,
                    ),
                )?;
                let selected = match edge_name {
                    "handle" => &handle,
                    "instruction" => &instruction,
                    "call_signal" => &signal,
                    "waiter" => &waiter,
                    "cancellation" => &cancellation,
                    "operation_signal" | "cue" | "task" | "intent" | "outcome" => &operation_edge,
                    _ => unreachable!(),
                };
                let marker = module.getattr("Marker")?.call0()?;
                let marker_ref = py.import("weakref")?.call_method1("ref", (&marker,))?;
                selected.set_item("runner", call.bind(py))?;
                selected.set_item("marker", &marker)?;

                drop((call, marker));
                drop((
                    handle,
                    instruction,
                    signal,
                    waiter,
                    cancellation,
                    operation_edge,
                ));
                py.import("gc")?.call_method0("collect")?;
                assert!(
                    marker_ref.call0()?.is_none(),
                    "untraversed {edge_name} edge"
                );
            }
            Ok::<_, PyErr>(())
        })
        .expect("CueCall and CueOperation Python edges must participate in cyclic GC");
    }

    #[test]
    fn repeated_cancellation_replaces_only_shield_and_preserves_signal_and_first_error() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let asyncio = py.import("asyncio")?;
            let event_loop = asyncio.call_method0("new_event_loop")?;
            asyncio.call_method1("set_event_loop", (&event_loop,))?;
            let fixture = PyModule::from_code(
                py,
                c"class TaskProbe:\n    def __init__(self):\n        self.cancel_calls = 0\n    def cancel(self):\n        self.cancel_calls += 1\n        return True\n\nasync def barrier():\n    import asyncio\n    reached = asyncio.get_running_loop().create_future()\n    asyncio.get_running_loop().call_soon(reached.set_result, None)\n    await reached\n",
                c"cue_call_shield_test.py",
                c"cue_call_shield_test",
            )?;
            for (index, scene_first) in [false, true].into_iter().enumerate() {
                let signal = event_loop.call_method0("create_future")?.unbind();
                let initial_shield = asyncio.call_method1("shield", (signal.bind(py),))?;
                let initial_waiter = initial_shield.call_method0("__await__")?.unbind();
                let task = fixture.getattr("TaskProbe")?.call0()?.unbind();
                let operation = CueOperation::new_for_test(index);
                operation.force_running_for_test(task.clone_ref(py));
                operation.set_signal_for_test(signal.clone_ref(py));
                let mut call = CueCall::new_for_test(
                    py.None(),
                    PyDict::new(py).unbind(),
                    signal.clone_ref(py),
                    initial_waiter,
                    None,
                    operation.clone(),
                );
                let first = asyncio
                    .getattr("CancelledError")?
                    .call1(("first",))?
                    .unbind();
                let second = asyncio
                    .getattr("CancelledError")?
                    .call1(("second",))?
                    .unbind();

                if scene_first {
                    assert!(operation.request_cancel());
                }
                call.throw(py, first.clone_ref(py))
                    .expect("first caller cancellation must wait on the shield");
                let first_waiter = call
                    .waiter
                    .as_ref()
                    .expect("first caller cancellation installs a shield")
                    .as_ptr();
                assert!(call
                    .signal
                    .as_ref()
                    .is_some_and(|actual| actual.as_ptr() == signal.as_ptr()));
                assert!(call
                    .cancellation
                    .as_ref()
                    .is_some_and(|actual| actual.as_ptr() == first.as_ptr()));
                assert!(!signal.bind(py).call_method0("done")?.is_truthy()?);

                call.throw(py, second)
                    .expect("second caller cancellation must replace and wait on the shield");
                assert_ne!(
                    call.waiter
                        .as_ref()
                        .expect("second caller cancellation replaces the shield")
                        .as_ptr(),
                    first_waiter
                );
                assert!(call
                    .signal
                    .as_ref()
                    .is_some_and(|actual| actual.as_ptr() == signal.as_ptr()));
                assert!(call
                    .cancellation
                    .as_ref()
                    .is_some_and(|actual| actual.as_ptr() == first.as_ptr()));
                assert_eq!(task.bind(py).getattr("cancel_calls")?.extract::<usize>()?, 1);

                assert!(operation.finish_for_test(py, Ok(py.None())));
                let barrier = fixture.getattr("barrier")?.call0()?;
                event_loop.call_method1("run_until_complete", (barrier,))?;
                let delivered = call
                    .send(py, py.None().bind(py))
                    .expect_err("terminal cancellation must re-raise the first caller error");
                assert!(delivered.value(py).is(first.bind(py)));
            }
            event_loop.call_method0("close")?;
            asyncio.call_method1("set_event_loop", (py.None(),))?;
            Ok::<_, PyErr>(())
        })
        .expect("repeated cancellation must reuse one signal and preserve first provenance");
    }

    #[test]
    fn created_throw_and_close_release_exact_handle_without_admission() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let state = Arc::new(ProductionState::new());
            let event_loop = py.import("asyncio")?.call_method0("new_event_loop")?;
            let binding = RunBinding::new(py, &state, &event_loop)?;
            state.bind(&binding)?;
            let scene = binding.next_scene(py)?;

            let actor_type = py.get_type::<Actor>();
            let name = PyString::new(py, "created-handle-control");
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
                Arc::downgrade(&state),
            ));
            let node = Py::new(py, ActorCapabilityNode::new(Arc::clone(&capability)))?;
            capability.attach_node(node.bind(py))?;
            let handle = Py::new(py, ActorHandle::from_node(node))?;
            let baseline = unsafe { pyo3::ffi::Py_REFCNT(handle.as_ptr()) };

            let mut thrown_call = CueCall::new(
                handle.clone_ref(py).into_any(),
                PyDict::new(py).unbind(),
                Arc::clone(&capability),
                &binding,
                &scene,
            );
            assert_eq!(
                unsafe { pyo3::ffi::Py_REFCNT(handle.as_ptr()) },
                baseline + 1
            );
            assert!(
                thrown_call
                    .handle
                    .as_ref()
                    .is_some_and(|edge| edge.as_ptr() == handle.as_ptr())
            );
            reset_admission_metrics_for_test();
            let cancellation = py
                .import("asyncio")?
                .getattr("CancelledError")?
                .call0()?
                .unbind();
            let thrown = thrown_call
                .throw(py, cancellation.clone_ref(py))
                .expect_err("Created throw must propagate the actual cancellation");
            assert!(thrown.value(py).is(cancellation.bind(py)));
            assert_eq!(admission_metrics_for_test(), AdmissionMetrics::default());
            assert_eq!(unsafe { pyo3::ffi::Py_REFCNT(handle.as_ptr()) }, baseline);
            let reuse = thrown_call
                .send(py, py.None().bind(py))
                .expect_err("terminal CueCall must reject reuse");
            assert_eq!(
                reuse.to_string(),
                "RuntimeError: cannot reuse already awaited coroutine"
            );

            let mut closed_call = CueCall::new(
                handle.clone_ref(py).into_any(),
                PyDict::new(py).unbind(),
                capability,
                &binding,
                &scene,
            );
            assert_eq!(
                unsafe { pyo3::ffi::Py_REFCNT(handle.as_ptr()) },
                baseline + 1
            );
            assert!(
                closed_call
                    .handle
                    .as_ref()
                    .is_some_and(|edge| edge.as_ptr() == handle.as_ptr())
            );
            reset_admission_metrics_for_test();
            closed_call.close();
            assert_eq!(admission_metrics_for_test(), AdmissionMetrics::default());
            assert_eq!(unsafe { pyo3::ffi::Py_REFCNT(handle.as_ptr()) }, baseline);
            let reuse = closed_call
                .send(py, py.None().bind(py))
                .expect_err("closed CueCall must reject reuse");
            assert_eq!(
                reuse.to_string(),
                "RuntimeError: cannot reuse already awaited coroutine"
            );

            scene.close();
            event_loop.call_method0("close")?;
            Ok::<_, PyErr>(())
        })
        .expect("Created termination must release the exact originating Handle wrapper");
    }

    #[test]
    fn admitted_queued_and_running_calls_retain_exact_handles_until_delivery() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            reset_admission_metrics_for_test();
            let asyncio = py.import("asyncio")?;
            let state = Arc::new(ProductionState::new());
            let event_loop = asyncio.call_method0("new_event_loop")?;
            asyncio.call_method1("set_event_loop", (&event_loop,))?;
            let binding = RunBinding::new(py, &state, &event_loop)?;
            state.bind(&binding)?;
            let scene = binding.next_scene(py)?;

            let globals = PyDict::new(py);
            globals.set_item("Actor", py.get_type::<Actor>())?;
            py.run(
                c"import asyncio\n\nclass MetricActor(Actor):\n    async def cued(self, cue):\n        entered.set()\n        try:\n            await release.wait()\n        except asyncio.CancelledError:\n            cleanup_entered.set()\n            await release.wait()\n            raise\n\nasync def consume(runner):\n    return await runner\n\nasync def bounded(awaitable):\n    return await asyncio.wait_for(asyncio.shield(awaitable), 5.0)\n\nasync def wait_event(event):\n    await asyncio.wait_for(event.wait(), 5.0)\n\nasync def barrier():\n    reached = asyncio.get_running_loop().create_future()\n    asyncio.get_running_loop().call_soon(reached.set_result, None)\n    await reached\n",
                Some(&globals),
                None,
            )?;
            let entered = asyncio.getattr("Event")?.call0()?;
            let cleanup_entered = asyncio.getattr("Event")?.call0()?;
            let release = asyncio.getattr("Event")?.call0()?;
            globals.set_item("entered", &entered)?;
            globals.set_item("cleanup_entered", &cleanup_entered)?;
            globals.set_item("release", &release)?;
            let actor_type = globals
                .get_item("MetricActor")?
                .expect("MetricActor must exist")
                .cast_into::<PyType>()?;
            let name = PyString::new(py, "admitted-handle-control");
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
                Arc::downgrade(&state),
            ));
            let node = Py::new(py, ActorCapabilityNode::new(Arc::clone(&capability)))?;
            capability.attach_node(node.bind(py))?;
            let running_handle = Py::new(py, ActorHandle::from_node(node.clone_ref(py)))?;
            let queued_handle = Py::new(py, ActorHandle::from_node(node))?;
            let running_baseline = unsafe { pyo3::ffi::Py_REFCNT(running_handle.as_ptr()) };
            let queued_baseline = unsafe { pyo3::ffi::Py_REFCNT(queued_handle.as_ptr()) };
            let running_runner = Py::new(
                py,
                CueCall::new(
                    running_handle.clone_ref(py).into_any(),
                    PyDict::new(py).unbind(),
                    Arc::clone(&capability),
                    &binding,
                    &scene,
                ),
            )?;
            let queued_runner = Py::new(
                py,
                CueCall::new(
                    queued_handle.clone_ref(py).into_any(),
                    PyDict::new(py).unbind(),
                    capability,
                    &binding,
                    &scene,
                ),
            )?;
            assert_eq!(
                unsafe { pyo3::ffi::Py_REFCNT(running_handle.as_ptr()) },
                running_baseline + 1
            );
            assert_eq!(
                unsafe { pyo3::ffi::Py_REFCNT(queued_handle.as_ptr()) },
                queued_baseline + 1
            );

            let consume = globals
                .get_item("consume")?
                .expect("consumer must exist");
            let running_consumer = consume.call1((running_runner.bind(py),))?;
            let queued_consumer = consume.call1((queued_runner.bind(py),))?;
            let running_task = event_loop.call_method1("create_task", (running_consumer,))?;
            let queued_task = event_loop.call_method1("create_task", (queued_consumer,))?;
            binding.register_task(&running_task, TaskLineage::from_scene(&scene))?;
            binding.register_task(&queued_task, TaskLineage::from_scene(&scene))?;
            let wait_entered = globals
                .get_item("wait_event")?
                .expect("event waiter must exist")
                .call1((&entered,))?;
            event_loop.call_method1("run_until_complete", (wait_entered,))?;
            let barrier = globals
                .get_item("barrier")?
                .expect("barrier must exist")
                .call0()?;
            event_loop.call_method1("run_until_complete", (barrier,))?;

            assert!(running_runner.bind(py).borrow().handle.as_ref().is_some_and(
                |edge| edge.as_ptr() == running_handle.as_ptr()
            ));
            assert!(queued_runner.bind(py).borrow().handle.as_ref().is_some_and(
                |edge| edge.as_ptr() == queued_handle.as_ptr()
            ));
            assert_eq!(
                unsafe { pyo3::ffi::Py_REFCNT(running_handle.as_ptr()) },
                running_baseline + 1
            );
            assert_eq!(
                unsafe { pyo3::ffi::Py_REFCNT(queued_handle.as_ptr()) },
                queued_baseline + 1
            );

            queued_task.call_method0("cancel")?;
            let bounded_queued = globals
                .get_item("bounded")?
                .expect("bounded waiter must exist")
                .call1((&queued_task,))?;
            let queued_error = event_loop
                .call_method1("run_until_complete", (bounded_queued,))
                .expect_err("queued caller must receive cancellation before running release");
            assert!(queued_error.is_instance(py, &asyncio.getattr("CancelledError")?));
            assert!(queued_runner.bind(py).borrow().handle.is_none());
            assert_eq!(
                unsafe { pyo3::ffi::Py_REFCNT(queued_handle.as_ptr()) },
                queued_baseline
            );
            assert!(running_runner.bind(py).borrow().handle.is_some());

            running_task.call_method0("cancel")?;
            let wait_cleanup = globals
                .get_item("wait_event")?
                .expect("event waiter must exist")
                .call1((&cleanup_entered,))?;
            event_loop.call_method1("run_until_complete", (wait_cleanup,))?;
            assert!(running_runner.bind(py).borrow().handle.as_ref().is_some_and(
                |edge| edge.as_ptr() == running_handle.as_ptr()
            ));
            assert_eq!(
                unsafe { pyo3::ffi::Py_REFCNT(running_handle.as_ptr()) },
                running_baseline + 1
            );
            release.call_method0("set")?;
            let bounded_running = globals
                .get_item("bounded")?
                .expect("bounded waiter must exist")
                .call1((&running_task,))?;
            let running_error = event_loop
                .call_method1("run_until_complete", (bounded_running,))
                .expect_err("running caller must receive cancellation after cleanup");
            assert!(running_error.is_instance(py, &asyncio.getattr("CancelledError")?));
            assert!(running_runner.bind(py).borrow().handle.is_none());
            assert_eq!(
                unsafe { pyo3::ffi::Py_REFCNT(running_handle.as_ptr()) },
                running_baseline
            );

            scene.close();
            event_loop.call_method0("close")?;
            asyncio.call_method1("set_event_loop", (py.None(),))?;

            assert_eq!(
                admission_metrics_for_test(),
                AdmissionMetrics {
                    copies: 2,
                    indexes: 2,
                    cues: 2,
                    registers: 2,
                    enqueues: 2,
                    drains: 2,
                    ..AdmissionMetrics::default()
                }
            );
            Ok::<_, PyErr>(())
        })
        .expect("admitted CueCalls must retain and release their exact Handle wrappers");
    }
}
