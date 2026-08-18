use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use pyo3::PyErr;
use troupe_agent_runtime::{
    AgentDiagnosticObservation, AgentTurnControl, AgentTurnDiagnosticIdentity,
};
use troupe_diagnostics_core::scalar::SchemaU64;

use crate::orchestration::scene_context::{CuedScope, RunBinding};

#[cfg(not(test))]
use crate::diagnostic_runtime::{
    load_producer::{DiagnosticProducerError, DiagnosticRunContext},
    runtime_producer::RuntimeLifecycleProducer,
};
#[cfg(not(test))]
use troupe_diagnostics_core::event::DiagnosticScope;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActHook {
    Admitted,
    DriverStarted,
    CancelRequested,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActCallerExit {
    Returned,
    AdmissionFailed,
    Cancelled,
    Closed,
    Cleared,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UsageFinalizationSettlement {
    NotSubmitted,
    Authoritative,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UsageFinalizationSnapshot {
    prompt_submitted: bool,
    settlement: Option<UsageFinalizationSettlement>,
}

impl UsageFinalizationSnapshot {
    pub(crate) const fn prompt_submitted(self) -> bool {
        self.prompt_submitted
    }

    pub(crate) const fn settlement(self) -> Option<UsageFinalizationSettlement> {
        self.settlement
    }
}

#[derive(Debug, Default)]
struct UsageFinalizationState {
    prompt_submitted: bool,
    settlement: Option<UsageFinalizationSettlement>,
}

#[derive(Debug)]
struct UsageFinalizationIdentity {
    act_id: Arc<str>,
    state: Mutex<UsageFinalizationState>,
}

pub(crate) struct UsageFinalizationSlot {
    identity: Arc<UsageFinalizationIdentity>,
}

pub(crate) struct UsageFinalizationAck {
    identity: Arc<UsageFinalizationIdentity>,
    usage_sequence: SchemaU64,
}

impl UsageFinalizationIdentity {
    fn new(act_id: impl Into<Arc<str>>) -> (Arc<Self>, UsageFinalizationSlot) {
        let identity = Arc::new(Self {
            act_id: act_id.into(),
            state: Mutex::new(UsageFinalizationState::default()),
        });
        let slot = UsageFinalizationSlot {
            identity: Arc::clone(&identity),
        };
        (identity, slot)
    }

    fn snapshot(&self) -> UsageFinalizationSnapshot {
        let state = lock(&self.state);
        UsageFinalizationSnapshot {
            prompt_submitted: state.prompt_submitted,
            settlement: state.settlement,
        }
    }

    fn mark_prompt_submitted(&self) -> Result<(), &'static str> {
        let mut state = lock(&self.state);
        if state.prompt_submitted || state.settlement.is_some() {
            return Err("act.usage-slot-prompt-transition-invalid");
        }
        state.prompt_submitted = true;
        Ok(())
    }

    fn settle(&self, settlement: UsageFinalizationSettlement) -> Result<bool, &'static str> {
        let mut state = lock(&self.state);
        if state.settlement == Some(settlement) {
            return Ok(false);
        }
        if state.settlement.is_some()
            || matches!(
                (state.prompt_submitted, settlement),
                (false, UsageFinalizationSettlement::Authoritative)
                    | (false, UsageFinalizationSettlement::Unknown)
                    | (true, UsageFinalizationSettlement::NotSubmitted)
            )
        {
            return Err("act.usage-slot-settlement-transition-invalid");
        }
        state.settlement = Some(settlement);
        Ok(true)
    }

    fn matches(&self, other: &Arc<Self>) -> bool {
        std::ptr::eq(self, other.as_ref())
    }
}

impl UsageFinalizationSlot {
    pub(crate) fn act_id(&self) -> &str {
        &self.identity.act_id
    }

    pub(crate) fn snapshot(&self) -> UsageFinalizationSnapshot {
        self.identity.snapshot()
    }

    pub(crate) fn acknowledge(
        self,
        usage_sequence: SchemaU64,
    ) -> Result<UsageFinalizationAck, Self> {
        if self.snapshot().settlement().is_none() {
            return Err(self);
        }
        Ok(UsageFinalizationAck {
            identity: self.identity,
            usage_sequence,
        })
    }
}

impl UsageFinalizationAck {
    fn act_id(&self) -> &str {
        &self.identity.act_id
    }

    const fn usage_sequence(&self) -> SchemaU64 {
        self.usage_sequence
    }
}
pub(crate) struct UsageFinalizationBridge {
    register_slot: fn(UsageFinalizationSlot),
    settlement_ready: fn(&str),
}

impl UsageFinalizationBridge {
    pub(crate) const fn new(
        register_slot: fn(UsageFinalizationSlot),
        settlement_ready: fn(&str),
    ) -> Self {
        Self {
            register_slot,
            settlement_ready,
        }
    }

    fn register_slot(&self, slot: UsageFinalizationSlot) {
        (self.register_slot)(slot);
    }

    fn settlement_ready(&self, act_id: &str) {
        (self.settlement_ready)(act_id);
    }
}

pub(crate) fn install_usage_finalization_bridge(
    bridge: UsageFinalizationBridge,
) -> Result<(), UsageFinalizationBridge> {
    usage_finalization_bridge_slot().set(bridge)
}

fn usage_finalization_bridge() -> Option<&'static UsageFinalizationBridge> {
    usage_finalization_bridge_slot().get()
}

fn usage_finalization_bridge_slot() -> &'static OnceLock<UsageFinalizationBridge> {
    static BRIDGE: OnceLock<UsageFinalizationBridge> = OnceLock::new();
    &BRIDGE
}

#[cfg(not(test))]
#[derive(Clone)]
pub(crate) struct ActLineageSnapshot {
    context: DiagnosticRunContext,
    act_scope: DiagnosticScope,
    event_scope: DiagnosticScope,
    act_span_id: SchemaU64,
    containing_span_id: SchemaU64,
}

