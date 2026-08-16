use std::collections::VecDeque;
use std::fmt;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use agent_client_protocol::schema::v1::{SessionId, SessionUpdate};
use uuid::Uuid;

use super::observer::{
    AgentDiagnosticErrorCode, AgentDiagnosticObservation, AgentDiagnosticObserver,
    AgentTurnDiagnosticSettlement,
};
#[cfg(test)]
use super::observer::AgentTurnDiagnosticOutcome;
use super::payload::ToolPayloadCapturePolicy;
use super::usage::{TurnTerminalObservation, TurnTerminalSettlement};
use super::{context, cost, message, payload, plan, thinking, tool};
use crate::profile::{AgentKind, ResolvedAgentProfile};
use crate::session::AgentReadySnapshot;
use crate::session::turn::AgentTurnControl;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AgentDiagnosticProvider {
    Codex,
    Claude,
    Kimi,
}

impl AgentDiagnosticProvider {
    pub(crate) const fn from_agent_kind(agent: AgentKind) -> Self {
        match agent {
            AgentKind::Codex => Self::Codex,
            AgentKind::Claude => Self::Claude,
            AgentKind::Kimi => Self::Kimi,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Kimi => "kimi",
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AgentSessionDiagnosticContext {
    actor_id: Arc<str>,
    session_id: Arc<str>,
}

impl AgentSessionDiagnosticContext {
    pub fn new(actor_id: impl Into<Arc<str>>, session_id: impl Into<Arc<str>>) -> Self {
        Self {
            actor_id: actor_id.into(),
            session_id: session_id.into(),
        }
    }

    pub fn actor_id(&self) -> &str {
        &self.actor_id
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSessionDiagnosticMetadata {
    context: AgentSessionDiagnosticContext,
    provider: AgentDiagnosticProvider,
    generation: Option<u64>,
    provider_session_id: Option<Arc<str>>,
    requested_model: Arc<str>,
    requested_effort: Option<Arc<str>>,
    effective_model: Option<Arc<str>>,
    effective_effort: Option<Arc<str>>,
}

impl AgentSessionDiagnosticMetadata {
    pub fn context(&self) -> &AgentSessionDiagnosticContext {
        &self.context
    }

    pub const fn provider(&self) -> AgentDiagnosticProvider {
        self.provider
    }

    pub const fn generation(&self) -> Option<u64> {
        self.generation
    }

    pub fn provider_session_id(&self) -> Option<&str> {
        self.provider_session_id.as_deref()
    }

    pub fn requested_model(&self) -> &str {
        &self.requested_model
    }

    pub fn requested_effort(&self) -> Option<&str> {
        self.requested_effort.as_deref()
    }

    pub fn effective_model(&self) -> Option<&str> {
        self.effective_model.as_deref()
    }

    pub fn effective_effort(&self) -> Option<&str> {
        self.effective_effort.as_deref()
    }

    fn opening(
        context: AgentSessionDiagnosticContext,
        provider: AgentDiagnosticProvider,
        profile: &ResolvedAgentProfile,
    ) -> Self {
        Self {
            context,
            provider,
            generation: None,
            provider_session_id: None,
            requested_model: Arc::from(profile.requested_model.as_str()),
            requested_effort: profile.requested_effort.as_deref().map(Arc::<str>::from),
            effective_model: None,
            effective_effort: None,
        }
    }

    pub(crate) fn snapshot(
        context: AgentSessionDiagnosticContext,
        provider: AgentDiagnosticProvider,
        profile: &ResolvedAgentProfile,
        ready: Option<&AgentReadySnapshot>,
    ) -> Self {
        let mut metadata = Self::opening(context, provider, profile);
        if let Some(ready) = ready {
            metadata.apply_ready(ready);
        }
        metadata
    }

    fn apply_ready(&mut self, snapshot: &AgentReadySnapshot) {
        self.generation = Some(snapshot.generation);
        self.provider_session_id = Some(Arc::from(snapshot.session_id.as_str()));
        self.effective_model = Some(Arc::from(snapshot.effective_model.as_str()));
        self.effective_effort = snapshot.effective_effort.as_deref().map(Arc::<str>::from);
    }

    #[cfg(test)]
    pub(crate) fn for_test(context: AgentSessionDiagnosticContext) -> Self {
        Self {
            context,
            provider: AgentDiagnosticProvider::Codex,
            generation: None,
            provider_session_id: None,
            requested_model: Arc::from("test-model"),
            requested_effort: None,
            effective_model: None,
            effective_effort: None,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AgentTurnDiagnosticIdentity {
    session: AgentSessionDiagnosticContext,
    act_id: Arc<str>,
    turn_id: Arc<str>,
}

impl AgentTurnDiagnosticIdentity {
    pub fn new(
        session: AgentSessionDiagnosticContext,
        act_id: impl Into<Arc<str>>,
        turn_id: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            session,
            act_id: act_id.into(),
            turn_id: turn_id.into(),
        }
    }

    pub fn session(&self) -> &AgentSessionDiagnosticContext {
        &self.session
    }

    pub fn act_id(&self) -> &str {
        &self.act_id
    }

    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentTurnDiagnosticMetadata {
    identity: AgentTurnDiagnosticIdentity,
    provider: AgentDiagnosticProvider,
    session_generation: u64,
    provider_session_id: Arc<str>,
    effective_model: Arc<str>,
    effective_effort: Option<Arc<str>>,
    operation_id: Uuid,
    turn_index: u64,
}

impl AgentTurnDiagnosticMetadata {
    pub fn identity(&self) -> &AgentTurnDiagnosticIdentity {
        &self.identity
    }

    pub const fn provider(&self) -> AgentDiagnosticProvider {
        self.provider
    }

    pub const fn session_generation(&self) -> u64 {
        self.session_generation
    }

    pub fn provider_session_id(&self) -> &str {
        &self.provider_session_id
    }

    pub fn effective_model(&self) -> &str {
        &self.effective_model
    }

    pub fn effective_effort(&self) -> Option<&str> {
        self.effective_effort.as_deref()
    }

    pub const fn operation_id(&self) -> Uuid {
        self.operation_id
    }

    pub const fn turn_index(&self) -> u64 {
        self.turn_index
    }
}

struct SessionDiagnosticLifecycle {
    observer: AgentDiagnosticObserver,
    metadata: Mutex<Arc<AgentSessionDiagnosticMetadata>>,
    state: Mutex<SessionDiagnosticLifecycleState>,
    #[cfg(test)]
    closing_observations: AtomicUsize,
    #[cfg(test)]
    closed_observations: AtomicUsize,
    #[cfg(test)]
    observations: Mutex<Vec<&'static str>>,
}

#[derive(Default)]
struct SessionDiagnosticLifecycleState {
    opening: bool,
    latest_opening_attempt: Option<u64>,
    ready: bool,
    broken: bool,
    closing: bool,
    cleanup_complete: bool,
    closed: bool,
    dispatching: bool,
    pending: VecDeque<AgentDiagnosticObservation>,
}

impl SessionDiagnosticLifecycle {
    fn new(observer: AgentDiagnosticObserver, metadata: AgentSessionDiagnosticMetadata) -> Self {
        Self {
            observer,
            metadata: Mutex::new(Arc::new(metadata)),
            state: Mutex::new(SessionDiagnosticLifecycleState::default()),
            #[cfg(test)]
            closing_observations: AtomicUsize::new(0),
            #[cfg(test)]
            closed_observations: AtomicUsize::new(0),
            #[cfg(test)]
            observations: Mutex::new(Vec::new()),
        }
    }

    fn metadata(&self) -> Arc<AgentSessionDiagnosticMetadata> {
        Arc::clone(&lock(&self.metadata))
    }

    fn enqueue(
        state: &mut SessionDiagnosticLifecycleState,
        observation: AgentDiagnosticObservation,
    ) -> bool {
        state.pending.push_back(observation);
        if state.dispatching {
            false
        } else {
            state.dispatching = true;
            true
        }
    }

    fn drain_observations(&self) {
        loop {
            let observation = {
                let mut state = lock(&self.state);
                let Some(observation) = state.pending.pop_front() else {
                    state.dispatching = false;
                    return;
                };
                observation
            };
            #[cfg(test)]
            let closing = matches!(&observation, AgentDiagnosticObservation::SessionClosing(_));
            #[cfg(test)]
            let closed = matches!(&observation, AgentDiagnosticObservation::SessionClosed(_));
            self.observer.observe(observation);
            #[cfg(test)]
            if closing {
                self.closing_observations.fetch_add(1, Ordering::AcqRel);
                lock(&self.observations).push("closing");
            }
            #[cfg(test)]
            if closed {
                self.closed_observations.fetch_add(1, Ordering::AcqRel);
                lock(&self.observations).push("closed");
            }
        }
    }

    fn observe_opening(&self) {
        let should_drain = {
            let mut state = lock(&self.state);
            if state.opening {
                return;
            }
            state.opening = true;
            Self::enqueue(
                &mut state,
                AgentDiagnosticObservation::SessionOpening(self.metadata()),
            )
        };
        if should_drain {
            self.drain_observations();
        }
    }

    fn observe_opening_attempt(&self, generation: u64) {
        let should_drain = {
            let mut state = lock(&self.state);
            if state
                .latest_opening_attempt
                .is_some_and(|latest| generation <= latest)
            {
                return;
            }
            state.latest_opening_attempt = Some(generation);
            let observation = {
                let mut metadata = lock(&self.metadata);
                let mut next = (**metadata).clone();
                next.generation = Some(generation);
                next.provider_session_id = None;
                next.effective_model = None;
                next.effective_effort = None;
                *metadata = Arc::new(next);
                AgentDiagnosticObservation::SessionOpeningAttempt(Arc::clone(&metadata))
            };
            Self::enqueue(&mut state, observation)
        };
        if should_drain {
            self.drain_observations();
        }
    }

    fn observe_ready(&self, snapshot: &AgentReadySnapshot) {
        let should_drain = {
            let mut state = lock(&self.state);
            if state.ready {
                return;
            }
            state.ready = true;
            let observation = {
                let mut metadata = lock(&self.metadata);
                let mut next = (**metadata).clone();
                next.apply_ready(snapshot);
                *metadata = Arc::new(next);
                AgentDiagnosticObservation::SessionReady(Arc::clone(&metadata))
            };
            Self::enqueue(&mut state, observation)
        };
        if should_drain {
            self.drain_observations();
        }
    }

    fn observe_broken(&self, code: &'static str) {
        let should_drain = {
            let mut state = lock(&self.state);
            if state.broken {
                return;
            }
            state.broken = true;
            Self::enqueue(
                &mut state,
                AgentDiagnosticObservation::SessionBroken {
                    metadata: self.metadata(),
                    error_code: AgentDiagnosticErrorCode::new(code),
                },
            )
        };
        if should_drain {
            self.drain_observations();
        }
    }

    fn observe_closing(&self) {
        let should_drain = {
            let mut state = lock(&self.state);
            let mut should_drain = false;
            if !state.closing {
                state.closing = true;
                should_drain |= Self::enqueue(
                    &mut state,
                    AgentDiagnosticObservation::SessionClosing(self.metadata()),
                );
            }
            if state.cleanup_complete && !state.closed {
                state.closed = true;
                should_drain |= Self::enqueue(
                    &mut state,
                    AgentDiagnosticObservation::SessionClosed(self.metadata()),
                );
            }
            should_drain
        };
        if should_drain {
            self.drain_observations();
        }
    }

    fn observe_cleanup_complete(&self) {
        let should_drain = {
            let mut state = lock(&self.state);
            state.cleanup_complete = true;
            if !state.closing || state.closed {
                return;
            }
            state.closed = true;
            Self::enqueue(
                &mut state,
                AgentDiagnosticObservation::SessionClosed(self.metadata()),
            )
        };
        if should_drain {
            self.drain_observations();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentDiagnosticSnapshotError {
    ProfileUnavailable,
}

impl fmt::Display for AgentDiagnosticSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProfileUnavailable => {
                formatter.write_str("agent session diagnostic profile is unavailable")
            }
        }
    }
}

impl std::error::Error for AgentDiagnosticSnapshotError {}

#[derive(Clone)]
pub(crate) struct SessionDiagnosticCleanupHandle {
    lifecycle: Arc<SessionDiagnosticLifecycle>,
}

impl SessionDiagnosticCleanupHandle {
    pub(crate) fn complete(&self) {
        self.lifecycle.observe_cleanup_complete();
    }

    #[cfg(test)]
    pub(crate) fn lifecycle_counts(&self) -> (usize, usize) {
        (
            self.lifecycle.closing_observations.load(Ordering::Acquire),
            self.lifecycle.closed_observations.load(Ordering::Acquire),
        )
    }

    #[cfg(test)]
    pub(crate) fn lifecycle_observations(&self) -> Vec<&'static str> {
        lock(&self.lifecycle.observations).clone()
    }
}

pub(crate) struct SessionDiagnostics {
    observer: Option<AgentDiagnosticObserver>,
    lifecycle: Option<Arc<SessionDiagnosticLifecycle>>,
}

impl SessionDiagnostics {
    #[cfg(test)]
    pub(crate) const fn new(observer: Option<AgentDiagnosticObserver>) -> Self {
        Self {
            observer,
            lifecycle: None,
        }
    }

    pub(crate) fn from_profile(
        observer: Option<AgentDiagnosticObserver>,
        context: Option<AgentSessionDiagnosticContext>,
        profile: &ResolvedAgentProfile,
    ) -> Self {
        let lifecycle = observer.as_ref().zip(context).map(|(observer, context)| {
            Arc::new(SessionDiagnosticLifecycle::new(
                observer.clone(),
                AgentSessionDiagnosticMetadata::opening(
                    context,
                    AgentDiagnosticProvider::from_agent_kind(profile.agent),
                    profile,
                ),
            ))
        });
        Self {
            observer,
            lifecycle,
        }
    }

    pub(crate) fn observer(&self) -> Option<&AgentDiagnosticObserver> {
        self.observer.as_ref()
    }

    pub(crate) fn context(&self) -> Option<AgentSessionDiagnosticContext> {
        self.lifecycle
            .as_ref()
            .map(|lifecycle| lifecycle.metadata())
            .map(|metadata| metadata.context.clone())
    }

    pub(crate) fn metadata(&self) -> Option<Arc<AgentSessionDiagnosticMetadata>> {
        self.lifecycle
            .as_ref()
            .map(|lifecycle| lifecycle.metadata())
    }

    pub(crate) fn cleanup_handle(&self) -> Option<SessionDiagnosticCleanupHandle> {
        self.lifecycle
            .as_ref()
            .map(|lifecycle| SessionDiagnosticCleanupHandle {
                lifecycle: Arc::clone(lifecycle),
            })
    }

    #[cfg(test)]
    pub(crate) fn for_test(observer: AgentDiagnosticObserver) -> Self {
        let context = AgentSessionDiagnosticContext::new("actor-test", "session-test");
        let lifecycle = Arc::new(SessionDiagnosticLifecycle::new(
            observer.clone(),
            AgentSessionDiagnosticMetadata::for_test(context),
        ));
        Self {
            observer: Some(observer),
            lifecycle: Some(lifecycle),
        }
    }

    fn update_context<'a>(
        &'a self,
        turn: Option<&'a TurnDiagnosticContext>,
        session_id: &'a SessionId,
    ) -> Option<AgentDiagnosticUpdateContext<'a>> {
        let observer = turn
            .and_then(TurnDiagnosticContext::effective_observer)
            .or(self.observer.as_ref())?;
        Some(AgentDiagnosticUpdateContext {
            observer,
            turn,
            session_id,
            session: self.metadata(),
        })
    }
}

#[derive(Clone)]
pub struct TurnDiagnosticContext {
    target: Weak<AgentTurnControl>,
    identity: AgentTurnDiagnosticIdentity,
    standalone_observer: Option<AgentDiagnosticObserver>,
    effective_observer: Option<AgentDiagnosticObserver>,
    tool_payload_capture: ToolPayloadCapturePolicy,
    runtime_metadata: Option<Arc<AgentTurnDiagnosticMetadata>>,
}

impl TurnDiagnosticContext {
    pub(crate) fn new(
        target: &Arc<AgentTurnControl>,
        identity: AgentTurnDiagnosticIdentity,
        standalone_observer: Option<AgentDiagnosticObserver>,
        tool_payload_capture: ToolPayloadCapturePolicy,
    ) -> Self {
        Self {
            target: Arc::downgrade(target),
            identity,
            standalone_observer,
            effective_observer: None,
            tool_payload_capture,
            runtime_metadata: None,
        }
    }

    pub fn act_id(&self) -> &str {
        self.identity.act_id()
    }

    pub fn identity(&self) -> &AgentTurnDiagnosticIdentity {
        &self.identity
    }

    pub fn runtime_metadata(&self) -> Option<&AgentTurnDiagnosticMetadata> {
        self.runtime_metadata.as_deref()
    }

    pub fn effective_observer(&self) -> Option<&AgentDiagnosticObserver> {
        self.effective_observer.as_ref()
    }

    pub const fn tool_payload_capture(&self) -> ToolPayloadCapturePolicy {
        self.tool_payload_capture
    }

    pub(crate) fn targets(&self, control: &Arc<AgentTurnControl>) -> bool {
        Weak::ptr_eq(&self.target, &Arc::downgrade(control))
    }

    pub(crate) fn bind(
        &mut self,
        session_observer: Option<&AgentDiagnosticObserver>,
        session_context: Option<&AgentSessionDiagnosticContext>,
    ) -> Result<(), TurnDiagnosticContextAttachError> {
        if session_context.is_some_and(|session| session != self.identity.session()) {
            return Err(TurnDiagnosticContextAttachError::SessionIdentityMismatch);
        }
        self.effective_observer = session_observer
            .cloned()
            .or_else(|| self.standalone_observer.take());
        if self.effective_observer.is_none() {
            return Err(TurnDiagnosticContextAttachError::ObserverUnavailable);
        }
        self.standalone_observer = None;
        Ok(())
    }

    pub(crate) fn bind_runtime_metadata(
        &mut self,
        provider: AgentDiagnosticProvider,
        snapshot: &AgentReadySnapshot,
        operation_id: Uuid,
        turn_index: u64,
    ) {
        let metadata = AgentTurnDiagnosticMetadata {
            identity: self.identity.clone(),
            provider,
            session_generation: snapshot.generation,
            provider_session_id: Arc::from(snapshot.session_id.as_str()),
            effective_model: Arc::from(snapshot.effective_model.as_str()),
            effective_effort: snapshot.effective_effort.as_deref().map(Arc::<str>::from),
            operation_id,
            turn_index,
        };
        if let Some(existing) = &self.runtime_metadata {
            debug_assert_eq!(**existing, metadata);
            return;
        }
        self.runtime_metadata = Some(Arc::new(metadata));
    }
}

impl fmt::Debug for TurnDiagnosticContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnDiagnosticContext")
            .field("identity", &self.identity)
            .field("bound", &self.effective_observer.is_some())
            .field("runtime_bound", &self.runtime_metadata.is_some())
            .field("tool_payload_capture", &self.tool_payload_capture)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnDiagnosticContextAttachError {
    WrongControl,
    NotAdmitted,
    AlreadyAttached,
    TooLate,
    SessionIdentityMismatch,
    ObserverUnavailable,
}

impl fmt::Display for TurnDiagnosticContextAttachError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::WrongControl => "the diagnostic context belongs to a different agent turn",
            Self::NotAdmitted => "the agent turn has not been admitted",
            Self::AlreadyAttached => "an agent turn diagnostic context is already attached",
            Self::TooLate => "the agent turn diagnostic context was attached too late",
            Self::SessionIdentityMismatch => {
                "the agent turn diagnostic context belongs to a different agent session"
            }
            Self::ObserverUnavailable => "the agent turn has no diagnostic observer destination",
        })
    }
}

impl std::error::Error for TurnDiagnosticContextAttachError {}

pub(crate) struct AgentDiagnosticUpdateContext<'a> {
    pub(crate) observer: &'a AgentDiagnosticObserver,
    pub(crate) turn: Option<&'a TurnDiagnosticContext>,
    pub(crate) session_id: &'a SessionId,
    pub(crate) session: Option<Arc<AgentSessionDiagnosticMetadata>>,
}

