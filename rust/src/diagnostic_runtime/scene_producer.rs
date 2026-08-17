use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, OnceLock},
};

use pyo3::{PyErr, Python, prelude::*, types::PyStringMethods};
use troupe_diagnostics_core::{
    detail::{EmptyDetail, SpanStartDetail},
    event::DiagnosticScope,
    id::RunLocalId,
    kinds::SpanOutcome,
    scalar::SchemaU64,
};

use crate::{
    diagnostic_runtime::{
        load_producer::DiagnosticRunContext,
        runtime_producer::{self, RuntimeLifecycleProducer},
    },
    orchestration::{
        python_task::TaskLineage,
        scene_context::{RunBinding, SceneScope},
    },
};

const SCENE_CANCELLED: &str = "scene-lifecycle-cancelled";
const SCENE_FAILED: &str = "scene-lifecycle-failed";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SceneHook {
    BindingCreated,
    SceneCreated,
    TaskRegistered,
    SceneFinished,
}

#[derive(Clone)]
pub(crate) struct SceneLineageSnapshot {
    context: DiagnosticRunContext,
    scope: DiagnosticScope,
    run_span_id: SchemaU64,
    scene_span_id: SchemaU64,
}

impl SceneLineageSnapshot {
    pub(crate) fn context(&self) -> DiagnosticRunContext {
        self.context.clone()
    }

    pub(crate) const fn scope(&self) -> &DiagnosticScope {
        &self.scope
    }

    pub(crate) const fn run_span_id(&self) -> SchemaU64 {
        self.run_span_id
    }

