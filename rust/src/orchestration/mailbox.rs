use std::collections::VecDeque;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyTuple};

use crate::diagnostic_runtime::cue_producer::{self, CueCaptureMode, CueHook, CueTerminalOutcome};
use crate::diagnostic_runtime::effect_producer::{self, EffectHook};
use crate::orchestration::actor::ActorCapability;
use crate::orchestration::cue::Cue;
use crate::orchestration::effect::Effect;
use crate::orchestration::python_task::{TaskLineage, create_registered_scope_task};
#[cfg(test)]
use crate::orchestration::scene_context::{AdmissionMetric, record_admission_metric_for_test};
use crate::orchestration::scene_context::{CuedScope, RunBinding, SceneScope, ScopeDriver};

#[cfg(test)]
static VALIDATION_CALLS: AtomicUsize = AtomicUsize::new(0);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn validate_cued_result(value: &Bound<'_, PyAny>) -> PyResult<Py<PyTuple>> {
    #[cfg(test)]
    VALIDATION_CALLS.fetch_add(1, Ordering::SeqCst);
    let tuple = value.cast::<PyTuple>().map_err(|_| {
        PyTypeError::new_err("Actor.cued() must return a tuple of Effect instances")
    })?;
    for (index, item) in tuple.iter().enumerate() {
        if !item.is_instance_of::<Effect>() {
            return Err(PyTypeError::new_err(format!(
                "Actor.cued() return item at index {index} is not an Effect"
            )));
        }
    }
    for item in tuple.iter() {
        let effect = item
            .cast::<Effect>()
            .expect("validated cued result items are Effects");
        effect_producer::observe(&effect.borrow(), EffectHook::Returned);
    }
    Ok(tuple.clone().unbind())
}

fn trusted_task_result(py: Python<'_>, task: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    py.import("asyncio")?
        .getattr("Future")?
        .call_method1("result", (task,))
        .map(Bound::unbind)
}

pub(crate) enum DispatchOutcome {
    Terminal(TerminalAction),
    Scheduled,
}

pub(crate) enum OperationPhase {
    Queued,
    Running { task: Py<PyAny> },
    CancellingRunning { task: Py<PyAny> },
    Terminal(TerminalOutcome),
}

pub(crate) enum TerminalOutcome {
    Success(Py<PyTuple>),
    Failure(Py<PyAny>),
    Cancelled(Py<PyAny>),
    CleanupFailure(Py<PyAny>),
}

pub(crate) enum CancellationIntent {
    Requested { dispatch_error: Option<Py<PyAny>> },
    AttachmentFailure { error: Py<PyAny> },
}

pub(crate) struct OperationState {
    phase: OperationPhase,
    intent: Option<CancellationIntent>,
    cue: Option<Py<Cue>>,
    signal: Option<Py<PyAny>>,
    #[cfg(test)]
    forced_callback_attachment_error: Option<Py<PyAny>>,
}

pub(crate) struct TerminalAction {
    diagnostic_outcome: CueTerminalOutcome,
    close_cued: bool,
    displaced_phase: OperationPhase,
    displaced_intent: Option<CancellationIntent>,
    displaced_cue: Option<Py<Cue>>,
    signal: Option<Py<PyAny>>,
    #[cfg(test)]
    displaced_forced_callback_attachment_error: Option<Py<PyAny>>,
}

pub(crate) struct TerminalDelivery {
    pub(crate) next: Option<CueOperation>,
    pub(crate) signal: Option<Py<PyAny>>,
}

#[derive(Default)]
pub(crate) struct MailboxTerminalTransition {
    removed: Option<CueOperation>,
    retired: Option<Running>,
    next: Option<CueOperation>,
}

#[derive(Clone, Copy)]
enum CompletionSnapshot {
    Validate,
    Requested,
    AttachmentFailure,
    Terminal,
}

enum ObservedTaskResult {
    Normal(Py<PyAny>),
    Error { error: Py<PyAny>, cancelled: bool },
}

enum ValidatedNormal {
    Success(Py<PyTuple>),
    Failure(Py<PyAny>),
}

#[derive(Clone)]
pub(crate) struct CueOperation {
    inner: Arc<CueOperationInner>,
}

pub(crate) struct CueOperationInner {
    #[cfg(test)]
    index: usize,
    #[cfg(test)]
    cancel_requests: AtomicUsize,
    scene: Weak<SceneScope>,
    actor: Weak<ActorCapability>,
    binding: Weak<RunBinding>,
    capture_mode: CueCaptureMode,
    cued: Arc<CuedScope>,
    state: Mutex<OperationState>,
}

impl CueOperation {
    #[allow(dead_code)]
    pub(crate) fn diagnostic_binding(&self) -> Option<Arc<RunBinding>> {
        self.inner.binding.upgrade()
    }

    #[allow(dead_code)]
    pub(crate) fn diagnostic_scene(&self) -> Option<Arc<SceneScope>> {
        self.inner.scene.upgrade()
    }

    #[allow(dead_code)]
    pub(crate) fn diagnostic_actor(&self) -> Option<Arc<ActorCapability>> {
        self.inner.actor.upgrade()
    }

    #[allow(dead_code)]
    pub(crate) fn diagnostic_cued(&self) -> &Arc<CuedScope> {
        &self.inner.cued
    }

    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn diagnostic_capture_mode(&self) -> CueCaptureMode {
        self.inner.capture_mode
    }

