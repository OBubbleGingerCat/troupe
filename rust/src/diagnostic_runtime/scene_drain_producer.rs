use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, OnceLock},
};

use pyo3::{PyErr, Python, exceptions::PyStopIteration, types::PyAnyMethods};
use troupe_diagnostics_core::{
    detail::{EmptyDetail, SpanStartDetail},
    event::{CausalLink, DiagnosticEventKind},
    kinds::{CausalRelation, SpanOutcome},
    scalar::SchemaU64,
};

use crate::{
    diagnostic_runtime::{
        load_producer::DiagnosticProducerError,
        runtime_producer::{self, RuntimeLifecycleProducer},
        scene_producer::{self, SceneLineageSnapshot},
    },
    orchestration::scene_context::SceneScope,
};

const DRAIN_CANCELLED: &str = "scene-drain-cancelled";
const DRAIN_FAILED: &str = "scene-drain-failed";
const CLEANUP_FAILED: &str = "scene-cleanup-failed";
const LOST_DRIVER_TERMINAL: &str = "scene-driver-terminal-unobserved";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SceneDrainHook {
    AdmissionClosed,
    CancellationStarted,
    CleanupStarted,
    CleanupFinished,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SceneDriverExit {
    Returned,
    Closed,
    Cleared,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TerminalSpan {
    outcome: SpanOutcome,
    error_code: Option<&'static str>,
}

impl TerminalSpan {
    const fn completed() -> Self {
        Self {
            outcome: SpanOutcome::Completed,
            error_code: None,
        }
    }

    const fn cancelled(code: &'static str) -> Self {
        Self {
            outcome: SpanOutcome::Cancelled,
            error_code: Some(code),
        }
    }

    const fn failed(code: &'static str) -> Self {
        Self {
            outcome: SpanOutcome::Failed,
            error_code: Some(code),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DriverObservation {
    Pending,
    Observed {
        drain: TerminalSpan,
        cleanup: TerminalSpan,
    },
    Lost,
}

struct SceneDrainState {
    driver: DriverObservation,
    admission_closed: bool,
    cancellation_started: bool,
    cleanup_started: bool,
    drain_span_id: Option<SchemaU64>,
    gap_sequence: Option<SchemaU64>,
    cleanup_span_id: Option<SchemaU64>,
    finished: bool,
    producer_failed: bool,
}

struct SceneDrainProducer {
    runtime: Arc<RuntimeLifecycleProducer>,
    lineage: SceneLineageSnapshot,
    state: Mutex<SceneDrainState>,
}

impl SceneDrainProducer {
    fn new(runtime: Arc<RuntimeLifecycleProducer>, lineage: SceneLineageSnapshot) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            lineage,
            state: Mutex::new(SceneDrainState {
                driver: DriverObservation::Pending,
                admission_closed: false,
                cancellation_started: false,
                cleanup_started: false,
                drain_span_id: None,
                gap_sequence: None,
                cleanup_span_id: None,
                finished: false,
                producer_failed: false,
            }),
        })
    }

    fn driver_exited(&self, exit: SceneDriverExit, error: Option<&PyErr>) {
        let mut state = lock(&self.state);
        if state.producer_failed {
            return;
        }
        if state.driver != DriverObservation::Pending || state.admission_closed || state.finished {
            self.fail_state(&mut state, "scene.driver-terminal-transition-invalid");
            return;
        }
        state.driver = driver_observation(exit, error);
    }

    fn observe(&self, hook: SceneDrainHook) -> bool {
        match hook {
            SceneDrainHook::AdmissionClosed => self.admission_closed(),
            SceneDrainHook::CancellationStarted => self.cancellation_started(),
            SceneDrainHook::CleanupStarted => self.cleanup_started(),
            SceneDrainHook::CleanupFinished => self.cleanup_finished(),
        }
    }

    fn admission_closed(&self) -> bool {
        let mut state = lock(&self.state);
        if state.producer_failed {
            return false;
        }
        if state.admission_closed || state.finished {
            return self.fail_state(&mut state, "scene.drain-admission-transition-invalid");
        }
        state.admission_closed = true;
        if state.driver == DriverObservation::Pending {
            state.driver = DriverObservation::Lost;
        }

        match state.driver {
            DriverObservation::Observed { .. } => {
                match self.lineage.context().start_span_with_causes(
                    self.lineage.scope().clone(),
                    SpanStartDetail::SceneDrain(EmptyDetail::new()),
                    Some(self.lineage.scene_span_id()),
                    caused_by(self.lineage.scene_span_id(), CausalRelation::FollowsFrom),
                ) {
                    Ok(span_id) => state.drain_span_id = Some(span_id),
                    Err(error) => self.fail_diagnostic(&mut state, error),
                }
            }
            DriverObservation::Lost => {
                let scope = self.lineage.scope().clone();
                match self.lineage.context().emit_observation_gap(
                    scope.clone(),
                    "runtime".to_owned(),
                    Some("scene-drain".to_owned()),
                    LOST_DRIVER_TERMINAL.to_owned(),
                    Some(SchemaU64::new(1)),
                    None,
                    Some(DiagnosticEventKind::SpanFinished),
                    Some(scope),
                    caused_by(self.lineage.scene_span_id(), CausalRelation::FollowsFrom),
                ) {
                    Ok(sequence) => state.gap_sequence = Some(sequence),
                    Err(error) => self.fail_diagnostic(&mut state, error),
                }
            }
            DriverObservation::Pending => unreachable!("pending driver was normalized above"),
        }
        false
    }

    fn cleanup_started(&self) -> bool {
        let mut state = lock(&self.state);
        if state.producer_failed {
            return false;
        }
        if !state.admission_closed || state.cleanup_started || state.finished {
            return self.fail_state(&mut state, "scene.cleanup-start-transition-invalid");
        }
        state.cleanup_started = true;
        let causal_source = state
            .drain_span_id
            .or(state.gap_sequence)
            .unwrap_or(self.lineage.scene_span_id());
        match self.lineage.context().start_span_with_causes(
            self.lineage.scope().clone(),
            SpanStartDetail::SceneCleanup(EmptyDetail::new()),
            Some(self.lineage.scene_span_id()),
            caused_by(causal_source, CausalRelation::Handoff),
        ) {
            Ok(span_id) => state.cleanup_span_id = Some(span_id),
            Err(error) => self.fail_diagnostic(&mut state, error),
        }
        false
    }

    fn cancellation_started(&self) -> bool {
        let mut state = lock(&self.state);
        if state.producer_failed {
            return false;
        }
        if !state.admission_closed
            || !state.cleanup_started
            || state.cancellation_started
            || state.finished
        {
            return self.fail_state(&mut state, "scene.cancellation-transition-invalid");
        }
        state.cancellation_started = true;
        false
    }

    fn cleanup_finished(&self) -> bool {
        let mut state = lock(&self.state);
        if state.producer_failed {
            state.finished = true;
            return true;
        }
        if !state.admission_closed || !state.cleanup_started || state.finished {
            self.fail_state(&mut state, "scene.cleanup-terminal-transition-invalid");
            state.finished = true;
            return true;
        }
        state.finished = true;

        let cleanup_terminal = match state.driver {
            DriverObservation::Observed { cleanup, .. } => cleanup,
            DriverObservation::Lost => TerminalSpan::completed(),
            DriverObservation::Pending => unreachable!("admission normalized pending driver"),
        };
        let drain_finish_sequence = match (state.drain_span_id, state.driver) {
            (Some(span_id), DriverObservation::Observed { drain, .. }) => {
                let relation = if state.cancellation_started {
                    CausalRelation::Handoff
                } else {
                    CausalRelation::FollowsFrom
                };
                match self.lineage.context().finish_span_with_causes(
                    self.lineage.scope().clone(),
                    span_id,
                    drain.outcome,
                    drain.error_code.map(str::to_owned),
                    caused_by(span_id, relation),
                ) {
                    Ok(sequence) => Some(sequence),
                    Err(error) => {
                        self.fail_diagnostic(&mut state, error);
                        return true;
                    }
                }
            }
            (None, DriverObservation::Lost) => None,
            _ => {
                self.fail_state(&mut state, "scene.drain-terminal-state-invalid");
                return true;
            }
        };

        let Some(cleanup_span_id) = state.cleanup_span_id else {
            self.fail_state(&mut state, "scene.cleanup-terminal-without-start");
            return true;
        };
        let (source, relation) = drain_finish_sequence
            .map_or((cleanup_span_id, CausalRelation::FollowsFrom), |sequence| {
                (sequence, CausalRelation::Handoff)
            });
        if let Err(error) = self.lineage.context().finish_span_with_causes(
            self.lineage.scope().clone(),
            cleanup_span_id,
            cleanup_terminal.outcome,
            cleanup_terminal.error_code.map(str::to_owned),
            caused_by(source, relation),
        ) {
            self.fail_diagnostic(&mut state, error);
        }
        true
    }

    fn fail_diagnostic(&self, state: &mut SceneDrainState, error: DiagnosticProducerError) {
        state.producer_failed = true;
        self.runtime.latch_diagnostic_failure(error);
    }

    fn fail_state(&self, state: &mut SceneDrainState, code: &'static str) -> bool {
        state.producer_failed = true;
        self.runtime.latch_state_failure(code);
        false
    }
}

fn producers() -> &'static Mutex<HashMap<usize, Arc<SceneDrainProducer>>> {
    static PRODUCERS: OnceLock<Mutex<HashMap<usize, Arc<SceneDrainProducer>>>> = OnceLock::new();
    PRODUCERS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[inline]
pub(crate) fn observe(scene: &SceneScope, hook: SceneDrainHook) {
    let Some(producer) = producer_or_install(scene) else {
        return;
    };
    if producer.observe(hook) {
        remove(scene, &producer);
    }
}

#[inline]
pub(crate) fn driver_exited(scene: &SceneScope, exit: SceneDriverExit, error: Option<&PyErr>) {
    let Some(producer) = producer_or_install(scene) else {
        return;
    };
    producer.driver_exited(exit, error);
}

fn producer_or_install(scene: &SceneScope) -> Option<Arc<SceneDrainProducer>> {
    let key = address(scene);
    if let Some(producer) = lock(producers()).get(&key).cloned() {
        return Some(producer);
    }
    let binding = scene.binding()?;
    let runtime = runtime_producer::producer_for_binding(&binding)?;
    if runtime.failure().is_some() {
        return None;
    }
    let lineage = scene_producer::snapshot_for_scene(scene)?;
    let candidate = SceneDrainProducer::new(runtime, lineage);
    let mut active = lock(producers());
    Some(active.entry(key).or_insert_with(|| candidate).clone())
}

fn remove(scene: &SceneScope, expected: &Arc<SceneDrainProducer>) {
    let key = address(scene);
    let mut active = lock(producers());
    if active
        .get(&key)
        .is_some_and(|producer| Arc::ptr_eq(producer, expected))
    {
        active.remove(&key);
    }
}

fn driver_observation(exit: SceneDriverExit, error: Option<&PyErr>) -> DriverObservation {
    match exit {
        SceneDriverExit::Returned => DriverObservation::Observed {
            drain: returned_terminal(error),
            cleanup: TerminalSpan::completed(),
        },
        SceneDriverExit::Closed => DriverObservation::Observed {
            drain: TerminalSpan::cancelled(DRAIN_CANCELLED),
            cleanup: if error.is_some() {
                TerminalSpan::failed(CLEANUP_FAILED)
            } else {
                TerminalSpan::completed()
            },
        },
        SceneDriverExit::Cleared => DriverObservation::Lost,
    }
}

fn returned_terminal(error: Option<&PyErr>) -> TerminalSpan {
    match error {
        None => TerminalSpan::completed(),
        Some(error) if is_stop_iteration(error) => TerminalSpan::completed(),
        Some(error) if is_cancelled_error(error) => TerminalSpan::cancelled(DRAIN_CANCELLED),
        Some(_) => TerminalSpan::failed(DRAIN_FAILED),
    }
}

fn is_stop_iteration(error: &PyErr) -> bool {
    Python::attach(|py| error.is_instance_of::<PyStopIteration>(py))
}

fn is_cancelled_error(error: &PyErr) -> bool {
    Python::attach(|py| {
        py.import("asyncio")
            .and_then(|asyncio| asyncio.getattr("CancelledError"))
            .is_ok_and(|cancelled| error.is_instance(py, &cancelled))
    })
}

fn caused_by(source: SchemaU64, relation: CausalRelation) -> Vec<CausalLink> {
    vec![CausalLink::new(source, relation)]
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
    use std::{fmt, sync::Mutex, time::Instant};

    use pyo3::{
        exceptions::{PyRuntimeError, PySystemExit, PyTimeoutError},
        prelude::*,
    };
    use troupe_diagnostics_core::{
        event::{DiagnosticEvent, DiagnosticEventHeader, SpanFinished, SpanStarted},
        hub::{
            AcceptedDiagnosticEvent, AdmissionReservation, AdmissionReserver, AdmissionSize,
            DeliveryFailure, LiveEventNotifier, MandatoryDurableReserver, ProductionDiagnosticHub,
        },
        id::{CanonicalUuid, RunLocalId},
        kinds::SpanKind,
        time::RunClock,
    };
    use uuid::Uuid;

    use crate::{
        diagnostic_runtime::{
            load_producer::DiagnosticRunContext,
            runtime_producer::{self, RuntimeHook},
            scene_producer::{self, SceneHook},
        },
        orchestration::{
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
            formatter.write_str("injected Scene drain admission failure")
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
            scene_producer::observe_scene(&scene, SceneHook::SceneCreated);
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

    fn close_scene(
        scene: &SceneScope,
        exit: Option<(SceneDriverExit, Option<&PyErr>)>,
        cancellation_started: bool,
        task_error: Option<&PyErr>,
    ) {
        if let Some((exit, error)) = exit {
            driver_exited(scene, exit, error);
        }
        observe(scene, SceneDrainHook::AdmissionClosed);
        observe(scene, SceneDrainHook::CleanupStarted);
        if cancellation_started {
            observe(scene, SceneDrainHook::CancellationStarted);
        }
        observe(scene, SceneDrainHook::CleanupFinished);
        scene_producer::observe_scene(scene, SceneHook::SceneFinished);
        scene_producer::task_finished(scene, task_error);
    }

    fn scene_id(header: &DiagnosticEventHeader) -> Option<&str> {
        header.scope().scene_id().map(RunLocalId::as_str)
    }

    fn started<'a>(
        events: &'a [AcceptedDiagnosticEvent],
        id: &str,
        kind: SpanKind,
    ) -> &'a SpanStarted {
        events
            .iter()
            .find_map(|event| match event.event() {
                DiagnosticEvent::SpanStarted(start)
                    if scene_id(start.header()) == Some(id) && start.span_kind() == kind =>
                {
                    Some(start)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing {kind:?} start for {id}"))
    }

    fn finished<'a>(
        events: &'a [AcceptedDiagnosticEvent],
        id: &str,
        span_id: SchemaU64,
    ) -> &'a SpanFinished {
        events
            .iter()
            .find_map(|event| match event.event() {
                DiagnosticEvent::SpanFinished(finish)
                    if scene_id(finish.header()) == Some(id) && finish.span_id() == span_id =>
                {
                    Some(finish)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing finish for span {} in {id}", span_id.get()))
    }

    fn assert_cause(header: &DiagnosticEventHeader, source: SchemaU64, relation: CausalRelation) {
        assert_eq!(header.caused_by().len(), 1);
        assert_eq!(header.caused_by()[0].source_sequence(), source);
        assert_eq!(header.caused_by()[0].relation(), relation);
    }

    fn assert_terminal(
        events: &[AcceptedDiagnosticEvent],
        id: &str,
        kind: SpanKind,
        outcome: SpanOutcome,
        error_code: Option<&str>,
    ) -> SchemaU64 {
        let start = started(events, id, kind);
        let span_id = start.header().sequence();
        let finish = finished(events, id, span_id);
        assert_eq!(finish.outcome(), outcome);
        assert_eq!(finish.error_code(), error_code);
        span_id
    }

    #[test]
    fn normal_drain_and_cancellation_handoff_have_exact_causal_order() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let harness = Harness::new(py, None);

            let normal = harness.scene(py, "scene-normal-drain");
            let returned = PyStopIteration::new_err(());
            close_scene(
                &normal,
                Some((SceneDriverExit::Returned, Some(&returned))),
                false,
                None,
            );

            let cancelling = harness.scene(py, "scene-cancel-existing");
            let returned = PyStopIteration::new_err(());
            close_scene(
                &cancelling,
                Some((SceneDriverExit::Returned, Some(&returned))),
                true,
                None,
            );

            let events = harness.log.events();
            for (id, relation) in [
                ("scene-normal-drain", CausalRelation::FollowsFrom),
                ("scene-cancel-existing", CausalRelation::Handoff),
            ] {
                let lifecycle = started(&events, id, SpanKind::SceneLifecycle);
                let drain = started(&events, id, SpanKind::SceneDrain);
                let cleanup = started(&events, id, SpanKind::SceneCleanup);
                let drain_finish = finished(&events, id, drain.header().sequence());
                let cleanup_finish = finished(&events, id, cleanup.header().sequence());
                let lifecycle_finish = finished(&events, id, lifecycle.header().sequence());

                assert_eq!(drain.parent_span_id(), Some(lifecycle.header().sequence()));
                assert_eq!(
                    cleanup.parent_span_id(),
                    Some(lifecycle.header().sequence())
                );
                assert_cause(
                    drain.header(),
                    lifecycle.header().sequence(),
                    CausalRelation::FollowsFrom,
                );
                assert_cause(
                    cleanup.header(),
                    drain.header().sequence(),
                    CausalRelation::Handoff,
                );
                assert_cause(drain_finish.header(), drain.header().sequence(), relation);
                assert_cause(
                    cleanup_finish.header(),
                    drain_finish.header().sequence(),
                    CausalRelation::Handoff,
                );
                assert_eq!(drain_finish.outcome(), SpanOutcome::Completed);
                assert_eq!(cleanup_finish.outcome(), SpanOutcome::Completed);
                assert!(
                    cleanup_finish.header().sequence() < lifecycle_finish.header().sequence(),
                    "Scene terminal facts must precede lifecycle finish"
                );
            }
            harness.finish();
        });
    }

    #[test]
    fn task_failures_and_cancellation_are_normalized_without_payload_capture() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let harness = Harness::new(py, None);

            let cases = [
                (
                    "scene-user-failure",
                    PyRuntimeError::new_err("secret user failure"),
                    SpanOutcome::Failed,
                    Some(DRAIN_FAILED),
                ),
                (
                    "scene-system-exit",
                    PySystemExit::new_err("secret signal payload"),
                    SpanOutcome::Failed,
                    Some(DRAIN_FAILED),
                ),
                (
                    "scene-timeout",
                    PyTimeoutError::new_err("secret timeout payload"),
                    SpanOutcome::Failed,
                    Some(DRAIN_FAILED),
                ),
                (
                    "scene-cancelled",
                    py.import("asyncio")?
                        .getattr("CancelledError")?
                        .call0()
                        .map(PyErr::from_value)?,
                    SpanOutcome::Cancelled,
                    Some(DRAIN_CANCELLED),
                ),
            ];
            for (id, error, _, _) in &cases {
                let scene = harness.scene(py, id);
                close_scene(
                    &scene,
                    Some((SceneDriverExit::Returned, Some(error))),
                    false,
                    Some(error),
                );
            }

            let events = harness.log.events();
            for (id, _, outcome, code) in cases {
                assert_terminal(&events, id, SpanKind::SceneDrain, outcome, code);
                assert_terminal(
                    &events,
                    id,
                    SpanKind::SceneCleanup,
                    SpanOutcome::Completed,
                    None,
                );
            }
            assert!(events.iter().all(|event| {
                let bytes = String::from_utf8_lossy(event.canonical_bytes());
                !bytes.contains("secret user failure")
                    && !bytes.contains("secret signal payload")
                    && !bytes.contains("secret timeout payload")
            }));
            harness.finish();
            Ok::<_, PyErr>(())
        })
        .expect("terminal normalization test must complete");
    }

    #[test]
    fn explicit_close_keeps_drain_cancellation_distinct_from_cleanup_failure() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let harness = Harness::new(py, None);

            let closed = harness.scene(py, "scene-closed");
            close_scene(&closed, Some((SceneDriverExit::Closed, None)), false, None);

            let cleanup_failed = harness.scene(py, "scene-cleanup-failed");
            let error = PyRuntimeError::new_err("secret cleanup failure");
            close_scene(
                &cleanup_failed,
                Some((SceneDriverExit::Closed, Some(&error))),
                false,
                Some(&error),
            );

            let events = harness.log.events();
            for id in ["scene-closed", "scene-cleanup-failed"] {
                assert_terminal(
                    &events,
                    id,
                    SpanKind::SceneDrain,
                    SpanOutcome::Cancelled,
                    Some(DRAIN_CANCELLED),
                );
            }
            assert_terminal(
                &events,
                "scene-closed",
                SpanKind::SceneCleanup,
                SpanOutcome::Completed,
                None,
            );
            assert_terminal(
                &events,
                "scene-cleanup-failed",
                SpanKind::SceneCleanup,
                SpanOutcome::Failed,
                Some(CLEANUP_FAILED),
            );
            assert!(events.iter().all(|event| {
                !String::from_utf8_lossy(event.canonical_bytes()).contains("secret cleanup failure")
            }));
            harness.finish();
        });
    }

    #[test]
    fn lost_driver_terminal_emits_a_gap_without_a_fake_drain_span() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let harness = Harness::new(py, None);

            let cleared = harness.scene(py, "scene-driver-cleared");
            close_scene(
                &cleared,
                Some((SceneDriverExit::Cleared, None)),
                false,
                None,
            );

            let missing = harness.scene(py, "scene-driver-missing");
            let task_error = PyRuntimeError::new_err("task creation failed before driver handoff");
            close_scene(&missing, None, false, Some(&task_error));

            let events = harness.log.events();
            for id in ["scene-driver-cleared", "scene-driver-missing"] {
                assert!(events.iter().all(|event| {
                    !matches!(
                        event.event(),
                        DiagnosticEvent::SpanStarted(start)
                            if scene_id(start.header()) == Some(id)
                                && start.span_kind() == SpanKind::SceneDrain
                    )
                }));
                let gap = events
                    .iter()
                    .find_map(|event| match event.event() {
                        DiagnosticEvent::ObservationGap(gap)
                            if scene_id(gap.header()) == Some(id) =>
                        {
                            Some(gap)
                        }
                        _ => None,
                    })
                    .expect("lost Scene driver terminal emits a canonical gap");
                assert_eq!(gap.producer(), "runtime");
                assert_eq!(gap.component(), Some("scene-drain"));
                assert_eq!(gap.reason(), LOST_DRIVER_TERMINAL);
                assert_eq!(gap.dropped_count().map(SchemaU64::get), Some(1));
                assert_eq!(gap.affected_kind(), Some(DiagnosticEventKind::SpanFinished));
                assert_eq!(gap.affected_scope(), Some(gap.header().scope()));
                assert_cause(
                    gap.header(),
                    started(&events, id, SpanKind::SceneLifecycle)
                        .header()
                        .sequence(),
                    CausalRelation::FollowsFrom,
                );
                assert_terminal(
                    &events,
                    id,
                    SpanKind::SceneCleanup,
                    SpanOutcome::Completed,
                    None,
                );
            }
            assert!(events.iter().all(|event| {
                !String::from_utf8_lossy(event.canonical_bytes())
                    .contains("task creation failed before driver handoff")
            }));
            harness.finish();
        });
    }

    #[test]
    fn drain_admission_failure_latches_runtime_and_expires_state() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let harness = Harness::new(py, Some(5));
            let scene = harness.scene(py, "scene-drain-admission-failure");
            let returned = PyStopIteration::new_err(());

            close_scene(
                &scene,
                Some((SceneDriverExit::Returned, Some(&returned))),
                false,
                None,
            );

            assert!(lock(producers()).get(&address(scene.as_ref())).is_none());
            assert_eq!(
                harness
                    .runtime
                    .failure()
                    .as_ref()
                    .map(runtime_producer::RuntimeProducerError::code),
                Some("diagnostic.admission-failed")
            );
            assert_eq!(harness.log.events().len(), 4);
            drop(harness);
        });
    }
}
