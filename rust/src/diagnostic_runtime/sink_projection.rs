use std::{fmt, sync::Arc};

use troupe_agent_runtime::diagnostics::payload::{
    AgentToolPayloadActBudget, SinkOnlyJsonValue, SinkOnlyToolPayload, ToolPayloadSource,
};
use troupe_diagnostics_core::{
    event::{DiagnosticEvent, DiagnosticScope},
    hub::AcceptedDiagnosticEvent,
    kinds::{CounterKind, InstantKind, SpanKind},
};

use super::hooks::DiagnosticCaptureConfig;

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct SinkProjectedJsonValue {
    canonical_json: Arc<str>,
}

impl SinkProjectedJsonValue {
    fn from_sink_only(value: &SinkOnlyJsonValue) -> Self {
        Self {
            canonical_json: Arc::from(value.as_json().to_string()),
        }
    }

    pub(crate) fn canonical_json(&self) -> &str {
        &self.canonical_json
    }

    #[cfg(test)]
    pub(crate) fn from_canonical_json_for_test(value: impl Into<Arc<str>>) -> Self {
        Self {
            canonical_json: value.into(),
        }
    }
}

impl fmt::Debug for SinkProjectedJsonValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SinkProjectedJsonValue(<redacted>)")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SinkProjectedToolLocation {
    path: Arc<str>,
    line: Option<u32>,
}