#[inline]
pub(crate) fn observe_opening(diagnostics: &SessionDiagnostics) {
    let Some(lifecycle) = diagnostics.lifecycle.as_ref() else {
        return;
    };
    lifecycle.observe_opening();
}

#[inline]
pub(crate) fn observe_opening_attempt(diagnostics: &SessionDiagnostics, generation: u64) {
    let Some(lifecycle) = diagnostics.lifecycle.as_ref() else {
        return;
    };
    lifecycle.observe_opening_attempt(generation);
}

#[inline]
pub(crate) fn observe_ready(diagnostics: &SessionDiagnostics, snapshot: &AgentReadySnapshot) {
    let Some(lifecycle) = diagnostics.lifecycle.as_ref() else {
        return;
    };
    lifecycle.observe_ready(snapshot);
}

#[inline]
pub(crate) fn observe_broken(diagnostics: &SessionDiagnostics, code: &'static str) {
    let Some(lifecycle) = diagnostics.lifecycle.as_ref() else {
        return;
    };
    lifecycle.observe_broken(code);
}

#[inline]
pub(crate) fn observe_closing(diagnostics: &SessionDiagnostics) {
    let Some(lifecycle) = diagnostics.lifecycle.as_ref() else {
        return;
    };
    lifecycle.observe_closing();
}

