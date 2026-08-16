#[allow(dead_code)]
pub(crate) mod machine {
    use troupe_agent_runtime::{AgentDiagnosticErrorCode, diagnostics::usage::AgentTurnUsage};
    use troupe_diagnostics_core::{
        kinds::{UsageAvailability, UsageSource, UsageUnavailableReason},
        scalar::{SchemaU64, TokenCount},
    };

    pub(crate) const CANDIDATE_DUPLICATED: AgentDiagnosticErrorCode =
        AgentDiagnosticErrorCode::new("usage_finalization_candidate_duplicated");
    pub(crate) const CANDIDATE_INVALID: AgentDiagnosticErrorCode =
        AgentDiagnosticErrorCode::new("usage_finalization_candidate_invalid");

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub(crate) struct FinalUsage {
        pub(crate) availability: UsageAvailability,
        pub(crate) source: Option<UsageSource>,
        pub(crate) unavailable_reason: Option<UsageUnavailableReason>,
        pub(crate) provider_total_tokens: Option<TokenCount>,
        pub(crate) input_tokens: Option<TokenCount>,
        pub(crate) output_tokens: Option<TokenCount>,
        pub(crate) thought_tokens: Option<TokenCount>,
        pub(crate) cached_read_tokens: Option<TokenCount>,
        pub(crate) cached_write_tokens: Option<TokenCount>,
    }

    impl FinalUsage {
        pub(crate) fn from_candidate(usage: &AgentTurnUsage) -> Self {
            Self {
                availability: usage.availability(),
                source: usage.source(),
                unavailable_reason: usage.unavailable_reason(),
                provider_total_tokens: usage.provider_total_tokens().cloned(),
                input_tokens: usage.input_tokens().cloned(),
                output_tokens: usage.output_tokens().cloned(),
                thought_tokens: usage.thought_tokens().cloned(),
                cached_read_tokens: usage.cached_read_tokens().cloned(),
                cached_write_tokens: usage.cached_write_tokens().cloned(),
            }
        }

        fn unavailable(reason: UsageUnavailableReason) -> Self {
            Self {
                availability: UsageAvailability::Unavailable,
                source: None,
                unavailable_reason: Some(reason),
                provider_total_tokens: None,
                input_tokens: None,
                output_tokens: None,
                thought_tokens: None,
                cached_read_tokens: None,
                cached_write_tokens: None,
            }
        }

        fn prompt_not_submitted() -> Self {
            Self::unavailable(UsageUnavailableReason::PromptNotSubmitted)
        }

        fn turn_settlement_unknown() -> Self {
            Self::unavailable(UsageUnavailableReason::TurnSettlementUnknown)
        }

        fn is_authoritative(&self) -> bool {
            !matches!(
                self.unavailable_reason,
                Some(
                    UsageUnavailableReason::PromptNotSubmitted
                        | UsageUnavailableReason::TurnSettlementUnknown
                )
            )
        }

        pub(crate) const fn availability(&self) -> UsageAvailability {
            self.availability
        }

        pub(crate) const fn source(&self) -> Option<UsageSource> {
            self.source
        }

        pub(crate) const fn unavailable_reason(&self) -> Option<UsageUnavailableReason> {
            self.unavailable_reason
        }

        pub(crate) const fn provider_total_tokens(&self) -> Option<&TokenCount> {
            self.provider_total_tokens.as_ref()
        }

        pub(crate) const fn input_tokens(&self) -> Option<&TokenCount> {
            self.input_tokens.as_ref()
        }

        pub(crate) const fn output_tokens(&self) -> Option<&TokenCount> {
            self.output_tokens.as_ref()
        }

        pub(crate) const fn thought_tokens(&self) -> Option<&TokenCount> {
            self.thought_tokens.as_ref()
        }

        pub(crate) const fn cached_read_tokens(&self) -> Option<&TokenCount> {
            self.cached_read_tokens.as_ref()
        }

