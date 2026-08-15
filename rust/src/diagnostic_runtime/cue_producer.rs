#[cfg(not(test))]
use std::sync::Arc;

use pyo3::types::PyString;
use pyo3::{Py, PyErr};
#[cfg(not(test))]
use troupe_diagnostics_core::{event::DiagnosticScope, scalar::SchemaU64};

#[cfg(not(test))]
use crate::diagnostic_runtime::load_producer::DiagnosticRunContext;
#[cfg(not(test))]
use crate::diagnostic_runtime::runtime_producer::RuntimeLifecycleProducer;
use crate::orchestration::mailbox::CueOperation;
#[cfg(not(test))]
use crate::orchestration::scene_context::CuedScope;
use crate::orchestration::scene_context::{RunBinding, SceneScope};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CueHook {
    Admitted,
    Dispatched,
    CancelRequested,
    CallerFinished,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CueMailboxHook {
    Enqueued,
    Retired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CueTerminalOutcome {
    Completed,
    Failed,
    Cancelled,
    CleanupFailed,
}

#[cfg(not(test))]
#[derive(Clone)]
pub(crate) struct CueLineageSnapshot {
    runtime: Arc<RuntimeLifecycleProducer>,
    context: DiagnosticRunContext,
    cue_scope: DiagnosticScope,
    containing_span_id: SchemaU64,
}

#[cfg(not(test))]
impl CueLineageSnapshot {
    pub(crate) fn runtime(&self) -> &Arc<RuntimeLifecycleProducer> {
        &self.runtime
    }

    pub(crate) fn context(&self) -> DiagnosticRunContext {
        self.context.clone()
    }

    pub(crate) fn cue_scope(&self) -> &DiagnosticScope {
        &self.cue_scope
    }

    pub(crate) const fn containing_span_id(&self) -> SchemaU64 {
        self.containing_span_id
    }
}

#[inline]
#[cfg(not(test))]
pub(crate) fn lineage_snapshot(cued: &Arc<CuedScope>) -> Option<CueLineageSnapshot> {
    active::lineage_snapshot(cued)
}

#[inline]
pub(crate) fn created(_id: &Py<PyString>, _source: &Py<PyString>) {}

#[inline]
pub(crate) fn admission_started(_binding: &RunBinding, _scene: &SceneScope) {}

#[inline]
pub(crate) fn observe(operation: &CueOperation, hook: CueHook) {
    #[cfg(not(test))]
    active::observe(operation, hook);
    #[cfg(test)]
    let _ = (operation, hook);
}

#[inline]
pub(crate) fn mailbox_changed(
    operation: &CueOperation,
    hook: CueMailboxHook,
    queued: usize,
    running: bool,
) {
    #[cfg(not(test))]
    active::mailbox_changed(operation, hook, queued, running);
    #[cfg(test)]
    let _ = (operation, hook, queued, running);
}

#[inline]
pub(crate) fn terminal(
    operation: &CueOperation,
    outcome: CueTerminalOutcome,
    error: impl FnOnce() -> Option<PyErr>,
) {
    #[cfg(not(test))]
    active::terminal(operation, outcome, error);
    #[cfg(test)]
    let _ = (operation, outcome, error);
}

#[cfg(not(test))]
mod active {
    use std::{
        collections::{HashMap, hash_map::Entry},
        sync::{
            Arc, Mutex, MutexGuard, OnceLock,
            atomic::{AtomicU64, Ordering},
        },
    };

    use pyo3::PyErr;
    use troupe_diagnostics_core::{
        detail::{EmptyDetail, InstantDetail, SpanStartDetail},
        event::{CausalLink, DiagnosticScope},
        id::RunLocalId,
        kinds::{CausalRelation, CounterKind, SpanOutcome},
        scalar::SchemaU64,
    };

    use super::{CueHook, CueLineageSnapshot, CueMailboxHook, CueOperation, CueTerminalOutcome};
    use crate::diagnostic_runtime::{
        actor_producer, effect_producer,
        load_producer::{DiagnosticProducerError, DiagnosticRunContext},
        runtime_producer::{self, RuntimeLifecycleProducer},
        scene_producer,
    };
    use crate::orchestration::scene_context::CuedScope;

    const CUE_CANCELLED: &str = "cue-cancelled";
    const CUE_DISPATCH_FAILED: &str = "cue-dispatch-failed";
    const CUE_EXECUTION_FAILED: &str = "cue-execution-failed";
    const CUE_CLEANUP_FAILED: &str = "cue-cleanup-failed";

    static NEXT_CUE_ID: AtomicU64 = AtomicU64::new(1);

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum CuePhase {
        Starting,
        Admitted {
            admitted_sequence: SchemaU64,
        },
        Enqueued {
            enqueued_sequence: SchemaU64,
            wait_span_id: SchemaU64,
        },
        Dispatched {
            execution_span_id: SchemaU64,
        },
        Terminal {
            terminal_sequence: Option<SchemaU64>,
        },
    }

    struct CueLifecycleState {
        phase: CuePhase,
        cancel_sequence: Option<SchemaU64>,
        caller_finished: bool,
        producer_failed: bool,
    }

    struct CueLifecycleProducer {
        runtime: Arc<RuntimeLifecycleProducer>,
        context: DiagnosticRunContext,
        cue_scope: DiagnosticScope,
        actor_scope: DiagnosticScope,
        scene_span_id: SchemaU64,
        _owner: Arc<CuedScope>,
        state: Mutex<CueLifecycleState>,
    }

    impl CueLifecycleProducer {
        fn prepare(operation: &CueOperation) -> Result<Option<Self>, &'static str> {
            let Some(binding) = operation.diagnostic_binding() else {
                return Ok(None);
            };
            let Some(runtime) = runtime_producer::producer_for_binding(&binding) else {
                return Ok(None);
            };
            let scene = operation
                .diagnostic_scene()
                .ok_or("cue.scene-lineage-unavailable")?;
            let actor = operation
                .diagnostic_actor()
                .ok_or("cue.actor-lineage-unavailable")?;
            let scene_lineage = scene_producer::snapshot_for_scene(&scene)
                .ok_or("cue.scene-lineage-unavailable")?;
            let actor_lineage =
                actor_producer::lineage_snapshot(&actor).ok_or("cue.actor-lineage-unavailable")?;
            let scene_id = scene_lineage
                .scope()
                .scene_id()
                .cloned()
                .ok_or("cue.scene-identifier-unavailable")?;
            let actor_id = actor_lineage
                .scope()
                .actor_id()
                .cloned()
                .ok_or("cue.actor-identifier-unavailable")?;
            let cue_id = next_cue_id()?;
            let cue_scope = DiagnosticScope::new(
                Some(scene_id),
                Some(actor_id.clone()),
                Some(cue_id),
                None,
                None,
                None,
                None,
            );
            let actor_scope =
                DiagnosticScope::new(None, Some(actor_id), None, None, None, None, None);
            Ok(Some(Self {
                context: runtime.context(),
                runtime,
                cue_scope,
                actor_scope,
                scene_span_id: scene_lineage.scene_span_id(),
                _owner: Arc::clone(operation.diagnostic_cued()),
                state: Mutex::new(CueLifecycleState {
                    phase: CuePhase::Starting,
                    cancel_sequence: None,
                    caller_finished: false,
                    producer_failed: false,
                }),
            }))
        }

        fn admitted(&self) {
            let mut state = lock(&self.state);
            if state.phase != CuePhase::Starting {
                self.fail_state(&mut state, "cue.admission-transition-invalid");
                return;
            }
            let admitted_sequence = match self.context.emit_instant_with_causes(
                self.cue_scope.clone(),
                InstantDetail::CueAdmitted(EmptyDetail::new()),
                Some(self.scene_span_id),
                Vec::new(),
            ) {
                Ok(sequence) => sequence,
                Err(error) => {
                    self.fail_diagnostic(&mut state, error);
                    return;
                }
            };
            if let Err(error) = self.context.emit_counter(
                self.cue_scope.clone(),
                CounterKind::CueActive,
                SchemaU64::new(1),
                follows_from(admitted_sequence),
            ) {
                self.fail_diagnostic(&mut state, error);
                return;
            }
            state.phase = CuePhase::Admitted { admitted_sequence };
        }

        fn enqueued(&self, queued: usize, running: bool) {
            let mut state = lock(&self.state);
            if state.producer_failed {
                return;
            }
            let CuePhase::Admitted { admitted_sequence } = state.phase else {
                self.fail_state(&mut state, "cue.enqueue-transition-invalid");
                return;
            };
            if !running {
                self.fail_state(&mut state, "cue.mailbox-running-snapshot-invalid");
                return;
            }
            let enqueued_sequence = match self.context.emit_instant_with_causes(
                self.cue_scope.clone(),
                InstantDetail::CueEnqueued(EmptyDetail::new()),
                Some(self.scene_span_id),
                follows_from(admitted_sequence),
            ) {
                Ok(sequence) => sequence,
                Err(error) => {
                    self.fail_diagnostic(&mut state, error);
                    return;
                }
            };
            let wait_span_id = match self.context.start_span_with_causes(
                self.cue_scope.clone(),
                SpanStartDetail::CueMailboxWait(EmptyDetail::new()),
                Some(self.scene_span_id),
                follows_from(enqueued_sequence),
            ) {
                Ok(span_id) => span_id,
                Err(error) => {
                    self.fail_diagnostic(&mut state, error);
                    return;
                }
            };
            let depth = match mailbox_depth(queued, running) {
                Ok(depth) => depth,
                Err(code) => {
                    self.fail_state(&mut state, code);
                    return;
                }
            };
            if let Err(error) = self.context.emit_counter(
                self.actor_scope.clone(),
                CounterKind::ActorMailboxDepth,
                depth,
                follows_from(enqueued_sequence),
            ) {
                self.fail_diagnostic(&mut state, error);
                return;
            }
            state.phase = CuePhase::Enqueued {
                enqueued_sequence,
                wait_span_id,
            };
        }

        fn dispatched(&self) {
            let mut state = lock(&self.state);
            if state.producer_failed {
                return;
            }
            let CuePhase::Enqueued {
                enqueued_sequence,
                wait_span_id,
            } = state.phase
            else {
                self.fail_state(&mut state, "cue.dispatch-transition-invalid");
                return;
            };
            let dispatched_sequence = match self.context.emit_instant_with_causes(
                self.cue_scope.clone(),
                InstantDetail::CueDispatched(EmptyDetail::new()),
                Some(wait_span_id),
                vec![CausalLink::new(enqueued_sequence, CausalRelation::Dispatch)],
            ) {
                Ok(sequence) => sequence,
                Err(error) => {
                    self.fail_diagnostic(&mut state, error);
                    return;
                }
            };
            if let Err(error) = self.context.finish_span_with_causes(
                self.cue_scope.clone(),
                wait_span_id,
                SpanOutcome::Completed,
                None,
                vec![CausalLink::new(
                    dispatched_sequence,
                    CausalRelation::Dispatch,
                )],
            ) {
                self.fail_diagnostic(&mut state, error);
                return;
            }
            let execution_span_id = match self.context.start_span_with_causes(
                self.cue_scope.clone(),
                SpanStartDetail::CueExecution(EmptyDetail::new()),
                Some(self.scene_span_id),
                vec![CausalLink::new(
                    dispatched_sequence,
                    CausalRelation::Dispatch,
                )],
            ) {
                Ok(span_id) => span_id,
                Err(error) => {
                    self.fail_diagnostic(&mut state, error);
                    return;
                }
            };
            state.phase = CuePhase::Dispatched { execution_span_id };
        }

        fn cancel_requested(&self) {
            let mut state = lock(&self.state);
            if state.producer_failed {
                return;
            }
            if state.cancel_sequence.is_some() || matches!(state.phase, CuePhase::Terminal { .. }) {
                self.fail_state(&mut state, "cue.cancel-request-transition-invalid");
                return;
            }
            let (source, containing_span_id) = match state.phase {
                CuePhase::Admitted { admitted_sequence } => {
                    (admitted_sequence, Some(self.scene_span_id))
                }
                CuePhase::Enqueued {
                    enqueued_sequence,
                    wait_span_id,
                } => (enqueued_sequence, Some(wait_span_id)),
                CuePhase::Dispatched { execution_span_id } => {
                    (execution_span_id, Some(execution_span_id))
                }
                CuePhase::Starting | CuePhase::Terminal { .. } => {
                    self.fail_state(&mut state, "cue.cancel-request-transition-invalid");
                    return;
                }
            };
            match self.context.emit_instant_with_causes(
                self.cue_scope.clone(),
                InstantDetail::CueCancelRequested(EmptyDetail::new()),
                containing_span_id,
                vec![CausalLink::new(source, CausalRelation::Handoff)],
            ) {
                Ok(sequence) => state.cancel_sequence = Some(sequence),
                Err(error) => self.fail_diagnostic(&mut state, error),
            }
        }

        fn caller_finished(&self) {
            let mut state = lock(&self.state);
            if state.caller_finished {
                self.fail_state(&mut state, "cue.caller-finish-transition-invalid");
                return;
            }
            state.caller_finished = true;
        }

        fn terminal(&self, outcome: CueTerminalOutcome, has_error: bool) {
            let mut state = lock(&self.state);
            if state.producer_failed {
                state.phase = CuePhase::Terminal {
                    terminal_sequence: None,
                };
                return;
            }
            if outcome != CueTerminalOutcome::Completed && !has_error {
                self.fail_state(&mut state, "cue.terminal-error-unavailable");
                state.phase = CuePhase::Terminal {
                    terminal_sequence: None,
                };
                return;
            }
            let terminal = terminal_span(outcome, state.phase);
            let (span_id, fallback_source) = match state.phase {
                CuePhase::Enqueued {
                    enqueued_sequence,
                    wait_span_id,
                } => (Some(wait_span_id), enqueued_sequence),
                CuePhase::Dispatched { execution_span_id } => {
                    (Some(execution_span_id), execution_span_id)
                }
                CuePhase::Admitted { admitted_sequence } => (None, admitted_sequence),
                CuePhase::Starting | CuePhase::Terminal { .. } => {
                    self.fail_state(&mut state, "cue.terminal-transition-invalid");
                    return;
                }
            };
            if outcome == CueTerminalOutcome::Completed
                && !matches!(state.phase, CuePhase::Dispatched { .. })
            {
                self.fail_state(&mut state, "cue.completed-before-dispatch");
                return;
            }
            let source = state.cancel_sequence.unwrap_or(fallback_source);
            let relation = if state.cancel_sequence.is_some() {
                CausalRelation::Handoff
            } else {
                CausalRelation::Return
            };
            effect_producer::cue_terminal(&self._owner, outcome, source);
            let terminal_sequence = if let Some(span_id) = span_id {
                match self.context.finish_span_with_causes(
                    self.cue_scope.clone(),
                    span_id,
                    terminal.outcome,
                    terminal.error_code.map(str::to_owned),
                    vec![CausalLink::new(source, relation)],
                ) {
                    Ok(sequence) => sequence,
                    Err(error) => {
                        self.fail_diagnostic(&mut state, error);
                        return;
                    }
                }
            } else {
                source
            };
            let active_zero_sequence = match self.context.emit_counter(
                self.cue_scope.clone(),
                CounterKind::CueActive,
                SchemaU64::new(0),
                follows_from(terminal_sequence),
            ) {
                Ok(sequence) => sequence,
                Err(error) => {
                    self.fail_diagnostic(&mut state, error);
                    return;
                }
            };
            state.phase = CuePhase::Terminal {
                terminal_sequence: Some(active_zero_sequence),
            };
        }

        fn retired(&self, queued: usize, running: bool) {
            let mut state = lock(&self.state);
            let CuePhase::Terminal { terminal_sequence } = state.phase else {
                self.fail_state(&mut state, "cue.mailbox-retire-transition-invalid");
                return;
            };
            if state.producer_failed {
                return;
            }
            let depth = match mailbox_depth(queued, running) {
                Ok(depth) => depth,
                Err(code) => {
                    self.fail_state(&mut state, code);
                    return;
                }
            };
            let caused_by = terminal_sequence.map(follows_from).unwrap_or_default();
            if let Err(error) = self.context.emit_counter(
                self.actor_scope.clone(),
                CounterKind::ActorMailboxDepth,
                depth,
                caused_by,
            ) {
                self.fail_diagnostic(&mut state, error);
            }
        }

        fn fail_diagnostic(&self, state: &mut CueLifecycleState, error: DiagnosticProducerError) {
            state.producer_failed = true;
            self.runtime.latch_diagnostic_failure(error);
        }

        fn fail_state(&self, state: &mut CueLifecycleState, code: &'static str) {
            state.producer_failed = true;
            self.runtime.latch_state_failure(code);
        }
    }

    #[derive(Clone, Copy)]
    struct TerminalSpan {
        outcome: SpanOutcome,
        error_code: Option<&'static str>,
    }

    pub(super) fn observe(operation: &CueOperation, hook: CueHook) {
        match hook {
            CueHook::Admitted => admitted(operation),
            CueHook::Dispatched => with_producer(operation, |producer| producer.dispatched()),
            CueHook::CancelRequested => {
                with_producer(operation, |producer| producer.cancel_requested());
            }
            CueHook::CallerFinished => {
                effect_producer::caller_finished(operation.diagnostic_cued());
                if let Some(producer) = producer(operation) {
                    producer.caller_finished();
                }
            }
        }
    }

    pub(super) fn mailbox_changed(
        operation: &CueOperation,
        hook: CueMailboxHook,
        queued: usize,
        running: bool,
    ) {
        match hook {
            CueMailboxHook::Enqueued => {
                with_producer(operation, |producer| producer.enqueued(queued, running));
            }
            CueMailboxHook::Retired => {
                if let Some(producer) = producer(operation) {
                    producer.retired(queued, running);
                    remove(operation, &producer);
                } else {
                    latch_missing(operation, "cue.mailbox-retire-without-lifecycle");
                }
            }
        }
    }

    pub(super) fn terminal(
        operation: &CueOperation,
        outcome: CueTerminalOutcome,
        error: impl FnOnce() -> Option<PyErr>,
    ) {
        let Some(producer) = producer(operation) else {
            latch_missing(operation, "cue.terminal-without-lifecycle");
            return;
        };
        let has_error = outcome == CueTerminalOutcome::Completed || error().is_some();
        producer.terminal(outcome, has_error);
        if operation.diagnostic_actor().is_none() {
            remove(operation, &producer);
        }
    }

    pub(super) fn lineage_snapshot(cued: &Arc<CuedScope>) -> Option<CueLineageSnapshot> {
        let producer = lock(producers()).get(&Arc::as_ptr(cued).addr()).cloned()?;
        let containing_span_id = {
            let state = lock(&producer.state);
            match state.phase {
                CuePhase::Dispatched { execution_span_id } => execution_span_id,
                CuePhase::Starting
                | CuePhase::Admitted { .. }
                | CuePhase::Enqueued { .. }
                | CuePhase::Terminal { .. } => return None,
            }
        };
        Some(CueLineageSnapshot {
            runtime: Arc::clone(&producer.runtime),
            context: producer.context.clone(),
            cue_scope: producer.cue_scope.clone(),
            containing_span_id,
        })
    }

    fn admitted(operation: &CueOperation) {
        let key = operation_key(operation);
        if lock(producers()).contains_key(&key) {
            latch_missing(operation, "cue.lifecycle-already-started");
            return;
        }
        let producer = match CueLifecycleProducer::prepare(operation) {
            Ok(Some(producer)) => Arc::new(producer),
            Ok(None) => return,
            Err(code) => {
                latch_missing(operation, code);
                return;
            }
        };
        {
            let mut active = lock(producers());
            match active.entry(key) {
                Entry::Vacant(entry) => {
                    entry.insert(Arc::clone(&producer));
                }
                Entry::Occupied(_) => {
                    producer
                        .runtime
                        .latch_state_failure("cue.lifecycle-install-raced");
                    return;
                }
            }
        }
        producer.admitted();
    }

    fn with_producer(operation: &CueOperation, apply: impl FnOnce(&CueLifecycleProducer)) {
        let Some(producer) = producer(operation) else {
            latch_missing(operation, "cue.lifecycle-unavailable");
            return;
        };
        apply(&producer);
    }

    fn producer(operation: &CueOperation) -> Option<Arc<CueLifecycleProducer>> {
        lock(producers()).get(&operation_key(operation)).cloned()
    }

    fn remove(operation: &CueOperation, expected: &Arc<CueLifecycleProducer>) {
        let key = operation_key(operation);
        let mut active = lock(producers());
        if active
            .get(&key)
            .is_some_and(|current| Arc::ptr_eq(current, expected))
        {
            active.remove(&key);
        }
    }

    fn latch_missing(operation: &CueOperation, code: &'static str) {
        let Some(binding) = operation.diagnostic_binding() else {
            return;
        };
        if let Some(runtime) = runtime_producer::producer_for_binding(&binding) {
            runtime.latch_state_failure(code);
        }
    }

    fn operation_key(operation: &CueOperation) -> usize {
        Arc::as_ptr(operation.diagnostic_cued()).addr()
    }

    fn next_cue_id() -> Result<RunLocalId, &'static str> {
        let value = NEXT_CUE_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| "cue.identifier-exhausted")?;
        RunLocalId::parse(&format!("cue-{value}")).map_err(|_| "cue.identifier-invalid")
    }

    fn mailbox_depth(queued: usize, running: bool) -> Result<SchemaU64, &'static str> {
        let total = queued
            .checked_add(usize::from(running))
            .ok_or("cue.mailbox-depth-overflow")?;
        let value = u64::try_from(total).map_err(|_| "cue.mailbox-depth-overflow")?;
        Ok(SchemaU64::new(value))
    }

    fn follows_from(source: SchemaU64) -> Vec<CausalLink> {
        vec![CausalLink::new(source, CausalRelation::FollowsFrom)]
    }

    fn terminal_span(outcome: CueTerminalOutcome, phase: CuePhase) -> TerminalSpan {
        match outcome {
            CueTerminalOutcome::Completed => TerminalSpan {
                outcome: SpanOutcome::Completed,
                error_code: None,
            },
            CueTerminalOutcome::Cancelled => TerminalSpan {
                outcome: SpanOutcome::Cancelled,
                error_code: Some(CUE_CANCELLED),
            },
            CueTerminalOutcome::CleanupFailed => TerminalSpan {
                outcome: SpanOutcome::Failed,
                error_code: Some(CUE_CLEANUP_FAILED),
            },
            CueTerminalOutcome::Failed => TerminalSpan {
                outcome: SpanOutcome::Failed,
                error_code: Some(if matches!(phase, CuePhase::Dispatched { .. }) {
                    CUE_EXECUTION_FAILED
                } else {
                    CUE_DISPATCH_FAILED
                }),
            },
        }
    }

    fn producers() -> &'static Mutex<HashMap<usize, Arc<CueLifecycleProducer>>> {
        static PRODUCERS: OnceLock<Mutex<HashMap<usize, Arc<CueLifecycleProducer>>>> =
            OnceLock::new();
        PRODUCERS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
