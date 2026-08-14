use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use pyo3::class::gc::{PyTraverseError, PyVisit};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{
    PyAnyMethods, PyDict, PyDictMethods, PyType, PyWeakrefMethods, PyWeakrefReference,
};
use pyo3_async_runtimes::TaskLocals;
use tokio::sync::oneshot;

use crate::diagnostic_runtime::scene_producer::{self, SceneHook};
use crate::orchestration::production::Production;
use crate::orchestration::scene_context::{
    CuedScope, RunBinding, SceneScope, ScopeDriver, TaskFactoryAction,
};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone)]
enum TaskLineageKind {
    Scene(Weak<SceneScope>),
    Cued(Weak<CuedScope>),
    #[cfg(test)]
    Test(usize),
}

#[derive(Clone)]
pub(crate) struct TaskLineage {
    kind: TaskLineageKind,
}

impl TaskLineage {
    pub(crate) fn from_scene(scene: &Arc<SceneScope>) -> Self {
        Self {
            kind: TaskLineageKind::Scene(Arc::downgrade(scene)),
        }
    }

    pub(crate) fn from_cued(cued: &Arc<CuedScope>) -> Self {
        Self {
            kind: TaskLineageKind::Cued(Arc::downgrade(cued)),
        }
    }

    pub(crate) fn scene(&self) -> Option<Arc<SceneScope>> {
        match &self.kind {
            TaskLineageKind::Scene(scene) => scene.upgrade(),
            TaskLineageKind::Cued(cued) => cued.upgrade().map(|cued| cued.scene()),
            #[cfg(test)]
            TaskLineageKind::Test(_) => None,
        }
    }