        pub(crate) const fn cached_write_tokens(&self) -> Option<&TokenCount> {
            self.cached_write_tokens.as_ref()
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum SettlementBoundary {
        NotSubmitted,
        Authoritative,
        Unknown,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) struct SlotSnapshot {
        prompt_submitted: bool,
        settlement: Option<SettlementBoundary>,
    }

    impl SlotSnapshot {
        pub(crate) const fn new(
            prompt_submitted: bool,
            settlement: Option<SettlementBoundary>,
        ) -> Self {
            Self {
                prompt_submitted,
                settlement,
            }
        }
    }

    pub(crate) trait FinalizationEffects {
        type Ack;

        fn admit(&mut self, usage: FinalUsage) -> Result<SchemaU64, AgentDiagnosticErrorCode>;

        fn acknowledge(
            &mut self,
            sequence: SchemaU64,
        ) -> Result<Option<Self::Ack>, AgentDiagnosticErrorCode>;
    }

    #[derive(Debug, Eq, PartialEq)]
    pub(crate) enum MachineDrive<A> {
        Pending,
        Admitted,
        Finalized(A),
        LateIgnored,
        Failed {
            error_code: AgentDiagnosticErrorCode,
            notify: bool,
        },
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FinalizationPhase {
        Pending,
        Admitted(SchemaU64),
        Finalized,
        Failed(AgentDiagnosticErrorCode),
    }

    #[derive(Debug)]
    pub(crate) struct FinalizationMachine {
        candidate: Option<FinalUsage>,
        session_terminal_observed: bool,
        phase: FinalizationPhase,
    }

    impl Default for FinalizationMachine {
        fn default() -> Self {
            Self {
                candidate: None,
                session_terminal_observed: false,
                phase: FinalizationPhase::Pending,
            }
        }
    }

    impl FinalizationMachine {
        pub(crate) fn observe_candidate(
            &mut self,
            usage: FinalUsage,
        ) -> Result<bool, AgentDiagnosticErrorCode> {
            if self.phase != FinalizationPhase::Pending {
                return Ok(false);
            }
            if self.candidate.is_some() {
                return Err(CANDIDATE_DUPLICATED);
            }
            if !usage.is_authoritative() {
                return Err(CANDIDATE_INVALID);
            }
            self.candidate = Some(usage);
            Ok(true)
        }

        pub(crate) fn observe_session_terminal(&mut self) {
            if self.phase == FinalizationPhase::Pending {
                self.session_terminal_observed = true;
            }
        }

        pub(crate) fn drive<E>(
            &mut self,
            snapshot: SlotSnapshot,
            effects: &mut E,
        ) -> MachineDrive<E::Ack>
        where
            E: FinalizationEffects,
        {
            match self.phase {
                FinalizationPhase::Failed(error_code) => {
                    return MachineDrive::Failed {
                        error_code,
                        notify: false,
                    };
                }
                FinalizationPhase::Finalized => return MachineDrive::LateIgnored,
                FinalizationPhase::Admitted(sequence) => {
                    return self.acknowledge_if_settled(snapshot, sequence, effects);
                }
                FinalizationPhase::Pending => {}
            }

            let Some(usage) = self.select_usage(snapshot) else {
                return MachineDrive::Pending;
            };
            let sequence = match effects.admit(usage) {
                Ok(sequence) => sequence,
                Err(error_code) => return self.fail(error_code),
            };
            self.phase = FinalizationPhase::Admitted(sequence);
            self.acknowledge_if_settled(snapshot, sequence, effects)
        }

        pub(crate) fn record_failure(
            &mut self,
            error_code: AgentDiagnosticErrorCode,
        ) -> (AgentDiagnosticErrorCode, bool) {
            if let FinalizationPhase::Failed(existing) = self.phase {
                return (existing, false);
            }
            self.phase = FinalizationPhase::Failed(error_code);
            (error_code, true)
        }

        fn select_usage(&self, snapshot: SlotSnapshot) -> Option<FinalUsage> {
            if self.session_terminal_observed
                && snapshot.prompt_submitted
                && snapshot.settlement.is_none()
            {
                return Some(FinalUsage::turn_settlement_unknown());
            }
            match snapshot.settlement? {
                SettlementBoundary::NotSubmitted => Some(FinalUsage::prompt_not_submitted()),
                SettlementBoundary::Unknown => Some(FinalUsage::turn_settlement_unknown()),
                SettlementBoundary::Authoritative => self.candidate.clone(),
            }
        }

        fn acknowledge_if_settled<E>(
            &mut self,
            snapshot: SlotSnapshot,
            sequence: SchemaU64,
            effects: &mut E,
        ) -> MachineDrive<E::Ack>
        where
            E: FinalizationEffects,
        {
            if snapshot.settlement.is_none() {
                return MachineDrive::Admitted;
            }
            match effects.acknowledge(sequence) {
                Ok(Some(ack)) => {
                    self.phase = FinalizationPhase::Finalized;
                    MachineDrive::Finalized(ack)
                }
                Ok(None) => MachineDrive::Admitted,
                Err(error_code) => self.fail(error_code),
            }
        }

        fn fail<A>(&mut self, error_code: AgentDiagnosticErrorCode) -> MachineDrive<A> {
            let (error_code, notify) = self.record_failure(error_code);
            MachineDrive::Failed { error_code, notify }
        }
    }
}

#[cfg(not(test))]
#[allow(dead_code)]
mod active {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex, MutexGuard, OnceLock},
    };

