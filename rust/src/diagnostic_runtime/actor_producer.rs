use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use pyo3::types::{PyAny, PyAnyMethods, PyString, PyStringMethods, PyType, PyTypeMethods};
use pyo3::{Bound, Py, PyErr, Python};
use troupe_agent_runtime::AgentSessionDiagnosticContext;
use troupe_diagnostics_core::{
    detail::{ActorDetail, InstantDetail, SpanStartDetail},
    event::DiagnosticScope,
    id::RunLocalId,
    kinds::SpanOutcome,
    scalar::SchemaU64,
};

use crate::{
    diagnostic_runtime::{
        load_producer::{
            DiagnosticProducerError, DiagnosticRunContext, current_production_construction,
        },
        runtime_producer::{self, RuntimeLifecycleProducer},
    },
    orchestration::{
        actor::{Actor, ActorCapability, ActorCapabilityNode, ActorIdentity},
        actor_handle::{ActorHandle, ActorHandleIdentity},
        actor_registry::ProductionState,
    },
};

const CAST_CANCELLED: &str = "actor-cast-cancelled";
const CAST_FAILED: &str = "actor-cast-failed";

static NEXT_ACTOR_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActorHook {
    Constructed,
    HandleCreated,
    HandleCleared,
    RegistryReserved,
    RegistryCommitted,
    RegistryDetached,
}

#[derive(Clone)]
pub(crate) struct ActorLineageSnapshot {
    context: DiagnosticRunContext,
    scope: DiagnosticScope,
    handle_span_id: SchemaU64,
}

#[allow(dead_code)]
impl ActorLineageSnapshot {
    pub(crate) fn context(&self) -> DiagnosticRunContext {
        self.context.clone()
    }

    pub(crate) const fn scope(&self) -> &DiagnosticScope {
        &self.scope
    }

    pub(crate) const fn handle_span_id(&self) -> SchemaU64 {
        self.handle_span_id
    }
}

#[derive(Clone)]
struct ActorEventSource {
    context: DiagnosticRunContext,
    containing_span_id: Option<SchemaU64>,
    runtime: Option<Arc<RuntimeLifecycleProducer>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CastTerminal {
    Succeeded,
    Failed,
    Cancelled,
}

impl CastTerminal {
    const fn span_terminal(self) -> ActorSpanTerminal {
        match self {
            Self::Succeeded => ActorSpanTerminal {
                outcome: SpanOutcome::Completed,
                error_code: None,
            },
            Self::Failed => ActorSpanTerminal {
                outcome: SpanOutcome::Failed,
                error_code: Some(CAST_FAILED),
            },
            Self::Cancelled => ActorSpanTerminal {
                outcome: SpanOutcome::Cancelled,
                error_code: Some(CAST_CANCELLED),
            },
        }
    }
}

#[derive(Clone, Copy)]
struct ActorSpanTerminal {
    outcome: SpanOutcome,
    error_code: Option<&'static str>,
}

struct HandleState {
    identity: ActorHandleIdentity,
    cleared: bool,
}

#[derive(Default)]
struct ActorLifecycleState {
    constructed: bool,
    committed: bool,
    detached: bool,
    handles: Vec<HandleState>,
    cast_terminal: Option<CastTerminal>,
    finish_attempted: bool,
}

struct ActorLifecycleProducer {
    context: DiagnosticRunContext,
    runtime: Option<Arc<RuntimeLifecycleProducer>>,
    scope: DiagnosticScope,
    session: AgentSessionDiagnosticContext,
    detail: ActorDetail,
    handle_span_id: SchemaU64,
    state: Mutex<ActorLifecycleState>,
}

impl ActorLifecycleProducer {
    fn start(
        source: ActorEventSource,
        actor_id: RunLocalId,
        session: AgentSessionDiagnosticContext,
        detail: ActorDetail,
    ) -> Result<Self, DiagnosticProducerError> {
        let scope = actor_scope(actor_id);
        source.context.emit_instant(
            scope.clone(),
            InstantDetail::ActorCast(detail.clone()),
            source.containing_span_id,
        )?;
        let handle_span_id = source.context.start_span(
            scope.clone(),
            SpanStartDetail::ActorHandleLifetime(detail.clone()),
            None,
        )?;
        Ok(Self {
            context: source.context,
            runtime: source.runtime,
            scope,
            session,
            detail,
            handle_span_id,
            state: Mutex::new(ActorLifecycleState::default()),
        })
    }