    pub(crate) fn cued(&self) -> Option<Arc<CuedScope>> {
        match &self.kind {
            TaskLineageKind::Cued(cued) => cued.upgrade(),
            _ => None,
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        match &self.kind {
            TaskLineageKind::Scene(scene) => scene.upgrade().is_some_and(|scene| scene.is_open()),
            TaskLineageKind::Cued(cued) => cued.upgrade().is_some_and(|cued| cued.is_active()),
            #[cfg(test)]
            TaskLineageKind::Test(_) => true,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(id: usize) -> Self {
        Self {
            kind: TaskLineageKind::Test(id),
        }
    }

    #[cfg(test)]
    pub(crate) fn id_for_test(&self) -> usize {
        match self.kind {
            TaskLineageKind::Test(id) => id,
            _ => panic!("only test lineage has a numeric id"),
        }
    }
}

struct WeakTaskIdentity {
    address: usize,
    reference: Py<PyWeakrefReference>,
}

impl WeakTaskIdentity {
    fn new(task: &Bound<'_, PyAny>) -> PyResult<Self> {
        Ok(Self {
            address: task.as_ptr() as usize,
            reference: PyWeakrefReference::new(task)?.unbind(),
        })
    }

    fn matches(&self, task: &Bound<'_, PyAny>) -> bool {
        self.address == task.as_ptr() as usize
            && self
                .reference
                .bind(task.py())
                .upgrade()
                .is_some_and(|live| live.is(task))
    }

    fn upgrade(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.reference.bind(py).upgrade().map(Bound::unbind)
    }
}

struct TaskLineageEntry {
    identity: WeakTaskIdentity,
    lineage: TaskLineage,
}

#[derive(Default)]
pub(crate) struct TaskLineageRegistry {
    entries: HashMap<usize, TaskLineageEntry>,
}

impl TaskLineageRegistry {
    pub(crate) fn register(
        &mut self,
        task: &Bound<'_, PyAny>,
        lineage: TaskLineage,
    ) -> PyResult<()> {
        let key = task.as_ptr() as usize;
        let identity = WeakTaskIdentity::new(task)?;
        self.entries
            .insert(key, TaskLineageEntry { identity, lineage });
        Ok(())
    }

    pub(crate) fn unregister(&mut self, task: &Bound<'_, PyAny>) -> bool {
        let key = task.as_ptr() as usize;
        let exact = self
            .entries
            .get(&key)
            .is_some_and(|entry| entry.identity.matches(task));
        if exact
            || self
                .entries
                .get(&key)
                .is_some_and(|entry| entry.identity.reference.bind(task.py()).upgrade().is_none())
        {
            self.entries.remove(&key);
        }
        exact
    }

    pub(crate) fn lookup(&mut self, task: &Bound<'_, PyAny>) -> PyResult<Option<TaskLineage>> {
        let key = task.as_ptr() as usize;
        let Some(entry) = self.entries.get(&key) else {
            return Ok(None);
        };
        if entry.identity.matches(task) {
            return Ok(Some(entry.lineage.clone()));
        }
        self.entries.remove(&key);
        Ok(None)
    }

    pub(crate) fn traverse(&self, visit: &PyVisit<'_>) -> Result<(), PyTraverseError> {
        for entry in self.entries.values() {
            visit.call(&entry.identity.reference)?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn prune(&mut self, py: Python<'_>) -> PyResult<()> {
        let mut dead = Vec::new();
        for (key, entry) in &self.entries {
            if entry.identity.reference.bind(py).upgrade().is_none() {
                dead.push(*key);
            }
        }
        for key in dead {
            self.entries.remove(&key);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn live_len_for_test(&mut self) -> PyResult<usize> {
        Python::attach(|py| self.prune(py))?;
        Ok(self.entries.len())
    }
}

struct ProvisionalEntry {
    value: ProvisionalValue,
    generation: usize,
    consumed: bool,
    eager_task: Option<WeakTaskIdentity>,
}

enum ProvisionalValue {
    Runtime {
        coroutine: Py<PyAny>,
        lineage: TaskLineage,
    },
    #[cfg(test)]
    Test(usize),
}

#[derive(Default)]
pub(crate) struct ProvisionalPermitStack {
    entries: Arc<Mutex<Vec<ProvisionalEntry>>>,
    next_generation: AtomicUsize,
}

pub(crate) struct ProvisionalPermitGuard {
    entries: Arc<Mutex<Vec<ProvisionalEntry>>>,
    generation: usize,
}

impl ProvisionalPermitGuard {
    pub(crate) fn eager_task(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        lock(&self.entries)
            .iter()
            .rfind(|entry| entry.generation == self.generation)
            .and_then(|entry| entry.eager_task.as_ref())
            .and_then(|identity| identity.upgrade(py))
    }
}

impl Drop for ProvisionalPermitGuard {
    fn drop(&mut self) {
        let mut entries = lock(&self.entries);
        if let Some(position) = entries
            .iter()
            .rposition(|entry| entry.generation == self.generation)
        {
            entries.remove(position);
        }
    }
}

impl ProvisionalPermitStack {
    pub(crate) fn push(
        &self,
        py: Python<'_>,
        coroutine: &Bound<'_, PyAny>,
        lineage: TaskLineage,
    ) -> ProvisionalPermitGuard {
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        lock(&self.entries).push(ProvisionalEntry {
            value: ProvisionalValue::Runtime {
                coroutine: coroutine.clone().unbind(),
                lineage,
            },
            generation,
            consumed: false,
            eager_task: None,
        });
        let _ = py;
        ProvisionalPermitGuard {
            entries: Arc::clone(&self.entries),
            generation,
        }
    }

    pub(crate) fn consume_exact(&self, coroutine: &Bound<'_, PyAny>) -> Option<TaskLineage> {
        let mut entries = lock(&self.entries);
        let entry = entries.last_mut()?;
        let (expected, lineage) = match &entry.value {
            ProvisionalValue::Runtime { coroutine, lineage } => (coroutine, lineage),
            #[cfg(test)]
            ProvisionalValue::Test(_) => return None,
        };
        if entry.consumed || !expected.bind(coroutine.py()).is(coroutine) {
            return None;
        }
        entry.consumed = true;
        Some(lineage.clone())
    }

    pub(crate) fn lineage_for_running_eager_task(
        &self,
        task: &Bound<'_, PyAny>,
    ) -> PyResult<Option<TaskLineage>> {
        let py = task.py();
        let (generation, expected, lineage) = {
            let entries = lock(&self.entries);
            let Some(entry) = entries.last() else {
                return Ok(None);
            };
            match &entry.value {
                ProvisionalValue::Runtime { coroutine, lineage } => {
                    (entry.generation, coroutine.clone_ref(py), lineage.clone())
                }
                #[cfg(test)]
                ProvisionalValue::Test(_) => return Ok(None),
            }
        };
        let base_task: Bound<'_, PyType> = py.import("asyncio")?.getattr("Task")?.cast_into()?;
        let actual = base_task.call_method1("get_coro", (task,))?;
        if !actual.is(expected.bind(py)) {
            return Ok(None);
        }
        let identity = WeakTaskIdentity::new(task)?;
        let mut entries = lock(&self.entries);
        let Some(entry) = entries
            .last_mut()
            .filter(|entry| entry.generation == generation)
        else {
            return Ok(None);
        };
        if let Some(captured) = &entry.eager_task {
            return Ok(captured.matches(task).then_some(lineage));
        }
        if entry.consumed {
            return Ok(None);
        }
        entry.consumed = true;
        entry.eager_task = Some(identity);
        Ok(Some(lineage))
    }

    pub(crate) fn traverse(&self, visit: &PyVisit<'_>) -> Result<(), PyTraverseError> {
        for entry in lock(&self.entries).iter() {
            match &entry.value {
                ProvisionalValue::Runtime { coroutine, .. } => visit.call(coroutine)?,
                #[cfg(test)]
                ProvisionalValue::Test(_) => {}
            }
            if let Some(identity) = &entry.eager_task {
                visit.call(&identity.reference)?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn push_for_test(&self, value: usize) -> ProvisionalPermitGuard {
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        lock(&self.entries).push(ProvisionalEntry {
            value: ProvisionalValue::Test(value),
            generation,
            consumed: false,
            eager_task: None,
        });
        ProvisionalPermitGuard {
            entries: Arc::clone(&self.entries),
            generation,
        }
    }

    #[cfg(test)]
    pub(crate) fn consume_for_test(&self, value: usize) -> Option<usize> {
        let mut entries = lock(&self.entries);
        let entry = entries.last_mut()?;
        let ProvisionalValue::Test(current) = &entry.value else {
            return None;
        };
        if *current != value || entry.consumed {
            return None;
        }
        entry.consumed = true;
        Some(*current)
    }
}

#[pyclass(name = "_TaskFactoryWrapper", module = "troupe._runtime")]
pub(crate) struct TaskFactoryWrapper {
    binding: Weak<RunBinding>,
}

impl TaskFactoryWrapper {
    pub(crate) fn new(binding: Weak<RunBinding>) -> Self {
        Self { binding }
    }

    #[cfg(test)]
    pub(crate) fn binding_for_test(&self) -> Option<Arc<RunBinding>> {
        self.binding.upgrade()
    }
}

#[pymethods]
impl TaskFactoryWrapper {
    #[pyo3(signature = (loop_, coroutine, **kwargs))]
    fn __call__(
        &self,
        py: Python<'_>,
        loop_: &Bound<'_, PyAny>,
        coroutine: &Bound<'_, PyAny>,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Py<PyAny>> {
        let binding = self.binding.upgrade().ok_or_else(|| {
            PyRuntimeError::new_err("Production runtime task factory is no longer active")
        })?;
        binding.create_delegated_task(py, loop_, coroutine, kwargs)
    }
}

#[pyclass]
struct HookCallback {
    production: Option<Py<PyAny>>,
    hook: &'static str,
    sender: Option<oneshot::Sender<PyResult<Py<PyAny>>>>,
}

#[pyclass]
struct TaskCancelCallback {
    task: Option<Py<PyAny>>,
    sender: Option<oneshot::Sender<PyResult<()>>>,
}

#[pyclass]
struct TaskFactoryActionCallback {
    binding: Option<Arc<RunBinding>>,
    action: TaskFactoryAction,
    sender: Option<oneshot::Sender<PyResult<()>>>,
}

#[pyclass]
struct SceneTaskCallback {
    binding: Option<Arc<RunBinding>>,
    production: Option<Py<PyAny>>,
    sender: Option<oneshot::Sender<SceneTaskResult>>,
}

type SceneTaskResult = PyResult<(Py<PyAny>, Arc<SceneScope>)>;

#[pyclass]
struct RunBindingCallback {
    production: Option<Py<PyAny>>,
    event_loop: Option<Py<PyAny>>,
    sender: Option<oneshot::Sender<PyResult<Arc<RunBinding>>>>,
}

impl TaskCancelCallback {
    fn new(task: Py<PyAny>, sender: oneshot::Sender<PyResult<()>>) -> Self {
        Self {
            task: Some(task),
            sender: Some(sender),
        }
    }

    fn invoke(&mut self, py: Python<'_>) -> PyResult<()> {
        let Some(sender) = self.sender.take() else {
            return Ok(());
        };
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        let result = task.bind(py).call_method0("cancel").map(|_| ());
        let _ = sender.send(result);
        Ok(())
    }
}

impl TaskFactoryActionCallback {
    fn invoke(&mut self, py: Python<'_>) -> PyResult<()> {
        let Some(sender) = self.sender.take() else {
            return Ok(());
        };
        let Some(binding) = self.binding.take() else {
            return Ok(());
        };
        let result = match self.action {
            TaskFactoryAction::Install => binding.install_wrapper(py),
            TaskFactoryAction::Check => binding.check_wrapper(py),
            TaskFactoryAction::Restore => binding.restore_factory(py),
        };
        let _ = sender.send(result);
        Ok(())
    }
}

impl SceneTaskCallback {
    fn invoke(&mut self, py: Python<'_>) -> PyResult<()> {
        let Some(sender) = self.sender.take() else {
            return Ok(());
        };
        let Some(binding) = self.binding.take() else {
            return Ok(());
        };
        let Some(production) = self.production.take() else {
            return Ok(());
        };
        let result = (|| {
            if let Err(error) = binding.check_wrapper(py) {
                binding.ensure_wrapper_for_drain(py);
                return Err(error);
            }
            let scene = binding.next_scene(py)?;
            let awaitable = match production.bind(py).getattr("scene")?.call0() {
                Ok(awaitable) => awaitable,
                Err(error) => {
                    scene.close();
                    return Err(error);
                }
            };
            let driver = Py::new(
                py,
                ScopeDriver::new_scene(Arc::clone(&scene), awaitable.unbind()),
            )?;
            let task = create_registered_scope_task(
                py,
                &binding,
                driver.bind(py).as_any(),
                TaskLineage::from_scene(&scene),
            )?;
            if binding.check_wrapper(py).is_err() {
                binding.ensure_wrapper_for_drain(py);
                let _ = task.bind(py).call_method0("cancel");
            }
            Ok((task, scene))
        })();
        let _ = sender.send(result);
        Ok(())
    }
}

impl RunBindingCallback {
    fn invoke(&mut self, py: Python<'_>) -> PyResult<()> {
        let Some(sender) = self.sender.take() else {
            return Ok(());
        };
        let Some(production) = self.production.take() else {
            return Ok(());
        };
        let Some(event_loop) = self.event_loop.take() else {
            return Ok(());
        };
        let result = (|| {
            let state = production.bind(py).cast::<Production>()?.borrow().state();
            let binding = RunBinding::new(py, &state, event_loop.bind(py))?;
            state.bind(&binding)?;
            Ok(binding)
        })();
        let _ = sender.send(result);
        Ok(())
    }
}

pub(crate) fn create_registered_scope_task(
    py: Python<'_>,
    binding: &Arc<RunBinding>,
    driver: &Bound<'_, PyAny>,
    lineage: TaskLineage,
) -> PyResult<Py<PyAny>> {
    let diagnostic_lineage = lineage.clone();
    let permit = binding.enter_task_permit(py, driver, lineage);
    let task_result = py
        .import("asyncio")
        .and_then(|module| module.call_method1("create_task", (driver,)));
    drop(permit);
    match task_result {
        Ok(task) => {
            scene_producer::observe_task(&diagnostic_lineage, SceneHook::TaskRegistered);
            Ok(task.unbind())
        }
        Err(error) => {
            let _ = driver.call_method0("close");
            Err(error)
        }
    }
}

impl HookCallback {
    fn new(
        production: Py<PyAny>,
        hook: &'static str,
        sender: oneshot::Sender<PyResult<Py<PyAny>>>,
    ) -> Self {
        Self {
            production: Some(production),
            hook,
            sender: Some(sender),
        }
    }

    fn invoke(&mut self, py: Python<'_>) -> PyResult<()> {
        let Some(sender) = self.sender.take() else {
            return Ok(());
        };
        let result = (|| {
            let production = self
                .production
                .take()
                .ok_or_else(|| PyRuntimeError::new_err("Python hook callback is cleared"))?;
            let awaitable = production.bind(py).getattr(self.hook)?.call0()?;
            Ok(awaitable.unbind())
        })();
        let _ = sender.send(result);
        Ok(())
    }
}

#[pymethods]
impl TaskCancelCallback {
    fn __call__(&mut self, py: Python<'_>) -> PyResult<()> {
        self.invoke(py)
    }

    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.task)
    }

    fn __clear__(&mut self) {
        self.task = None;
        self.sender = None;
    }
}

#[pymethods]
impl TaskFactoryActionCallback {
    fn __call__(&mut self, py: Python<'_>) -> PyResult<()> {
        self.invoke(py)
    }

    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        if let Some(binding) = &self.binding {
            binding.traverse(&visit)?;
        }
        Ok(())
    }

    fn __clear__(&mut self) {
        self.binding = None;
        self.sender = None;
    }
}

#[pymethods]
impl SceneTaskCallback {
    fn __call__(&mut self, py: Python<'_>) -> PyResult<()> {
        self.invoke(py)
    }

    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        if let Some(binding) = &self.binding {
            binding.traverse(&visit)?;
        }
        visit.call(&self.production)
    }

    fn __clear__(&mut self) {
        self.binding = None;
        self.production = None;
        self.sender = None;
    }
}

#[pymethods]
impl RunBindingCallback {
    fn __call__(&mut self, py: Python<'_>) -> PyResult<()> {
        self.invoke(py)
    }

    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.production)?;
        visit.call(&self.event_loop)
    }

    fn __clear__(&mut self) {
        self.production = None;
        self.event_loop = None;
        self.sender = None;
    }
}

#[pymethods]
impl HookCallback {
    fn __call__(&mut self, py: Python<'_>) -> PyResult<()> {
        self.invoke(py)
    }

    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.production)
    }

    fn __clear__(&mut self) {
        self.production = None;
        self.sender = None;
    }
}

async fn dispatch_hook(
    locals: &TaskLocals,
    production: &Py<PyAny>,
    hook: &'static str,
) -> PyResult<Py<PyAny>> {
    let receiver = Python::attach(|py| {
        let (sender, receiver) = oneshot::channel();
        let callback = Py::new(
            py,
            HookCallback::new(production.clone_ref(py), hook, sender),
        )?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("context", locals.context(py))?;
        locals
            .event_loop(py)
            .call_method("call_soon_threadsafe", (callback,), Some(&kwargs))?;
        Ok::<_, PyErr>(receiver)
    })?;

    match receiver.await {
        Ok(result) => result,
        Err(_) => Err(PyRuntimeError::new_err(
            "Python hook callback did not return a result",
        )),
    }
}

pub(crate) async fn await_hook(
    locals: &TaskLocals,
    production: &Py<PyAny>,
    hook: &'static str,
) -> PyResult<()> {
    let awaitable = dispatch_hook(locals, production, hook).await?;
    let future = Python::attach(|py| {
        pyo3_async_runtimes::into_future_with_locals(locals, awaitable.into_bound(py))
    })?;
    future.await?;
    Ok(())
}

pub(crate) async fn apply_task_factory_action(
    locals: &TaskLocals,
    binding: Arc<RunBinding>,
    action: TaskFactoryAction,
) -> PyResult<()> {
    let receiver = Python::attach(|py| {
        let (sender, receiver) = oneshot::channel();
        let callback = Py::new(
            py,
            TaskFactoryActionCallback {
                binding: Some(binding),
                action,
                sender: Some(sender),
            },
        )?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("context", locals.context(py))?;
        locals
            .event_loop(py)
            .call_method("call_soon_threadsafe", (callback,), Some(&kwargs))?;
        Ok::<_, PyErr>(receiver)
    })?;
    receiver
        .await
        .map_err(|_| PyRuntimeError::new_err("task factory callback did not return a result"))?
}

pub(crate) async fn create_run_binding(
    locals: &TaskLocals,
    production: &Py<PyAny>,
) -> PyResult<Arc<RunBinding>> {
    let receiver = Python::attach(|py| {
        let (sender, receiver) = oneshot::channel();
        let callback = Py::new(
            py,
            RunBindingCallback {
                production: Some(production.clone_ref(py)),
                event_loop: Some(locals.event_loop(py).unbind()),
                sender: Some(sender),
            },
        )?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("context", locals.context(py))?;
        locals
            .event_loop(py)
            .call_method("call_soon_threadsafe", (callback,), Some(&kwargs))?;
        Ok::<_, PyErr>(receiver)
    })?;
    receiver
        .await
        .map_err(|_| PyRuntimeError::new_err("run binding callback did not return a result"))?
}

pub(crate) async fn create_scene_task(
    locals: &TaskLocals,
    production: &Py<PyAny>,
    binding: Arc<RunBinding>,
) -> PyResult<PythonTask> {
    let receiver = Python::attach(|py| {
        let (sender, receiver) = oneshot::channel();
        let callback = Py::new(
            py,
            SceneTaskCallback {
                binding: Some(binding),
                production: Some(production.clone_ref(py)),
                sender: Some(sender),
            },
        )?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("context", locals.context(py))?;
        locals
            .event_loop(py)
            .call_method("call_soon_threadsafe", (callback,), Some(&kwargs))?;
        Ok::<_, PyErr>(receiver)
    })?;
    let (task, scene) = receiver
        .await
        .map_err(|_| PyRuntimeError::new_err("scene task callback did not return a result"))??;
    Ok(PythonTask { task, scene })
}

pub(crate) struct PythonTask {
    task: Py<PyAny>,
    scene: Arc<SceneScope>,
}

impl PythonTask {
    pub(crate) async fn cancel(&self, locals: &TaskLocals) -> PyResult<()> {
        let receiver = Python::attach(|py| {
            let (sender, receiver) = oneshot::channel();
            let callback = Py::new(py, TaskCancelCallback::new(self.task.clone_ref(py), sender))?;
            let kwargs = PyDict::new(py);
            kwargs.set_item("context", locals.context(py))?;
            locals.event_loop(py).call_method(
                "call_soon_threadsafe",
                (callback,),
                Some(&kwargs),
            )?;
            Ok::<_, PyErr>(receiver)
        })?;

        match receiver.await {
            Ok(result) => result,
            Err(_) => Err(PyRuntimeError::new_err(
                "Python task cancellation callback did not return a result",
            )),
        }
    }

    pub(crate) async fn wait(&self, locals: &TaskLocals) -> PyResult<()> {
        let future = Python::attach(|py| {
            pyo3_async_runtimes::into_future_with_locals(
                locals,
                self.task.clone_ref(py).into_bound(py),
            )
        })?;
        let result = future.await;
        scene_producer::task_finished(&self.scene, result.as_ref().err());
        result.map(|_| ())
    }

    pub(crate) async fn wait_scene_closed(&self) {
        self.scene.wait_closed().await;
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;
    use std::sync::Arc;

    use pyo3::prelude::*;
    use pyo3::types::{PyAnyMethods, PyList, PyListMethods, PyModule, PyWeakrefReference};
    use tokio::sync::oneshot;

    use crate::orchestration::actor_registry::ProductionState;
    use crate::orchestration::scene_context::{
        CuedScope, RunBinding, SceneScope, ScopeDriver, TaskFactoryAction,
    };

    use super::{
        HookCallback, ProvisionalPermitStack, SceneTaskCallback, TaskCancelCallback,
        TaskFactoryActionCallback, TaskFactoryWrapper, TaskLineage, TaskLineageRegistry,
        create_registered_scope_task,
    };

    struct AttributeRestore {
        module: Py<PyModule>,
        name: &'static str,
        value: Py<PyAny>,
    }

    impl Drop for AttributeRestore {
        fn drop(&mut self) {
            Python::attach(|py| {
                let _ = self.module.bind(py).setattr(self.name, self.value.bind(py));
            });
        }
    }

    fn assert_transported_identity(
        py: Python<'_>,
        production: Py<PyAny>,
        hook: &'static str,
        expected: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let (sender, mut receiver) = oneshot::channel();
        let callback = Py::new(py, HookCallback::new(production, hook, sender))?;

        let returned = callback.bind(py).call0()?;
        assert!(returned.is_none());

        let transported = receiver
            .try_recv()
            .expect("callback must send exactly one result");
        let error = match transported {
            Ok(_) => panic!("the callback unexpectedly returned an awaitable"),
            Err(error) => error,
        };
        assert!(error.value(py).is(expected));
        Ok(())
    }

    fn assert_scene_task_error(
        py: Python<'_>,
        production: Py<PyAny>,
        expected: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let (sender, mut receiver) = oneshot::channel();
        let callback = Py::new(
            py,
            SceneTaskCallback {
                binding: Some(Arc::new(RunBinding::new_for_test(py)?)),
                production: Some(production),
                sender: Some(sender),
            },
        )?;
        assert!(callback.bind(py).call0()?.is_none());
        let transported = receiver
            .try_recv()
            .expect("scene callback must send exactly one result");
        let error = match transported {
            Ok(_) => panic!("the scene callback unexpectedly returned a Task"),
            Err(error) => error,
        };
        assert!(error.value(py).is(expected));
        Ok(())
    }

    #[test]
    fn callback_transports_each_synchronous_error_without_raising() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let source = CString::new(
                "lookup_error = ValueError('lookup boom')\n".to_owned()
                    + "call_error = ValueError('call boom')\n"
                    + "create_task_error = ValueError('create-task boom')\n"
                    + "class LookupFailure:\n"
                    + "    @property\n"
                    + "    def start(self):\n"
                    + "        raise lookup_error\n"
                    + "class CallFailure:\n"
                    + "    def start(self):\n"
                    + "        raise call_error\n"
                    + "class CreateTaskFailure:\n"
                    + "    def scene(self):\n"
                    + "        return object()\n"
                    + "def fail_create_task(awaitable):\n"
                    + "    raise create_task_error\n",
            )
            .expect("test source has no nul byte");
            let module = PyModule::from_code(
                py,
                source.as_c_str(),
                c"python_task_test.py",
                c"python_task_test",
            )?;

            assert_transported_identity(
                py,
                module.getattr("LookupFailure")?.call0()?.unbind(),
                "start",
                &module.getattr("lookup_error")?,
            )?;
            assert_transported_identity(
                py,
                module.getattr("CallFailure")?.call0()?.unbind(),
                "start",
                &module.getattr("call_error")?,
            )?;

            let asyncio = py.import("asyncio")?;
            let restore_create_task = AttributeRestore {
                module: asyncio.clone().unbind(),
                name: "create_task",
                value: asyncio.getattr("create_task")?.unbind(),
            };
            asyncio.setattr("create_task", module.getattr("fail_create_task")?)?;
            assert_scene_task_error(
                py,
                module.getattr("CreateTaskFailure")?.call0()?.unbind(),
                &module.getattr("create_task_error")?,
            )?;
            drop(restore_create_task);

            Ok::<(), PyErr>(())
        })
        .expect("embedded Python test must succeed");
    }