#[cfg(not(test))]
impl ActLineageSnapshot {
    pub(crate) fn context(&self) -> DiagnosticRunContext {
        self.context.clone()
    }

    pub(crate) const fn act_scope(&self) -> &DiagnosticScope {
        &self.act_scope
    }

    pub(crate) const fn event_scope(&self) -> &DiagnosticScope {
        &self.event_scope
    }

    pub(crate) const fn act_span_id(&self) -> SchemaU64 {
        self.act_span_id
    }

    pub(crate) const fn containing_span_id(&self) -> SchemaU64 {
        self.containing_span_id
    }
}

#[cfg(not(test))]
pub(crate) trait ActDiagnosticFailureOwner: Send + Sync + 'static {
    fn latch_diagnostic_failure(&self, error: DiagnosticProducerError);

    fn latch_state_failure(&self, code: &'static str);
}

#[cfg(not(test))]
impl ActDiagnosticFailureOwner for RuntimeLifecycleProducer {
    fn latch_diagnostic_failure(&self, error: DiagnosticProducerError) {
        RuntimeLifecycleProducer::latch_diagnostic_failure(self, error);
    }

    fn latch_state_failure(&self, code: &'static str) {
        RuntimeLifecycleProducer::latch_state_failure(self, code);
    }
}

#[cfg(not(test))]
pub(crate) struct StandaloneActAdmission {
    context: DiagnosticRunContext,
    cue_scope: DiagnosticScope,
    session: Arc<troupe_agent_runtime::AgentSessionDiagnosticMetadata>,
    failure_owner: Arc<dyn ActDiagnosticFailureOwner>,
}

#[cfg(not(test))]
impl StandaloneActAdmission {
    pub(crate) fn new(
        context: DiagnosticRunContext,
        cue_scope: DiagnosticScope,
        session: Arc<troupe_agent_runtime::AgentSessionDiagnosticMetadata>,
        failure_owner: Arc<dyn ActDiagnosticFailureOwner>,
    ) -> Self {
        Self {
            context,
            cue_scope,
            session,
            failure_owner,
        }
    }

    fn into_parts(
        self,
    ) -> (
        DiagnosticRunContext,
        DiagnosticScope,
        Arc<troupe_agent_runtime::AgentSessionDiagnosticMetadata>,
        Arc<dyn ActDiagnosticFailureOwner>,
    ) {
        (
            self.context,
            self.cue_scope,
            self.session,
            self.failure_owner,
        )
    }
}

#[cfg(not(test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ActAdmissionPrepareError {
    code: &'static str,
}

#[cfg(not(test))]
impl ActAdmissionPrepareError {
    pub(crate) const fn code(&self) -> &'static str {
        self.code
    }
}

#[cfg(not(test))]
impl std::fmt::Display for ActAdmissionPrepareError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Act diagnostic admission failed [{}]", self.code)
    }
}

#[cfg(not(test))]
impl std::error::Error for ActAdmissionPrepareError {}

#[cfg(not(test))]
pub(crate) use active::PreparedActAdmission;

#[cfg(not(test))]
pub(crate) fn prepare_admission(
    binding: &RunBinding,
    cued: &Arc<CuedScope>,
    control: &Arc<AgentTurnControl>,
    standalone: Option<StandaloneActAdmission>,
) -> Result<Option<PreparedActAdmission>, ActAdmissionPrepareError> {
    active::prepare_admission(binding, cued, control, standalone)
}

#[inline]
pub(crate) fn admitted(
    binding: &RunBinding,
    cued: &Arc<CuedScope>,
    control: &Arc<AgentTurnControl>,
) {
    #[cfg(not(test))]
    active::admitted(binding, cued, control);
    #[cfg(test)]
    let _ = (binding, cued, control);
}

#[inline]
pub(crate) fn observe(control: &Arc<AgentTurnControl>, hook: ActHook) {
    #[cfg(not(test))]
    active::observe(control, hook);
    #[cfg(test)]
    let _ = (control, hook);
}

#[inline]
pub(crate) fn caller_finished(
    control: &Arc<AgentTurnControl>,
    exit: ActCallerExit,
    error: Option<&PyErr>,
) {
    #[cfg(not(test))]
    active::caller_finished(control, exit, error);
    #[cfg(test)]
    let _ = (control, exit, error);
}

#[inline]
pub(crate) fn observe_agent(observation: &AgentDiagnosticObservation) {
    #[cfg(not(test))]
    active::observe_agent(observation);
    #[cfg(test)]
    let _ = observation;
}

#[inline]
pub(crate) fn diagnostic_identity(
    control: &Arc<AgentTurnControl>,
) -> Option<AgentTurnDiagnosticIdentity> {
    #[cfg(not(test))]
    return active::diagnostic_identity(control);
    #[cfg(test)]
    {
        let _ = control;
        None
    }
}

#[cfg(not(test))]
#[inline]
pub(crate) fn lineage_snapshot(act_id: &str) -> Option<ActLineageSnapshot> {
    active::lineage_snapshot(act_id)
}

#[inline]
pub(crate) fn take_usage_finalization_slot(act_id: &str) -> Option<UsageFinalizationSlot> {
    #[cfg(not(test))]
    return active::take_usage_finalization_slot(act_id);
    #[cfg(test)]
    {
        let _ = act_id;
        None
    }
}

#[inline]
pub(crate) fn usage_finalized(ack: UsageFinalizationAck) {
    #[cfg(not(test))]
    active::usage_finalized(ack);
    #[cfg(test)]
    let _ = ack;
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(not(test))]
mod active {
    use std::{
        collections::HashMap,
        sync::atomic::{AtomicU64, Ordering},
        sync::{Arc, Mutex, OnceLock, Weak},
    };

