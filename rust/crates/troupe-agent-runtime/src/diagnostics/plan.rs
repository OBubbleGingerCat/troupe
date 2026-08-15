use std::io::{self, Write};
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    Plan, PlanEntry as AcpPlanEntry, PlanEntryPriority as AcpPlanEntryPriority,
    PlanEntryStatus as AcpPlanEntryStatus, SessionUpdate,
};
use troupe_diagnostics_core::detail::PlanEntry;
use troupe_diagnostics_core::kinds::{PlanEntryPriority, PlanEntryStatus};

use super::observer::{AgentDiagnosticCandidate, AgentDiagnosticObservation};
use super::session::{
    AgentDiagnosticUpdateContext, AgentSessionDiagnosticMetadata, AgentTurnDiagnosticMetadata,
};

pub const MAX_AGENT_PLAN_SNAPSHOT_BYTES: usize = 256 * 1024;
pub const AGENT_PLAN_SNAPSHOT_CANDIDATE_KIND: &str = "agent_plan_snapshot";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentPlanSnapshotPayload {
    entries: Vec<PlanEntry>,
    truncated: bool,
}

impl AgentPlanSnapshotPayload {
    /// Normalizes the complete ordered ACP plan. The byte budget applies to the canonical JSON
    /// encoding of `entries`, excluding protocol metadata and the surrounding ACP envelope.
    pub fn from_acp(plan: &Plan) -> Self {
        if canonical_entries_size(&plan.entries).is_none() {
            return Self {
                entries: Vec::new(),
                truncated: true,
            };
        }

        let Some(entries) = plan.entries.iter().map(normalize_entry).collect() else {
            return Self {
                entries: Vec::new(),
                truncated: true,
            };
        };
        Self {
            entries,
            truncated: false,
        }
    }

    pub fn entries(&self) -> &[PlanEntry] {
        &self.entries
    }

    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

/// Stable runtime metadata copied at the point where the plan update was observed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentPlanSnapshotMetadata {
    Turn(AgentTurnDiagnosticMetadata),
    Session(AgentSessionDiagnosticMetadata),
}

impl AgentPlanSnapshotMetadata {
    pub fn effective_model(&self) -> Option<&str> {
        match self {
            Self::Turn(metadata) => Some(metadata.effective_model()),
            Self::Session(metadata) => metadata.effective_model(),
        }
    }

    pub fn effective_effort(&self) -> Option<&str> {
        match self {
            Self::Turn(metadata) => metadata.effective_effort(),
            Self::Session(metadata) => metadata.effective_effort(),
        }
    }

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
pub struct AgentPlanSnapshotCandidate {
    metadata: AgentPlanSnapshotMetadata,
    payload: AgentPlanSnapshotPayload,
}

impl AgentPlanSnapshotCandidate {
    fn new(metadata: AgentPlanSnapshotMetadata, payload: AgentPlanSnapshotPayload) -> Self {
        Self { metadata, payload }
    }

    pub const fn metadata(&self) -> &AgentPlanSnapshotMetadata {
        &self.metadata
    }

    pub const fn payload(&self) -> &AgentPlanSnapshotPayload {
        &self.payload
    }

    pub fn entries(&self) -> &[PlanEntry] {
        self.payload.entries()
    }

    pub const fn truncated(&self) -> bool {
        self.payload.truncated()
    }
}

impl AgentDiagnosticCandidate for AgentPlanSnapshotCandidate {
    fn kind(&self) -> &'static str {
        AGENT_PLAN_SNAPSHOT_CANDIDATE_KIND
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[inline]
pub(crate) fn observe_update(context: &AgentDiagnosticUpdateContext<'_>, update: &SessionUpdate) {
    let SessionUpdate::Plan(plan) = update else {
        return;
    };
    let Some(metadata) = observation_metadata(context) else {
        return;
    };
    let candidate =
        AgentPlanSnapshotCandidate::new(metadata, AgentPlanSnapshotPayload::from_acp(plan));
    context
        .observer
        .observe(AgentDiagnosticObservation::Candidate(Arc::new(candidate)));
}

fn observation_metadata(
    context: &AgentDiagnosticUpdateContext<'_>,
) -> Option<AgentPlanSnapshotMetadata> {
    if let Some(metadata) = context
        .turn
        .and_then(|turn| turn.runtime_metadata())
        .cloned()
    {
        return Some(AgentPlanSnapshotMetadata::Turn(metadata));
    }
    context
        .session
        .as_deref()
        .cloned()
        .map(AgentPlanSnapshotMetadata::Session)
}

fn normalize_entry(entry: &AcpPlanEntry) -> Option<PlanEntry> {
    Some(PlanEntry::new(
        entry.content.clone(),
        normalize_priority(&entry.priority)?.0,
        normalize_status(&entry.status)?.0,
    ))
}

fn normalize_priority(
    priority: &AcpPlanEntryPriority,
) -> Option<(PlanEntryPriority, &'static [u8])> {
    match priority {
        AcpPlanEntryPriority::High => Some((PlanEntryPriority::High, br#""high""#)),
        AcpPlanEntryPriority::Medium => Some((PlanEntryPriority::Medium, br#""medium""#)),
        AcpPlanEntryPriority::Low => Some((PlanEntryPriority::Low, br#""low""#)),
        _ => None,
    }
}

fn normalize_status(status: &AcpPlanEntryStatus) -> Option<(PlanEntryStatus, &'static [u8])> {
    match status {
        AcpPlanEntryStatus::Pending => Some((PlanEntryStatus::Pending, br#""pending""#)),
        AcpPlanEntryStatus::InProgress => Some((PlanEntryStatus::InProgress, br#""in_progress""#)),
        AcpPlanEntryStatus::Completed => Some((PlanEntryStatus::Completed, br#""completed""#)),
        _ => None,
    }
}

fn canonical_entries_size(entries: &[AcpPlanEntry]) -> Option<usize> {
    let mut counter = BoundedByteCounter::default();
    counter.append(b"[")?;
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            counter.append(b",")?;
        }
        let (_, priority) = normalize_priority(&entry.priority)?;
        let (_, status) = normalize_status(&entry.status)?;
        counter.append(br#"{"content":"#)?;
        serde_json::to_writer(&mut counter, &entry.content).ok()?;
        counter.append(b",\"priority\":")?;
        counter.append(priority)?;
        counter.append(b",\"status\":")?;
        counter.append(status)?;
        counter.append(b"}")?;
    }
    counter.append(b"]")?;
    Some(counter.bytes)
}

#[derive(Default)]
struct BoundedByteCounter {
    bytes: usize,
}

impl BoundedByteCounter {
    fn append(&mut self, bytes: &[u8]) -> Option<()> {
        self.write_all(bytes).ok()
    }
}

impl Write for BoundedByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self
            .bytes
            .checked_add(bytes.len())
            .filter(|next| *next <= MAX_AGENT_PLAN_SNAPSHOT_BYTES)
            .ok_or_else(|| io::Error::other("agent plan snapshot byte limit exceeded"))?;
        self.bytes = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