    fn snapshot(&self) -> Option<ActorLineageSnapshot> {
        let state = lock(&self.state);
        if state.cast_terminal != Some(CastTerminal::Succeeded)
            || state.detached
            || state.finish_attempted
        {
            return None;
        }
        Some(ActorLineageSnapshot {
            context: self.context.clone(),
            scope: self.scope.clone(),
            handle_span_id: self.handle_span_id,
        })
    }

    fn observe_identity(&self, hook: ActorHook) -> Transition {
        let mut state = lock(&self.state);
        match hook {
            ActorHook::Constructed if !state.constructed && state.cast_terminal.is_none() => {
                state.constructed = true;
                Transition::default()
            }
            ActorHook::RegistryCommitted if !state.committed && state.cast_terminal.is_none() => {
                state.committed = true;
                Transition::default()
            }
            ActorHook::RegistryDetached if !state.detached => {
                state.detached = true;
                let terminal = match state.cast_terminal {
                    Some(CastTerminal::Succeeded) => {
                        begin_finish(&mut state, CastTerminal::Succeeded.span_terminal())
                    }
                    Some(CastTerminal::Failed | CastTerminal::Cancelled) | None => None,
                };
                Transition {
                    terminal,
                    state_error: None,
                }
            }
            ActorHook::RegistryReserved => Transition::default(),
            ActorHook::HandleCreated | ActorHook::HandleCleared => {
                Transition::invalid("actor.identity-hook-kind-invalid")
            }
            ActorHook::Constructed => Transition::invalid("actor.constructed-transition-invalid"),
            ActorHook::RegistryCommitted => {
                Transition::invalid("actor.registry-commit-transition-invalid")
            }
            ActorHook::RegistryDetached => {
                Transition::invalid("actor.registry-detach-transition-invalid")
            }
        }
    }

    fn observe_handle(&self, handle: ActorHandleIdentity, hook: ActorHook) -> Transition {
        let mut state = lock(&self.state);
        match hook {
            ActorHook::HandleCreated
                if !state.detached
                    && !state.finish_attempted
                    && !matches!(
                        state.cast_terminal,
                        Some(CastTerminal::Failed | CastTerminal::Cancelled)
                    )
                    && !state
                        .handles
                        .iter()
                        .any(|current| current.identity == handle) =>
            {
                state.handles.push(HandleState {
                    identity: handle,
                    cleared: false,
                });
                Transition::default()
            }
            ActorHook::HandleCleared => {
                let Some(current) = state
                    .handles
                    .iter_mut()
                    .find(|current| current.identity == handle)
                else {
                    return Transition::invalid("actor.handle-clear-without-create");
                };
                if current.cleared {
                    return Transition::invalid("actor.handle-clear-transition-invalid");
                }
                current.cleared = true;
                Transition::default()
            }
            ActorHook::HandleCreated => {
                Transition::invalid("actor.handle-create-transition-invalid")
            }
            ActorHook::Constructed
            | ActorHook::RegistryReserved
            | ActorHook::RegistryCommitted
            | ActorHook::RegistryDetached => Transition::invalid("actor.handle-hook-kind-invalid"),
        }
    }

    fn cast_finished(
        &self,
        terminal: CastTerminal,
        handle: Option<ActorHandleIdentity>,
        detail_matches: bool,
    ) -> Transition {
        let mut state = lock(&self.state);
        if state.cast_terminal.is_some() || state.finish_attempted {
            return Transition::invalid("actor.cast-terminal-transition-invalid");
        }

        state.cast_terminal = Some(terminal);
        let success_transition_valid = terminal != CastTerminal::Succeeded
            || (state.constructed
                && state.committed
                && handle.is_some_and(|handle| {
                    state
                        .handles
                        .iter()
                        .any(|current| current.identity == handle)
                }));
        let state_error = (!detail_matches)
            .then_some("actor.cast-detail-changed")
            .or_else(|| {
                (!success_transition_valid).then_some("actor.cast-success-transition-invalid")
            });
        let terminal = match terminal {
            CastTerminal::Succeeded if state.detached => {
                begin_finish(&mut state, CastTerminal::Succeeded.span_terminal())
            }
            CastTerminal::Succeeded => None,
            terminal => begin_finish(&mut state, terminal.span_terminal()),
        };
        Transition {
            terminal,
            state_error,
        }
    }

