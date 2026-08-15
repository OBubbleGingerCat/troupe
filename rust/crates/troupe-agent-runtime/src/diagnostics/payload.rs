use std::{
    any::Any,
    fmt,
    io::{self, Write},
    sync::Arc,
};

use agent_client_protocol::schema::v1::{
    Annotations, ContentBlock, EmbeddedResourceResource, Role, SessionUpdate, ToolCallContent,
    ToolCallLocation,
};
use serde_json::{Map, Value};

use super::{
    observer::{AgentDiagnosticCandidate, AgentDiagnosticObservation},
    session::{AgentDiagnosticUpdateContext, AgentTurnDiagnosticMetadata},
};

pub const TOOL_PAYLOAD_MAX_DEPTH: usize = 32;
pub const TOOL_PAYLOAD_MAX_NODES: usize = 65_536;
pub const TOOL_PAYLOAD_SNAPSHOT_MAX_BYTES: usize = 256 * 1024;
pub const ACT_TOOL_PAYLOAD_MAX_BYTES: usize = 4 * 1024 * 1024;
pub const AGENT_TOOL_PAYLOAD_CANDIDATE_KIND: &str = "agent_tool_payload_sidecar";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ToolPayloadCapturePolicy {
    capture_input: bool,
    capture_output: bool,
}

impl ToolPayloadCapturePolicy {
    pub const fn new(capture_input: bool, capture_output: bool) -> Self {
        Self {
            capture_input,
            capture_output,
        }
    }

    pub const fn capture_input(self) -> bool {
        self.capture_input
    }

    pub const fn capture_output(self) -> bool {
        self.capture_output
    }

    pub const fn captures_payload(self) -> bool {
        self.capture_input || self.capture_output
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolPayloadSource {
    Started,
    Updated,
}

impl ToolPayloadSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Updated => "updated",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolPayloadOmissionReason {
    InvalidType,
    DepthLimit,
    NodeLimit,
    SnapshotByteLimit,
    ActByteLimit,
}

impl ToolPayloadOmissionReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidType => "invalid_type",
            Self::DepthLimit => "depth_limit",
            Self::NodeLimit => "node_limit",
            Self::SnapshotByteLimit => "snapshot_byte_limit",
            Self::ActByteLimit => "act_byte_limit",
        }
    }
}

/// An immutable JSON value that is retained only for the per-Act sink sidecar.
///
/// This type intentionally does not implement `Serialize`: canonical diagnostics, the store,
/// Web, and Perfetto cannot accidentally treat the opaque value as event data.
#[derive(Clone, Eq, PartialEq)]
pub struct SinkOnlyJsonValue(Value);

impl SinkOnlyJsonValue {
    pub const fn as_json(&self) -> &Value {
        &self.0
    }
}

impl fmt::Debug for SinkOnlyJsonValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SinkOnlyJsonValue(<redacted>)")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SinkOnlyToolLocation {
    path: Arc<str>,
    line: Option<u32>,
}

impl SinkOnlyToolLocation {
    pub fn path(&self) -> &str {
        &self.path
    }

