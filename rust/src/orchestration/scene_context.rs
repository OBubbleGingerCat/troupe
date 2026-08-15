#[cfg(test)]
use std::cell::Cell;
#[cfg(test)]
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use pyo3::class::gc::{PyTraverseError, PyVisit};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict, PyInt, PyString, PyTuple, PyTupleMethods};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::agent::AgentTurnControl;
use crate::diagnostic_runtime::cue_producer::{self, CueHook};
use crate::diagnostic_runtime::scene_drain_producer::{self, SceneDrainHook, SceneDriverExit};
use crate::diagnostic_runtime::scene_producer::{self, SceneHook};
use crate::orchestration::DiagnosticAdmissionSlot;
use crate::orchestration::actor_registry::ProductionState;
use crate::orchestration::mailbox::CueOperation;
use crate::orchestration::python_task::{
    ProvisionalPermitGuard, ProvisionalPermitStack, TaskFactoryWrapper, TaskLineage,
    TaskLineageRegistry,
};

pub(crate) const CUE_CONTEXT_ERROR: &str =
    "ActorHandle.cue() must be called within an active scene context";
pub(crate) const FACTORY_REPLACED_ERROR: &str =
    "Production event loop task factory was replaced while runtime was active";

#[derive(Clone, Copy)]
pub(crate) enum TaskFactoryAction {
    Install,
    Check,
    Restore,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn decimal_from_py_int(py: Python<'_>, value: &Py<PyInt>) -> PyResult<String> {
    const BASE: u64 = 1_000_000_000_000_000_000;
    const WIDTH: usize = 18;

    let negative = value.bind(py).lt(0)?;
    let mut magnitude = if negative {
        value.bind(py).neg()?.cast_into::<PyInt>()?.unbind()
    } else {
        value.clone_ref(py)
    };
    let divisor = BASE.into_pyobject(py)?;
    let mut chunks = Vec::new();
    loop {
        let parts = magnitude
            .bind(py)
            .divmod(&divisor)?
            .cast_into::<PyTuple>()?;
        let quotient = parts.get_item(0)?.cast_into::<PyInt>()?.unbind();
        chunks.push(parts.get_item(1)?.extract::<u64>()?);
        if !quotient.bind(py).is_truthy()? {
            break;
        }
        magnitude = quotient;
    }

    let mut chunks = chunks.into_iter().rev();
    let first = chunks.next().expect("decimal conversion emits one chunk");
    let mut text = if negative {
        format!("-{first}")
    } else {
        first.to_string()
    };
    for chunk in chunks {
        write!(&mut text, "{chunk:0WIDTH$}").expect("writing to String cannot fail");
    }
    Ok(text)
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct AdmissionMetrics {
    pub(crate) copies: usize,
    pub(crate) indexes: usize,
    pub(crate) cues: usize,
    pub(crate) registers: usize,
    pub(crate) enqueues: usize,
    pub(crate) cancels: usize,
    pub(crate) drains: usize,
    pub(crate) rollbacks: usize,
}

#[cfg(test)]
thread_local! {
    static GLOBAL_ADMISSION_METRICS: Cell<AdmissionMetrics> =
        const { Cell::new(AdmissionMetrics {
            copies: 0,
            indexes: 0,
            cues: 0,
            registers: 0,
            enqueues: 0,
            cancels: 0,
            drains: 0,
            rollbacks: 0,
        }) };
}

#[cfg(test)]
pub(crate) enum AdmissionMetric {
    Copy,
    Index,
    Cue,
    Register,
    Enqueue,
    Cancel,
    Drain,
    Rollback,
}

#[cfg(test)]
pub(crate) fn reset_admission_metrics_for_test() {
    GLOBAL_ADMISSION_METRICS.set(AdmissionMetrics::default());
}

#[cfg(test)]
pub(crate) fn admission_metrics_for_test() -> AdmissionMetrics {
    GLOBAL_ADMISSION_METRICS.get()
}

#[cfg(test)]
pub(crate) fn record_admission_metric_for_test(metric: AdmissionMetric) {
    GLOBAL_ADMISSION_METRICS.with(|cell| {
        let mut metrics = cell.get();
        match metric {
            AdmissionMetric::Copy => metrics.copies += 1,
            AdmissionMetric::Index => metrics.indexes += 1,
            AdmissionMetric::Cue => metrics.cues += 1,
            AdmissionMetric::Register => metrics.registers += 1,
            AdmissionMetric::Enqueue => metrics.enqueues += 1,
            AdmissionMetric::Cancel => metrics.cancels += 1,
            AdmissionMetric::Drain => metrics.drains += 1,
            AdmissionMetric::Rollback => metrics.rollbacks += 1,
        }
        cell.set(metrics);
    });
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AdmissionState {
    Open,
    Admitting,
    ClosePending,
    Closing,
    Closed,
}

enum ScenePhase {
    Open,
    Admitting(u64),
    ClosePending(u64),
    Closing,
    Closed,
}

struct SceneState {
    phase: ScenePhase,
    next_token: u64,
    operations: Vec<CueOperation>,
}

pub(crate) struct SceneScope {
    name: Py<PyString>,
    binding: Weak<RunBinding>,
    state: Mutex<SceneState>,
    pub(crate) counter: Mutex<Py<PyInt>>,
    closed: Notify,
}

impl SceneScope {
    fn observe_closed(&self) {
        scene_drain_producer::observe(self, SceneDrainHook::CleanupFinished);
        scene_producer::observe_scene(self, SceneHook::SceneFinished);
    }

    fn new(name: Py<PyString>, counter: Py<PyInt>, binding: Weak<RunBinding>) -> Arc<Self> {
        Arc::new(Self {
            name,
            binding,
            state: Mutex::new(SceneState {
                phase: ScenePhase::Open,
                next_token: 0,
                operations: Vec::new(),
            }),
            counter: Mutex::new(counter),
            closed: Notify::new(),
        })
    }

    pub(crate) fn new_runtime(
        py: Python<'_>,
        name: &str,
        binding: &Arc<RunBinding>,
    ) -> PyResult<Arc<Self>> {
        let zero = 0_i64.into_pyobject(py)?.cast_into::<PyInt>()?.unbind();
        let scene = Self::new(
            PyString::new(py, name).unbind(),
            zero,
            Arc::downgrade(binding),
        );
        scene_producer::observe_scene(&scene, SceneHook::SceneCreated);
        Ok(scene)
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(py: Python<'_>, name: &str, counter: Py<PyInt>) -> Arc<Self> {
        Self::new(PyString::new(py, name).unbind(), counter, Weak::new())
    }

    #[cfg(test)]
    pub(crate) fn zero_for_test(py: Python<'_>, name: &str) -> PyResult<Arc<Self>> {
        let zero = 0_i64.into_pyobject(py)?.cast_into::<PyInt>()?.unbind();
        Ok(Self::new_for_test(py, name, zero))
    }

    #[cfg(test)]
    pub(crate) fn zero_for_binding_for_test(
        py: Python<'_>,
        name: &str,
        binding: &Arc<RunBinding>,
    ) -> PyResult<Arc<Self>> {
        let zero = 0_i64.into_pyobject(py)?.cast_into::<PyInt>()?.unbind();
        Ok(Self::new(
            PyString::new(py, name).unbind(),
            zero,
            Arc::downgrade(binding),
        ))
    }

    pub(crate) fn name(&self, py: Python<'_>) -> Py<PyString> {
        self.name.clone_ref(py)
    }

    pub(crate) fn binding(&self) -> Option<Arc<RunBinding>> {
        self.binding.upgrade()
    }

    fn traverse_python_edges(&self, visit: &PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.name)?;
        visit.call(&*lock(&self.counter))
    }

    pub(crate) fn begin_admission(self: &Arc<Self>) -> Option<AdmissionTxn> {
        let token = {
            let mut state = lock(&self.state);
            if !matches!(state.phase, ScenePhase::Open) {
                return None;
            }
            let token = state.next_token;
            state.next_token = state.next_token.wrapping_add(1);
            state.phase = ScenePhase::Admitting(token);
            token
        };
        Some(AdmissionTxn {
            scope: Arc::clone(self),
            token,
            transferred: false,
        })
    }

    pub(crate) fn close(&self) {
        if let Some(binding) = self.binding() {
            Python::attach(|py| binding.ensure_wrapper_for_drain(py));
        }
        enum CloseAction {
            Closed,
            Pending,
            Cancel(Vec<CueOperation>),
        }
        let action = {
            let mut state = lock(&self.state);
            match state.phase {
                ScenePhase::Open => {
                    if state.operations.is_empty() {
                        state.phase = ScenePhase::Closed;
                        CloseAction::Closed
                    } else {
                        state.phase = ScenePhase::Closing;
                        CloseAction::Cancel(state.operations.clone())
                    }
                }
                ScenePhase::Admitting(token) => {
                    state.phase = ScenePhase::ClosePending(token);
                    CloseAction::Pending
                }
                ScenePhase::ClosePending(_) | ScenePhase::Closing | ScenePhase::Closed => return,
            }
        };
        scene_drain_producer::observe(self, SceneDrainHook::AdmissionClosed);
        scene_drain_producer::observe(self, SceneDrainHook::CleanupStarted);
        match action {
            CloseAction::Closed => {
                self.observe_closed();
                self.closed.notify_waiters();
            }
            CloseAction::Pending => {}
            CloseAction::Cancel(operations) => {
                scene_drain_producer::observe(self, SceneDrainHook::CancellationStarted);
                self.cancel_operations(operations);
            }
        }
    }

    fn cancel_operations(&self, operations: Vec<CueOperation>) {
        #[cfg(test)]
        let cancelled = operations
            .into_iter()
            .filter(CueOperation::request_cancel)
            .count();
        #[cfg(test)]
        for _ in 0..cancelled {
            record_admission_metric_for_test(AdmissionMetric::Cancel);
        }
        #[cfg(not(test))]
        for operation in operations {
            operation.request_cancel();
        }
    }

    pub(crate) fn operation_finished(&self, operation: &CueOperation) -> Option<CueOperation> {
        #[cfg(test)]
        assert!(
            operation.actor_mailbox_unlinked_for_test(),
            "Scene deregistration must follow exact Actor mailbox retirement"
        );
        let (removed, closed) = {
            let mut state = lock(&self.state);
            let removed = state
                .operations
                .iter()
                .position(|current| current.ptr_eq(operation))
                .map(|index| state.operations.remove(index));
            if removed.is_some() {
                #[cfg(test)]
                record_admission_metric_for_test(AdmissionMetric::Drain);
            }
            let closed =
                if matches!(state.phase, ScenePhase::Closing) && state.operations.is_empty() {
                    state.phase = ScenePhase::Closed;
                    true
                } else {
                    false
                };
            (removed, closed)
        };
        if closed {
            self.observe_closed();
            self.closed.notify_waiters();
        }
        removed
    }

    pub(crate) async fn wait_closed(&self) {
        loop {
            let notified = self.closed.notified();
            let (closed, transitioned) = {
                let mut state = lock(&self.state);
                if matches!(state.phase, ScenePhase::Closing) && state.operations.is_empty() {
                    state.phase = ScenePhase::Closed;
                    (true, true)
                } else {
                    (matches!(state.phase, ScenePhase::Closed), false)
                }
            };
            if closed {
                if transitioned {
                    self.observe_closed();
                }
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        matches!(lock(&self.state).phase, ScenePhase::Open)
    }

    fn rollback_admission(&self, token: u64) {
        let operations = {
            let mut state = lock(&self.state);
            match state.phase {
                ScenePhase::Admitting(current) if current == token => {
                    state.phase = ScenePhase::Open;
                    None
                }
                ScenePhase::ClosePending(current) if current == token => {
                    state.phase = ScenePhase::Closing;
                    Some(state.operations.clone())
                }
                _ => return,
            }
        };
        #[cfg(test)]
        record_admission_metric_for_test(AdmissionMetric::Rollback);
        if let Some(operations) = operations {
            if !operations.is_empty() {
                scene_drain_producer::observe(self, SceneDrainHook::CancellationStarted);
            }
            self.cancel_operations(operations);
        }
    }

    #[cfg(test)]
    fn admission_state(&self) -> AdmissionState {
        match lock(&self.state).phase {
            ScenePhase::Open => AdmissionState::Open,
            ScenePhase::Admitting(_) => AdmissionState::Admitting,
            ScenePhase::ClosePending(_) => AdmissionState::ClosePending,
            ScenePhase::Closing => AdmissionState::Closing,
            ScenePhase::Closed => AdmissionState::Closed,
        }
    }

    #[cfg(test)]
    pub(crate) fn state_lock_available_for_test(&self) -> bool {
        self.state.try_lock().is_ok()
    }

    #[cfg(test)]
    pub(crate) fn operation_count_for_test(&self) -> usize {
        lock(&self.state).operations.len()
    }

    #[cfg(test)]
    pub(crate) fn operation_registered_weak_for_test(
        &self,
        expected: &Weak<crate::orchestration::mailbox::CueOperationInner>,
    ) -> bool {
        lock(&self.state)
            .operations
            .iter()
            .any(|operation| operation.downgrade_for_test().ptr_eq(expected))
    }
}

pub(crate) struct AdmissionTxn {
    scope: Arc<SceneScope>,
    token: u64,
    transferred: bool,
}

impl AdmissionTxn {
    fn prepare_with_probe(
        mut self,
        py: Python<'_>,
        mut probe: impl FnMut(),
    ) -> PyResult<PreparedAdmission> {
        let current = {
            let counter = lock(&self.scope.counter);
            counter.clone_ref(py)
        };
        probe();
        let next = current
            .bind(py)
            .call_method1("__add__", (1,))?
            .cast_into::<PyInt>()?
            .unbind();
        probe();
        let text = decimal_from_py_int(py, &current)?;
        probe();
        let id = format!("{}-cue{text}", self.scope.name.bind(py).to_str()?);
        #[cfg(test)]
        record_admission_metric_for_test(AdmissionMetric::Index);
        self.transferred = true;
        Ok(PreparedAdmission {
            scope: Arc::clone(&self.scope),
            token: self.token,
            id,
            next: Some(next),
            committed: false,
        })
    }

    pub(crate) fn prepare(self, py: Python<'_>) -> PyResult<PreparedAdmission> {
        self.prepare_with_probe(py, || {})
    }
}

impl Drop for AdmissionTxn {
    fn drop(&mut self) {
        if self.transferred {
            return;
        }
        self.scope.rollback_admission(self.token);
    }
}

pub(crate) struct PreparedAdmission {
    scope: Arc<SceneScope>,
    token: u64,
    id: String,
    next: Option<Py<PyInt>>,
    committed: bool,
}

impl PreparedAdmission {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn commit(mut self, operation: CueOperation) -> PyResult<()> {
        let (old, operations_to_cancel) = {
            let mut state = lock(&self.scope.state);
            let close_pending = match state.phase {
                ScenePhase::Admitting(token) if token == self.token => {
                    state.phase = ScenePhase::Open;
                    false
                }
                ScenePhase::ClosePending(token) if token == self.token => {
                    state.phase = ScenePhase::Closing;
                    true
                }
                _ => panic!("admission token must remain unique until commit"),
            };
            let mut counter = lock(&self.scope.counter);
            let old = std::mem::replace(
                &mut *counter,
                self.next.take().expect("prepared counter must exist"),
            );
            state.operations.push(operation.clone());
            #[cfg(test)]
            record_admission_metric_for_test(AdmissionMetric::Register);
            let operations_to_cancel = close_pending.then(|| state.operations.clone());
            (old, operations_to_cancel)
        };
        drop(old);
        cue_producer::observe(&operation, CueHook::Admitted);
        if let Some(operations) = operations_to_cancel {
            if !operations.is_empty() {
                scene_drain_producer::observe(&self.scope, SceneDrainHook::CancellationStarted);
            }
            self.scope.cancel_operations(operations);
        }
        operation.enqueue()?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for PreparedAdmission {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        self.scope.rollback_admission(self.token);
    }
}

#[derive(Default)]
pub(crate) struct SceneNameGenerator {
    #[cfg(test)]
    sequence: VecDeque<Uuid>,
}

impl SceneNameGenerator {
    pub(crate) fn next_name(&mut self) -> String {
        #[cfg(test)]
        if let Some(value) = self.sequence.pop_front() {
            return format!("scene-{value}");
        }
        format!("scene-{}", Uuid::new_v4())
    }

    #[cfg(test)]
    pub(crate) fn from_sequence_for_test(values: impl IntoIterator<Item = Uuid>) -> Self {
        Self {
            sequence: values.into_iter().collect(),
        }
    }
}

pub(crate) struct CuedScope {
    scene: Arc<SceneScope>,
    source: Py<PyString>,
    actor_identity: usize,
    cue_id: Py<PyString>,
    effect_counter: Mutex<Py<PyInt>>,
    agent_turns: Mutex<Vec<Weak<AgentTurnControl>>>,
    active: AtomicBool,
}

impl CuedScope {
    pub(crate) fn new(
        py: Python<'_>,
        scene: Arc<SceneScope>,
        source: Py<PyString>,
        actor_identity: usize,
        cue_id: Py<PyString>,
    ) -> PyResult<Arc<Self>> {
        let zero = 0_i64.into_pyobject(py)?.cast_into::<PyInt>()?.unbind();
        Ok(Arc::new(Self {
            scene,
            source,
            actor_identity,
            cue_id,
            effect_counter: Mutex::new(zero),
            agent_turns: Mutex::new(Vec::new()),
            active: AtomicBool::new(true),
        }))
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        scene: Arc<SceneScope>,
        source: &str,
        actor_identity: usize,
    ) -> Arc<Self> {
        Python::attach(|py| {
            Arc::new(Self {
                scene,
                source: PyString::new(py, source).unbind(),
                actor_identity,
                cue_id: PyString::new(py, "test-cue0").unbind(),
                effect_counter: Mutex::new(
                    0_i64
                        .into_pyobject(py)
                        .expect("zero must convert to Python")
                        .cast_into::<PyInt>()
                        .expect("zero must be a Python int")
                        .unbind(),
                ),
                agent_turns: Mutex::new(Vec::new()),
                active: AtomicBool::new(true),
            })
        })
    }

    pub(crate) fn close_inline(&self) {
        let turns = {
            let mut registered = lock(&self.agent_turns);
            self.active.store(false, Ordering::Release);
            registered
                .drain(..)
                .filter_map(|turn| turn.upgrade())
                .collect::<Vec<_>>()
        };
        for turn in turns {
            turn.request_cancel();
        }
    }

    pub(crate) fn register_agent_turn(&self, turn: &Arc<AgentTurnControl>) -> bool {
        let mut registered = lock(&self.agent_turns);
        if !self.active.load(Ordering::Acquire) {
            return false;
        }
        registered.retain(|existing| existing.strong_count() != 0);
        registered.push(Arc::downgrade(turn));
        true
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub(crate) fn scene(&self) -> Arc<SceneScope> {
        Arc::clone(&self.scene)
    }

    pub(crate) fn source(&self, py: Python<'_>) -> Py<PyString> {
        self.source.clone_ref(py)
    }

    #[allow(dead_code)]
    pub(crate) fn cue_id(&self, py: Python<'_>) -> Py<PyString> {
        self.cue_id.clone_ref(py)
    }

    pub(crate) fn actor_identity(&self) -> usize {
        self.actor_identity
    }

    pub(crate) fn next_effect_id(&self, py: Python<'_>) -> PyResult<Py<PyString>> {
        let current = {
            let counter = lock(&self.effect_counter);
            counter.clone_ref(py)
        };
        let next = current
            .bind(py)
            .call_method1("__add__", (1,))?
            .cast_into::<PyInt>()?
            .unbind();
        let index = decimal_from_py_int(py, &current)?;
        let id = PyString::new(
            py,
            &format!("{}-effect{index}", self.cue_id.bind(py).to_str()?),
        )
        .unbind();
        let old = {
            let mut counter = lock(&self.effect_counter);
            std::mem::replace(&mut *counter, next)
        };
        drop(old);
        Ok(id)
    }

    pub(crate) fn traverse_python_edges(&self, visit: &PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.source)?;
        visit.call(&self.cue_id)?;
        visit.call(&*lock(&self.effect_counter))?;
        self.scene.traverse_python_edges(visit)
    }

    #[cfg(test)]
    pub(crate) fn scene_for_test(&self) -> &Arc<SceneScope> {
        &self.scene
    }

    #[cfg(test)]
    pub(crate) fn source_for_test(&self, py: Python<'_>) -> PyResult<String> {
        self.source.bind(py).extract()
    }

    #[cfg(test)]
    pub(crate) fn actor_identity_for_test(&self) -> usize {
        self.actor_identity
    }
}

pub(crate) struct RunBinding {
    production: Weak<ProductionState>,
    event_loop: Py<PyAny>,
    thread_id: std::thread::ThreadId,
    pid: u32,
    current_task_lookup: Py<PyAny>,
    running_loop_lookup: Py<PyAny>,
    tasks: Mutex<TaskLineageRegistry>,
    permits: ProvisionalPermitStack,
    names: Mutex<SceneNameGenerator>,
    original_factory: Py<PyAny>,
    wrapper: Mutex<Option<Py<TaskFactoryWrapper>>>,
    factory_replaced: AtomicBool,
    diagnostic_admission: DiagnosticAdmissionSlot,
}

impl RunBinding {
    pub(crate) fn new(
        py: Python<'_>,
        production: &Arc<ProductionState>,
        event_loop: &Bound<'_, PyAny>,
    ) -> PyResult<Arc<Self>> {
        let original_factory = event_loop.call_method0("get_task_factory")?.unbind();
        let asyncio = py.import("asyncio")?;
        let binding = Arc::new(Self {
            production: Arc::downgrade(production),
            event_loop: event_loop.clone().unbind(),
            thread_id: std::thread::current().id(),
            pid: std::process::id(),
            current_task_lookup: asyncio.getattr("current_task")?.unbind(),
            running_loop_lookup: asyncio.getattr("get_running_loop")?.unbind(),
            tasks: Mutex::new(TaskLineageRegistry::default()),
            permits: ProvisionalPermitStack::default(),
            names: Mutex::new(SceneNameGenerator::default()),
            original_factory,
            wrapper: Mutex::new(None),
            factory_replaced: AtomicBool::new(false),
            diagnostic_admission: DiagnosticAdmissionSlot::new(),
        });
        scene_producer::binding_created(&binding);
        let wrapper = Py::new(py, TaskFactoryWrapper::new(Arc::downgrade(&binding)))?;
        *lock(&binding.wrapper) = Some(wrapper);
        Ok(binding)
    }

    pub(crate) fn event_loop(&self, py: Python<'_>) -> Py<PyAny> {
        self.event_loop.clone_ref(py)
    }

    #[allow(dead_code)]
    pub(crate) fn production(&self) -> Option<Arc<ProductionState>> {
        self.production.upgrade()
    }

    pub(crate) fn diagnostic_admission(&self) -> &DiagnosticAdmissionSlot {
        &self.diagnostic_admission
    }

    pub(crate) fn traverse(&self, visit: &PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.event_loop)?;
        visit.call(&self.current_task_lookup)?;
        visit.call(&self.running_loop_lookup)?;
        visit.call(&self.original_factory)?;
        visit.call(&*lock(&self.wrapper))?;
        lock(&self.tasks).traverse(visit)?;
        self.permits.traverse(visit)?;
        self.diagnostic_admission.traverse(visit)
    }

    pub(crate) fn production_matches(&self, production: &Arc<ProductionState>) -> bool {
        self.production
            .upgrade()
            .is_some_and(|current| Arc::ptr_eq(&current, production))
    }

    pub(crate) fn next_scene(self: &Arc<Self>, py: Python<'_>) -> PyResult<Arc<SceneScope>> {
        let name = lock(&self.names).next_name();
        SceneScope::new_runtime(py, &name, self)
    }

    pub(crate) fn install_wrapper(&self, py: Python<'_>) -> PyResult<()> {
        let current = self.event_loop.bind(py).call_method0("get_task_factory")?;
        if !current.is(self.original_factory.bind(py)) {
            self.factory_replaced.store(true, Ordering::Release);
            return Err(PyRuntimeError::new_err(FACTORY_REPLACED_ERROR));
        }
        let wrapper = lock(&self.wrapper)
            .as_ref()
            .expect("runtime wrapper must be initialized")
            .clone_ref(py);
        self.event_loop
            .bind(py)
            .call_method1("set_task_factory", (wrapper,))?;
        Ok(())
    }

    pub(crate) fn ensure_wrapper_for_drain(&self, py: Python<'_>) {
        let Some(wrapper) = lock(&self.wrapper)
            .as_ref()
            .map(|wrapper| wrapper.clone_ref(py))
        else {
            return;
        };
        let loop_ = self.event_loop.bind(py);
        match loop_.call_method0("get_task_factory") {
            Ok(current) if current.is(wrapper.bind(py)) => {}
            Ok(_) => {
                self.factory_replaced.store(true, Ordering::Release);
                let _ = loop_.call_method1("set_task_factory", (wrapper,));
            }
            Err(_) => {
                self.factory_replaced.store(true, Ordering::Release);
            }
        }
    }

    pub(crate) fn check_wrapper(&self, py: Python<'_>) -> PyResult<()> {
        if self.factory_replaced() {
            return Err(PyRuntimeError::new_err(FACTORY_REPLACED_ERROR));
        }
        let wrapper = lock(&self.wrapper)
            .as_ref()
            .map(|wrapper| wrapper.clone_ref(py));
        let Some(wrapper) = wrapper else {
            #[cfg(test)]
            return Ok(());
            #[cfg(not(test))]
            panic!("runtime wrapper must be initialized");
        };
        let current = match self.event_loop.bind(py).call_method0("get_task_factory") {
            Ok(current) => current,
            Err(_) => {
                self.mark_factory_replaced();
                return Err(PyRuntimeError::new_err(FACTORY_REPLACED_ERROR));
            }
        };
        if !current.is(wrapper.bind(py)) {
            self.mark_factory_replaced();
            return Err(PyRuntimeError::new_err(FACTORY_REPLACED_ERROR));
        }
        Ok(())
    }

    pub(crate) fn restore_factory(&self, py: Python<'_>) -> PyResult<()> {
        let check = self.check_wrapper(py);
        let restore = self
            .event_loop
            .bind(py)
            .call_method1("set_task_factory", (self.original_factory.bind(py),))
            .map(|_| ());
        match check {
            Err(error) => Err(error),
            Ok(()) => restore,
        }
    }

    pub(crate) fn factory_replaced(&self) -> bool {
        self.factory_replaced.load(Ordering::Acquire)
    }

    pub(crate) fn mark_factory_replaced(&self) {
        self.factory_replaced.store(true, Ordering::Release);
    }

    fn current_task<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyAny>>> {
        let kwargs = PyDict::new(py);
        kwargs.set_item("loop", self.event_loop.bind(py))?;
        let task = self.current_task_lookup.bind(py).call((), Some(&kwargs))?;
        Ok((!task.is_none()).then_some(task))
    }

    pub(crate) fn current_lineage(&self, py: Python<'_>) -> PyResult<Option<TaskLineage>> {
        if self.pid != std::process::id() || self.thread_id != std::thread::current().id() {
            return Ok(None);
        }
        let asyncio = match py.import("asyncio") {
            Ok(asyncio) => asyncio,
            Err(_) => return Ok(None),
        };
        let current_task_is_canonical = asyncio
            .getattr("current_task")
            .is_ok_and(|lookup| lookup.is(self.current_task_lookup.bind(py)));
        let running_loop_is_canonical = asyncio
            .getattr("get_running_loop")
            .is_ok_and(|lookup| lookup.is(self.running_loop_lookup.bind(py)));
        if !current_task_is_canonical || !running_loop_is_canonical {
            return Ok(None);
        }
        let running_loop = match self.running_loop_lookup.bind(py).call0() {
            Ok(loop_) => loop_,
            Err(_) => return Ok(None),
        };
        if !running_loop.is(self.event_loop.bind(py)) {
            return Ok(None);
        }
        let Some(task) = self.current_task(py)? else {
            return Ok(None);
        };
        let mut tasks = lock(&self.tasks);
        if let Some(lineage) = tasks.lookup(&task)? {
            return Ok(Some(lineage));
        }
        drop(tasks);
        let eager = self.permits.lineage_for_running_eager_task(&task)?;
        if let Some(lineage) = eager {
            lock(&self.tasks).register(&task, lineage.clone())?;
            return Ok(Some(lineage));
        }
        Ok(None)
    }

    pub(crate) fn validate_lineage_for_scene(
        &self,
        py: Python<'_>,
        expected: &Arc<SceneScope>,
    ) -> PyResult<TaskLineage> {
        let lineage = self
            .current_lineage(py)?
            .filter(TaskLineage::is_active)
            .ok_or_else(|| {
                crate::orchestration::cue::CueContextError::new_err(CUE_CONTEXT_ERROR)
            })?;
        let scene = lineage
            .scene()
            .filter(|scene| Arc::ptr_eq(scene, expected))
            .ok_or_else(|| {
                crate::orchestration::cue::CueContextError::new_err(CUE_CONTEXT_ERROR)
            })?;
        if !scene.is_open() {
            return Err(crate::orchestration::cue::CueContextError::new_err(
                CUE_CONTEXT_ERROR,
            ));
        }
        Ok(lineage)
    }

    pub(crate) fn register_task(
        &self,
        task: &Bound<'_, PyAny>,
        lineage: TaskLineage,
    ) -> PyResult<()> {
        lock(&self.tasks).register(task, lineage)
    }

    fn unregister_task(&self, task: &Bound<'_, PyAny>) {
        lock(&self.tasks).unregister(task);
    }

    fn base_task_owns_coroutine(
        py: Python<'_>,
        task: &Bound<'_, PyAny>,
        coroutine: &Bound<'_, PyAny>,
    ) -> bool {
        py.import("asyncio")
            .and_then(|asyncio| asyncio.getattr("Task"))
            .and_then(|base_task| base_task.call_method1("get_coro", (task,)))
            .is_ok_and(|actual| actual.is(coroutine))
    }

    pub(crate) fn enter_task_permit(
        &self,
        py: Python<'_>,
        coroutine: &Bound<'_, PyAny>,
        lineage: TaskLineage,
    ) -> ProvisionalPermitGuard {
        self.permits.push(py, coroutine, lineage)
    }

    pub(crate) fn create_delegated_task(
        &self,
        py: Python<'_>,
        loop_: &Bound<'_, PyAny>,
        coroutine: &Bound<'_, PyAny>,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Py<PyAny>> {
        if !loop_.is(self.event_loop.bind(py)) {
            return Err(PyRuntimeError::new_err(
                "Production task factory was called for another event loop",
            ));
        }
        let exact_lineage = self.permits.consume_exact(coroutine);
        let lineage = match exact_lineage {
            Some(lineage) => Some(lineage),
            None => self.current_lineage(py)?.filter(TaskLineage::is_active),
        };
        let delegate_permit = lineage
            .as_ref()
            .map(|lineage| self.enter_task_permit(py, coroutine, lineage.clone()));
        let task_result = if self.original_factory.bind(py).is_none() {
            let task_kwargs = kwargs.map_or_else(|| Ok(PyDict::new(py)), Bound::copy)?;
            task_kwargs.set_item("loop", loop_)?;
            py.import("asyncio")?
                .getattr("Task")?
                .call((coroutine,), Some(&task_kwargs))
        } else {
            self.original_factory
                .bind(py)
                .call((loop_, coroutine), kwargs)
        };
        let eager_task = delegate_permit
            .as_ref()
            .and_then(|permit| permit.eager_task(py));
        drop(delegate_permit);
        let task = match task_result {
            Ok(task) => task,
            Err(error) => {
                if let Some(eager_task) = eager_task {
                    self.unregister_task(eager_task.bind(py));
                }
                return Err(error);
            }
        };
        if let Some(lineage) = lineage {
            match eager_task {
                Some(eager_task) if task.is(eager_task.bind(py)) => {
                    self.register_task(&task, lineage)?;
                }
                Some(eager_task) => {
                    self.unregister_task(eager_task.bind(py));
                }
                None if Self::base_task_owns_coroutine(py, &task, coroutine) => {
                    self.register_task(&task, lineage)?;
                }
                None => {}
            }
        }
        let current = loop_.call_method0("get_task_factory")?;
        let wrapper = lock(&self.wrapper)
            .as_ref()
            .expect("runtime wrapper must be initialized")
            .clone_ref(py);
        if !current.is(wrapper.bind(py)) {
            self.mark_factory_replaced();
        }
        Ok(task.unbind())
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(py: Python<'_>) -> PyResult<Self> {
        let asyncio = py.import("asyncio")?;
        Ok(Self {
            production: Weak::new(),
            event_loop: py.None(),
            thread_id: std::thread::current().id(),
            pid: std::process::id(),
            current_task_lookup: asyncio.getattr("current_task")?.unbind(),
            running_loop_lookup: asyncio.getattr("get_running_loop")?.unbind(),
            tasks: Mutex::new(TaskLineageRegistry::default()),
            permits: ProvisionalPermitStack::default(),
            names: Mutex::new(SceneNameGenerator::default()),
            original_factory: py.None(),
            wrapper: Mutex::new(None),
            factory_replaced: AtomicBool::new(false),
            diagnostic_admission: DiagnosticAdmissionSlot::new(),
        })
    }
}

enum ScopeOwner {
    Scene(Arc<SceneScope>),
    Cued(Arc<CuedScope>),
}

#[pyclass(name = "_ScopeDriver", module = "troupe._runtime")]
pub(crate) struct ScopeDriver {
    owner: ScopeOwner,
    inner: Mutex<Option<Py<PyAny>>>,
    owner_closed: AtomicBool,
}

impl ScopeDriver {
    pub(crate) fn new_scene(scene: Arc<SceneScope>, inner: Py<PyAny>) -> Self {
        Self {
            owner: ScopeOwner::Scene(scene),
            inner: Mutex::new(Some(inner)),
            owner_closed: AtomicBool::new(false),
        }
    }

    pub(crate) fn new_cued(cued: Arc<CuedScope>, inner: Py<PyAny>) -> Self {
        Self {
            owner: ScopeOwner::Cued(cued),
            inner: Mutex::new(Some(inner)),
            owner_closed: AtomicBool::new(false),
        }
    }

    fn close_owner(&self, exit: SceneDriverExit, error: Option<&PyErr>) {
        if self.owner_closed.swap(true, Ordering::AcqRel) {
            return;
        }
        match &self.owner {
            ScopeOwner::Scene(scene) => {
                scene_drain_producer::driver_exited(scene, exit, error);
                scene.close();
            }
            ScopeOwner::Cued(cued) => cued.close_inline(),
        }
    }

    fn drive(&self, py: Python<'_>, method: &str, value: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let inner = lock(&self.inner)
            .as_ref()
            .map(|inner| inner.clone_ref(py))
            .ok_or_else(|| PyRuntimeError::new_err("cannot reuse already awaited coroutine"))?;
        match inner.bind(py).call_method1(method, (value,)) {
            Ok(yielded) => Ok(yielded.unbind()),
            Err(error) => {
                let inner = lock(&self.inner).take();
                drop(inner);
                self.close_owner(SceneDriverExit::Returned, Some(&error));
                Err(error)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn new_scene_for_test(scene: Arc<SceneScope>) -> Self {
        Self {
            owner: ScopeOwner::Scene(scene),
            inner: Mutex::new(None),
            owner_closed: AtomicBool::new(false),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_cued_for_test(cued: Arc<CuedScope>) -> Self {
        Self {
            owner: ScopeOwner::Cued(cued),
            inner: Mutex::new(None),
            owner_closed: AtomicBool::new(false),
        }
    }
}

#[pymethods]
impl ScopeDriver {
    fn send(&self, py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        self.drive(py, "send", value)
    }

    fn throw(&self, py: Python<'_>, exc: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        self.drive(py, "throw", exc)
    }

    fn close(&self, py: Python<'_>) -> PyResult<()> {
        let inner = lock(&self.inner).take();
        let result = match inner {
            Some(inner) => inner.bind(py).call_method0("close").map(|_| ()),
            None => Ok(()),
        };
        self.close_owner(SceneDriverExit::Closed, result.as_ref().err());
        result
    }

    fn __await__(self_: Py<Self>) -> Py<Self> {
        self_
    }

    fn __iter__(self_: Py<Self>) -> Py<Self> {
        self_
    }

    fn __next__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.drive(py, "send", py.None().bind(py))
    }

    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        match &self.owner {
            ScopeOwner::Scene(scene) => scene.traverse_python_edges(&visit)?,
            ScopeOwner::Cued(cued) => cued.traverse_python_edges(&visit)?,
        }
        visit.call(&*lock(&self.inner))
    }

    fn __clear__(&self) {
        let inner = lock(&self.inner).take();
        drop(inner);
        self.close_owner(SceneDriverExit::Cleared, None);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use pyo3::exceptions::PyTypeError;
    use pyo3::prelude::*;
    use pyo3::types::{PyCFunction, PyDict, PyInt, PyModule, PyString, PyTuple, PyType};
    use uuid::Uuid;

    use super::{
        AdmissionMetrics, AdmissionState, CuedScope, PreparedAdmission, RunBinding,
        SceneNameGenerator, SceneScope, ScopeDriver, admission_metrics_for_test,
        reset_admission_metrics_for_test,
    };
    use crate::orchestration::actor::{Actor, ActorCapability, ActorIdentity, enter_actor_permit};
    use crate::orchestration::actor_registry::{NameKey, ProductionState};
    use crate::orchestration::cue::Cue;
    use crate::orchestration::mailbox::CueOperation;
    use crate::orchestration::python_task::TaskLineage;

    fn counter_value(py: Python<'_>, counter: &Mutex<Py<PyInt>>) -> String {
        let guard = match counter.lock() {
            Ok(counter) => counter,
            Err(poisoned) => poisoned.into_inner(),
        };
        let value = guard.clone_ref(py);
        drop(guard);
        py.get_type::<PyInt>()
            .call_method1("__str__", (value.bind(py),))
            .unwrap()
            .extract()
            .unwrap()
    }

    fn admission_locks_available(scope: &SceneScope) -> bool {
        scope.state_lock_available_for_test() && scope.counter.try_lock().is_ok()
    }

    struct CounterHarness {
        scope: Arc<SceneScope>,
        module: Py<PyModule>,
        lock_failures: Arc<AtomicUsize>,
    }

    struct IntDigitLimitRestore {
        previous: usize,
    }

    impl Drop for IntDigitLimitRestore {
        fn drop(&mut self) {
            Python::attach(|py| {
                if let Ok(sys) = py.import("sys") {
                    let _ = sys.call_method1("set_int_max_str_digits", (self.previous,));
                }
            });
        }
    }

    fn tracked_counter_scope(py: Python<'_>, name: &str, seed: &str) -> PyResult<CounterHarness> {
        let module = PyModule::from_code(
            py,
            c"on_drop = None\ndropped = []\nclass CounterProbe(int):\n    fail_format = False\n    def __new__(cls, value):\n        return int.__new__(cls, value)\n    def __add__(self, other):\n        return type(self)(int(self) + other)\n    def __divmod__(self, other):\n        if type(self).fail_format:\n            raise TypeError('counter formatting failed')\n        return divmod(int(self), other)\n    def __del__(self):\n        dropped.append(int.__str__(self))\n        if on_drop is not None:\n            on_drop()\n",
            c"counter_probe.py",
            c"counter_probe",
        )?;
        let counter_type = module.getattr("CounterProbe")?;
        let seed = counter_type.call1((seed,))?.cast_into::<PyInt>()?.unbind();
        let scope = SceneScope::new_for_test(py, name, seed);
        let _: &Mutex<Py<PyInt>> = &scope.counter;
        let lock_failures = Arc::new(AtomicUsize::new(0));
        let drop_scope: std::sync::Weak<SceneScope> = Arc::downgrade(&scope);
        let callback_lock_failures = Arc::clone(&lock_failures);
        let on_drop = PyCFunction::new_closure(
            py,
            None,
            None,
            move |_args: &Bound<'_, PyTuple>,
                  _kwargs: Option<&Bound<'_, PyDict>>|
                  -> PyResult<()> {
                if let Some(scope) = drop_scope.upgrade()
                    && !admission_locks_available(&scope)
                {
                    callback_lock_failures.fetch_add(1, Ordering::SeqCst);
                }
                Ok(())
            },
        )?;
        module.setattr("on_drop", on_drop)?;
        Ok(CounterHarness {
            scope,
            module: module.unbind(),
            lock_failures,
        })
    }

    fn dropped_counters(py: Python<'_>, harness: &CounterHarness) -> PyResult<Vec<String>> {
        harness.module.bind(py).getattr("dropped")?.extract()
    }

    #[test]
    fn counter_is_python_int_and_unbounded() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            const SEED: &str = "1606938044258990275541962092341162602522202993782792835301376";
            const NEXT: &str = "1606938044258990275541962092341162602522202993782792835301377";
            const NEXT_NEXT: &str = "1606938044258990275541962092341162602522202993782792835301378";
            let harness = tracked_counter_scope(py, "scene-counter", SEED)?;
            let scope = &harness.scope;
            let first_probe_calls = Cell::new(0);

            let first_prepared: PreparedAdmission = scope
                .begin_admission()
                .expect("first admission must begin")
                .prepare_with_probe(py, || {
                    assert!(scope.state_lock_available_for_test());
                    assert!(scope.counter.try_lock().is_ok());
                    assert_eq!(counter_value(py, &scope.counter), SEED);
                    first_probe_calls.set(first_probe_calls.get() + 1);
                })?;
            assert_eq!(first_probe_calls.get(), 3);
            assert_eq!(counter_value(py, &scope.counter), SEED);
            assert!(dropped_counters(py, &harness)?.is_empty());
            let first = first_prepared.id().to_owned();
            first_prepared.commit(CueOperation::new_for_test(0))?;
            assert_eq!(counter_value(py, &scope.counter), NEXT);
            assert_eq!(dropped_counters(py, &harness)?, [SEED]);
            assert_eq!(harness.lock_failures.load(Ordering::SeqCst), 0);

            let second_probe_calls = Cell::new(0);
            let second_prepared: PreparedAdmission = scope
                .begin_admission()
                .expect("second admission must begin")
                .prepare_with_probe(py, || {
                    assert!(scope.state_lock_available_for_test());
                    assert!(scope.counter.try_lock().is_ok());
                    assert_eq!(counter_value(py, &scope.counter), NEXT);
                    second_probe_calls.set(second_probe_calls.get() + 1);
                })?;
            assert_eq!(second_probe_calls.get(), 3);
            assert_eq!(counter_value(py, &scope.counter), NEXT);
            assert_eq!(dropped_counters(py, &harness)?, [SEED]);
            let second = second_prepared.id().to_owned();
            second_prepared.commit(CueOperation::new_for_test(1))?;
            assert_eq!(counter_value(py, &scope.counter), NEXT_NEXT);
            assert_eq!(dropped_counters(py, &harness)?, [SEED, NEXT]);
            assert_eq!(harness.lock_failures.load(Ordering::SeqCst), 0);
            assert_eq!(first, format!("scene-counter-cue{SEED}"));
            assert_eq!(second, format!("scene-counter-cue{NEXT}"));
            Ok::<_, PyErr>(())
        })
        .expect("the Python integer counter must remain unbounded");
    }

    #[test]
    fn cue_and_effect_ids_ignore_python_int_string_digit_limit() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let sys = py.import("sys")?;
            if !sys.hasattr("set_int_max_str_digits")? {
                return Ok::<_, PyErr>(());
            }
            let previous = sys
                .call_method0("get_int_max_str_digits")?
                .extract::<usize>()?;
            let huge = py
                .get_type::<PyInt>()
                .call_method1("__pow__", (10, 640))?
                .cast_into::<PyInt>()?
                .unbind();
            sys.call_method1("set_int_max_str_digits", (640,))?;
            let _restore = IntDigitLimitRestore { previous };
            let digits = format!("1{}", "0".repeat(640));

            let scene = SceneScope::new_for_test(py, "scene-digit-limit", huge.clone_ref(py));
            let operation = CueOperation::new_for_scene_for_test(0, &scene);
            let prepared = scene
                .begin_admission()
                .expect("large counter Scene must admit")
                .prepare(py)?;
            assert_eq!(prepared.id(), format!("scene-digit-limit-cue{digits}"));
            prepared.commit(operation.clone())?;
            scene.close();
            assert!(operation.is_terminal());

            let effect_scene = SceneScope::zero_for_test(py, "scene-effect-digit-limit")?;
            let cued = CuedScope {
                scene: effect_scene,
                source: PyString::new(py, "digit-owner").unbind(),
                actor_identity: 1,
                cue_id: PyString::new(py, "scene-effect-digit-limit-cue0").unbind(),
                effect_counter: Mutex::new(huge),
                agent_turns: Mutex::new(Vec::new()),
                active: AtomicBool::new(true),
            };
            assert_eq!(
                cued.next_effect_id(py)?.bind(py).to_str()?,
                format!("scene-effect-digit-limit-cue0-effect{digits}")
            );
            Ok::<_, PyErr>(())
        })
        .expect("ID formatting must not use Python's bounded integer string conversion");
    }

    #[test]
    fn injected_scene_names_are_canonical_v4_and_scope_local() {
        let first = Uuid::parse_str("12345678-1234-4234-9234-123456789abc").unwrap();
        let second = Uuid::parse_str("abcdefab-cdef-4def-8def-abcdefabcdef").unwrap();
        let mut generator = SceneNameGenerator::from_sequence_for_test([first, second]);

        assert_eq!(
            generator.next_name(),
            "scene-12345678-1234-4234-9234-123456789abc"
        );
        assert_eq!(
            generator.next_name(),
            "scene-abcdefab-cdef-4def-8def-abcdefabcdef"
        );
    }

    #[test]
    fn admission_txn_drive_first_is_exact_once_and_auto_drains() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            reset_admission_metrics_for_test();
            let scope = SceneScope::zero_for_test(py, "scene-drive-first")?;
            let transaction = scope
                .begin_admission()
                .expect("open scope must admit one transaction");
            assert_eq!(scope.admission_state(), AdmissionState::Admitting);
            assert!(scope.state_lock_available_for_test());

            let operation = CueOperation::new_for_scene_for_test(0, &scope);
            let unlocked_python_calls = Cell::new(0);
            let prepared: PreparedAdmission = transaction.prepare_with_probe(py, || {
                assert!(scope.state_lock_available_for_test());
                assert!(scope.counter.try_lock().is_ok());
                unlocked_python_calls.set(unlocked_python_calls.get() + 1);
            })?;
            let id = prepared.id().to_owned();
            prepared.commit(operation.clone())?;
            assert_eq!(id, "scene-drive-first-cue0");
            assert_eq!(unlocked_python_calls.get(), 3);
            assert_eq!(scope.admission_state(), AdmissionState::Open);
            assert_eq!(
                admission_metrics_for_test(),
                AdmissionMetrics {
                    indexes: 1,
                    registers: 1,
                    ..AdmissionMetrics::default()
                }
            );

            scope.close();
            assert_eq!(scope.admission_state(), AdmissionState::Closed);
            assert_eq!(operation.cancel_requests_for_test(), 1);
            assert!(operation.is_terminal());
            assert_eq!(
                admission_metrics_for_test(),
                AdmissionMetrics {
                    indexes: 1,
                    registers: 1,
                    cancels: 1,
                    drains: 1,
                    ..AdmissionMetrics::default()
                }
            );
            Ok::<_, PyErr>(())
        })
        .expect("drive-first transaction must complete");
    }

    #[test]
    fn queued_scene_close_terminalizes_and_deregisters_without_manual_finish() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            reset_admission_metrics_for_test();
            let scope = SceneScope::zero_for_test(py, "scene-queued-cancel")?;
            let actor_type = py.get_type::<Actor>();
            let actor_name = PyString::new(py, "scene-queued-cancel-actor");
            let production = py.None().into_bound(py);
            let identity = Arc::new(ActorIdentity);
            let (_, permit) =
                enter_actor_permit(&actor_type, &actor_name, &production, Arc::clone(&identity));
            let actor = actor_type.call0()?.cast_into::<Actor>()?.unbind();
            drop(permit);
            let production_state = Arc::new(ProductionState::new());
            let capability = Arc::new(ActorCapability::new(
                actor,
                &actor_name,
                NameKey::from_python(&actor_name)?,
                identity,
                Arc::downgrade(&production_state),
            ));
            let running = CueOperation::new_for_actor_for_test(0, &capability, &scope);
            capability.install_running_for_test(running.clone());
            let prepared = scope
                .begin_admission()
                .expect("Open scene must admit the queued operation")
                .prepare(py)?;
            let operation = CueOperation::new_for_actor_for_test(1, &capability, &scope);
            prepared.commit(operation.clone())?;
            assert_eq!(scope.operation_count_for_test(), 1);
            assert!(capability.queued_contains_for_test(&operation));

            scope.close();

            assert!(operation.is_terminal());
            assert_eq!(operation.cancel_requests_for_test(), 1);
            assert_eq!(scope.operation_count_for_test(), 0);
            assert_eq!(scope.admission_state(), AdmissionState::Closed);
            assert!(capability.queue_empty_and_running_exact_for_test(&running));
            assert_eq!(
                admission_metrics_for_test(),
                AdmissionMetrics {
                    indexes: 1,
                    registers: 1,
                    cancels: 1,
                    drains: 1,
                    ..AdmissionMetrics::default()
                }
            );
            running.finish_cancel();
            assert!(capability.retire_running_for_test(&running));
            Ok::<_, PyErr>(())
        })
        .expect("queued Scene cancellation must terminalize and deregister synchronously");
    }

    #[test]
    fn cued_scope_is_inactive_before_task_done_observers_run() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let event_loop = py.import("asyncio")?.call_method0("new_event_loop")?;
            let scene = SceneScope::zero_for_test(py, "scene-cued-close-order")?;
            let cued = CuedScope::new_for_test(Arc::clone(&scene), "close-order", 0);
            assert!(cued.is_active());
            let module = PyModule::from_code(
                py,
                c"async def complete():\n    return None\n",
                c"cued_close_order.py",
                c"cued_close_order",
            )?;
            let inner = module.getattr("complete")?.call0()?.unbind();
            let driver = Py::new(py, ScopeDriver::new_cued(Arc::clone(&cued), inner))?;
            let task = event_loop.call_method1("create_task", (driver,))?;
            let observed_inactive = Arc::new(AtomicBool::new(false));
            let callback_scope = Arc::clone(&cued);
            let callback_observed = Arc::clone(&observed_inactive);
            let callback = PyCFunction::new_closure(
                py,
                None,
                None,
                move |_args: &Bound<'_, PyTuple>,
                      _kwargs: Option<&Bound<'_, PyDict>>|
                      -> PyResult<()> {
                    callback_observed.store(!callback_scope.is_active(), Ordering::SeqCst);
                    Ok(())
                },
            )?;
            py.import("asyncio")?
                .getattr("Future")?
                .call_method1("add_done_callback", (&task, callback))?;
            event_loop.call_method1("run_until_complete", (&task,))?;

            assert!(observed_inactive.load(Ordering::SeqCst));
            assert!(!cued.is_active());
            scene.close();
            event_loop.call_method0("close")?;
            Ok::<_, PyErr>(())
        })
        .expect("ScopeDriver must close CuedScope inline before Task observation");
    }

    #[test]
    fn admission_txn_close_first_has_zero_side_effects() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            reset_admission_metrics_for_test();
            let scope = SceneScope::zero_for_test(py, "scene-close-first")?;
            scope.close();
            assert_eq!(scope.admission_state(), AdmissionState::Closed);
            assert!(scope.begin_admission().is_none());
            assert_eq!(admission_metrics_for_test(), AdmissionMetrics::default());
            Ok::<_, PyErr>(())
        })
        .expect("close-first transaction must complete");
    }

    #[test]
    fn admission_txn_close_during_prepare_commits_once_and_replaces_counter() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            reset_admission_metrics_for_test();
            let harness = tracked_counter_scope(py, "scene-close-during", "0")?;
            let scope = &harness.scope;
            let transaction = scope
                .begin_admission()
                .expect("open scope must start admission");
            assert!(scope.state_lock_available_for_test());

            let operation = CueOperation::new_for_scene_for_test(0, scope);
            let prepare_calls = Cell::new(0);
            let prepared: PreparedAdmission = transaction.prepare_with_probe(py, || {
                assert!(scope.state_lock_available_for_test());
                assert!(scope.counter.try_lock().is_ok());
                assert_eq!(counter_value(py, &scope.counter), "0");
                assert!(dropped_counters(py, &harness).unwrap().is_empty());
                if prepare_calls.get() == 0 {
                    scope.close();
                }
                assert_eq!(scope.admission_state(), AdmissionState::ClosePending);
                prepare_calls.set(prepare_calls.get() + 1);
            })?;
            assert_eq!(prepare_calls.get(), 3);
            assert_eq!(counter_value(py, &scope.counter), "0");
            assert!(dropped_counters(py, &harness)?.is_empty());
            let id = prepared.id().to_owned();
            prepared.commit(operation.clone())?;
            assert_eq!(id, "scene-close-during-cue0");
            assert_eq!(counter_value(py, &scope.counter), "1");
            assert_eq!(dropped_counters(py, &harness)?, ["0"]);
            assert_eq!(harness.lock_failures.load(Ordering::SeqCst), 0);
            assert_eq!(scope.admission_state(), AdmissionState::Closed);
            assert_eq!(operation.cancel_requests_for_test(), 1);
            assert!(operation.is_terminal());
            assert_eq!(
                admission_metrics_for_test(),
                AdmissionMetrics {
                    indexes: 1,
                    registers: 1,
                    cancels: 1,
                    drains: 1,
                    ..AdmissionMetrics::default()
                }
            );
            Ok::<_, PyErr>(())
        })
        .expect("close-during transaction must complete");
    }

    #[test]
    fn close_pending_commit_cancels_preexisting_and_new_operations() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            reset_admission_metrics_for_test();
            let scope = SceneScope::zero_for_test(py, "scene-close-pending-all")?;
            let existing = CueOperation::new_for_scene_for_test(0, &scope);
            scope
                .begin_admission()
                .expect("open scope must admit the existing operation")
                .prepare(py)?
                .commit(existing.clone())?;

            let transaction = scope
                .begin_admission()
                .expect("open scope must begin the second admission");
            let calls = Cell::new(0);
            let prepared = transaction.prepare_with_probe(py, || {
                if calls.get() == 0 {
                    scope.close();
                }
                calls.set(calls.get() + 1);
            })?;
            let admitted = CueOperation::new_for_scene_for_test(1, &scope);
            prepared.commit(admitted.clone())?;

            assert_eq!(existing.cancel_requests_for_test(), 1);
            assert_eq!(admitted.cancel_requests_for_test(), 1);
            assert!(existing.is_terminal());
            assert!(admitted.is_terminal());
            assert_eq!(scope.operation_count_for_test(), 0);
            assert_eq!(scope.admission_state(), AdmissionState::Closed);
            assert_eq!(
                admission_metrics_for_test(),
                AdmissionMetrics {
                    indexes: 2,
                    registers: 2,
                    cancels: 2,
                    drains: 2,
                    ..AdmissionMetrics::default()
                }
            );
            Ok::<_, PyErr>(())
        })
        .expect("close-pending commit must cancel every registered operation");
    }

    #[test]
    fn close_pending_rollback_cancels_preexisting_operations() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            reset_admission_metrics_for_test();
            let scope = SceneScope::zero_for_test(py, "scene-close-pending-rollback")?;
            let existing = CueOperation::new_for_scene_for_test(0, &scope);
            scope
                .begin_admission()
                .expect("open scope must admit the existing operation")
                .prepare(py)?
                .commit(existing.clone())?;

            let transaction = scope
                .begin_admission()
                .expect("open scope must begin a provisional admission");
            scope.close();
            assert_eq!(scope.admission_state(), AdmissionState::ClosePending);
            drop(transaction);

            assert_eq!(existing.cancel_requests_for_test(), 1);
            assert!(existing.is_terminal());
            assert_eq!(scope.operation_count_for_test(), 0);
            assert_eq!(scope.admission_state(), AdmissionState::Closed);
            assert_eq!(
                admission_metrics_for_test(),
                AdmissionMetrics {
                    indexes: 1,
                    registers: 1,
                    cancels: 1,
                    drains: 1,
                    rollbacks: 1,
                    ..AdmissionMetrics::default()
                }
            );
            Ok::<_, PyErr>(())
        })
        .expect("close-pending rollback must cancel every registered operation");
    }

    #[test]
    fn step2_close_pending_commit_cancels_before_actor_dispatch() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let asyncio = py.import("asyncio")?;
            let event_loop = asyncio.call_method0("new_event_loop")?;
            let state = Arc::new(ProductionState::new());
            let binding = RunBinding::new(py, &state, &event_loop)?;
            let scope =
                SceneScope::zero_for_binding_for_test(py, "scene-close-before-dispatch", &binding)?;
            let probe_calls = Cell::new(0);
            let prepared = scope
                .begin_admission()
                .expect("open scope must begin admission")
                .prepare_with_probe(py, || {
                    if probe_calls.get() == 0 {
                        scope.close();
                    }
                    probe_calls.set(probe_calls.get() + 1);
                })?;
            assert_eq!(scope.admission_state(), AdmissionState::ClosePending);

            let hook_module = PyModule::from_code(
                py,
                c"calls = []\ndef cued(self, cue):\n    calls.append(cue)\n    async def result():\n        return ()\n    return result()\n",
                c"close_pending_dispatch_test.py",
                c"close_pending_dispatch_test",
            )?;
            let namespace = PyDict::new(py);
            namespace.set_item("cued", hook_module.getattr("cued")?)?;
            let bases = PyTuple::new(py, [py.get_type::<Actor>()])?;
            let actor_type = py
                .import("builtins")?
                .getattr("type")?
                .call1(("ClosePendingActor", bases, namespace))?
                .cast_into::<PyType>()?;
            let actor_name = PyString::new(py, "close-before-dispatch");
            let production = py.None().into_bound(py);
            let identity = Arc::new(ActorIdentity);
            let (_, permit) = enter_actor_permit(
                &actor_type,
                &actor_name,
                &production,
                Arc::clone(&identity),
            );
            let actor = actor_type.call0()?.cast_into::<Actor>()?.unbind();
            drop(permit);
            let capability = Arc::new(ActorCapability::new(
                actor,
                &actor_name,
                NameKey::from_python(&actor_name)?,
                identity,
                Arc::downgrade(&state),
            ));
            let cued = CuedScope::new(
                py,
                Arc::clone(&scope),
                actor_name.clone().unbind(),
                capability.identity_address(),
                PyString::new(py, "scene-close-before-dispatch-cue0").unbind(),
            )?;
            let cue = Py::new(
                py,
                Cue::new_runtime(
                    PyString::new(py, prepared.id()).unbind(),
                    PyDict::new(py).unbind().into_any(),
                    PyString::new(py, "scene-close-before-dispatch").unbind(),
                ),
            )?;
            let operation = CueOperation::new_runtime(
                &scope,
                &capability,
                &binding,
                cued,
                cue,
                event_loop.call_method0("create_future")?.unbind(),
            );
            let signal = operation.signal_for_test(py);

            prepared.commit(operation.clone())?;

            assert_eq!(hook_module.getattr("calls")?.len()?, 0);
            assert_eq!(operation.cancel_requests_for_test(), 1);
            assert!(operation.is_terminal());
            assert_eq!(scope.operation_count_for_test(), 0);
            assert_eq!(scope.admission_state(), AdmissionState::Closed);
            assert!(signal.bind(py).call_method0("done")?.is_truthy()?);
            assert!(signal.bind(py).call_method0("result")?.is_none());
            assert!(!capability.queued_contains_for_test(&operation));
            event_loop.call_method0("close")?;
            Ok::<_, PyErr>(())
        })
        .expect("a close-winning admission must be cancelled before actor dispatch");
    }

    #[test]
    fn admission_prepare_error_rolls_back_and_drops_temporary_counter_unlocked() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            reset_admission_metrics_for_test();
            let harness = tracked_counter_scope(py, "scene-rollback", "0")?;
            let scope = &harness.scope;
            harness
                .module
                .bind(py)
                .getattr("CounterProbe")?
                .setattr("fail_format", true)?;
            let transaction = scope
                .begin_admission()
                .expect("open scope must admit one transaction");
            let prepare_calls = Cell::new(0);
            let error: PyErr = match transaction.prepare_with_probe(py, || {
                assert!(scope.state_lock_available_for_test());
                assert!(scope.counter.try_lock().is_ok());
                assert_eq!(counter_value(py, &scope.counter), "0");
                prepare_calls.set(prepare_calls.get() + 1);
            }) {
                Ok(_) => panic!("formatting failure must roll back preparation"),
                Err(error) => error,
            };
            assert!(error.is_instance_of::<PyTypeError>(py));
            assert_eq!(error.value(py).to_string(), "counter formatting failed");
            assert!(error.traceback(py).is_some());
            assert_eq!(prepare_calls.get(), 2);
            assert_eq!(scope.admission_state(), AdmissionState::Open);
            assert_eq!(counter_value(py, &scope.counter), "0");
            assert_eq!(dropped_counters(py, &harness)?, ["1"]);
            assert_eq!(harness.lock_failures.load(Ordering::SeqCst), 0);
            drop(error);
            assert_eq!(
                admission_metrics_for_test(),
                AdmissionMetrics {
                    rollbacks: 1,
                    ..AdmissionMetrics::default()
                }
            );

            harness
                .module
                .bind(py)
                .getattr("CounterProbe")?
                .setattr("fail_format", false)?;
            let operation = CueOperation::new_for_scene_for_test(0, scope);
            let prepared: PreparedAdmission = scope
                .begin_admission()
                .expect("rolled-back scope must admit again")
                .prepare(py)?;
            let id = prepared.id().to_owned();
            prepared.commit(operation.clone())?;
            assert_eq!(id, "scene-rollback-cue0");
            assert_eq!(counter_value(py, &scope.counter), "1");
            assert_eq!(dropped_counters(py, &harness)?, ["1", "0"]);
            assert_eq!(harness.lock_failures.load(Ordering::SeqCst), 0);
            assert_eq!(
                admission_metrics_for_test(),
                AdmissionMetrics {
                    indexes: 1,
                    registers: 1,
                    rollbacks: 1,
                    ..AdmissionMetrics::default()
                }
            );
            scope.close();
            assert!(operation.is_terminal());
            assert_eq!(scope.admission_state(), AdmissionState::Closed);
            Ok::<_, PyErr>(())
        })
        .expect("prepare failure must leave the scope reusable");
    }

    #[test]
    fn admission_prepare_error_with_close_pending_rolls_back_without_counter_commit() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            reset_admission_metrics_for_test();
            let harness = tracked_counter_scope(py, "scene-close-rollback", "0")?;
            let scope = &harness.scope;
            harness
                .module
                .bind(py)
                .getattr("CounterProbe")?
                .setattr("fail_format", true)?;
            let transaction = scope
                .begin_admission()
                .expect("open scope must begin close-pending preparation");
            let prepare_calls = Cell::new(0);
            let error: PyErr = match transaction.prepare_with_probe(py, || {
                assert!(scope.state_lock_available_for_test());
                assert!(scope.counter.try_lock().is_ok());
                assert_eq!(counter_value(py, &scope.counter), "0");
                if prepare_calls.get() == 0 {
                    scope.close();
                }
                assert_eq!(scope.admission_state(), AdmissionState::ClosePending);
                prepare_calls.set(prepare_calls.get() + 1);
            }) {
                Ok(_) => panic!("close-pending formatting failure must roll back"),
                Err(error) => error,
            };
            assert!(error.is_instance_of::<PyTypeError>(py));
            assert_eq!(error.value(py).to_string(), "counter formatting failed");
            assert!(error.traceback(py).is_some());
            assert_eq!(prepare_calls.get(), 2);
            assert_eq!(scope.admission_state(), AdmissionState::Closing);
            assert_eq!(counter_value(py, &scope.counter), "0");
            assert_eq!(dropped_counters(py, &harness)?, ["1"]);
            assert_eq!(harness.lock_failures.load(Ordering::SeqCst), 0);
            drop(error);
            assert_eq!(
                admission_metrics_for_test(),
                AdmissionMetrics {
                    rollbacks: 1,
                    ..AdmissionMetrics::default()
                }
            );
            assert!(scope.begin_admission().is_none());
            Ok::<_, PyErr>(())
        })
        .expect("close-pending prepare failure must not commit a counter or operation");
    }

    #[test]
    fn cued_scope_lives_in_scene_context_and_closes_inline() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let scene = SceneScope::zero_for_test(py, "scene-cued")?;
            let cued = CuedScope::new_for_test(Arc::clone(&scene), "actor-source", 42);
            assert!(Arc::ptr_eq(cued.scene_for_test(), &scene));
            assert_eq!(cued.source_for_test(py)?, "actor-source");
            assert_eq!(cued.actor_identity_for_test(), 42);
            assert!(cued.is_active());
            cued.close_inline();
            assert!(!cued.is_active());
            Ok::<_, PyErr>(())
        })
        .expect("CuedScope lifetime test must complete");
    }

    #[test]
    fn cued_effect_counter_is_python_int_unbounded_and_uses_cue_id_snapshot() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            const SEED: &str = "1606938044258990275541962092341162602522202993782792835301376";
            const NEXT: &str = "1606938044258990275541962092341162602522202993782792835301377";
            let scene = SceneScope::zero_for_test(py, "scene-effect-counter")?;
            let seed = py
                .get_type::<PyInt>()
                .call1((SEED,))?
                .cast_into::<PyInt>()?
                .unbind();
            let cued = Arc::new(CuedScope {
                scene,
                source: PyString::new(py, "counter-owner").unbind(),
                actor_identity: 11,
                cue_id: PyString::new(py, "scene-effect-counter-cue9").unbind(),
                effect_counter: Mutex::new(seed),
                agent_turns: Mutex::new(Vec::new()),
                active: AtomicBool::new(true),
            });
            let _: &Mutex<Py<PyInt>> = &cued.effect_counter;

            let first = cued.next_effect_id(py)?;
            let second = cued.next_effect_id(py)?;
            assert_eq!(
                first.bind(py).to_str()?,
                format!("scene-effect-counter-cue9-effect{SEED}")
            );
            assert_eq!(
                second.bind(py).to_str()?,
                format!("scene-effect-counter-cue9-effect{NEXT}")
            );
            assert!(
                cued.effect_counter
                    .lock()
                    .unwrap()
                    .bind(py)
                    .is_instance_of::<PyInt>()
            );
            Ok::<_, PyErr>(())
        })
        .expect("CuedScope Effect counter must remain an arbitrary-precision Python int");
    }

    #[test]
    fn cued_scope_traverses_cue_id_and_effect_counter_cycles_independently() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let module = PyModule::from_code(
                py,
                c"class Marker: pass\nclass CueId(str): pass\nclass Counter(int): pass\n",
                c"cued_effect_gc_test.py",
                c"cued_effect_gc_test",
            )?;

            for edge_name in ["cue_id", "effect_counter"] {
                let scene = SceneScope::zero_for_test(py, "scene-effect-cycle")?;
                let cue_id = module
                    .getattr("CueId")?
                    .call1(("scene-effect-cycle-cue0",))?
                    .cast_into::<PyString>()?;
                let counter = module
                    .getattr("Counter")?
                    .call1((0,))?
                    .cast_into::<PyInt>()?;
                let edge = if edge_name == "cue_id" {
                    cue_id.as_any().clone()
                } else {
                    counter.as_any().clone()
                };
                let marker = module.getattr("Marker")?.call0()?;
                let marker_ref = pyo3::types::PyWeakrefReference::new(&marker)?.unbind();
                edge.setattr("marker", &marker)?;
                let cued = Arc::new(CuedScope {
                    scene,
                    source: PyString::new(py, "cycle-owner").unbind(),
                    actor_identity: 12,
                    cue_id: cue_id.unbind(),
                    effect_counter: Mutex::new(counter.unbind()),
                    agent_turns: Mutex::new(Vec::new()),
                    active: AtomicBool::new(true),
                });
                let driver = Py::new(py, ScopeDriver::new_cued_for_test(Arc::clone(&cued)))?;
                edge.setattr("driver", driver.bind(py))?;

                drop((driver, cued, edge, marker));
                py.import("gc")?.call_method0("collect")?;
                assert!(
                    marker_ref.bind(py).call0()?.is_none(),
                    "{edge_name} must participate in CuedScope traversal"
                );
            }
            Ok::<_, PyErr>(())
        })
        .expect("both CuedScope Python edges must be independently collectable");
    }

    #[test]
    fn scope_drivers_are_the_only_strong_runtime_scope_owners() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let scene = SceneScope::zero_for_test(py, "scene-owner")?;
            let scene_weak = Arc::downgrade(&scene);
            let scene_lineage = TaskLineage::from_scene(&scene);
            let scene_driver = ScopeDriver::new_scene_for_test(Arc::clone(&scene));
            drop(scene);
            assert!(scene_weak.upgrade().is_some());
            drop(scene_driver);
            assert!(scene_weak.upgrade().is_none());
            drop(scene_lineage);

            let root = SceneScope::zero_for_test(py, "scene-cued-owner")?;
            let cued = CuedScope::new_for_test(Arc::clone(&root), "actor-owner", 7);
            let cued_weak = Arc::downgrade(&cued);
            let cued_lineage = TaskLineage::from_cued(&cued);
            let cued_driver = ScopeDriver::new_cued_for_test(Arc::clone(&cued));
            drop(cued);
            assert!(cued_weak.upgrade().is_some());
            drop(cued_driver);
            assert!(cued_weak.upgrade().is_none());
            drop((cued_lineage, root));
            Ok::<_, PyErr>(())
        })
        .expect("scope driver ownership must be acyclic and bounded");
    }

    #[test]
    fn terminal_runtime_graph_releases_binding_scope_and_operation() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let state = Arc::new(ProductionState::new());
            let binding = Arc::new(RunBinding::new_for_test(py)?);
            state.bind_for_test(&binding)?;
            let binding_weak = Arc::downgrade(&binding);

            let scene =
                SceneScope::zero_for_binding_for_test(py, "scene-complete-graph", &binding)?;
            let scene_weak = Arc::downgrade(&scene);
            let task_type = PyModule::from_code(
                py,
                c"class Task: pass\n",
                c"scene_graph_test.py",
                c"scene_graph_test",
            )?
            .getattr("Task")?;
            let task = task_type.call0()?;
            binding.register_task(&task, TaskLineage::from_scene(&scene))?;
            let driver = ScopeDriver::new_scene_for_test(Arc::clone(&scene));

            let operation = CueOperation::new_for_scene_for_test(0, &scene);
            let operation_weak = operation.downgrade_for_test();
            scene
                .begin_admission()
                .expect("open graph scope must admit")
                .prepare(py)?
                .commit(operation.clone())?;
            scene.close();
            assert!(operation.is_terminal());
            assert_eq!(scene.admission_state(), AdmissionState::Closed);

            drop((driver, operation, scene, binding, task));
            assert!(binding_weak.upgrade().is_none());
            assert!(scene_weak.upgrade().is_none());
            assert!(operation_weak.upgrade().is_none());
            assert!(state.active_binding_for_test().is_none());
            Ok::<_, PyErr>(())
        })
        .expect("the completed runtime ownership graph must be acyclic");
    }
}
