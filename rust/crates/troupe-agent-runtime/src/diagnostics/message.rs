use std::{
    any::Any,
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
};

use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate};
use troupe_diagnostics_core::id::RunLocalId;
use uuid::Uuid;

use super::{
    observer::{AgentDiagnosticCandidate, AgentDiagnosticObservation},
    session::{AgentDiagnosticUpdateContext, AgentTurnDiagnosticMetadata},
};

pub const AGENT_MESSAGE_DELTA_FLUSH_BYTES: usize = 16 * 1024;
pub const AGENT_MESSAGE_DELTA_FLUSH_NS: u64 = 20_000_000;
pub const AGENT_MESSAGE_MAX_BYTES: usize = 4 * 1024 * 1024;
pub const ACT_AGENT_MESSAGES_MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentMessageChunkObservation {
    turn: Arc<AgentTurnDiagnosticMetadata>,
    source_message_id: Option<Arc<str>>,
    text: Arc<str>,
}

impl AgentMessageChunkObservation {
    pub fn turn(&self) -> &AgentTurnDiagnosticMetadata {
        &self.turn
    }

    pub fn source_message_id(&self) -> Option<&str> {
        self.source_message_id.as_deref()
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

impl AgentDiagnosticCandidate for AgentMessageChunkObservation {
    fn kind(&self) -> &'static str {
        "agent_message_chunk"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentMessageDeltaCandidate {
    elapsed_ns: u64,
    message_id: RunLocalId,
    source_message_id: Option<Arc<str>>,
    text_delta: String,
}

impl AgentMessageDeltaCandidate {
    pub const fn elapsed_ns(&self) -> u64 {
        self.elapsed_ns
    }

    pub const fn message_id(&self) -> &RunLocalId {
        &self.message_id
    }

    pub fn source_message_id(&self) -> Option<&str> {
        self.source_message_id.as_deref()
    }

    pub fn text_delta(&self) -> &str {
        &self.text_delta
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentMessageCompletedCandidate {
    elapsed_ns: u64,
    message_id: RunLocalId,
    utf8_bytes: u64,
    unicode_scalar_count: u64,
    truncated: bool,
}

impl AgentMessageCompletedCandidate {
    pub const fn elapsed_ns(&self) -> u64 {
        self.elapsed_ns
    }

    pub const fn message_id(&self) -> &RunLocalId {
        &self.message_id
    }

    pub const fn utf8_bytes(&self) -> u64 {
        self.utf8_bytes
    }

    pub const fn unicode_scalar_count(&self) -> u64 {
        self.unicode_scalar_count
    }

    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentMessageSourceGapReason {
    CompletedSourceMessageIdReused,
}

impl AgentMessageSourceGapReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CompletedSourceMessageIdReused => "completed_source_message_id_reused",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentMessageSourceGapCandidate {
    elapsed_ns: u64,
    reason: AgentMessageSourceGapReason,
    source_message_id: Arc<str>,
    previous_message_id: RunLocalId,
    replacement_message_id: RunLocalId,
}

impl AgentMessageSourceGapCandidate {
    pub const fn elapsed_ns(&self) -> u64 {
        self.elapsed_ns
    }

    pub const fn reason(&self) -> AgentMessageSourceGapReason {
        self.reason
    }

    pub fn source_message_id(&self) -> &str {
        &self.source_message_id
    }

    pub const fn previous_message_id(&self) -> &RunLocalId {
        &self.previous_message_id
    }

    pub const fn replacement_message_id(&self) -> &RunLocalId {
        &self.replacement_message_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentMessageCandidate {
    Delta(AgentMessageDeltaCandidate),
    Completed(AgentMessageCompletedCandidate),
    SourceGap(AgentMessageSourceGapCandidate),
}

impl AgentMessageCandidate {
    pub const fn delta(&self) -> Option<&AgentMessageDeltaCandidate> {
        match self {
            Self::Delta(candidate) => Some(candidate),
            Self::Completed(_) | Self::SourceGap(_) => None,
        }
    }

    pub const fn completed(&self) -> Option<&AgentMessageCompletedCandidate> {
        match self {
            Self::Completed(candidate) => Some(candidate),
            Self::Delta(_) | Self::SourceGap(_) => None,
        }
    }

    pub const fn source_gap(&self) -> Option<&AgentMessageSourceGapCandidate> {
        match self {
            Self::SourceGap(candidate) => Some(candidate),
            Self::Delta(_) | Self::Completed(_) => None,
        }
    }
}

impl AgentDiagnosticCandidate for AgentMessageCandidate {
    fn kind(&self) -> &'static str {
        match self {
            Self::Delta(_) => "agent_message_delta",
            Self::Completed(_) => "agent_message_completed",
            Self::SourceGap(_) => "observation_gap",
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentMessageNormalizationError {
    TurnAlreadyTerminal,
}

impl AgentMessageNormalizationError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::TurnAlreadyTerminal => "turn_already_terminal",
        }
    }
}

impl fmt::Display for AgentMessageNormalizationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for AgentMessageNormalizationError {}

#[derive(Debug)]
struct OpenMessage {
    message_id: RunLocalId,
    source_message_id: Option<Arc<str>>,
    utf8_bytes: usize,
    unicode_scalar_count: usize,
    truncated: bool,
}

#[derive(Debug)]
struct PendingDelta {
    elapsed_ns: u64,
    message_id: RunLocalId,
    source_message_id: Option<Arc<str>>,
    text: String,
}

pub struct AgentMessageNormalizer {
    open_messages: Vec<OpenMessage>,
    anonymous_message_id: Option<RunLocalId>,
    active_explicit_message_id: Option<RunLocalId>,
    completed_explicit_message_ids: HashMap<Arc<str>, RunLocalId>,
    allocated_message_ids: HashSet<RunLocalId>,
    pending_delta: Option<PendingDelta>,
    captured_act_utf8_bytes: usize,
    terminal: bool,
}

impl AgentMessageNormalizer {
    pub fn new() -> Self {
        Self {
            open_messages: Vec::new(),
            anonymous_message_id: None,
            active_explicit_message_id: None,
            completed_explicit_message_ids: HashMap::new(),
            allocated_message_ids: HashSet::new(),
            pending_delta: None,
            captured_act_utf8_bytes: 0,
            terminal: false,
        }
    }

    pub fn observe_session_update(
        &mut self,
        update: &SessionUpdate,
        elapsed_ns: u64,
    ) -> Result<Vec<AgentMessageCandidate>, AgentMessageNormalizationError> {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                ContentBlock::Text(text) => {
                    let source_message_id = chunk.message_id.as_ref().map(ToString::to_string);
                    self.observe_text(source_message_id.as_deref(), &text.text, elapsed_ns)
                }
                _ => self.flush_elapsed(elapsed_ns),
            },
            SessionUpdate::AgentThoughtChunk(_)
            | SessionUpdate::ToolCall(_)
            | SessionUpdate::ToolCallUpdate(_)
            | SessionUpdate::Plan(_)
            | SessionUpdate::UsageUpdate(_) => self.observe_other_candidate(elapsed_ns),
            _ => self.flush_elapsed(elapsed_ns),
        }
    }

    pub fn observe_chunk(
        &mut self,
        observation: &AgentMessageChunkObservation,
        elapsed_ns: u64,
    ) -> Result<Vec<AgentMessageCandidate>, AgentMessageNormalizationError> {
        self.observe_text(
            observation.source_message_id(),
            observation.text(),
            elapsed_ns,
        )
    }

    pub fn observe_text(
        &mut self,
        source_message_id: Option<&str>,
        text: &str,
        elapsed_ns: u64,
    ) -> Result<Vec<AgentMessageCandidate>, AgentMessageNormalizationError> {
        self.ensure_open()?;
        let mut candidates = Vec::new();
        self.flush_if_elapsed(elapsed_ns, &mut candidates);
        if text.is_empty() {
            return Ok(candidates);
        }

        let message_id = self.message_for_source(source_message_id, elapsed_ns, &mut candidates);
        self.capture_text(&message_id, text, elapsed_ns, &mut candidates);
        Ok(candidates)
    }

    pub fn flush_elapsed(
        &mut self,
        elapsed_ns: u64,
    ) -> Result<Vec<AgentMessageCandidate>, AgentMessageNormalizationError> {
        self.ensure_open()?;
        let mut candidates = Vec::new();
        self.flush_if_elapsed(elapsed_ns, &mut candidates);
        Ok(candidates)
    }

    pub fn observe_other_candidate(
        &mut self,
        _elapsed_ns: u64,
    ) -> Result<Vec<AgentMessageCandidate>, AgentMessageNormalizationError> {
        self.ensure_open()?;
        let mut candidates = Vec::new();
        self.flush_pending(&mut candidates);
        Ok(candidates)
    }

    pub fn turn_terminal(
        &mut self,
        elapsed_ns: u64,
        source_truncated: bool,
    ) -> Result<Vec<AgentMessageCandidate>, AgentMessageNormalizationError> {
        self.ensure_open()?;
        let mut candidates = Vec::new();
        self.flush_pending(&mut candidates);

        for mut message in std::mem::take(&mut self.open_messages) {
            message.truncated |= source_truncated;
            if let Some(source_message_id) = &message.source_message_id {
                self.completed_explicit_message_ids
                    .insert(Arc::clone(source_message_id), message.message_id.clone());
            }
            candidates.push(AgentMessageCandidate::Completed(
                AgentMessageCompletedCandidate {
                    elapsed_ns,
                    message_id: message.message_id,
                    utf8_bytes: message.utf8_bytes as u64,
                    unicode_scalar_count: message.unicode_scalar_count as u64,
                    truncated: message.truncated,
                },
            ));
        }

        self.anonymous_message_id = None;
        self.active_explicit_message_id = None;
        self.terminal = true;
        Ok(candidates)
    }

    pub fn next_flush_deadline_ns(&self) -> Option<u64> {
        self.pending_delta
            .as_ref()
            .and_then(|pending| pending.elapsed_ns.checked_add(AGENT_MESSAGE_DELTA_FLUSH_NS))
    }

    pub const fn captured_act_utf8_bytes(&self) -> usize {
        self.captured_act_utf8_bytes
    }

    pub const fn is_terminal(&self) -> bool {
        self.terminal
    }

    fn ensure_open(&self) -> Result<(), AgentMessageNormalizationError> {
        if self.terminal {
            Err(AgentMessageNormalizationError::TurnAlreadyTerminal)
        } else {
            Ok(())
        }
    }

    fn message_for_source(
        &mut self,
        source_message_id: Option<&str>,
        elapsed_ns: u64,
        candidates: &mut Vec<AgentMessageCandidate>,
    ) -> RunLocalId {
        let Some(source_message_id) = source_message_id else {
            if let Some(message_id) = &self.anonymous_message_id {
                return message_id.clone();
            }
            let message_id = self.allocate_message_id();
            self.open_messages.push(OpenMessage {
                message_id: message_id.clone(),
                source_message_id: None,
                utf8_bytes: 0,
                unicode_scalar_count: 0,
                truncated: false,
            });
            self.anonymous_message_id = Some(message_id.clone());
            return message_id;
        };

        if let Some(active_message_id) = &self.active_explicit_message_id {
            let active_source = self
                .message(active_message_id)
                .and_then(|message| message.source_message_id.as_deref());
            if active_source == Some(source_message_id) {
                return active_message_id.clone();
            }
        }

        self.flush_pending(candidates);
        if let Some(previous_message_id) = self.active_explicit_message_id.take() {
            self.complete_message(&previous_message_id, elapsed_ns, candidates);
        }

        let previous_generation = self
            .completed_explicit_message_ids
            .get(source_message_id)
            .cloned();
        let message_id = self.allocate_message_id();
        let source_message_id: Arc<str> = Arc::from(source_message_id);
        self.open_messages.push(OpenMessage {
            message_id: message_id.clone(),
            source_message_id: Some(Arc::clone(&source_message_id)),
            utf8_bytes: 0,
            unicode_scalar_count: 0,
            truncated: false,
        });
        self.active_explicit_message_id = Some(message_id.clone());

        if let Some(previous_message_id) = previous_generation {
            candidates.push(AgentMessageCandidate::SourceGap(
                AgentMessageSourceGapCandidate {
                    elapsed_ns,
                    reason: AgentMessageSourceGapReason::CompletedSourceMessageIdReused,
                    source_message_id,
                    previous_message_id,
                    replacement_message_id: message_id.clone(),
                },
            ));
        }
        message_id
    }

    fn capture_text(
        &mut self,
        message_id: &RunLocalId,
        text: &str,
        elapsed_ns: u64,
        candidates: &mut Vec<AgentMessageCandidate>,
    ) {
        let position = self
            .message_position(message_id)
            .expect("open message identity must resolve");
        if self.open_messages[position].truncated {
            return;
        }

        let message_remaining = AGENT_MESSAGE_MAX_BYTES - self.open_messages[position].utf8_bytes;
        let act_remaining = ACT_AGENT_MESSAGES_MAX_BYTES - self.captured_act_utf8_bytes;
        let available = message_remaining.min(act_remaining);
        let captured_len = utf8_prefix_len(text, available);

        if captured_len == 0 {
            self.open_messages[position].truncated = true;
            return;
        }

        let captured = &text[..captured_len];
        let source_message_id = self.open_messages[position].source_message_id.clone();
        self.open_messages[position].utf8_bytes += captured_len;
        self.open_messages[position].unicode_scalar_count += captured.chars().count();
        self.captured_act_utf8_bytes += captured_len;
        self.append_captured(
            message_id,
            source_message_id,
            captured,
            elapsed_ns,
            candidates,
        );

        if captured_len < text.len() {
            self.open_messages[position].truncated = true;
        }
    }

    fn append_captured(
        &mut self,
        message_id: &RunLocalId,
        source_message_id: Option<Arc<str>>,
        mut text: &str,
        elapsed_ns: u64,
        candidates: &mut Vec<AgentMessageCandidate>,
    ) {
        while !text.is_empty() {
            if self
                .pending_delta
                .as_ref()
                .is_some_and(|pending| pending.message_id != *message_id)
            {
                self.flush_pending(candidates);
            }
            if self.pending_delta.is_none() {
                self.pending_delta = Some(PendingDelta {
                    elapsed_ns,
                    message_id: message_id.clone(),
                    source_message_id: source_message_id.clone(),
                    text: String::new(),
                });
            }

            let pending_len = self
                .pending_delta
                .as_ref()
                .map_or(0, |pending| pending.text.len());
            let capacity = AGENT_MESSAGE_DELTA_FLUSH_BYTES - pending_len;
            let captured_len = utf8_prefix_len(text, capacity);
            if captured_len == 0 {
                self.flush_pending(candidates);
                continue;
            }

            self.pending_delta
                .as_mut()
                .expect("pending delta was initialized")
                .text
                .push_str(&text[..captured_len]);
            text = &text[captured_len..];
            if self
                .pending_delta
                .as_ref()
                .is_some_and(|pending| pending.text.len() == AGENT_MESSAGE_DELTA_FLUSH_BYTES)
            {
                self.flush_pending(candidates);
            }
        }
    }

    fn flush_if_elapsed(&mut self, elapsed_ns: u64, candidates: &mut Vec<AgentMessageCandidate>) {
        if self.pending_delta.as_ref().is_some_and(|pending| {
            elapsed_ns.saturating_sub(pending.elapsed_ns) >= AGENT_MESSAGE_DELTA_FLUSH_NS
        }) {
            self.flush_pending(candidates);
        }
    }

    fn flush_pending(&mut self, candidates: &mut Vec<AgentMessageCandidate>) {
        let Some(pending) = self.pending_delta.take() else {
            return;
        };
        debug_assert!(!pending.text.is_empty());
        candidates.push(AgentMessageCandidate::Delta(AgentMessageDeltaCandidate {
            elapsed_ns: pending.elapsed_ns,
            message_id: pending.message_id,
            source_message_id: pending.source_message_id,
            text_delta: pending.text,
        }));
    }

    fn complete_message(
        &mut self,
        message_id: &RunLocalId,
        elapsed_ns: u64,
        candidates: &mut Vec<AgentMessageCandidate>,
    ) {
        let position = self
            .message_position(message_id)
            .expect("completed message identity must resolve");
        let message = self.open_messages.remove(position);
        if let Some(source_message_id) = &message.source_message_id {
            self.completed_explicit_message_ids
                .insert(Arc::clone(source_message_id), message.message_id.clone());
        } else {
            self.anonymous_message_id = None;
        }
        candidates.push(AgentMessageCandidate::Completed(
            AgentMessageCompletedCandidate {
                elapsed_ns,
                message_id: message.message_id,
                utf8_bytes: message.utf8_bytes as u64,
                unicode_scalar_count: message.unicode_scalar_count as u64,
                truncated: message.truncated,
            },
        ));
    }

    fn allocate_message_id(&mut self) -> RunLocalId {
        loop {
            let value = format!("message-{}", Uuid::new_v4().simple());
            let message_id =
                RunLocalId::parse(&value).expect("UUID-derived message ID is schema-valid");
            if self.allocated_message_ids.insert(message_id.clone()) {
                return message_id;
            }
        }
    }

    fn message(&self, message_id: &RunLocalId) -> Option<&OpenMessage> {
        self.open_messages
            .iter()
            .find(|message| message.message_id == *message_id)
    }

    fn message_position(&self, message_id: &RunLocalId) -> Option<usize> {
        self.open_messages
            .iter()
            .position(|message| message.message_id == *message_id)
    }
}

impl Default for AgentMessageNormalizer {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for AgentMessageNormalizer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentMessageNormalizer")
            .field("open_message_count", &self.open_messages.len())
            .field("has_pending_delta", &self.pending_delta.is_some())
            .field("captured_act_utf8_bytes", &self.captured_act_utf8_bytes)
            .field("terminal", &self.terminal)
            .finish()
    }
}

fn utf8_prefix_len(text: &str, limit: usize) -> usize {
    if text.len() <= limit {
        return text.len();
    }
    let mut prefix_len = limit;
    while prefix_len > 0 && !text.is_char_boundary(prefix_len) {
        prefix_len -= 1;
    }
    prefix_len
}

#[inline]
pub(crate) fn observe_update(context: &AgentDiagnosticUpdateContext<'_>, update: &SessionUpdate) {
    let SessionUpdate::AgentMessageChunk(chunk) = update else {
        return;
    };
    let ContentBlock::Text(text) = &chunk.content else {
        return;
    };
    if text.text.is_empty() {
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
            AgentMessageChunkObservation {
                turn: Arc::new(turn),
                source_message_id: chunk
                    .message_id
                    .as_ref()
                    .map(|message_id| Arc::from(message_id.to_string())),
                text: Arc::from(text.text.as_str()),
            },
        )));
}
