use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::{
    id::RunLocalId,
    kinds::{
        ComponentFailureErrorCode, ComponentFailureStage, DiagnosticComponent, InstantKind,
        PlanEntryPriority, PlanEntryStatus, SpanKind, ToolCallStatus, ToolKind,
    },
    scalar::{DecimalString, SchemaU64},
    wire::{WireValueError, deserialize_string},
};

pub const MAX_CUSTOM_NAME_BYTES: usize = 128;
pub const MAX_CUSTOM_KEY_BYTES: usize = 64;
pub const MAX_CUSTOM_UNIT_BYTES: usize = 32;
pub const MAX_CUSTOM_ATTRIBUTES: usize = 32;
pub const MAX_CUSTOM_DIMENSIONS: usize = 8;
pub const MAX_CUSTOM_LIST_ITEMS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DetailValidationError(&'static str);

impl DetailValidationError {
    const fn new(message: &'static str) -> Self {
        Self(message)
    }

    pub const fn message(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for DetailValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for DetailValidationError {}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyDetail {}

impl EmptyDetail {
    pub const fn new() -> Self {
        Self {}
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProductionPathResolutionDetail {
    pub(crate) production_root: String,
    pub(crate) package: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProductionLoadDetail {
    pub(crate) package: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProductionConstructDetail {
    pub(crate) package: String,
    pub(crate) class_name: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActorDetail {
    pub(crate) display_name: String,
    pub(crate) actor_type: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EffectDetail {
    pub(crate) effect_type: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSessionDetail {
    pub(crate) provider: String,
    pub(crate) effective_model: Option<String>,
    pub(crate) effective_effort: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSessionBrokenDetail {
    pub(crate) provider: String,
    pub(crate) effective_model: Option<String>,
    pub(crate) effective_effort: Option<String>,
    pub(crate) error_code: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTurnTerminalDetail {
    pub(crate) error_code: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallDetail {
    pub(crate) title: String,
    pub(crate) tool_kind: ToolKind,
    pub(crate) status: ToolCallStatus,
    pub(crate) error_code: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResultIssue {
    pub(crate) code: String,
    pub(crate) path: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResultTransitionDetail {
    pub(crate) issue: Option<ResultIssue>,
    pub(crate) error_code: Option<String>,
}

impl ProductionPathResolutionDetail {
    pub fn new(production_root: String, package: String) -> Self {
        Self {
            production_root,
            package,
        }
    }

    pub fn production_root(&self) -> &str {
        &self.production_root
    }

    pub fn package(&self) -> &str {
        &self.package
    }
}

impl ProductionLoadDetail {
    pub fn new(package: String) -> Self {
        Self { package }
    }

    pub fn package(&self) -> &str {
        &self.package
    }
}

impl ProductionConstructDetail {
    pub fn new(package: String, class_name: String) -> Self {
        Self {
            package,
            class_name,
        }
    }

    pub fn package(&self) -> &str {
        &self.package
    }

    pub fn class_name(&self) -> &str {
        &self.class_name
    }
}

impl ActorDetail {
    pub fn new(display_name: String, actor_type: String) -> Self {
        Self {
            display_name,
            actor_type,
        }
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn actor_type(&self) -> &str {
        &self.actor_type
    }
}

impl EffectDetail {
    pub fn new(effect_type: String) -> Self {
        Self { effect_type }
    }

    pub fn effect_type(&self) -> &str {
        &self.effect_type
    }
}

impl AgentSessionDetail {
    pub fn new(
        provider: String,
        effective_model: Option<String>,
        effective_effort: Option<String>,
    ) -> Self {
        Self {
            provider,
            effective_model,
            effective_effort,
        }
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn effective_model(&self) -> Option<&str> {
        self.effective_model.as_deref()
    }

    pub fn effective_effort(&self) -> Option<&str> {
        self.effective_effort.as_deref()
    }
}

impl AgentSessionBrokenDetail {
    pub fn new(
        provider: String,
        effective_model: Option<String>,
        effective_effort: Option<String>,
        error_code: String,
    ) -> Self {
        Self {
            provider,
            effective_model,
            effective_effort,
            error_code,
        }
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn effective_model(&self) -> Option<&str> {
        self.effective_model.as_deref()
    }

    pub fn effective_effort(&self) -> Option<&str> {
        self.effective_effort.as_deref()
    }

    pub fn error_code(&self) -> &str {
        &self.error_code
    }
}

impl AgentTurnTerminalDetail {
    pub fn new(error_code: Option<String>) -> Self {
        Self { error_code }
    }

    pub fn error_code(&self) -> Option<&str> {
        self.error_code.as_deref()
    }
}

impl ToolCallDetail {
    pub fn new(
        title: String,
        tool_kind: ToolKind,
        status: ToolCallStatus,
        error_code: Option<String>,
    ) -> Self {
        Self {
            title,
            tool_kind,
            status,
            error_code,
        }
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

    pub fn error_code(&self) -> Option<&str> {
        self.error_code.as_deref()
    }
}

impl ResultIssue {
    pub fn new(code: String, path: String) -> Self {
        Self { code, path }
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn path(&self) -> &str {
        &self.path
    }
}

impl ResultTransitionDetail {
    pub fn new(issue: Option<ResultIssue>, error_code: Option<String>) -> Self {
        Self { issue, error_code }
    }

    pub const fn issue(&self) -> Option<&ResultIssue> {
        self.issue.as_ref()
    }

    pub fn error_code(&self) -> Option<&str> {
        self.error_code.as_deref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DiagnosticComponentFailedDetail {
    pub(crate) component: DiagnosticComponent,
    pub(crate) component_id: RunLocalId,
    pub(crate) stage: ComponentFailureStage,
    pub(crate) error_code: ComponentFailureErrorCode,
    pub(crate) related_event_sequence: Option<SchemaU64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiagnosticComponentFailedDetailWire {
    component: DiagnosticComponent,
    component_id: RunLocalId,
    stage: ComponentFailureStage,
    error_code: ComponentFailureErrorCode,
    related_event_sequence: Option<SchemaU64>,
}

impl DiagnosticComponentFailedDetail {
    pub fn new(
        component_id: RunLocalId,
        stage: ComponentFailureStage,
        error_code: ComponentFailureErrorCode,
        related_event_sequence: Option<SchemaU64>,
    ) -> Result<Self, DetailValidationError> {
        let valid_pair = matches!(
            (stage, error_code),
            (
                ComponentFailureStage::Enqueue,
                ComponentFailureErrorCode::DeliveryQueueUnavailable
            ) | (
                ComponentFailureStage::Callback,
                ComponentFailureErrorCode::CallbackRaised
                    | ComponentFailureErrorCode::CallbackInvalidReturn
            )
        );
        if !valid_pair {
            return Err(DetailValidationError::new(
                "component failure stage and error code do not match",
            ));
        }
        Ok(Self {
            component: DiagnosticComponent::Sink,
            component_id,
            stage,
            error_code,
            related_event_sequence,
        })
    }

    pub const fn component(&self) -> DiagnosticComponent {
        self.component
    }

    pub const fn component_id(&self) -> &RunLocalId {
        &self.component_id
    }

    pub const fn stage(&self) -> ComponentFailureStage {
        self.stage
    }

    pub const fn error_code(&self) -> ComponentFailureErrorCode {
        self.error_code
    }

    pub const fn related_event_sequence(&self) -> Option<SchemaU64> {
        self.related_event_sequence
    }
}

impl TryFrom<DiagnosticComponentFailedDetailWire> for DiagnosticComponentFailedDetail {
    type Error = DetailValidationError;

    fn try_from(wire: DiagnosticComponentFailedDetailWire) -> Result<Self, Self::Error> {
        if wire.component != DiagnosticComponent::Sink {
            return Err(DetailValidationError::new(
                "diagnostic component must be sink",
            ));
        }
        Self::new(
            wire.component_id,
            wire.stage,
            wire.error_code,
            wire.related_event_sequence,
        )
    }
}

impl<'de> Deserialize<'de> for DiagnosticComponentFailedDetail {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        DiagnosticComponentFailedDetailWire::deserialize(deserializer)?
            .try_into()
            .map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "span_kind", content = "detail")]
pub enum SpanStartDetail {
    #[serde(rename = "run.lifecycle")]
    RunLifecycle(EmptyDetail),
    #[serde(rename = "production.path_resolution")]
    ProductionPathResolution(ProductionPathResolutionDetail),
    #[serde(rename = "production.load")]
    ProductionLoad(ProductionLoadDetail),
    #[serde(rename = "production.construct")]
    ProductionConstruct(ProductionConstructDetail),
    #[serde(rename = "production.start")]
    ProductionStart(EmptyDetail),
    #[serde(rename = "production.stop")]
    ProductionStop(EmptyDetail),
    #[serde(rename = "production.shutdown")]
    ProductionShutdown(EmptyDetail),
    #[serde(rename = "scene.lifecycle")]
    SceneLifecycle(EmptyDetail),
    #[serde(rename = "scene.drain")]
    SceneDrain(EmptyDetail),
    #[serde(rename = "scene.cleanup")]
    SceneCleanup(EmptyDetail),
    #[serde(rename = "actor.handle_lifetime")]
    ActorHandleLifetime(ActorDetail),
    #[serde(rename = "cue.mailbox_wait")]
    CueMailboxWait(EmptyDetail),
    #[serde(rename = "cue.execution")]
    CueExecution(EmptyDetail),
    #[serde(rename = "effect.lifecycle")]
    EffectLifecycle(EffectDetail),
    #[serde(rename = "agent.session.opening")]
    AgentSessionOpening(AgentSessionDetail),
    #[serde(rename = "agent.session.lifecycle")]
    AgentSessionLifecycle(AgentSessionDetail),
    #[serde(rename = "agent.session.closing")]
    AgentSessionClosing(AgentSessionDetail),
    #[serde(rename = "act.lifecycle")]
    ActLifecycle(AgentSessionDetail),
    #[serde(rename = "act.caller")]
    ActCaller(EmptyDetail),
    #[serde(rename = "agent.turn")]
    AgentTurn(AgentSessionDetail),
    #[serde(rename = "agent.thinking")]
    AgentThinking(EmptyDetail),
    #[serde(rename = "tool.call")]
    ToolCall(ToolCallDetail),
}

impl SpanStartDetail {
    pub const fn kind(&self) -> SpanKind {
        match self {
            Self::RunLifecycle(_) => SpanKind::RunLifecycle,
            Self::ProductionPathResolution(_) => SpanKind::ProductionPathResolution,
            Self::ProductionLoad(_) => SpanKind::ProductionLoad,
            Self::ProductionConstruct(_) => SpanKind::ProductionConstruct,
            Self::ProductionStart(_) => SpanKind::ProductionStart,
            Self::ProductionStop(_) => SpanKind::ProductionStop,
            Self::ProductionShutdown(_) => SpanKind::ProductionShutdown,
            Self::SceneLifecycle(_) => SpanKind::SceneLifecycle,
            Self::SceneDrain(_) => SpanKind::SceneDrain,
            Self::SceneCleanup(_) => SpanKind::SceneCleanup,
            Self::ActorHandleLifetime(_) => SpanKind::ActorHandleLifetime,
            Self::CueMailboxWait(_) => SpanKind::CueMailboxWait,
            Self::CueExecution(_) => SpanKind::CueExecution,
            Self::EffectLifecycle(_) => SpanKind::EffectLifecycle,
            Self::AgentSessionOpening(_) => SpanKind::AgentSessionOpening,
            Self::AgentSessionLifecycle(_) => SpanKind::AgentSessionLifecycle,
            Self::AgentSessionClosing(_) => SpanKind::AgentSessionClosing,
            Self::ActLifecycle(_) => SpanKind::ActLifecycle,
            Self::ActCaller(_) => SpanKind::ActCaller,
            Self::AgentTurn(_) => SpanKind::AgentTurn,
            Self::AgentThinking(_) => SpanKind::AgentThinking,
            Self::ToolCall(_) => SpanKind::ToolCall,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "instant_kind", content = "detail")]
pub enum InstantDetail {
    #[serde(rename = "actor.cast")]
    ActorCast(ActorDetail),
    #[serde(rename = "cue.admitted")]
    CueAdmitted(EmptyDetail),
    #[serde(rename = "cue.enqueued")]
    CueEnqueued(EmptyDetail),
    #[serde(rename = "cue.dispatched")]
    CueDispatched(EmptyDetail),
    #[serde(rename = "cue.cancel_requested")]
    CueCancelRequested(EmptyDetail),
    #[serde(rename = "effect.created")]
    EffectCreated(EffectDetail),
    #[serde(rename = "effect.returned")]
    EffectReturned(EffectDetail),
    #[serde(rename = "effect.consumed")]
    EffectConsumed(EffectDetail),
    #[serde(rename = "agent.session.ready")]
    AgentSessionReady(AgentSessionDetail),
    #[serde(rename = "agent.session.broken")]
    AgentSessionBroken(AgentSessionBrokenDetail),
    #[serde(rename = "act.admitted")]
    ActAdmitted(EmptyDetail),
    #[serde(rename = "act.waiting_ready")]
    ActWaitingReady(EmptyDetail),
    #[serde(rename = "act.prompt_submitted")]
    ActPromptSubmitted(EmptyDetail),
    #[serde(rename = "act.cancel_requested")]
    ActCancelRequested(EmptyDetail),
    #[serde(rename = "act.supervisor_handoff")]
    ActSupervisorHandoff(EmptyDetail),
    #[serde(rename = "agent.turn.activity")]
    AgentTurnActivity(EmptyDetail),
    #[serde(rename = "agent.turn.terminal")]
    AgentTurnTerminal(AgentTurnTerminalDetail),
    #[serde(rename = "agent.turn.settled")]
    AgentTurnSettled(AgentTurnTerminalDetail),
    #[serde(rename = "tool.updated")]
    ToolUpdated(ToolCallDetail),
    #[serde(rename = "result.submitted")]
    ResultSubmitted(ResultTransitionDetail),
    #[serde(rename = "result.rejected")]
    ResultRejected(ResultTransitionDetail),
    #[serde(rename = "result.repair_requested")]
    ResultRepairRequested(ResultTransitionDetail),
    #[serde(rename = "result.accepted")]
    ResultAccepted(ResultTransitionDetail),
    #[serde(rename = "result.missing")]
    ResultMissing(ResultTransitionDetail),
    #[serde(rename = "diagnostic.component_failed")]
    DiagnosticComponentFailed(DiagnosticComponentFailedDetail),
}

impl InstantDetail {
    pub const fn kind(&self) -> InstantKind {
        match self {
            Self::ActorCast(_) => InstantKind::ActorCast,
            Self::CueAdmitted(_) => InstantKind::CueAdmitted,
            Self::CueEnqueued(_) => InstantKind::CueEnqueued,
            Self::CueDispatched(_) => InstantKind::CueDispatched,
            Self::CueCancelRequested(_) => InstantKind::CueCancelRequested,
            Self::EffectCreated(_) => InstantKind::EffectCreated,
            Self::EffectReturned(_) => InstantKind::EffectReturned,
            Self::EffectConsumed(_) => InstantKind::EffectConsumed,
            Self::AgentSessionReady(_) => InstantKind::AgentSessionReady,
            Self::AgentSessionBroken(_) => InstantKind::AgentSessionBroken,
            Self::ActAdmitted(_) => InstantKind::ActAdmitted,
            Self::ActWaitingReady(_) => InstantKind::ActWaitingReady,
            Self::ActPromptSubmitted(_) => InstantKind::ActPromptSubmitted,
            Self::ActCancelRequested(_) => InstantKind::ActCancelRequested,
            Self::ActSupervisorHandoff(_) => InstantKind::ActSupervisorHandoff,
            Self::AgentTurnActivity(_) => InstantKind::AgentTurnActivity,
            Self::AgentTurnTerminal(_) => InstantKind::AgentTurnTerminal,
            Self::AgentTurnSettled(_) => InstantKind::AgentTurnSettled,
            Self::ToolUpdated(_) => InstantKind::ToolUpdated,
            Self::ResultSubmitted(_) => InstantKind::ResultSubmitted,
            Self::ResultRejected(_) => InstantKind::ResultRejected,
            Self::ResultRepairRequested(_) => InstantKind::ResultRepairRequested,
            Self::ResultAccepted(_) => InstantKind::ResultAccepted,
            Self::ResultMissing(_) => InstantKind::ResultMissing,
            Self::DiagnosticComponentFailed(_) => InstantKind::DiagnosticComponentFailed,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanEntry {
    pub(crate) content: String,
    pub(crate) priority: PlanEntryPriority,
    pub(crate) status: PlanEntryStatus,
}

impl PlanEntry {
    pub fn new(content: String, priority: PlanEntryPriority, status: PlanEntryStatus) -> Self {
        Self {
            content,
            priority,
            status,
        }
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub const fn priority(&self) -> PlanEntryPriority {
        self.priority
    }

    pub const fn status(&self) -> PlanEntryStatus {
        self.status
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CanonicalInteger(String);

impl CanonicalInteger {
    pub fn parse(value: &str) -> Result<Self, WireValueError> {
        if value.is_empty() || !value.is_ascii() {
            return Err(WireValueError::new("integer must be nonempty ASCII"));
        }
        let (negative, digits) = match value.strip_prefix('-') {
            Some(digits) => (true, digits),
            None => (false, value),
        };
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(WireValueError::new(
                "integer must contain only an optional minus and decimal digits",
            ));
        }
        if (digits.len() > 1 && digits.starts_with('0')) || (negative && digits == "0") {
            return Err(WireValueError::new("integer wire value is not canonical"));
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for CanonicalInteger {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CanonicalInteger {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_string(deserializer, Self::parse)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum DiagnosticScalar {
    Null,
    Boolean(bool),
    Integer(CanonicalInteger),
    Decimal(DecimalString),
    String(String),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum DiagnosticAttributeValue {
    Null,
    Boolean(bool),
    Integer(CanonicalInteger),
    Decimal(DecimalString),
    String(String),
    List(Vec<DiagnosticScalar>),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum DiagnosticDimension {
    Boolean(bool),
    Integer(CanonicalInteger),
    Decimal(DecimalString),
    String(String),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum CustomNumber {
    Integer(CanonicalInteger),
    Decimal(DecimalString),
}

pub type DiagnosticAttributes = BTreeMap<String, DiagnosticAttributeValue>;
pub type DiagnosticDimensions = BTreeMap<String, DiagnosticDimension>;

pub(crate) fn validate_custom_name(value: &str) -> Result<(), DetailValidationError> {
    if value.is_empty() || value.len() > MAX_CUSTOM_NAME_BYTES || !value.is_ascii() {
        return Err(DetailValidationError::new("custom name is out of bounds"));
    }
    let mut segments = value.split('.');
    let Some(first) = segments.next() else {
        return Err(DetailValidationError::new("custom name is invalid"));
    };
    let mut count = 1;
    if !valid_identifier_segment(first) || first == "troupe" {
        return Err(DetailValidationError::new(
            "custom name is invalid or reserved",
        ));
    }
    for segment in segments {
        count += 1;
        if !valid_identifier_segment(segment) {
            return Err(DetailValidationError::new("custom name is invalid"));
        }
    }
    if count < 2 {
        return Err(DetailValidationError::new(
            "custom name requires at least two segments",
        ));
    }
    Ok(())
}

fn valid_identifier_segment(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn validate_key(value: &str) -> Result<(), DetailValidationError> {
    if value.is_empty() || value.len() > MAX_CUSTOM_KEY_BYTES {
        return Err(DetailValidationError::new("custom key is out of bounds"));
    }
    Ok(())
}

pub(crate) fn validate_attributes(
    values: &DiagnosticAttributes,
) -> Result<(), DetailValidationError> {
    if values.len() > MAX_CUSTOM_ATTRIBUTES {
        return Err(DetailValidationError::new("too many custom attributes"));
    }
    for (key, value) in values {
        validate_key(key)?;
        if matches!(value, DiagnosticAttributeValue::List(items) if items.len() > MAX_CUSTOM_LIST_ITEMS)
        {
            return Err(DetailValidationError::new("custom scalar list is too long"));
        }
    }
    Ok(())
}

pub(crate) fn validate_dimensions(
    values: &DiagnosticDimensions,
) -> Result<(), DetailValidationError> {
    if values.len() > MAX_CUSTOM_DIMENSIONS {
        return Err(DetailValidationError::new("too many custom dimensions"));
    }
    for key in values.keys() {
        validate_key(key)?;
    }
    Ok(())
}

pub(crate) fn validate_unit(value: Option<&str>) -> Result<(), DetailValidationError> {
    if value.is_some_and(|unit| unit.is_empty() || unit.len() > MAX_CUSTOM_UNIT_BYTES) {
        return Err(DetailValidationError::new(
            "custom counter unit is out of bounds",
        ));
    }
    Ok(())
}