    pub(crate) const fn scene_span_id(&self) -> SchemaU64 {
        self.scene_span_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SceneTerminal {
    outcome: SpanOutcome,
    error_code: Option<&'static str>,
}

struct SceneLifecycleState {
    task_terminal: Option<SceneTerminal>,
    cleanup_finished: bool,
    finish_attempted: bool,
}

struct SceneLifecycleProducer {
    runtime: Arc<RuntimeLifecycleProducer>,
    snapshot: SceneLineageSnapshot,
    state: Mutex<SceneLifecycleState>,
}

impl SceneLifecycleProducer {
    fn start(runtime: Arc<RuntimeLifecycleProducer>, scene_id: RunLocalId) -> Option<Arc<Self>> {
        if runtime.failure().is_some() {
            return None;
        }
        let context = runtime.context();
        let run_span_id = runtime.run_span_id();
        let scope = DiagnosticScope::new(Some(scene_id), None, None, None, None, None, None);
        let scene_span_id = match context.start_span(
            scope.clone(),
            SpanStartDetail::SceneLifecycle(EmptyDetail::new()),
            Some(run_span_id),
        ) {
            Ok(span_id) => span_id,
            Err(error) => {
                runtime.latch_diagnostic_failure(error);
                return None;
            }
        };
        Some(Arc::new(Self {
            runtime,
            snapshot: SceneLineageSnapshot {
                context,
                scope,
                run_span_id,
                scene_span_id,
            },
            state: Mutex::new(SceneLifecycleState {
                task_terminal: None,
                cleanup_finished: false,
                finish_attempted: false,
            }),
        }))
    }

    fn snapshot(&self) -> Option<SceneLineageSnapshot> {
        let state = lock(&self.state);
        (!state.finish_attempted).then(|| self.snapshot.clone())
    }

    fn task_finished(&self, error: Option<&PyErr>) -> bool {
        let mut state = lock(&self.state);
        if state.task_terminal.is_some() || state.finish_attempted {
            self.runtime
                .latch_state_failure("scene.task-terminal-transition-invalid");
            return state.finish_attempted;
        }
        state.task_terminal = Some(scene_terminal(error));
        self.try_finish(&mut state)
    }

    fn cleanup_finished(&self) -> bool {
        let mut state = lock(&self.state);
        if state.cleanup_finished || state.finish_attempted {
            self.runtime
                .latch_state_failure("scene.cleanup-terminal-transition-invalid");
            return state.finish_attempted;
        }
        state.cleanup_finished = true;
        self.try_finish(&mut state)
    }

    fn try_finish(&self, state: &mut SceneLifecycleState) -> bool {
        let Some(terminal) = state.task_terminal.as_ref() else {
            return false;
        };
        if !state.cleanup_finished {
            return false;
        }
        state.finish_attempted = true;
        if self.runtime.failure().is_some() {
            return true;
        }
        if let Err(error) = self.snapshot.context.finish_span(
            self.snapshot.scope.clone(),
            self.snapshot.scene_span_id,
            terminal.outcome,
            terminal.error_code.map(str::to_owned),
        ) {
            self.runtime.latch_diagnostic_failure(error);
        }
        true
    }
}

fn scenes() -> &'static Mutex<HashMap<usize, Arc<SceneLifecycleProducer>>> {
    static SCENES: OnceLock<Mutex<HashMap<usize, Arc<SceneLifecycleProducer>>>> = OnceLock::new();
    SCENES.get_or_init(|| Mutex::new(HashMap::new()))
}

#[inline]
pub(crate) fn binding_created(_binding: &RunBinding) {}

#[inline]
pub(crate) fn observe_scene(scene: &SceneScope, hook: SceneHook) {
    match hook {
        SceneHook::SceneCreated => scene_created(scene),
        SceneHook::SceneFinished => scene_finished(scene),
        SceneHook::BindingCreated | SceneHook::TaskRegistered => {}
    }
}

#[inline]
pub(crate) fn observe_task(lineage: &TaskLineage, hook: SceneHook) {
    if hook != SceneHook::TaskRegistered {
        return;
    }
    let Some(scene) = lineage.scene() else {
        return;
    };
    let Some(binding) = scene.binding() else {
        return;
    };
    let Some(runtime) = runtime_producer::producer_for_binding(&binding) else {
        return;
    };
    if !lineage.is_active() || lineage_snapshot(lineage).is_none() {
        runtime.latch_state_failure("scene.registered-task-lineage-invalid");
    }
}

#[inline]
pub(crate) fn task_finished(scene: &SceneScope, error: Option<&PyErr>) {
    let producer = lock(scenes()).get(&address(scene)).cloned();
    let Some(producer) = producer else {
        if let Some(runtime) = runtime_for_scene(scene) {
            runtime.latch_state_failure("scene.task-terminal-without-lifecycle");
        }
        return;
    };
    if producer.task_finished(error) {
        remove_scene(scene, &producer);
    }
}

pub(crate) fn lineage_snapshot(lineage: &TaskLineage) -> Option<SceneLineageSnapshot> {
    if !lineage.is_active() {
        return None;
    }
    lineage.scene().and_then(|scene| snapshot_for_scene(&scene))
}

pub(crate) fn current_scene_snapshot(
    py: Python<'_>,
    binding: &RunBinding,
) -> PyResult<Option<SceneLineageSnapshot>> {
    Ok(binding
        .current_lineage(py)?
        .as_ref()
        .and_then(lineage_snapshot))
}

pub(crate) fn snapshot_for_scene(scene: &SceneScope) -> Option<SceneLineageSnapshot> {
    lock(scenes())
        .get(&address(scene))
        .and_then(|producer| producer.snapshot())
}

fn scene_created(scene: &SceneScope) {
    let Some(runtime) = runtime_for_scene(scene) else {
        return;
    };
    let scene_id = match scene_identifier(scene) {
        Ok(scene_id) => scene_id,
        Err(code) => {
            runtime.latch_state_failure(code);
            return;
        }
    };
    let key = address(scene);
    let mut active = lock(scenes());
    if active.contains_key(&key) {
        runtime.latch_state_failure("scene.lifecycle-already-started");
        return;
    }
    if let Some(producer) = SceneLifecycleProducer::start(runtime, scene_id) {
        active.insert(key, producer);
    }
}

fn scene_finished(scene: &SceneScope) {
    let producer = lock(scenes()).get(&address(scene)).cloned();
    let Some(producer) = producer else {
        if let Some(runtime) = runtime_for_scene(scene) {
            runtime.latch_state_failure("scene.cleanup-terminal-without-lifecycle");
        }
        return;
    };
    if producer.cleanup_finished() {
        remove_scene(scene, &producer);
    }
}

fn remove_scene(scene: &SceneScope, expected: &Arc<SceneLifecycleProducer>) {
    let key = address(scene);
    let mut active = lock(scenes());
    if active
        .get(&key)
        .is_some_and(|producer| Arc::ptr_eq(producer, expected))
    {
        active.remove(&key);
    }
}

fn runtime_for_scene(scene: &SceneScope) -> Option<Arc<RuntimeLifecycleProducer>> {
    scene
        .binding()
        .and_then(|binding| runtime_producer::producer_for_binding(&binding))
}

fn scene_identifier(scene: &SceneScope) -> Result<RunLocalId, &'static str> {
    let name = Python::attach(|py| {
        scene
            .name(py)
            .bind(py)
            .to_str()
            .map(str::to_owned)
            .map_err(|_| "scene.name-not-utf8")
    })?;
    RunLocalId::parse(&name).map_err(|_| "scene.invalid-run-local-id")
}

