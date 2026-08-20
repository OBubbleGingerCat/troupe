use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::Value;

use crate::{
    detail::{
        CustomNumber, DetailValidationError, DiagnosticAttributes, DiagnosticDimensions,
        InstantDetail, PlanEntry, SpanStartDetail, validate_attributes, validate_custom_name,
        validate_dimensions, validate_unit,
    },
    id::{CanonicalUuid, RunLocalId},
    kinds::{
        CausalRelation, ContextSampleOrigin, CounterKind, CustomSeverity, SpanOutcome,
        UsageAvailability, UsageSource, UsageUnavailableReason,
    },
    scalar::{CurrencyCode, DecimalString, SchemaU64, TokenCount},
    time::ElapsedNs,
};

pub const EVENT_SCHEMA_VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum DiagnosticEventKind {
    #[serde(rename = "span_started")]
    SpanStarted,
    #[serde(rename = "span_finished")]
    SpanFinished,
    #[serde(rename = "instant_occurred")]
    InstantOccurred,
    #[serde(rename = "counter_sampled")]
    CounterSampled,
    #[serde(rename = "agent_message_delta")]
    AgentMessageDelta,
    #[serde(rename = "agent_message_completed")]
    AgentMessageCompleted,
    #[serde(rename = "agent_plan_snapshot")]
    AgentPlanSnapshot,
    #[serde(rename = "context_usage_sampled")]
    ContextUsageSampled,
    #[serde(rename = "act_token_usage_finalized")]
    ActTokenUsageFinalized,
    #[serde(rename = "observation_gap")]
    ObservationGap,
    #[serde(rename = "custom_span_started")]
    CustomSpanStarted,
    #[serde(rename = "custom_span_finished")]
    CustomSpanFinished,
    #[serde(rename = "custom_instant_occurred")]
    CustomInstantOccurred,
    #[serde(rename = "custom_counter_sampled")]
    CustomCounterSampled,
}

