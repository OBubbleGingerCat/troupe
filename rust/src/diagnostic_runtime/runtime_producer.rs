use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex, MutexGuard, OnceLock, Weak},
};

use pyo3::{PyErr, Python, types::PyAnyMethods};
use troupe_diagnostics_core::{
    detail::{EmptyDetail, SpanStartDetail},
    event::DiagnosticScope,
    kinds::SpanOutcome,
    scalar::SchemaU64,
};

use crate::{
    diagnostic_runtime::load_producer::{DiagnosticProducerError, DiagnosticRunContext},
    orchestration::{
        actor_registry::ProductionState, python_task::RuntimeTaskPhase, runtime::RuntimeCore,
        scene_context::RunBinding,
    },
};

const START_CANCELLED: &str = "production-start-cancelled";
const START_FAILED: &str = "production-start-failed";
const SCENE_FAILED: &str = "production-scene-failed";
const STOP_CANCELLED: &str = "production-stop-cancelled";
const STOP_FAILED: &str = "production-stop-failed";
const RUN_CANCELLED: &str = "production-lifecycle-cancelled";
const RUN_FAILED: &str = "production-lifecycle-failed";
const SHUTDOWN_CANCELLED: &str = "production-shutdown-cancelled";
const SHUTDOWN_FAILED: &str = "production-shutdown-failed";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeHook {
    ProductionCreated,
    RunStarted,
    ProductionStartEntered,
    ProductionStartReturned,
    SceneEntered,
    SceneReturned,
    ProductionStopEntered,
    ProductionStopReturned,
    ShutdownRequested,
    RunLifecycleReturned,
    RunFinished,
}

#[derive(Clone)]
pub(crate) struct RuntimeLineageSnapshot {
    runtime: Arc<RuntimeLifecycleProducer>,
    context: DiagnosticRunContext,
    scope: DiagnosticScope,
    containing_span_id: SchemaU64,
}

impl RuntimeLineageSnapshot {
    pub(crate) fn runtime(&self) -> &Arc<RuntimeLifecycleProducer> {
        &self.runtime
    }

    pub(crate) fn context(&self) -> DiagnosticRunContext {
        self.context.clone()
    }

    pub(crate) const fn scope(&self) -> &DiagnosticScope {
        &self.scope
    }

    pub(crate) const fn containing_span_id(&self) -> SchemaU64 {
        self.containing_span_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeProducerError {
    Diagnostic(DiagnosticProducerError),
    State { code: &'static str },
}

impl RuntimeProducerError {
    const fn state(code: &'static str) -> Self {
        Self::State { code }
    }

    pub(crate) fn code(&self) -> &str {
        match self {
            Self::Diagnostic(error) => error.code(),
            Self::State { code } => code,
        }
    }
}

impl From<DiagnosticProducerError> for RuntimeProducerError {
    fn from(error: DiagnosticProducerError) -> Self {
        Self::Diagnostic(error)
    }
}

impl fmt::Display for RuntimeProducerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Diagnostic(error) => fmt::Display::fmt(error, formatter),
            Self::State { code } => write!(formatter, "runtime diagnostic state failed [{code}]"),
        }
    }
}

impl std::error::Error for RuntimeProducerError {}

#[derive(Clone, Debug, Eq, PartialEq)]
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

#[derive(Clone, Debug, Eq, PartialEq)]
enum PhaseState {
    NotEntered,
    Open(SchemaU64),
    Closed(TerminalSpan),
}

#[derive(Debug)]
struct RuntimeLifecycleState {
    run_span_id: SchemaU64,
    run_started: bool,
    start: PhaseState,
    stop: PhaseState,
    shutdown: PhaseState,
    first_user_failure: Option<TerminalSpan>,
    lifecycle_terminal: Option<TerminalSpan>,
    run_closed: bool,
    producer_failure: Option<RuntimeProducerError>,
}