fn scene_terminal(error: Option<&PyErr>) -> SceneTerminal {
    match error {
        None => SceneTerminal {
            outcome: SpanOutcome::Completed,
            error_code: None,
        },
        Some(error) if is_cancelled_error(error) => SceneTerminal {
            outcome: SpanOutcome::Cancelled,
            error_code: Some(SCENE_CANCELLED),
        },
        Some(_) => SceneTerminal {
            outcome: SpanOutcome::Failed,
            error_code: Some(SCENE_FAILED),
        },
    }
}

fn is_cancelled_error(error: &PyErr) -> bool {
    Python::attach(|py| {
        py.import("asyncio")
            .and_then(|asyncio| asyncio.getattr("CancelledError"))
            .is_ok_and(|cancelled| error.is_instance(py, &cancelled))
    })
}

fn address<T>(value: &T) -> usize {
    std::ptr::from_ref(value).addr()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::{
        fmt,
        sync::{Arc, Mutex},
        time::Instant,
    };

    use pyo3::{exceptions::PyRuntimeError, types::PyAnyMethods};
    use troupe_diagnostics_core::{
        event::DiagnosticEvent,
        hub::{
            AcceptedDiagnosticEvent, AdmissionReservation, AdmissionReserver, AdmissionSize,
            DeliveryFailure, LiveEventNotifier, MandatoryDurableReserver, ProductionDiagnosticHub,
        },
        id::CanonicalUuid,
        kinds::{SpanKind, SpanOutcome},
        time::RunClock,
    };
    use uuid::Uuid;

    use crate::{
        diagnostic_runtime::{
            load_producer::DiagnosticRunContext,
            runtime_producer::{self, RuntimeHook},
        },
        orchestration::{
            python_task::TaskLineage,
            runtime::{RunPermit, RuntimeCore},
            scene_context::{RunBinding, SceneScope},
        },
    };

    use super::*;

    #[derive(Clone, Default)]
    struct EventLog(Arc<Mutex<Vec<AcceptedDiagnosticEvent>>>);

    impl EventLog {
        fn events(&self) -> Vec<AcceptedDiagnosticEvent> {
            lock(&self.0).clone()
        }
    }

    struct RecordingReservation(EventLog);

    impl AdmissionReservation for RecordingReservation {
        fn commit(self, event: AcceptedDiagnosticEvent) {
            lock(&self.0.0).push(event);
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct InjectedAdmissionError;

    impl fmt::Display for InjectedAdmissionError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("injected Scene admission failure")
        }
    }

    impl std::error::Error for InjectedAdmissionError {}

    struct RecordingReserver {
        log: EventLog,
        attempts: usize,
        fail_on_attempt: Option<usize>,
    }

    impl AdmissionReserver for RecordingReserver {
        type Error = InjectedAdmissionError;
        type Reservation = RecordingReservation;

        fn try_reserve(&mut self, _size: AdmissionSize) -> Result<Self::Reservation, Self::Error> {
            self.attempts += 1;
            if self.fail_on_attempt == Some(self.attempts) {
                return Err(InjectedAdmissionError);
            }
            Ok(RecordingReservation(self.log.clone()))
        }
    }

    impl MandatoryDurableReserver for RecordingReserver {}

    struct IgnoreLive;

    impl LiveEventNotifier for IgnoreLive {
        fn notify(&mut self, _event: AcceptedDiagnosticEvent) -> Result<(), DeliveryFailure> {
            Ok(())
        }
    }

    struct Harness {
        core: Arc<RuntimeCore>,
        permit: Option<RunPermit>,
        binding: Arc<RunBinding>,
        runtime: Arc<RuntimeLifecycleProducer>,
        log: EventLog,
    }

    impl Harness {
        fn new(py: Python<'_>, fail_on_attempt: Option<usize>) -> Self {
            let log = EventLog::default();
            let hub = Arc::new(ProductionDiagnosticHub::production(
                CanonicalUuid::new(Uuid::new_v4()),
                RecordingReserver {
                    log: log.clone(),
                    attempts: 0,
                    fail_on_attempt,
                },
                Box::new(IgnoreLive),
            ));
            let context =
                DiagnosticRunContext::with_hub(hub, RunClock::from_origin(Instant::now()));
            let core = Arc::new(RuntimeCore::new());
            let permit = core.begin().expect("start test Runtime");
            let binding = Arc::new(RunBinding::new_for_test(py).expect("create test Run binding"));
            let runtime = runtime_producer::install(&core, &binding, context)
                .expect("install Runtime producer");
            runtime_producer::run_started(&core, &binding);
            runtime_producer::observe_binding(&binding, RuntimeHook::ProductionStartEntered, None);
            runtime_producer::observe_binding(&binding, RuntimeHook::ProductionStartReturned, None);
            Self {
                core,
                permit: Some(permit),
                binding,
                runtime,
                log,
            }
        }

        fn scene(&self, py: Python<'_>, id: &str) -> Arc<SceneScope> {
            let scene = SceneScope::zero_for_binding_for_test(py, id, &self.binding)
                .expect("create test Scene");
            observe_scene(&scene, SceneHook::SceneCreated);
            scene
        }

        fn finish(mut self) {
            self.core.request_shutdown();
            runtime_producer::observe_binding(
                &self.binding,
                RuntimeHook::ProductionStopEntered,
                None,
            );
            runtime_producer::observe_binding(
                &self.binding,
                RuntimeHook::ProductionStopReturned,
                None,
            );
            runtime_producer::observe_binding(
                &self.binding,
                RuntimeHook::RunLifecycleReturned,
                None,
            );
            drop(self.permit.take());
            assert!(self.runtime.failure().is_none());
        }
    }

    fn assert_scene_start(event: &AcceptedDiagnosticEvent, sequence: u64, id: &str, parent: u64) {
        assert_eq!(event.identity().sequence().get(), sequence);
        let DiagnosticEvent::SpanStarted(start) = event.event() else {
            panic!("expected Scene span start")
        };
        assert_eq!(start.span_kind(), SpanKind::SceneLifecycle);
        assert_eq!(start.parent_span_id().map(SchemaU64::get), Some(parent));
        assert_eq!(
            start.header().scope().scene_id().map(RunLocalId::as_str),
            Some(id)
        );
        assert!(start.header().scope().actor_id().is_none());
    }

    fn assert_scene_finish<'a>(
        events: &'a [AcceptedDiagnosticEvent],
        span_id: u64,
        id: &str,
        outcome: SpanOutcome,
        code: Option<&str>,
    ) -> &'a AcceptedDiagnosticEvent {
        let event = events
            .iter()
            .find(|event| {
                matches!(
                    event.event(),
                    DiagnosticEvent::SpanFinished(finish) if finish.span_id().get() == span_id
                )
            })
            .unwrap_or_else(|| panic!("missing Scene lifecycle finish for span {span_id}"));
        let DiagnosticEvent::SpanFinished(finish) = event.event() else {
            unreachable!("the Scene finish lookup only returns SpanFinished events")
        };
        assert_eq!(finish.span_id().get(), span_id);
        assert_eq!(finish.outcome(), outcome);
        assert_eq!(finish.error_code(), code);
        assert_eq!(
            finish.header().scope().scene_id().map(RunLocalId::as_str),
            Some(id)
        );
        event
    }

    fn scene_is_finished(events: &[AcceptedDiagnosticEvent], span_id: u64) -> bool {
        events.iter().any(|event| {
            matches!(
                event.event(),
                DiagnosticEvent::SpanFinished(finish) if finish.span_id().get() == span_id
            )
        })
    }

    #[test]
    fn scene_waits_for_task_and_cleanup_in_either_order() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let harness = Harness::new(py, None);
            let first = harness.scene(py, "scene-first");
            let second = harness.scene(py, "scene-second");
            let first_span_id = snapshot_for_scene(&first).unwrap().scene_span_id().get();
            let second_span_id = snapshot_for_scene(&second).unwrap().scene_span_id().get();

            task_finished(&first, None);
            assert!(!scene_is_finished(&harness.log.events(), first_span_id));
            second.close();
            assert!(!scene_is_finished(&harness.log.events(), second_span_id));
            first.close();
            assert!(scene_is_finished(&harness.log.events(), first_span_id));
            assert!(!scene_is_finished(&harness.log.events(), second_span_id));
            task_finished(&second, None);

            let events = harness.log.events();
            assert_scene_start(&events[3], 4, "scene-first", 1);
            assert_scene_start(&events[4], 5, "scene-second", 1);
            let first_finish = assert_scene_finish(
                &events,
                first_span_id,
                "scene-first",
                SpanOutcome::Completed,
                None,
            );
            let second_finish = assert_scene_finish(
                &events,
                second_span_id,
                "scene-second",
                SpanOutcome::Completed,
                None,
            );
            assert!(
                first_finish.identity().sequence().get()
                    < second_finish.identity().sequence().get()
            );
            assert!(events.windows(2).all(|pair| {
                pair[0].event().header().elapsed_ns().get()
                    <= pair[1].event().header().elapsed_ns().get()
            }));
            harness.finish();
        });
    }

    #[test]
    fn registered_lineage_gets_an_immutable_scope_and_expires_on_close() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let harness = Harness::new(py, None);
            let scene = harness.scene(py, "scene-lineage");
            let lineage = TaskLineage::from_scene(&scene);

            observe_task(&lineage, SceneHook::TaskRegistered);
            let snapshot = lineage_snapshot(&lineage).expect("registered active lineage has scope");
            assert_eq!(
                snapshot.scope().scene_id().map(RunLocalId::as_str),
                Some("scene-lineage")
            );
            assert_eq!(snapshot.run_span_id().get(), 1);
            assert_eq!(snapshot.scene_span_id().get(), 4);
            assert!(current_scene_snapshot(py, &harness.binding)?.is_none());

            let other_thread = Arc::clone(&harness.binding);
            let cross_thread_is_empty = py.detach(move || {
                std::thread::spawn(move || {
                    Python::attach(|py| {
                        current_scene_snapshot(py, &other_thread)
                            .expect("cross-thread lookup is finite")
                            .is_none()
                    })
                })
                .join()
                .expect("cross-thread lookup must not panic")
            });
            assert!(cross_thread_is_empty);

            scene.close();
            assert!(lineage_snapshot(&lineage).is_none());
            assert_eq!(
                snapshot.scope().scene_id().map(RunLocalId::as_str),
                Some("scene-lineage"),
                "an admitted snapshot remains immutable"
            );
            task_finished(&scene, None);
            assert!(snapshot_for_scene(&scene).is_none());
            harness.finish();
            Ok::<_, PyErr>(())
        })
        .expect("lineage scope test must complete");
    }

    #[test]
    fn failure_and_cancellation_are_normalized_without_payload_capture() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let harness = Harness::new(py, None);
            let failed = harness.scene(py, "scene-failed");
            let failed_span_id = snapshot_for_scene(&failed).unwrap().scene_span_id().get();
            let failure = PyRuntimeError::new_err("secret Scene failure payload");
            task_finished(&failed, Some(&failure));
            failed.close();

            let cancelled = harness.scene(py, "scene-cancelled");
            let cancelled_span_id = snapshot_for_scene(&cancelled)
                .unwrap()
                .scene_span_id()
                .get();
            let cancellation = py
                .import("asyncio")?
                .getattr("CancelledError")?
                .call0()
                .map(PyErr::from_value)?;
            cancelled.close();
            task_finished(&cancelled, Some(&cancellation));

            let events = harness.log.events();
            assert_scene_finish(
                &events,
                failed_span_id,
                "scene-failed",
                SpanOutcome::Failed,
                Some(SCENE_FAILED),
            );
            assert_scene_finish(
                &events,
                cancelled_span_id,
                "scene-cancelled",
                SpanOutcome::Cancelled,
                Some(SCENE_CANCELLED),
            );
            assert!(events.iter().all(|event| {
                !String::from_utf8_lossy(event.canonical_bytes())
                    .contains("secret Scene failure payload")
            }));
            harness.finish();
            Ok::<_, PyErr>(())
        })
        .expect("Scene terminal normalization test must complete");
    }

    #[test]
    fn scene_finish_admission_failure_latches_runtime_and_expires_lineage() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let harness = Harness::new(py, Some(5));
            let scene = harness.scene(py, "scene-admission-failure");
            let lineage = TaskLineage::from_scene(&scene);
            task_finished(&scene, None);
            scene.close();

            assert_eq!(harness.log.events().len(), 4);
            assert!(snapshot_for_scene(&scene).is_none());
            assert!(lineage_snapshot(&lineage).is_none());
            assert_eq!(
                harness
                    .runtime
                    .failure()
                    .as_ref()
                    .map(runtime_producer::RuntimeProducerError::code),
                Some("diagnostic.admission-failed")
            );
            drop(harness);
        });
    }

    #[test]
    fn inactive_scene_hooks_do_not_emit_or_allocate_state() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let binding = Arc::new(RunBinding::new_for_test(py)?);
            let scene = SceneScope::zero_for_binding_for_test(py, "scene-inactive", &binding)?;
            observe_scene(&scene, SceneHook::SceneCreated);
            observe_task(&TaskLineage::from_scene(&scene), SceneHook::TaskRegistered);
            task_finished(&scene, None);
            scene.close();
            assert!(snapshot_for_scene(&scene).is_none());
            Ok::<_, PyErr>(())
        })
        .expect("inactive Scene hooks remain no-op");
    }
}