    fn report_state_error(
        &self,
        fallback_runtime: Option<&Arc<RuntimeLifecycleProducer>>,
        code: &'static str,
    ) {
        if let Some(runtime) = self.runtime.as_ref().or(fallback_runtime) {
            runtime.latch_state_failure(code);
        }
    }

    fn finish(
        &self,
        fallback_runtime: Option<&Arc<RuntimeLifecycleProducer>>,
        terminal: ActorSpanTerminal,
    ) {
        if let Err(error) = self.context.finish_span(
            self.scope.clone(),
            self.handle_span_id,
            terminal.outcome,
            terminal.error_code.map(str::to_owned),
        ) && let Some(runtime) = self.runtime.as_ref().or(fallback_runtime)
        {
            runtime.latch_diagnostic_failure(error);
        }
    }
}

#[derive(Default)]
struct Transition {
    terminal: Option<ActorSpanTerminal>,
    state_error: Option<&'static str>,
}

impl Transition {
    const fn invalid(code: &'static str) -> Self {
        Self {
            terminal: None,
            state_error: Some(code),
        }
    }
}

fn begin_finish(
    state: &mut ActorLifecycleState,
    terminal: ActorSpanTerminal,
) -> Option<ActorSpanTerminal> {
    if state.finish_attempted {
        return None;
    }
    state.finish_attempted = true;
    Some(terminal)
}

fn actors() -> &'static Mutex<HashMap<usize, Arc<ActorLifecycleProducer>>> {
    static ACTORS: OnceLock<Mutex<HashMap<usize, Arc<ActorLifecycleProducer>>>> = OnceLock::new();
    ACTORS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[inline]
pub(crate) fn observe_identity(
    production: Option<&ProductionState>,
    identity: &ActorIdentity,
    _name: Option<&Bound<'_, PyString>>,
    hook: ActorHook,
) {
    if hook == ActorHook::RegistryReserved {
        return;
    }
    let producer = lock(actors()).get(&address(identity)).cloned();
    let Some(producer) = producer else {
        return;
    };
    let runtime = production.and_then(runtime_for_production);
    apply_transition(
        identity,
        &producer,
        runtime.as_ref(),
        producer.observe_identity(hook),
    );
}

#[inline]
pub(crate) fn observe_handle(
    handle: ActorHandleIdentity,
    node: &Py<ActorCapabilityNode>,
    hook: ActorHook,
) {
    let capability = Python::attach(|py| node.bind(py).borrow().capability());
    let Some(capability) = capability else {
        return;
    };
    let identity = capability.identity();
    let producer = lock(actors()).get(&identity_key(identity)).cloned();
    let Some(producer) = producer else {
        return;
    };
    let runtime = capability
        .production_state()
        .as_deref()
        .and_then(runtime_for_production);
    apply_transition(
        identity,
        &producer,
        runtime.as_ref(),
        producer.observe_handle(handle, hook),
    );
}

#[inline]
pub(crate) fn cleared(_actor: &Actor, _production: Option<&Py<PyAny>>) {
    // Registry detachment is the logical terminal; a leaked raw Actor may outlive it by design.
}

#[inline]
pub(crate) fn cast_started(
    production: &ProductionState,
    identity: &Arc<ActorIdentity>,
    actor_type: &Bound<'_, PyType>,
    name: &Bound<'_, PyString>,
) {
    let Some(source) = source_for_production(production) else {
        return;
    };
    if source
        .runtime
        .as_ref()
        .is_some_and(|runtime| runtime.failure().is_some())
    {
        return;
    }
    let detail = match actor_detail(actor_type, name) {
        Ok(detail) => detail,
        Err(code) => {
            if let Some(runtime) = &source.runtime {
                runtime.latch_state_failure(code);
            }
            return;
        }
    };
    start_actor(identity, source, detail);
}

#[inline]
pub(crate) fn cast_finished(
    production: &ProductionState,
    identity: &Arc<ActorIdentity>,
    actor_type: &Bound<'_, PyType>,
    name: &Bound<'_, PyString>,
    outcome: Result<&Py<ActorHandle>, &PyErr>,
) {
    let producer = lock(actors()).get(&identity_key(identity)).cloned();
    let Some(producer) = producer else {
        return;
    };
    let runtime = runtime_for_production(production);
    let detail_matches =
        actor_detail(actor_type, name).is_ok_and(|detail| detail == producer.detail);
    let (terminal, handle) = match outcome {
        Ok(handle) => (
            CastTerminal::Succeeded,
            Some(handle.bind(actor_type.py()).borrow().diagnostic_identity()),
        ),
        Err(error) if is_cancelled_error(error) => (CastTerminal::Cancelled, None),
        Err(_) => (CastTerminal::Failed, None),
    };
    apply_transition(
        identity,
        &producer,
        runtime.as_ref(),
        producer.cast_finished(terminal, handle, detail_matches),
    );
}

#[allow(dead_code)]
pub(crate) fn lineage_snapshot(actor: &ActorCapability) -> Option<ActorLineageSnapshot> {
    lock(actors())
        .get(&identity_key(actor.identity()))
        .and_then(|producer| producer.snapshot())
}

pub(crate) fn agent_session_context(
    identity: &ActorIdentity,
) -> Option<AgentSessionDiagnosticContext> {
    lock(actors())
        .get(&address(identity))
        .map(|producer| producer.session.clone())
}

fn start_actor(identity: &Arc<ActorIdentity>, source: ActorEventSource, detail: ActorDetail) {
    let key = identity_key(identity);
    if lock(actors()).contains_key(&key) {
        if let Some(runtime) = &source.runtime {
            runtime.latch_state_failure("actor.lifecycle-already-started");
        }
        return;
    }
    let (actor_id, session) = match next_actor_id() {
        Ok(identity) => identity,
        Err(code) => {
            if let Some(runtime) = &source.runtime {
                runtime.latch_state_failure(code);
            }
            return;
        }
    };
    let runtime = source.runtime.clone();
    let producer = match ActorLifecycleProducer::start(source, actor_id, session, detail) {
        Ok(producer) => Arc::new(producer),
        Err(error) => {
            if let Some(runtime) = runtime {
                runtime.latch_diagnostic_failure(error);
            }
            return;
        }
    };
    let previous = lock(actors()).insert(key, Arc::clone(&producer));
    if let Some(previous) = previous {
        lock(actors()).insert(key, previous);
        producer.report_state_error(runtime.as_ref(), "actor.lifecycle-install-raced");
        let transition = {
            let mut state = lock(&producer.state);
            begin_finish(
                &mut state,
                ActorSpanTerminal {
                    outcome: SpanOutcome::Failed,
                    error_code: Some(CAST_FAILED),
                },
            )
        };
        if let Some(terminal) = transition {
            producer.finish(runtime.as_ref(), terminal);
        }
    }
}

fn apply_transition(
    identity: &ActorIdentity,
    producer: &Arc<ActorLifecycleProducer>,
    fallback_runtime: Option<&Arc<RuntimeLifecycleProducer>>,
    transition: Transition,
) {
    if let Some(code) = transition.state_error {
        producer.report_state_error(fallback_runtime, code);
    }
    let Some(terminal) = transition.terminal else {
        return;
    };
    producer.finish(fallback_runtime, terminal);
    let key = address(identity);
    let mut active = lock(actors());
    if active
        .get(&key)
        .is_some_and(|current| Arc::ptr_eq(current, producer))
    {
        active.remove(&key);
    }
}

fn source_for_production(production: &ProductionState) -> Option<ActorEventSource> {
    if let Some(runtime) = runtime_for_production(production) {
        return Some(ActorEventSource {
            context: runtime.context(),
            containing_span_id: Some(runtime.run_span_id()),
            runtime: Some(runtime),
        });
    }
    current_production_construction().map(|construction| ActorEventSource {
        context: construction.context(),
        containing_span_id: Some(construction.construct_span_id()),
        runtime: None,
    })
}

fn runtime_for_production(production: &ProductionState) -> Option<Arc<RuntimeLifecycleProducer>> {
    lock(&production.active)
        .upgrade()
        .and_then(|binding| runtime_producer::producer_for_binding(&binding))
}

fn actor_detail(
    actor_type: &Bound<'_, PyType>,
    name: &Bound<'_, PyString>,
) -> Result<ActorDetail, &'static str> {
    let display_name = name
        .to_str()
        .map(str::to_owned)
        .map_err(|_| "actor.display-name-not-utf8")?;
    let actor_type = actor_type
        .name()
        .and_then(|name| name.to_str().map(str::to_owned))
        .map_err(|_| "actor.type-name-not-utf8")?;
    Ok(ActorDetail::new(display_name, actor_type))
}