pub(crate) struct RuntimeLifecycleProducer {
    context: DiagnosticRunContext,
    core_key: usize,
    binding_key: usize,
    state: Mutex<RuntimeLifecycleState>,
}

impl RuntimeLifecycleProducer {
    fn start(
        core_key: usize,
        binding_key: usize,
        context: DiagnosticRunContext,
    ) -> Result<Self, RuntimeProducerError> {
        let run_span_id = context.start_span(
            empty_scope(),
            SpanStartDetail::RunLifecycle(EmptyDetail::new()),
            None,
        )?;
        Ok(Self {
            context,
            core_key,
            binding_key,
            state: Mutex::new(RuntimeLifecycleState {
                run_span_id,
                run_started: false,
                start: PhaseState::NotEntered,
                stop: PhaseState::NotEntered,
                shutdown: PhaseState::NotEntered,
                first_user_failure: None,
                lifecycle_terminal: None,
                run_closed: false,
                producer_failure: None,
            }),
        })
    }

    pub(crate) fn context(&self) -> DiagnosticRunContext {
        self.context.clone()
    }

    pub(crate) fn run_span_id(&self) -> SchemaU64 {
        lock(&self.state).run_span_id
    }

    pub(crate) fn failure(&self) -> Option<RuntimeProducerError> {
        lock(&self.state).producer_failure.clone()
    }

    pub(crate) fn terminal_outcome(&self) -> Result<SpanOutcome, RuntimeProducerError> {
        let state = lock(&self.state);
        if let Some(failure) = state.producer_failure.clone() {
            return Err(failure);
        }
        if !state.run_closed {
            return Err(RuntimeProducerError::state(
                "runtime.terminal-outcome-before-run-finished",
            ));
        }
        state
            .lifecycle_terminal
            .as_ref()
            .map(|terminal| terminal.outcome)
            .ok_or_else(|| RuntimeProducerError::state("runtime.terminal-outcome-missing"))
    }

    pub(crate) fn latch_diagnostic_failure(&self, error: DiagnosticProducerError) {
        let mut state = lock(&self.state);
        latch_failure(&mut state, error.into());
    }

    fn lineage_snapshot(
        self: &Arc<Self>,
        phase: RuntimeTaskPhase,
    ) -> Option<RuntimeLineageSnapshot> {
        let state = lock(&self.state);
        if state.producer_failure.is_some() || state.run_closed {
            return None;
        }
        let containing_span_id = match phase {
            RuntimeTaskPhase::Start => match state.start {
                PhaseState::Open(span_id) => span_id,
                PhaseState::NotEntered | PhaseState::Closed(_) => return None,
            },
            RuntimeTaskPhase::Stop => match state.stop {
                PhaseState::Open(span_id) => span_id,
                PhaseState::NotEntered | PhaseState::Closed(_) => return None,
            },
        };
        Some(RuntimeLineageSnapshot {
            runtime: Arc::clone(self),
            context: self.context.clone(),
            scope: empty_scope(),
            containing_span_id,
        })
    }

    fn observe(&self, hook: RuntimeHook, error: Option<&PyErr>) {
        let mut state = lock(&self.state);
        if state.producer_failure.is_some() {
            return;
        }

        match hook {
            RuntimeHook::RunStarted => self.run_started(&mut state),
            RuntimeHook::ProductionStartEntered => self.enter_start(&mut state),
            RuntimeHook::ProductionStartReturned => self.finish_start(&mut state, error),
            RuntimeHook::SceneReturned => self.observe_scene_return(&mut state, error),
            RuntimeHook::ProductionStopEntered => self.enter_stop(&mut state),
            RuntimeHook::ProductionStopReturned => self.finish_stop(&mut state, error),
            RuntimeHook::ShutdownRequested => self.enter_shutdown(&mut state),
            RuntimeHook::RunLifecycleReturned => self.lifecycle_returned(&mut state, error),
            RuntimeHook::RunFinished => self.finish_run(&mut state),
            RuntimeHook::ProductionCreated | RuntimeHook::SceneEntered => {}
        }
    }