    use pyo3::{PyErr, Python, exceptions::PyStopIteration, types::PyAnyMethods};
    use troupe_agent_runtime::{
        AgentDiagnosticObservation, AgentSessionDiagnosticMetadata, AgentTurnControl,
        AgentTurnDiagnosticIdentity, AgentTurnDiagnosticMetadata, AgentTurnDiagnosticOutcome,
        AgentTurnDiagnosticSettlement,
    };
    use troupe_diagnostics_core::{
        detail::{
            AgentSessionDetail, AgentTurnTerminalDetail, EmptyDetail, InstantDetail,
            SpanStartDetail,
        },
        event::{CausalLink, DiagnosticScope},
        id::RunLocalId,
        kinds::{CausalRelation, CounterKind, SpanOutcome},
        scalar::SchemaU64,
    };

    use super::{
        ActAdmissionPrepareError, ActCallerExit, ActDiagnosticFailureOwner, ActHook,
        ActLineageSnapshot, StandaloneActAdmission, UsageFinalizationAck,
        UsageFinalizationIdentity, UsageFinalizationSettlement, UsageFinalizationSlot, lock,
    };
    use crate::{
        diagnostic_runtime::{
            cue_producer,
            load_producer::{DiagnosticProducerError, DiagnosticRunContext},
            runtime_producer,
        },
        orchestration::scene_context::{CuedScope, RunBinding},
    };

    const CALLER_CANCELLED: &str = "act-caller-cancelled";
    const CALLER_FAILED: &str = "act-caller-failed";

    static NEXT_ACT_ID: AtomicU64 = AtomicU64::new(1);

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

    struct ActLifecycleState {
        act_span_id: Option<SchemaU64>,
        caller_span_id: Option<SchemaU64>,
        admitted_sequence: Option<SchemaU64>,
        waiting_sequence: Option<SchemaU64>,
        prompt_sequence: Option<SchemaU64>,
        turn_span_id: Option<SchemaU64>,
        turn_scope: Option<DiagnosticScope>,
        turn_activity_sequence: Option<SchemaU64>,
        cancel_sequence: Option<SchemaU64>,
        handoff_sequence: Option<SchemaU64>,
        turn_terminal_sequence: Option<SchemaU64>,
        turn_terminal: Option<TerminalSpan>,
        settlement: Option<UsageFinalizationSettlement>,
        caller_terminal: Option<TerminalSpan>,
        usage_slot: Option<UsageFinalizationSlot>,
        settlement_notified: bool,
        usage_ack_sequence: Option<SchemaU64>,
        finish_attempted: bool,
        producer_failed: bool,
    }

    struct ActLifecycleProducer {
        failure_owner: Arc<dyn ActDiagnosticFailureOwner>,
        context: DiagnosticRunContext,
        control_key: usize,
        act_id: RunLocalId,
        identity: AgentTurnDiagnosticIdentity,
        session: Arc<AgentSessionDiagnosticMetadata>,
        act_scope: DiagnosticScope,
        usage_identity: Arc<UsageFinalizationIdentity>,
        _control: Arc<AgentTurnControl>,
        state: Mutex<ActLifecycleState>,
    }

    #[derive(Default)]
    struct ActRegistry {
        by_control: HashMap<usize, Arc<ActLifecycleProducer>>,
        by_act_id: HashMap<String, Weak<ActLifecycleProducer>>,
    }

    pub(crate) struct PreparedActAdmission {
        producer: Arc<ActLifecycleProducer>,
    }

    impl PreparedActAdmission {
        pub(crate) fn identity(&self) -> &AgentTurnDiagnosticIdentity {
            &self.producer.identity
        }

        pub(crate) fn act_scope(&self) -> &DiagnosticScope {
            &self.producer.act_scope
        }

        pub(crate) fn commit(self) {
            let producer = self.producer;
            {
                let mut registry = lock(registry());
                if registry.by_control.contains_key(&producer.control_key)
                    || registry.by_act_id.contains_key(producer.act_id.as_str())
                {
                    producer
                        .failure_owner
                        .latch_state_failure("act.lifecycle-install-raced");
                    return;
                }
                registry
                    .by_control
                    .insert(producer.control_key, Arc::clone(&producer));
                registry.by_act_id.insert(
                    producer.act_id.as_str().to_owned(),
                    Arc::downgrade(&producer),
                );
            }
            producer.start();
            register_usage_slot(&producer);
        }
    }

    impl ActLifecycleProducer {
        fn prepare(
            binding: &RunBinding,
            cued: &Arc<CuedScope>,
            control: &Arc<AgentTurnControl>,
            standalone: Option<StandaloneActAdmission>,
        ) -> Result<Option<Self>, ActAdmissionPrepareError> {
            let (context, cue_scope, session, failure_owner) = if let Some(runtime) =
                runtime_producer::producer_for_binding(binding)
            {
                let failure_owner: Arc<dyn ActDiagnosticFailureOwner> = runtime.clone();
                if standalone.is_some() {
                    return Err(prepare_error(
                        &failure_owner,
                        "act.standalone-admission-with-production",
                    ));
                }
                let lineage = cue_producer::lineage_snapshot(cued)
                    .ok_or_else(|| prepare_error(&failure_owner, "act.cue-lineage-unavailable"))?;
                if !Arc::ptr_eq(&runtime, lineage.runtime()) {
                    return Err(prepare_error(
                        &failure_owner,
                        "act.runtime-lineage-mismatch",
                    ));
                }
                let session = control.diagnostic_session_metadata().ok_or_else(|| {
                    prepare_error(&failure_owner, "act.session-metadata-unavailable")
                })?;
                (
                    lineage.context(),
                    lineage.cue_scope().clone(),
                    session,
                    failure_owner,
                )
            } else {
                let Some(standalone) = standalone else {
                    return Ok(None);
                };
                standalone.into_parts()
            };
            let actor_id = cue_scope
                .actor_id()
                .ok_or_else(|| prepare_error(&failure_owner, "act.actor-identifier-unavailable"))?;
            if actor_id.as_str() != session.context().actor_id() {
                return Err(prepare_error(
                    &failure_owner,
                    "act.session-actor-identity-mismatch",
                ));
            }
            let (act_id, turn_id) =
                next_act_identity().map_err(|code| prepare_error(&failure_owner, code))?;
            let act_scope = scope_for_act(&cue_scope, act_id.clone(), session.generation())
                .map_err(|code| prepare_error(&failure_owner, code))?;
            let identity = AgentTurnDiagnosticIdentity::new(
                session.context().clone(),
                act_id.as_str(),
                turn_id,
            );
            let (usage_identity, usage_slot) =
                UsageFinalizationIdentity::new(Arc::<str>::from(act_id.as_str()));
            Ok(Some(Self {
                failure_owner,
                context,
                control_key: control_key(control),
                act_id,
                identity,
                session,
                act_scope,
                usage_identity,
                _control: Arc::clone(control),
                state: Mutex::new(ActLifecycleState {
                    act_span_id: None,
                    caller_span_id: None,
                    admitted_sequence: None,
                    waiting_sequence: None,
                    prompt_sequence: None,
                    turn_span_id: None,
                    turn_scope: None,
                    turn_activity_sequence: None,
                    cancel_sequence: None,
                    handoff_sequence: None,
                    turn_terminal_sequence: None,
                    turn_terminal: None,
                    settlement: None,
                    caller_terminal: None,
                    usage_slot: Some(usage_slot),
                    settlement_notified: false,
                    usage_ack_sequence: None,
                    finish_attempted: false,
                    producer_failed: false,
                }),
            }))
        }

