use std::any::Any;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, MutexGuard};

use super::session::{AgentSessionDiagnosticMetadata, AgentTurnDiagnosticMetadata};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AgentDiagnosticObservationKind {
    SessionOpening,
    SessionOpeningAttempt,
    SessionReady,
    SessionBroken,
    SessionClosing,
    SessionClosed,
    TurnSubmitted,
    TurnSupervisorHandoff,
    TurnTerminal,
    Candidate(&'static str),
}

impl AgentDiagnosticObservationKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionOpening => "session_opening",
            Self::SessionOpeningAttempt => "session_opening_attempt",
            Self::SessionReady => "session_ready",
            Self::SessionBroken => "session_broken",
            Self::SessionClosing => "session_closing",
            Self::SessionClosed => "session_closed",
            Self::TurnSubmitted => "turn_submitted",
            Self::TurnSupervisorHandoff => "turn_supervisor_handoff",
            Self::TurnTerminal => "turn_terminal",
            Self::Candidate(kind) => kind,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AgentDiagnosticErrorCode(&'static str);

impl AgentDiagnosticErrorCode {
    pub const fn new(code: &'static str) -> Self {
        assert!(!code.is_empty(), "agent diagnostic error code is empty");
        Self(code)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for AgentDiagnosticErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for AgentDiagnosticErrorCode {}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AgentTurnDiagnosticSettlement {
    NotSubmitted,
    Authoritative,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AgentTurnDiagnosticOutcome {
    Completed,
    Cancelled,
    Failed,
}

impl AgentTurnDiagnosticOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

impl AgentTurnDiagnosticSettlement {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotSubmitted => "not_submitted",
            Self::Authoritative => "authoritative",
            Self::Unknown => "unknown",
        }
    }
}

pub trait AgentDiagnosticCandidate: fmt::Debug + Send + Sync + 'static {
    fn kind(&self) -> &'static str;

    fn as_any(&self) -> &dyn Any;
}

#[derive(Clone, Debug)]
pub enum AgentDiagnosticObservation {
    SessionOpening(Arc<AgentSessionDiagnosticMetadata>),
    SessionOpeningAttempt(Arc<AgentSessionDiagnosticMetadata>),
    SessionReady(Arc<AgentSessionDiagnosticMetadata>),
    SessionBroken {
        metadata: Arc<AgentSessionDiagnosticMetadata>,
        error_code: AgentDiagnosticErrorCode,
    },
    SessionClosing(Arc<AgentSessionDiagnosticMetadata>),
    SessionClosed(Arc<AgentSessionDiagnosticMetadata>),
    TurnSubmitted(Arc<AgentTurnDiagnosticMetadata>),
    TurnSupervisorHandoff(Arc<AgentTurnDiagnosticMetadata>),
    TurnTerminal {
        metadata: Arc<AgentTurnDiagnosticMetadata>,
        settlement: AgentTurnDiagnosticSettlement,
        outcome: AgentTurnDiagnosticOutcome,
        error_code: Option<AgentDiagnosticErrorCode>,
    },
    Candidate(Arc<dyn AgentDiagnosticCandidate>),
}

impl AgentDiagnosticObservation {
    pub fn kind(&self) -> AgentDiagnosticObservationKind {
        match self {
            Self::SessionOpening(_) => AgentDiagnosticObservationKind::SessionOpening,
            Self::SessionOpeningAttempt(_) => AgentDiagnosticObservationKind::SessionOpeningAttempt,
            Self::SessionReady(_) => AgentDiagnosticObservationKind::SessionReady,
            Self::SessionBroken { .. } => AgentDiagnosticObservationKind::SessionBroken,
            Self::SessionClosing(_) => AgentDiagnosticObservationKind::SessionClosing,
            Self::SessionClosed(_) => AgentDiagnosticObservationKind::SessionClosed,
            Self::TurnSubmitted(_) => AgentDiagnosticObservationKind::TurnSubmitted,
            Self::TurnSupervisorHandoff(_) => {
                AgentDiagnosticObservationKind::TurnSupervisorHandoff
            }
            Self::TurnTerminal { .. } => AgentDiagnosticObservationKind::TurnTerminal,
            Self::Candidate(candidate) => {
                AgentDiagnosticObservationKind::Candidate(candidate.kind())
            }
        }
    }

    pub fn session_metadata(&self) -> Option<&AgentSessionDiagnosticMetadata> {
        match self {
            Self::SessionOpening(metadata)
            | Self::SessionOpeningAttempt(metadata)
            | Self::SessionReady(metadata)
            | Self::SessionClosing(metadata)
            | Self::SessionClosed(metadata)
            | Self::SessionBroken { metadata, .. } => Some(metadata),
            Self::TurnSubmitted(_)
            | Self::TurnSupervisorHandoff(_)
            | Self::TurnTerminal { .. }
            | Self::Candidate(_) => None,
        }
    }

    pub fn turn_metadata(&self) -> Option<&AgentTurnDiagnosticMetadata> {
        match self {
            Self::TurnSubmitted(metadata)
            | Self::TurnSupervisorHandoff(metadata)
            | Self::TurnTerminal { metadata, .. } => Some(metadata),
            Self::SessionOpening(_)
            | Self::SessionOpeningAttempt(_)
            | Self::SessionReady(_)
            | Self::SessionBroken { .. }
            | Self::SessionClosing(_)
            | Self::SessionClosed(_)
            | Self::Candidate(_) => None,
        }
    }

    pub const fn error_code(&self) -> Option<AgentDiagnosticErrorCode> {
        match self {
            Self::SessionBroken { error_code, .. }
            | Self::TurnTerminal {
                error_code: Some(error_code),
                ..
            } => Some(*error_code),
            _ => None,
        }
    }

    pub const fn turn_outcome(&self) -> Option<AgentTurnDiagnosticOutcome> {
        match self {
            Self::TurnTerminal { outcome, .. } => Some(*outcome),
            _ => None,
        }
    }

    pub const fn turn_settlement(&self) -> Option<AgentTurnDiagnosticSettlement> {
        match self {
            Self::TurnTerminal { settlement, .. } => Some(*settlement),
            _ => None,
        }
    }

    pub fn candidate(&self) -> Option<&dyn AgentDiagnosticCandidate> {
        match self {
            Self::Candidate(candidate) => Some(candidate.as_ref()),
            _ => None,
        }
    }
}

pub trait AgentDiagnosticDestination: Send + Sync + 'static {
    fn try_observe(
        &self,
        observation: AgentDiagnosticObservation,
    ) -> Result<(), AgentDiagnosticErrorCode>;
}

pub trait AgentDiagnosticFailureOwner: Send + Sync + 'static {
    fn observer_failed(&self, failure: AgentDiagnosticObserverFailure);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentDiagnosticObserverFailure {
    observation_kind: AgentDiagnosticObservationKind,
    error_code: AgentDiagnosticErrorCode,
}

impl AgentDiagnosticObserverFailure {
    const fn new(
        observation_kind: AgentDiagnosticObservationKind,
        error_code: AgentDiagnosticErrorCode,
    ) -> Self {
        Self {
            observation_kind,
            error_code,
        }
    }

    pub const fn observation_kind(self) -> AgentDiagnosticObservationKind {
        self.observation_kind
    }

    pub const fn error_code(self) -> AgentDiagnosticErrorCode {
        self.error_code
    }
}

struct AgentDiagnosticObserverInner {
    destination_identity: Arc<dyn Any + Send + Sync>,
    destination: Box<
        dyn Fn(AgentDiagnosticObservation) -> Result<(), AgentDiagnosticErrorCode> + Send + Sync,
    >,
    failure_owner: Box<dyn Fn(AgentDiagnosticObserverFailure) + Send + Sync>,
    delivery_order: Mutex<()>,
}

#[derive(Clone)]
pub struct AgentDiagnosticObserver {
    inner: Arc<AgentDiagnosticObserverInner>,
}

impl AgentDiagnosticObserver {
    pub fn new<D, O>(destination: Arc<D>, failure_owner: Arc<O>) -> Self
    where
        D: AgentDiagnosticDestination,
        O: AgentDiagnosticFailureOwner,
    {
        let destination_identity: Arc<dyn Any + Send + Sync> = destination.clone();
        Self {
            inner: Arc::new(AgentDiagnosticObserverInner {
                destination_identity,
                destination: Box::new(move |observation| destination.try_observe(observation)),
                failure_owner: Box::new(move |failure| {
                    failure_owner.observer_failed(failure);
                }),
                delivery_order: Mutex::new(()),
            }),
        }
    }

    pub fn from_destination<T>(destination: Arc<T>) -> Self
    where
        T: Any + Send + Sync,
    {
        let destination_identity: Arc<dyn Any + Send + Sync> = destination;
        Self {
            inner: Arc::new(AgentDiagnosticObserverInner {
                destination_identity,
                destination: Box::new(|_| Ok(())),
                failure_owner: Box::new(|_| {}),
                delivery_order: Mutex::new(()),
            }),
        }
    }

    pub fn same_destination(&self, other: &Self) -> bool {
        Arc::ptr_eq(
            &self.inner.destination_identity,
            &other.inner.destination_identity,
        )
    }

    pub(crate) fn observe(&self, observation: AgentDiagnosticObservation) {
        let observation_kind = observation.kind();
        let outcome = {
            let _ordered = lock(&self.inner.delivery_order);
            catch_unwind(AssertUnwindSafe(|| (self.inner.destination)(observation)))
        };
        let error_code = match outcome {
            Ok(Ok(())) => return,
            Ok(Err(error_code)) => error_code,
            Err(_) => AgentDiagnosticErrorCode::new("observer_panicked"),
        };
        let failure = AgentDiagnosticObserverFailure::new(observation_kind, error_code);
        let _ = catch_unwind(AssertUnwindSafe(|| (self.inner.failure_owner)(failure)));
    }
}

impl fmt::Debug for AgentDiagnosticObserver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentDiagnosticObserver")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentDiagnosticObserverInstallError {
    AlreadyInstalled,
    SessionOpeningStarted,
}

impl fmt::Display for AgentDiagnosticObserverInstallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::AlreadyInstalled => "an agent diagnostic observer is already installed",
            Self::SessionOpeningStarted => {
                "the agent diagnostic observer must be installed before session opening"
            }
        })
    }
}

impl std::error::Error for AgentDiagnosticObserverInstallError {}