    pub const fn line(&self) -> Option<u32> {
        self.line
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct SinkOnlyToolInput {
    raw_input: Option<SinkOnlyJsonValue>,
    canonical_bytes: usize,
    omission_reason: Option<ToolPayloadOmissionReason>,
}

impl SinkOnlyToolInput {
    pub const fn raw_input(&self) -> Option<&SinkOnlyJsonValue> {
        self.raw_input.as_ref()
    }

    pub const fn canonical_bytes(&self) -> usize {
        self.canonical_bytes
    }

    pub const fn truncated(&self) -> bool {
        self.omission_reason.is_some()
    }

    pub const fn omission_reason(&self) -> Option<ToolPayloadOmissionReason> {
        self.omission_reason
    }

    fn captured(value: Value, canonical_bytes: usize) -> Self {
        Self {
            raw_input: Some(SinkOnlyJsonValue(value)),
            canonical_bytes,
            omission_reason: None,
        }
    }

    fn omitted(reason: ToolPayloadOmissionReason) -> Self {
        Self {
            raw_input: None,
            canonical_bytes: 0,
            omission_reason: Some(reason),
        }
    }
}

impl fmt::Debug for SinkOnlyToolInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SinkOnlyToolInput")
            .field("captured", &self.raw_input.is_some())
            .field("canonical_bytes", &self.canonical_bytes)
            .field("omission_reason", &self.omission_reason)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct SinkOnlyToolOutput {
    raw_output: Option<SinkOnlyJsonValue>,
    content: Vec<SinkOnlyJsonValue>,
    locations: Vec<SinkOnlyToolLocation>,
    canonical_bytes: usize,
    omission_reason: Option<ToolPayloadOmissionReason>,
}

impl SinkOnlyToolOutput {
    pub const fn raw_output(&self) -> Option<&SinkOnlyJsonValue> {
        self.raw_output.as_ref()
    }

    pub fn content(&self) -> &[SinkOnlyJsonValue] {
        &self.content
    }

    pub fn locations(&self) -> &[SinkOnlyToolLocation] {
        &self.locations
    }

    pub const fn canonical_bytes(&self) -> usize {
        self.canonical_bytes
    }

    pub const fn truncated(&self) -> bool {
        self.omission_reason.is_some()
    }

    pub const fn omission_reason(&self) -> Option<ToolPayloadOmissionReason> {
        self.omission_reason
    }

    fn captured(
        raw_output: Option<Value>,
        content: Vec<Value>,
        locations: Vec<SinkOnlyToolLocation>,
        canonical_bytes: usize,
    ) -> Self {
        Self {
            raw_output: raw_output.map(SinkOnlyJsonValue),
            content: content.into_iter().map(SinkOnlyJsonValue).collect(),
            locations,
            canonical_bytes,
            omission_reason: None,
        }
    }

    fn omitted(reason: ToolPayloadOmissionReason) -> Self {
        Self {
            raw_output: None,
            content: Vec::new(),
            locations: Vec::new(),
            canonical_bytes: 0,
            omission_reason: Some(reason),
        }
    }
}

impl fmt::Debug for SinkOnlyToolOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SinkOnlyToolOutput")
            .field("raw_output_captured", &self.raw_output.is_some())
            .field("content_items", &self.content.len())
            .field("locations", &self.locations.len())
            .field("canonical_bytes", &self.canonical_bytes)
            .field("omission_reason", &self.omission_reason)
            .finish()
    }
}

/// The provider payload selection carried from the agent boundary to the per-Act sink projector.
///
/// Values are never part of a canonical diagnostic event. B15 clones this selection and applies
/// one `AgentToolPayloadActBudget` before constructing the public sink projection.
#[derive(Clone, Eq, PartialEq)]
pub struct SinkOnlyToolPayload {
    tool_call_id: Arc<str>,
    source: ToolPayloadSource,
    input: Option<SinkOnlyToolInput>,
    output: Option<SinkOnlyToolOutput>,
    act_budget_applied: bool,
}

impl SinkOnlyToolPayload {
    pub fn from_acp(update: &SessionUpdate, policy: ToolPayloadCapturePolicy) -> Option<Self> {
        if !policy.captures_payload() {
            return None;
        }

        let fields = SelectedToolFields::from_update(update)?;
        let input = policy
            .capture_input()
            .then(|| fields.raw_input.map(capture_input))
            .flatten();
        let output = policy
            .capture_output()
            .then(|| fields.output.map(capture_output))
            .flatten();
        if input.is_none() && output.is_none() {
            return None;
        }

        Some(Self {
            tool_call_id: Arc::clone(fields.tool_call_id),
            source: fields.source,
            input,
            output,
            act_budget_applied: false,
        })
    }