    fn run_started(&self, state: &mut RuntimeLifecycleState) {
        if state.run_started || state.run_closed {
            latch_failure(
                state,
                RuntimeProducerError::state("runtime.run-start-transition-invalid"),
            );
            return;
        }
        state.run_started = true;
    }

    fn enter_start(&self, state: &mut RuntimeLifecycleState) {
        if !state.run_started || !matches!(state.start, PhaseState::NotEntered) {
            latch_failure(
                state,
                RuntimeProducerError::state("runtime.production-start-transition-invalid"),
            );
            return;
        }
        match self.start_child(SpanStartDetail::ProductionStart(EmptyDetail::new()), state) {
            Ok(span_id) => state.start = PhaseState::Open(span_id),
            Err(error) => latch_failure(state, error),
        }
    }

    fn finish_start(&self, state: &mut RuntimeLifecycleState, error: Option<&PyErr>) {
        let PhaseState::Open(span_id) = state.start else {
            latch_failure(
                state,
                RuntimeProducerError::state("runtime.production-start-return-without-entry"),
            );
            return;
        };
        let terminal = phase_terminal(error, START_CANCELLED, START_FAILED);
        if error.is_some() {
            state.first_user_failure = Some(terminal.clone());
        }
        match self.finish_child(span_id, &terminal) {
            Ok(()) => state.start = PhaseState::Closed(terminal),
            Err(error) => latch_failure(state, error),
        }
    }

    fn observe_scene_return(&self, state: &mut RuntimeLifecycleState, error: Option<&PyErr>) {
        let Some(error) = error else {
            return;
        };
        if !is_cancelled_error(error) && state.first_user_failure.is_none() {
            state.first_user_failure = Some(TerminalSpan::failed(SCENE_FAILED));
        }
    }

    fn enter_stop(&self, state: &mut RuntimeLifecycleState) {
        if !state.run_started
            || !matches!(
                state.start,
                PhaseState::Closed(TerminalSpan {
                    outcome: SpanOutcome::Completed,
                    ..
                })
            )
            || !matches!(state.stop, PhaseState::NotEntered)
        {
            latch_failure(
                state,
                RuntimeProducerError::state("runtime.production-stop-transition-invalid"),
            );
            return;
        }
        match self.start_child(SpanStartDetail::ProductionStop(EmptyDetail::new()), state) {
            Ok(span_id) => state.stop = PhaseState::Open(span_id),
            Err(error) => latch_failure(state, error),
        }
    }

    fn finish_stop(&self, state: &mut RuntimeLifecycleState, error: Option<&PyErr>) {
        let PhaseState::Open(span_id) = state.stop else {
            latch_failure(
                state,
                RuntimeProducerError::state("runtime.production-stop-return-without-entry"),
            );
            return;
        };
        let terminal = phase_terminal(error, STOP_CANCELLED, STOP_FAILED);
        if error.is_some() && state.first_user_failure.is_none() {
            state.first_user_failure = Some(terminal.clone());
        }
        match self.finish_child(span_id, &terminal) {
            Ok(()) => state.stop = PhaseState::Closed(terminal),
            Err(error) => latch_failure(state, error),
        }
    }

    fn enter_shutdown(&self, state: &mut RuntimeLifecycleState) {
        if state.run_closed {
            return;
        }
        if !matches!(state.shutdown, PhaseState::NotEntered) {
            latch_failure(
                state,
                RuntimeProducerError::state("runtime.production-shutdown-transition-invalid"),
            );
            return;
        }
        match self.start_child(
            SpanStartDetail::ProductionShutdown(EmptyDetail::new()),
            state,
        ) {
            Ok(span_id) => state.shutdown = PhaseState::Open(span_id),
            Err(error) => latch_failure(state, error),
        }
    }