fn next_actor_id() -> Result<(RunLocalId, AgentSessionDiagnosticContext), &'static str> {
    let value = NEXT_ACTOR_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .map_err(|_| "actor.identifier-exhausted")?;
    let actor_id =
        RunLocalId::parse(&format!("actor-{value}")).map_err(|_| "actor.identifier-invalid")?;
    let session =
        AgentSessionDiagnosticContext::new(actor_id.as_str(), format!("agent-session-{value}"));
    Ok((actor_id, session))
}

fn actor_scope(actor_id: RunLocalId) -> DiagnosticScope {
    DiagnosticScope::new(None, Some(actor_id), None, None, None, None, None)
}

fn is_cancelled_error(error: &PyErr) -> bool {
    Python::attach(|py| {
        py.import("asyncio")
            .and_then(|asyncio| asyncio.getattr("CancelledError"))
            .is_ok_and(|cancelled| error.is_instance(py, &cancelled))
    })
}

fn identity_key(identity: &Arc<ActorIdentity>) -> usize {
    Arc::as_ptr(identity).addr()
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
    use std::{fmt, time::Instant};

    use pyo3::{exceptions::PyRuntimeError, prelude::*};
    use troupe_diagnostics_core::{
        detail::{ProductionConstructDetail, SpanStartDetail},
        event::DiagnosticEvent,
        hub::{
            AcceptedDiagnosticEvent, AdmissionReservation, AdmissionReserver, AdmissionSize,
            DeliveryFailure, LiveEventNotifier, MandatoryDurableReserver, ProductionDiagnosticHub,
        },
        id::CanonicalUuid,
        kinds::{InstantKind, SpanKind},
        time::RunClock,
    };
    use uuid::Uuid;

    use crate::{
        diagnostic_runtime::runtime_producer::{self, RuntimeHook},
        orchestration::{
            actor::{ActorCapability, enter_actor_permit},
            actor_registry::NameKey,
            runtime::RuntimeCore,
            scene_context::RunBinding,
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
    struct RecordingError;

    impl fmt::Display for RecordingError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("injected Actor diagnostic admission failure")
        }
    }

    impl std::error::Error for RecordingError {}

    struct RecordingReserver(EventLog);

    impl AdmissionReserver for RecordingReserver {
        type Error = RecordingError;
        type Reservation = RecordingReservation;

        fn try_reserve(&mut self, _size: AdmissionSize) -> Result<Self::Reservation, Self::Error> {
            Ok(RecordingReservation(self.0.clone()))
        }
    }

    impl MandatoryDurableReserver for RecordingReserver {}

    struct IgnoreLive;

    impl LiveEventNotifier for IgnoreLive {
        fn notify(&mut self, _event: AcceptedDiagnosticEvent) -> Result<(), DeliveryFailure> {
            Ok(())
        }
    }

    fn context() -> (DiagnosticRunContext, EventLog) {
        let log = EventLog::default();
        let hub = Arc::new(ProductionDiagnosticHub::production(
            CanonicalUuid::new(Uuid::new_v4()),
            RecordingReserver(log.clone()),
            Box::new(IgnoreLive),
        ));
        (
            DiagnosticRunContext::with_hub(hub, RunClock::from_origin(Instant::now())),
            log,
        )
    }

    struct LiveActor {
        identity: Arc<ActorIdentity>,
        capability: Arc<ActorCapability>,
        handles: Vec<Py<ActorHandle>>,
    }

    fn cast_success(py: Python<'_>, production: &Arc<ProductionState>, name: &str) -> LiveActor {
        let actor_type = py.get_type::<Actor>();
        let name = PyString::new(py, name);
        let reservation = production.reserve_name(&name).expect("reserve Actor name");
        let identity = Arc::clone(reservation.identity());
        cast_started(production, &identity, &actor_type, &name);

        let production_object = py.None();
        let (construction, permit) = enter_actor_permit(
            &actor_type,
            &name,
            production_object.bind(py),
            Arc::clone(&identity),
        );
        let actor = actor_type
            .call0()
            .expect("construct Actor")
            .cast_into::<Actor>()
            .expect("cast Actor result");
        drop(permit);
        assert!(construction.was_consumed());
        let capability = Arc::new(ActorCapability::new(
            actor.clone().unbind(),
            &name,
            NameKey::from_python(&name).expect("Actor name key"),
            Arc::clone(&identity),
            Arc::downgrade(production),
        ));
        let node = Py::new(py, ActorCapabilityNode::new(Arc::clone(&capability)))
            .expect("create capability node");
        capability
            .attach_node(node.bind(py))
            .expect("attach capability node");
        actor.borrow().attach_capability(&capability);
        let handle = Py::new(py, ActorHandle::from_node(node)).expect("create Actor handle");
        reservation.commit(&capability);
        cast_finished(production, &identity, &actor_type, &name, Ok(&handle));
        LiveActor {
            identity,
            capability,
            handles: vec![handle],
        }
    }

    fn close_runtime(
        core: &Arc<RuntimeCore>,
        binding: &Arc<RunBinding>,
        permit: crate::orchestration::runtime::RunPermit,
    ) {
        core.request_shutdown();
        runtime_producer::observe_binding(binding, RuntimeHook::ProductionStopEntered, None);
        runtime_producer::observe_binding(binding, RuntimeHook::ProductionStopReturned, None);
        runtime_producer::observe_binding(binding, RuntimeHook::RunLifecycleReturned, None);
        drop(permit);
    }

    #[test]
    fn constructor_cast_is_contained_without_parenting_the_long_lived_handle_span() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let (context, log) = context();
            let construct_span_id = context
                .start_span(
                    DiagnosticScope::new(None, None, None, None, None, None, None),
                    SpanStartDetail::ProductionConstruct(ProductionConstructDetail::new(
                        "production".to_owned(),
                        "Production".to_owned(),
                    )),
                    None,
                )
                .expect("start construction span");
            let identity = Arc::new(ActorIdentity);
            start_actor(
                &identity,
                ActorEventSource {
                    context,
                    containing_span_id: Some(construct_span_id),
                    runtime: None,
                },
                ActorDetail::new("worker".to_owned(), "Actor".to_owned()),
            );
            let actor_type = py.get_type::<Actor>();
            let name = PyString::new(py, "worker");
            let error = PyRuntimeError::new_err("secret profile token");
            cast_finished(
                &ProductionState::new(),
                &identity,
                &actor_type,
                &name,
                Err(&error),
            );

            let events = log.events();
            assert_eq!(events.len(), 4);
            let DiagnosticEvent::InstantOccurred(cast) = events[1].event() else {
                panic!("expected actor.cast")
            };
            assert_eq!(cast.instant_kind(), InstantKind::ActorCast);
            assert_eq!(cast.containing_span_id(), Some(construct_span_id));
            assert!(cast.header().scope().session_generation().is_none());
            let InstantDetail::ActorCast(detail) = cast.detail() else {
                panic!("expected Actor cast detail")
            };
            assert_eq!(detail.display_name(), "worker");
            assert_eq!(detail.actor_type(), "Actor");
            let DiagnosticEvent::SpanStarted(handle) = events[2].event() else {
                panic!("expected Actor handle span")
            };
            assert_eq!(handle.span_kind(), SpanKind::ActorHandleLifetime);
            assert_eq!(handle.parent_span_id(), None);
            assert_eq!(handle.header().scope(), cast.header().scope());
            let DiagnosticEvent::SpanFinished(finish) = events[3].event() else {
                panic!("expected Actor handle finish")
            };
            assert_eq!(finish.outcome(), SpanOutcome::Failed);
            assert_eq!(finish.error_code(), Some(CAST_FAILED));
            assert!(events.iter().all(|event| {
                !String::from_utf8_lossy(event.canonical_bytes()).contains("secret profile token")
            }));
        });
    }

    #[test]
    fn cancelled_cast_has_a_stable_terminal_without_exception_payload() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let (context, log) = context();
            let identity = Arc::new(ActorIdentity);
            start_actor(
                &identity,
                ActorEventSource {
                    context,
                    containing_span_id: None,
                    runtime: None,
                },
                ActorDetail::new("cancelled".to_owned(), "Actor".to_owned()),
            );
            let actor_type = py.get_type::<Actor>();
            let name = PyString::new(py, "cancelled");
            let cancelled = py
                .import("asyncio")
                .and_then(|asyncio| asyncio.getattr("CancelledError"))
                .and_then(|cancelled| cancelled.call1(("secret cancellation payload",)))
                .map(PyErr::from_value)
                .expect("construct CancelledError");
            cast_finished(
                &ProductionState::new(),
                &identity,
                &actor_type,
                &name,
                Err(&cancelled),
            );

            let events = log.events();
            assert_eq!(events.len(), 3);
            let DiagnosticEvent::SpanFinished(finish) = events[2].event() else {
                panic!("expected cancelled Actor handle finish")
            };
            assert_eq!(finish.outcome(), SpanOutcome::Cancelled);
            assert_eq!(finish.error_code(), Some(CAST_CANCELLED));
            assert!(events.iter().all(|event| {
                !String::from_utf8_lossy(event.canonical_bytes())
                    .contains("secret cancellation payload")
            }));
        });
    }

    #[test]
    fn handles_share_actor_identity_and_finish_only_after_registry_detaches() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let production = Arc::new(ProductionState::new());
            let binding = Arc::new(RunBinding::new_for_test(py).expect("create Run binding"));
            production
                .bind_for_test(&binding)
                .expect("bind test Production");
            let core = Arc::new(RuntimeCore::new());
            let permit = core.begin().expect("start test runtime");
            let (context, log) = context();
            let runtime = runtime_producer::install(&core, &binding, context)
                .expect("install runtime diagnostics");
            runtime_producer::run_started(&core, &binding);
            runtime_producer::observe_binding(&binding, RuntimeHook::ProductionStartEntered, None);
            runtime_producer::observe_binding(&binding, RuntimeHook::ProductionStartReturned, None);

            let mut actor = cast_success(py, &production, "shared");
            let snapshot = lineage_snapshot(&actor.capability).expect("Actor lineage snapshot");
            assert_eq!(
                snapshot.scope().actor_id(),
                Some(
                    snapshot
                        .scope()
                        .actor_id()
                        .expect("Actor ID remains present")
                )
            );
            assert!(snapshot.scope().session_generation().is_none());
            let query_node = actor
                .capability
                .node(py)
                .expect("query capability node")
                .expect("live capability node");
            actor.handles.push(
                Py::new(py, ActorHandle::from_node(query_node)).expect("create query handle"),
            );

            let first = actor.handles.remove(0);
            drop(first);
            py.import("gc")
                .and_then(|gc| gc.call_method0("collect"))
                .expect("collect first handle");
            assert!(lineage_snapshot(&actor.capability).is_some());

            let actor_id = snapshot.scope().actor_id().expect("Actor ID").clone();
            drop(actor.capability);
            drop(actor.handles);
            py.import("gc")
                .and_then(|gc| gc.call_method0("collect"))
                .expect("collect final handle");
            assert!(lock(actors()).get(&identity_key(&actor.identity)).is_none());

            let events = log.events();
            let actor_events = events
                .iter()
                .filter(|event| event.event().header().scope().actor_id() == Some(&actor_id))
                .collect::<Vec<_>>();
            assert_eq!(actor_events.len(), 3);
            assert!(matches!(
                actor_events[0].event(),
                DiagnosticEvent::InstantOccurred(event)
                    if event.instant_kind() == InstantKind::ActorCast
                        && event.containing_span_id() == Some(runtime.run_span_id())
            ));
            assert!(matches!(
                actor_events[1].event(),
                DiagnosticEvent::SpanStarted(event)
                    if event.span_kind() == SpanKind::ActorHandleLifetime
                        && event.parent_span_id().is_none()
            ));
            assert!(matches!(
                actor_events[2].event(),
                DiagnosticEvent::SpanFinished(event)
                    if event.outcome() == SpanOutcome::Completed && event.error_code().is_none()
            ));
            assert!(runtime.failure().is_none());
            close_runtime(&core, &binding, permit);
        });
    }

    #[test]
    fn concurrent_and_reused_names_keep_distinct_actor_scopes() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let production = Arc::new(ProductionState::new());
            let binding = Arc::new(RunBinding::new_for_test(py).expect("create Run binding"));
            production
                .bind_for_test(&binding)
                .expect("bind test Production");
            let core = Arc::new(RuntimeCore::new());
            let permit = core.begin().expect("start test runtime");
            let (context, log) = context();
            let runtime = runtime_producer::install(&core, &binding, context)
                .expect("install runtime diagnostics");
            runtime_producer::run_started(&core, &binding);
            runtime_producer::observe_binding(&binding, RuntimeHook::ProductionStartEntered, None);
            runtime_producer::observe_binding(&binding, RuntimeHook::ProductionStartReturned, None);

            let left = cast_success(py, &production, "left");
            let right = cast_success(py, &production, "right");
            let left_id = lineage_snapshot(&left.capability)
                .expect("left snapshot")
                .scope()
                .actor_id()
                .expect("left Actor ID")
                .clone();
            let right_id = lineage_snapshot(&right.capability)
                .expect("right snapshot")
                .scope()
                .actor_id()
                .expect("right Actor ID")
                .clone();
            assert_ne!(left_id, right_id);

            drop(right.capability);
            drop(right.handles);
            drop(left.capability);
            drop(left.handles);
            py.import("gc")
                .and_then(|gc| gc.call_method0("collect"))
                .expect("collect Actors");

            let replacement = cast_success(py, &production, "left");
            let replacement_id = lineage_snapshot(&replacement.capability)
                .expect("replacement snapshot")
                .scope()
                .actor_id()
                .expect("replacement Actor ID")
                .clone();
            assert_ne!(replacement_id, left_id);
            drop(replacement.capability);
            drop(replacement.handles);
            py.import("gc")
                .and_then(|gc| gc.call_method0("collect"))
                .expect("collect replacement");

            let events = log.events();
            for actor_id in [&left_id, &right_id, &replacement_id] {
                let scoped = events
                    .iter()
                    .filter(|event| event.event().header().scope().actor_id() == Some(actor_id))
                    .collect::<Vec<_>>();
                assert_eq!(scoped.len(), 3);
                assert!(matches!(
                    scoped.as_slice(),
                    [cast, start, finish]
                        if matches!(cast.event(), DiagnosticEvent::InstantOccurred(_))
                            && matches!(start.event(), DiagnosticEvent::SpanStarted(_))
                            && matches!(finish.event(), DiagnosticEvent::SpanFinished(_))
                ));
            }
            assert!(runtime.failure().is_none());
            close_runtime(&core, &binding, permit);
        });
    }

    #[test]
    fn disabled_producer_is_a_noop() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let production = ProductionState::new();
            let identity = Arc::new(ActorIdentity);
            let actor_type = py.get_type::<Actor>();
            let name = PyString::new(py, "disabled");
            cast_started(&production, &identity, &actor_type, &name);
            let error = PyRuntimeError::new_err("ignored payload");
            cast_finished(&production, &identity, &actor_type, &name, Err(&error));
            assert!(lock(actors()).get(&identity_key(&identity)).is_none());
        });
    }
}