    use troupe_agent_runtime::{
        AgentDiagnosticDestination, AgentDiagnosticErrorCode, AgentDiagnosticObservation,
        AgentSessionDiagnosticContext, AgentSessionDiagnosticMetadata, AgentTurnDiagnosticMetadata,
        diagnostics::usage::AgentTurnUsageCandidate,
    };
    use troupe_diagnostics_core::{
        event::{
            ActTokenUsageFinalized, CausalLink, DiagnosticEvent, DiagnosticEventHeader,
            DiagnosticScope,
        },
        hub::{
            ActEventSubscriber, BoundedInMemoryReserver, EventIdentity, MandatoryDurableReserver,
            ProductionDiagnosticHub, SinkOnlyDiagnosticHub,
        },
        kinds::CausalRelation,
        scalar::SchemaU64,
        time::{ElapsedNs, RunClock},
    };

    use crate::diagnostic_runtime::{
        act_producer::{
            self, UsageFinalizationAck, UsageFinalizationBridge, UsageFinalizationSettlement,
            UsageFinalizationSlot,
        },
        hooks::DiagnosticActSubscriberLookup,
        observation_bridge::{CanonicalObservationBridge, ObservationDisposition},
    };

    use super::machine::{
        FinalUsage, FinalizationEffects, FinalizationMachine, MachineDrive, SettlementBoundary,
        SlotSnapshot,
    };

    const ADMISSION_FAILED: AgentDiagnosticErrorCode =
        AgentDiagnosticErrorCode::new("usage_finalization_admission_failed");
    const CLOCK_FAILED: AgentDiagnosticErrorCode =
        AgentDiagnosticErrorCode::new("usage_finalization_clock_failed");
    const BRIDGE_INSTALL_FAILED: AgentDiagnosticErrorCode =
        AgentDiagnosticErrorCode::new("usage_finalization_bridge_install_failed");
    const SLOT_DUPLICATED: AgentDiagnosticErrorCode =
        AgentDiagnosticErrorCode::new("usage_finalization_slot_duplicated");
    const SLOT_LINEAGE_UNAVAILABLE: AgentDiagnosticErrorCode =
        AgentDiagnosticErrorCode::new("usage_finalization_slot_lineage_unavailable");
    const CONTEXT_MISMATCH: AgentDiagnosticErrorCode =
        AgentDiagnosticErrorCode::new("usage_finalization_context_mismatch");
    const SESSION_MISMATCH: AgentDiagnosticErrorCode =
        AgentDiagnosticErrorCode::new("usage_finalization_session_mismatch");
    const B12_DISPOSITION_INVALID: AgentDiagnosticErrorCode =
        AgentDiagnosticErrorCode::new("usage_finalization_b12_disposition_invalid");

    type CanonicalEventBuilder = Box<dyn FnOnce(EventIdentity) -> DiagnosticEvent + Send + 'static>;

    trait UsageAdmission: Send + Sync + 'static {
        fn admit(
            &self,
            candidate: CanonicalEventBuilder,
            subscriber: Option<&dyn ActEventSubscriber>,
        ) -> Result<SchemaU64, AgentDiagnosticErrorCode>;
    }

    struct ProductionUsageAdmission<R> {
        hub: Arc<ProductionDiagnosticHub<R>>,
    }