    fn lifecycle_returned(&self, state: &mut RuntimeLifecycleState, error: Option<&PyErr>) {
        if state.lifecycle_terminal.is_some() || state.run_closed {
            latch_failure(
                state,
                RuntimeProducerError::state("runtime.lifecycle-return-transition-invalid"),
            );
            return;
        }
        let lifecycle_shape_is_proven = matches!(
            (&state.start, &state.stop),
            (
                PhaseState::Closed(TerminalSpan {
                    outcome: SpanOutcome::Completed,
                    ..
                }),
                PhaseState::Closed(_),
            ) | (
                PhaseState::Closed(TerminalSpan {
                    outcome: SpanOutcome::Cancelled | SpanOutcome::Failed,
                    ..
                }),
                PhaseState::NotEntered,
            )
        );
        if !lifecycle_shape_is_proven {
            latch_failure(
                state,
                RuntimeProducerError::state("runtime.lifecycle-return-with-unproven-phases"),
            );
            return;
        }
        let terminal = match error {
            None if state.first_user_failure.is_none() => TerminalSpan::completed(),
            None => {
                latch_failure(
                    state,
                    RuntimeProducerError::state("runtime.lifecycle-success-after-phase-failure"),
                );
                return;
            }
            Some(error) if is_cancelled_error(error) => TerminalSpan::cancelled(RUN_CANCELLED),
            Some(_) => state
                .first_user_failure
                .clone()
                .unwrap_or_else(|| TerminalSpan::failed(RUN_FAILED)),
        };
        state.lifecycle_terminal = Some(terminal);
    }

    fn finish_run(&self, state: &mut RuntimeLifecycleState) {
        if state.run_closed {
            latch_failure(
                state,
                RuntimeProducerError::state("runtime.run-finish-transition-invalid"),
            );
            return;
        }
        let Some(run_terminal) = state.lifecycle_terminal.clone() else {
            latch_failure(
                state,
                RuntimeProducerError::state("runtime.run-finished-before-lifecycle-return"),
            );
            return;
        };
        if matches!(state.start, PhaseState::Open(_)) || matches!(state.stop, PhaseState::Open(_)) {
            latch_failure(
                state,
                RuntimeProducerError::state("runtime.run-finished-with-open-phase"),
            );
            return;
        }

        if let PhaseState::Open(span_id) = state.shutdown {
            let shutdown_terminal = match run_terminal.outcome {
                SpanOutcome::Completed => TerminalSpan::completed(),
                SpanOutcome::Cancelled => TerminalSpan::cancelled(SHUTDOWN_CANCELLED),
                SpanOutcome::Failed => TerminalSpan::failed(SHUTDOWN_FAILED),
            };
            if let Err(error) = self.finish_child(span_id, &shutdown_terminal) {
                latch_failure(state, error);
                return;
            }
            state.shutdown = PhaseState::Closed(shutdown_terminal);
        }

        match self.context.finish_span(
            empty_scope(),
            state.run_span_id,
            run_terminal.outcome,
            run_terminal.error_code.map(str::to_owned),
        ) {
            Ok(()) => state.run_closed = true,
            Err(error) => latch_failure(state, error.into()),
        }
    }

    fn start_child(
        &self,
        detail: SpanStartDetail,
        state: &RuntimeLifecycleState,
    ) -> Result<SchemaU64, RuntimeProducerError> {
        self.context
            .start_span(empty_scope(), detail, Some(state.run_span_id))
            .map_err(Into::into)
    }

    fn finish_child(
        &self,
        span_id: SchemaU64,
        terminal: &TerminalSpan,
    ) -> Result<(), RuntimeProducerError> {
        self.context
            .finish_span(
                empty_scope(),
                span_id,
                terminal.outcome,
                terminal.error_code.map(str::to_owned),
            )
            .map_err(Into::into)
    }
}

#[derive(Default)]
struct RuntimeProducerRegistry {
    by_core: HashMap<usize, Arc<RuntimeLifecycleProducer>>,
    by_binding: HashMap<usize, Weak<RuntimeLifecycleProducer>>,
}

