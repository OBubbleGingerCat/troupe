#[cfg(not(test))]
#[allow(dead_code)]
mod active {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex, MutexGuard},
    };

    use troupe_agent_runtime::{
        AgentDiagnosticCandidate, AgentDiagnosticErrorCode, AgentDiagnosticObservation,
        AgentSessionDiagnosticContext, AgentSessionDiagnosticMetadata, AgentTurnDiagnosticMetadata,
        AgentTurnDiagnosticOutcome,
        diagnostics::{
            context::{
                AgentContextOccupancy, AgentContextOccupancyCandidate,
                AgentContextOccupancyMetadata, AgentContextUsageSampleCandidate,
            },
            cost::AgentCostCandidate,
            message::{
                AgentMessageCandidate, AgentMessageChunkObservation, AgentMessageNormalizer,
            },
            payload::{AgentToolPayloadCandidate, SinkOnlyToolPayload, ToolPayloadSource},
            plan::{AgentPlanSnapshotCandidate, AgentPlanSnapshotMetadata},
            result::AgentResultCandidate,
            thinking::{
                AgentThinkingActivityObservation, AgentThinkingActivityPhase,
                AgentThinkingCandidate, AgentThinkingNormalizer,
            },
            tool::{
                AgentToolCandidate, AgentToolNormalizer, AgentToolObservation,
                AgentToolTerminalOutcome,
            },
            usage::AgentTurnUsageCandidate,
        },
    };
    use troupe_diagnostics_core::{
        detail::{
            AgentSessionBrokenDetail, AgentSessionDetail, EmptyDetail, InstantDetail,
            SpanStartDetail, ToolCallDetail,
        },
        event::{
            AgentMessageCompleted, AgentMessageDelta, AgentPlanSnapshot, CausalLink,
            ContextUsageSampled, CounterSampled, DiagnosticEvent, DiagnosticEventHeader,
            DiagnosticEventKind, DiagnosticScope, InstantOccurred, ObservationGap, SpanFinished,
            SpanStarted,
        },
        hub::{
            ActEventSubscriber, BoundedInMemoryReserver, EventIdentity, MandatoryDurableReserver,
            ProductionDiagnosticHub, SinkOnlyDiagnosticHub,
        },
        id::RunLocalId,
        kinds::{CausalRelation, ContextSampleOrigin, CounterKind, InstantKind, SpanOutcome},
        scalar::SchemaU64,
        time::{ElapsedNs, RunClock},
    };

    use crate::diagnostic_runtime::{
        act_producer,
        hooks::{DiagnosticActSubscriberLookup, NoopDiagnosticActSubscriber},
    };

    const ADMISSION_FAILED: AgentDiagnosticErrorCode =
        AgentDiagnosticErrorCode::new("canonical_admission_failed");
    const CLOCK_FAILED: AgentDiagnosticErrorCode =
        AgentDiagnosticErrorCode::new("observation_clock_failed");
    const ACT_LINEAGE_UNAVAILABLE: AgentDiagnosticErrorCode =
        AgentDiagnosticErrorCode::new("observation_act_lineage_unavailable");
    const TURN_IDENTITY_MISMATCH: AgentDiagnosticErrorCode =
        AgentDiagnosticErrorCode::new("observation_turn_identity_mismatch");
    const SESSION_TRANSITION_INVALID: AgentDiagnosticErrorCode =
        AgentDiagnosticErrorCode::new("observation_session_transition_invalid");
    const CANDIDATE_UNSUPPORTED: AgentDiagnosticErrorCode =
        AgentDiagnosticErrorCode::new("observation_candidate_unsupported");
    const RESULT_TRANSITION_INVALID: AgentDiagnosticErrorCode =
        AgentDiagnosticErrorCode::new("observation_result_transition_invalid");
    const TOOL_TRANSITION_INVALID: AgentDiagnosticErrorCode =
        AgentDiagnosticErrorCode::new("observation_tool_transition_invalid");
    const THINKING_TRANSITION_INVALID: AgentDiagnosticErrorCode =
        AgentDiagnosticErrorCode::new("observation_thinking_transition_invalid");
    const IDENTIFIER_INVALID: AgentDiagnosticErrorCode =
        AgentDiagnosticErrorCode::new("observation_identifier_invalid");

    type CanonicalEventBuilder = Box<dyn FnOnce(EventIdentity) -> DiagnosticEvent + Send + 'static>;

    trait CanonicalObservationAdmission: Send + Sync + 'static {
        fn admit(
            &self,
            candidate: CanonicalEventBuilder,
            subscriber: Option<&dyn ActEventSubscriber>,
        ) -> Result<SchemaU64, AgentDiagnosticErrorCode>;
    }

    struct ProductionAdmission<R> {
        hub: Arc<ProductionDiagnosticHub<R>>,
    }

    impl<R> CanonicalObservationAdmission for ProductionAdmission<R>
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

    struct SinkOnlyAdmission<R> {
        hub: Arc<SinkOnlyDiagnosticHub<R>>,
        fallback_subscriber: Arc<dyn ActEventSubscriber>,
    }

    impl<R> CanonicalObservationAdmission for SinkOnlyAdmission<R>
    where
        R: BoundedInMemoryReserver + 'static,
    {
        fn admit(
            &self,
            candidate: CanonicalEventBuilder,
            subscriber: Option<&dyn ActEventSubscriber>,
        ) -> Result<SchemaU64, AgentDiagnosticErrorCode> {
            self.hub
                .admit(
                    candidate,
                    subscriber.unwrap_or(self.fallback_subscriber.as_ref()),
                )
                .map(|receipt| receipt.accepted().identity().sequence())
                .map_err(|_| ADMISSION_FAILED)
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum ObservationDisposition {
        Admitted,
        Buffered,
        DeferredUsage,
        SinkOnlyPayload,
    }

    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    struct SessionKey {
        actor_id: String,
        session_id: String,
    }

    impl SessionKey {
        fn new(context: &AgentSessionDiagnosticContext) -> Self {
            Self {
                actor_id: context.actor_id().to_owned(),
                session_id: context.session_id().to_owned(),
            }
        }
    }

    #[derive(Clone)]
    struct OpenSpan {
        span_id: SchemaU64,
        scope: DiagnosticScope,
    }

    struct SessionProjection {
        provider: troupe_agent_runtime::AgentDiagnosticProvider,
        requested_model: String,
        lifecycle: OpenSpan,
        opening: Option<(u64, OpenSpan)>,
        closing: Option<OpenSpan>,
        ready: bool,
        broken_code: Option<&'static str>,
        closed: bool,
        last_sequence: SchemaU64,
    }

    struct ToolProjection {
        span_id: SchemaU64,
        scope: DiagnosticScope,
        finished: bool,
    }

    struct ThinkingProjection {
        span_id: SchemaU64,
        scope: DiagnosticScope,
    }

    struct TurnProjection {
        identity: troupe_agent_runtime::AgentTurnDiagnosticIdentity,
        session_generation: u64,
        messages: AgentMessageNormalizer,
        thinking: AgentThinkingNormalizer,
        tools: AgentToolNormalizer,
        thinking_span: Option<ThinkingProjection>,
        tool_spans: HashMap<String, ToolProjection>,
        pending_tool_payloads: Vec<SinkOnlyToolPayload>,
        last_sequence: SchemaU64,
        last_rejection_count: u64,
        pending_rejection_sequence: Option<SchemaU64>,
    }

    impl TurnProjection {
        fn new(metadata: &AgentTurnDiagnosticMetadata, initial_sequence: SchemaU64) -> Self {
            Self {
                identity: metadata.identity().clone(),
                session_generation: metadata.session_generation(),
                messages: AgentMessageNormalizer::new(),
                thinking: AgentThinkingNormalizer::new(),
                tools: AgentToolNormalizer::new(),
                thinking_span: None,
                tool_spans: HashMap::new(),
                pending_tool_payloads: Vec::new(),
                last_sequence: initial_sequence,
                last_rejection_count: 0,
                pending_rejection_sequence: None,
            }
        }

        fn from_result(
            metadata: &troupe_agent_runtime::diagnostics::result::AgentResultMetadata,
            initial_sequence: SchemaU64,
        ) -> Self {
            Self {
                identity: metadata.identity().clone(),
                session_generation: metadata.session_generation(),
                messages: AgentMessageNormalizer::new(),
                thinking: AgentThinkingNormalizer::new(),
                tools: AgentToolNormalizer::new(),
                thinking_span: None,
                tool_spans: HashMap::new(),
                pending_tool_payloads: Vec::new(),
                last_sequence: initial_sequence,
                last_rejection_count: 0,
                pending_rejection_sequence: None,
            }
        }
    }

    #[derive(Clone)]
    struct CarriedContext {
        generation: Option<u64>,
        scope: DiagnosticScope,
        containing_span_id: Option<SchemaU64>,
        occupancy: AgentContextOccupancy,
        observed_elapsed_ns: u64,
        sequence: SchemaU64,
    }

    #[derive(Default)]
    struct BridgeState {
        sessions: HashMap<SessionKey, SessionProjection>,
        turns: HashMap<String, TurnProjection>,
        latest_context: HashMap<SessionKey, CarriedContext>,
        next_tool_id: u64,
    }

    pub(crate) struct CanonicalObservationBridge {
        admission: Arc<dyn CanonicalObservationAdmission>,
        subscribers: Option<Arc<dyn DiagnosticActSubscriberLookup>>,
        clock: RunClock,
        state: Mutex<BridgeState>,
    }

    impl CanonicalObservationBridge {
        pub(crate) fn production<R>(
            hub: Arc<ProductionDiagnosticHub<R>>,
            clock: RunClock,
        ) -> Arc<Self>
        where
            R: MandatoryDurableReserver + 'static,
        {
            Arc::new(Self {
                admission: Arc::new(ProductionAdmission { hub }),
                subscribers: None,
                clock,
                state: Mutex::new(BridgeState {
                    next_tool_id: 1,
                    ..BridgeState::default()
                }),
            })
        }

        pub(crate) fn production_with_subscribers<R>(
            hub: Arc<ProductionDiagnosticHub<R>>,
            subscribers: Arc<dyn DiagnosticActSubscriberLookup>,
            clock: RunClock,
        ) -> Arc<Self>
        where
            R: MandatoryDurableReserver + 'static,
        {
            Arc::new(Self {
                admission: Arc::new(ProductionAdmission { hub }),
                subscribers: Some(subscribers),
                clock,
                state: Mutex::new(BridgeState {
                    next_tool_id: 1,
                    ..BridgeState::default()
                }),
            })
        }

        pub(crate) fn sink_only<R>(
            hub: Arc<SinkOnlyDiagnosticHub<R>>,
            subscriber: Arc<dyn ActEventSubscriber>,
            clock: RunClock,
        ) -> Arc<Self>
        where
            R: BoundedInMemoryReserver + 'static,
        {
            Arc::new(Self {
                admission: Arc::new(SinkOnlyAdmission {
                    hub,
                    fallback_subscriber: subscriber,
                }),
                subscribers: None,
                clock,
                state: Mutex::new(BridgeState {
                    next_tool_id: 1,
                    ..BridgeState::default()
                }),
            })
        }

        pub(crate) fn sink_only_with_subscribers<R>(
            hub: Arc<SinkOnlyDiagnosticHub<R>>,
            subscribers: Arc<dyn DiagnosticActSubscriberLookup>,
            clock: RunClock,
        ) -> Arc<Self>
        where
            R: BoundedInMemoryReserver + 'static,
        {
            let fallback_subscriber: Arc<dyn ActEventSubscriber> =
                Arc::new(NoopDiagnosticActSubscriber);
            Arc::new(Self {
                admission: Arc::new(SinkOnlyAdmission {
                    hub,
                    fallback_subscriber,
                }),
                subscribers: Some(subscribers),
                clock,
                state: Mutex::new(BridgeState {
                    next_tool_id: 1,
                    ..BridgeState::default()
                }),
            })
        }

        pub(crate) fn observe(
            &self,
            observation: &AgentDiagnosticObservation,
        ) -> Result<ObservationDisposition, AgentDiagnosticErrorCode> {
            match observation {
                AgentDiagnosticObservation::SessionOpening(metadata) => {
                    self.session_opening(metadata)
                }
                AgentDiagnosticObservation::SessionOpeningAttempt(metadata) => {
                    self.session_opening_attempt(metadata)
                }
                AgentDiagnosticObservation::SessionReady(metadata) => self.session_ready(metadata),
                AgentDiagnosticObservation::SessionBroken {
                    metadata,
                    error_code,
                } => self.session_broken(metadata, error_code.as_str()),
                AgentDiagnosticObservation::SessionClosing(metadata) => {
                    self.session_closing(metadata)
                }
                AgentDiagnosticObservation::SessionClosed(metadata) => {
                    self.session_closed(metadata)
                }
                AgentDiagnosticObservation::TurnSubmitted(_)
                | AgentDiagnosticObservation::TurnSupervisorHandoff(_) => {
                    act_producer::observe_agent(observation);
                    Ok(ObservationDisposition::Buffered)
                }
                AgentDiagnosticObservation::TurnTerminal {
                    metadata, outcome, ..
                } => {
                    self.turn_terminal(metadata, *outcome)?;
                    act_producer::observe_agent(observation);
                    Ok(ObservationDisposition::Admitted)
                }
                AgentDiagnosticObservation::Candidate(candidate) => {
                    self.observe_candidate(candidate.as_ref())
                }
            }
        }

        fn session_opening(
            &self,
            metadata: &AgentSessionDiagnosticMetadata,
        ) -> Result<ObservationDisposition, AgentDiagnosticErrorCode> {
            let key = SessionKey::new(metadata.context());
            let mut state = lock(&self.state);
            if state.sessions.contains_key(&key) {
                return Err(SESSION_TRANSITION_INVALID);
            }
            let scope = session_scope(metadata.context(), None)?;
            let span_id = self.admit_span_start(
                self.now()?,
                scope.clone(),
                SpanStartDetail::AgentSessionLifecycle(session_detail(metadata)),
                None,
                Vec::new(),
            )?;
            state.sessions.insert(
                key,
                SessionProjection {
                    provider: metadata.provider(),
                    requested_model: metadata.requested_model().to_owned(),
                    lifecycle: OpenSpan { span_id, scope },
                    opening: None,
                    closing: None,
                    ready: false,
                    broken_code: None,
                    closed: false,
                    last_sequence: span_id,
                },
            );
            Ok(ObservationDisposition::Admitted)
        }

        fn session_opening_attempt(
            &self,
            metadata: &AgentSessionDiagnosticMetadata,
        ) -> Result<ObservationDisposition, AgentDiagnosticErrorCode> {
            let generation = metadata.generation().ok_or(SESSION_TRANSITION_INVALID)?;
            let key = SessionKey::new(metadata.context());
            let mut state = lock(&self.state);
            let session = session_mut(&mut state, &key, metadata)?;
            if session.ready || session.closing.is_some() || session.closed {
                return Err(SESSION_TRANSITION_INVALID);
            }
            if session
                .opening
                .as_ref()
                .is_some_and(|(previous_generation, _)| generation <= *previous_generation)
            {
                return Err(SESSION_TRANSITION_INVALID);
            }
            let mut relation = CausalRelation::Dispatch;
            if let Some((_, previous)) = session.opening.take() {
                let sequence = self.admit_span_finish(
                    self.now()?,
                    previous.scope,
                    previous.span_id,
                    SpanOutcome::Failed,
                    Some("agent-session-opening-retried".to_owned()),
                    follows_from(session.last_sequence),
                )?;
                session.last_sequence = sequence;
                relation = CausalRelation::Retry;
            }
            let scope = session_scope(metadata.context(), Some(generation))?;
            let span_id = self.admit_span_start(
                self.now()?,
                scope.clone(),
                SpanStartDetail::AgentSessionOpening(session_detail(metadata)),
                Some(session.lifecycle.span_id),
                caused_by(session.last_sequence, relation),
            )?;
            session.opening = Some((generation, OpenSpan { span_id, scope }));
            session.last_sequence = span_id;
            Ok(ObservationDisposition::Admitted)
        }

        fn session_ready(
            &self,
            metadata: &AgentSessionDiagnosticMetadata,
        ) -> Result<ObservationDisposition, AgentDiagnosticErrorCode> {
            let generation = metadata.generation().ok_or(SESSION_TRANSITION_INVALID)?;
            let key = SessionKey::new(metadata.context());
            let mut state = lock(&self.state);
            let session = session_mut(&mut state, &key, metadata)?;
            if session.ready || session.closed {
                return Err(SESSION_TRANSITION_INVALID);
            }
            let Some((opening_generation, _)) = session.opening.as_ref() else {
                return Err(SESSION_TRANSITION_INVALID);
            };
            if *opening_generation != generation {
                return Err(SESSION_TRANSITION_INVALID);
            }
            let (_, opening) = session
                .opening
                .take()
                .expect("the opening span was validated before mutation");
            let ready_sequence = self.admit_instant(
                self.now()?,
                opening.scope.clone(),
                InstantDetail::AgentSessionReady(session_detail(metadata)),
                Some(opening.span_id),
                follows_from(session.last_sequence),
            )?;
            let finish_sequence = self.admit_span_finish(
                self.now()?,
                opening.scope,
                opening.span_id,
                SpanOutcome::Completed,
                None,
                follows_from(ready_sequence),
            )?;
            session.ready = true;
            session.last_sequence = finish_sequence;
            Ok(ObservationDisposition::Admitted)
        }

        fn session_broken(
            &self,
            metadata: &AgentSessionDiagnosticMetadata,
            error_code: &'static str,
        ) -> Result<ObservationDisposition, AgentDiagnosticErrorCode> {
            let key = SessionKey::new(metadata.context());
            let mut state = lock(&self.state);
            let session = session_mut(&mut state, &key, metadata)?;
            if session.broken_code.is_some() || session.closed {
                return Err(SESSION_TRANSITION_INVALID);
            }
            let containing_span_id = session
                .opening
                .as_ref()
                .map_or(session.lifecycle.span_id, |(_, opening)| opening.span_id);
            let scope = session_scope(metadata.context(), metadata.generation())?;
            let sequence = self.admit_instant(
                self.now()?,
                scope,
                InstantDetail::AgentSessionBroken(AgentSessionBrokenDetail::new(
                    metadata.provider().name().to_owned(),
                    effective_model(metadata),
                    effective_effort(metadata),
                    error_code.to_owned(),
                )),
                Some(containing_span_id),
                follows_from(session.last_sequence),
            )?;
            session.broken_code = Some(error_code);
            session.last_sequence = sequence;
            Ok(ObservationDisposition::Admitted)
        }

        fn session_closing(
            &self,
            metadata: &AgentSessionDiagnosticMetadata,
        ) -> Result<ObservationDisposition, AgentDiagnosticErrorCode> {
            let key = SessionKey::new(metadata.context());
            let mut state = lock(&self.state);
            let session = session_mut(&mut state, &key, metadata)?;
            if session.closing.is_some() || session.closed {
                return Err(SESSION_TRANSITION_INVALID);
            }
            if let Some((_, opening)) = session.opening.take() {
                let error_code = session
                    .broken_code
                    .unwrap_or("agent-session-opening-abandoned");
                let sequence = self.admit_span_finish(
                    self.now()?,
                    opening.scope,
                    opening.span_id,
                    SpanOutcome::Failed,
                    Some(error_code.to_owned()),
                    follows_from(session.last_sequence),
                )?;
                session.last_sequence = sequence;
            }
            let scope = session_scope(metadata.context(), metadata.generation())?;
            let span_id = self.admit_span_start(
                self.now()?,
                scope.clone(),
                SpanStartDetail::AgentSessionClosing(session_detail(metadata)),
                Some(session.lifecycle.span_id),
                follows_from(session.last_sequence),
            )?;
            session.closing = Some(OpenSpan { span_id, scope });
            session.last_sequence = span_id;
            Ok(ObservationDisposition::Admitted)
        }

        fn session_closed(
            &self,
            metadata: &AgentSessionDiagnosticMetadata,
        ) -> Result<ObservationDisposition, AgentDiagnosticErrorCode> {
            let key = SessionKey::new(metadata.context());
            let mut state = lock(&self.state);
            let session = session_mut(&mut state, &key, metadata)?;
            let Some(closing) = session.closing.take() else {
                return Err(SESSION_TRANSITION_INVALID);
            };
            if session.closed {
                return Err(SESSION_TRANSITION_INVALID);
            }
            let closing_sequence = self.admit_span_finish(
                self.now()?,
                closing.scope,
                closing.span_id,
                SpanOutcome::Completed,
                None,
                follows_from(session.last_sequence),
            )?;
            let (outcome, error_code) = match session.broken_code {
                Some(error_code) => (SpanOutcome::Failed, Some(error_code.to_owned())),
                None => (SpanOutcome::Completed, None),
            };
            let lifecycle_sequence = self.admit_span_finish(
                self.now()?,
                session.lifecycle.scope.clone(),
                session.lifecycle.span_id,
                outcome,
                error_code,
                follows_from(closing_sequence),
            )?;
            session.closed = true;
            session.last_sequence = lifecycle_sequence;
            Ok(ObservationDisposition::Admitted)
        }

        fn observe_candidate(
            &self,
            candidate: &dyn AgentDiagnosticCandidate,
        ) -> Result<ObservationDisposition, AgentDiagnosticErrorCode> {
            if candidate
                .as_any()
                .downcast_ref::<AgentTurnUsageCandidate>()
                .is_some()
            {
                return Ok(ObservationDisposition::DeferredUsage);
            }
            if let Some(candidate) = candidate
                .as_any()
                .downcast_ref::<AgentToolPayloadCandidate>()
            {
                if self.subscribers.is_some() {
                    let act_id = candidate.turn().identity().act_id();
                    let lineage = act_lineage(candidate.turn())?;
                    let mut state = lock(&self.state);
                    ensure_turn(&mut state, candidate.turn(), lineage.containing_span_id())?;
                    state
                        .turns
                        .get_mut(act_id)
                        .expect("the turn projection was installed")
                        .pending_tool_payloads
                        .push(candidate.payload().clone());
                }
                return Ok(ObservationDisposition::SinkOnlyPayload);
            }
            if let Some(observation) = candidate
                .as_any()
                .downcast_ref::<AgentMessageChunkObservation>()
            {
                return self.message_observed(observation);
            }
            if let Some(observation) = candidate
                .as_any()
                .downcast_ref::<AgentThinkingActivityObservation>()
            {
                return self.thinking_observed(observation);
            }
            if let Some(observation) = candidate.as_any().downcast_ref::<AgentToolObservation>() {
                return self.tool_observed(observation);
            }
            if let Some(candidate) = candidate
                .as_any()
                .downcast_ref::<AgentPlanSnapshotCandidate>()
            {
                return self.plan_observed(candidate);
            }
            if let Some(candidate) = candidate
                .as_any()
                .downcast_ref::<AgentContextOccupancyCandidate>()
            {
                return self.context_observed(candidate);
            }
            if let Some(candidate) = candidate
                .as_any()
                .downcast_ref::<AgentContextUsageSampleCandidate>()
            {
                return self.context_sample_observed(candidate);
            }
            if let Some(candidate) = candidate.as_any().downcast_ref::<AgentCostCandidate>() {
                return self.cost_observed(candidate);
            }
            if let Some(candidate) = candidate.as_any().downcast_ref::<AgentResultCandidate>() {
                return self.result_observed(candidate);
            }
            Err(CANDIDATE_UNSUPPORTED)
        }

        fn message_observed(
            &self,
            observation: &AgentMessageChunkObservation,
        ) -> Result<ObservationDisposition, AgentDiagnosticErrorCode> {
            let elapsed_ns = self.now()?.get();
            let act_id = observation.turn().identity().act_id();
            let lineage = act_lineage(observation.turn())?;
            let mut state = lock(&self.state);
            ensure_turn(&mut state, observation.turn(), lineage.containing_span_id())?;
            let candidates = state
                .turns
                .get_mut(act_id)
                .expect("the turn projection was installed")
                .messages
                .observe_chunk(observation, elapsed_ns)
                .map_err(|error| AgentDiagnosticErrorCode::new(error.code()))?;
            self.project_message_candidates(&mut state, act_id, candidates)
        }

        fn thinking_observed(
            &self,
            observation: &AgentThinkingActivityObservation,
        ) -> Result<ObservationDisposition, AgentDiagnosticErrorCode> {
            let elapsed_ns = self.now()?.get();
            let act_id = observation.turn().identity().act_id();
            let lineage = act_lineage(observation.turn())?;
            let mut state = lock(&self.state);
            ensure_turn(&mut state, observation.turn(), lineage.containing_span_id())?;
            self.flush_messages(&mut state, act_id, elapsed_ns)?;
            let candidates = state
                .turns
                .get_mut(act_id)
                .expect("the turn projection was installed")
                .thinking
                .observe_observation(observation, elapsed_ns);
            self.project_thinking_candidates(&mut state, act_id, candidates, None)
        }

        fn tool_observed(
            &self,
            observation: &AgentToolObservation,
        ) -> Result<ObservationDisposition, AgentDiagnosticErrorCode> {
            let elapsed_ns = self.now()?.get();
            let act_id = observation.turn().identity().act_id();
            let lineage = act_lineage(observation.turn())?;
            let mut state = lock(&self.state);
            ensure_turn(&mut state, observation.turn(), lineage.containing_span_id())?;
            self.flush_messages(&mut state, act_id, elapsed_ns)?;
            let candidates = state
                .turns
                .get_mut(act_id)
                .expect("the turn projection was installed")
                .tools
                .observe(observation, elapsed_ns);
            self.project_tool_candidates(&mut state, act_id, candidates)
        }

        fn plan_observed(
            &self,
            candidate: &AgentPlanSnapshotCandidate,
        ) -> Result<ObservationDisposition, AgentDiagnosticErrorCode> {
            let elapsed_ns = self.now()?.get();
            let mut state = lock(&self.state);
            let (scope, containing_span_id, previous) = match candidate.metadata() {
                AgentPlanSnapshotMetadata::Turn(metadata) => {
                    let lineage = act_lineage(metadata)?;
                    ensure_turn(&mut state, metadata, lineage.containing_span_id())?;
                    self.flush_messages(&mut state, metadata.identity().act_id(), elapsed_ns)?;
                    let turn = state
                        .turns
                        .get(metadata.identity().act_id())
                        .expect("the turn projection was installed");
                    (
                        lineage.event_scope().clone(),
                        Some(lineage.containing_span_id()),
                        turn.last_sequence,
                    )
                }
                AgentPlanSnapshotMetadata::Session(metadata) => {
                    session_candidate_lineage(&state, metadata)?
                }
            };
            let entries = candidate.entries().to_vec();
            let truncated = candidate.truncated();
            let sequence = self.admit_event(
                ElapsedNs::new(elapsed_ns),
                scope,
                follows_from(previous),
                move |header| {
                    DiagnosticEvent::AgentPlanSnapshot(AgentPlanSnapshot::new(
                        header, entries, truncated,
                    ))
                },
            )?;
            update_candidate_sequence(&mut state, candidate.metadata(), sequence)?;
            let _ = containing_span_id;
            Ok(ObservationDisposition::Admitted)
        }

        fn context_observed(
            &self,
            candidate: &AgentContextOccupancyCandidate,
        ) -> Result<ObservationDisposition, AgentDiagnosticErrorCode> {
            let elapsed_ns = self.now()?.get();
            self.emit_context_sample(
                candidate.metadata(),
                candidate.occupancy(),
                elapsed_ns,
                elapsed_ns,
            )
        }

        fn context_sample_observed(
            &self,
            candidate: &AgentContextUsageSampleCandidate,
        ) -> Result<ObservationDisposition, AgentDiagnosticErrorCode> {
            self.emit_context_sample(
                candidate.metadata(),
                candidate.observation().occupancy(),
                self.now()?.get(),
                candidate.observed_elapsed_ns(),
            )
        }

        fn emit_context_sample(
            &self,
            metadata: &AgentContextOccupancyMetadata,
            occupancy: AgentContextOccupancy,
            event_elapsed_ns: u64,
            observed_elapsed_ns: u64,
        ) -> Result<ObservationDisposition, AgentDiagnosticErrorCode> {
            let mut state = lock(&self.state);
            let (key, generation, scope, containing_span_id, previous) = match metadata {
                AgentContextOccupancyMetadata::Turn(metadata) => {
                    let lineage = act_lineage(metadata)?;
                    ensure_turn(&mut state, metadata, lineage.containing_span_id())?;
                    self.flush_messages(
                        &mut state,
                        metadata.identity().act_id(),
                        event_elapsed_ns,
                    )?;
                    let turn = state
                        .turns
                        .get(metadata.identity().act_id())
                        .expect("the turn projection was installed");
                    (
                        SessionKey::new(metadata.identity().session()),
                        Some(metadata.session_generation()),
                        lineage.event_scope().clone(),
                        Some(lineage.containing_span_id()),
                        turn.last_sequence,
                    )
                }
                AgentContextOccupancyMetadata::Session(metadata) => {
                    let (scope, containing_span_id, previous) =
                        session_candidate_lineage(&state, metadata)?;
                    (
                        SessionKey::new(metadata.context()),
                        metadata.generation(),
                        scope,
                        containing_span_id,
                        previous,
                    )
                }
            };
            let context_used_tokens = occupancy.context_used_tokens().map(SchemaU64::new);
            let context_window_tokens = occupancy.context_window_tokens().map(SchemaU64::new);
            let event_scope = scope.clone();
            let sequence = self.admit_event(
                ElapsedNs::new(event_elapsed_ns),
                scope.clone(),
                follows_from(previous),
                move |header| {
                    DiagnosticEvent::ContextUsageSampled(
                        ContextUsageSampled::new(
                            header,
                            context_used_tokens,
                            context_window_tokens,
                            None,
                            None,
                            ContextSampleOrigin::Provider,
                            None,
                        )
                        .expect("A06 emitted a validated context occupancy"),
                    )
                },
            )?;
            state.latest_context.insert(
                key,
                CarriedContext {
                    generation,
                    scope: event_scope,
                    containing_span_id,
                    occupancy,
                    observed_elapsed_ns,
                    sequence,
                },
            );
            update_context_metadata_sequence(&mut state, metadata, sequence)?;
            Ok(ObservationDisposition::Admitted)
        }

        fn cost_observed(
            &self,
            candidate: &AgentCostCandidate,
        ) -> Result<ObservationDisposition, AgentDiagnosticErrorCode> {
            let elapsed_ns = self.now()?;
            let key = SessionKey::new(candidate.detail().session());
            let mut state = lock(&self.state);
            let carried =
                state.latest_context.get(&key).cloned().filter(|context| {
                    context.generation == candidate.detail().session_generation()
                });
            let (scope, containing_span_id, previous, occupancy, observed_elapsed_ns, origin) =
                match carried {
                    Some(context) => (
                        context.scope,
                        context.containing_span_id,
                        context.sequence,
                        context.occupancy,
                        Some(ElapsedNs::new(context.observed_elapsed_ns)),
                        ContextSampleOrigin::CarriedForward,
                    ),
                    None => {
                        let scope = session_scope(
                            candidate.detail().session(),
                            candidate.detail().session_generation(),
                        )?;
                        let (containing_span_id, previous) =
                            state.sessions.get(&key).map_or((None, None), |session| {
                                (Some(session.lifecycle.span_id), Some(session.last_sequence))
                            });
                        (
                            scope,
                            containing_span_id,
                            previous.unwrap_or(SchemaU64::new(1)),
                            AgentContextOccupancy::new(None, None)
                                .expect("empty context occupancy is valid"),
                            None,
                            ContextSampleOrigin::Provider,
                        )
                    }
                };
            let amount = candidate.cost().amount().clone();
            let currency = candidate.cost().currency().clone();
            let context_used_tokens = occupancy.context_used_tokens().map(SchemaU64::new);
            let context_window_tokens = occupancy.context_window_tokens().map(SchemaU64::new);
            let causes = if origin == ContextSampleOrigin::CarriedForward
                || state.sessions.contains_key(&key)
            {
                follows_from(previous)
            } else {
                Vec::new()
            };
            let sequence = self.admit_event(elapsed_ns, scope, causes, move |header| {
                DiagnosticEvent::ContextUsageSampled(
                    ContextUsageSampled::new(
                        header,
                        context_used_tokens,
                        context_window_tokens,
                        Some(amount),
                        Some(currency),
                        origin,
                        observed_elapsed_ns,
                    )
                    .expect("A06/A07 emitted validated context and cost values"),
                )
            })?;
            if let Some(session) = state.sessions.get_mut(&key) {
                session.last_sequence = sequence;
            }
            let _ = containing_span_id;
            Ok(ObservationDisposition::Admitted)
        }

        fn result_observed(
            &self,
            candidate: &AgentResultCandidate,
        ) -> Result<ObservationDisposition, AgentDiagnosticErrorCode> {
            let elapsed_ns = self.now()?;
            let metadata = candidate.metadata();
            let act_id = metadata.identity().act_id();
            let lineage = act_producer::lineage_snapshot(act_id).ok_or(ACT_LINEAGE_UNAVAILABLE)?;
            if lineage
                .event_scope()
                .session_generation()
                .map(SchemaU64::get)
                != Some(metadata.session_generation())
            {
                return Err(TURN_IDENTITY_MISMATCH);
            }
            let mut state = lock(&self.state);
            state.turns.entry(act_id.to_owned()).or_insert_with(|| {
                TurnProjection::from_result(metadata, lineage.containing_span_id())
            });
            ensure_turn_identity(&state, metadata.identity(), metadata.session_generation())?;
            self.flush_messages(&mut state, act_id, elapsed_ns.get())?;
            if let Some(transition) = candidate.transition() {
                let turn = state.turns.get_mut(act_id).ok_or(TURN_IDENTITY_MISMATCH)?;
                if turn.pending_rejection_sequence.is_some() {
                    return Err(RESULT_TRANSITION_INVALID);
                }
                let detail = transition.detail().clone();
                let instant = result_instant(transition.instant_kind(), detail)?;
                let sequence = self.admit_instant(
                    elapsed_ns,
                    lineage.event_scope().clone(),
                    instant,
                    Some(lineage.containing_span_id()),
                    follows_from(turn.last_sequence),
                )?;
                turn.last_sequence = sequence;
                if transition.instant_kind() == InstantKind::ResultRejected {
                    turn.pending_rejection_sequence = Some(sequence);
                }
                return Ok(ObservationDisposition::Admitted);
            }
            let counter = candidate
                .validation_rejections()
                .ok_or(RESULT_TRANSITION_INVALID)?;
            if counter.counter_kind() != CounterKind::ResultValidationRejections {
                return Err(RESULT_TRANSITION_INVALID);
            }
            let turn = state.turns.get_mut(act_id).ok_or(TURN_IDENTITY_MISMATCH)?;
            let expected = turn
                .last_rejection_count
                .checked_add(1)
                .ok_or(RESULT_TRANSITION_INVALID)?;
            if counter.value() != expected || turn.pending_rejection_sequence.is_none() {
                return Err(RESULT_TRANSITION_INVALID);
            }
            let source = turn
                .pending_rejection_sequence
                .take()
                .expect("the rejection transition was checked");
            let sequence = self.admit_counter(
                elapsed_ns,
                lineage.act_scope().clone(),
                counter.counter_kind(),
                SchemaU64::new(counter.value()),
                follows_from(source),
            )?;
            turn.last_rejection_count = counter.value();
            turn.last_sequence = sequence;
            Ok(ObservationDisposition::Admitted)
        }

        fn turn_terminal(
            &self,
            metadata: &AgentTurnDiagnosticMetadata,
            outcome: AgentTurnDiagnosticOutcome,
        ) -> Result<(), AgentDiagnosticErrorCode> {
            let act_id = metadata.identity().act_id();
            let mut state = lock(&self.state);
            if !state.turns.contains_key(act_id) {
                return Ok(());
            }
            let elapsed_ns = self.now()?.get();
            let lineage = act_lineage(metadata)?;
            ensure_turn(&mut state, metadata, lineage.containing_span_id())?;
            let messages = state
                .turns
                .get_mut(act_id)
                .expect("the turn projection exists")
                .messages
                .turn_terminal(elapsed_ns, false)
                .map_err(|error| AgentDiagnosticErrorCode::new(error.code()))?;
            self.project_message_candidates(&mut state, act_id, messages)?;
            if state
                .turns
                .get(act_id)
                .is_some_and(|turn| turn.pending_rejection_sequence.is_some())
            {
                return Err(RESULT_TRANSITION_INVALID);
            }
            let thinking = state
                .turns
                .get_mut(act_id)
                .expect("the turn projection exists")
                .thinking
                .turn_terminal(elapsed_ns);
            self.project_thinking_candidates(&mut state, act_id, thinking, Some(outcome))?;
            let tools = state
                .turns
                .get_mut(act_id)
                .expect("the turn projection exists")
                .tools
                .turn_terminal(elapsed_ns);
            self.project_tool_candidates(&mut state, act_id, tools)?;
            let removed = state.turns.remove(act_id);
            debug_assert!(removed.is_some(), "the terminal turn projection must exist");
            Ok(())
        }

        fn flush_messages(
            &self,
            state: &mut BridgeState,
            act_id: &str,
            elapsed_ns: u64,
        ) -> Result<(), AgentDiagnosticErrorCode> {
            let candidates = state
                .turns
                .get_mut(act_id)
                .ok_or(TURN_IDENTITY_MISMATCH)?
                .messages
                .observe_other_candidate(elapsed_ns)
                .map_err(|error| AgentDiagnosticErrorCode::new(error.code()))?;
            self.project_message_candidates(state, act_id, candidates)
                .map(|_| ())
        }

        fn project_message_candidates(
            &self,
            state: &mut BridgeState,
            act_id: &str,
            candidates: Vec<AgentMessageCandidate>,
        ) -> Result<ObservationDisposition, AgentDiagnosticErrorCode> {
            if candidates.is_empty() {
                return Ok(ObservationDisposition::Buffered);
            }
            for candidate in candidates {
                let lineage =
                    act_producer::lineage_snapshot(act_id).ok_or(ACT_LINEAGE_UNAVAILABLE)?;
                let previous = state
                    .turns
                    .get(act_id)
                    .ok_or(TURN_IDENTITY_MISMATCH)?
                    .last_sequence;
                let sequence = if let Some(delta) = candidate.delta() {
                    let message_id = delta.message_id().clone();
                    let source_message_id = delta.source_message_id().map(str::to_owned);
                    let text_delta = delta.text_delta().to_owned();
                    self.admit_event(
                        ElapsedNs::new(delta.elapsed_ns()),
                        lineage.event_scope().clone(),
                        follows_from(previous),
                        move |header| {
                            DiagnosticEvent::AgentMessageDelta(AgentMessageDelta::new(
                                header,
                                message_id,
                                source_message_id,
                                text_delta,
                            ))
                        },
                    )?
                } else if let Some(completed) = candidate.completed() {
                    let message_id = completed.message_id().clone();
                    let utf8_bytes = SchemaU64::new(completed.utf8_bytes());
                    let unicode_scalar_count = SchemaU64::new(completed.unicode_scalar_count());
                    let truncated = completed.truncated();
                    self.admit_event(
                        ElapsedNs::new(completed.elapsed_ns()),
                        lineage.event_scope().clone(),
                        follows_from(previous),
                        move |header| {
                            DiagnosticEvent::AgentMessageCompleted(AgentMessageCompleted::new(
                                header,
                                message_id,
                                utf8_bytes,
                                unicode_scalar_count,
                                truncated,
                            ))
                        },
                    )?
                } else {
                    let gap = candidate.source_gap().ok_or(CANDIDATE_UNSUPPORTED)?;
                    self.admit_gap(
                        ElapsedNs::new(gap.elapsed_ns()),
                        lineage.event_scope().clone(),
                        "agent.message",
                        gap.reason().as_str(),
                        Some(DiagnosticEventKind::AgentMessageDelta),
                        follows_from(previous),
                    )?
                };
                state
                    .turns
                    .get_mut(act_id)
                    .ok_or(TURN_IDENTITY_MISMATCH)?
                    .last_sequence = sequence;
            }
            Ok(ObservationDisposition::Admitted)
        }

        fn project_thinking_candidates(
            &self,
            state: &mut BridgeState,
            act_id: &str,
            candidates: Vec<AgentThinkingCandidate>,
            terminal_outcome: Option<AgentTurnDiagnosticOutcome>,
        ) -> Result<ObservationDisposition, AgentDiagnosticErrorCode> {
            if candidates.is_empty() {
                return Ok(ObservationDisposition::Buffered);
            }
            let mut admitted = false;
            for candidate in candidates {
                let lineage =
                    act_producer::lineage_snapshot(act_id).ok_or(ACT_LINEAGE_UNAVAILABLE)?;
                let turn = state.turns.get_mut(act_id).ok_or(TURN_IDENTITY_MISMATCH)?;
                if let Some(activity) = candidate.activity() {
                    match activity.phase() {
                        AgentThinkingActivityPhase::Start => {
                            if turn.thinking_span.is_some() {
                                return Err(THINKING_TRANSITION_INVALID);
                            }
                            let scope = lineage.event_scope().clone();
                            let span_id = self.admit_span_start(
                                ElapsedNs::new(activity.elapsed_ns()),
                                scope.clone(),
                                SpanStartDetail::AgentThinking(EmptyDetail::new()),
                                Some(lineage.containing_span_id()),
                                follows_from(turn.last_sequence),
                            )?;
                            turn.thinking_span = Some(ThinkingProjection { span_id, scope });
                            turn.last_sequence = span_id;
                            admitted = true;
                        }
                        AgentThinkingActivityPhase::Progress => {
                            if turn.thinking_span.is_none() {
                                return Err(THINKING_TRANSITION_INVALID);
                            }
                        }
                        AgentThinkingActivityPhase::Finish => {
                            let span = turn
                                .thinking_span
                                .take()
                                .ok_or(THINKING_TRANSITION_INVALID)?;
                            let (outcome, error_code) = thinking_terminal(terminal_outcome);
                            let sequence = self.admit_span_finish(
                                ElapsedNs::new(activity.elapsed_ns()),
                                span.scope,
                                span.span_id,
                                outcome,
                                error_code.map(str::to_owned),
                                follows_from(turn.last_sequence),
                            )?;
                            turn.last_sequence = sequence;
                            admitted = true;
                        }
                    }
                } else {
                    let gap = candidate.source_gap().ok_or(CANDIDATE_UNSUPPORTED)?;
                    let sequence = self.admit_gap(
                        ElapsedNs::new(gap.elapsed_ns()),
                        lineage.event_scope().clone(),
                        "agent.thinking",
                        gap.reason().as_str(),
                        None,
                        follows_from(turn.last_sequence),
                    )?;
                    turn.last_sequence = sequence;
                    admitted = true;
                }
            }
            Ok(if admitted {
                ObservationDisposition::Admitted
            } else {
                ObservationDisposition::Buffered
            })
        }

        fn project_tool_candidates(
            &self,
            state: &mut BridgeState,
            act_id: &str,
            candidates: Vec<AgentToolCandidate>,
        ) -> Result<ObservationDisposition, AgentDiagnosticErrorCode> {
            if candidates.is_empty() {
                return Ok(ObservationDisposition::Buffered);
            }
            for candidate in candidates {
                let lineage =
                    act_producer::lineage_snapshot(act_id).ok_or(ACT_LINEAGE_UNAVAILABLE)?;
                let previous = state
                    .turns
                    .get(act_id)
                    .ok_or(TURN_IDENTITY_MISMATCH)?
                    .last_sequence;
                let sequence = if let Some(started) = candidate.started() {
                    let source_tool_call_id = started.metadata().source_tool_call_id().to_owned();
                    if state
                        .turns
                        .get(act_id)
                        .is_some_and(|turn| turn.tool_spans.contains_key(&source_tool_call_id))
                    {
                        return Err(TOOL_TRANSITION_INVALID);
                    }
                    let tool_id = next_tool_id(state)?;
                    let scope = tool_scope(lineage.event_scope(), tool_id);
                    let canonical_tool_call_id = scope
                        .tool_call_id()
                        .ok_or(IDENTIFIER_INVALID)?
                        .as_str()
                        .to_owned();
                    let detail = tool_detail(started.metadata(), None);
                    self.deliver_pending_tool_payload(
                        state,
                        act_id,
                        &source_tool_call_id,
                        ToolPayloadSource::Started,
                        &canonical_tool_call_id,
                    )?;
                    let span_id = self.admit_span_start(
                        ElapsedNs::new(started.elapsed_ns()),
                        scope.clone(),
                        SpanStartDetail::ToolCall(detail),
                        Some(lineage.containing_span_id()),
                        follows_from(previous),
                    )?;
                    state
                        .turns
                        .get_mut(act_id)
                        .ok_or(TURN_IDENTITY_MISMATCH)?
                        .tool_spans
                        .insert(
                            source_tool_call_id,
                            ToolProjection {
                                span_id,
                                scope,
                                finished: false,
                            },
                        );
                    span_id
                } else if let Some(updated) = candidate.updated() {
                    let source_tool_call_id = updated.metadata().source_tool_call_id();
                    let (tool_scope, tool_span_id, canonical_tool_call_id) = {
                        let tool = state
                            .turns
                            .get(act_id)
                            .and_then(|turn| turn.tool_spans.get(source_tool_call_id))
                            .ok_or(TOOL_TRANSITION_INVALID)?;
                        if tool.finished {
                            return Err(TOOL_TRANSITION_INVALID);
                        }
                        let canonical_tool_call_id = tool
                            .scope
                            .tool_call_id()
                            .ok_or(IDENTIFIER_INVALID)?
                            .as_str()
                            .to_owned();
                        (tool.scope.clone(), tool.span_id, canonical_tool_call_id)
                    };
                    self.deliver_pending_tool_payload(
                        state,
                        act_id,
                        source_tool_call_id,
                        ToolPayloadSource::Updated,
                        &canonical_tool_call_id,
                    )?;
                    self.admit_instant(
                        ElapsedNs::new(updated.elapsed_ns()),
                        tool_scope,
                        InstantDetail::ToolUpdated(tool_detail(updated.metadata(), None)),
                        Some(tool_span_id),
                        follows_from(previous),
                    )?
                } else if let Some(finished) = candidate.finished() {
                    let tool = state
                        .turns
                        .get_mut(act_id)
                        .and_then(|turn| turn.tool_spans.get_mut(finished.source_tool_call_id()))
                        .ok_or(TOOL_TRANSITION_INVALID)?;
                    if tool.finished {
                        return Err(TOOL_TRANSITION_INVALID);
                    }
                    let (outcome, fallback_error) = tool_terminal(finished.outcome());
                    let error_code = finished
                        .error_code()
                        .map(|code| code.as_str())
                        .or(fallback_error)
                        .map(str::to_owned);
                    let sequence = self.admit_span_finish(
                        ElapsedNs::new(finished.elapsed_ns()),
                        tool.scope.clone(),
                        tool.span_id,
                        outcome,
                        error_code,
                        follows_from(previous),
                    )?;
                    tool.finished = true;
                    sequence
                } else {
                    let gap = candidate.source_gap().ok_or(CANDIDATE_UNSUPPORTED)?;
                    let affected_scope = gap.source_tool_call_id().and_then(|source| {
                        state
                            .turns
                            .get(act_id)
                            .and_then(|turn| turn.tool_spans.get(source))
                            .map(|tool| tool.scope.clone())
                    });
                    self.admit_gap_with_scope(
                        ElapsedNs::new(gap.elapsed_ns()),
                        lineage.event_scope().clone(),
                        "agent.tool",
                        gap.reason().as_str(),
                        None,
                        affected_scope,
                        follows_from(previous),
                    )?
                };
                state
                    .turns
                    .get_mut(act_id)
                    .ok_or(TURN_IDENTITY_MISMATCH)?
                    .last_sequence = sequence;
            }
            Ok(ObservationDisposition::Admitted)
        }

        fn deliver_pending_tool_payload(
            &self,
            state: &mut BridgeState,
            act_id: &str,
            source_tool_call_id: &str,
            source: ToolPayloadSource,
            canonical_tool_call_id: &str,
        ) -> Result<(), AgentDiagnosticErrorCode> {
            let Some(subscribers) = &self.subscribers else {
                return Ok(());
            };
            let turn = state.turns.get_mut(act_id).ok_or(TURN_IDENTITY_MISMATCH)?;
            let Some(index) = turn.pending_tool_payloads.iter().position(|payload| {
                payload.tool_call_id() == source_tool_call_id && payload.source() == source
            }) else {
                return Ok(());
            };
            let payload = turn.pending_tool_payloads.remove(index);
            subscribers.deliver_tool_payload(act_id, canonical_tool_call_id, &payload);
            Ok(())
        }

        fn now(&self) -> Result<ElapsedNs, AgentDiagnosticErrorCode> {
            self.clock.elapsed_now().map_err(|_| CLOCK_FAILED)
        }

        fn admit_event<F>(
            &self,
            elapsed_ns: ElapsedNs,
            scope: DiagnosticScope,
            caused_by: Vec<CausalLink>,
            build: F,
        ) -> Result<SchemaU64, AgentDiagnosticErrorCode>
        where
            F: FnOnce(DiagnosticEventHeader) -> DiagnosticEvent + Send + 'static,
        {
            let subscriber = scope.act_id().and_then(|act_id| {
                self.subscribers
                    .as_ref()
                    .and_then(|lookup| lookup.subscriber_for(act_id.as_str()))
            });
            self.admission.admit(
                Box::new(move |identity| {
                    let header = DiagnosticEventHeader::new(
                        identity.run_id(),
                        identity.sequence(),
                        elapsed_ns,
                        scope,
                        caused_by,
                    )
                    .expect("hub identities always have a nonzero sequence");
                    build(header)
                }),
                subscriber.as_deref(),
            )
        }

        fn admit_span_start(
            &self,
            elapsed_ns: ElapsedNs,
            scope: DiagnosticScope,
            detail: SpanStartDetail,
            parent_span_id: Option<SchemaU64>,
            caused_by: Vec<CausalLink>,
        ) -> Result<SchemaU64, AgentDiagnosticErrorCode> {
            self.admit_event(elapsed_ns, scope, caused_by, move |header| {
                DiagnosticEvent::SpanStarted(SpanStarted::new(header, detail, parent_span_id))
            })
        }

        #[allow(clippy::too_many_arguments)]
        fn admit_span_finish(
            &self,
            elapsed_ns: ElapsedNs,
            scope: DiagnosticScope,
            span_id: SchemaU64,
            outcome: SpanOutcome,
            error_code: Option<String>,
            caused_by: Vec<CausalLink>,
        ) -> Result<SchemaU64, AgentDiagnosticErrorCode> {
            self.admit_event(elapsed_ns, scope, caused_by, move |header| {
                DiagnosticEvent::SpanFinished(SpanFinished::new(
                    header, span_id, outcome, error_code,
                ))
            })
        }

        fn admit_instant(
            &self,
            elapsed_ns: ElapsedNs,
            scope: DiagnosticScope,
            detail: InstantDetail,
            containing_span_id: Option<SchemaU64>,
            caused_by: Vec<CausalLink>,
        ) -> Result<SchemaU64, AgentDiagnosticErrorCode> {
            self.admit_event(elapsed_ns, scope, caused_by, move |header| {
                DiagnosticEvent::InstantOccurred(InstantOccurred::new(
                    header,
                    detail,
                    containing_span_id,
                ))
            })
        }

        fn admit_counter(
            &self,
            elapsed_ns: ElapsedNs,
            scope: DiagnosticScope,
            kind: troupe_diagnostics_core::kinds::CounterKind,
            value: SchemaU64,
            caused_by: Vec<CausalLink>,
        ) -> Result<SchemaU64, AgentDiagnosticErrorCode> {
            self.admit_event(elapsed_ns, scope, caused_by, move |header| {
                DiagnosticEvent::CounterSampled(CounterSampled::new(header, kind, value))
            })
        }

        fn admit_gap(
            &self,
            elapsed_ns: ElapsedNs,
            scope: DiagnosticScope,
            producer: &'static str,
            reason: &'static str,
            affected_kind: Option<DiagnosticEventKind>,
            caused_by: Vec<CausalLink>,
        ) -> Result<SchemaU64, AgentDiagnosticErrorCode> {
            self.admit_gap_with_scope(
                elapsed_ns,
                scope,
                producer,
                reason,
                affected_kind,
                None,
                caused_by,
            )
        }

        #[allow(clippy::too_many_arguments)]
        fn admit_gap_with_scope(
            &self,
            elapsed_ns: ElapsedNs,
            scope: DiagnosticScope,
            producer: &'static str,
            reason: &'static str,
            affected_kind: Option<DiagnosticEventKind>,
            affected_scope: Option<DiagnosticScope>,
            caused_by: Vec<CausalLink>,
        ) -> Result<SchemaU64, AgentDiagnosticErrorCode> {
            self.admit_event(elapsed_ns, scope, caused_by, move |header| {
                DiagnosticEvent::ObservationGap(ObservationGap::new(
                    header,
                    producer.to_owned(),
                    None,
                    reason.to_owned(),
                    None,
                    None,
                    affected_kind,
                    affected_scope,
                ))
            })
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn session_mut<'a>(
        state: &'a mut BridgeState,
        key: &SessionKey,
        metadata: &AgentSessionDiagnosticMetadata,
    ) -> Result<&'a mut SessionProjection, AgentDiagnosticErrorCode> {
        let session = state
            .sessions
            .get_mut(key)
            .ok_or(SESSION_TRANSITION_INVALID)?;
        if session.provider != metadata.provider()
            || session.requested_model != metadata.requested_model()
        {
            return Err(SESSION_TRANSITION_INVALID);
        }
        Ok(session)
    }

    fn ensure_turn(
        state: &mut BridgeState,
        metadata: &AgentTurnDiagnosticMetadata,
        initial_sequence: SchemaU64,
    ) -> Result<(), AgentDiagnosticErrorCode> {
        let act_id = metadata.identity().act_id();
        match state.turns.get(act_id) {
            Some(turn)
                if turn.identity != *metadata.identity()
                    || turn.session_generation != metadata.session_generation() =>
            {
                Err(TURN_IDENTITY_MISMATCH)
            }
            Some(_) => Ok(()),
            None => {
                state.turns.insert(
                    act_id.to_owned(),
                    TurnProjection::new(metadata, initial_sequence),
                );
                Ok(())
            }
        }
    }

    fn ensure_turn_identity(
        state: &BridgeState,
        identity: &troupe_agent_runtime::AgentTurnDiagnosticIdentity,
        generation: u64,
    ) -> Result<(), AgentDiagnosticErrorCode> {
        let turn = state
            .turns
            .get(identity.act_id())
            .ok_or(TURN_IDENTITY_MISMATCH)?;
        if turn.identity != *identity || turn.session_generation != generation {
            return Err(TURN_IDENTITY_MISMATCH);
        }
        Ok(())
    }

    fn act_lineage(
        metadata: &AgentTurnDiagnosticMetadata,
    ) -> Result<act_producer::ActLineageSnapshot, AgentDiagnosticErrorCode> {
        let lineage = act_producer::lineage_snapshot(metadata.identity().act_id())
            .ok_or(ACT_LINEAGE_UNAVAILABLE)?;
        if lineage
            .event_scope()
            .session_generation()
            .map(SchemaU64::get)
            != Some(metadata.session_generation())
        {
            return Err(TURN_IDENTITY_MISMATCH);
        }
        Ok(lineage)
    }

    fn session_candidate_lineage(
        state: &BridgeState,
        metadata: &AgentSessionDiagnosticMetadata,
    ) -> Result<(DiagnosticScope, Option<SchemaU64>, SchemaU64), AgentDiagnosticErrorCode> {
        let key = SessionKey::new(metadata.context());
        let scope = session_scope(metadata.context(), metadata.generation())?;
        let session = state.sessions.get(&key).ok_or(SESSION_TRANSITION_INVALID)?;
        let containing = session
            .closing
            .as_ref()
            .map_or(session.lifecycle.span_id, |closing| closing.span_id);
        Ok((scope, Some(containing), session.last_sequence))
    }

    fn update_candidate_sequence(
        state: &mut BridgeState,
        metadata: &AgentPlanSnapshotMetadata,
        sequence: SchemaU64,
    ) -> Result<(), AgentDiagnosticErrorCode> {
        match metadata {
            AgentPlanSnapshotMetadata::Turn(metadata) => {
                state
                    .turns
                    .get_mut(metadata.identity().act_id())
                    .ok_or(TURN_IDENTITY_MISMATCH)?
                    .last_sequence = sequence;
            }
            AgentPlanSnapshotMetadata::Session(metadata) => {
                state
                    .sessions
                    .get_mut(&SessionKey::new(metadata.context()))
                    .ok_or(SESSION_TRANSITION_INVALID)?
                    .last_sequence = sequence;
            }
        }
        Ok(())
    }

    fn update_context_metadata_sequence(
        state: &mut BridgeState,
        metadata: &AgentContextOccupancyMetadata,
        sequence: SchemaU64,
    ) -> Result<(), AgentDiagnosticErrorCode> {
        match metadata {
            AgentContextOccupancyMetadata::Turn(metadata) => {
                state
                    .turns
                    .get_mut(metadata.identity().act_id())
                    .ok_or(TURN_IDENTITY_MISMATCH)?
                    .last_sequence = sequence;
            }
            AgentContextOccupancyMetadata::Session(metadata) => {
                state
                    .sessions
                    .get_mut(&SessionKey::new(metadata.context()))
                    .ok_or(SESSION_TRANSITION_INVALID)?
                    .last_sequence = sequence;
            }
        }
        Ok(())
    }

    fn session_scope(
        context: &AgentSessionDiagnosticContext,
        generation: Option<u64>,
    ) -> Result<DiagnosticScope, AgentDiagnosticErrorCode> {
        let actor_id = RunLocalId::parse(context.actor_id()).map_err(|_| IDENTIFIER_INVALID)?;
        Ok(DiagnosticScope::new(
            None,
            Some(actor_id),
            None,
            None,
            None,
            None,
            generation.map(SchemaU64::new),
        ))
    }

    fn tool_scope(scope: &DiagnosticScope, tool_call_id: RunLocalId) -> DiagnosticScope {
        DiagnosticScope::new(
            scope.scene_id().cloned(),
            scope.actor_id().cloned(),
            scope.cue_id().cloned(),
            None,
            scope.act_id().cloned(),
            Some(tool_call_id),
            scope.session_generation(),
        )
    }

    fn next_tool_id(state: &mut BridgeState) -> Result<RunLocalId, AgentDiagnosticErrorCode> {
        let value = state.next_tool_id;
        state.next_tool_id = value.checked_add(1).ok_or(IDENTIFIER_INVALID)?;
        RunLocalId::parse(&format!("tool-{value}")).map_err(|_| IDENTIFIER_INVALID)
    }

    fn session_detail(metadata: &AgentSessionDiagnosticMetadata) -> AgentSessionDetail {
        AgentSessionDetail::new(
            metadata.provider().name().to_owned(),
            effective_model(metadata),
            effective_effort(metadata),
        )
    }

    fn effective_model(metadata: &AgentSessionDiagnosticMetadata) -> Option<String> {
        Some(
            metadata
                .effective_model()
                .unwrap_or_else(|| metadata.requested_model())
                .to_owned(),
        )
    }

    fn effective_effort(metadata: &AgentSessionDiagnosticMetadata) -> Option<String> {
        metadata
            .effective_effort()
            .or_else(|| metadata.requested_effort())
            .map(str::to_owned)
    }

    fn result_instant(
        kind: InstantKind,
        detail: troupe_diagnostics_core::detail::ResultTransitionDetail,
    ) -> Result<InstantDetail, AgentDiagnosticErrorCode> {
        match kind {
            InstantKind::ResultSubmitted => Ok(InstantDetail::ResultSubmitted(detail)),
            InstantKind::ResultRejected => Ok(InstantDetail::ResultRejected(detail)),
            InstantKind::ResultRepairRequested => Ok(InstantDetail::ResultRepairRequested(detail)),
            InstantKind::ResultAccepted => Ok(InstantDetail::ResultAccepted(detail)),
            InstantKind::ResultMissing => Ok(InstantDetail::ResultMissing(detail)),
            _ => Err(RESULT_TRANSITION_INVALID),
        }
    }

    fn tool_detail(
        metadata: &troupe_agent_runtime::diagnostics::tool::AgentToolMetadata,
        error_code: Option<String>,
    ) -> ToolCallDetail {
        ToolCallDetail::new(
            metadata.title().to_owned(),
            metadata.tool_kind(),
            metadata.status(),
            error_code,
        )
    }

    fn tool_terminal(outcome: AgentToolTerminalOutcome) -> (SpanOutcome, Option<&'static str>) {
        match outcome {
            AgentToolTerminalOutcome::Completed => (SpanOutcome::Completed, None),
            AgentToolTerminalOutcome::Failed => (SpanOutcome::Failed, Some("tool_failed")),
            AgentToolTerminalOutcome::Cancelled => (SpanOutcome::Cancelled, Some("tool_cancelled")),
        }
    }

    fn thinking_terminal(
        outcome: Option<AgentTurnDiagnosticOutcome>,
    ) -> (SpanOutcome, Option<&'static str>) {
        match outcome {
            None | Some(AgentTurnDiagnosticOutcome::Completed) => (SpanOutcome::Completed, None),
            Some(AgentTurnDiagnosticOutcome::Cancelled) => {
                (SpanOutcome::Cancelled, Some("agent-thinking-cancelled"))
            }
            Some(AgentTurnDiagnosticOutcome::Failed) => {
                (SpanOutcome::Failed, Some("agent-thinking-failed"))
            }
        }
    }

    fn follows_from(sequence: SchemaU64) -> Vec<CausalLink> {
        caused_by(sequence, CausalRelation::FollowsFrom)
    }

    fn caused_by(sequence: SchemaU64, relation: CausalRelation) -> Vec<CausalLink> {
        vec![CausalLink::new(sequence, relation)]
    }
}

#[cfg(not(test))]
#[allow(unused_imports)]
pub(crate) use active::{CanonicalObservationBridge, ObservationDisposition};