    pub fn tool_call_id(&self) -> &str {
        &self.tool_call_id
    }

    pub const fn source(&self) -> ToolPayloadSource {
        self.source
    }

    pub const fn input(&self) -> Option<&SinkOnlyToolInput> {
        self.input.as_ref()
    }

    pub const fn output(&self) -> Option<&SinkOnlyToolOutput> {
        self.output.as_ref()
    }

    pub const fn act_budget_applied(&self) -> bool {
        self.act_budget_applied
    }

    /// Applies the aggregate Act budget once. Input precedes output when both directions occur in
    /// one ACP update, making an otherwise partial remaining budget deterministic.
    pub fn apply_act_budget(&mut self, budget: &mut AgentToolPayloadActBudget) {
        if self.act_budget_applied {
            return;
        }
        if let Some(input) = self.input.as_mut() {
            if let Some(reason) = input.omission_reason {
                budget.note_omission(reason);
            } else if !budget.admit(input.canonical_bytes) {
                *input = SinkOnlyToolInput::omitted(ToolPayloadOmissionReason::ActByteLimit);
            }
        }
        if let Some(output) = self.output.as_mut() {
            if let Some(reason) = output.omission_reason {
                budget.note_omission(reason);
            } else if !budget.admit(output.canonical_bytes) {
                *output = SinkOnlyToolOutput::omitted(ToolPayloadOmissionReason::ActByteLimit);
            }
        }
        self.act_budget_applied = true;
    }
}

impl fmt::Debug for SinkOnlyToolPayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SinkOnlyToolPayload")
            .field("tool_call_id", &self.tool_call_id)
            .field("source", &self.source)
            .field("input", &self.input)
            .field("output", &self.output)
            .field("act_budget_applied", &self.act_budget_applied)
            .finish()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentToolPayloadActBudget {
    accepted_bytes: usize,
    truncated: bool,
}

impl AgentToolPayloadActBudget {
    pub const fn new() -> Self {
        Self {
            accepted_bytes: 0,
            truncated: false,
        }
    }

    pub const fn accepted_bytes(&self) -> usize {
        self.accepted_bytes
    }

    pub const fn remaining_bytes(&self) -> usize {
        ACT_TOOL_PAYLOAD_MAX_BYTES - self.accepted_bytes
    }

    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    fn admit(&mut self, bytes: usize) -> bool {
        if bytes <= self.remaining_bytes() {
            self.accepted_bytes += bytes;
            true
        } else {
            self.truncated = true;
            false
        }
    }

    fn note_omission(&mut self, _reason: ToolPayloadOmissionReason) {
        self.truncated = true;
    }
}

#[derive(Clone)]
pub struct AgentToolPayloadCandidate {
    turn: Arc<AgentTurnDiagnosticMetadata>,
    payload: SinkOnlyToolPayload,
}

impl AgentToolPayloadCandidate {
    pub fn turn(&self) -> &AgentTurnDiagnosticMetadata {
        &self.turn
    }

    pub const fn payload(&self) -> &SinkOnlyToolPayload {
        &self.payload
    }

    pub const fn is_sink_only(&self) -> bool {
        true
    }
}

impl fmt::Debug for AgentToolPayloadCandidate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentToolPayloadCandidate")
            .field("turn", &self.turn)
            .field("payload", &self.payload)
            .field("routing", &"act_sink_sidecar")
            .finish()
    }
}

impl AgentDiagnosticCandidate for AgentToolPayloadCandidate {
    fn kind(&self) -> &'static str {
        AGENT_TOOL_PAYLOAD_CANDIDATE_KIND
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Clone, Copy)]
struct SelectedOutput<'a> {
    raw_output: Option<&'a Value>,
    content: Option<&'a [ToolCallContent]>,
    locations: Option<&'a [ToolCallLocation]>,
}