fn registry() -> &'static Mutex<RuntimeProducerRegistry> {
    static REGISTRY: OnceLock<Mutex<RuntimeProducerRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(RuntimeProducerRegistry::default()))
}

pub(crate) fn install(
    core: &RuntimeCore,
    binding: &RunBinding,
    context: DiagnosticRunContext,
) -> Result<Arc<RuntimeLifecycleProducer>, RuntimeProducerError> {
    let core_key = address(core);
    let binding_key = address(binding);
    let mut producers = lock(registry());
    if producers
        .by_binding
        .get(&binding_key)
        .is_some_and(|producer| producer.upgrade().is_none())
    {
        producers.by_binding.remove(&binding_key);
    }
    if producers.by_core.contains_key(&core_key) || producers.by_binding.contains_key(&binding_key)
    {
        return Err(RuntimeProducerError::state(
            "runtime.diagnostic-producer-already-installed",
        ));
    }

    let producer = Arc::new(RuntimeLifecycleProducer::start(
        core_key,
        binding_key,
        context,
    )?);
    producers.by_core.insert(core_key, Arc::clone(&producer));
    producers
        .by_binding
        .insert(binding_key, Arc::downgrade(&producer));
    Ok(producer)
}

pub(crate) fn producer_for_binding(binding: &RunBinding) -> Option<Arc<RuntimeLifecycleProducer>> {
    lock(registry())
        .by_binding
        .get(&address(binding))
        .and_then(Weak::upgrade)
}

pub(crate) fn lineage_snapshot(
    binding: &RunBinding,
    phase: RuntimeTaskPhase,
) -> Option<RuntimeLineageSnapshot> {
    producer_for_binding(binding)?.lineage_snapshot(phase)
}

fn producer_for_core(core: &RuntimeCore) -> Option<Arc<RuntimeLifecycleProducer>> {
    lock(registry()).by_core.get(&address(core)).cloned()
}

fn detach(producer: &Arc<RuntimeLifecycleProducer>) {
    let mut producers = lock(registry());
    if producers
        .by_core
        .get(&producer.core_key)
        .is_some_and(|current| Arc::ptr_eq(current, producer))
    {
        producers.by_core.remove(&producer.core_key);
    }
    let remove_binding = match producers.by_binding.get(&producer.binding_key) {
        Some(current) => current
            .upgrade()
            .is_none_or(|current| Arc::ptr_eq(&current, producer)),
        None => false,
    };
    if remove_binding {
        producers.by_binding.remove(&producer.binding_key);
    }
}

#[inline]
pub(crate) fn observe_core(core: &RuntimeCore, hook: RuntimeHook) {
    let Some(producer) = producer_for_core(core) else {
        return;
    };
    producer.observe(hook, None);
    if hook == RuntimeHook::RunFinished {
        detach(&producer);
    }
}

#[inline]
pub(crate) fn run_started(core: &RuntimeCore, binding: &RunBinding) {
    let Some(producer) = producer_for_core(core) else {
        return;
    };
    let Some(binding_producer) = producer_for_binding(binding) else {
        producer.observe(RuntimeHook::RunStarted, None);
        producer.latch_state_failure("runtime.run-binding-producer-missing");
        return;
    };
    if !Arc::ptr_eq(&producer, &binding_producer) {
        producer.latch_state_failure("runtime.run-binding-producer-mismatch");
        return;
    }
    producer.observe(RuntimeHook::RunStarted, None);
}

#[inline]
pub(crate) fn observe_production(_production: &ProductionState, _hook: RuntimeHook) {}

#[inline]
pub(crate) fn observe_binding(binding: &RunBinding, hook: RuntimeHook, error: Option<&PyErr>) {
    if let Some(producer) = producer_for_binding(binding) {
        producer.observe(hook, error);
    }
}

impl RuntimeLifecycleProducer {
    pub(crate) fn latch_state_failure(&self, code: &'static str) {
        let mut state = lock(&self.state);
        latch_failure(&mut state, RuntimeProducerError::state(code));
    }
}