    #[test]
    fn registered_scope_task_creation_closes_once_and_preserves_the_create_error() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let module = PyModule::from_code(
                py,
                c"def make_error(message):\n    try:\n        raise RuntimeError(message)\n    except RuntimeError as error:\n        return error\n\ncreate_boom = make_error('create task failed')\nclose_boom = make_error('driver close failed')\n\nclass Inner:\n    def __init__(self):\n        self.close_calls = 0\n    def close(self):\n        self.close_calls += 1\n        raise close_boom\n\ndef fail_create_task(awaitable):\n    raise create_boom\n",
                c"registered_scope_task_test.py",
                c"registered_scope_task_test",
            )?;
            let asyncio = py.import("asyncio")?;
            let restore_create_task = AttributeRestore {
                module: asyncio.clone().unbind(),
                name: "create_task",
                value: asyncio.getattr("create_task")?.unbind(),
            };
            asyncio.setattr("create_task", module.getattr("fail_create_task")?)?;

            let binding = Arc::new(RunBinding::new_for_test(py)?);
            let scene = SceneScope::zero_for_binding_for_test(
                py,
                "scene-registered-task-error",
                &binding,
            )?;
            let inner = module.getattr("Inner")?.call0()?;
            let driver = Py::new(
                py,
                ScopeDriver::new_scene(Arc::clone(&scene), inner.clone().unbind()),
            )?;
            let error = create_registered_scope_task(
                py,
                &binding,
                driver.bind(py).as_any(),
                TaskLineage::from_scene(&scene),
            )
            .expect_err("the injected create_task error must be returned");