struct SelectedToolFields<'a> {
    tool_call_id: &'a Arc<str>,
    source: ToolPayloadSource,
    raw_input: Option<&'a Value>,
    output: Option<SelectedOutput<'a>>,
}

impl<'a> SelectedToolFields<'a> {
    fn from_update(update: &'a SessionUpdate) -> Option<Self> {
        match update {
            SessionUpdate::ToolCall(call) => {
                let content = (!call.content.is_empty()).then_some(call.content.as_slice());
                let locations = (!call.locations.is_empty()).then_some(call.locations.as_slice());
                let output =
                    (call.raw_output.is_some() || content.is_some() || locations.is_some())
                        .then_some(SelectedOutput {
                            raw_output: call.raw_output.as_ref(),
                            content,
                            locations,
                        });
                Some(Self {
                    tool_call_id: &call.tool_call_id.0,
                    source: ToolPayloadSource::Started,
                    raw_input: call.raw_input.as_ref(),
                    output,
                })
            }
            SessionUpdate::ToolCallUpdate(update) => {
                let fields = &update.fields;
                let content = fields.content.as_deref();
                let locations = fields.locations.as_deref();
                let output =
                    (fields.raw_output.is_some() || content.is_some() || locations.is_some())
                        .then_some(SelectedOutput {
                            raw_output: fields.raw_output.as_ref(),
                            content,
                            locations,
                        });
                Some(Self {
                    tool_call_id: &update.tool_call_id.0,
                    source: ToolPayloadSource::Updated,
                    raw_input: fields.raw_input.as_ref(),
                    output,
                })
            }
            _ => None,
        }
    }
}

fn capture_input(value: &Value) -> SinkOnlyToolInput {
    match checked_snapshot_size(value) {
        Ok(canonical_bytes) => {
            SinkOnlyToolInput::captured(canonicalize_json(value), canonical_bytes)
        }
        Err(reason) => SinkOnlyToolInput::omitted(reason),
    }
}

fn capture_output(selected: SelectedOutput<'_>) -> SinkOnlyToolOutput {
    let (raw_output, raw_output_bytes) = match selected.raw_output {
        Some(raw_output) => match checked_component_size(raw_output, 2, 1) {
            Ok(bytes) => (Some(canonicalize_json(raw_output)), bytes),
            Err(reason) => return SinkOnlyToolOutput::omitted(reason),
        },
        None => (None, 0),
    };
    let mut retention = RetentionBudget::with_bytes(raw_output_bytes);
    let content = match selected.content {
        Some(content) => match normalize_content(content, &mut retention) {
            Ok(content) => content,
            Err(reason) => return SinkOnlyToolOutput::omitted(reason),
        },
        None => Vec::new(),
    };
    let locations = match selected.locations {
        Some(locations) => match normalize_locations(locations, &mut retention) {
            Ok(locations) => locations,
            Err(reason) => return SinkOnlyToolOutput::omitted(reason),
        },
        None => Vec::new(),
    };

    let mut snapshot = Map::new();
    if selected.content.is_some() {
        snapshot.insert("content".to_owned(), Value::Array(content.clone()));
    }
    if selected.locations.is_some() {
        snapshot.insert(
            "locations".to_owned(),
            Value::Array(locations.iter().map(location_json).collect()),
        );
    }
    if let Some(raw_output) = raw_output.as_ref() {
        snapshot.insert("raw_output".to_owned(), raw_output.clone());
    }

    match checked_snapshot_size(&Value::Object(snapshot)) {
        Ok(canonical_bytes) => {
            SinkOnlyToolOutput::captured(raw_output, content, locations, canonical_bytes)
        }
        Err(reason) => SinkOnlyToolOutput::omitted(reason),
    }
}

fn normalize_content(
    content: &[ToolCallContent],
    retention: &mut RetentionBudget,
) -> Result<Vec<Value>, ToolPayloadOmissionReason> {
    content
        .iter()
        .map(|item| tool_content_json(item, retention))
        .collect()
}