        fn start(&self) {
            let mut state = lock(&self.state);
            let act_span_id = match self.context.start_span(
                self.act_scope.clone(),
                SpanStartDetail::ActLifecycle(session_detail(&self.session)),
                None,
            ) {
                Ok(span_id) => span_id,
                Err(error) => {
                    self.fail_diagnostic(&mut state, error);
                    return;
                }
            };
            state.act_span_id = Some(act_span_id);
            let caller_span_id = match self.context.start_span(
                self.act_scope.clone(),
                SpanStartDetail::ActCaller(EmptyDetail::new()),
                Some(act_span_id),
            ) {
                Ok(span_id) => span_id,
                Err(error) => {
                    self.fail_diagnostic(&mut state, error);
                    return;
                }
            };
            state.caller_span_id = Some(caller_span_id);
            let admitted_sequence = match self.context.emit_instant_with_causes(
                self.act_scope.clone(),
                InstantDetail::ActAdmitted(EmptyDetail::new()),
                Some(act_span_id),
                follows_from(caller_span_id),
            ) {
                Ok(sequence) => sequence,
                Err(error) => {
                    self.fail_diagnostic(&mut state, error);
                    return;
                }
            };
            state.admitted_sequence = Some(admitted_sequence);
        }

        fn driver_started(&self) {
            let mut state = lock(&self.state);
            if state.producer_failed {
                return;
            }
            let (Some(act_span_id), Some(admitted_sequence)) =
                (state.act_span_id, state.admitted_sequence)
            else {
                self.fail_state(&mut state, "act.driver-before-lifecycle");
                return;
            };
            if state.waiting_sequence.is_some() || state.caller_terminal.is_some() {
                self.fail_state(&mut state, "act.driver-transition-invalid");
                return;
            }
            match self.context.emit_instant_with_causes(
                self.act_scope.clone(),
                InstantDetail::ActWaitingReady(EmptyDetail::new()),
                Some(act_span_id),
                follows_from(admitted_sequence),
            ) {
                Ok(sequence) => state.waiting_sequence = Some(sequence),
                Err(error) => self.fail_diagnostic(&mut state, error),
            }
        }

        fn cancel_requested(&self) {
            let mut state = lock(&self.state);
            if state.producer_failed || state.cancel_sequence.is_some() {
                return;
            }
            let (Some(act_span_id), Some(caller_span_id)) =
                (state.act_span_id, state.caller_span_id)
            else {
                self.fail_state(&mut state, "act.cancel-before-lifecycle");
                return;
            };
            if state.caller_terminal.is_some() {
                return;
            }
            let source = state
                .turn_terminal_sequence
                .or(state.handoff_sequence)
                .or(state.turn_activity_sequence)
                .or(state.prompt_sequence)
                .or(state.waiting_sequence)
                .or(state.admitted_sequence)
                .expect("an active Act has an admitted sequence");
            match self.context.emit_instant_with_causes(
                self.act_scope.clone(),
                InstantDetail::ActCancelRequested(EmptyDetail::new()),
                Some(caller_span_id),
                caused_by(source, CausalRelation::Handoff),
            ) {
                Ok(sequence) => state.cancel_sequence = Some(sequence),
                Err(error) => self.fail_diagnostic(&mut state, error),
            }
            let _ = act_span_id;
        }