#[inline]
pub(crate) fn observe_closed(diagnostics: &SessionDiagnostics) {
    let Some(lifecycle) = diagnostics.lifecycle.as_ref() else {
        return;
    };
    lifecycle.observe_cleanup_complete();
}

#[inline]
pub(crate) fn observe_update(
    diagnostics: &SessionDiagnostics,
    turn: Option<&TurnDiagnosticContext>,
    session_id: &SessionId,
    update: &SessionUpdate,
) {
    let Some(context) = diagnostics.update_context(turn, session_id) else {
        return;
    };
    let _ = (
        context.observer,
        context.turn,
        context.session_id,
        context.session.as_ref(),
    );
    message::observe_update(&context, update);
    plan::observe_update(&context, update);
    thinking::observe_update(&context, update);
    context::observe_update(&context, update);
    cost::observe_update(&context, update);
    tool::observe_update(&context, update);
    payload::observe_update(&context, update);
}

#[inline]
pub(crate) fn observe_turn_submitted(context: Option<&TurnDiagnosticContext>) {
    let Some(context) = context else {
        return;
    };
    let (Some(observer), Some(metadata)) = (
        context.effective_observer(),
        context.runtime_metadata.as_ref(),
    ) else {
        return;
    };
    observer.observe(AgentDiagnosticObservation::TurnSubmitted(Arc::clone(
        metadata,
    )));
}

