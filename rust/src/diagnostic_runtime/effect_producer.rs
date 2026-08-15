use std::sync::Arc;

use pyo3::types::PyString;
use pyo3::{Bound, Py, PyErr};
use troupe_diagnostics_core::scalar::SchemaU64;

use crate::diagnostic_runtime::cue_producer::{CueCallerOutcome, CueTerminalOutcome};
use crate::orchestration::effect::{Effect, EffectConstruction};
use crate::orchestration::scene_context::CuedScope;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EffectHook {
    Created,
    Returned,
}

#[inline]
pub(crate) fn observe(effect: &Effect, hook: EffectHook) {
    #[cfg(not(test))]
    active::observe(effect, hook);
    #[cfg(test)]
    let _ = (effect, hook);
}

#[inline]
pub(crate) fn construction_started(construction: &EffectConstruction) {
    #[cfg(not(test))]
    active::construction_started(construction);
    #[cfg(test)]
    let _ = construction;
}

#[inline]
pub(crate) fn construction_finished(
    construction: &EffectConstruction,
    outcome: Result<&Bound<'_, Effect>, &PyErr>,
) {
    #[cfg(not(test))]
    active::construction_finished(construction, outcome);
    #[cfg(test)]
    let _ = (construction, outcome);
}

#[inline]
pub(crate) fn cleared(effect: &Effect, id: Option<&Py<PyString>>, owner: Option<&Py<PyString>>) {
    #[cfg(not(test))]
    active::cleared(effect);
    let _ = (effect, id, owner);
}

#[inline]
pub(crate) fn cue_terminal(
    cued: &Arc<CuedScope>,
    outcome: CueTerminalOutcome,
    causal_source: SchemaU64,
) {
    #[cfg(not(test))]
    active::cue_terminal(cued, outcome, causal_source);
    #[cfg(test)]
    let _ = (cued, outcome, causal_source);
}