        fn prompt_submitted(&self, metadata: &AgentTurnDiagnosticMetadata) {
            let mut state = lock(&self.state);
            if state.producer_failed {
                return;
            }
            if let Err(code) = self.validate_turn_metadata(metadata) {
                self.fail_state(&mut state, code);
                return;
            }
            let (Some(act_span_id), Some(waiting_sequence)) =
                (state.act_span_id, state.waiting_sequence)
            else {
                self.fail_state(&mut state, "act.prompt-before-driver");
                return;
            };
            if state.prompt_sequence.is_some()
                || state.turn_span_id.is_some()
                || state.caller_terminal.is_some()
                || state.settlement.is_some()
            {
                self.fail_state(&mut state, "act.prompt-transition-invalid");
                return;
            }
            if let Err(code) = self.usage_identity.mark_prompt_submitted() {
                self.fail_state(&mut state, code);
                return;
            }
            let prompt_sequence = match self.context.emit_instant_with_causes(
                self.act_scope.clone(),
                InstantDetail::ActPromptSubmitted(EmptyDetail::new()),
                Some(act_span_id),
                follows_from(waiting_sequence),
            ) {
                Ok(sequence) => sequence,
                Err(error) => {
                    self.fail_diagnostic(&mut state, error);
                    return;
                }
            };
            let turn_scope = scope_for_turn(&self.act_scope, metadata);
            let turn_span_id = match self.context.start_span_with_causes(
                turn_scope.clone(),
                SpanStartDetail::AgentTurn(turn_session_detail(metadata)),
                Some(act_span_id),
                caused_by(prompt_sequence, CausalRelation::Dispatch),
            ) {
                Ok(span_id) => span_id,
                Err(error) => {
                    self.fail_diagnostic(&mut state, error);
                    return;
                }
            };
            let active_sequence = match self.context.emit_counter(
                turn_scope.clone(),
                CounterKind::AgentTurnActive,
                SchemaU64::new(1),
                follows_from(turn_span_id),
            ) {
                Ok(sequence) => sequence,
                Err(error) => {
                    self.fail_diagnostic(&mut state, error);
                    return;
                }
            };
            let activity_sequence = match self.context.emit_instant_with_causes(
                turn_scope.clone(),
                InstantDetail::AgentTurnActivity(EmptyDetail::new()),
                Some(turn_span_id),
                follows_from(active_sequence),
            ) {
                Ok(sequence) => sequence,
                Err(error) => {
                    self.fail_diagnostic(&mut state, error);
                    return;
                }
            };
            state.prompt_sequence = Some(prompt_sequence);
            state.turn_span_id = Some(turn_span_id);
            state.turn_scope = Some(turn_scope);
            state.turn_activity_sequence = Some(activity_sequence);
        }

        fn supervisor_handoff(&self, metadata: &AgentTurnDiagnosticMetadata) {
            let mut state = lock(&self.state);
            if state.producer_failed {
                return;
            }
            if let Err(code) = self.validate_turn_metadata(metadata) {
                self.fail_state(&mut state, code);
                return;
            }
            let (Some(act_span_id), Some(activity_sequence)) =
                (state.act_span_id, state.turn_activity_sequence)
            else {
                self.fail_state(&mut state, "act.handoff-before-turn");
                return;
            };
            if state.handoff_sequence.is_some() || state.turn_terminal.is_some() {
                self.fail_state(&mut state, "act.handoff-transition-invalid");
                return;
            }
            let source = state.cancel_sequence.unwrap_or(activity_sequence);
            match self.context.emit_instant_with_causes(
                self.act_scope.clone(),
                InstantDetail::ActSupervisorHandoff(EmptyDetail::new()),
                Some(act_span_id),
                caused_by(source, CausalRelation::Handoff),
            ) {
                Ok(sequence) => state.handoff_sequence = Some(sequence),
                Err(error) => self.fail_diagnostic(&mut state, error),
            }
        }

        fn turn_terminal(
            &self,
            metadata: &AgentTurnDiagnosticMetadata,
            settlement: AgentTurnDiagnosticSettlement,
            outcome: AgentTurnDiagnosticOutcome,
            error_code: Option<&'static str>,
        ) -> bool {
            let mut state = lock(&self.state);
            if state.producer_failed {
                return false;
            }
            if let Err(code) = self.validate_turn_metadata(metadata) {
                self.fail_state(&mut state, code);
                return false;
            }
            let settlement = usage_settlement(settlement);
            if state.settlement == Some(settlement)
                && settlement == UsageFinalizationSettlement::NotSubmitted
            {
                return self.maybe_finish(&mut state);
            }
            if state.settlement.is_some() || state.turn_terminal.is_some() {
                self.fail_state(&mut state, "act.turn-terminal-transition-invalid");
                return false;
            }
            if !terminal_detail_is_valid(outcome, error_code) {
                self.fail_state(&mut state, "act.turn-terminal-detail-invalid");
                return false;
            }
            if settlement == UsageFinalizationSettlement::NotSubmitted {
                if state.prompt_sequence.is_some() || state.turn_span_id.is_some() {
                    self.fail_state(&mut state, "act.not-submitted-after-turn-start");
                    return false;
                }
                if let Err(code) = self.usage_identity.settle(settlement) {
                    self.fail_state(&mut state, code);
                    return false;
                }
                state.settlement = Some(settlement);
                return self.maybe_finish(&mut state);
            }
            let (Some(turn_span_id), Some(turn_scope), Some(activity_sequence)) = (
                state.turn_span_id,
                state.turn_scope.clone(),
                state.turn_activity_sequence,
            ) else {
                self.fail_state(&mut state, "act.turn-terminal-before-start");
                return false;
            };
            let source = state.handoff_sequence.unwrap_or(activity_sequence);
            let detail = || AgentTurnTerminalDetail::new(error_code.map(str::to_owned));
            let terminal_sequence = match self.context.emit_instant_with_causes(
                turn_scope.clone(),
                InstantDetail::AgentTurnTerminal(detail()),
                Some(turn_span_id),
                caused_by(source, CausalRelation::Return),
            ) {
                Ok(sequence) => sequence,
                Err(error) => {
                    self.fail_diagnostic(&mut state, error);
                    return false;
                }
            };
            let settlement_sequence = if settlement == UsageFinalizationSettlement::Authoritative {
                match self.context.emit_instant_with_causes(
                    turn_scope.clone(),
                    InstantDetail::AgentTurnSettled(detail()),
                    Some(turn_span_id),
                    follows_from(terminal_sequence),
                ) {
                    Ok(sequence) => sequence,
                    Err(error) => {
                        self.fail_diagnostic(&mut state, error);
                        return false;
                    }
                }
            } else {
                terminal_sequence
            };
            let terminal = terminal_span(outcome, error_code);
            let finish_sequence = match self.context.finish_span_with_causes(
                turn_scope.clone(),
                turn_span_id,
                terminal.outcome,
                error_code.map(str::to_owned),
                caused_by(settlement_sequence, CausalRelation::Return),
            ) {
                Ok(sequence) => sequence,
                Err(error) => {
                    self.fail_diagnostic(&mut state, error);
                    return false;
                }
            };
            let inactive_sequence = match self.context.emit_counter(
                turn_scope,
                CounterKind::AgentTurnActive,
                SchemaU64::new(0),
                follows_from(finish_sequence),
            ) {
                Ok(sequence) => sequence,
                Err(error) => {
                    self.fail_diagnostic(&mut state, error);
                    return false;
                }
            };
            if let Err(code) = self.usage_identity.settle(settlement) {
                self.fail_state(&mut state, code);
                return false;
            }
            state.turn_terminal_sequence = Some(inactive_sequence);
            state.turn_terminal = Some(terminal);
            state.settlement = Some(settlement);
            self.maybe_finish(&mut state)
        }

