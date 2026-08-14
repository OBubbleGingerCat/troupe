use std::fmt;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use agent_client_protocol::schema::v1::{SessionId, SessionUpdate};
use uuid::Uuid;

use super::observer::AgentDiagnosticObserver;
use super::payload::ToolPayloadCapturePolicy;
use super::usage::TurnTerminalObservation;
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

    #[cfg(test)]
    fn for_test(context: AgentSessionDiagnosticContext) -> Self {
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
    closing: bool,
    cleanup_complete: bool,
    closed: bool,
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

    fn observe_opening_attempt(&self, generation: u64) {
        let mut metadata = lock(&self.metadata);
        let mut next = (**metadata).clone();
        next.generation = Some(generation);
        next.provider_session_id = None;
        next.effective_model = None;
        next.effective_effort = None;
        *metadata = Arc::new(next);
    }

    fn observe_ready(&self, snapshot: &AgentReadySnapshot) {
        let mut metadata = lock(&self.metadata);
        let mut next = (**metadata).clone();
        next.generation = Some(snapshot.generation);
        next.provider_session_id = Some(Arc::from(snapshot.session_id.as_str()));
        next.effective_model = Some(Arc::from(snapshot.effective_model.as_str()));
        next.effective_effort = snapshot.effective_effort.as_deref().map(Arc::<str>::from);
        *metadata = Arc::new(next);
    }

    fn observe_closing(&self) {
        let mut state = lock(&self.state);
        if !state.closing {
            state.closing = true;
            let metadata = self.metadata();
            let _ = (&self.observer, metadata);
            #[cfg(test)]
            {
                self.closing_observations.fetch_add(1, Ordering::AcqRel);
                lock(&self.observations).push("closing");
            }
        }
        self.observe_closed_if_complete(&mut state);
    }

    fn observe_cleanup_complete(&self) {
        let mut state = lock(&self.state);
        state.cleanup_complete = true;
        self.observe_closed_if_complete(&mut state);
    }

    fn observe_closed_if_complete(&self, state: &mut SessionDiagnosticLifecycleState) {
        if !state.closing || !state.cleanup_complete || state.closed {
            return;
        }
        state.closed = true;
        let metadata = self.metadata();
        let _ = (&self.observer, metadata);
        #[cfg(test)]
        {
            self.closed_observations.fetch_add(1, Ordering::AcqRel);
            lock(&self.observations).push("closed");
        }
    }
}

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
    let (Some(_observer), Some(_metadata)) = (diagnostics.observer(), diagnostics.metadata())
    else {
        return;
    };
}

#[inline]
pub(crate) fn observe_opening_attempt(diagnostics: &SessionDiagnostics, generation: u64) {
    let Some(lifecycle) = diagnostics.lifecycle.as_ref() else {
        return;
    };
    lifecycle.observe_opening_attempt(generation);
    let _ = (&lifecycle.observer, lifecycle.metadata());
}

#[inline]
pub(crate) fn observe_ready(diagnostics: &SessionDiagnostics, snapshot: &AgentReadySnapshot) {
    let Some(lifecycle) = diagnostics.lifecycle.as_ref() else {
        return;
    };
    lifecycle.observe_ready(snapshot);
    let _ = (&lifecycle.observer, lifecycle.metadata());
}

#[inline]
pub(crate) fn observe_broken(diagnostics: &SessionDiagnostics, _code: &'static str) {
    let (Some(_observer), Some(_metadata)) = (diagnostics.observer(), diagnostics.metadata())
    else {
        return;
    };
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
    let Some(_observer) = context.and_then(TurnDiagnosticContext::effective_observer) else {
        return;
    };
    let _metadata = context.and_then(TurnDiagnosticContext::runtime_metadata);
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
    super::usage::observe_turn_terminal(context, observation);
}