#[inline]
pub(crate) fn observe_turn_supervisor_handoff(context: Option<&TurnDiagnosticContext>) {
    let Some(context) = context else {
        return;
    };
    let (Some(observer), Some(metadata)) = (
        context.effective_observer(),
        context.runtime_metadata.as_ref(),
    ) else {
        return;
    };
    observer.observe(AgentDiagnosticObservation::TurnSupervisorHandoff(
        Arc::clone(metadata),
    ));
}

#[inline]
pub(crate) fn observe_turn_terminal(
    context: Option<&TurnDiagnosticContext>,
    observation: &TurnTerminalObservation<'_>,
) {
    let Some(context) = context else {
        return;
    };
    if context.effective_observer().is_none() {
        return;
    }
    if let (Some(observer), Some(metadata)) = (
        context.effective_observer(),
        context.runtime_metadata.as_ref(),
    ) {
        let settlement = match observation.settlement {
            TurnTerminalSettlement::NotSubmitted => AgentTurnDiagnosticSettlement::NotSubmitted,
            TurnTerminalSettlement::Authoritative => AgentTurnDiagnosticSettlement::Authoritative,
            TurnTerminalSettlement::Unknown => AgentTurnDiagnosticSettlement::Unknown,
        };
        observer.observe(AgentDiagnosticObservation::TurnTerminal {
            metadata: Arc::clone(metadata),
            settlement,
            outcome: observation.outcome,
            error_code: observation.error_code,
        });
    }
    super::usage::observe_turn_terminal(context, observation);
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use agent_client_protocol::schema::v1::AgentCapabilities;

    use super::*;
    use crate::diagnostics::observer::{
        AgentDiagnosticCandidate, AgentDiagnosticDestination, AgentDiagnosticFailureOwner,
        AgentDiagnosticObservationKind, AgentDiagnosticObserverFailure,
    };

    #[derive(Debug, Eq, PartialEq)]
    struct TestCandidate(u8);

    impl AgentDiagnosticCandidate for TestCandidate {
        fn kind(&self) -> &'static str {
            "test_candidate"
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[derive(Default)]
    struct RecordingDestination {
        observations: Mutex<Vec<AgentDiagnosticObservation>>,
        fail_on: Mutex<Option<AgentDiagnosticObservationKind>>,
        panic_on: Mutex<Option<AgentDiagnosticObservationKind>>,
    }

    impl RecordingDestination {
        fn observations(&self) -> Vec<AgentDiagnosticObservation> {
            lock(&self.observations).clone()
        }

        fn fail_once(&self, kind: AgentDiagnosticObservationKind) {
            *lock(&self.fail_on) = Some(kind);
        }

        fn panic_once(&self, kind: AgentDiagnosticObservationKind) {
            *lock(&self.panic_on) = Some(kind);
        }
    }

    impl AgentDiagnosticDestination for RecordingDestination {
        fn try_observe(
            &self,
            observation: AgentDiagnosticObservation,
        ) -> Result<(), AgentDiagnosticErrorCode> {
            let kind = observation.kind();
            let should_panic = {
                let mut panic_on = lock(&self.panic_on);
                let matches = *panic_on == Some(kind);
                if matches {
                    *panic_on = None;
                }
                matches
            };
            if should_panic {
                panic!("injected observer panic");
            }
            lock(&self.observations).push(observation);
            let should_fail = {
                let mut fail_on = lock(&self.fail_on);
                let matches = *fail_on == Some(kind);
                if matches {
                    *fail_on = None;
                }
                matches
            };
            if should_fail {
                Err(AgentDiagnosticErrorCode::new("destination_unavailable"))
            } else {
                Ok(())
            }
        }
    }

    #[derive(Default)]
    struct RecordingFailureOwner {
        failures: Mutex<Vec<AgentDiagnosticObserverFailure>>,
        panic: AtomicBool,
    }

    impl RecordingFailureOwner {
        fn failures(&self) -> Vec<AgentDiagnosticObserverFailure> {
            lock(&self.failures).clone()
        }
    }

    impl AgentDiagnosticFailureOwner for RecordingFailureOwner {
        fn observer_failed(&self, failure: AgentDiagnosticObserverFailure) {
            if self.panic.load(Ordering::Acquire) {
                panic!("injected owner panic");
            }
            lock(&self.failures).push(failure);
        }
    }

    fn recording_observer() -> (
        AgentDiagnosticObserver,
        Arc<RecordingDestination>,
        Arc<RecordingFailureOwner>,
    ) {
        let destination = Arc::new(RecordingDestination::default());
        let owner = Arc::new(RecordingFailureOwner::default());
        let observer = AgentDiagnosticObserver::new(Arc::clone(&destination), Arc::clone(&owner));
        (observer, destination, owner)
    }

    fn ready_snapshot(generation: u64) -> AgentReadySnapshot {
        AgentReadySnapshot {
            pid: 7,
            session_id: "provider-session".to_owned(),
            agent_info: None,
            agent_capabilities: AgentCapabilities::default(),
            generation,
            server_name: "result-route".to_owned(),
            endpoint: "http://127.0.0.1:1/mcp".to_owned(),
            effective_model: "effective-model".to_owned(),
            effective_effort: Some("high".to_owned()),
        }
    }

    #[test]
    fn session_lifecycle_is_ordered_exactly_once_and_generation_accurate() {
        let (observer, destination, owner) = recording_observer();
        let diagnostics = SessionDiagnostics::for_test(observer);

        observe_opening(&diagnostics);
        observe_opening(&diagnostics);
        observe_opening_attempt(&diagnostics, 7);
        observe_opening_attempt(&diagnostics, 7);
        observe_opening_attempt(&diagnostics, 6);
        observe_ready(&diagnostics, &ready_snapshot(7));
        observe_ready(&diagnostics, &ready_snapshot(7));
        observe_broken(&diagnostics, "transport_lost");
        observe_broken(&diagnostics, "different_late_failure");
        observe_closed(&diagnostics);
        observe_closing(&diagnostics);
        observe_closing(&diagnostics);

        let observations = destination.observations();
        assert_eq!(
            observations
                .iter()
                .map(AgentDiagnosticObservation::kind)
                .collect::<Vec<_>>(),
            vec![
                AgentDiagnosticObservationKind::SessionOpening,
                AgentDiagnosticObservationKind::SessionOpeningAttempt,
                AgentDiagnosticObservationKind::SessionReady,
                AgentDiagnosticObservationKind::SessionBroken,
                AgentDiagnosticObservationKind::SessionClosing,
                AgentDiagnosticObservationKind::SessionClosed,
            ]
        );
        assert_eq!(
            observations[0].session_metadata().unwrap().generation(),
            None
        );
        assert_eq!(
            observations[1].session_metadata().unwrap().generation(),
            Some(7)
        );
        let ready = observations[2].session_metadata().unwrap();
        assert_eq!(ready.generation(), Some(7));
        assert_eq!(ready.provider_session_id(), Some("provider-session"));
        assert_eq!(ready.effective_model(), Some("effective-model"));
        assert_eq!(ready.effective_effort(), Some("high"));
        assert_eq!(
            observations[3].error_code().unwrap().as_str(),
            "transport_lost"
        );
        assert!(owner.failures().is_empty());
    }

    #[test]
    fn destination_failure_and_panic_only_report_to_owner() {
        let (observer, destination, owner) = recording_observer();
        let diagnostics = SessionDiagnostics::for_test(observer);
        destination.panic_once(AgentDiagnosticObservationKind::SessionOpening);
        destination.fail_once(AgentDiagnosticObservationKind::SessionReady);

        observe_opening(&diagnostics);
        observe_opening_attempt(&diagnostics, 1);
        observe_ready(&diagnostics, &ready_snapshot(1));
        observe_closing(&diagnostics);
        observe_closed(&diagnostics);

        assert_eq!(
            owner
                .failures()
                .into_iter()
                .map(|failure| (failure.observation_kind(), failure.error_code()))
                .collect::<Vec<_>>(),
            vec![
                (
                    AgentDiagnosticObservationKind::SessionOpening,
                    AgentDiagnosticErrorCode::new("observer_panicked"),
                ),
                (
                    AgentDiagnosticObservationKind::SessionReady,
                    AgentDiagnosticErrorCode::new("destination_unavailable"),
                ),
            ]
        );
        assert_eq!(
            destination
                .observations()
                .iter()
                .map(AgentDiagnosticObservation::kind)
                .collect::<Vec<_>>(),
            vec![
                AgentDiagnosticObservationKind::SessionOpeningAttempt,
                AgentDiagnosticObservationKind::SessionReady,
                AgentDiagnosticObservationKind::SessionClosing,
                AgentDiagnosticObservationKind::SessionClosed,
            ]
        );
    }

    #[test]
    fn owner_panic_and_absent_observer_do_not_escape_into_agent_state() {
        let (observer, destination, owner) = recording_observer();
        owner.panic.store(true, Ordering::Release);
        destination.fail_once(AgentDiagnosticObservationKind::SessionOpening);
        let diagnostics = SessionDiagnostics::for_test(observer);
        observe_opening(&diagnostics);
        observe_opening_attempt(&diagnostics, 1);

        let disabled = SessionDiagnostics::new(None);
        observe_opening(&disabled);
        observe_opening_attempt(&disabled, 1);
        observe_broken(&disabled, "transport_lost");
        observe_closing(&disabled);
        observe_closed(&disabled);

        assert_eq!(
            destination
                .observations()
                .iter()
                .map(AgentDiagnosticObservation::kind)
                .collect::<Vec<_>>(),
            vec![
                AgentDiagnosticObservationKind::SessionOpening,
                AgentDiagnosticObservationKind::SessionOpeningAttempt,
            ]
        );
        assert!(disabled.metadata().is_none());
    }

    #[test]
    fn turn_boundaries_share_immutable_metadata_without_raw_response() {
        let (observer, destination, owner) = recording_observer();
        let identity = AgentTurnDiagnosticIdentity::new(
            AgentSessionDiagnosticContext::new("actor", "session"),
            "act",
            "turn",
        );
        let metadata = Arc::new(AgentTurnDiagnosticMetadata {
            identity: identity.clone(),
            provider: AgentDiagnosticProvider::Claude,
            session_generation: 4,
            provider_session_id: Arc::from("provider-session"),
            effective_model: Arc::from("model"),
            effective_effort: Some(Arc::from("medium")),
            operation_id: Uuid::nil(),
            turn_index: 2,
        });
        let context = TurnDiagnosticContext {
            target: Weak::new(),
            identity,
            standalone_observer: None,
            effective_observer: Some(observer),
            tool_payload_capture: ToolPayloadCapturePolicy::new(true, false),
            runtime_metadata: Some(Arc::clone(&metadata)),
        };

        observe_turn_submitted(Some(&context));
        observe_turn_supervisor_handoff(Some(&context));
        observe_turn_terminal(Some(&context), &TurnTerminalObservation::not_submitted());

        let observations = destination.observations();
        assert_eq!(
            observations
                .iter()
                .map(AgentDiagnosticObservation::kind)
                .collect::<Vec<_>>(),
            vec![
                AgentDiagnosticObservationKind::TurnSubmitted,
                AgentDiagnosticObservationKind::TurnSupervisorHandoff,
                AgentDiagnosticObservationKind::TurnTerminal,
                AgentDiagnosticObservationKind::Candidate("agent_turn_usage_terminal"),
            ]
        );
        assert_eq!(observations[0].turn_metadata(), Some(metadata.as_ref()));
        assert_eq!(observations[1].turn_metadata(), Some(metadata.as_ref()));
        assert_eq!(observations[2].turn_metadata(), Some(metadata.as_ref()));
        assert_eq!(
            observations[2].turn_settlement(),
            Some(AgentTurnDiagnosticSettlement::NotSubmitted)
        );
        assert_eq!(
            observations[2].turn_outcome(),
            Some(AgentTurnDiagnosticOutcome::Cancelled)
        );
        assert_eq!(
            observations[2].error_code().map(AgentDiagnosticErrorCode::as_str),
            Some("prompt_not_submitted")
        );
        assert!(owner.failures().is_empty());
    }

    #[test]
    fn production_observer_wins_without_changing_sidecar_capture_policy() {
        let (production, _, _) = recording_observer();
        let (standalone, _, _) = recording_observer();
        let identity = AgentTurnDiagnosticIdentity::new(
            AgentSessionDiagnosticContext::new("actor", "session"),
            "act",
            "turn",
        );
        let mut context = TurnDiagnosticContext {
            target: Weak::new(),
            identity: identity.clone(),
            standalone_observer: Some(standalone),
            effective_observer: None,
            tool_payload_capture: ToolPayloadCapturePolicy::new(true, false),
            runtime_metadata: None,
        };

        context
            .bind(Some(&production), Some(identity.session()))
            .unwrap();

        assert!(
            context
                .effective_observer()
                .unwrap()
                .same_destination(&production)
        );
        assert!(context.tool_payload_capture().capture_input());
        assert!(!context.tool_payload_capture().capture_output());
    }

    #[test]
    fn later_normalizers_can_submit_typed_candidates_without_changing_observer() {
        let (observer, destination, owner) = recording_observer();

        observer.observe(AgentDiagnosticObservation::Candidate(Arc::new(
            TestCandidate(9),
        )));

        let observations = destination.observations();
        assert_eq!(
            observations[0].kind(),
            AgentDiagnosticObservationKind::Candidate("test_candidate")
        );
        assert_eq!(
            observations[0]
                .candidate()
                .unwrap()
                .as_any()
                .downcast_ref::<TestCandidate>(),
            Some(&TestCandidate(9))
        );
        assert!(owner.failures().is_empty());
    }
}