fn normalize_locations(
    locations: &[ToolCallLocation],
    retention: &mut RetentionBudget,
) -> Result<Vec<SinkOnlyToolLocation>, ToolPayloadOmissionReason> {
    locations
        .iter()
        .map(|location| {
            let Some(path) = location.path.to_str() else {
                return Err(ToolPayloadOmissionReason::InvalidType);
            };
            Ok(SinkOnlyToolLocation {
                path: Arc::from(retention.string(path)?),
                line: location.line,
            })
        })
        .collect()
}

fn tool_content_json(
    content: &ToolCallContent,
    retention: &mut RetentionBudget,
) -> Result<Value, ToolPayloadOmissionReason> {
    let mut value = Map::new();
    match content {
        ToolCallContent::Content(content) => {
            value.insert(
                "content".to_owned(),
                content_block_json(&content.content, retention)?,
            );
            value.insert("type".to_owned(), Value::String("content".to_owned()));
        }
        ToolCallContent::Diff(diff) => {
            let Some(path) = diff.path.to_str() else {
                return Err(ToolPayloadOmissionReason::InvalidType);
            };
            value.insert(
                "newText".to_owned(),
                Value::String(retention.string(&diff.new_text)?),
            );
            if let Some(old_text) = diff.old_text.as_deref() {
                value.insert(
                    "oldText".to_owned(),
                    Value::String(retention.string(old_text)?),
                );
            }
            value.insert("path".to_owned(), Value::String(retention.string(path)?));
            value.insert("type".to_owned(), Value::String("diff".to_owned()));
        }
        ToolCallContent::Terminal(terminal) => {
            value.insert(
                "terminalId".to_owned(),
                Value::String(retention.string(&terminal.terminal_id.0)?),
            );
            value.insert("type".to_owned(), Value::String("terminal".to_owned()));
        }
        _ => return Err(ToolPayloadOmissionReason::InvalidType),
    }
    Ok(Value::Object(value))
}

fn content_block_json(
    content: &ContentBlock,
    retention: &mut RetentionBudget,
) -> Result<Value, ToolPayloadOmissionReason> {
    let mut value = Map::new();
    match content {
        ContentBlock::Text(content) => {
            insert_annotations(&mut value, content.annotations.as_ref(), retention)?;
            value.insert(
                "text".to_owned(),
                Value::String(retention.string(&content.text)?),
            );
            value.insert("type".to_owned(), Value::String("text".to_owned()));
        }
        ContentBlock::Image(content) => {
            insert_annotations(&mut value, content.annotations.as_ref(), retention)?;
            value.insert(
                "data".to_owned(),
                Value::String(retention.string(&content.data)?),
            );
            value.insert(
                "mimeType".to_owned(),
                Value::String(retention.string(&content.mime_type)?),
            );
            if let Some(uri) = content.uri.as_deref() {
                value.insert("uri".to_owned(), Value::String(retention.string(uri)?));
            }
            value.insert("type".to_owned(), Value::String("image".to_owned()));
        }
        ContentBlock::Audio(content) => {
            insert_annotations(&mut value, content.annotations.as_ref(), retention)?;
            value.insert(
                "data".to_owned(),
                Value::String(retention.string(&content.data)?),
            );
            value.insert(
                "mimeType".to_owned(),
                Value::String(retention.string(&content.mime_type)?),
            );
            value.insert("type".to_owned(), Value::String("audio".to_owned()));
        }
        ContentBlock::ResourceLink(content) => {
            insert_annotations(&mut value, content.annotations.as_ref(), retention)?;
            insert_optional_string(
                &mut value,
                "description",
                content.description.as_deref(),
                retention,
            )?;
            insert_optional_string(
                &mut value,
                "mimeType",
                content.mime_type.as_deref(),
                retention,
            )?;
            value.insert(
                "name".to_owned(),
                Value::String(retention.string(&content.name)?),
            );
            if let Some(size) = content.size {
                value.insert("size".to_owned(), Value::Number(size.into()));
            }
            insert_optional_string(&mut value, "title", content.title.as_deref(), retention)?;
            value.insert(
                "uri".to_owned(),
                Value::String(retention.string(&content.uri)?),
            );
            value.insert("type".to_owned(), Value::String("resource_link".to_owned()));
        }
        ContentBlock::Resource(content) => {
            insert_annotations(&mut value, content.annotations.as_ref(), retention)?;
            value.insert(
                "resource".to_owned(),
                embedded_resource_json(&content.resource, retention)?,
            );
            value.insert("type".to_owned(), Value::String("resource".to_owned()));
        }
        _ => return Err(ToolPayloadOmissionReason::InvalidType),
    }
    Ok(Value::Object(value))
}

