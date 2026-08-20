use std::{
    any::Any,
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
};

use agent_client_protocol::schema::v1::{
    SessionUpdate, ToolCall, ToolCallStatus as AcpToolCallStatus, ToolCallUpdate,
    ToolKind as AcpToolKind,
};
use troupe_diagnostics_core::kinds::{ToolCallStatus, ToolKind};

use super::{
    observer::{AgentDiagnosticCandidate, AgentDiagnosticObservation},
    session::{AgentDiagnosticUpdateContext, AgentTurnDiagnosticMetadata},
};

pub const AGENT_TOOL_OBSERVATION_KIND: &str = "agent_tool_observed";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentToolStartedObservation {
    turn: Arc<AgentTurnDiagnosticMetadata>,
    source_tool_call_id: Arc<str>,
    title: Arc<str>,
    tool_kind: ToolKind,
    status: ToolCallStatus,
}

impl AgentToolStartedObservation {
    pub fn turn(&self) -> &AgentTurnDiagnosticMetadata {
        &self.turn
    }

    pub fn source_tool_call_id(&self) -> &str {
        &self.source_tool_call_id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub const fn tool_kind(&self) -> ToolKind {
        self.tool_kind
    }

    pub const fn status(&self) -> ToolCallStatus {
        self.status
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentToolUpdatedObservation {
    turn: Arc<AgentTurnDiagnosticMetadata>,
    source_tool_call_id: Arc<str>,
    title: Option<Arc<str>>,
    tool_kind: Option<ToolKind>,
    status: Option<ToolCallStatus>,
}

impl AgentToolUpdatedObservation {
    pub fn turn(&self) -> &AgentTurnDiagnosticMetadata {
        &self.turn
    }

    pub fn source_tool_call_id(&self) -> &str {
        &self.source_tool_call_id
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub const fn tool_kind(&self) -> Option<ToolKind> {
        self.tool_kind
    }

    pub const fn status(&self) -> Option<ToolCallStatus> {
        self.status
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentToolObservation {
    Started(AgentToolStartedObservation),
    Updated(AgentToolUpdatedObservation),
}

impl AgentToolObservation {
    pub fn turn(&self) -> &AgentTurnDiagnosticMetadata {
        match self {
            Self::Started(observation) => observation.turn(),
            Self::Updated(observation) => observation.turn(),
        }
    }

    pub fn source_tool_call_id(&self) -> &str {
        match self {
            Self::Started(observation) => observation.source_tool_call_id(),
            Self::Updated(observation) => observation.source_tool_call_id(),
        }
    }
}

impl AgentDiagnosticCandidate for AgentToolObservation {
    fn kind(&self) -> &'static str {
        AGENT_TOOL_OBSERVATION_KIND
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentToolMetadata {
    source_tool_call_id: Arc<str>,
    title: Arc<str>,
    tool_kind: ToolKind,
    status: ToolCallStatus,
}

impl AgentToolMetadata {
    pub fn source_tool_call_id(&self) -> &str {
        &self.source_tool_call_id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub const fn tool_kind(&self) -> ToolKind {
        self.tool_kind
    }

    pub const fn status(&self) -> ToolCallStatus {
        self.status
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AgentToolTerminalOutcome {
    Completed,
    Failed,
    Cancelled,
}

impl AgentToolTerminalOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AgentToolErrorCode {
    ToolFailed,
}

impl AgentToolErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ToolFailed => "tool_failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentToolActivityCandidate {
    elapsed_ns: u64,
    metadata: AgentToolMetadata,
}

impl AgentToolActivityCandidate {
    pub const fn elapsed_ns(&self) -> u64 {
        self.elapsed_ns
    }

    pub const fn metadata(&self) -> &AgentToolMetadata {
        &self.metadata
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentToolFinishedCandidate {
    elapsed_ns: u64,
    source_tool_call_id: Arc<str>,
    outcome: AgentToolTerminalOutcome,
    error_code: Option<AgentToolErrorCode>,
}

impl AgentToolFinishedCandidate {
    pub const fn elapsed_ns(&self) -> u64 {
        self.elapsed_ns
    }

    pub fn source_tool_call_id(&self) -> &str {
        &self.source_tool_call_id
    }

    pub const fn outcome(&self) -> AgentToolTerminalOutcome {
        self.outcome
    }

    pub const fn error_code(&self) -> Option<AgentToolErrorCode> {
        self.error_code
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AgentToolSourceGapReason {
    EmptyToolCallId,
    DuplicateActiveToolCall,
    FinishedToolCallIdReused,
    UpdateBeforeStart,
    UpdateAfterFinish,
    InvalidStatusTransition,
    ActivityAfterTurnTerminal,
    RepeatedTurnTerminal,
}

impl AgentToolSourceGapReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EmptyToolCallId => "tool_call_id_empty",
            Self::DuplicateActiveToolCall => "tool_call_duplicate_active",
            Self::FinishedToolCallIdReused => "tool_call_id_reused",
            Self::UpdateBeforeStart => "tool_update_before_start",
            Self::UpdateAfterFinish => "tool_update_after_finish",
            Self::InvalidStatusTransition => "tool_status_transition_invalid",
            Self::ActivityAfterTurnTerminal => "tool_activity_after_turn_terminal",
            Self::RepeatedTurnTerminal => "tool_turn_terminal_repeated",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentToolSourceGapCandidate {
    elapsed_ns: u64,
    reason: AgentToolSourceGapReason,
    source_tool_call_id: Option<Arc<str>>,
}

impl AgentToolSourceGapCandidate {
    pub const fn elapsed_ns(&self) -> u64 {
        self.elapsed_ns
    }

    pub const fn reason(&self) -> AgentToolSourceGapReason {
        self.reason
    }

    pub fn source_tool_call_id(&self) -> Option<&str> {
        self.source_tool_call_id.as_deref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentToolCandidate {
    Started(AgentToolActivityCandidate),
    Updated(AgentToolActivityCandidate),
    Finished(AgentToolFinishedCandidate),
    SourceGap(AgentToolSourceGapCandidate),
}

impl AgentToolCandidate {
    pub const fn started(&self) -> Option<&AgentToolActivityCandidate> {
        match self {
            Self::Started(candidate) => Some(candidate),
            _ => None,
        }
    }

    pub const fn updated(&self) -> Option<&AgentToolActivityCandidate> {
        match self {
            Self::Updated(candidate) => Some(candidate),
            _ => None,
        }
    }

    pub const fn finished(&self) -> Option<&AgentToolFinishedCandidate> {
        match self {
            Self::Finished(candidate) => Some(candidate),
            _ => None,
        }
    }

    pub const fn source_gap(&self) -> Option<&AgentToolSourceGapCandidate> {
        match self {
            Self::SourceGap(candidate) => Some(candidate),
            _ => None,
        }
    }
}

impl AgentDiagnosticCandidate for AgentToolCandidate {
    fn kind(&self) -> &'static str {
        match self {
            Self::Started(_) => "agent_tool_started",
            Self::Updated(_) => "agent_tool_updated",
            Self::Finished(_) => "agent_tool_finished",
            Self::SourceGap(_) => "observation_gap",
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Default)]
pub struct AgentToolNormalizer {
    open: HashMap<Arc<str>, AgentToolMetadata>,
    open_order: Vec<Arc<str>>,
    finished: HashSet<Arc<str>>,
    terminal: bool,
}

impl AgentToolNormalizer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(
        &mut self,
        observation: &AgentToolObservation,
        elapsed_ns: u64,
    ) -> Vec<AgentToolCandidate> {
        match observation {
            AgentToolObservation::Started(observation) => {
                self.observe_started(observation, elapsed_ns)
            }
            AgentToolObservation::Updated(observation) => {
                self.observe_updated(observation, elapsed_ns)
            }
        }
    }

    pub fn observe_session_update(
        &mut self,
        update: &SessionUpdate,
        elapsed_ns: u64,
    ) -> Vec<AgentToolCandidate> {
        match update {
            SessionUpdate::ToolCall(call) => {
                let Some(status) = normalize_status(call.status) else {
                    return Vec::new();
                };
                self.start(
                    &call.tool_call_id.0,
                    &call.title,
                    normalize_tool_kind(call.kind),
                    status,
                    elapsed_ns,
                )
            }
            SessionUpdate::ToolCallUpdate(update) => {
                let status = match update.fields.status {
                    Some(status) => {
                        let Some(status) = normalize_status(status) else {
                            return Vec::new();
                        };
                        Some(status)
                    }
                    None => None,
                };
                self.update(
                    &update.tool_call_id.0,
                    update.fields.title.as_deref(),
                    update.fields.kind.map(normalize_tool_kind),
                    status,
                    elapsed_ns,
                )
            }
            _ => Vec::new(),
        }
    }

    pub fn observe_start(
        &mut self,
        source_tool_call_id: &str,
        title: &str,
        tool_kind: ToolKind,
        status: ToolCallStatus,
        elapsed_ns: u64,
    ) -> Vec<AgentToolCandidate> {
        self.start(source_tool_call_id, title, tool_kind, status, elapsed_ns)
    }

    pub fn observe_update_fields(
        &mut self,
        source_tool_call_id: &str,
        title: Option<&str>,
        tool_kind: Option<ToolKind>,
        status: Option<ToolCallStatus>,
        elapsed_ns: u64,
    ) -> Vec<AgentToolCandidate> {
        self.update(source_tool_call_id, title, tool_kind, status, elapsed_ns)
    }

    pub fn turn_terminal(&mut self, elapsed_ns: u64) -> Vec<AgentToolCandidate> {
        if self.terminal {
            return vec![source_gap(
                elapsed_ns,
                AgentToolSourceGapReason::RepeatedTurnTerminal,
                None,
            )];
        }
        self.terminal = true;

        let mut candidates = Vec::with_capacity(self.open.len());
        for source_tool_call_id in std::mem::take(&mut self.open_order) {
            if self.open.remove(&source_tool_call_id).is_none() {
                continue;
            }
            self.finished.insert(Arc::clone(&source_tool_call_id));
            candidates.push(finished(
                elapsed_ns,
                source_tool_call_id,
                AgentToolTerminalOutcome::Cancelled,
            ));
        }
        candidates
    }

    pub fn active_tool_count(&self) -> usize {
        self.open.len()
    }

    pub const fn is_terminal(&self) -> bool {
        self.terminal
    }

    fn observe_started(
        &mut self,
        observation: &AgentToolStartedObservation,
        elapsed_ns: u64,
    ) -> Vec<AgentToolCandidate> {
        self.start(
            observation.source_tool_call_id(),
            observation.title(),
            observation.tool_kind(),
            observation.status(),
            elapsed_ns,
        )
    }

    fn start(
        &mut self,
        source_tool_call_id: &str,
        title: &str,
        tool_kind: ToolKind,
        status: ToolCallStatus,
        elapsed_ns: u64,
    ) -> Vec<AgentToolCandidate> {
        if let Some(candidate) = self.reject_common(source_tool_call_id, elapsed_ns) {
            return vec![candidate];
        }
        if self.open.contains_key(source_tool_call_id) {
            return vec![source_gap(
                elapsed_ns,
                AgentToolSourceGapReason::DuplicateActiveToolCall,
                Some(Arc::from(source_tool_call_id)),
            )];
        }
        if self.finished.contains(source_tool_call_id) {
            return vec![source_gap(
                elapsed_ns,
                AgentToolSourceGapReason::FinishedToolCallIdReused,
                Some(Arc::from(source_tool_call_id)),
            )];
        }

        let metadata = AgentToolMetadata {
            source_tool_call_id: Arc::from(source_tool_call_id),
            title: Arc::from(title),
            tool_kind,
            status,
        };
        let mut candidates = vec![AgentToolCandidate::Started(AgentToolActivityCandidate {
            elapsed_ns,
            metadata: metadata.clone(),
        })];
        if let Some(outcome) = terminal_outcome(metadata.status) {
            self.finished
                .insert(Arc::clone(&metadata.source_tool_call_id));
            candidates.push(finished(elapsed_ns, metadata.source_tool_call_id, outcome));
        } else {
            self.open_order
                .push(Arc::clone(&metadata.source_tool_call_id));
            self.open
                .insert(Arc::clone(&metadata.source_tool_call_id), metadata);
        }
        candidates
    }

    fn observe_updated(
        &mut self,
        observation: &AgentToolUpdatedObservation,
        elapsed_ns: u64,
    ) -> Vec<AgentToolCandidate> {
        self.update(
            observation.source_tool_call_id(),
            observation.title(),
            observation.tool_kind(),
            observation.status(),
            elapsed_ns,
        )
    }

    fn update(
        &mut self,
        source_tool_call_id: &str,
        title: Option<&str>,
        tool_kind: Option<ToolKind>,
        status: Option<ToolCallStatus>,
        elapsed_ns: u64,
    ) -> Vec<AgentToolCandidate> {
        if let Some(candidate) = self.reject_common(source_tool_call_id, elapsed_ns) {
            return vec![candidate];
        }
        if self.finished.contains(source_tool_call_id) {
            return vec![source_gap(
                elapsed_ns,
                AgentToolSourceGapReason::UpdateAfterFinish,
                Some(Arc::from(source_tool_call_id)),
            )];
        }
        let Some(open) = self.open.get_mut(source_tool_call_id) else {
            return vec![source_gap(
                elapsed_ns,
                AgentToolSourceGapReason::UpdateBeforeStart,
                Some(Arc::from(source_tool_call_id)),
            )];
        };

        let next_status = status.unwrap_or(open.status);
        if !valid_status_transition(open.status, next_status) {
            return vec![source_gap(
                elapsed_ns,
                AgentToolSourceGapReason::InvalidStatusTransition,
                Some(Arc::from(source_tool_call_id)),
            )];
        }
        if let Some(title) = title {
            open.title = Arc::from(title);
        }
        if let Some(tool_kind) = tool_kind {
            open.tool_kind = tool_kind;
        }
        open.status = next_status;

        let metadata = open.clone();
        let mut candidates = vec![AgentToolCandidate::Updated(AgentToolActivityCandidate {
            elapsed_ns,
            metadata: metadata.clone(),
        })];
        if let Some(outcome) = terminal_outcome(metadata.status) {
            self.open.remove(source_tool_call_id);
            self.finished
                .insert(Arc::clone(&metadata.source_tool_call_id));
            candidates.push(finished(elapsed_ns, metadata.source_tool_call_id, outcome));
        }
        candidates
    }

    fn reject_common(
        &self,
        source_tool_call_id: &str,
        elapsed_ns: u64,
    ) -> Option<AgentToolCandidate> {
        if self.terminal {
            return Some(source_gap(
                elapsed_ns,
                AgentToolSourceGapReason::ActivityAfterTurnTerminal,
                Some(Arc::from(source_tool_call_id)),
            ));
        }
        if source_tool_call_id.is_empty() {
            return Some(source_gap(
                elapsed_ns,
                AgentToolSourceGapReason::EmptyToolCallId,
                None,
            ));
        }
        None
    }
}

impl fmt::Debug for AgentToolNormalizer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentToolNormalizer")
            .field("open_tool_count", &self.open.len())
            .field("finished_tool_count", &self.finished.len())
            .field("terminal", &self.terminal)
            .finish()
    }
}

fn valid_status_transition(current: ToolCallStatus, next: ToolCallStatus) -> bool {
    match current {
        ToolCallStatus::Pending => true,
        ToolCallStatus::InProgress => !matches!(next, ToolCallStatus::Pending),
        ToolCallStatus::Completed | ToolCallStatus::Failed => false,
    }
}

fn terminal_outcome(status: ToolCallStatus) -> Option<AgentToolTerminalOutcome> {
    match status {
        ToolCallStatus::Completed => Some(AgentToolTerminalOutcome::Completed),
        ToolCallStatus::Failed => Some(AgentToolTerminalOutcome::Failed),
        ToolCallStatus::Pending | ToolCallStatus::InProgress => None,
    }
}

fn finished(
    elapsed_ns: u64,
    source_tool_call_id: Arc<str>,
    outcome: AgentToolTerminalOutcome,
) -> AgentToolCandidate {
    AgentToolCandidate::Finished(AgentToolFinishedCandidate {
        elapsed_ns,
        source_tool_call_id,
        outcome,
        error_code: (outcome == AgentToolTerminalOutcome::Failed)
            .then_some(AgentToolErrorCode::ToolFailed),
    })
}

fn source_gap(
    elapsed_ns: u64,
    reason: AgentToolSourceGapReason,
    source_tool_call_id: Option<Arc<str>>,
) -> AgentToolCandidate {
    AgentToolCandidate::SourceGap(AgentToolSourceGapCandidate {
        elapsed_ns,
        reason,
        source_tool_call_id,
    })
}

fn started_observation(
    turn: Arc<AgentTurnDiagnosticMetadata>,
    call: &ToolCall,
) -> Option<AgentToolObservation> {
    Some(AgentToolObservation::Started(AgentToolStartedObservation {
        turn,
        source_tool_call_id: Arc::clone(&call.tool_call_id.0),
        title: Arc::from(call.title.as_str()),
        tool_kind: normalize_tool_kind(call.kind),
        status: normalize_status(call.status)?,
    }))
}

fn updated_observation(
    turn: Arc<AgentTurnDiagnosticMetadata>,
    update: &ToolCallUpdate,
) -> Option<AgentToolObservation> {
    let status = match update.fields.status {
        Some(status) => Some(normalize_status(status)?),
        None => None,
    };
    Some(AgentToolObservation::Updated(AgentToolUpdatedObservation {
        turn,
        source_tool_call_id: Arc::clone(&update.tool_call_id.0),
        title: update.fields.title.as_deref().map(Arc::from),
        tool_kind: update.fields.kind.map(normalize_tool_kind),
        status,
    }))
}

const fn normalize_tool_kind(kind: AcpToolKind) -> ToolKind {
    match kind {
        AcpToolKind::Read => ToolKind::Read,
        AcpToolKind::Edit => ToolKind::Edit,
        AcpToolKind::Delete => ToolKind::Delete,
        AcpToolKind::Move => ToolKind::Move,
        AcpToolKind::Search => ToolKind::Search,
        AcpToolKind::Execute => ToolKind::Execute,
        AcpToolKind::Think => ToolKind::Think,
        AcpToolKind::Fetch => ToolKind::Fetch,
        AcpToolKind::SwitchMode => ToolKind::SwitchMode,
        AcpToolKind::Other => ToolKind::Other,
        _ => ToolKind::Other,
    }
}

const fn normalize_status(status: AcpToolCallStatus) -> Option<ToolCallStatus> {
    match status {
        AcpToolCallStatus::Pending => Some(ToolCallStatus::Pending),
        AcpToolCallStatus::InProgress => Some(ToolCallStatus::InProgress),
        AcpToolCallStatus::Completed => Some(ToolCallStatus::Completed),
        AcpToolCallStatus::Failed => Some(ToolCallStatus::Failed),
        _ => None,
    }
}

#[inline]
pub(crate) fn observe_update(context: &AgentDiagnosticUpdateContext<'_>, update: &SessionUpdate) {
    let Some(turn) = context
        .turn
        .and_then(|turn| turn.runtime_metadata())
        .cloned()
        .map(Arc::new)
    else {
        return;
    };
    let observation = match update {
        SessionUpdate::ToolCall(call) => started_observation(turn, call),
        SessionUpdate::ToolCallUpdate(update) => updated_observation(turn, update),
        _ => None,
    };
    let Some(observation) = observation else {
        return;
    };
    context
        .observer
        .observe(AgentDiagnosticObservation::Candidate(Arc::new(observation)));
}