            let create_boom = module.getattr("create_boom")?;
            let close_boom = module.getattr("close_boom")?;
            assert!(error.value(py).is(&create_boom));
            assert!(!create_boom.getattr("__traceback__")?.is_none());
            assert!(create_boom.getattr("__context__")?.is_none());
            assert!(create_boom.getattr("__cause__")?.is_none());
            assert!(!error.value(py).is(&close_boom));
            assert_eq!(inner.getattr("close_calls")?.extract::<usize>()?, 1);
            assert!(!scene.is_open());

            drop(restore_create_task);
            Ok::<(), PyErr>(())
        })
        .expect("registered scope Task creation must preserve its primary error");
    }

    #[test]
    fn weak_task_registry_is_identity_based_and_raii_safe() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let module = PyModule::from_code(
                py,
                c"class HostileTask:\n    def __hash__(self): raise AssertionError('hash')\n    def __eq__(self, other): raise AssertionError('eq')\n",
                c"task_registry_test.py",
                c"task_registry_test",
            )?;
            let task_type = module.getattr("HostileTask")?;
            let first = task_type.call0()?;
            let equal_but_distinct = task_type.call0()?;
            let first_ref = py.import("weakref")?.call_method1("ref", (&first,))?;

            let mut registry = TaskLineageRegistry::default();
            registry.register(&first, TaskLineage::new_for_test(7))?;
            assert_eq!(
                registry
                    .lookup(&first)?
                    .expect("exact task must resolve")
                    .id_for_test(),
                7
            );
            assert!(registry.lookup(&equal_but_distinct)?.is_none());
            assert!(!registry.unregister(&equal_but_distinct));
            assert_eq!(
                registry
                    .lookup(&first)?
                    .expect("distinct unregister must preserve the exact Task")
                    .id_for_test(),
                7
            );
            assert!(registry.unregister(&first));
            assert!(registry.lookup(&first)?.is_none());
            registry.register(&first, TaskLineage::new_for_test(7))?;

            drop(first);
            py.import("gc")?.call_method0("collect")?;
            assert!(first_ref.call0()?.is_none());
            assert_eq!(registry.live_len_for_test()?, 0);
            assert_eq!(registry.live_len_for_test()?, 0);
            Ok::<_, PyErr>(())
        })
        .expect("identity registry must not dispatch Python hash or equality");
    }

    #[test]
    fn provisional_permit_is_lifo_exact_and_raii_safe() {
        let stack = ProvisionalPermitStack::default();
        let outer = stack.push_for_test(10);
        {
            let inner = stack.push_for_test(20);
            assert!(stack.consume_for_test(10).is_none());
            assert_eq!(stack.consume_for_test(20), Some(20));
            assert!(stack.consume_for_test(20).is_none());
            drop(inner);
        }
        assert_eq!(stack.consume_for_test(10), Some(10));
        drop(outer);
        assert!(stack.consume_for_test(10).is_none());

        {
            let rollback = stack.push_for_test(30);
            drop(rollback);
        }
        assert!(stack.consume_for_test(30).is_none());
    }

    #[test]
    fn cued_lineage_activity_is_independent_of_scene_closing() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let scene = SceneScope::zero_for_test(py, "scene-cued-lineage")?;
            let cued = CuedScope::new_for_test(Arc::clone(&scene), "lineage-owner", 1);
            let lineage = TaskLineage::from_cued(&cued);
            assert!(lineage.is_active());
            scene.close();
            assert!(!scene.is_open());
            assert!(cued.is_active());
            assert!(
                lineage.is_active(),
                "an active CuedScope must survive SceneScope Closing"
            );
            cued.close_inline();
            assert!(!lineage.is_active());
            Ok::<_, PyErr>(())
        })
        .expect("Cued lineage lifetime test must complete");
    }

    #[test]
    fn task_factory_wrapper_does_not_own_the_run_binding() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let binding = Arc::new(RunBinding::new_for_test(py)?);
            let binding_weak = Arc::downgrade(&binding);
            let wrapper = TaskFactoryWrapper::new(Arc::downgrade(&binding));
            drop(binding);

            assert!(binding_weak.upgrade().is_none());
            assert!(wrapper.binding_for_test().is_none());
            Ok::<_, PyErr>(())
        })
        .expect("the loop-owned wrapper must keep only a weak run binding");
    }

    #[test]
    fn task_callbacks_release_python_reference_cycles() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let module = PyModule::from_code(
                py,
                c"class Marker: pass\nclass Loop:\n    def get_task_factory(self): return None\n",
                c"task_callback_gc_test.py",
                c"task_callback_gc_test",
            )?;
            let marker_type = module.getattr("Marker")?;

            let hook_owner = PyList::empty(py);
            let hook_marker = marker_type.call0()?;
            let hook_marker_ref = PyWeakrefReference::new(&hook_marker)?.unbind();
            hook_owner.append(&hook_marker)?;
            let (hook_sender, hook_receiver) = oneshot::channel();
            let hook_callback = Py::new(
                py,
                HookCallback::new(hook_owner.clone().unbind().into_any(), "start", hook_sender),
            )?;
            hook_owner.append(hook_callback.bind(py))?;
            drop((hook_receiver, hook_callback, hook_marker, hook_owner));

            let cancel_owner = PyList::empty(py);
            let cancel_marker = marker_type.call0()?;
            let cancel_marker_ref = PyWeakrefReference::new(&cancel_marker)?.unbind();
            cancel_owner.append(&cancel_marker)?;
            let (cancel_sender, cancel_receiver) = oneshot::channel();
            let cancel_callback = Py::new(
                py,
                TaskCancelCallback::new(cancel_owner.clone().unbind().into_any(), cancel_sender),
            )?;
            cancel_owner.append(cancel_callback.bind(py))?;
            drop((
                cancel_receiver,
                cancel_callback,
                cancel_marker,
                cancel_owner,
            ));

            let scene_owner = PyList::empty(py);
            let scene_marker = marker_type.call0()?;
            let scene_marker_ref = PyWeakrefReference::new(&scene_marker)?.unbind();
            scene_owner.append(&scene_marker)?;
            let (scene_sender, scene_receiver) = oneshot::channel();
            let scene_callback = Py::new(
                py,
                SceneTaskCallback {
                    binding: Some(Arc::new(RunBinding::new_for_test(py)?)),
                    production: Some(scene_owner.clone().unbind().into_any()),
                    sender: Some(scene_sender),
                },
            )?;
            scene_owner.append(scene_callback.bind(py))?;
            drop((scene_receiver, scene_callback, scene_marker, scene_owner));

            let factory_loop = module.getattr("Loop")?.call0()?;
            let factory_marker = marker_type.call0()?;
            let factory_marker_ref = PyWeakrefReference::new(&factory_marker)?.unbind();
            factory_loop.setattr("marker", &factory_marker)?;
            let state = Arc::new(ProductionState::new());
            let binding = RunBinding::new(py, &state, &factory_loop)?;
            let (factory_sender, factory_receiver) = oneshot::channel();
            let factory_callback = Py::new(
                py,
                TaskFactoryActionCallback {
                    binding: Some(binding),
                    action: TaskFactoryAction::Install,
                    sender: Some(factory_sender),
                },
            )?;
            factory_loop.setattr("callback", factory_callback.bind(py))?;
            drop((
                factory_receiver,
                factory_callback,
                factory_marker,
                factory_loop,
                state,
            ));

            py.import("gc")?.call_method0("collect")?;
            assert!(hook_marker_ref.bind(py).call0()?.is_none());
            assert!(cancel_marker_ref.bind(py).call0()?.is_none());
            assert!(scene_marker_ref.bind(py).call0()?.is_none());
            assert!(factory_marker_ref.bind(py).call0()?.is_none());
            Ok::<_, PyErr>(())
        })
        .expect("task callback Python edges must participate in cyclic GC");
    }
}