        fn caller_finished(&self, exit: ActCallerExit, error: Option<&PyErr>) -> bool {
            let terminal = caller_terminal(exit, error);
            let mut state = lock(&self.state);
            if state.producer_failed {
                return false;
            }
            let Some(caller_span_id) = state.caller_span_id else {
                self.fail_state(&mut state, "act.caller-finish-before-start");
                return false;
            };
            if state.caller_terminal.is_some() {
                self.fail_state(&mut state, "act.caller-finish-transition-invalid");
                return false;
            }
            let source = state
                .cancel_sequence
                .or(state.turn_terminal_sequence)
                .or(state.turn_activity_sequence)
                .or(state.waiting_sequence)
                .or(state.admitted_sequence)
                .expect("a started caller has an admitted sequence");
            let relation = if terminal.outcome == SpanOutcome::Cancelled {
                CausalRelation::Handoff
            } else {
                CausalRelation::Return
            };
            if let Err(error) = self.context.finish_span_with_causes(
                self.act_scope.clone(),
                caller_span_id,
                terminal.outcome,
                terminal.error_code.map(str::to_owned),
                caused_by(source, relation),
            ) {
                self.fail_diagnostic(&mut state, error);
                return false;
            }
            state.caller_terminal = Some(terminal);
            if state.prompt_sequence.is_none() && state.settlement.is_none() {
                let settlement = UsageFinalizationSettlement::NotSubmitted;
                if let Err(code) = self.usage_identity.settle(settlement) {
                    self.fail_state(&mut state, code);
                    return false;
                }
                state.settlement = Some(settlement);
            }
            self.maybe_finish(&mut state)
        }

        fn take_usage_slot(&self) -> Option<UsageFinalizationSlot> {
            let mut state = lock(&self.state);
            if state.producer_failed || state.act_span_id.is_none() || state.finish_attempted {
                return None;
            }
            state.usage_slot.take()
        }

        fn take_settlement_notification(&self) -> bool {
            let mut state = lock(&self.state);
            if state.producer_failed || state.settlement.is_none() || state.settlement_notified {
                return false;
            }
            state.settlement_notified = true;
            true
        }

        fn usage_finalized(&self, ack: UsageFinalizationAck) -> bool {
            let mut state = lock(&self.state);
            if state.producer_failed {
                return false;
            }
            if !self.usage_identity.matches(&ack.identity)
                || state.usage_slot.is_some()
                || state.usage_ack_sequence.is_some()
                || state.settlement.is_none()
            {
                self.fail_state(&mut state, "act.usage-ack-transition-invalid");
                return false;
            }
            state.usage_ack_sequence = Some(ack.usage_sequence());
            self.maybe_finish(&mut state)
        }

        fn snapshot(&self) -> Option<ActLineageSnapshot> {
            let state = lock(&self.state);
            if state.producer_failed || state.finish_attempted {
                return None;
            }
            let act_span_id = state.act_span_id?;
            let event_scope = state
                .turn_scope
                .clone()
                .unwrap_or_else(|| self.act_scope.clone());
            let containing_span_id = match (state.turn_span_id, state.turn_terminal) {
                (Some(turn_span_id), None) => turn_span_id,
                _ => act_span_id,
            };
            Some(ActLineageSnapshot {
                context: self.context.clone(),
                act_scope: self.act_scope.clone(),
                event_scope,
                act_span_id,
                containing_span_id,
            })
        }

        fn validate_turn_metadata(
            &self,
            metadata: &AgentTurnDiagnosticMetadata,
        ) -> Result<(), &'static str> {
            if metadata.identity() != &self.identity {
                return Err("act.turn-identity-mismatch");
            }
            if metadata.provider() != self.session.provider() {
                return Err("act.turn-provider-mismatch");
            }
            if self
                .session
                .generation()
                .is_some_and(|generation| generation != metadata.session_generation())
            {
                return Err("act.turn-generation-mismatch");
            }
            Ok(())
        }

        fn maybe_finish(&self, state: &mut ActLifecycleState) -> bool {
            if state.finish_attempted || state.producer_failed {
                return state.finish_attempted;
            }
            let (Some(act_span_id), Some(caller), Some(settlement), Some(usage_sequence)) = (
                state.act_span_id,
                state.caller_terminal,
                state.settlement,
                state.usage_ack_sequence,
            ) else {
                return false;
            };
            let remote = match settlement {
                UsageFinalizationSettlement::NotSubmitted => None,
                UsageFinalizationSettlement::Authoritative
                | UsageFinalizationSettlement::Unknown => {
                    let Some(remote) = state.turn_terminal else {
                        self.fail_state(state, "act.finish-before-turn-terminal");
                        return false;
                    };
                    Some(remote)
                }
            };
            let terminal = act_terminal(caller, remote);
            state.finish_attempted = true;
            if let Err(error) = self.context.finish_span_with_causes(
                self.act_scope.clone(),
                act_span_id,
                terminal.outcome,
                terminal.error_code.map(str::to_owned),
                follows_from(usage_sequence),
            ) {
                self.fail_diagnostic(state, error);
                return false;
            }
            true
        }