fn phase_terminal(
    error: Option<&PyErr>,
    cancelled_code: &'static str,
    failed_code: &'static str,
) -> TerminalSpan {
    match error {
        None => TerminalSpan::completed(),
        Some(error) if is_cancelled_error(error) => TerminalSpan::cancelled(cancelled_code),
        Some(_) => TerminalSpan::failed(failed_code),
    }
}

fn is_cancelled_error(error: &PyErr) -> bool {
    Python::attach(|py| {
        py.import("asyncio")
            .and_then(|asyncio| asyncio.getattr("CancelledError"))
            .is_ok_and(|cancelled| error.is_instance(py, &cancelled))
    })
}

fn empty_scope() -> DiagnosticScope {
    DiagnosticScope::new(None, None, None, None, None, None, None)
}

fn address<T>(value: &T) -> usize {
    std::ptr::from_ref(value).addr()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn latch_failure(state: &mut RuntimeLifecycleState, error: RuntimeProducerError) {
    if state.producer_failure.is_none() {
        state.producer_failure = Some(error);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Instant,
    };

    use pyo3::{exceptions::PyRuntimeError, prelude::*};
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

    use crate::orchestration::{runtime::RuntimeCore, scene_context::RunBinding};

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
            formatter.write_str("injected runtime diagnostic admission failure")
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

    fn context(fail_on_attempt: Option<usize>) -> (DiagnosticRunContext, EventLog) {
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
        (
            DiagnosticRunContext::with_hub(hub, RunClock::from_origin(Instant::now())),
            log,
        )
    }

    fn binding(py: Python<'_>) -> Arc<RunBinding> {
        Arc::new(RunBinding::new_for_test(py).expect("create test Run binding"))
    }

    fn assert_start(
        event: &AcceptedDiagnosticEvent,
        sequence: u64,
        kind: SpanKind,
        parent: Option<u64>,
    ) {
        assert_eq!(event.identity().sequence().get(), sequence);
        let DiagnosticEvent::SpanStarted(start) = event.event() else {
            panic!("expected span start")
        };
        assert_eq!(start.span_kind(), kind);
        assert_eq!(start.parent_span_id().map(SchemaU64::get), parent);
        assert_eq!(start.header().scope(), &empty_scope());
    }

    fn assert_finish(
        event: &AcceptedDiagnosticEvent,
        sequence: u64,
        span_id: u64,
        outcome: SpanOutcome,
        error_code: Option<&str>,
    ) {
        assert_eq!(event.identity().sequence().get(), sequence);
        let DiagnosticEvent::SpanFinished(finish) = event.event() else {
            panic!("expected span finish")
        };
        assert_eq!(finish.span_id().get(), span_id);
        assert_eq!(finish.outcome(), outcome);
        assert_eq!(finish.error_code(), error_code);
        assert_eq!(finish.header().scope(), &empty_scope());
    }

    #[test]
    fn success_path_emits_exact_nested_lifecycle_pairs() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let core = Arc::new(RuntimeCore::new());
            let permit = core.begin().expect("start test runtime");
            let binding = binding(py);
            let (context, log) = context(None);
            let producer = install(&core, &binding, context).expect("install runtime producer");

            run_started(&core, &binding);
            observe_binding(&binding, RuntimeHook::ProductionStartEntered, None);
            let start_lineage = lineage_snapshot(&binding, RuntimeTaskPhase::Start)
                .expect("open start phase exposes lineage");
            assert!(Arc::ptr_eq(start_lineage.runtime(), &producer));
            assert_eq!(start_lineage.scope(), &empty_scope());
            assert_eq!(start_lineage.containing_span_id(), SchemaU64::new(2));
            observe_binding(&binding, RuntimeHook::ProductionStartReturned, None);
            assert!(lineage_snapshot(&binding, RuntimeTaskPhase::Start).is_none());
            core.request_shutdown();
            observe_binding(&binding, RuntimeHook::ProductionStopEntered, None);
            let stop_lineage = lineage_snapshot(&binding, RuntimeTaskPhase::Stop)
                .expect("open stop phase exposes lineage");
            assert!(Arc::ptr_eq(stop_lineage.runtime(), &producer));
            assert_eq!(stop_lineage.scope(), &empty_scope());
            assert_eq!(stop_lineage.containing_span_id(), SchemaU64::new(5));
            observe_binding(&binding, RuntimeHook::ProductionStopReturned, None);
            assert!(lineage_snapshot(&binding, RuntimeTaskPhase::Stop).is_none());
            observe_binding(&binding, RuntimeHook::RunLifecycleReturned, None);
            drop(permit);

            assert!(producer.failure().is_none());
            assert!(producer_for_binding(&binding).is_none());
            let events = log.events();
            assert_eq!(events.len(), 8);
            assert_start(&events[0], 1, SpanKind::RunLifecycle, None);
            assert_start(&events[1], 2, SpanKind::ProductionStart, Some(1));
            assert_finish(&events[2], 3, 2, SpanOutcome::Completed, None);
            assert_start(&events[3], 4, SpanKind::ProductionShutdown, Some(1));
            assert_start(&events[4], 5, SpanKind::ProductionStop, Some(1));
            assert_finish(&events[5], 6, 5, SpanOutcome::Completed, None);
            assert_finish(&events[6], 7, 4, SpanOutcome::Completed, None);
            assert_finish(&events[7], 8, 1, SpanOutcome::Completed, None);
            assert!(events.windows(2).all(|pair| {
                pair[0].event().header().elapsed_ns().get()
                    <= pair[1].event().header().elapsed_ns().get()
            }));
        });
    }

    #[test]
    fn start_failure_closes_only_proven_spans_and_keeps_payload_out() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let core = Arc::new(RuntimeCore::new());
            let permit = core.begin().expect("start test runtime");
            let binding = binding(py);
            let (context, log) = context(None);
            let producer = install(&core, &binding, context).expect("install runtime producer");
            let error = PyRuntimeError::new_err("secret start payload");

            run_started(&core, &binding);
            observe_binding(&binding, RuntimeHook::ProductionStartEntered, None);
            observe_binding(&binding, RuntimeHook::ProductionStartReturned, Some(&error));
            observe_binding(&binding, RuntimeHook::RunLifecycleReturned, Some(&error));
            drop(permit);

            assert!(producer.failure().is_none());
            let events = log.events();
            assert_eq!(events.len(), 4);
            assert_start(&events[0], 1, SpanKind::RunLifecycle, None);
            assert_start(&events[1], 2, SpanKind::ProductionStart, Some(1));
            assert_finish(&events[2], 3, 2, SpanOutcome::Failed, Some(START_FAILED));
            assert_finish(&events[3], 4, 1, SpanOutcome::Failed, Some(START_FAILED));
            assert!(events.iter().all(|event| {
                !String::from_utf8_lossy(event.canonical_bytes()).contains("secret start payload")
            }));
        });
    }

    #[test]
    fn cancelled_start_is_distinct_and_does_not_invent_stop() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let core = Arc::new(RuntimeCore::new());
            let permit = core.begin().expect("start test runtime");
            let binding = binding(py);
            let (context, log) = context(None);
            install(&core, &binding, context).expect("install runtime producer");
            let cancelled = py
                .import("asyncio")
                .and_then(|asyncio| asyncio.getattr("CancelledError"))
                .and_then(|cancelled| cancelled.call0())
                .map(PyErr::from_value)
                .expect("construct CancelledError");

            run_started(&core, &binding);
            observe_binding(&binding, RuntimeHook::ProductionStartEntered, None);
            core.request_shutdown();
            observe_binding(
                &binding,
                RuntimeHook::ProductionStartReturned,
                Some(&cancelled),
            );
            observe_binding(
                &binding,
                RuntimeHook::RunLifecycleReturned,
                Some(&cancelled),
            );
            drop(permit);

            let events = log.events();
            assert_eq!(events.len(), 6);
            assert_finish(
                &events[3],
                4,
                2,
                SpanOutcome::Cancelled,
                Some(START_CANCELLED),
            );
            assert_finish(
                &events[4],
                5,
                3,
                SpanOutcome::Cancelled,
                Some(SHUTDOWN_CANCELLED),
            );
            assert_finish(
                &events[5],
                6,
                1,
                SpanOutcome::Cancelled,
                Some(RUN_CANCELLED),
            );
        });
    }

    #[test]
    fn aggregate_failure_after_phase_hooks_uses_authoritative_final_result() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let core = Arc::new(RuntimeCore::new());
            let permit = core.begin().expect("start test runtime");
            let binding = binding(py);
            let (context, log) = context(None);
            install(&core, &binding, context).expect("install runtime producer");
            let aggregate = PyRuntimeError::new_err("task factory restore payload");

            run_started(&core, &binding);
            observe_binding(&binding, RuntimeHook::ProductionStartEntered, None);
            observe_binding(&binding, RuntimeHook::ProductionStartReturned, None);
            core.request_shutdown();
            observe_binding(&binding, RuntimeHook::ProductionStopEntered, None);
            observe_binding(&binding, RuntimeHook::ProductionStopReturned, None);
            observe_binding(
                &binding,
                RuntimeHook::RunLifecycleReturned,
                Some(&aggregate),
            );
            drop(permit);

            let events = log.events();
            assert_eq!(events.len(), 8);
            assert_finish(&events[6], 7, 4, SpanOutcome::Failed, Some(SHUTDOWN_FAILED));
            assert_finish(&events[7], 8, 1, SpanOutcome::Failed, Some(RUN_FAILED));
            assert!(events.iter().all(|event| {
                !String::from_utf8_lossy(event.canonical_bytes())
                    .contains("task factory restore payload")
            }));
        });
    }

    #[test]
    fn admission_failure_is_latched_without_replacing_user_failure() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let core = Arc::new(RuntimeCore::new());
            let permit = core.begin().expect("start test runtime");
            let binding = binding(py);
            let (context, log) = context(Some(3));
            let producer = install(&core, &binding, context).expect("install runtime producer");
            let error = PyRuntimeError::new_err("original user object");
            let expected = error.value(py).clone().unbind();

            run_started(&core, &binding);
            observe_binding(&binding, RuntimeHook::ProductionStartEntered, None);
            observe_binding(&binding, RuntimeHook::ProductionStartReturned, Some(&error));
            assert!(error.value(py).is(expected.bind(py)));
            assert_eq!(
                producer.failure().as_ref().map(RuntimeProducerError::code),
                Some("diagnostic.admission-failed")
            );
            observe_binding(&binding, RuntimeHook::RunLifecycleReturned, Some(&error));
            drop(permit);

            assert_eq!(log.events().len(), 2);
            assert!(producer_for_binding(&binding).is_none());
        });
    }

    #[test]
    fn missing_final_result_leaves_run_open_instead_of_guessing() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let core = Arc::new(RuntimeCore::new());
            let permit = core.begin().expect("start test runtime");
            let binding = binding(py);
            let (context, log) = context(None);
            let producer = install(&core, &binding, context).expect("install runtime producer");

            run_started(&core, &binding);
            observe_binding(&binding, RuntimeHook::ProductionStartEntered, None);
            observe_binding(&binding, RuntimeHook::ProductionStartReturned, None);
            drop(permit);

            assert_eq!(log.events().len(), 3);
            assert_eq!(
                producer.failure().as_ref().map(RuntimeProducerError::code),
                Some("runtime.run-finished-before-lifecycle-return")
            );
        });
    }

    #[test]
    fn inactive_hooks_remain_allocation_and_event_free() {
        let core = Arc::new(RuntimeCore::new());
        let permit = core.begin().expect("start inactive runtime");
        core.request_shutdown();
        drop(permit);
        assert!(producer_for_core(&core).is_none());
    }
}
