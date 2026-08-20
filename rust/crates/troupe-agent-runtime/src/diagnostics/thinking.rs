use std::{any::Any, fmt, sync::Arc};

use agent_client_protocol::schema::v1::SessionUpdate;

use super::{
    observer::{AgentDiagnosticCandidate, AgentDiagnosticObservation},
    session::{AgentDiagnosticUpdateContext, AgentTurnDiagnosticMetadata},
};

pub const AGENT_THINKING_ACTIVITY_OBSERVATION_KIND: &str = "agent_thinking_activity_observed";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentThinkingActivityObservation {
    turn: Arc<AgentTurnDiagnosticMetadata>,
}

impl AgentThinkingActivityObservation {
    pub fn turn(&self) -> &AgentTurnDiagnosticMetadata {
        &self.turn
    }
}

impl AgentDiagnosticCandidate for AgentThinkingActivityObservation {
    fn kind(&self) -> &'static str {
        AGENT_THINKING_ACTIVITY_OBSERVATION_KIND
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AgentThinkingActivityPhase {
    Start,
    Progress,
    Finish,
}

impl AgentThinkingActivityPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Progress => "progress",
            Self::Finish => "finish",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentThinkingActivityCandidate {
    elapsed_ns: u64,
    phase: AgentThinkingActivityPhase,
}

impl AgentThinkingActivityCandidate {
    pub const fn elapsed_ns(self) -> u64 {
        self.elapsed_ns
    }

    pub const fn phase(self) -> AgentThinkingActivityPhase {
        self.phase
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AgentThinkingSourceGapReason {
    ActivityAfterTurnTerminal,
    RepeatedTurnTerminal,
}

impl AgentThinkingSourceGapReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ActivityAfterTurnTerminal => "thinking_activity_after_turn_terminal",
            Self::RepeatedTurnTerminal => "thinking_turn_terminal_repeated",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentThinkingSourceGapCandidate {
    elapsed_ns: u64,
    reason: AgentThinkingSourceGapReason,
}

impl AgentThinkingSourceGapCandidate {
    pub const fn elapsed_ns(self) -> u64 {
        self.elapsed_ns
    }

    pub const fn reason(self) -> AgentThinkingSourceGapReason {
        self.reason
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentThinkingCandidate {
    Activity(AgentThinkingActivityCandidate),
    SourceGap(AgentThinkingSourceGapCandidate),
}

impl AgentThinkingCandidate {
    pub const fn activity(self) -> Option<AgentThinkingActivityCandidate> {
        match self {
            Self::Activity(candidate) => Some(candidate),
            Self::SourceGap(_) => None,
        }
    }

    pub const fn source_gap(self) -> Option<AgentThinkingSourceGapCandidate> {
        match self {
            Self::SourceGap(candidate) => Some(candidate),
            Self::Activity(_) => None,
        }
    }
}

impl AgentDiagnosticCandidate for AgentThinkingCandidate {
    fn kind(&self) -> &'static str {
        match self {
            Self::Activity(candidate) => match candidate.phase {
                AgentThinkingActivityPhase::Start => "agent_thinking_start",
                AgentThinkingActivityPhase::Progress => "agent_thinking_progress",
                AgentThinkingActivityPhase::Finish => "agent_thinking_finish",
            },
            Self::SourceGap(_) => "observation_gap",
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Default)]
pub struct AgentThinkingNormalizer {
    active: bool,
    terminal: bool,
}

impl AgentThinkingNormalizer {
    pub const fn new() -> Self {
        Self {
            active: false,
            terminal: false,
        }
    }

    pub fn observe_session_update(
        &mut self,
        update: &SessionUpdate,
        elapsed_ns: u64,
    ) -> Vec<AgentThinkingCandidate> {
        if matches!(update, SessionUpdate::AgentThoughtChunk(_)) {
            self.observe_activity(elapsed_ns)
        } else {
            Vec::new()
        }
    }

    pub fn observe_observation(
        &mut self,
        _observation: &AgentThinkingActivityObservation,
        elapsed_ns: u64,
    ) -> Vec<AgentThinkingCandidate> {
        self.observe_activity(elapsed_ns)
    }

    pub fn observe_activity(&mut self, elapsed_ns: u64) -> Vec<AgentThinkingCandidate> {
        if self.terminal {
            return vec![source_gap(
                elapsed_ns,
                AgentThinkingSourceGapReason::ActivityAfterTurnTerminal,
            )];
        }

        let phase = if self.active {
            AgentThinkingActivityPhase::Progress
        } else {
            self.active = true;
            AgentThinkingActivityPhase::Start
        };
        vec![activity(elapsed_ns, phase)]
    }

    pub fn turn_terminal(&mut self, elapsed_ns: u64) -> Vec<AgentThinkingCandidate> {
        if self.terminal {
            return vec![source_gap(
                elapsed_ns,
                AgentThinkingSourceGapReason::RepeatedTurnTerminal,
            )];
        }

        self.terminal = true;
        if !self.active {
            return Vec::new();
        }
        self.active = false;
        vec![activity(elapsed_ns, AgentThinkingActivityPhase::Finish)]
    }

    pub const fn is_active(&self) -> bool {
        self.active
    }

    pub const fn is_terminal(&self) -> bool {
        self.terminal
    }
}

impl fmt::Debug for AgentThinkingNormalizer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentThinkingNormalizer")
            .field("active", &self.active)
            .field("terminal", &self.terminal)
            .finish()
    }
}

const fn activity(elapsed_ns: u64, phase: AgentThinkingActivityPhase) -> AgentThinkingCandidate {
    AgentThinkingCandidate::Activity(AgentThinkingActivityCandidate { elapsed_ns, phase })
}

const fn source_gap(
    elapsed_ns: u64,
    reason: AgentThinkingSourceGapReason,
) -> AgentThinkingCandidate {
    AgentThinkingCandidate::SourceGap(AgentThinkingSourceGapCandidate { elapsed_ns, reason })
}

#[inline]
pub(crate) fn observe_update(context: &AgentDiagnosticUpdateContext<'_>, update: &SessionUpdate) {
    if !matches!(update, SessionUpdate::AgentThoughtChunk(_)) {
        return;
    }
    let Some(turn) = context
        .turn
        .and_then(|turn| turn.runtime_metadata())
        .cloned()
    else {
        return;
    };

    context
        .observer
        .observe(AgentDiagnosticObservation::Candidate(Arc::new(
            AgentThinkingActivityObservation {
                turn: Arc::new(turn),
            },
        )));
}