        fn fail_diagnostic(&self, state: &mut ActLifecycleState, error: DiagnosticProducerError) {
            state.producer_failed = true;
            self.failure_owner.latch_diagnostic_failure(error);
        }

        fn fail_state(&self, state: &mut ActLifecycleState, code: &'static str) {
            state.producer_failed = true;
            self.failure_owner.latch_state_failure(code);
        }
    }

    pub(super) fn prepare_admission(
        binding: &RunBinding,
        cued: &Arc<CuedScope>,
        control: &Arc<AgentTurnControl>,
        standalone: Option<StandaloneActAdmission>,
    ) -> Result<Option<PreparedActAdmission>, ActAdmissionPrepareError> {
        Ok(
            ActLifecycleProducer::prepare(binding, cued, control, standalone)?.map(|producer| {
                PreparedActAdmission {
                    producer: Arc::new(producer),
                }
            }),
        )
    }

    pub(super) fn admitted(
        binding: &RunBinding,
        cued: &Arc<CuedScope>,
        control: &Arc<AgentTurnControl>,
    ) {
        let prepared = match prepare_admission(binding, cued, control, None) {
            Ok(Some(prepared)) => prepared,
            Ok(None) => return,
            Err(_) => return,
        };
        prepared.commit();
    }

    pub(super) fn observe(control: &Arc<AgentTurnControl>, hook: ActHook) {
        let Some(producer) = producer_for_control(control) else {
            return;
        };
        match hook {
            ActHook::Admitted => {
                let mut state = lock(&producer.state);
                producer.fail_state(&mut state, "act.admission-hook-duplicated");
            }
            ActHook::DriverStarted => producer.driver_started(),
            ActHook::CancelRequested => producer.cancel_requested(),
        }
    }

    pub(super) fn caller_finished(
        control: &Arc<AgentTurnControl>,
        exit: ActCallerExit,
        error: Option<&PyErr>,
    ) {
        let Some(producer) = producer_for_control(control) else {
            return;
        };
        let finished = producer.caller_finished(exit, error);
        notify_settlement_ready(&producer);
        remove_if_finished(&producer, finished);
    }

    pub(super) fn observe_agent(observation: &AgentDiagnosticObservation) {
        let Some(metadata) = observation.turn_metadata() else {
            return;
        };
        let Some(producer) = producer_for_act(metadata.identity().act_id()) else {
            return;
        };
        let finished = match observation {
            AgentDiagnosticObservation::TurnSubmitted(metadata) => {
                producer.prompt_submitted(metadata);
                false
            }
            AgentDiagnosticObservation::TurnSupervisorHandoff(metadata) => {
                producer.supervisor_handoff(metadata);
                false
            }
            AgentDiagnosticObservation::TurnTerminal {
                metadata,
                settlement,
                outcome,
                error_code,
            } => producer.turn_terminal(
                metadata,
                *settlement,
                *outcome,
                error_code.map(|code| code.as_str()),
            ),
            AgentDiagnosticObservation::SessionOpening(_)
            | AgentDiagnosticObservation::SessionOpeningAttempt(_)
            | AgentDiagnosticObservation::SessionReady(_)
            | AgentDiagnosticObservation::SessionBroken { .. }
            | AgentDiagnosticObservation::SessionClosing(_)
            | AgentDiagnosticObservation::SessionClosed(_)
            | AgentDiagnosticObservation::Candidate(_) => false,
        };
        notify_settlement_ready(&producer);
        remove_if_finished(&producer, finished);
    }

    pub(super) fn diagnostic_identity(
        control: &Arc<AgentTurnControl>,
    ) -> Option<AgentTurnDiagnosticIdentity> {
        producer_for_control(control).map(|producer| producer.identity.clone())
    }

    pub(super) fn lineage_snapshot(act_id: &str) -> Option<ActLineageSnapshot> {
        producer_for_act(act_id)?.snapshot()
    }

    pub(super) fn take_usage_finalization_slot(act_id: &str) -> Option<UsageFinalizationSlot> {
        producer_for_act(act_id)?.take_usage_slot()
    }

    pub(super) fn usage_finalized(ack: UsageFinalizationAck) {
        let act_id = ack.act_id().to_owned();
        let Some(producer) = producer_for_act(&act_id) else {
            return;
        };
        let finished = producer.usage_finalized(ack);
        remove_if_finished(&producer, finished);
    }

    fn registry() -> &'static Mutex<ActRegistry> {
        static REGISTRY: OnceLock<Mutex<ActRegistry>> = OnceLock::new();
        REGISTRY.get_or_init(|| Mutex::new(ActRegistry::default()))
    }

    fn producer_for_control(control: &Arc<AgentTurnControl>) -> Option<Arc<ActLifecycleProducer>> {
        lock(registry())
            .by_control
            .get(&control_key(control))
            .cloned()
    }

    fn producer_for_act(act_id: &str) -> Option<Arc<ActLifecycleProducer>> {
        lock(registry())
            .by_act_id
            .get(act_id)
            .and_then(Weak::upgrade)
    }

    fn remove_if_finished(producer: &Arc<ActLifecycleProducer>, finished: bool) {
        if !finished {
            return;
        }
        let mut registry = lock(registry());
        if registry
            .by_control
            .get(&producer.control_key)
            .is_some_and(|current| Arc::ptr_eq(current, producer))
        {
            registry.by_control.remove(&producer.control_key);
        }
        let remove_act = registry
            .by_act_id
            .get(producer.act_id.as_str())
            .and_then(Weak::upgrade)
            .is_none_or(|current| Arc::ptr_eq(&current, producer));
        if remove_act {
            registry.by_act_id.remove(producer.act_id.as_str());
        }
    }

    fn register_usage_slot(producer: &Arc<ActLifecycleProducer>) {
        let Some(bridge) = super::usage_finalization_bridge() else {
            return;
        };
        if let Some(slot) = producer.take_usage_slot() {
            bridge.register_slot(slot);
        }
    }

    fn notify_settlement_ready(producer: &Arc<ActLifecycleProducer>) {
        let Some(bridge) = super::usage_finalization_bridge() else {
            return;
        };
        if producer.take_settlement_notification() {
            bridge.settlement_ready(producer.act_id.as_str());
        }
    }

    fn prepare_error(
        failure_owner: &Arc<dyn ActDiagnosticFailureOwner>,
        code: &'static str,
    ) -> ActAdmissionPrepareError {
        failure_owner.latch_state_failure(code);
        ActAdmissionPrepareError { code }
    }

    fn next_act_identity() -> Result<(RunLocalId, String), &'static str> {
        let value = NEXT_ACT_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| "act.identifier-exhausted")?;
        let act_id =
            RunLocalId::parse(&format!("act-{value}")).map_err(|_| "act.identifier-invalid")?;
        Ok((act_id, format!("turn-{value}")))
    }

    fn scope_for_act(
        cue_scope: &DiagnosticScope,
        act_id: RunLocalId,
        generation: Option<u64>,
    ) -> Result<DiagnosticScope, &'static str> {
        Ok(DiagnosticScope::new(
            Some(
                cue_scope
                    .scene_id()
                    .ok_or("act.scene-identifier-unavailable")?
                    .clone(),
            ),
            Some(
                cue_scope
                    .actor_id()
                    .ok_or("act.actor-identifier-unavailable")?
                    .clone(),
            ),
            Some(
                cue_scope
                    .cue_id()
                    .ok_or("act.cue-identifier-unavailable")?
                    .clone(),
            ),
            None,
            Some(act_id),
            None,
            generation.map(SchemaU64::new),
        ))
    }

    fn scope_for_turn(
        act_scope: &DiagnosticScope,
        metadata: &AgentTurnDiagnosticMetadata,
    ) -> DiagnosticScope {
        DiagnosticScope::new(
            act_scope.scene_id().cloned(),
            act_scope.actor_id().cloned(),
            act_scope.cue_id().cloned(),
            None,
            act_scope.act_id().cloned(),
            None,
            Some(SchemaU64::new(metadata.session_generation())),
        )
    }

    fn session_detail(metadata: &AgentSessionDiagnosticMetadata) -> AgentSessionDetail {
        AgentSessionDetail::new(
            metadata.provider().name().to_owned(),
            metadata.effective_model().map(str::to_owned),
            metadata.effective_effort().map(str::to_owned),
        )
    }

    fn turn_session_detail(metadata: &AgentTurnDiagnosticMetadata) -> AgentSessionDetail {
        AgentSessionDetail::new(
            metadata.provider().name().to_owned(),
            Some(metadata.effective_model().to_owned()),
            metadata.effective_effort().map(str::to_owned),
        )
    }

    fn usage_settlement(settlement: AgentTurnDiagnosticSettlement) -> UsageFinalizationSettlement {
        match settlement {
            AgentTurnDiagnosticSettlement::NotSubmitted => {
                UsageFinalizationSettlement::NotSubmitted
            }
            AgentTurnDiagnosticSettlement::Authoritative => {
                UsageFinalizationSettlement::Authoritative
            }
            AgentTurnDiagnosticSettlement::Unknown => UsageFinalizationSettlement::Unknown,
        }
    }

    fn terminal_span(
        outcome: AgentTurnDiagnosticOutcome,
        error_code: Option<&'static str>,
    ) -> TerminalSpan {
        match outcome {
            AgentTurnDiagnosticOutcome::Completed => TerminalSpan::completed(),
            AgentTurnDiagnosticOutcome::Cancelled => TerminalSpan {
                outcome: SpanOutcome::Cancelled,
                error_code,
            },
            AgentTurnDiagnosticOutcome::Failed => TerminalSpan {
                outcome: SpanOutcome::Failed,
                error_code,
            },
        }
    }

    fn terminal_detail_is_valid(
        outcome: AgentTurnDiagnosticOutcome,
        error_code: Option<&str>,
    ) -> bool {
        match outcome {
            AgentTurnDiagnosticOutcome::Completed => error_code.is_none(),
            AgentTurnDiagnosticOutcome::Cancelled | AgentTurnDiagnosticOutcome::Failed => {
                error_code.is_some()
            }
        }
    }

    fn caller_terminal(exit: ActCallerExit, error: Option<&PyErr>) -> TerminalSpan {
        match exit {
            ActCallerExit::Returned if error.is_some_and(is_stop_iteration) => {
                TerminalSpan::completed()
            }
            ActCallerExit::Returned if error.is_some_and(is_cancelled_error) => {
                TerminalSpan::cancelled(CALLER_CANCELLED)
            }
            ActCallerExit::Returned | ActCallerExit::AdmissionFailed => {
                TerminalSpan::failed(CALLER_FAILED)
            }
            ActCallerExit::Cancelled | ActCallerExit::Closed | ActCallerExit::Cleared => {
                TerminalSpan::cancelled(CALLER_CANCELLED)
            }
        }
    }

    fn act_terminal(caller: TerminalSpan, remote: Option<TerminalSpan>) -> TerminalSpan {
        if remote.is_some_and(|remote| remote.outcome != SpanOutcome::Completed) {
            remote.expect("a checked remote terminal exists")
        } else if caller.outcome == SpanOutcome::Failed {
            caller
        } else if let Some(remote) = remote {
            remote
        } else {
            caller
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

    fn control_key(control: &Arc<AgentTurnControl>) -> usize {
        Arc::as_ptr(control).addr()
    }

    fn follows_from(source: SchemaU64) -> Vec<CausalLink> {
        caused_by(source, CausalRelation::FollowsFrom)
    }

    fn caused_by(source: SchemaU64, relation: CausalRelation) -> Vec<CausalLink> {
        vec![CausalLink::new(source, relation)]
    }
}