fn embedded_resource_json(
    resource: &EmbeddedResourceResource,
    retention: &mut RetentionBudget,
) -> Result<Value, ToolPayloadOmissionReason> {
    let mut value = Map::new();
    match resource {
        EmbeddedResourceResource::TextResourceContents(resource) => {
            insert_optional_string(
                &mut value,
                "mimeType",
                resource.mime_type.as_deref(),
                retention,
            )?;
            value.insert(
                "text".to_owned(),
                Value::String(retention.string(&resource.text)?),
            );
            value.insert(
                "uri".to_owned(),
                Value::String(retention.string(&resource.uri)?),
            );
        }
        EmbeddedResourceResource::BlobResourceContents(resource) => {
            value.insert(
                "blob".to_owned(),
                Value::String(retention.string(&resource.blob)?),
            );
            insert_optional_string(
                &mut value,
                "mimeType",
                resource.mime_type.as_deref(),
                retention,
            )?;
            value.insert(
                "uri".to_owned(),
                Value::String(retention.string(&resource.uri)?),
            );
        }
        _ => return Err(ToolPayloadOmissionReason::InvalidType),
    }
    Ok(Value::Object(value))
}

fn insert_annotations(
    target: &mut Map<String, Value>,
    annotations: Option<&Annotations>,
    retention: &mut RetentionBudget,
) -> Result<(), ToolPayloadOmissionReason> {
    let Some(annotations) = annotations else {
        return Ok(());
    };
    let mut value = Map::new();
    if let Some(audience) = annotations.audience.as_ref() {
        let audience = audience
            .iter()
            .map(|role| match role {
                Role::Assistant => Ok(Value::String("assistant".to_owned())),
                Role::User => Ok(Value::String("user".to_owned())),
                _ => Err(ToolPayloadOmissionReason::InvalidType),
            })
            .collect::<Result<Vec<_>, _>>()?;
        value.insert("audience".to_owned(), Value::Array(audience));
    }
    insert_optional_string(
        &mut value,
        "lastModified",
        annotations.last_modified.as_deref(),
        retention,
    )?;
    if let Some(priority) = annotations.priority {
        let Some(priority) = serde_json::Number::from_f64(priority) else {
            return Err(ToolPayloadOmissionReason::InvalidType);
        };
        value.insert("priority".to_owned(), Value::Number(priority));
    }
    target.insert("annotations".to_owned(), Value::Object(value));
    Ok(())
}

fn insert_optional_string(
    target: &mut Map<String, Value>,
    key: &'static str,
    value: Option<&str>,
    retention: &mut RetentionBudget,
) -> Result<(), ToolPayloadOmissionReason> {
    if let Some(value) = value {
        target.insert(key.to_owned(), Value::String(retention.string(value)?));
    }
    Ok(())
}

fn location_json(location: &SinkOnlyToolLocation) -> Value {
    let mut value = Map::new();
    if let Some(line) = location.line {
        value.insert("line".to_owned(), Value::Number(line.into()));
    }
    value.insert("path".to_owned(), Value::String(location.path.to_string()));
    Value::Object(value)
}