    pub(crate) fn enqueue(&self) -> PyResult<()> {
        if self.is_terminal() {
            return Ok(());
        }
        let Some(actor) = self.inner.actor.upgrade() else {
            #[cfg(test)]
            return Ok(());
            #[cfg(not(test))]
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "Actor is no longer alive",
            ));
        };
        actor.enqueue_operation(self.clone())?;
        #[cfg(test)]
        if self.inner.binding.upgrade().is_some() {
            record_admission_metric_for_test(AdmissionMetric::Enqueue);
        }
        Ok(())
    }

    pub(crate) fn new_runtime(
        scene: &Arc<SceneScope>,
        actor: &Arc<ActorCapability>,
        binding: &Arc<RunBinding>,
        cued: Arc<CuedScope>,
        cue: Py<Cue>,
        signal: Py<PyAny>,
        capture_mode: CueCaptureMode,
    ) -> Self {
        Self {
            inner: Arc::new(CueOperationInner {
                #[cfg(test)]
                index: 0,
                #[cfg(test)]
                cancel_requests: AtomicUsize::new(0),
                scene: Arc::downgrade(scene),
                actor: Arc::downgrade(actor),
                binding: Arc::downgrade(binding),
                capture_mode,
                cued,
                state: Mutex::new(OperationState {
                    phase: OperationPhase::Queued,
                    intent: None,
                    cue: Some(cue),
                    signal: Some(signal),
                    #[cfg(test)]
                    forced_callback_attachment_error: None,
                }),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(index: usize) -> Self {
        Self::new(index, Weak::new())
    }

    #[cfg(test)]
    pub(crate) fn new_for_scene_for_test(index: usize, scene: &Arc<SceneScope>) -> Self {
        Self::new(index, Arc::downgrade(scene))
    }

    #[cfg(test)]
    pub(crate) fn new_for_actor_for_test(
        index: usize,
        actor: &Arc<ActorCapability>,
        scene: &Arc<SceneScope>,
    ) -> Self {
        Self::new_with_actor(index, Arc::downgrade(scene), Arc::downgrade(actor))
    }

    #[cfg(test)]
    fn new(index: usize, scene: Weak<SceneScope>) -> Self {
        Self::new_with_actor(index, scene, Weak::new())
    }

    #[cfg(test)]
    fn new_with_actor(index: usize, scene: Weak<SceneScope>, actor: Weak<ActorCapability>) -> Self {
        let cued_scene = scene.upgrade().unwrap_or_else(|| {
            Python::attach(|py| {
                SceneScope::zero_for_test(py, "scene-operation-fixture")
                    .expect("test SceneScope must construct")
            })
        });
        Self {
            inner: Arc::new(CueOperationInner {
                index,
                cancel_requests: AtomicUsize::new(0),
                scene,
                actor,
                binding: Weak::new(),
                capture_mode: CueCaptureMode::Capture,
                cued: CuedScope::new_for_test(cued_scene, "operation-fixture", index),
                state: Mutex::new(OperationState {
                    phase: OperationPhase::Queued,
                    intent: None,
                    cue: None,
                    signal: None,
                    forced_callback_attachment_error: None,
                }),
            }),
        }
    }

    pub(crate) fn dispatch(&self) -> DispatchOutcome {
        Python::attach(|py| match self.dispatch_with_gil(py) {
            Ok(()) => DispatchOutcome::Scheduled,
            Err(error) => DispatchOutcome::Terminal(
                self.transition_from_result(py, Err(error))
                    .expect("a dispatched queued operation must terminalize exactly once"),
            ),
        })
    }

    fn dispatch_with_gil(&self, py: Python<'_>) -> PyResult<()> {
        let actor =
            self.inner.actor.upgrade().ok_or_else(|| {
                pyo3::exceptions::PyRuntimeError::new_err("Actor is no longer alive")
            })?;
        let binding = self.inner.binding.upgrade().ok_or_else(|| {
            pyo3::exceptions::PyRuntimeError::new_err("Production runtime is no longer active")
        })?;
        let cue = {
            let state = lock(&self.inner.state);
            assert!(matches!(state.phase, OperationPhase::Queued));
            state
                .cue
                .as_ref()
                .map(|cue| cue.clone_ref(py))
                .expect("runtime CueOperation must own a Cue")
        };
        let cued = Arc::clone(&self.inner.cued);
        let callback = Py::new(
            py,
            CueDoneCallback {
                operation: Arc::downgrade(&self.inner),
            },
        )?;
        let awaitable = actor.actor(py).bind(py).call_method1("cued", (cue,))?;
        let driver = Py::new(
            py,
            ScopeDriver::new_cued(Arc::clone(&cued), awaitable.unbind()),
        )?;
        let task = create_registered_scope_task(
            py,
            &binding,
            driver.bind(py).as_any(),
            TaskLineage::from_cued(&cued),
        )?;
        let previous = {
            let mut state = lock(&self.inner.state);
            std::mem::replace(
                &mut state.phase,
                OperationPhase::Running {
                    task: task.clone_ref(py),
                },
            )
        };
        assert!(matches!(previous, OperationPhase::Queued));
        drop(previous);

        if let Err(error) = self.attach_done_callback(py, task.bind(py), callback.bind(py)) {
            self.begin_attachment_failure(py, &binding, task.bind(py), error);
        }
        cue_producer::observe(self, CueHook::Dispatched);
        Ok(())
    }

    fn attach_done_callback(
        &self,
        py: Python<'_>,
        task: &Bound<'_, PyAny>,
        callback: &Bound<'_, CueDoneCallback>,
    ) -> PyResult<()> {
        #[cfg(test)]
        {
            let forced = {
                lock(&self.inner.state)
                    .forced_callback_attachment_error
                    .take()
            };
            if let Some(error) = forced {
                return Err(PyErr::from_value(error.into_bound(py)));
            }
        }
        py.import("asyncio")?
            .getattr("Future")?
            .call_method1("add_done_callback", (task, callback))?;
        Ok(())
    }

    fn begin_attachment_failure(
        &self,
        py: Python<'_>,
        binding: &Arc<RunBinding>,
        task: &Bound<'_, PyAny>,
        error: PyErr,
    ) {
        let error = error.into_value(py).into_any();
        let (armed, displaced_error) = {
            let mut state = lock(&self.inner.state);
            let previous = std::mem::replace(&mut state.phase, OperationPhase::Queued);
            match previous {
                OperationPhase::Running { task } => {
                    state.phase = OperationPhase::CancellingRunning { task };
                    state.intent = Some(CancellationIntent::AttachmentFailure { error });
                    (true, None)
                }
                other => {
                    state.phase = other;
                    (false, Some(error))
                }
            }
        };
        drop(displaced_error);
        if !armed {
            return;
        }
        if let Ok(task_type) = py
            .import("asyncio")
            .and_then(|module| module.getattr("Task"))
        {
            let _ = task_type.call_method1("cancel", (task,));
        }
        if let Ok(fallback) = Py::new(
            py,
            CueSetupFallbackCallback {
                operation: Arc::downgrade(&self.inner),
            },
        ) {
            let _ = binding
                .event_loop(py)
                .bind(py)
                .call_method1("call_soon", (fallback,));
        }
    }

    fn cancelled_error(py: Python<'_>) -> Py<PyAny> {
        py.import("asyncio")
            .and_then(|module| module.getattr("CancelledError"))
            .and_then(|error_type| error_type.call0())
            .expect("asyncio.CancelledError must remain constructible")
            .unbind()
    }

    pub(crate) fn request_cancel(&self) -> bool {
        Python::attach(|py| {
            let cancelled = Self::cancelled_error(py);
            enum RequestAction {
                Queued(TerminalAction),
                Running(Py<PyAny>),
            }
            let action = {
                let mut state = lock(&self.inner.state);
                let previous = std::mem::replace(&mut state.phase, OperationPhase::Queued);
                match previous {
                    OperationPhase::Queued => {
                        state.phase =
                            OperationPhase::Terminal(TerminalOutcome::Cancelled(cancelled));
                        let action = TerminalAction {
                            diagnostic_outcome: CueTerminalOutcome::Cancelled,
                            close_cued: true,
                            displaced_phase: OperationPhase::Queued,
                            displaced_intent: state.intent.take(),
                            displaced_cue: state.cue.take(),
                            signal: state.signal.as_ref().map(|signal| signal.clone_ref(py)),
                            #[cfg(test)]
                            displaced_forced_callback_attachment_error: state
                                .forced_callback_attachment_error
                                .take(),
                        };
                        Some(RequestAction::Queued(action))
                    }
                    OperationPhase::Running { task } => {
                        let dispatch_task = task.clone_ref(py);
                        state.phase = OperationPhase::CancellingRunning { task };
                        state.intent = Some(CancellationIntent::Requested {
                            dispatch_error: None,
                        });
                        Some(RequestAction::Running(dispatch_task))
                    }
                    other @ (OperationPhase::CancellingRunning { .. }
                    | OperationPhase::Terminal(_)) => {
                        state.phase = other;
                        None
                    }
                }
            };
            let Some(action) = action else {
                return false;
            };
            cue_producer::observe(self, CueHook::CancelRequested);
            #[cfg(test)]
            self.inner.cancel_requests.fetch_add(1, Ordering::Relaxed);
            match action {
                RequestAction::Queued(action) => self.perform_terminal_action(action),
                RequestAction::Running(task) => {
                    if let Err(error) = task.bind(py).call_method0("cancel") {
                        let error = error.into_value(py).into_any();
                        let unclaimed = {
                            let mut state = lock(&self.inner.state);
                            match &mut state.intent {
                                Some(CancellationIntent::Requested { dispatch_error })
                                    if dispatch_error.is_none() =>
                                {
                                    *dispatch_error = Some(error);
                                    None
                                }
                                _ => Some(error),
                            }
                        };
                        drop(unclaimed);
                    }
                }
            }
            true
        })
    }

    fn completion_snapshot(&self) -> CompletionSnapshot {
        let state = lock(&self.inner.state);
        match (&state.phase, &state.intent) {
            (OperationPhase::Queued | OperationPhase::Running { .. }, _) => {
                CompletionSnapshot::Validate
            }
            (
                OperationPhase::CancellingRunning { .. },
                Some(CancellationIntent::Requested { .. }),
            ) => CompletionSnapshot::Requested,
            (
                OperationPhase::CancellingRunning { .. },
                Some(CancellationIntent::AttachmentFailure { .. }),
            ) => CompletionSnapshot::AttachmentFailure,
            (OperationPhase::Terminal(_), _) => CompletionSnapshot::Terminal,
            (OperationPhase::CancellingRunning { .. }, None) => {
                panic!("cancelling operation must retain its intent")
            }
        }
    }

    fn terminal_error(&self) -> Option<PyErr> {
        Python::attach(|py| {
            let state = lock(&self.inner.state);
            let error = match &state.phase {
                OperationPhase::Terminal(
                    TerminalOutcome::Failure(error)
                    | TerminalOutcome::Cancelled(error)
                    | TerminalOutcome::CleanupFailure(error),
                ) => Some(error),
                OperationPhase::Terminal(TerminalOutcome::Success(_))
                | OperationPhase::Queued
                | OperationPhase::Running { .. }
                | OperationPhase::CancellingRunning { .. } => None,
            }?;
            Some(PyErr::from_value(error.clone_ref(py).into_bound(py)))
        })
    }

    fn transition_from_result(
        &self,
        py: Python<'_>,
        result: PyResult<Py<PyAny>>,
    ) -> Option<TerminalAction> {
        let snapshot = self.completion_snapshot();
        if matches!(snapshot, CompletionSnapshot::Terminal) {
            return None;
        }
        let observed = match result {
            Ok(value) => ObservedTaskResult::Normal(value),
            Err(error) => {
                let cancelled = py
                    .import("asyncio")
                    .and_then(|module| module.getattr("CancelledError"))
                    .is_ok_and(|cancelled| error.is_instance(py, &cancelled));
                ObservedTaskResult::Error {
                    error: error.into_value(py).into_any(),
                    cancelled,
                }
            }
        };
        let validated = match (&snapshot, &observed) {
            (CompletionSnapshot::Validate, ObservedTaskResult::Normal(value)) => {
                Some(match validate_cued_result(value.bind(py)) {
                    Ok(result) => ValidatedNormal::Success(result),
                    Err(error) => ValidatedNormal::Failure(error.into_value(py).into_any()),
                })
            }
            _ => None,
        };
        let cancelled_fallback = matches!(
            (&snapshot, &observed),
            (CompletionSnapshot::Requested, ObservedTaskResult::Normal(_))
        )
        .then(|| Self::cancelled_error(py));

        let mut state = lock(&self.inner.state);
        let displaced_phase = std::mem::replace(&mut state.phase, OperationPhase::Queued);
        if matches!(displaced_phase, OperationPhase::Terminal(_)) {
            state.phase = displaced_phase;
            return None;
        }
        let displaced_intent = state.intent.take();
        let outcome = match &displaced_phase {
            OperationPhase::Queued | OperationPhase::Running { .. } => match &observed {
                ObservedTaskResult::Normal(_) => match validated
                    .as_ref()
                    .expect("normal running completion must be validated")
                {
                    ValidatedNormal::Success(result) => {
                        TerminalOutcome::Success(result.clone_ref(py))
                    }
                    ValidatedNormal::Failure(error) => {
                        TerminalOutcome::Failure(error.clone_ref(py))
                    }
                },
                ObservedTaskResult::Error { error, cancelled } => {
                    if *cancelled {
                        TerminalOutcome::Cancelled(error.clone_ref(py))
                    } else {
                        TerminalOutcome::Failure(error.clone_ref(py))
                    }
                }
            },
            OperationPhase::CancellingRunning { .. } => match displaced_intent
                .as_ref()
                .expect("cancelling completion must retain an intent")
            {
                CancellationIntent::AttachmentFailure { error } => {
                    TerminalOutcome::Failure(error.clone_ref(py))
                }
                CancellationIntent::Requested { dispatch_error } => match &observed {
                    ObservedTaskResult::Error {
                        error,
                        cancelled: false,
                    } => TerminalOutcome::CleanupFailure(error.clone_ref(py)),
                    _ if dispatch_error.is_some() => TerminalOutcome::CleanupFailure(
                        dispatch_error
                            .as_ref()
                            .expect("checked dispatch error must exist")
                            .clone_ref(py),
                    ),
                    ObservedTaskResult::Error { error, .. } => {
                        TerminalOutcome::Cancelled(error.clone_ref(py))
                    }
                    ObservedTaskResult::Normal(_) => TerminalOutcome::Cancelled(
                        cancelled_fallback
                            .as_ref()
                            .expect("cancelled normal completion needs a cancellation value")
                            .clone_ref(py),
                    ),
                },
            },
            OperationPhase::Terminal(_) => unreachable!(),
        };
        let diagnostic_outcome = match &outcome {
            TerminalOutcome::Success(_) => CueTerminalOutcome::Completed,
            TerminalOutcome::Failure(_) => CueTerminalOutcome::Failed,
            TerminalOutcome::Cancelled(_) => CueTerminalOutcome::Cancelled,
            TerminalOutcome::CleanupFailure(_) => CueTerminalOutcome::CleanupFailed,
        };
        state.phase = OperationPhase::Terminal(outcome);
        let action = TerminalAction {
            diagnostic_outcome,
            close_cued: matches!(displaced_phase, OperationPhase::Queued),
            displaced_phase,
            displaced_intent,
            displaced_cue: state.cue.take(),
            signal: state.signal.as_ref().map(|signal| signal.clone_ref(py)),
            #[cfg(test)]
            displaced_forced_callback_attachment_error: state
                .forced_callback_attachment_error
                .take(),
        };
        drop(state);
        Some(action)
    }

    fn finish_with_result(&self, py: Python<'_>, result: PyResult<Py<PyAny>>) -> bool {
        let Some(action) = self.transition_from_result(py, result) else {
            return false;
        };
        self.perform_terminal_action(action);
        true
    }

    fn finish_from_task(&self, py: Python<'_>, task: &Bound<'_, PyAny>) {
        self.finish_with_result(py, trusted_task_result(py, task));
    }

    fn perform_terminal_action(&self, action: TerminalAction) {
        cue_producer::terminal(self, action.diagnostic_outcome, || self.terminal_error());
        if let Some(actor) = self.inner.actor.upgrade() {
            actor.finish_terminal_action(self.clone(), action);
        } else {
            let delivery = self.complete_terminal_action(action, None);
            debug_assert!(delivery.next.is_none());
            Self::resolve_terminal_signal(delivery.signal);
        }
    }

    pub(crate) fn complete_terminal_action(
        &self,
        action: TerminalAction,
        actor: Option<&ActorCapability>,
    ) -> TerminalDelivery {
        let TerminalAction {
            diagnostic_outcome,
            close_cued,
            displaced_phase,
            displaced_intent,
            displaced_cue,
            signal,
            #[cfg(test)]
            displaced_forced_callback_attachment_error,
        } = action;
        if close_cued {
            self.inner.cued.close_inline();
        }
        let MailboxTerminalTransition {
            removed,
            retired,
            next,
        } = actor
            .map(|actor| actor.unlink_terminal_operation(self))
            .unwrap_or_default();
        let scene_owner = self
            .inner
            .scene
            .upgrade()
            .and_then(|scene| scene.operation_finished(self));
        #[cfg(test)]
        drop((
            diagnostic_outcome,
            displaced_phase,
            displaced_intent,
            displaced_cue,
            displaced_forced_callback_attachment_error,
            removed,
            retired,
            scene_owner,
        ));
        #[cfg(not(test))]
        drop((
            diagnostic_outcome,
            displaced_phase,
            displaced_intent,
            displaced_cue,
            removed,
            retired,
            scene_owner,
        ));
        TerminalDelivery { next, signal }
    }

    pub(crate) fn resolve_terminal_signal(signal: Option<Py<PyAny>>) {
        let Some(signal) = signal else {
            return;
        };
        Python::attach(|py| {
            let Ok(future) = py
                .import("asyncio")
                .and_then(|module| module.getattr("Future"))
            else {
                return;
            };
            let done = future
                .call_method1("done", (signal.bind(py),))
                .and_then(|done| done.is_truthy())
                .unwrap_or(true);
            if !done {
                let _ = future.call_method1("set_result", (signal.bind(py), py.None()));
            }
        });
    }

    pub(crate) fn result_for_caller(
        &self,
        py: Python<'_>,
        caller_cancellation: Option<&Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let result = {
            let state = lock(&self.inner.state);
            match &state.phase {
                OperationPhase::Terminal(TerminalOutcome::Success(result)) => {
                    Ok(result.clone_ref(py).into_any())
                }
                OperationPhase::Terminal(TerminalOutcome::Failure(error))
                | OperationPhase::Terminal(TerminalOutcome::CleanupFailure(error)) => {
                    Err(error.clone_ref(py))
                }
                OperationPhase::Terminal(TerminalOutcome::Cancelled(error)) => {
                    Err(caller_cancellation
                        .map(|caller| caller.clone_ref(py))
                        .unwrap_or_else(|| error.clone_ref(py)))
                }
                _ => panic!("terminal signal must follow a terminal outcome"),
            }
        };
        result.map_err(|error| PyErr::from_value(error.into_bound(py)))
    }

    pub(crate) fn is_terminal(&self) -> bool {
        matches!(lock(&self.inner.state).phase, OperationPhase::Terminal(_))
    }

    pub(crate) fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub(crate) fn traverse(
        &self,
        visit: &pyo3::class::gc::PyVisit<'_>,
    ) -> Result<(), pyo3::class::gc::PyTraverseError> {
        self.inner.cued.traverse_python_edges(visit)?;
        let state = lock(&self.inner.state);
        visit.call(&state.cue)?;
        visit.call(&state.signal)?;
        #[cfg(test)]
        visit.call(&state.forced_callback_attachment_error)?;
        match &state.phase {
            OperationPhase::Running { task } | OperationPhase::CancellingRunning { task } => {
                visit.call(task)?;
            }
            OperationPhase::Terminal(TerminalOutcome::Success(result)) => visit.call(result)?,
            OperationPhase::Terminal(
                TerminalOutcome::Failure(error)
                | TerminalOutcome::Cancelled(error)
                | TerminalOutcome::CleanupFailure(error),
            ) => visit.call(error)?,
            OperationPhase::Queued => {}
        }
        match &state.intent {
            Some(CancellationIntent::Requested {
                dispatch_error: Some(error),
            })
            | Some(CancellationIntent::AttachmentFailure { error }) => visit.call(error),
            _ => Ok(()),
        }
    }

    #[cfg(test)]
    pub(crate) fn finish_cancel(&self) {
        Python::attach(|py| {
            let previous = {
                let mut state = lock(&self.inner.state);
                let previous = std::mem::replace(
                    &mut state.phase,
                    OperationPhase::Terminal(TerminalOutcome::Cancelled(Self::cancelled_error(py))),
                );
                let intent = state.intent.take();
                (previous, intent)
            };
            drop(previous);
        });
    }

    #[cfg(test)]
    pub(crate) fn index_for_test(&self) -> usize {
        self.inner.index
    }

    #[cfg(test)]
    pub(crate) fn cancel_requests_for_test(&self) -> usize {
        self.inner.cancel_requests.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn downgrade_for_test(&self) -> Weak<CueOperationInner> {
        Arc::downgrade(&self.inner)
    }

    #[cfg(test)]
    pub(crate) fn actor_mailbox_unlinked_for_test(&self) -> bool {
        match self.inner.actor.upgrade() {
            Some(actor) => actor.operation_absent_for_test(self),
            None => true,
        }
    }

    #[cfg(test)]
    pub(crate) fn force_callback_attachment_error_for_test(&self, error: Py<PyAny>) {
        let previous = {
            let mut state = lock(&self.inner.state);
            state.forced_callback_attachment_error.replace(error)
        };
        drop(previous);
    }

    #[cfg(test)]
    pub(crate) fn force_running_for_test(&self, task: Py<PyAny>) {
        let previous = {
            let mut state = lock(&self.inner.state);
            let phase = std::mem::replace(&mut state.phase, OperationPhase::Running { task });
            let intent = state.intent.take();
            (phase, intent)
        };
        drop(previous);
    }

    #[cfg(test)]
    pub(crate) fn set_signal_for_test(&self, signal: Py<PyAny>) {
        let previous = lock(&self.inner.state).signal.replace(signal);
        drop(previous);
    }

    #[cfg(test)]
    pub(crate) fn signal_for_test(&self, py: Python<'_>) -> Py<PyAny> {
        lock(&self.inner.state)
            .signal
            .as_ref()
            .expect("test operation must retain a signal")
            .clone_ref(py)
    }

    #[cfg(test)]
    pub(crate) fn finish_for_test(&self, py: Python<'_>, result: PyResult<Py<PyAny>>) -> bool {
        self.finish_with_result(py, result)
    }

    #[cfg(test)]
    pub(crate) fn terminal_success_for_test(&self, py: Python<'_>) -> Option<Py<PyTuple>> {
        let state = lock(&self.inner.state);
        match &state.phase {
            OperationPhase::Terminal(TerminalOutcome::Success(result)) => {
                Some(result.clone_ref(py))
            }
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn terminal_error_for_test(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        let state = lock(&self.inner.state);
        match &state.phase {
            OperationPhase::Terminal(
                TerminalOutcome::Failure(error)
                | TerminalOutcome::Cancelled(error)
                | TerminalOutcome::CleanupFailure(error),
            ) => Some(error.clone_ref(py)),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn terminal_owns_no_task_or_intent_for_test(&self) -> bool {
        let state = lock(&self.inner.state);
        matches!(state.phase, OperationPhase::Terminal(_)) && state.intent.is_none()
    }

    #[cfg(test)]
    pub(crate) fn install_traversal_edge_for_test(
        &self,
        py: Python<'_>,
        edge_name: &str,
        edge: Py<PyAny>,
    ) -> PyResult<()> {
        let displaced = {
            let mut state = lock(&self.inner.state);
            match edge_name {
                "signal" => (None, None, state.signal.replace(edge)),
                "task" => {
                    let phase =
                        std::mem::replace(&mut state.phase, OperationPhase::Running { task: edge });
                    (Some(phase), state.intent.take(), None)
                }
                "intent" => {
                    let phase = std::mem::replace(
                        &mut state.phase,
                        OperationPhase::CancellingRunning { task: py.None() },
                    );
                    let intent = state.intent.replace(CancellationIntent::Requested {
                        dispatch_error: Some(edge),
                    });
                    (Some(phase), intent, None)
                }
                "outcome" => {
                    let phase = std::mem::replace(
                        &mut state.phase,
                        OperationPhase::Terminal(TerminalOutcome::Failure(edge)),
                    );
                    (Some(phase), state.intent.take(), None)
                }
                "cue" => {
                    let cue = Py::new(
                        py,
                        Cue::new_runtime(
                            pyo3::types::PyString::new(py, "traversal-cue0").unbind(),
                            edge,
                            pyo3::types::PyString::new(py, "traversal-scene").unbind(),
                        ),
                    )?;
                    (None, None, state.cue.replace(cue).map(|cue| cue.into_any()))
                }
                _ => panic!("unknown traversal edge {edge_name}"),
            }
        };
        drop(displaced);
        Ok(())
    }
}

#[pyclass]
struct CueDoneCallback {
    operation: Weak<CueOperationInner>,
}

#[pymethods]
impl CueDoneCallback {
    fn __call__(&self, py: Python<'_>, task: &Bound<'_, PyAny>) {
        if let Some(inner) = self.operation.upgrade() {
            CueOperation { inner }.finish_from_task(py, task);
        }
    }
}

#[pyclass]
struct CueSetupFallbackCallback {
    operation: Weak<CueOperationInner>,
}

#[pymethods]
impl CueSetupFallbackCallback {
    fn __call__(self_: Py<Self>, py: Python<'_>) {
        let operation = {
            let callback = self_.bind(py).borrow();
            callback
                .operation
                .upgrade()
                .map(|inner| CueOperation { inner })
        };
        let Some(operation) = operation else {
            return;
        };
        let task = {
            let state = lock(&operation.inner.state);
            match (&state.phase, &state.intent) {
                (
                    OperationPhase::CancellingRunning { task },
                    Some(CancellationIntent::AttachmentFailure { .. }),
                ) => Some(task.clone_ref(py)),
                _ => None,
            }
        };
        let Some(task) = task else {
            return;
        };
        let done = py
            .import("asyncio")
            .and_then(|module| module.getattr("Future"))
            .and_then(|future| future.call_method1("done", (task.bind(py),)))
            .and_then(|done| done.is_truthy());
        match done {
            Ok(true) => {
                operation.finish_with_result(py, trusted_task_result(py, task.bind(py)));
            }
            Ok(false) => {
                if let Some(binding) = operation.inner.binding.upgrade() {
                    let _ = binding
                        .event_loop(py)
                        .bind(py)
                        .call_method1("call_soon", (self_,));
                }
            }
            Err(_) => {}
        }
    }
}

pub(crate) struct Running {
    pub(crate) operation: CueOperation,
}

impl Running {
    pub(crate) fn new(operation: CueOperation) -> Self {
        Self { operation }
    }
}

#[derive(Default)]
pub(crate) struct Mailbox {
    pub(crate) queue: VecDeque<CueOperation>,
    pub(crate) running: Option<Running>,
}

impl Mailbox {
    pub(crate) fn enqueue(&mut self, operation: CueOperation) {
        self.queue.push_back(operation);
    }

    pub(crate) fn claim_next_if_idle(&mut self) -> Option<CueOperation> {
        if self.running.is_some() {
            return None;
        }
        let operation = self.queue.pop_front()?;
        self.running = Some(Running::new(operation.clone()));
        Some(operation)
    }

    pub(crate) fn complete_running(
        &mut self,
        operation: &CueOperation,
    ) -> Option<(Running, Option<CueOperation>)> {
        if !self
            .running
            .as_ref()
            .is_some_and(|running| running.operation.ptr_eq(operation))
        {
            return None;
        }
        let retired = self
            .running
            .take()
            .expect("the exact running operation must remain present");
        let next = self.queue.pop_front();
        if let Some(operation) = &next {
            self.running = Some(Running::new(operation.clone()));
        }
        Some((retired, next))
    }

    pub(crate) fn remove_queued(&mut self, operation: &CueOperation) -> Option<CueOperation> {
        let index = self
            .queue
            .iter()
            .position(|queued| queued.ptr_eq(operation))?;
        self.queue.remove(index)
    }

    pub(crate) fn terminal_transition(
        &mut self,
        operation: &CueOperation,
    ) -> MailboxTerminalTransition {
        if let Some(removed) = self.remove_queued(operation) {
            return MailboxTerminalTransition {
                removed: Some(removed),
                ..MailboxTerminalTransition::default()
            };
        }
        let Some((retired, next)) = self.complete_running(operation) else {
            return MailboxTerminalTransition::default();
        };
        MailboxTerminalTransition {
            removed: None,
            retired: Some(retired),
            next,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, Weak};

    use pyo3::prelude::*;
    use pyo3::types::{PyCFunction, PyCapsule, PyDict, PyModule, PyString, PyTuple};

    use crate::orchestration::actor::{Actor, ActorCapability, ActorIdentity, enter_actor_permit};
    use crate::orchestration::actor_registry::{NameKey, ProductionState};
    use crate::orchestration::cue::Cue;
    use crate::orchestration::scene_context::{CuedScope, SceneScope};

    use super::{
        CancellationIntent, CueCaptureMode, CueOperation, CueOperationInner, Mailbox,
        OperationPhase, OperationState, Running, TerminalOutcome, VALIDATION_CALLS, lock,
        validate_cued_result,
    };

    #[test]
    fn operation_phase_outcome_and_intent_variants_are_exact() {
        fn inner_shape(inner: CueOperationInner) {
            let CueOperationInner {
                index,
                cancel_requests,
                scene,
                actor,
                binding,
                capture_mode,
                cued,
                state,
            } = inner;
            let _: usize = index;
            let _: AtomicUsize = cancel_requests;
            let _: Weak<SceneScope> = scene;
            let _: Weak<crate::orchestration::actor::ActorCapability> = actor;
            let _: Weak<crate::orchestration::scene_context::RunBinding> = binding;
            let _: CueCaptureMode = capture_mode;
            let _: Arc<CuedScope> = cued;
            let _: Mutex<OperationState> = state;
        }

        fn state_shape(state: OperationState) {
            let OperationState {
                phase,
                intent,
                cue,
                signal,
                forced_callback_attachment_error,
            } = state;
            let _: OperationPhase = phase;
            let _: Option<CancellationIntent> = intent;
            let _: Option<Py<Cue>> = cue;
            let _: Option<Py<PyAny>> = signal;
            let _: Option<Py<PyAny>> = forced_callback_attachment_error;
        }

        fn phase_tag(phase: &OperationPhase) -> u8 {
            match phase {
                OperationPhase::Queued => 0,
                OperationPhase::Running { task } => {
                    let _: &Py<PyAny> = task;
                    1
                }
                OperationPhase::CancellingRunning { task } => {
                    let _: &Py<PyAny> = task;
                    2
                }
                OperationPhase::Terminal(_) => 3,
            }
        }

        fn outcome_tag(outcome: &TerminalOutcome) -> u8 {
            match outcome {
                TerminalOutcome::Success(result) => {
                    let _: &Py<PyTuple> = result;
                    0
                }
                TerminalOutcome::Failure(error) => {
                    let _: &Py<PyAny> = error;
                    1
                }
                TerminalOutcome::Cancelled(error) => {
                    let _: &Py<PyAny> = error;
                    2
                }
                TerminalOutcome::CleanupFailure(error) => {
                    let _: &Py<PyAny> = error;
                    3
                }
            }
        }

        fn intent_tag(intent: &CancellationIntent) -> u8 {
            match intent {
                CancellationIntent::Requested { dispatch_error } => {
                    let _: &Option<Py<PyAny>> = dispatch_error;
                    0
                }
                CancellationIntent::AttachmentFailure { error } => {
                    let _: &Py<PyAny> = error;
                    1
                }
            }
        }

        let _: fn(CueOperationInner) = inner_shape;
        let _: fn(OperationState) = state_shape;
        let _: fn(&OperationPhase) -> u8 = phase_tag;
        let _: fn(&TerminalOutcome) -> u8 = outcome_tag;
        let _: fn(&CancellationIntent) -> u8 = intent_tag;
    }

    #[test]
    fn mailbox_has_only_fifo_queue_and_running_slot() {
        let _python_test_guard = crate::initialize_python_for_test();
        let mut mailbox = Mailbox::default();
        let _: () = mailbox.enqueue(CueOperation::new_for_test(0));
        let _: () = mailbox.enqueue(CueOperation::new_for_test(1));

        let Mailbox { queue, running } = mailbox;
        let queue: VecDeque<CueOperation> = queue;
        let indexes: Vec<_> = queue
            .into_iter()
            .map(|operation| operation.index_for_test())
            .collect();
        assert_eq!(indexes, [0, 1]);
        assert!(running.is_none());

        let _: Option<Running> = running;
    }

    #[test]
    fn mailbox_transitions_are_exact_unbounded_and_fifo() {
        let _python_test_guard = crate::initialize_python_for_test();
        let mut mailbox = Mailbox::default();
        let first = CueOperation::new_for_test(0);
        mailbox.enqueue(first.clone());
        let claimed = mailbox
            .claim_next_if_idle()
            .expect("an idle mailbox must claim its first operation");
        assert!(claimed.ptr_eq(&first));
        assert!(mailbox.claim_next_if_idle().is_none());

        let second = CueOperation::new_for_test(1);
        let unrelated = CueOperation::new_for_test(usize::MAX);
        mailbox.enqueue(second.clone());
        assert!(mailbox.complete_running(&second).is_none());
        assert!(mailbox.complete_running(&unrelated).is_none());
        assert!(
            mailbox
                .running
                .as_ref()
                .is_some_and(|running| running.operation.ptr_eq(&first))
        );
        assert_eq!(mailbox.queue.len(), 1);
        assert!(mailbox.queue[0].ptr_eq(&second));

        for index in 2..=10_000 {
            let _: () = mailbox.enqueue(CueOperation::new_for_test(index));
        }
        assert_eq!(mailbox.queue.len(), 10_000);
        assert!(mailbox.claim_next_if_idle().is_none());

        let mut current = claimed;
        let mut drained = Vec::with_capacity(10_001);
        loop {
            let (retired, next) = mailbox
                .complete_running(&current)
                .expect("the exact running operation must complete");
            drained.push(retired.operation.index_for_test());
            drop(retired);
            match next {
                Some(operation) => current = operation,
                None => break,
            }
        }

        assert_eq!(drained, (0..=10_000).collect::<Vec<_>>());
        assert!(mailbox.queue.is_empty());
        assert!(mailbox.running.is_none());
    }

    #[test]
    fn queued_cancellation_removes_its_real_actor_and_scene_links_exactly() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let actor_type = py.get_type::<Actor>();
            let name = PyString::new(py, "queued-removal");
            let production = py.None().into_bound(py);
            let identity = Arc::new(ActorIdentity);
            let (_, permit) =
                enter_actor_permit(&actor_type, &name, &production, Arc::clone(&identity));
            let actor = actor_type.call0()?.cast_into::<Actor>()?.unbind();
            drop(permit);
            let production_state = Arc::new(ProductionState::new());
            let capability = Arc::new(ActorCapability::new(
                actor,
                &name,
                NameKey::from_python(&name)?,
                identity,
                Arc::downgrade(&production_state),
            ));
            let scene = SceneScope::zero_for_test(py, "scene-queued-removal")?;
            let first = CueOperation::new_for_actor_for_test(0, &capability, &scene);
            let second = CueOperation::new_for_actor_for_test(1, &capability, &scene);
            let third = CueOperation::new_for_actor_for_test(2, &capability, &scene);
            capability.install_running_for_test(first.clone());

            scene
                .begin_admission()
                .expect("second operation must register")
                .prepare(py)?
                .commit(second.clone())?;
            scene
                .begin_admission()
                .expect("third operation must register")
                .prepare(py)?
                .commit(third.clone())?;
            assert_eq!(scene.operation_count_for_test(), 2);
            assert!(second.request_cancel());
            assert!(second.is_terminal());
            assert_eq!(scene.operation_count_for_test(), 1);
            assert!(capability.queue_only_contains_and_running_exact_for_test(&first, &third));

            assert!(third.request_cancel());
            assert_eq!(scene.operation_count_for_test(), 0);
            first.finish_cancel();
            assert!(capability.retire_running_for_test(&first));
            Ok::<_, PyErr>(())
        })
        .expect("queued cancellation must unlink the exact Actor and Scene entries");
    }

    #[test]
    fn result_cancel_linearization_is_exact_once_and_skips_cancelled_validation() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let event_loop = py.import("asyncio")?.call_method0("new_event_loop")?;
            let probe_type = PyModule::from_code(
                py,
                c"class TaskProbe:\n    def __init__(self):\n        self.cancel_calls = 0\n    def cancel(self):\n        self.cancel_calls += 1\n        return True\n",
                c"operation_state_test.py",
                c"operation_state_test",
            )?
            .getattr("TaskProbe")?;
            let dispatch_probe_type = PyModule::from_code(
                py,
                c"class DispatchProbe:\n    def __init__(self, error):\n        self.error = error\n        self.cancel_calls = 0\n    def cancel(self):\n        self.cancel_calls += 1\n        raise self.error\n",
                c"operation_dispatch_error_test.py",
                c"operation_dispatch_error_test",
            )?
            .getattr("DispatchProbe")?;

            let completion_first = CueOperation::new_for_test(0);
            let completion_signal = event_loop.call_method0("create_future")?.unbind();
            {
                let mut state = lock(&completion_first.inner.state);
                state.phase = OperationPhase::Running { task: py.None() };
                state.signal = Some(completion_signal.clone_ref(py));
            }
            VALIDATION_CALLS.store(0, Ordering::SeqCst);
            let tuple = PyTuple::empty(py);
            assert!(completion_first.finish_with_result(
                py,
                Ok(tuple.clone().unbind().into_any()),
            ));
            assert!(!completion_first.request_cancel());
            assert!(!completion_first.finish_with_result(py, Ok(py.None())));
            assert_eq!(VALIDATION_CALLS.load(Ordering::SeqCst), 1);
            assert!(completion_signal
                .bind(py)
                .call_method0("done")?
                .is_truthy()?);
            assert!(completion_signal.bind(py).call_method0("result")?.is_none());
            {
                let state = lock(&completion_first.inner.state);
                match &state.phase {
                    OperationPhase::Terminal(TerminalOutcome::Success(result)) => {
                        assert!(result.bind(py).is(&tuple));
                    }
                    _ => panic!("completion-first must retain one Success outcome"),
                }
                assert!(state.intent.is_none());
            }

            for (index, invalid) in [
                py.None(),
                PyTuple::new(py, [py.None().bind(py)])?.unbind().into_any(),
            ]
            .into_iter()
            .enumerate()
            {
                let operation = CueOperation::new_for_test(index + 1);
                let task = probe_type.call0()?;
                let signal = event_loop.call_method0("create_future")?.unbind();
                {
                    let mut state = lock(&operation.inner.state);
                    state.phase = OperationPhase::Running {
                        task: task.clone().unbind(),
                    };
                    state.signal = Some(signal.clone_ref(py));
                }
                VALIDATION_CALLS.store(0, Ordering::SeqCst);
                assert!(operation.request_cancel());
                {
                    let state = lock(&operation.inner.state);
                    assert!(matches!(
                        &state.phase,
                        OperationPhase::CancellingRunning { .. }
                    ));
                    assert!(matches!(
                        &state.intent,
                        Some(CancellationIntent::Requested {
                            dispatch_error: None
                        })
                    ));
                }
                assert!(operation.finish_with_result(py, Ok(invalid)));
                assert!(!operation.finish_with_result(py, Ok(py.None())));
                assert!(!operation.request_cancel());
                assert_eq!(VALIDATION_CALLS.load(Ordering::SeqCst), 0);
                assert_eq!(task.getattr("cancel_calls")?.extract::<usize>()?, 1);
                assert!(signal.bind(py).call_method0("done")?.is_truthy()?);
                assert!(signal.bind(py).call_method0("result")?.is_none());
                let state = lock(&operation.inner.state);
                assert!(matches!(
                    &state.phase,
                    OperationPhase::Terminal(TerminalOutcome::Cancelled(_))
                ));
                assert!(state.intent.is_none());
            }

            let dispatch_errors = [
                pyo3::exceptions::PyRuntimeError::new_err("dispatch failed")
                    .into_value(py)
                    .into_any(),
                py.import("asyncio")?
                    .getattr("CancelledError")?
                    .call1(("dispatch cancelled",))?
                    .unbind(),
            ];
            for (index, dispatch_error) in dispatch_errors.into_iter().enumerate() {
                let operation = CueOperation::new_for_test(index + 10);
                let task = dispatch_probe_type.call1((dispatch_error.bind(py),))?;
                let signal = event_loop.call_method0("create_future")?.unbind();
                {
                    let mut state = lock(&operation.inner.state);
                    state.phase = OperationPhase::Running {
                        task: task.clone().unbind(),
                    };
                    state.signal = Some(signal.clone_ref(py));
                }
                VALIDATION_CALLS.store(0, Ordering::SeqCst);

                assert!(operation.request_cancel());
                {
                    let state = lock(&operation.inner.state);
                    match &state.intent {
                        Some(CancellationIntent::Requested {
                            dispatch_error: Some(actual),
                        }) => assert!(actual.bind(py).is(dispatch_error.bind(py))),
                        _ => panic!("Task.cancel failure must remain the requested intent"),
                    }
                }
                assert!(operation.finish_with_result(py, Ok(py.None())));
                assert_eq!(VALIDATION_CALLS.load(Ordering::SeqCst), 0);
                assert_eq!(task.getattr("cancel_calls")?.extract::<usize>()?, 1);
                assert!(signal.bind(py).call_method0("done")?.is_truthy()?);
                assert!(signal.bind(py).call_method0("result")?.is_none());
                let state = lock(&operation.inner.state);
                match &state.phase {
                    OperationPhase::Terminal(TerminalOutcome::CleanupFailure(actual)) => {
                        assert!(actual.bind(py).is(dispatch_error.bind(py)));
                    }
                    _ => panic!("dispatch error must win an invalid normal Task return"),
                }
                assert!(state.intent.is_none());
            }
            event_loop.call_method0("close")?;
            Ok::<_, PyErr>(())
        })
        .expect("result/cancel phase writes must choose one exact terminal outcome");
    }

    #[test]
    fn operation_cancel_is_idempotent() {
        let _python_test_guard = crate::initialize_python_for_test();
        let operation = CueOperation::new_for_test(0);
        assert!(operation.request_cancel());
        assert!(!operation.request_cancel());
        operation.finish_cancel();
        assert!(operation.is_terminal());
        assert_eq!(operation.cancel_requests_for_test(), 1);
    }

    #[test]
    fn step2_terminal_cue_is_released_after_operation_mutex_unlock() {
        struct LockProbe {
            mutex_address: usize,
            dropped: Arc<AtomicBool>,
            observed_unlocked: Arc<AtomicBool>,
        }

        impl Drop for LockProbe {
            fn drop(&mut self) {
                let mutex = unsafe { &*(self.mutex_address as *const Mutex<OperationState>) };
                self.dropped.store(true, Ordering::SeqCst);
                self.observed_unlocked
                    .store(mutex.try_lock().is_ok(), Ordering::SeqCst);
            }
        }

        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let operation = CueOperation::new_for_test(0);
            let dropped = Arc::new(AtomicBool::new(false));
            let observed_unlocked = Arc::new(AtomicBool::new(false));
            let mutex_address = std::ptr::from_ref(&operation.inner.state) as usize;
            let probe = PyCapsule::new_with_value(
                py,
                LockProbe {
                    mutex_address,
                    dropped: Arc::clone(&dropped),
                    observed_unlocked: Arc::clone(&observed_unlocked),
                },
                c"troupe.mailbox.terminal_drop_probe",
            )?;
            let instruction = PyDict::new(py);
            instruction.set_item("probe", &probe)?;
            let cue = Py::new(
                py,
                Cue::new_runtime(
                    PyString::new(py, "scene-terminal-drop-cue0").unbind(),
                    instruction.unbind().into_any(),
                    PyString::new(py, "scene-terminal-drop").unbind(),
                ),
            )?;
            {
                let mut state = lock(&operation.inner.state);
                state.phase = OperationPhase::Running { task: py.None() };
                state.cue = Some(cue);
            }
            drop(probe);

            operation.finish_with_result(py, Ok(PyTuple::empty(py).unbind().into_any()));

            assert!(dropped.load(Ordering::SeqCst));
            assert!(observed_unlocked.load(Ordering::SeqCst));
            Ok::<_, PyErr>(())
        })
        .expect("terminal Cue release must not run Python destructors under its mutex");
    }

    #[test]
    fn terminal_task_intent_and_outcome_last_owners_drop_after_all_locks() {
        struct LockSetProbe {
            operation_mutex_address: Option<usize>,
            actor: Weak<ActorCapability>,
            scene: Weak<SceneScope>,
            dropped: Arc<AtomicBool>,
            observed_unlocked: Arc<AtomicBool>,
        }

        impl Drop for LockSetProbe {
            fn drop(&mut self) {
                let operation_unlocked = match self.operation_mutex_address {
                    Some(address) => {
                        let mutex = unsafe { &*(address as *const Mutex<OperationState>) };
                        mutex.try_lock().is_ok()
                    }
                    None => true,
                };
                let mailbox_unlocked = self
                    .actor
                    .upgrade()
                    .is_some_and(|actor| actor.mailbox_lock_available_for_test());
                let scene_unlocked = self
                    .scene
                    .upgrade()
                    .is_some_and(|scene| scene.state_lock_available_for_test());
                self.dropped.store(true, Ordering::SeqCst);
                self.observed_unlocked.store(
                    operation_unlocked && mailbox_unlocked && scene_unlocked,
                    Ordering::SeqCst,
                );
            }
        }

        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let actor_type = py.get_type::<Actor>();
            let name = PyString::new(py, "terminal-owner-drop");
            let production = py.None().into_bound(py);
            let identity = Arc::new(ActorIdentity);
            let (_, permit) =
                enter_actor_permit(&actor_type, &name, &production, Arc::clone(&identity));
            let actor = actor_type.call0()?.cast_into::<Actor>()?.unbind();
            drop(permit);
            let production_state = Arc::new(ProductionState::new());
            let capability = Arc::new(ActorCapability::new(
                actor,
                &name,
                NameKey::from_python(&name)?,
                identity,
                Arc::downgrade(&production_state),
            ));
            let scene = SceneScope::zero_for_test(py, "scene-terminal-owner-drop")?;
            let placeholder = CueOperation::new_for_actor_for_test(0, &capability, &scene);
            capability.install_running_for_test(placeholder.clone());
            let operation = CueOperation::new_for_actor_for_test(1, &capability, &scene);
            scene
                .begin_admission()
                .expect("operation must register")
                .prepare(py)?
                .commit(operation.clone())?;
            assert!(!capability.retire_running_for_test(&placeholder));
            assert!(capability.queue_empty_and_running_exact_for_test(&operation));

            let task_dropped = Arc::new(AtomicBool::new(false));
            let task_unlocked = Arc::new(AtomicBool::new(false));
            let intent_dropped = Arc::new(AtomicBool::new(false));
            let intent_unlocked = Arc::new(AtomicBool::new(false));
            let outcome_dropped = Arc::new(AtomicBool::new(false));
            let outcome_unlocked = Arc::new(AtomicBool::new(false));
            let operation_mutex_address = std::ptr::from_ref(&operation.inner.state) as usize;
            let task_marker = PyCapsule::new_with_value(
                py,
                LockSetProbe {
                    operation_mutex_address: Some(operation_mutex_address),
                    actor: Arc::downgrade(&capability),
                    scene: Arc::downgrade(&scene),
                    dropped: Arc::clone(&task_dropped),
                    observed_unlocked: Arc::clone(&task_unlocked),
                },
                c"troupe.mailbox.task_owner_drop_probe",
            )?;
            let intent_marker = PyCapsule::new_with_value(
                py,
                LockSetProbe {
                    operation_mutex_address: Some(operation_mutex_address),
                    actor: Arc::downgrade(&capability),
                    scene: Arc::downgrade(&scene),
                    dropped: Arc::clone(&intent_dropped),
                    observed_unlocked: Arc::clone(&intent_unlocked),
                },
                c"troupe.mailbox.intent_owner_drop_probe",
            )?;
            let outcome_marker = PyCapsule::new_with_value(
                py,
                LockSetProbe {
                    operation_mutex_address: None,
                    actor: Arc::downgrade(&capability),
                    scene: Arc::downgrade(&scene),
                    dropped: Arc::clone(&outcome_dropped),
                    observed_unlocked: Arc::clone(&outcome_unlocked),
                },
                c"troupe.mailbox.outcome_owner_drop_probe",
            )?;
            let owner_type = PyModule::from_code(
                py,
                c"class Owner:\n    def __init__(self, marker):\n        self.marker = marker\n",
                c"terminal_owner_drop_test.py",
                c"terminal_owner_drop_test",
            )?
            .getattr("Owner")?;
            let task = owner_type.call1((&task_marker,))?.unbind();
            let intent_error = pyo3::exceptions::PyRuntimeError::new_err("dispatch error")
                .into_value(py)
                .into_any();
            intent_error.setattr(py, "marker", &intent_marker)?;
            let outcome_error = pyo3::exceptions::PyRuntimeError::new_err("cleanup error")
                .into_value(py)
                .into_any();
            outcome_error.setattr(py, "marker", &outcome_marker)?;
            let event_loop = py.import("asyncio")?.call_method0("new_event_loop")?;
            let signal = event_loop.call_method0("create_future")?.unbind();
            {
                let mut state = lock(&operation.inner.state);
                state.phase = OperationPhase::CancellingRunning { task };
                state.intent = Some(CancellationIntent::Requested {
                    dispatch_error: Some(intent_error),
                });
                state.signal = Some(signal.clone_ref(py));
            }
            drop((task_marker, intent_marker, outcome_marker));

            assert!(
                operation
                    .finish_with_result(py, Err(PyErr::from_value(outcome_error.into_bound(py))),)
            );
            assert!(task_dropped.load(Ordering::SeqCst));
            assert!(task_unlocked.load(Ordering::SeqCst));
            assert!(intent_dropped.load(Ordering::SeqCst));
            assert!(intent_unlocked.load(Ordering::SeqCst));
            assert!(!outcome_dropped.load(Ordering::SeqCst));
            assert_eq!(scene.operation_count_for_test(), 0);
            assert!(signal.bind(py).call_method0("done")?.is_truthy()?);

            drop(operation);
            assert!(outcome_dropped.load(Ordering::SeqCst));
            assert!(outcome_unlocked.load(Ordering::SeqCst));
            event_loop.call_method0("close")?;
            Ok::<_, PyErr>(())
        })
        .expect("terminal Python owners must be dropped only after every runtime lock is released");
    }

    #[test]
    fn operation_task_cancel_runs_unlocked_and_terminal_signal_is_non_error() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let event_loop = py.import("asyncio")?.call_method0("new_event_loop")?;
            let operation = CueOperation::new_for_test(0);
            let probe_type = PyModule::from_code(
                py,
                c"class TaskProbe:\n    def __init__(self, callback):\n        self.callback = callback\n        self.cancel_calls = 0\n    def cancel(self):\n        self.cancel_calls += 1\n        self.callback()\n        return True\n",
                c"operation_callback_lock_test.py",
                c"operation_callback_lock_test",
            )?
            .getattr("TaskProbe")?;

            let task_unlocked = Arc::new(AtomicBool::new(false));
            let mutex_address = std::ptr::from_ref(&operation.inner.state) as usize;
            let callback_unlocked = Arc::clone(&task_unlocked);
            let callback = PyCFunction::new_closure(
                py,
                None,
                None,
                move |_args: &Bound<'_, PyTuple>,
                      _kwargs: Option<&Bound<'_, PyDict>>|
                      -> PyResult<()> {
                    let mutex = unsafe { &*(mutex_address as *const Mutex<OperationState>) };
                    callback_unlocked.store(mutex.try_lock().is_ok(), Ordering::SeqCst);
                    Ok(())
                },
            )?;
            let task = probe_type.call1((callback,))?.unbind();
            let signal = event_loop.call_method0("create_future")?.unbind();
            {
                let mut state = lock(&operation.inner.state);
                state.phase = OperationPhase::Running {
                    task: task.clone_ref(py),
                };
                state.signal = Some(signal.clone_ref(py));
            }

            assert!(operation.request_cancel());
            operation.finish_with_result(py, Ok(py.None()));

            assert!(task_unlocked.load(Ordering::SeqCst));
            assert_eq!(task.bind(py).getattr("cancel_calls")?.extract::<usize>()?, 1);
            assert!(signal.bind(py).call_method0("done")?.is_truthy()?);
            assert!(signal.bind(py).call_method0("result")?.is_none());
            event_loop.call_method0("close")?;
            Ok::<_, PyErr>(())
        })
        .expect("operation Task cancellation and signal completion must run after state unlock");
    }

    #[test]
    fn cued_result_validation_preserves_outer_tuple_and_terminalizes_once() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let result = PyTuple::empty(py);
            let validated = validate_cued_result(result.as_any())?;
            assert!(validated.bind(py).is(&result));

            let outer_error =
                validate_cued_result(py.None().bind(py)).expect_err("a non-tuple result must fail");
            assert_eq!(
                outer_error.to_string(),
                "TypeError: Actor.cued() must return a tuple of Effect instances"
            );
            let bad_item = PyTuple::new(py, [py.None().bind(py)])?;
            let item_error = validate_cued_result(bad_item.as_any())
                .expect_err("a non-Effect tuple item must fail");
            assert_eq!(
                item_error.to_string(),
                "TypeError: Actor.cued() return item at index 0 is not an Effect"
            );

            let event_loop = py.import("asyncio")?.call_method0("new_event_loop")?;
            let signal = event_loop.call_method0("create_future")?.unbind();
            let operation = CueOperation::new_for_test(0);
            operation.force_running_for_test(py.None());
            operation.set_signal_for_test(signal.clone_ref(py));
            assert!(operation.finish_with_result(py, Ok(result.clone().unbind().into_any()),));
            assert!(!operation.finish_with_result(py, Ok(py.None())));
            assert!(operation.is_terminal());
            assert!(signal.bind(py).call_method0("result")?.is_none());
            assert!(
                operation
                    .terminal_success_for_test(py)
                    .is_some_and(|actual| actual.bind(py).is(&result))
            );

            let invalid_signal = event_loop.call_method0("create_future")?.unbind();
            let invalid_operation = CueOperation::new_for_test(1);
            invalid_operation.force_running_for_test(py.None());
            invalid_operation.set_signal_for_test(invalid_signal.clone_ref(py));
            assert!(invalid_operation.finish_with_result(py, Ok(py.None())));
            assert!(invalid_signal.bind(py).call_method0("result")?.is_none());
            let error = invalid_operation
                .terminal_error_for_test(py)
                .expect("invalid result must store a Failure outcome")
                .bind(py)
                .to_string();
            assert_eq!(
                error,
                "Actor.cued() must return a tuple of Effect instances"
            );
            event_loop.call_method0("close")?;
            Ok::<_, PyErr>(())
        })
        .expect("Cue result validation and terminal commit must be linearized once");
    }
}
