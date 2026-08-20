use std::{any::Any, fmt, sync::Arc};

use agent_client_protocol::schema::v1::{SessionUpdate, UsageUpdate};

use super::{
    observer::{AgentDiagnosticCandidate, AgentDiagnosticObservation},
    session::{
        AgentDiagnosticUpdateContext, AgentSessionDiagnosticMetadata, AgentTurnDiagnosticMetadata,
    },
};

pub const AGENT_CONTEXT_OCCUPANCY_CANDIDATE_KIND: &str = "context_occupancy";
pub const AGENT_CONTEXT_USAGE_SAMPLE_CANDIDATE_KIND: &str = "context_usage_sampled";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentContextOccupancyError {
    UsedExceedsWindow,
}

impl AgentContextOccupancyError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::UsedExceedsWindow => "used_exceeds_context_window",
        }
    }
}

impl fmt::Display for AgentContextOccupancyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for AgentContextOccupancyError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentContextOccupancy {
    context_used_tokens: Option<u64>,
    context_window_tokens: Option<u64>,
}

impl AgentContextOccupancy {
    pub fn new(
        context_used_tokens: Option<u64>,
        context_window_tokens: Option<u64>,
    ) -> Result<Self, AgentContextOccupancyError> {
        if matches!(
            (context_used_tokens, context_window_tokens),
            (Some(used), Some(window)) if used > window
        ) {
            return Err(AgentContextOccupancyError::UsedExceedsWindow);
        }
        Ok(Self {
            context_used_tokens,
            context_window_tokens,
        })
    }

    pub fn from_acp(update: &UsageUpdate) -> Result<Self, AgentContextOccupancyError> {
        Self::new(Some(update.used), Some(update.size))
    }

    pub const fn context_used_tokens(self) -> Option<u64> {
        self.context_used_tokens
    }

    pub const fn context_window_tokens(self) -> Option<u64> {
        self.context_window_tokens
    }

    pub const fn observed_at(self, observed_elapsed_ns: u64) -> ObservedAgentContextOccupancy {
        ObservedAgentContextOccupancy {
            occupancy: self,
            observed_elapsed_ns,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObservedAgentContextOccupancy {
    occupancy: AgentContextOccupancy,
    observed_elapsed_ns: u64,
}

impl ObservedAgentContextOccupancy {
    pub const fn occupancy(self) -> AgentContextOccupancy {
        self.occupancy
    }

    pub const fn context_used_tokens(self) -> Option<u64> {
        self.occupancy.context_used_tokens()
    }

    pub const fn context_window_tokens(self) -> Option<u64> {
        self.occupancy.context_window_tokens()
    }

    pub const fn observed_elapsed_ns(self) -> u64 {
        self.observed_elapsed_ns
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentContextOccupancyMetadata {
    Turn(AgentTurnDiagnosticMetadata),
    Session(AgentSessionDiagnosticMetadata),
}

impl AgentContextOccupancyMetadata {
    pub const fn turn(&self) -> Option<&AgentTurnDiagnosticMetadata> {
        match self {
            Self::Turn(metadata) => Some(metadata),
            Self::Session(_) => None,
        }
    }

    pub const fn session(&self) -> Option<&AgentSessionDiagnosticMetadata> {
        match self {
            Self::Session(metadata) => Some(metadata),
            Self::Turn(_) => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentContextOccupancyCandidate {
    metadata: AgentContextOccupancyMetadata,
    occupancy: AgentContextOccupancy,
}

impl AgentContextOccupancyCandidate {
    fn new(metadata: AgentContextOccupancyMetadata, occupancy: AgentContextOccupancy) -> Self {
        Self {
            metadata,
            occupancy,
        }
    }

    pub const fn metadata(&self) -> &AgentContextOccupancyMetadata {
        &self.metadata
    }

    pub const fn occupancy(&self) -> AgentContextOccupancy {
        self.occupancy
    }

    pub fn observed_at(&self, observed_elapsed_ns: u64) -> AgentContextUsageSampleCandidate {
        AgentContextUsageSampleCandidate {
            metadata: self.metadata.clone(),
            observation: self.occupancy.observed_at(observed_elapsed_ns),
        }
    }
}

impl AgentDiagnosticCandidate for AgentContextOccupancyCandidate {
    fn kind(&self) -> &'static str {
        AGENT_CONTEXT_OCCUPANCY_CANDIDATE_KIND
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentContextUsageSampleCandidate {
    metadata: AgentContextOccupancyMetadata,
    observation: ObservedAgentContextOccupancy,
}

impl AgentContextUsageSampleCandidate {
    pub const fn metadata(&self) -> &AgentContextOccupancyMetadata {
        &self.metadata
    }

    pub const fn observation(&self) -> ObservedAgentContextOccupancy {
        self.observation
    }

    pub const fn context_used_tokens(&self) -> Option<u64> {
        self.observation.context_used_tokens()
    }

    pub const fn context_window_tokens(&self) -> Option<u64> {
        self.observation.context_window_tokens()
    }

    pub const fn observed_elapsed_ns(&self) -> u64 {
        self.observation.observed_elapsed_ns()
    }
}

impl AgentDiagnosticCandidate for AgentContextUsageSampleCandidate {
    fn kind(&self) -> &'static str {
        AGENT_CONTEXT_USAGE_SAMPLE_CANDIDATE_KIND
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[inline]
pub(crate) fn observe_update(context: &AgentDiagnosticUpdateContext<'_>, update: &SessionUpdate) {
    let SessionUpdate::UsageUpdate(update) = update else {
        return;
    };
    let (Some(metadata), Ok(occupancy)) = (
        observation_metadata(context),
        AgentContextOccupancy::from_acp(update),
    ) else {
        return;
    };
    context
        .observer
        .observe(AgentDiagnosticObservation::Candidate(Arc::new(
            AgentContextOccupancyCandidate::new(metadata, occupancy),
        )));
}

fn observation_metadata(
    context: &AgentDiagnosticUpdateContext<'_>,
) -> Option<AgentContextOccupancyMetadata> {
    if let Some(metadata) = context
        .turn
        .and_then(|turn| turn.runtime_metadata())
        .cloned()
    {
        return Some(AgentContextOccupancyMetadata::Turn(metadata));
    }
    context
        .session
        .as_deref()
        .cloned()
        .map(AgentContextOccupancyMetadata::Session)
}