impl DiagnosticEventKind {
    pub const ALL: [Self; 14] = [
        Self::SpanStarted,
        Self::SpanFinished,
        Self::InstantOccurred,
        Self::CounterSampled,
        Self::AgentMessageDelta,
        Self::AgentMessageCompleted,
        Self::AgentPlanSnapshot,
        Self::ContextUsageSampled,
        Self::ActTokenUsageFinalized,
        Self::ObservationGap,
        Self::CustomSpanStarted,
        Self::CustomSpanFinished,
        Self::CustomInstantOccurred,
        Self::CustomCounterSampled,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SpanStarted => "span_started",
            Self::SpanFinished => "span_finished",
            Self::InstantOccurred => "instant_occurred",
            Self::CounterSampled => "counter_sampled",
            Self::AgentMessageDelta => "agent_message_delta",
            Self::AgentMessageCompleted => "agent_message_completed",
            Self::AgentPlanSnapshot => "agent_plan_snapshot",
            Self::ContextUsageSampled => "context_usage_sampled",
            Self::ActTokenUsageFinalized => "act_token_usage_finalized",
            Self::ObservationGap => "observation_gap",
            Self::CustomSpanStarted => "custom_span_started",
            Self::CustomSpanFinished => "custom_span_finished",
            Self::CustomInstantOccurred => "custom_instant_occurred",
            Self::CustomCounterSampled => "custom_counter_sampled",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DiagnosticEventHeader {
    pub(crate) schema_version: u8,
    pub(crate) run_id: CanonicalUuid,
    pub(crate) sequence: SchemaU64,
    pub(crate) elapsed_ns: ElapsedNs,
    pub(crate) scope: DiagnosticScope,
    pub(crate) caused_by: Vec<CausalLink>,
}

impl DiagnosticEventHeader {
    pub fn new(
        run_id: CanonicalUuid,
        sequence: SchemaU64,
        elapsed_ns: ElapsedNs,
        scope: DiagnosticScope,
        caused_by: Vec<CausalLink>,
    ) -> Result<Self, EventValidationError> {
        if sequence.get() == 0 {
            return Err(EventValidationError::new(
                "diagnostic event sequence must start at one",
            ));
        }
        Ok(Self {
            schema_version: EVENT_SCHEMA_VERSION,
            run_id,
            sequence,
            elapsed_ns,
            scope,
            caused_by,
        })
    }

    pub const fn schema_version(&self) -> u8 {
        self.schema_version
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub const fn sequence(&self) -> SchemaU64 {
        self.sequence
    }

    pub const fn elapsed_ns(&self) -> ElapsedNs {
        self.elapsed_ns
    }

    pub const fn scope(&self) -> &DiagnosticScope {
        &self.scope
    }

    pub fn caused_by(&self) -> &[CausalLink] {
        &self.caused_by
    }
}

#[derive(Deserialize)]
struct DiagnosticEventHeaderWire {
    schema_version: u8,
    run_id: CanonicalUuid,
    sequence: SchemaU64,
    elapsed_ns: ElapsedNs,
    scope: DiagnosticScope,
    caused_by: Vec<CausalLink>,
}

impl TryFrom<DiagnosticEventHeaderWire> for DiagnosticEventHeader {
    type Error = EventValidationError;

    fn try_from(wire: DiagnosticEventHeaderWire) -> Result<Self, Self::Error> {
        if wire.schema_version != EVENT_SCHEMA_VERSION {
            return Err(EventValidationError::new(
                "unsupported diagnostic event schema version",
            ));
        }
        Self::new(
            wire.run_id,
            wire.sequence,
            wire.elapsed_ns,
            wire.scope,
            wire.caused_by,
        )
    }
}

impl<'de> Deserialize<'de> for DiagnosticEventHeader {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        DiagnosticEventHeaderWire::deserialize(deserializer)?
            .try_into()
            .map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticScope {
    pub(crate) scene_id: Option<RunLocalId>,
    pub(crate) actor_id: Option<RunLocalId>,
    pub(crate) cue_id: Option<RunLocalId>,
    pub(crate) effect_id: Option<RunLocalId>,
    pub(crate) act_id: Option<RunLocalId>,
    pub(crate) tool_call_id: Option<RunLocalId>,
    pub(crate) session_generation: Option<SchemaU64>,
}

impl DiagnosticScope {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        scene_id: Option<RunLocalId>,
        actor_id: Option<RunLocalId>,
        cue_id: Option<RunLocalId>,
        effect_id: Option<RunLocalId>,
        act_id: Option<RunLocalId>,
        tool_call_id: Option<RunLocalId>,
        session_generation: Option<SchemaU64>,
    ) -> Self {
        Self {
            scene_id,
            actor_id,
            cue_id,
            effect_id,
            act_id,
            tool_call_id,
            session_generation,
        }
    }

    pub const fn scene_id(&self) -> Option<&RunLocalId> {
        self.scene_id.as_ref()
    }

    pub const fn actor_id(&self) -> Option<&RunLocalId> {
        self.actor_id.as_ref()
    }

    pub const fn cue_id(&self) -> Option<&RunLocalId> {
        self.cue_id.as_ref()
    }

    pub const fn effect_id(&self) -> Option<&RunLocalId> {
        self.effect_id.as_ref()
    }

    pub const fn act_id(&self) -> Option<&RunLocalId> {
        self.act_id.as_ref()
    }

    pub const fn tool_call_id(&self) -> Option<&RunLocalId> {
        self.tool_call_id.as_ref()
    }

    pub const fn session_generation(&self) -> Option<SchemaU64> {
        self.session_generation
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CausalLink {
    pub(crate) source_sequence: SchemaU64,
    pub(crate) relation: CausalRelation,
}

impl CausalLink {
    pub const fn new(source_sequence: SchemaU64, relation: CausalRelation) -> Self {
        Self {
            source_sequence,
            relation,
        }
    }

    pub const fn source_sequence(&self) -> SchemaU64 {
        self.source_sequence
    }

    pub const fn relation(&self) -> CausalRelation {
        self.relation
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EventValidationError(&'static str);

impl EventValidationError {
    const fn new(message: &'static str) -> Self {
        Self(message)
    }

    pub const fn message(self) -> &'static str {
        self.0
    }
}

impl From<DetailValidationError> for EventValidationError {
    fn from(_: DetailValidationError) -> Self {
        Self::new("custom diagnostic payload is invalid")
    }
}

impl fmt::Display for EventValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for EventValidationError {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpanStarted {
    #[serde(flatten)]
    pub(crate) header: DiagnosticEventHeader,
    #[serde(flatten)]
    pub(crate) detail: SpanStartDetail,
    pub(crate) parent_span_id: Option<SchemaU64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpanFinished {
    #[serde(flatten)]
    pub(crate) header: DiagnosticEventHeader,
    pub(crate) span_id: SchemaU64,
    pub(crate) outcome: SpanOutcome,
    pub(crate) error_code: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InstantOccurred {
    #[serde(flatten)]
    pub(crate) header: DiagnosticEventHeader,
    #[serde(flatten)]
    pub(crate) detail: InstantDetail,
    pub(crate) containing_span_id: Option<SchemaU64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CounterSampled {
    #[serde(flatten)]
    pub(crate) header: DiagnosticEventHeader,
    pub(crate) counter_kind: CounterKind,
    pub(crate) value: SchemaU64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentMessageDelta {
    #[serde(flatten)]
    pub(crate) header: DiagnosticEventHeader,
    pub(crate) message_id: RunLocalId,
    pub(crate) source_message_id: Option<String>,
    pub(crate) text_delta: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentMessageCompleted {
    #[serde(flatten)]
    pub(crate) header: DiagnosticEventHeader,
    pub(crate) message_id: RunLocalId,
    pub(crate) utf8_bytes: SchemaU64,
    pub(crate) unicode_scalar_count: SchemaU64,
    pub(crate) truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentPlanSnapshot {
    #[serde(flatten)]
    pub(crate) header: DiagnosticEventHeader,
    pub(crate) entries: Vec<PlanEntry>,
    pub(crate) truncated: bool,
}

impl SpanStarted {
    pub fn new(
        header: DiagnosticEventHeader,
        detail: SpanStartDetail,
        parent_span_id: Option<SchemaU64>,
    ) -> Self {
        Self {
            header,
            detail,
            parent_span_id,
        }
    }

    pub const fn header(&self) -> &DiagnosticEventHeader {
        &self.header
    }

    pub const fn span_kind(&self) -> crate::kinds::SpanKind {
        self.detail.kind()
    }

    pub const fn detail(&self) -> &SpanStartDetail {
        &self.detail
    }

    pub const fn parent_span_id(&self) -> Option<SchemaU64> {
        self.parent_span_id
    }
}

impl SpanFinished {
    pub fn new(
        header: DiagnosticEventHeader,
        span_id: SchemaU64,
        outcome: SpanOutcome,
        error_code: Option<String>,
    ) -> Self {
        Self {
            header,
            span_id,
            outcome,
            error_code,
        }
    }

    pub const fn header(&self) -> &DiagnosticEventHeader {
        &self.header
    }

    pub const fn span_id(&self) -> SchemaU64 {
        self.span_id
    }

    pub const fn outcome(&self) -> SpanOutcome {
        self.outcome
    }

    pub fn error_code(&self) -> Option<&str> {
        self.error_code.as_deref()
    }
}

impl InstantOccurred {
    pub fn new(
        header: DiagnosticEventHeader,
        detail: InstantDetail,
        containing_span_id: Option<SchemaU64>,
    ) -> Self {
        Self {
            header,
            detail,
            containing_span_id,
        }
    }

    pub const fn header(&self) -> &DiagnosticEventHeader {
        &self.header
    }

    pub const fn instant_kind(&self) -> crate::kinds::InstantKind {
        self.detail.kind()
    }

    pub const fn detail(&self) -> &InstantDetail {
        &self.detail
    }

    pub const fn containing_span_id(&self) -> Option<SchemaU64> {
        self.containing_span_id
    }
}

impl CounterSampled {
    pub fn new(header: DiagnosticEventHeader, counter_kind: CounterKind, value: SchemaU64) -> Self {
        Self {
            header,
            counter_kind,
            value,
        }
    }

    pub const fn header(&self) -> &DiagnosticEventHeader {
        &self.header
    }

    pub const fn counter_kind(&self) -> CounterKind {
        self.counter_kind
    }

    pub const fn value(&self) -> SchemaU64 {
        self.value
    }
}

impl AgentMessageDelta {
    pub fn new(
        header: DiagnosticEventHeader,
        message_id: RunLocalId,
        source_message_id: Option<String>,
        text_delta: String,
    ) -> Self {
        Self {
            header,
            message_id,
            source_message_id,
            text_delta,
        }
    }

    pub const fn header(&self) -> &DiagnosticEventHeader {
        &self.header
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

impl AgentMessageCompleted {
    pub fn new(
        header: DiagnosticEventHeader,
        message_id: RunLocalId,
        utf8_bytes: SchemaU64,
        unicode_scalar_count: SchemaU64,
        truncated: bool,
    ) -> Self {
        Self {
            header,
            message_id,
            utf8_bytes,
            unicode_scalar_count,
            truncated,
        }
    }

    pub const fn header(&self) -> &DiagnosticEventHeader {
        &self.header
    }

    pub const fn message_id(&self) -> &RunLocalId {
        &self.message_id
    }

    pub const fn utf8_bytes(&self) -> SchemaU64 {
        self.utf8_bytes
    }

    pub const fn unicode_scalar_count(&self) -> SchemaU64 {
        self.unicode_scalar_count
    }

    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

impl AgentPlanSnapshot {
    pub fn new(header: DiagnosticEventHeader, entries: Vec<PlanEntry>, truncated: bool) -> Self {
        Self {
            header,
            entries,
            truncated,
        }
    }

    pub const fn header(&self) -> &DiagnosticEventHeader {
        &self.header
    }

    pub fn entries(&self) -> &[PlanEntry] {
        &self.entries
    }

    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContextUsageSampled {
    #[serde(flatten)]
    pub(crate) header: DiagnosticEventHeader,
    pub(crate) context_used_tokens: Option<SchemaU64>,
    pub(crate) context_window_tokens: Option<SchemaU64>,
    pub(crate) cumulative_cost_amount: Option<DecimalString>,
    pub(crate) cumulative_cost_currency: Option<CurrencyCode>,
    pub(crate) sample_origin: ContextSampleOrigin,
    pub(crate) observed_elapsed_ns: Option<ElapsedNs>,
}

impl ContextUsageSampled {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        header: DiagnosticEventHeader,
        context_used_tokens: Option<SchemaU64>,
        context_window_tokens: Option<SchemaU64>,
        cumulative_cost_amount: Option<DecimalString>,
        cumulative_cost_currency: Option<CurrencyCode>,
        sample_origin: ContextSampleOrigin,
        observed_elapsed_ns: Option<ElapsedNs>,
    ) -> Result<Self, EventValidationError> {
        if cumulative_cost_amount.is_some() != cumulative_cost_currency.is_some() {
            return Err(EventValidationError::new(
                "cumulative cost amount and currency must appear together",
            ));
        }
        if cumulative_cost_amount
            .as_ref()
            .is_some_and(|amount| amount.as_str().starts_with('-'))
        {
            return Err(EventValidationError::new(
                "cumulative cost amount must be nonnegative",
            ));
        }
        if matches!(
            (context_used_tokens, context_window_tokens),
            (Some(used), Some(window)) if used.get() > window.get()
        ) {
            return Err(EventValidationError::new(
                "context used tokens exceed the context window",
            ));
        }
        if sample_origin == ContextSampleOrigin::CarriedForward && observed_elapsed_ns.is_none() {
            return Err(EventValidationError::new(
                "carried-forward context usage requires its observation time",
            ));
        }
        Ok(Self {
            header,
            context_used_tokens,
            context_window_tokens,
            cumulative_cost_amount,
            cumulative_cost_currency,
            sample_origin,
            observed_elapsed_ns,
        })
    }

    pub const fn header(&self) -> &DiagnosticEventHeader {
        &self.header
    }

    pub const fn context_used_tokens(&self) -> Option<SchemaU64> {
        self.context_used_tokens
    }

    pub const fn context_window_tokens(&self) -> Option<SchemaU64> {
        self.context_window_tokens
    }

    pub const fn cumulative_cost_amount(&self) -> Option<&DecimalString> {
        self.cumulative_cost_amount.as_ref()
    }

    pub const fn cumulative_cost_currency(&self) -> Option<&CurrencyCode> {
        self.cumulative_cost_currency.as_ref()
    }

    pub const fn sample_origin(&self) -> ContextSampleOrigin {
        self.sample_origin
    }

    pub const fn observed_elapsed_ns(&self) -> Option<ElapsedNs> {
        self.observed_elapsed_ns
    }
}

#[derive(Deserialize)]
struct ContextUsageSampledWire {
    #[serde(flatten)]
    header: DiagnosticEventHeader,
    context_used_tokens: Option<SchemaU64>,
    context_window_tokens: Option<SchemaU64>,
    cumulative_cost_amount: Option<DecimalString>,
    cumulative_cost_currency: Option<CurrencyCode>,
    sample_origin: ContextSampleOrigin,
    observed_elapsed_ns: Option<ElapsedNs>,
}

impl TryFrom<ContextUsageSampledWire> for ContextUsageSampled {
    type Error = EventValidationError;

    fn try_from(wire: ContextUsageSampledWire) -> Result<Self, Self::Error> {
        Self::new(
            wire.header,
            wire.context_used_tokens,
            wire.context_window_tokens,
            wire.cumulative_cost_amount,
            wire.cumulative_cost_currency,
            wire.sample_origin,
            wire.observed_elapsed_ns,
        )
    }
}

impl<'de> Deserialize<'de> for ContextUsageSampled {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        ContextUsageSampledWire::deserialize(deserializer)?
            .try_into()
            .map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ActTokenUsageFinalized {
    #[serde(flatten)]
    pub(crate) header: DiagnosticEventHeader,
    pub(crate) availability: UsageAvailability,
    pub(crate) source: Option<UsageSource>,
    pub(crate) unavailable_reason: Option<UsageUnavailableReason>,
    pub(crate) provider_total_tokens: Option<TokenCount>,
    pub(crate) input_tokens: Option<TokenCount>,
    pub(crate) output_tokens: Option<TokenCount>,
    pub(crate) thought_tokens: Option<TokenCount>,
    pub(crate) cached_read_tokens: Option<TokenCount>,
    pub(crate) cached_write_tokens: Option<TokenCount>,
}

impl ActTokenUsageFinalized {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        header: DiagnosticEventHeader,
        availability: UsageAvailability,
        source: Option<UsageSource>,
        unavailable_reason: Option<UsageUnavailableReason>,
        provider_total_tokens: Option<TokenCount>,
        input_tokens: Option<TokenCount>,
        output_tokens: Option<TokenCount>,
        thought_tokens: Option<TokenCount>,
        cached_read_tokens: Option<TokenCount>,
        cached_write_tokens: Option<TokenCount>,
    ) -> Result<Self, EventValidationError> {
        let primary_complete =
            provider_total_tokens.is_some() && input_tokens.is_some() && output_tokens.is_some();
        let any_value = provider_total_tokens.is_some()
            || input_tokens.is_some()
            || output_tokens.is_some()
            || thought_tokens.is_some()
            || cached_read_tokens.is_some()
            || cached_write_tokens.is_some();
        let state_is_valid = match availability {
            UsageAvailability::Available => {
                primary_complete
                    && source == Some(UsageSource::AcpPromptResponseUsage)
                    && unavailable_reason.is_none()
            }
            UsageAvailability::Partial => {
                any_value
                    && !primary_complete
                    && source == Some(UsageSource::AcpPromptResponseUsage)
                    && unavailable_reason.is_none()
            }
            UsageAvailability::Unavailable => {
                !any_value && source.is_none() && unavailable_reason.is_some()
            }
        };
        if !state_is_valid {
            return Err(EventValidationError::new(
                "terminal usage availability fields are inconsistent",
            ));
        }
        Ok(Self {
            header,
            availability,
            source,
            unavailable_reason,
            provider_total_tokens,
            input_tokens,
            output_tokens,
            thought_tokens,
            cached_read_tokens,
            cached_write_tokens,
        })
    }

    pub const fn header(&self) -> &DiagnosticEventHeader {
        &self.header
    }

    pub const fn availability(&self) -> UsageAvailability {
        self.availability
    }

    pub const fn source(&self) -> Option<UsageSource> {
        self.source
    }

    pub const fn unavailable_reason(&self) -> Option<UsageUnavailableReason> {
        self.unavailable_reason
    }

    pub const fn provider_total_tokens(&self) -> Option<&TokenCount> {
        self.provider_total_tokens.as_ref()
    }

    pub const fn input_tokens(&self) -> Option<&TokenCount> {
        self.input_tokens.as_ref()
    }

    pub const fn output_tokens(&self) -> Option<&TokenCount> {
        self.output_tokens.as_ref()
    }

    pub const fn thought_tokens(&self) -> Option<&TokenCount> {
        self.thought_tokens.as_ref()
    }

    pub const fn cached_read_tokens(&self) -> Option<&TokenCount> {
        self.cached_read_tokens.as_ref()
    }

    pub const fn cached_write_tokens(&self) -> Option<&TokenCount> {
        self.cached_write_tokens.as_ref()
    }
}

#[derive(Deserialize)]
struct ActTokenUsageFinalizedWire {
    #[serde(flatten)]
    header: DiagnosticEventHeader,
    availability: UsageAvailability,
    source: Option<UsageSource>,
    unavailable_reason: Option<UsageUnavailableReason>,
    provider_total_tokens: Option<TokenCount>,
    input_tokens: Option<TokenCount>,
    output_tokens: Option<TokenCount>,
    thought_tokens: Option<TokenCount>,
    cached_read_tokens: Option<TokenCount>,
    cached_write_tokens: Option<TokenCount>,
}

impl TryFrom<ActTokenUsageFinalizedWire> for ActTokenUsageFinalized {
    type Error = EventValidationError;

    fn try_from(wire: ActTokenUsageFinalizedWire) -> Result<Self, Self::Error> {
        Self::new(
            wire.header,
            wire.availability,
            wire.source,
            wire.unavailable_reason,
            wire.provider_total_tokens,
            wire.input_tokens,
            wire.output_tokens,
            wire.thought_tokens,
            wire.cached_read_tokens,
            wire.cached_write_tokens,
        )
    }
}

impl<'de> Deserialize<'de> for ActTokenUsageFinalized {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        ActTokenUsageFinalizedWire::deserialize(deserializer)?
            .try_into()
            .map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AffectedElapsedInterval {
    pub(crate) start_ns: ElapsedNs,
    pub(crate) end_ns: ElapsedNs,
}

impl AffectedElapsedInterval {
    pub const fn new(start_ns: ElapsedNs, end_ns: ElapsedNs) -> Self {
        Self { start_ns, end_ns }
    }

    pub const fn start_ns(&self) -> ElapsedNs {
        self.start_ns
    }

    pub const fn end_ns(&self) -> ElapsedNs {
        self.end_ns
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObservationGap {
    #[serde(flatten)]
    pub(crate) header: DiagnosticEventHeader,
    pub(crate) producer: String,
    pub(crate) component: Option<String>,
    pub(crate) reason: String,
    pub(crate) dropped_count: Option<SchemaU64>,
    pub(crate) affected_elapsed: Option<AffectedElapsedInterval>,
    pub(crate) affected_kind: Option<DiagnosticEventKind>,
    pub(crate) affected_scope: Option<DiagnosticScope>,
}

impl ObservationGap {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        header: DiagnosticEventHeader,
        producer: String,
        component: Option<String>,
        reason: String,
        dropped_count: Option<SchemaU64>,
        affected_elapsed: Option<AffectedElapsedInterval>,
        affected_kind: Option<DiagnosticEventKind>,
        affected_scope: Option<DiagnosticScope>,
    ) -> Self {
        Self {
            header,
            producer,
            component,
            reason,
            dropped_count,
            affected_elapsed,
            affected_kind,
            affected_scope,
        }
    }

    pub const fn header(&self) -> &DiagnosticEventHeader {
        &self.header
    }

    pub fn producer(&self) -> &str {
        &self.producer
    }

    pub fn component(&self) -> Option<&str> {
        self.component.as_deref()
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub const fn dropped_count(&self) -> Option<SchemaU64> {
        self.dropped_count
    }

    pub const fn affected_elapsed(&self) -> Option<&AffectedElapsedInterval> {
        self.affected_elapsed.as_ref()
    }

    pub const fn affected_kind(&self) -> Option<DiagnosticEventKind> {
        self.affected_kind
    }

    pub const fn affected_scope(&self) -> Option<&DiagnosticScope> {
        self.affected_scope.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CustomSpanStarted {
    #[serde(flatten)]
    pub(crate) header: DiagnosticEventHeader,
    pub(crate) name: String,
    pub(crate) parent_span_id: Option<SchemaU64>,
    pub(crate) attributes: DiagnosticAttributes,
}

impl CustomSpanStarted {
    pub fn validate_fields(
        name: &str,
        attributes: &DiagnosticAttributes,
    ) -> Result<(), EventValidationError> {
        validate_custom_name(name)?;
        validate_attributes(attributes)?;
        Ok(())
    }

    pub fn new(
        header: DiagnosticEventHeader,
        name: String,
        parent_span_id: Option<SchemaU64>,
        attributes: DiagnosticAttributes,
    ) -> Result<Self, EventValidationError> {
        Self::validate_fields(&name, &attributes)?;
        Ok(Self {
            header,
            name,
            parent_span_id,
            attributes,
        })
    }

    pub const fn header(&self) -> &DiagnosticEventHeader {
        &self.header
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn parent_span_id(&self) -> Option<SchemaU64> {
        self.parent_span_id
    }

    pub const fn attributes(&self) -> &DiagnosticAttributes {
        &self.attributes
    }
}

#[derive(Deserialize)]
struct CustomSpanStartedWire {
    #[serde(flatten)]
    header: DiagnosticEventHeader,
    name: String,
    parent_span_id: Option<SchemaU64>,
    attributes: DiagnosticAttributes,
}

impl TryFrom<CustomSpanStartedWire> for CustomSpanStarted {
    type Error = EventValidationError;

    fn try_from(wire: CustomSpanStartedWire) -> Result<Self, Self::Error> {
        Self::new(wire.header, wire.name, wire.parent_span_id, wire.attributes)
    }
}

impl<'de> Deserialize<'de> for CustomSpanStarted {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        CustomSpanStartedWire::deserialize(deserializer)?
            .try_into()
            .map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CustomSpanFinished {
    #[serde(flatten)]
    pub(crate) header: DiagnosticEventHeader,
    pub(crate) span_id: SchemaU64,
    pub(crate) outcome: SpanOutcome,
}

impl CustomSpanFinished {
    pub const fn new(
        header: DiagnosticEventHeader,
        span_id: SchemaU64,
        outcome: SpanOutcome,
    ) -> Self {
        Self {
            header,
            span_id,
            outcome,
        }
    }

    pub const fn header(&self) -> &DiagnosticEventHeader {
        &self.header
    }

    pub const fn span_id(&self) -> SchemaU64 {
        self.span_id
    }

    pub const fn outcome(&self) -> SpanOutcome {
        self.outcome
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CustomInstantOccurred {
    #[serde(flatten)]
    pub(crate) header: DiagnosticEventHeader,
    pub(crate) name: String,
    pub(crate) containing_span_id: Option<SchemaU64>,
    pub(crate) severity: Option<CustomSeverity>,
    pub(crate) attributes: DiagnosticAttributes,
}

impl CustomInstantOccurred {
    pub fn validate_fields(
        name: &str,
        attributes: &DiagnosticAttributes,
    ) -> Result<(), EventValidationError> {
        validate_custom_name(name)?;
        validate_attributes(attributes)?;
        Ok(())
    }

    pub fn new(
        header: DiagnosticEventHeader,
        name: String,
        containing_span_id: Option<SchemaU64>,
        severity: Option<CustomSeverity>,
        attributes: DiagnosticAttributes,
    ) -> Result<Self, EventValidationError> {
        Self::validate_fields(&name, &attributes)?;
        Ok(Self {
            header,
            name,
            containing_span_id,
            severity,
            attributes,
        })
    }

    pub const fn header(&self) -> &DiagnosticEventHeader {
        &self.header
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn containing_span_id(&self) -> Option<SchemaU64> {
        self.containing_span_id
    }

    pub const fn severity(&self) -> Option<CustomSeverity> {
        self.severity
    }

    pub const fn attributes(&self) -> &DiagnosticAttributes {
        &self.attributes
    }
}

#[derive(Deserialize)]
struct CustomInstantOccurredWire {
    #[serde(flatten)]
    header: DiagnosticEventHeader,
    name: String,
    containing_span_id: Option<SchemaU64>,
    severity: Option<CustomSeverity>,
    attributes: DiagnosticAttributes,
}

impl TryFrom<CustomInstantOccurredWire> for CustomInstantOccurred {
    type Error = EventValidationError;

    fn try_from(wire: CustomInstantOccurredWire) -> Result<Self, Self::Error> {
        Self::new(
            wire.header,
            wire.name,
            wire.containing_span_id,
            wire.severity,
            wire.attributes,
        )
    }
}

impl<'de> Deserialize<'de> for CustomInstantOccurred {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        CustomInstantOccurredWire::deserialize(deserializer)?
            .try_into()
            .map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CustomCounterSampled {
    #[serde(flatten)]
    pub(crate) header: DiagnosticEventHeader,
    pub(crate) name: String,
    pub(crate) value: CustomNumber,
    pub(crate) unit: Option<String>,
    pub(crate) dimensions: DiagnosticDimensions,
}

impl CustomCounterSampled {
    pub fn validate_fields(
        name: &str,
        unit: Option<&str>,
        dimensions: &DiagnosticDimensions,
    ) -> Result<(), EventValidationError> {
        validate_custom_name(name)?;
        validate_unit(unit)?;
        validate_dimensions(dimensions)?;
        Ok(())
    }

    pub fn new(
        header: DiagnosticEventHeader,
        name: String,
        value: CustomNumber,
        unit: Option<String>,
        dimensions: DiagnosticDimensions,
    ) -> Result<Self, EventValidationError> {
        Self::validate_fields(&name, unit.as_deref(), &dimensions)?;
        Ok(Self {
            header,
            name,
            value,
            unit,
            dimensions,
        })
    }

    pub const fn header(&self) -> &DiagnosticEventHeader {
        &self.header
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn value(&self) -> &CustomNumber {
        &self.value
    }

    pub fn unit(&self) -> Option<&str> {
        self.unit.as_deref()
    }

    pub const fn dimensions(&self) -> &DiagnosticDimensions {
        &self.dimensions
    }
}

#[derive(Deserialize)]
struct CustomCounterSampledWire {
    #[serde(flatten)]
    header: DiagnosticEventHeader,
    name: String,
    value: CustomNumber,
    unit: Option<String>,
    dimensions: DiagnosticDimensions,
}

impl TryFrom<CustomCounterSampledWire> for CustomCounterSampled {
    type Error = EventValidationError;

    fn try_from(wire: CustomCounterSampledWire) -> Result<Self, Self::Error> {
        Self::new(
            wire.header,
            wire.name,
            wire.value,
            wire.unit,
            wire.dimensions,
        )
    }
}

impl<'de> Deserialize<'de> for CustomCounterSampled {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        CustomCounterSampledWire::deserialize(deserializer)?
            .try_into()
            .map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiagnosticEvent {
    SpanStarted(SpanStarted),
    SpanFinished(SpanFinished),
    InstantOccurred(InstantOccurred),
    CounterSampled(CounterSampled),
    AgentMessageDelta(AgentMessageDelta),
    AgentMessageCompleted(AgentMessageCompleted),
    AgentPlanSnapshot(AgentPlanSnapshot),
    ContextUsageSampled(ContextUsageSampled),
    ActTokenUsageFinalized(ActTokenUsageFinalized),
    ObservationGap(ObservationGap),
    CustomSpanStarted(CustomSpanStarted),
    CustomSpanFinished(CustomSpanFinished),
    CustomInstantOccurred(CustomInstantOccurred),
    CustomCounterSampled(CustomCounterSampled),
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum DiagnosticEventWire {
    SpanStarted(SpanStarted),
    SpanFinished(SpanFinished),
    InstantOccurred(InstantOccurred),
    CounterSampled(CounterSampled),
    AgentMessageDelta(AgentMessageDelta),
    AgentMessageCompleted(AgentMessageCompleted),
    AgentPlanSnapshot(AgentPlanSnapshot),
    ContextUsageSampled(ContextUsageSampled),
    ActTokenUsageFinalized(ActTokenUsageFinalized),
    ObservationGap(ObservationGap),
    CustomSpanStarted(CustomSpanStarted),
    CustomSpanFinished(CustomSpanFinished),
    CustomInstantOccurred(CustomInstantOccurred),
    CustomCounterSampled(CustomCounterSampled),
}

impl From<DiagnosticEventWire> for DiagnosticEvent {
    fn from(wire: DiagnosticEventWire) -> Self {
        match wire {
            DiagnosticEventWire::SpanStarted(value) => Self::SpanStarted(value),
            DiagnosticEventWire::SpanFinished(value) => Self::SpanFinished(value),
            DiagnosticEventWire::InstantOccurred(value) => Self::InstantOccurred(value),
            DiagnosticEventWire::CounterSampled(value) => Self::CounterSampled(value),
            DiagnosticEventWire::AgentMessageDelta(value) => Self::AgentMessageDelta(value),
            DiagnosticEventWire::AgentMessageCompleted(value) => Self::AgentMessageCompleted(value),
            DiagnosticEventWire::AgentPlanSnapshot(value) => Self::AgentPlanSnapshot(value),
            DiagnosticEventWire::ContextUsageSampled(value) => Self::ContextUsageSampled(value),
            DiagnosticEventWire::ActTokenUsageFinalized(value) => {
                Self::ActTokenUsageFinalized(value)
            }
            DiagnosticEventWire::ObservationGap(value) => Self::ObservationGap(value),
            DiagnosticEventWire::CustomSpanStarted(value) => Self::CustomSpanStarted(value),
            DiagnosticEventWire::CustomSpanFinished(value) => Self::CustomSpanFinished(value),
            DiagnosticEventWire::CustomInstantOccurred(value) => Self::CustomInstantOccurred(value),
            DiagnosticEventWire::CustomCounterSampled(value) => Self::CustomCounterSampled(value),
        }
    }
}

impl<'de> Deserialize<'de> for DiagnosticEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        validate_event_fields(&value).map_err(de::Error::custom)?;
        serde_json::from_value::<DiagnosticEventWire>(value)
            .map(Into::into)
            .map_err(de::Error::custom)
    }
}

impl DiagnosticEvent {
    pub const fn kind(&self) -> DiagnosticEventKind {
        match self {
            Self::SpanStarted(_) => DiagnosticEventKind::SpanStarted,
            Self::SpanFinished(_) => DiagnosticEventKind::SpanFinished,
            Self::InstantOccurred(_) => DiagnosticEventKind::InstantOccurred,
            Self::CounterSampled(_) => DiagnosticEventKind::CounterSampled,
            Self::AgentMessageDelta(_) => DiagnosticEventKind::AgentMessageDelta,
            Self::AgentMessageCompleted(_) => DiagnosticEventKind::AgentMessageCompleted,
            Self::AgentPlanSnapshot(_) => DiagnosticEventKind::AgentPlanSnapshot,
            Self::ContextUsageSampled(_) => DiagnosticEventKind::ContextUsageSampled,
            Self::ActTokenUsageFinalized(_) => DiagnosticEventKind::ActTokenUsageFinalized,
            Self::ObservationGap(_) => DiagnosticEventKind::ObservationGap,
            Self::CustomSpanStarted(_) => DiagnosticEventKind::CustomSpanStarted,
            Self::CustomSpanFinished(_) => DiagnosticEventKind::CustomSpanFinished,
            Self::CustomInstantOccurred(_) => DiagnosticEventKind::CustomInstantOccurred,
            Self::CustomCounterSampled(_) => DiagnosticEventKind::CustomCounterSampled,
        }
    }

    pub const fn header(&self) -> &DiagnosticEventHeader {
        match self {
            Self::SpanStarted(value) => &value.header,
            Self::SpanFinished(value) => &value.header,
            Self::InstantOccurred(value) => &value.header,
            Self::CounterSampled(value) => &value.header,
            Self::AgentMessageDelta(value) => &value.header,
            Self::AgentMessageCompleted(value) => &value.header,
            Self::AgentPlanSnapshot(value) => &value.header,
            Self::ContextUsageSampled(value) => &value.header,
            Self::ActTokenUsageFinalized(value) => &value.header,
            Self::ObservationGap(value) => &value.header,
            Self::CustomSpanStarted(value) => &value.header,
            Self::CustomSpanFinished(value) => &value.header,
            Self::CustomInstantOccurred(value) => &value.header,
            Self::CustomCounterSampled(value) => &value.header,
        }
    }
}

const COMMON_FIELDS: &[&str] = &[
    "schema_version",
    "run_id",
    "sequence",
    "elapsed_ns",
    "scope",
    "caused_by",
    "kind",
];

fn validate_event_fields(value: &Value) -> Result<(), EventValidationError> {
    let object = value
        .as_object()
        .ok_or_else(|| EventValidationError::new("diagnostic event must be a JSON object"))?;
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| EventValidationError::new("diagnostic event kind must be a string"))?;
    let payload_fields: &[&str] = match kind {
        "span_started" => &["span_kind", "parent_span_id", "detail"],
        "span_finished" => &["span_id", "outcome", "error_code"],
        "instant_occurred" => &["instant_kind", "containing_span_id", "detail"],
        "counter_sampled" => &["counter_kind", "value"],
        "agent_message_delta" => &["message_id", "source_message_id", "text_delta"],
        "agent_message_completed" => &[
            "message_id",
            "utf8_bytes",
            "unicode_scalar_count",
            "truncated",
        ],
        "agent_plan_snapshot" => &["entries", "truncated"],
        "context_usage_sampled" => &[
            "context_used_tokens",
            "context_window_tokens",
            "cumulative_cost_amount",
            "cumulative_cost_currency",
            "sample_origin",
            "observed_elapsed_ns",
        ],
        "act_token_usage_finalized" => &[
            "availability",
            "source",
            "unavailable_reason",
            "provider_total_tokens",
            "input_tokens",
            "output_tokens",
            "thought_tokens",
            "cached_read_tokens",
            "cached_write_tokens",
        ],
        "observation_gap" => &[
            "producer",
            "component",
            "reason",
            "dropped_count",
            "affected_elapsed",
            "affected_kind",
            "affected_scope",
        ],
        "custom_span_started" => &["name", "parent_span_id", "attributes"],
        "custom_span_finished" => &["span_id", "outcome"],
        "custom_instant_occurred" => &["name", "containing_span_id", "severity", "attributes"],
        "custom_counter_sampled" => &["name", "value", "unit", "dimensions"],
        _ => return Err(EventValidationError::new("unknown diagnostic event kind")),
    };
    if object.keys().any(|key| {
        !COMMON_FIELDS.contains(&key.as_str()) && !payload_fields.contains(&key.as_str())
    }) {
        return Err(EventValidationError::new(
            "diagnostic event contains an unknown field",
        ));
    }
    Ok(())
}