#[inline]
pub(crate) fn caller_finished(cued: &Arc<CuedScope>, outcome: CueCallerOutcome) {
    #[cfg(not(test))]
    active::caller_finished(cued, outcome);
    #[cfg(test)]
    let _ = (cued, outcome);
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

    use pyo3::types::{PyAnyMethods, PyStringMethods, PyTypeMethods};
    use pyo3::{Bound, PyErr, Python};
    use troupe_diagnostics_core::{
        detail::{EffectDetail, InstantDetail, SpanStartDetail},
        event::{CausalLink, DiagnosticScope},
        id::RunLocalId,
        kinds::{CausalRelation, SpanOutcome},
        scalar::SchemaU64,
    };

    use super::{CueCallerOutcome, CueTerminalOutcome, EffectHook};
    use crate::diagnostic_runtime::{
        cue_producer,
        load_producer::{DiagnosticProducerError, DiagnosticRunContext},
        runtime_producer::RuntimeLifecycleProducer,
    };
    use crate::orchestration::{
        effect::{Effect, EffectConstruction, EffectIdentity},
        scene_context::CuedScope,
    };

    const CONSTRUCTION_CANCELLED: &str = "effect-construction-cancelled";
    const CONSTRUCTION_FAILED: &str = "effect-construction-failed";
    const NOT_RETURNED: &str = "effect-not-returned";
    const CONSUMER_ABANDONED: &str = "effect-consumer-abandoned";
    const OWNER_CANCELLED: &str = "effect-owner-cancelled";
    const OWNER_FAILED: &str = "effect-owner-failed";
    const OWNER_CLEANUP_FAILED: &str = "effect-owner-cleanup-failed";

    static NEXT_EFFECT_ID: AtomicU64 = AtomicU64::new(1);

    #[derive(Default)]
    struct EffectLifecycleState {
        created_sequence: Option<SchemaU64>,
        construction_finished: bool,
        returned_sequence: Option<SchemaU64>,
        owner_terminal: Option<CueTerminalOutcome>,
        caller_outcome: Option<CueCallerOutcome>,
        cleared: bool,
        finish_attempted: bool,
        producer_failed: bool,
    }

    struct EffectLifecycleProducer {
        runtime: Arc<RuntimeLifecycleProducer>,
        context: DiagnosticRunContext,
        scope: DiagnosticScope,
        detail: EffectDetail,
        lifecycle_span_id: SchemaU64,
        identity_key: usize,
        owner_key: usize,
        _identity: Arc<EffectIdentity>,
        _owner: Arc<CuedScope>,
        state: Mutex<EffectLifecycleState>,
    }

    #[derive(Clone, Copy)]
    struct EffectTerminal {
        outcome: SpanOutcome,
        error_code: Option<&'static str>,
        causal_source: SchemaU64,
        relation: CausalRelation,
    }

    impl EffectTerminal {
        const fn completed(causal_source: SchemaU64) -> Self {
            Self {
                outcome: SpanOutcome::Completed,
                error_code: None,
                causal_source,
                relation: CausalRelation::FollowsFrom,
            }
        }

        const fn cancelled(
            error_code: &'static str,
            causal_source: SchemaU64,
            relation: CausalRelation,
        ) -> Self {
            Self {
                outcome: SpanOutcome::Cancelled,
                error_code: Some(error_code),
                causal_source,
                relation,
            }
        }

        const fn failed(
            error_code: &'static str,
            causal_source: SchemaU64,
            relation: CausalRelation,
        ) -> Self {
            Self {
                outcome: SpanOutcome::Failed,
                error_code: Some(error_code),
                causal_source,
                relation,
            }
        }
    }

    impl EffectLifecycleProducer {
        fn start(
            construction: &EffectConstruction,
            owner: Arc<CuedScope>,
            lineage: cue_producer::CueLineageSnapshot,
        ) -> Result<Self, &'static str> {
            let effect_id = next_effect_id()?;
            let scope = effect_scope(lineage.cue_scope(), effect_id)?;
            let detail = effect_detail(construction)?;
            let context = lineage.context();
            let lifecycle_span_id = context
                .start_span(
                    scope.clone(),
                    SpanStartDetail::EffectLifecycle(detail.clone()),
                    Some(lineage.containing_span_id()),
                )
                .map_err(|error| {
                    lineage.runtime().latch_diagnostic_failure(error);
                    "effect.lifecycle-start-failed"
                })?;
            Ok(Self {
                runtime: Arc::clone(lineage.runtime()),
                context,
                scope,
                detail,
                lifecycle_span_id,
                identity_key: identity_key(construction.identity()),
                owner_key: owner_key(&owner),
                _identity: Arc::clone(construction.identity()),
                _owner: owner,
                state: Mutex::new(EffectLifecycleState::default()),
            })
        }

        fn created(&self) -> bool {
            let mut state = lock(&self.state);
            if state.producer_failed || state.finish_attempted {
                return true;
            }
            if state.created_sequence.is_some() || state.construction_finished {
                return self.fail_state(&mut state, "effect.created-transition-invalid");
            }
            match self.context.emit_instant_with_causes(
                self.scope.clone(),
                InstantDetail::EffectCreated(self.detail.clone()),
                Some(self.lifecycle_span_id),
                caused_by(self.lifecycle_span_id, CausalRelation::FollowsFrom),
            ) {
                Ok(sequence) => {
                    state.created_sequence = Some(sequence);
                    false
                }
                Err(error) => self.fail_diagnostic(&mut state, error),
            }
        }

        fn construction_finished(&self, error: Option<&PyErr>) -> bool {
            let mut state = lock(&self.state);
            if state.producer_failed || state.finish_attempted {
                return true;
            }
            if state.construction_finished {
                return self.fail_state(
                    &mut state,
                    "effect.construction-terminal-transition-invalid",
                );
            }
            state.construction_finished = true;
            if error.is_none() {
                if state.created_sequence.is_none() {
                    return self
                        .fail_state(&mut state, "effect.construction-finished-without-create");
                }
                return false;
            }

            let causal_source = state.created_sequence.unwrap_or(self.lifecycle_span_id);
            let terminal = if error.is_some_and(is_cancelled_error) {
                EffectTerminal::cancelled(
                    CONSTRUCTION_CANCELLED,
                    causal_source,
                    CausalRelation::FollowsFrom,
                )
            } else {
                EffectTerminal::failed(
                    CONSTRUCTION_FAILED,
                    causal_source,
                    CausalRelation::FollowsFrom,
                )
            };
            self.finish(&mut state, terminal)
        }

        fn returned(&self) -> bool {
            let mut state = lock(&self.state);
            if state.producer_failed || state.finish_attempted {
                return true;
            }
            if state.returned_sequence.is_some() {
                // The runtime permits the same Effect object to occupy multiple tuple slots.
                return false;
            }
            let Some(created_sequence) = state.created_sequence else {
                return self.fail_state(&mut state, "effect.returned-without-create");
            };
            if !state.construction_finished || state.owner_terminal.is_some() {
                return self.fail_state(&mut state, "effect.returned-transition-invalid");
            }
            match self.context.emit_instant_with_causes(
                self.scope.clone(),
                InstantDetail::EffectReturned(self.detail.clone()),
                Some(self.lifecycle_span_id),
                caused_by(created_sequence, CausalRelation::Return),
            ) {
                Ok(sequence) => {
                    state.returned_sequence = Some(sequence);
                    false
                }
                Err(error) => self.fail_diagnostic(&mut state, error),
            }
        }

        fn owner_terminal(&self, outcome: CueTerminalOutcome, causal_source: SchemaU64) -> bool {
            let mut state = lock(&self.state);
            if state.producer_failed || state.finish_attempted {
                return true;
            }
            if state.owner_terminal.is_some() {
                return self.fail_state(&mut state, "effect.owner-terminal-transition-invalid");
            }
            state.owner_terminal = Some(outcome);
            match outcome {
                CueTerminalOutcome::Completed => {
                    let Some(returned_sequence) = state.returned_sequence else {
                        return self.finish(
                            &mut state,
                            EffectTerminal::failed(
                                NOT_RETURNED,
                                causal_source,
                                CausalRelation::Return,
                            ),
                        );
                    };
                    match state.caller_outcome {
                        Some(CueCallerOutcome::Consumed) => self.consume_and_finish(&mut state),
                        Some(CueCallerOutcome::Abandoned) => {
                            self.abandon_and_finish(&mut state, returned_sequence)
                        }
                        None if state.cleared => {
                            self.abandon_and_finish(&mut state, returned_sequence)
                        }
                        None => false,
                    }
                }
                CueTerminalOutcome::Cancelled => self.finish(
                    &mut state,
                    EffectTerminal::cancelled(
                        OWNER_CANCELLED,
                        causal_source,
                        CausalRelation::Handoff,
                    ),
                ),
                CueTerminalOutcome::Failed => self.finish(
                    &mut state,
                    EffectTerminal::failed(OWNER_FAILED, causal_source, CausalRelation::Return),
                ),
                CueTerminalOutcome::CleanupFailed => self.finish(
                    &mut state,
                    EffectTerminal::failed(
                        OWNER_CLEANUP_FAILED,
                        causal_source,
                        CausalRelation::Handoff,
                    ),
                ),
            }
        }

        fn caller_finished(&self, outcome: CueCallerOutcome) -> bool {
            let mut state = lock(&self.state);
            if state.producer_failed || state.finish_attempted {
                return true;
            }
            if state.caller_outcome.is_some() {
                return false;
            }
            state.caller_outcome = Some(outcome);
            if state.owner_terminal == Some(CueTerminalOutcome::Completed)
                && let Some(returned_sequence) = state.returned_sequence
            {
                match outcome {
                    CueCallerOutcome::Consumed => self.consume_and_finish(&mut state),
                    CueCallerOutcome::Abandoned => {
                        self.abandon_and_finish(&mut state, returned_sequence)
                    }
                }
            } else {
                false
            }
        }

        fn cleared(&self) -> bool {
            let mut state = lock(&self.state);
            if state.producer_failed || state.finish_attempted {
                return true;
            }
            if state.cleared {
                return false;
            }
            state.cleared = true;
            if state.owner_terminal == Some(CueTerminalOutcome::Completed)
                && let Some(returned_sequence) = state.returned_sequence
            {
                self.abandon_and_finish(&mut state, returned_sequence)
            } else {
                false
            }
        }

        fn abandon_and_finish(
            &self,
            state: &mut EffectLifecycleState,
            returned_sequence: SchemaU64,
        ) -> bool {
            self.finish(
                state,
                EffectTerminal::cancelled(
                    CONSUMER_ABANDONED,
                    returned_sequence,
                    CausalRelation::Handoff,
                ),
            )
        }

        fn consume_and_finish(&self, state: &mut EffectLifecycleState) -> bool {
            let Some(returned_sequence) = state.returned_sequence else {
                return self.fail_state(state, "effect.consumed-without-return");
            };
            let consumed_sequence = match self.context.emit_instant_with_causes(
                self.scope.clone(),
                InstantDetail::EffectConsumed(self.detail.clone()),
                Some(self.lifecycle_span_id),
                caused_by(returned_sequence, CausalRelation::Handoff),
            ) {
                Ok(sequence) => sequence,
                Err(error) => return self.fail_diagnostic(state, error),
            };
            self.finish(state, EffectTerminal::completed(consumed_sequence))
        }

        fn finish(&self, state: &mut EffectLifecycleState, terminal: EffectTerminal) -> bool {
            if state.finish_attempted {
                return self.fail_state(state, "effect.lifecycle-finish-transition-invalid");
            }
            state.finish_attempted = true;
            if let Err(error) = self.context.finish_span_with_causes(
                self.scope.clone(),
                self.lifecycle_span_id,
                terminal.outcome,
                terminal.error_code.map(str::to_owned),
                caused_by(terminal.causal_source, terminal.relation),
            ) {
                state.producer_failed = true;
                self.runtime.latch_diagnostic_failure(error);
            }
            true
        }

        fn fail_diagnostic(
            &self,
            state: &mut EffectLifecycleState,
            error: DiagnosticProducerError,
        ) -> bool {
            state.producer_failed = true;
            state.finish_attempted = true;
            self.runtime.latch_diagnostic_failure(error);
            true
        }

        fn fail_state(&self, state: &mut EffectLifecycleState, code: &'static str) -> bool {
            state.producer_failed = true;
            state.finish_attempted = true;
            self.runtime.latch_state_failure(code);
            true
        }
    }

    pub(super) fn construction_started(construction: &EffectConstruction) {
        let Some(owner) = construction.diagnostic_cued() else {
            return;
        };
        let Some(lineage) = cue_producer::lineage_snapshot(&owner) else {
            return;
        };
        if lineage.runtime().failure().is_some() {
            return;
        }
        let identity_key = identity_key(construction.identity());
        if lock(producers()).contains_key(&identity_key) {
            lineage
                .runtime()
                .latch_state_failure("effect.lifecycle-already-started");
            return;
        }
        let producer = match EffectLifecycleProducer::start(construction, owner, lineage.clone()) {
            Ok(producer) => Arc::new(producer),
            Err(code) => {
                if code != "effect.lifecycle-start-failed" {
                    lineage.runtime().latch_state_failure(code);
                }
                return;
            }
        };
        let mut active = lock(producers());
        match active.entry(identity_key) {
            Entry::Vacant(entry) => {
                entry.insert(producer);
            }
            Entry::Occupied(_) => {
                producer
                    .runtime
                    .latch_state_failure("effect.lifecycle-install-raced");
            }
        }
    }

    pub(super) fn construction_finished(
        construction: &EffectConstruction,
        outcome: Result<&Bound<'_, Effect>, &PyErr>,
    ) {
        let key = identity_key(construction.identity());
        let Some(producer) = producer(key) else {
            return;
        };
        if let Ok(effect) = outcome
            && identity_key(effect.borrow().diagnostic_identity()) != key
        {
            producer
                .runtime
                .latch_state_failure("effect.construction-identity-changed");
            remove(key, &producer);
            return;
        }
        let should_remove = producer.construction_finished(outcome.err());
        remove_if_terminal(key, &producer, should_remove);
    }

    pub(super) fn observe(effect: &Effect, hook: EffectHook) {
        let key = identity_key(effect.diagnostic_identity());
        let Some(producer) = producer(key) else {
            if hook == EffectHook::Created
                && let Some(owner) = effect.diagnostic_cued()
                && let Some(lineage) = cue_producer::lineage_snapshot(&owner)
            {
                lineage
                    .runtime()
                    .latch_state_failure("effect.lifecycle-unavailable");
            }
            return;
        };
        let should_remove = match hook {
            EffectHook::Created => producer.created(),
            EffectHook::Returned => producer.returned(),
        };
        remove_if_terminal(key, &producer, should_remove);
    }

    pub(super) fn cleared(effect: &Effect) {
        let key = identity_key(effect.diagnostic_identity());
        let Some(producer) = producer(key) else {
            return;
        };
        let should_remove = producer.cleared();
        remove_if_terminal(key, &producer, should_remove);
    }

    pub(super) fn cue_terminal(
        cued: &Arc<CuedScope>,
        outcome: CueTerminalOutcome,
        causal_source: SchemaU64,
    ) {
        let owner_key = owner_key(cued);
        for producer in producers_for_owner(owner_key) {
            let should_remove = producer.owner_terminal(outcome, causal_source);
            remove_if_terminal(producer.identity_key, &producer, should_remove);
        }
    }

    pub(super) fn caller_finished(cued: &Arc<CuedScope>, outcome: CueCallerOutcome) {
        let owner_key = owner_key(cued);
        for producer in producers_for_owner(owner_key) {
            let should_remove = producer.caller_finished(outcome);
            remove_if_terminal(producer.identity_key, &producer, should_remove);
        }
    }

    fn effect_detail(construction: &EffectConstruction) -> Result<EffectDetail, &'static str> {
        Python::attach(|py| {
            let effect_type = construction.effect_type(py);
            let effect_type = effect_type
                .bind(py)
                .name()
                .and_then(|name| name.to_str().map(str::to_owned))
                .map_err(|_| "effect.type-name-not-utf8")?;
            Ok(EffectDetail::new(effect_type))
        })
    }

    fn effect_scope(
        cue_scope: &DiagnosticScope,
        effect_id: RunLocalId,
    ) -> Result<DiagnosticScope, &'static str> {
        let scene_id = cue_scope
            .scene_id()
            .cloned()
            .ok_or("effect.scene-identifier-unavailable")?;
        let actor_id = cue_scope
            .actor_id()
            .cloned()
            .ok_or("effect.actor-identifier-unavailable")?;
        let cue_id = cue_scope
            .cue_id()
            .cloned()
            .ok_or("effect.cue-identifier-unavailable")?;
        Ok(DiagnosticScope::new(
            Some(scene_id),
            Some(actor_id),
            Some(cue_id),
            Some(effect_id),
            None,
            None,
            None,
        ))
    }

    fn next_effect_id() -> Result<RunLocalId, &'static str> {
        let value = NEXT_EFFECT_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| "effect.identifier-exhausted")?;
        RunLocalId::parse(&format!("effect-{value}")).map_err(|_| "effect.identifier-invalid")
    }

    fn producer(key: usize) -> Option<Arc<EffectLifecycleProducer>> {
        lock(producers()).get(&key).cloned()
    }

    fn producers_for_owner(owner_key: usize) -> Vec<Arc<EffectLifecycleProducer>> {
        lock(producers())
            .values()
            .filter(|producer| producer.owner_key == owner_key)
            .cloned()
            .collect()
    }

    fn remove_if_terminal(
        key: usize,
        producer: &Arc<EffectLifecycleProducer>,
        should_remove: bool,
    ) {
        if should_remove {
            remove(key, producer);
        }
    }

    fn remove(key: usize, expected: &Arc<EffectLifecycleProducer>) {
        let mut active = lock(producers());
        if active
            .get(&key)
            .is_some_and(|current| Arc::ptr_eq(current, expected))
        {
            active.remove(&key);
        }
    }

    fn producers() -> &'static Mutex<HashMap<usize, Arc<EffectLifecycleProducer>>> {
        static PRODUCERS: OnceLock<Mutex<HashMap<usize, Arc<EffectLifecycleProducer>>>> =
            OnceLock::new();
        PRODUCERS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn caused_by(source: SchemaU64, relation: CausalRelation) -> Vec<CausalLink> {
        vec![CausalLink::new(source, relation)]
    }

    fn is_cancelled_error(error: &PyErr) -> bool {
        Python::attach(|py| {
            py.import("asyncio")
                .and_then(|asyncio| asyncio.getattr("CancelledError"))
                .is_ok_and(|cancelled| error.is_instance(py, &cancelled))
        })
    }

    fn identity_key(identity: &Arc<EffectIdentity>) -> usize {
        Arc::as_ptr(identity).addr()
    }

    fn owner_key(owner: &Arc<CuedScope>) -> usize {
        Arc::as_ptr(owner).addr()
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