impl SinkProjectedToolLocation {
    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) const fn line(&self) -> Option<u32> {
        self.line
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(path: impl Into<Arc<str>>, line: Option<u32>) -> Self {
        Self {
            path: path.into(),
            line,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SinkProjectedToolInput {
    raw_input: Option<SinkProjectedJsonValue>,
    truncated: bool,
}

impl SinkProjectedToolInput {
    pub(crate) const fn raw_input(&self) -> Option<&SinkProjectedJsonValue> {
        self.raw_input.as_ref()
    }

    pub(crate) const fn truncated(&self) -> bool {
        self.truncated
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(raw_input: Option<SinkProjectedJsonValue>, truncated: bool) -> Self {
        Self {
            raw_input,
            truncated,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SinkProjectedToolOutput {
    raw_output: Option<SinkProjectedJsonValue>,
    content: Vec<SinkProjectedJsonValue>,
    locations: Vec<SinkProjectedToolLocation>,
    truncated: bool,
}

impl SinkProjectedToolOutput {
    pub(crate) const fn raw_output(&self) -> Option<&SinkProjectedJsonValue> {
        self.raw_output.as_ref()
    }

    pub(crate) fn content(&self) -> &[SinkProjectedJsonValue] {
        &self.content
    }

    pub(crate) fn locations(&self) -> &[SinkProjectedToolLocation] {
        &self.locations
    }

    pub(crate) const fn truncated(&self) -> bool {
        self.truncated
    }

    #[cfg(test)]
    pub(crate) const fn new_for_test(
        raw_output: Option<SinkProjectedJsonValue>,
        content: Vec<SinkProjectedJsonValue>,
        locations: Vec<SinkProjectedToolLocation>,
        truncated: bool,
    ) -> Self {
        Self {
            raw_output,
            content,
            locations,
            truncated,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreparedSinkToolPayload {
    tool_call_id: Arc<str>,
    source: ToolPayloadSource,
    input: Option<SinkProjectedToolInput>,
    output: Option<SinkProjectedToolOutput>,
}

impl PreparedSinkToolPayload {
    fn from_applied(canonical_tool_call_id: &str, payload: &SinkOnlyToolPayload) -> Self {
        let input = payload.input().map(|input| SinkProjectedToolInput {
            raw_input: input
                .raw_input()
                .map(SinkProjectedJsonValue::from_sink_only),
            truncated: input.truncated(),
        });
        let output = payload.output().map(|output| SinkProjectedToolOutput {
            raw_output: output
                .raw_output()
                .map(SinkProjectedJsonValue::from_sink_only),
            content: output
                .content()
                .iter()
                .map(SinkProjectedJsonValue::from_sink_only)
                .collect(),
            locations: output
                .locations()
                .iter()
                .map(|location| SinkProjectedToolLocation {
                    path: Arc::from(location.path()),
                    line: location.line(),
                })
                .collect(),
            truncated: output.truncated(),
        });
        Self {
            tool_call_id: Arc::from(canonical_tool_call_id),
            source: payload.source(),
            input,
            output,
        }
    }

    pub(crate) fn tool_call_id(&self) -> &str {
        &self.tool_call_id
    }

    pub(crate) const fn source(&self) -> ToolPayloadSource {
        self.source
    }

    pub(crate) const fn input(&self) -> Option<&SinkProjectedToolInput> {
        self.input.as_ref()
    }

    pub(crate) const fn output(&self) -> Option<&SinkProjectedToolOutput> {
        self.output.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        tool_call_id: impl Into<Arc<str>>,
        source: ToolPayloadSource,
        input: Option<SinkProjectedToolInput>,
        output: Option<SinkProjectedToolOutput>,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            source,
            input,
            output,
        }
    }
}

pub(crate) fn prepare_sink_tool_payload(
    canonical_tool_call_id: &str,
    payload: &SinkOnlyToolPayload,
    budget: &mut AgentToolPayloadActBudget,
) -> PreparedSinkToolPayload {
    let mut payload = payload.clone();
    payload.apply_act_budget(budget);
    PreparedSinkToolPayload::from_applied(canonical_tool_call_id, &payload)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SinkProjectedEvent {
    canonical: AcceptedDiagnosticEvent,
    captured_input: Option<SinkProjectedToolInput>,
    captured_output: Option<SinkProjectedToolOutput>,
}

impl SinkProjectedEvent {
    pub(crate) fn canonical(&self) -> &AcceptedDiagnosticEvent {
        &self.canonical
    }

    pub(crate) fn event(&self) -> &DiagnosticEvent {
        self.canonical.event()
    }

    pub(crate) fn canonical_bytes(&self) -> &[u8] {
        self.canonical.canonical_bytes()
    }

    pub(crate) const fn captured_input(&self) -> Option<&SinkProjectedToolInput> {
        self.captured_input.as_ref()
    }

    pub(crate) const fn captured_output(&self) -> Option<&SinkProjectedToolOutput> {
        self.captured_output.as_ref()
    }
}

pub(crate) fn project_act_event(
    canonical: &AcceptedDiagnosticEvent,
    current_act_scope: &DiagnosticScope,
    capture: DiagnosticCaptureConfig,
    payload: Option<&PreparedSinkToolPayload>,
) -> Option<SinkProjectedEvent> {
    let event = canonical.event();
    if !event_impacts_current_act(event, current_act_scope) || !event_selected(canonical, capture) {
        return None;
    }

    let (captured_input, captured_output) = projected_tool_payload(event, capture, payload);
    Some(SinkProjectedEvent {
        canonical: canonical.clone(),
        captured_input,
        captured_output,
    })
}

fn event_impacts_current_act(event: &DiagnosticEvent, current_act_scope: &DiagnosticScope) -> bool {
    let affected_scope = match event {
        DiagnosticEvent::ObservationGap(gap) => gap
            .affected_scope()
            .unwrap_or_else(|| event.header().scope()),
        _ => event.header().scope(),
    };
    same_act(affected_scope, current_act_scope)
}

fn same_act(left: &DiagnosticScope, right: &DiagnosticScope) -> bool {
    matches!(
        (left.act_id(), right.act_id()),
        (Some(left), Some(right)) if left == right
    )
}

fn event_selected(canonical: &AcceptedDiagnosticEvent, capture: DiagnosticCaptureConfig) -> bool {
    match canonical.event() {
        DiagnosticEvent::SpanStarted(event) => span_selected(event.span_kind(), capture),
        DiagnosticEvent::SpanFinished(_) => canonical
            .built_in_span_kind()
            .is_some_and(|kind| span_selected(kind, capture)),
        DiagnosticEvent::InstantOccurred(event) => instant_selected(event.instant_kind(), capture),
        DiagnosticEvent::CounterSampled(event) => counter_selected(event.counter_kind(), capture),
        DiagnosticEvent::AgentMessageDelta(_) | DiagnosticEvent::AgentMessageCompleted(_) => {
            capture.agent_messages
        }
        DiagnosticEvent::AgentPlanSnapshot(_) => capture.plans,
        DiagnosticEvent::ContextUsageSampled(_) | DiagnosticEvent::ActTokenUsageFinalized(_) => {
            capture.usage
        }
        DiagnosticEvent::ObservationGap(_) => true,
        DiagnosticEvent::CustomSpanStarted(_)
        | DiagnosticEvent::CustomSpanFinished(_)
        | DiagnosticEvent::CustomInstantOccurred(_)
        | DiagnosticEvent::CustomCounterSampled(_) => capture.custom_events,
    }
}

pub(crate) const fn span_selected(kind: SpanKind, capture: DiagnosticCaptureConfig) -> bool {
    match kind {
        SpanKind::ActLifecycle | SpanKind::ActCaller | SpanKind::AgentTurn => true,
        SpanKind::AgentThinking => capture.agent_messages,
        SpanKind::ToolCall => capture.tool_calls,
        SpanKind::RunLifecycle
        | SpanKind::ProductionPathResolution
        | SpanKind::ProductionLoad
        | SpanKind::ProductionConstruct
        | SpanKind::ProductionStart
        | SpanKind::ProductionStop
        | SpanKind::ProductionShutdown
        | SpanKind::SceneLifecycle
        | SpanKind::SceneDrain
        | SpanKind::SceneCleanup
        | SpanKind::ActorHandleLifetime
        | SpanKind::CueMailboxWait
        | SpanKind::CueExecution
        | SpanKind::EffectLifecycle
        | SpanKind::AgentSessionOpening
        | SpanKind::AgentSessionLifecycle
        | SpanKind::AgentSessionClosing => false,
    }
}

pub(crate) const fn instant_selected(kind: InstantKind, capture: DiagnosticCaptureConfig) -> bool {
    match kind {
        InstantKind::ActAdmitted
        | InstantKind::ActWaitingReady
        | InstantKind::ActPromptSubmitted
        | InstantKind::ActCancelRequested
        | InstantKind::ActSupervisorHandoff
        | InstantKind::AgentTurnActivity
        | InstantKind::AgentTurnTerminal
        | InstantKind::AgentTurnSettled => true,
        InstantKind::ToolUpdated => capture.tool_calls,
        InstantKind::ResultSubmitted
        | InstantKind::ResultRejected
        | InstantKind::ResultRepairRequested
        | InstantKind::ResultAccepted
        | InstantKind::ResultMissing => capture.result_validation,
        InstantKind::ActorCast
        | InstantKind::CueAdmitted
        | InstantKind::CueEnqueued
        | InstantKind::CueDispatched
        | InstantKind::CueCancelRequested
        | InstantKind::EffectCreated
        | InstantKind::EffectReturned
        | InstantKind::EffectConsumed
        | InstantKind::AgentSessionReady
        | InstantKind::AgentSessionBroken
        | InstantKind::DiagnosticComponentFailed => false,
    }
}

pub(crate) const fn counter_selected(kind: CounterKind, capture: DiagnosticCaptureConfig) -> bool {
    match kind {
        CounterKind::AgentTurnActive | CounterKind::DiagnosticDroppedEvents => true,
        CounterKind::ResultValidationRejections => capture.result_validation,
        CounterKind::ActorMailboxDepth | CounterKind::CueActive => false,
    }
}

pub(crate) fn projected_tool_payload(
    event: &DiagnosticEvent,
    capture: DiagnosticCaptureConfig,
    payload: Option<&PreparedSinkToolPayload>,
) -> (
    Option<SinkProjectedToolInput>,
    Option<SinkProjectedToolOutput>,
) {
    let Some(payload) = payload.filter(|payload| payload_matches_event(payload, event)) else {
        return (None, None);
    };
    let input = if capture.tool_calls && capture.tool_inputs {
        payload.input().cloned()
    } else {
        None
    };
    let output = if capture.tool_calls && capture.tool_outputs {
        payload.output().cloned()
    } else {
        None
    };
    (input, output)
}

fn payload_matches_event(payload: &PreparedSinkToolPayload, event: &DiagnosticEvent) -> bool {
    let event_source = match event {
        DiagnosticEvent::SpanStarted(event) if event.span_kind() == SpanKind::ToolCall => {
            ToolPayloadSource::Started
        }
        DiagnosticEvent::InstantOccurred(event)
            if event.instant_kind() == InstantKind::ToolUpdated =>
        {
            ToolPayloadSource::Updated
        }
        _ => return false,
    };
    event_source == payload.source()
        && event
            .header()
            .scope()
            .tool_call_id()
            .is_some_and(|tool_call_id| tool_call_id.as_str() == payload.tool_call_id())
}