    impl<R> UsageAdmission for ProductionUsageAdmission<R>
    where
        R: MandatoryDurableReserver + 'static,
    {
        fn admit(
            &self,
            candidate: CanonicalEventBuilder,
            subscriber: Option<&dyn ActEventSubscriber>,
        ) -> Result<SchemaU64, AgentDiagnosticErrorCode> {
            self.hub
                .admit(candidate, subscriber)
                .map(|receipt| receipt.accepted().identity().sequence())
                .map_err(|_| ADMISSION_FAILED)
        }
    }

    struct SinkOnlyUsageAdmission<R> {
        hub: Arc<SinkOnlyDiagnosticHub<R>>,
        subscriber: Arc<dyn ActEventSubscriber>,
    }

    impl<R> UsageAdmission for SinkOnlyUsageAdmission<R>
    where
        R: BoundedInMemoryReserver + 'static,
    {
        fn admit(
            &self,
            candidate: CanonicalEventBuilder,
            _subscriber: Option<&dyn ActEventSubscriber>,
        ) -> Result<SchemaU64, AgentDiagnosticErrorCode> {
            self.hub
                .admit(candidate, self.subscriber.as_ref())
                .map(|receipt| receipt.accepted().identity().sequence())
                .map_err(|_| ADMISSION_FAILED)
        }
    }