fn canonicalize_json(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonicalize_json).collect()),
        Value::Object(values) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            let mut canonical = Map::new();
            for key in keys {
                canonical.insert(key.clone(), canonicalize_json(&values[key]));
            }
            Value::Object(canonical)
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.clone(),
    }
}

fn checked_snapshot_size(value: &Value) -> Result<usize, ToolPayloadOmissionReason> {
    checked_component_size(value, 1, 0)
}

fn checked_component_size(
    value: &Value,
    depth: usize,
    initial_nodes: usize,
) -> Result<usize, ToolPayloadOmissionReason> {
    let mut nodes = initial_nodes;
    check_json_resources(value, depth, &mut nodes)?;

    let mut counter = CappedByteCounter::new(TOOL_PAYLOAD_SNAPSHOT_MAX_BYTES);
    match serde_json::to_writer(&mut counter, value) {
        Ok(()) => Ok(counter.bytes),
        Err(_) if counter.exceeded => Err(ToolPayloadOmissionReason::SnapshotByteLimit),
        Err(_) => Err(ToolPayloadOmissionReason::InvalidType),
    }
}

struct RetentionBudget {
    bytes: usize,
}

impl RetentionBudget {
    const fn with_bytes(bytes: usize) -> Self {
        Self { bytes }
    }

    fn string(&mut self, value: &str) -> Result<String, ToolPayloadOmissionReason> {
        let Some(total) = self.bytes.checked_add(value.len()) else {
            return Err(ToolPayloadOmissionReason::SnapshotByteLimit);
        };
        if total > TOOL_PAYLOAD_SNAPSHOT_MAX_BYTES {
            return Err(ToolPayloadOmissionReason::SnapshotByteLimit);
        }
        self.bytes = total;
        Ok(value.to_owned())
    }
}

fn check_json_resources(
    value: &Value,
    depth: usize,
    nodes: &mut usize,
) -> Result<(), ToolPayloadOmissionReason> {
    if depth > TOOL_PAYLOAD_MAX_DEPTH {
        return Err(ToolPayloadOmissionReason::DepthLimit);
    }
    *nodes += 1;
    if *nodes > TOOL_PAYLOAD_MAX_NODES {
        return Err(ToolPayloadOmissionReason::NodeLimit);
    }

    match value {
        Value::Array(values) => {
            for value in values {
                check_json_resources(value, depth + 1, nodes)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                check_json_resources(value, depth + 1, nodes)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
    Ok(())
}

struct CappedByteCounter {
    bytes: usize,
    limit: usize,
    exceeded: bool,
}

impl CappedByteCounter {
    const fn new(limit: usize) -> Self {
        Self {
            bytes: 0,
            limit,
            exceeded: false,
        }
    }
}

impl Write for CappedByteCounter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let Some(total) = self.bytes.checked_add(buffer.len()) else {
            self.exceeded = true;
            return Err(io::Error::other(
                "tool payload snapshot byte limit exceeded",
            ));
        };
        if total > self.limit {
            self.exceeded = true;
            return Err(io::Error::other(
                "tool payload snapshot byte limit exceeded",
            ));
        }
        self.bytes = total;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[inline]
pub(crate) fn observe_update(context: &AgentDiagnosticUpdateContext<'_>, update: &SessionUpdate) {
    let Some(turn) = context.turn else {
        return;
    };
    let policy = turn.tool_payload_capture();
    if !policy.captures_payload() {
        return;
    }
    let Some(metadata) = turn.runtime_metadata().cloned() else {
        return;
    };
    let Some(payload) = SinkOnlyToolPayload::from_acp(update, policy) else {
        return;
    };

    context
        .observer
        .observe(AgentDiagnosticObservation::Candidate(Arc::new(
            AgentToolPayloadCandidate {
                turn: Arc::new(metadata),
                payload,
            },
        )));
}