    pub(crate) trait UsageFinalizationFailureOwner: Send + Sync + 'static {
        fn usage_finalization_failed(&self, act_id: &str, error_code: AgentDiagnosticErrorCode);
    }

    struct UsageDestinationContext {
        admission: Arc<dyn UsageAdmission>,
        subscribers: Option<Arc<dyn DiagnosticActSubscriberLookup>>,
        clock: RunClock,
        failure_owner: Arc<dyn UsageFinalizationFailureOwner>,
    }

    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    struct SessionKey {
        actor_id: String,
        session_id: String,
        generation: u64,
    }

    impl SessionKey {
        fn from_turn(metadata: &AgentTurnDiagnosticMetadata) -> Self {
            Self::new(metadata.identity().session(), metadata.session_generation())
        }

        fn from_session(metadata: &AgentSessionDiagnosticMetadata) -> Option<Self> {
            Some(Self::new(metadata.context(), metadata.generation()?))
        }

        fn new(context: &AgentSessionDiagnosticContext, generation: u64) -> Self {
            Self {
                actor_id: context.actor_id().to_owned(),
                session_id: context.session_id().to_owned(),
                generation,
            }
        }
    }

    struct ActFinalization {
        slot: Option<UsageFinalizationSlot>,
        context: Option<Arc<UsageDestinationContext>>,
        session: Option<SessionKey>,
        machine: FinalizationMachine,
        scope: DiagnosticScope,
        causal_sequence: SchemaU64,
    }

    #[derive(Default)]
    struct RouterState {
        acts: HashMap<String, ActFinalization>,
        invariant_error: Option<AgentDiagnosticErrorCode>,
    }

    #[derive(Default)]
    struct UsageFinalizationRouter {
        state: Mutex<RouterState>,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum DriveStatus {
        Pending,
        Admitted,
        Finalized,
        LateIgnored,
    }

    enum LockedDrive {
        Pending,
        Admitted,
        Finalized(UsageFinalizationAck),
        LateIgnored,
        Failed {
            owner: Option<Arc<dyn UsageFinalizationFailureOwner>>,
            error_code: AgentDiagnosticErrorCode,
            notify: bool,
        },
    }

    impl UsageFinalizationRouter {
        fn register_slot(
            &self,
            slot: UsageFinalizationSlot,
        ) -> Result<(), AgentDiagnosticErrorCode> {
            let act_id = slot.act_id().to_owned();
            let Some(lineage) = act_producer::lineage_snapshot(&act_id) else {
                self.record_invariant_error(SLOT_LINEAGE_UNAVAILABLE);
                return Err(SLOT_LINEAGE_UNAVAILABLE);
            };
            if lineage.act_scope().act_id().map(|id| id.as_str()) != Some(act_id.as_str()) {
                self.record_invariant_error(SLOT_LINEAGE_UNAVAILABLE);
                return Err(SLOT_LINEAGE_UNAVAILABLE);
            }
            let mut state = lock(&self.state);
            if state.acts.contains_key(&act_id) {
                state.invariant_error.get_or_insert(SLOT_DUPLICATED);
                return Err(SLOT_DUPLICATED);
            }
            state.acts.insert(
                act_id,
                ActFinalization {
                    slot: Some(slot),
                    context: None,
                    session: None,
                    machine: FinalizationMachine::default(),
                    scope: lineage.act_scope().clone(),
                    causal_sequence: lineage.act_span_id(),
                },
            );
            Ok(())
        }

        fn bind_act(
            &self,
            act_id: &str,
            context: Arc<UsageDestinationContext>,
        ) -> Result<DriveStatus, AgentDiagnosticErrorCode> {
            self.bind(act_id, context, None)
        }

        fn bind_turn(
            &self,
            metadata: &AgentTurnDiagnosticMetadata,
            context: Arc<UsageDestinationContext>,
        ) -> Result<DriveStatus, AgentDiagnosticErrorCode> {
            self.bind(
                metadata.identity().act_id(),
                context,
                Some(SessionKey::from_turn(metadata)),
            )
        }

        fn bind(
            &self,
            act_id: &str,
            context: Arc<UsageDestinationContext>,
            session: Option<SessionKey>,
        ) -> Result<DriveStatus, AgentDiagnosticErrorCode> {
            {
                let mut state = lock(&self.state);
                if let Some(error) = state.invariant_error {
                    return Err(error);
                }
                let Some(entry) = state.acts.get_mut(act_id) else {
                    return Ok(DriveStatus::LateIgnored);
                };
                if let Some(existing) = &entry.context {
                    if !Arc::ptr_eq(existing, &context) {
                        return Err(CONTEXT_MISMATCH);
                    }
                } else {
                    entry.context = Some(context);
                }
                if let Some(session) = session {
                    if entry
                        .session
                        .as_ref()
                        .is_some_and(|existing| existing != &session)
                    {
                        return Err(SESSION_MISMATCH);
                    }
                    entry.session = Some(session);
                }
            }
            self.drive(act_id)
        }

        fn observe_candidate(
            &self,
            candidate: &AgentTurnUsageCandidate,
        ) -> Result<DriveStatus, AgentDiagnosticErrorCode> {
            let act_id = candidate.turn().identity().act_id();
            {
                let mut state = lock(&self.state);
                let Some(entry) = state.acts.get_mut(act_id) else {
                    return Ok(DriveStatus::LateIgnored);
                };
                let session = SessionKey::from_turn(candidate.turn());
                if entry
                    .session
                    .as_ref()
                    .is_some_and(|existing| existing != &session)
                {
                    return Err(SESSION_MISMATCH);
                }
                let usage = FinalUsage::from_candidate(candidate.usage());
                if !entry.machine.observe_candidate(usage)? {
                    return Ok(DriveStatus::LateIgnored);
                }
                entry.session = Some(session);
            }
            self.drive(act_id)
        }

        fn session_terminal(
            &self,
            metadata: &AgentSessionDiagnosticMetadata,
        ) -> Result<DriveStatus, AgentDiagnosticErrorCode> {
            let Some(session) = SessionKey::from_session(metadata) else {
                return Ok(DriveStatus::Pending);
            };
            let act_ids = {
                let mut state = lock(&self.state);
                let mut act_ids = Vec::new();
                for (act_id, entry) in &mut state.acts {
                    if entry.session.as_ref() == Some(&session) {
                        entry.machine.observe_session_terminal();
                        act_ids.push(act_id.clone());
                    }
                }
                act_ids
            };
            let mut aggregate = DriveStatus::Pending;
            let mut first_error = None;
            for act_id in act_ids {
                match self.drive(&act_id) {
                    Ok(DriveStatus::Finalized) => aggregate = DriveStatus::Finalized,
                    Ok(DriveStatus::Admitted) if aggregate != DriveStatus::Finalized => {
                        aggregate = DriveStatus::Admitted;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        first_error.get_or_insert(error);
                    }
                }
            }
            first_error.map_or(Ok(aggregate), Err)
        }

        fn settlement_ready(&self, act_id: &str) -> Result<DriveStatus, AgentDiagnosticErrorCode> {
            self.drive(act_id)
        }

        fn drive(&self, act_id: &str) -> Result<DriveStatus, AgentDiagnosticErrorCode> {
            let locked = {
                let mut state = lock(&self.state);
                if let Some(error) = state.invariant_error {
                    return Err(error);
                }
                let Some(entry) = state.acts.get_mut(act_id) else {
                    return Ok(DriveStatus::LateIgnored);
                };
                let outcome = drive_entry(entry);
                if matches!(outcome, LockedDrive::Finalized(_)) {
                    state.acts.remove(act_id);
                }
                outcome
            };
            match locked {
                LockedDrive::Pending => Ok(DriveStatus::Pending),
                LockedDrive::Admitted => Ok(DriveStatus::Admitted),
                LockedDrive::LateIgnored => Ok(DriveStatus::LateIgnored),
                LockedDrive::Finalized(ack) => {
                    act_producer::usage_finalized(ack);
                    Ok(DriveStatus::Finalized)
                }
                LockedDrive::Failed {
                    owner,
                    error_code,
                    notify,
                } => {
                    if notify && let Some(owner) = owner {
                        owner.usage_finalization_failed(act_id, error_code);
                    }
                    Err(error_code)
                }
            }
        }

        fn record_invariant_error(&self, error: AgentDiagnosticErrorCode) {
            lock(&self.state).invariant_error.get_or_insert(error);
        }
    }

    fn drive_entry(entry: &mut ActFinalization) -> LockedDrive {
        let Some(context) = entry.context.as_ref().cloned() else {
            return LockedDrive::Pending;
        };
        let Some(slot) = entry.slot.as_ref() else {
            let (error_code, notify) = entry.machine.record_failure(CONTEXT_MISMATCH);
            return LockedDrive::Failed {
                owner: notify.then(|| Arc::clone(&context.failure_owner)),
                error_code,
                notify,
            };
        };
        let snapshot = slot_snapshot(slot);
        let subscriber = entry.scope.act_id().and_then(|act_id| {
            context
                .subscribers
                .as_ref()
                .and_then(|lookup| lookup.subscriber_for(act_id.as_str()))
        });
        let owner = Arc::clone(&context.failure_owner);
        let mut effects = RuntimeEffects {
            admission: context.admission.as_ref(),
            subscriber,
            clock: context.clock,
            scope: entry.scope.clone(),
            causal_sequence: entry.causal_sequence,
            slot: &mut entry.slot,
        };
        match entry.machine.drive(snapshot, &mut effects) {
            MachineDrive::Pending => LockedDrive::Pending,
            MachineDrive::Admitted => LockedDrive::Admitted,
            MachineDrive::Finalized(ack) => LockedDrive::Finalized(ack),
            MachineDrive::LateIgnored => LockedDrive::LateIgnored,
            MachineDrive::Failed { error_code, notify } => LockedDrive::Failed {
                owner: notify.then_some(owner),
                error_code,
                notify,
            },
        }
    }

    fn slot_snapshot(slot: &UsageFinalizationSlot) -> SlotSnapshot {
        let snapshot = slot.snapshot();
        SlotSnapshot::new(
            snapshot.prompt_submitted(),
            snapshot.settlement().map(|settlement| match settlement {
                UsageFinalizationSettlement::NotSubmitted => SettlementBoundary::NotSubmitted,
                UsageFinalizationSettlement::Authoritative => SettlementBoundary::Authoritative,
                UsageFinalizationSettlement::Unknown => SettlementBoundary::Unknown,
            }),
        )
    }

    struct RuntimeEffects<'a> {
        admission: &'a dyn UsageAdmission,
        subscriber: Option<Arc<dyn ActEventSubscriber>>,
        clock: RunClock,
        scope: DiagnosticScope,
        causal_sequence: SchemaU64,
        slot: &'a mut Option<UsageFinalizationSlot>,
    }

    impl FinalizationEffects for RuntimeEffects<'_> {
        type Ack = UsageFinalizationAck;

        fn admit(&mut self, usage: FinalUsage) -> Result<SchemaU64, AgentDiagnosticErrorCode> {
            let elapsed_ns = self.clock.elapsed_now().map_err(|_| CLOCK_FAILED)?;
            admit_usage(
                self.admission,
                self.subscriber.as_deref(),
                elapsed_ns,
                self.scope.clone(),
                self.causal_sequence,
                usage,
            )
        }

        fn acknowledge(
            &mut self,
            sequence: SchemaU64,
        ) -> Result<Option<Self::Ack>, AgentDiagnosticErrorCode> {
            let slot = self.slot.take().ok_or(CONTEXT_MISMATCH)?;
            match slot.acknowledge(sequence) {
                Ok(ack) => Ok(Some(ack)),
                Err(slot) => {
                    *self.slot = Some(slot);
                    Ok(None)
                }
            }
        }
    }

    fn admit_usage(
        admission: &dyn UsageAdmission,
        subscriber: Option<&dyn ActEventSubscriber>,
        elapsed_ns: ElapsedNs,
        scope: DiagnosticScope,
        causal_sequence: SchemaU64,
        usage: FinalUsage,
    ) -> Result<SchemaU64, AgentDiagnosticErrorCode> {
        admission.admit(
            Box::new(move |identity| {
                let header = DiagnosticEventHeader::new(
                    identity.run_id(),
                    identity.sequence(),
                    elapsed_ns,
                    scope,
                    vec![CausalLink::new(
                        causal_sequence,
                        CausalRelation::FollowsFrom,
                    )],
                )
                .expect("hub identities always have a nonzero sequence");
                DiagnosticEvent::ActTokenUsageFinalized(
                    ActTokenUsageFinalized::new(
                        header,
                        usage.availability,
                        usage.source,
                        usage.unavailable_reason,
                        usage.provider_total_tokens,
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.thought_tokens,
                        usage.cached_read_tokens,
                        usage.cached_write_tokens,
                    )
                    .expect("A04 and B17 construct a validated terminal usage"),
                )
            }),
            subscriber,
        )
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum UsageObservationDisposition {
        Canonical(ObservationDisposition),
        UsagePending,
        UsageAdmitted,
        UsageFinalized,
        LateIgnored,
    }

    pub(crate) struct UsageFinalizingObservationBridge {
        canonical: Arc<CanonicalObservationBridge>,
        context: Arc<UsageDestinationContext>,
    }

    impl UsageFinalizingObservationBridge {
        pub(crate) fn production<R>(
            hub: Arc<ProductionDiagnosticHub<R>>,
            clock: RunClock,
            failure_owner: Arc<dyn UsageFinalizationFailureOwner>,
        ) -> Result<Arc<Self>, AgentDiagnosticErrorCode>
        where
            R: MandatoryDurableReserver + 'static,
        {
            ensure_slot_bridge()?;
            let canonical = CanonicalObservationBridge::production(Arc::clone(&hub), clock);
            Ok(Arc::new(Self {
                canonical,
                context: Arc::new(UsageDestinationContext {
                    admission: Arc::new(ProductionUsageAdmission { hub }),
                    subscribers: None,
                    clock,
                    failure_owner,
                }),
            }))
        }

        pub(crate) fn production_with_subscribers<R>(
            hub: Arc<ProductionDiagnosticHub<R>>,
            subscribers: Arc<dyn DiagnosticActSubscriberLookup>,
            clock: RunClock,
            failure_owner: Arc<dyn UsageFinalizationFailureOwner>,
        ) -> Result<Arc<Self>, AgentDiagnosticErrorCode>
        where
            R: MandatoryDurableReserver + 'static,
        {
            ensure_slot_bridge()?;
            let canonical = CanonicalObservationBridge::production_with_subscribers(
                Arc::clone(&hub),
                Arc::clone(&subscribers),
                clock,
            );
            Ok(Arc::new(Self {
                canonical,
                context: Arc::new(UsageDestinationContext {
                    admission: Arc::new(ProductionUsageAdmission { hub }),
                    subscribers: Some(subscribers),
                    clock,
                    failure_owner,
                }),
            }))
        }

        pub(crate) fn sink_only<R>(
            hub: Arc<SinkOnlyDiagnosticHub<R>>,
            subscriber: Arc<dyn ActEventSubscriber>,
            clock: RunClock,
            failure_owner: Arc<dyn UsageFinalizationFailureOwner>,
        ) -> Result<Arc<Self>, AgentDiagnosticErrorCode>
        where
            R: BoundedInMemoryReserver + 'static,
        {
            ensure_slot_bridge()?;
            let canonical = CanonicalObservationBridge::sink_only(
                Arc::clone(&hub),
                Arc::clone(&subscriber),
                clock,
            );
            Ok(Arc::new(Self {
                canonical,
                context: Arc::new(UsageDestinationContext {
                    admission: Arc::new(SinkOnlyUsageAdmission { hub, subscriber }),
                    subscribers: None,
                    clock,
                    failure_owner,
                }),
            }))
        }

        pub(crate) fn bind_act(
            &self,
            act_id: &str,
        ) -> Result<UsageObservationDisposition, AgentDiagnosticErrorCode> {
            usage_router()
                .bind_act(act_id, Arc::clone(&self.context))
                .map(map_drive_status)
        }

        pub(crate) fn observe(
            &self,
            observation: &AgentDiagnosticObservation,
        ) -> Result<UsageObservationDisposition, AgentDiagnosticErrorCode> {
            if let Some(metadata) = observation.turn_metadata() {
                usage_router().bind_turn(metadata, Arc::clone(&self.context))?;
            } else if let Some(candidate) = usage_candidate(observation) {
                usage_router().bind_turn(candidate.turn(), Arc::clone(&self.context))?;
            }

            if let Some(candidate) = usage_candidate(observation) {
                let disposition = self.canonical.observe(observation)?;
                if disposition != ObservationDisposition::DeferredUsage {
                    return Err(B12_DISPOSITION_INVALID);
                }
                return usage_router()
                    .observe_candidate(candidate)
                    .map(map_drive_status);
            }

            let disposition = self.canonical.observe(observation)?;
            if let AgentDiagnosticObservation::SessionClosed(metadata) = observation {
                return usage_router()
                    .session_terminal(metadata)
                    .map(map_drive_status);
            }
            if let AgentDiagnosticObservation::TurnTerminal { metadata, .. } = observation {
                let drive = usage_router().settlement_ready(metadata.identity().act_id())?;
                if drive != DriveStatus::LateIgnored && drive != DriveStatus::Pending {
                    return Ok(map_drive_status(drive));
                }
            }
            Ok(UsageObservationDisposition::Canonical(disposition))
        }
    }

    impl AgentDiagnosticDestination for UsageFinalizingObservationBridge {
        fn try_observe(
            &self,
            observation: AgentDiagnosticObservation,
        ) -> Result<(), AgentDiagnosticErrorCode> {
            self.observe(&observation).map(|_| ())
        }
    }

    fn usage_candidate(
        observation: &AgentDiagnosticObservation,
    ) -> Option<&AgentTurnUsageCandidate> {
        observation
            .candidate()?
            .as_any()
            .downcast_ref::<AgentTurnUsageCandidate>()
    }

    fn map_drive_status(status: DriveStatus) -> UsageObservationDisposition {
        match status {
            DriveStatus::Pending => UsageObservationDisposition::UsagePending,
            DriveStatus::Admitted => UsageObservationDisposition::UsageAdmitted,
            DriveStatus::Finalized => UsageObservationDisposition::UsageFinalized,
            DriveStatus::LateIgnored => UsageObservationDisposition::LateIgnored,
        }
    }

    fn usage_router() -> &'static UsageFinalizationRouter {
        static ROUTER: OnceLock<UsageFinalizationRouter> = OnceLock::new();
        ROUTER.get_or_init(UsageFinalizationRouter::default)
    }

    fn ensure_slot_bridge() -> Result<(), AgentDiagnosticErrorCode> {
        static INSTALLED: OnceLock<Result<(), AgentDiagnosticErrorCode>> = OnceLock::new();
        *INSTALLED.get_or_init(|| {
            act_producer::install_usage_finalization_bridge(UsageFinalizationBridge::new(
                register_slot,
                settlement_ready,
            ))
            .map_err(|_| BRIDGE_INSTALL_FAILED)
        })
    }

    fn register_slot(slot: UsageFinalizationSlot) {
        let _ = usage_router().register_slot(slot);
    }

    fn settlement_ready(act_id: &str) {
        let _ = usage_router().settlement_ready(act_id);
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(not(test))]
#[allow(unused_imports)]
pub(crate) use active::{
    UsageFinalizationFailureOwner, UsageFinalizingObservationBridge, UsageObservationDisposition,
};
