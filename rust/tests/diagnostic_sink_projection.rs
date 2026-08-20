use std::convert::Infallible;

use troupe_agent_runtime::diagnostics::payload::{
    ACT_TOOL_PAYLOAD_MAX_BYTES, AgentToolPayloadActBudget, TOOL_PAYLOAD_SNAPSHOT_MAX_BYTES,
    ToolPayloadSource, with_sized_tool_input_for_test,
};
use troupe_diagnostics_core::{
    detail::{DiagnosticAttributes, EmptyDetail, PlanEntry, SpanStartDetail, ToolCallDetail},
    event::{
        AgentMessageDelta, AgentPlanSnapshot, ContextUsageSampled, CounterSampled,
        CustomInstantOccurred, DiagnosticEvent, DiagnosticEventHeader, DiagnosticEventKind,
        DiagnosticScope, ObservationGap, SpanFinished, SpanStarted,
    },
    hub::{
        AcceptedDiagnosticEvent, ActEventSubscriber, AdmissionReservation, AdmissionReserver,
        AdmissionSize, BoundedInMemoryReserver, DeliveryFailure, DiagnosticEventCandidate,
        EventIdentity, SinkOnlyDiagnosticHub,
    },
    id::{CanonicalUuid, RunLocalId},
    kinds::{
        ContextSampleOrigin, CounterKind, CustomSeverity, InstantKind, SpanKind, SpanOutcome,
        ToolCallStatus, ToolKind,
    },
    scalar::SchemaU64,
    time::ElapsedNs,
};

mod orchestration {
    pub(crate) mod scene_context {
        pub(crate) struct CuedScope;
        pub(crate) struct RunBinding;
    }
}

#[allow(dead_code)]
#[path = "../src/diagnostic_runtime/hooks.rs"]
mod hooks;
#[allow(dead_code)]
#[path = "../src/diagnostic_runtime/sink_projection.rs"]
mod sink_projection;

use hooks::DiagnosticCaptureConfig;
use sink_projection::{
    PreparedSinkToolPayload, SinkProjectedJsonValue, SinkProjectedToolInput,
    SinkProjectedToolLocation, SinkProjectedToolOutput, counter_selected, instant_selected,
    prepare_sink_tool_payload, project_act_event, projected_tool_payload, span_selected,
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";

#[derive(Clone, Copy, Debug, Default)]
struct TestReserver;

struct TestReservation;

impl AdmissionReservation for TestReservation {
    fn commit(self, _event: AcceptedDiagnosticEvent) {}
}

impl AdmissionReserver for TestReserver {
    type Error = Infallible;
    type Reservation = TestReservation;

    fn try_reserve(&mut self, _size: AdmissionSize) -> Result<Self::Reservation, Self::Error> {
        Ok(TestReservation)
    }
}

impl BoundedInMemoryReserver for TestReserver {}

struct TestSubscriber;

impl ActEventSubscriber for TestSubscriber {
    fn deliver(&self, _event: AcceptedDiagnosticEvent) -> Result<(), DeliveryFailure> {
        Ok(())
    }
}

fn hub() -> SinkOnlyDiagnosticHub<TestReserver> {
    SinkOnlyDiagnosticHub::sink_only(CanonicalUuid::parse(RUN_ID).unwrap(), TestReserver)
}

fn id(value: &str) -> RunLocalId {
    RunLocalId::parse(value).unwrap()
}

fn act_scope(act_id: &str) -> DiagnosticScope {
    DiagnosticScope::new(
        Some(id("scene-1")),
        Some(id("actor-1")),
        Some(id("cue-1")),
        None,
        Some(id(act_id)),
        None,
        Some(SchemaU64::new(1)),
    )
}

fn tool_scope(act_id: &str, tool_call_id: &str) -> DiagnosticScope {
    DiagnosticScope::new(
        Some(id("scene-1")),
        Some(id("actor-1")),
        Some(id("cue-1")),
        None,
        Some(id(act_id)),
        Some(id(tool_call_id)),
        Some(SchemaU64::new(1)),
    )
}

fn header(identity: EventIdentity, scope: DiagnosticScope) -> DiagnosticEventHeader {
    DiagnosticEventHeader::new(
        identity.run_id(),
        identity.sequence(),
        ElapsedNs::new(identity.sequence().get()),
        scope,
        Vec::new(),
    )
    .unwrap()
}

fn accept<C>(hub: &SinkOnlyDiagnosticHub<TestReserver>, candidate: C) -> AcceptedDiagnosticEvent
where
    C: DiagnosticEventCandidate,
{
    hub.admit(candidate, &TestSubscriber)
        .unwrap()
        .accepted()
        .clone()
}

#[allow(clippy::too_many_arguments)]
fn capture(
    agent_messages: bool,
    plans: bool,
    tool_calls: bool,
    result_validation: bool,
    usage: bool,
    custom_events: bool,
    tool_inputs: bool,
    tool_outputs: bool,
) -> DiagnosticCaptureConfig {
    DiagnosticCaptureConfig::new(
        agent_messages,
        plans,
        tool_calls,
        result_validation,
        usage,
        custom_events,
        tool_inputs,
        tool_outputs,
    )
}

fn none() -> DiagnosticCaptureConfig {
    capture(false, false, false, false, false, false, false, false)
}

fn all() -> DiagnosticCaptureConfig {
    capture(true, true, true, true, true, true, true, true)
}

fn expected_span_capture(kind: SpanKind) -> &'static str {
    match kind {
        SpanKind::ActLifecycle | SpanKind::ActCaller | SpanKind::AgentTurn => "always",
        SpanKind::AgentThinking => "agent_messages",
        SpanKind::ToolCall => "tool_calls",
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
        | SpanKind::AgentSessionClosing => "excluded",
    }
}

fn expected_instant_capture(kind: InstantKind) -> &'static str {
    match kind {
        InstantKind::ActAdmitted
        | InstantKind::ActWaitingReady
        | InstantKind::ActPromptSubmitted
        | InstantKind::ActCancelRequested
        | InstantKind::ActSupervisorHandoff
        | InstantKind::AgentTurnActivity
        | InstantKind::AgentTurnTerminal
        | InstantKind::AgentTurnSettled => "always",
        InstantKind::ToolUpdated => "tool_calls",
        InstantKind::ResultSubmitted
        | InstantKind::ResultRejected
        | InstantKind::ResultRepairRequested
        | InstantKind::ResultAccepted
        | InstantKind::ResultMissing => "result_validation",
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
        | InstantKind::DiagnosticComponentFailed => "excluded",
    }
}

fn expected_counter_capture(kind: CounterKind) -> &'static str {
    match kind {
        CounterKind::AgentTurnActive | CounterKind::DiagnosticDroppedEvents => "always",
        CounterKind::ResultValidationRejections => "result_validation",
        CounterKind::ActorMailboxDepth | CounterKind::CueActive => "excluded",
    }
}

fn assert_rule(fixture: &str, kind: &str, capture: &str) {
    let rule = format!(r#"{{"kind": "{kind}", "capture": "{capture}"}}"#);
    assert!(fixture.contains(&rule), "missing fixture rule {rule}");
}

#[test]
fn checked_fixture_and_projection_exhaust_the_frozen_d34_matrix() {
    let fixture = include_str!("../../tests/fixtures/diagnostics/sink-projection.json");
    assert!(fixture.ends_with('\n'));
    assert!(fixture.contains(r#""schema": "troupe.diagnostics.sink-projection""#));
    assert_eq!(fixture.matches(r#"{"kind": "#).count(), 66);

    let disabled = none();
    let enabled = all();
    for &kind in SpanKind::ALL {
        let expected = expected_span_capture(kind);
        assert_rule(fixture, kind.as_str(), expected);
        assert_eq!(span_selected(kind, disabled), expected == "always");
        assert_eq!(span_selected(kind, enabled), expected != "excluded");
    }
    for &kind in InstantKind::ALL {
        let expected = expected_instant_capture(kind);
        assert_rule(fixture, kind.as_str(), expected);
        assert_eq!(instant_selected(kind, disabled), expected == "always");
        assert_eq!(instant_selected(kind, enabled), expected != "excluded");
    }
    for &kind in CounterKind::ALL {
        let expected = expected_counter_capture(kind);
        assert_rule(fixture, kind.as_str(), expected);
        assert_eq!(counter_selected(kind, disabled), expected == "always");
        assert_eq!(counter_selected(kind, enabled), expected != "excluded");
    }

    for (kind, capture) in [
        (DiagnosticEventKind::SpanStarted, "by_span_kind"),
        (DiagnosticEventKind::SpanFinished, "by_span_kind"),
        (DiagnosticEventKind::InstantOccurred, "by_instant_kind"),
        (DiagnosticEventKind::CounterSampled, "by_counter_kind"),
        (DiagnosticEventKind::AgentMessageDelta, "agent_messages"),
        (DiagnosticEventKind::AgentMessageCompleted, "agent_messages"),
        (DiagnosticEventKind::AgentPlanSnapshot, "plans"),
        (DiagnosticEventKind::ContextUsageSampled, "usage"),
        (DiagnosticEventKind::ActTokenUsageFinalized, "usage"),
        (DiagnosticEventKind::ObservationGap, "always"),
        (DiagnosticEventKind::CustomSpanStarted, "custom_events"),
        (DiagnosticEventKind::CustomSpanFinished, "custom_events"),
        (DiagnosticEventKind::CustomInstantOccurred, "custom_events"),
        (DiagnosticEventKind::CustomCounterSampled, "custom_events"),
    ] {
        assert_rule(fixture, kind.as_str(), capture);
    }
}

#[test]
fn projection_is_current_act_only_and_preserves_the_accepted_fact() {
    let hub = hub();
    let current = act_scope("act-current");
    let other = act_scope("act-other");
    let current_counter = accept(&hub, {
        let scope = current.clone();
        move |identity| {
            DiagnosticEvent::CounterSampled(CounterSampled::new(
                header(identity, scope),
                CounterKind::AgentTurnActive,
                SchemaU64::new(1),
            ))
        }
    });
    let projected = project_act_event(&current_counter, &current, none(), None).unwrap();
    assert!(projected.canonical().same_fact(&current_counter));
    assert_eq!(projected.event(), current_counter.event());
    assert_eq!(
        projected.canonical_bytes(),
        current_counter.canonical_bytes()
    );
    assert_eq!(
        Some(projected.clone()),
        project_act_event(&current_counter, &current, none(), None)
    );

    let other_counter = accept(&hub, {
        let scope = other.clone();
        move |identity| {
            DiagnosticEvent::CounterSampled(CounterSampled::new(
                header(identity, scope),
                CounterKind::DiagnosticDroppedEvents,
                SchemaU64::new(2),
            ))
        }
    });
    assert!(project_act_event(&other_counter, &current, all(), None).is_none());

    let gap_affecting_current = accept(&hub, {
        let header_scope = other.clone();
        let affected_scope = current.clone();
        move |identity| {
            DiagnosticEvent::ObservationGap(ObservationGap::new(
                header(identity, header_scope),
                "act_sink".to_owned(),
                None,
                "subscriber_overflow".to_owned(),
                Some(SchemaU64::new(1)),
                None,
                None,
                Some(affected_scope),
            ))
        }
    });
    assert!(project_act_event(&gap_affecting_current, &current, none(), None).is_some());

    let gap_affecting_other = accept(&hub, {
        let header_scope = current.clone();
        let affected_scope = other;
        move |identity| {
            DiagnosticEvent::ObservationGap(ObservationGap::new(
                header(identity, header_scope),
                "act_sink".to_owned(),
                None,
                "subscriber_overflow".to_owned(),
                Some(SchemaU64::new(1)),
                None,
                None,
                Some(affected_scope),
            ))
        }
    });
    assert!(project_act_event(&gap_affecting_other, &current, all(), None).is_none());
}

fn finish_span(
    hub: &SinkOnlyDiagnosticHub<TestReserver>,
    scope: DiagnosticScope,
    span_id: SchemaU64,
) -> AcceptedDiagnosticEvent {
    accept(hub, move |identity| {
        DiagnosticEvent::SpanFinished(SpanFinished::new(
            header(identity, scope),
            span_id,
            SpanOutcome::Completed,
            None,
        ))
    })
}

#[test]
fn resolved_finish_kinds_follow_the_same_capture_rule_as_their_starts() {
    let hub = hub();
    let current = act_scope("act-finish");

    let caller_start = accept(&hub, {
        let scope = current.clone();
        move |identity| {
            DiagnosticEvent::SpanStarted(SpanStarted::new(
                header(identity, scope),
                SpanStartDetail::ActCaller(EmptyDetail::new()),
                None,
            ))
        }
    });
    let caller_finish = finish_span(&hub, current.clone(), caller_start.identity().sequence());
    assert_eq!(
        caller_finish.built_in_span_kind(),
        Some(SpanKind::ActCaller)
    );
    assert!(project_act_event(&caller_start, &current, none(), None).is_some());
    assert!(project_act_event(&caller_finish, &current, none(), None).is_some());

    let thinking_start = accept(&hub, {
        let scope = current.clone();
        move |identity| {
            DiagnosticEvent::SpanStarted(SpanStarted::new(
                header(identity, scope),
                SpanStartDetail::AgentThinking(EmptyDetail::new()),
                None,
            ))
        }
    });
    let thinking_finish = finish_span(&hub, current.clone(), thinking_start.identity().sequence());
    assert_eq!(
        thinking_finish.built_in_span_kind(),
        Some(SpanKind::AgentThinking)
    );
    assert!(project_act_event(&thinking_start, &current, none(), None).is_none());
    assert!(project_act_event(&thinking_finish, &current, none(), None).is_none());
    let messages = capture(true, false, false, false, false, false, false, false);
    assert!(project_act_event(&thinking_start, &current, messages, None).is_some());
    assert!(project_act_event(&thinking_finish, &current, messages, None).is_some());

    let tool = tool_scope("act-finish", "tool-finish");
    let tool_start = accept(&hub, {
        let scope = tool.clone();
        move |identity| {
            DiagnosticEvent::SpanStarted(SpanStarted::new(
                header(identity, scope),
                SpanStartDetail::ToolCall(ToolCallDetail::new(
                    "Read".to_owned(),
                    ToolKind::Read,
                    ToolCallStatus::InProgress,
                    None,
                )),
                None,
            ))
        }
    });
    let tool_finish = finish_span(&hub, tool, tool_start.identity().sequence());
    assert_eq!(tool_finish.built_in_span_kind(), Some(SpanKind::ToolCall));
    assert!(project_act_event(&tool_start, &current, none(), None).is_none());
    assert!(project_act_event(&tool_finish, &current, none(), None).is_none());
    let tools = capture(false, false, true, false, false, false, false, false);
    assert!(project_act_event(&tool_start, &current, tools, None).is_some());
    assert!(project_act_event(&tool_finish, &current, tools, None).is_some());
}

#[test]
fn variant_flags_only_add_their_frozen_event_families() {
    let hub = hub();
    let current = act_scope("act-flags");

    let message = accept(&hub, {
        let scope = current.clone();
        move |identity| {
            DiagnosticEvent::AgentMessageDelta(AgentMessageDelta::new(
                header(identity, scope),
                id("message-1"),
                None,
                "hello".to_owned(),
            ))
        }
    });
    assert!(project_act_event(&message, &current, none(), None).is_none());
    assert!(
        project_act_event(
            &message,
            &current,
            capture(true, false, false, false, false, false, false, false),
            None,
        )
        .is_some()
    );

    let plan = accept(&hub, {
        let scope = current.clone();
        move |identity| {
            DiagnosticEvent::AgentPlanSnapshot(AgentPlanSnapshot::new(
                header(identity, scope),
                Vec::<PlanEntry>::new(),
                false,
            ))
        }
    });
    assert!(project_act_event(&plan, &current, none(), None).is_none());
    assert!(
        project_act_event(
            &plan,
            &current,
            capture(false, true, false, false, false, false, false, false),
            None,
        )
        .is_some()
    );

    let usage = accept(&hub, {
        let scope = current.clone();
        move |identity| {
            DiagnosticEvent::ContextUsageSampled(
                ContextUsageSampled::new(
                    header(identity, scope),
                    Some(SchemaU64::new(10)),
                    Some(SchemaU64::new(100)),
                    None,
                    None,
                    ContextSampleOrigin::Provider,
                    None,
                )
                .unwrap(),
            )
        }
    });
    assert!(project_act_event(&usage, &current, none(), None).is_none());
    assert!(
        project_act_event(
            &usage,
            &current,
            capture(false, false, false, false, true, false, false, false),
            None,
        )
        .is_some()
    );

    let custom = accept(&hub, {
        let scope = current.clone();
        move |identity| {
            DiagnosticEvent::CustomInstantOccurred(
                CustomInstantOccurred::new(
                    header(identity, scope),
                    "demo.notice".to_owned(),
                    None,
                    Some(CustomSeverity::Info),
                    DiagnosticAttributes::default(),
                )
                .unwrap(),
            )
        }
    });
    assert!(project_act_event(&custom, &current, none(), None).is_none());
    assert!(
        project_act_event(
            &custom,
            &current,
            capture(false, false, false, false, false, true, false, false),
            None,
        )
        .is_some()
    );
}

fn projected_value(json: &str) -> SinkProjectedJsonValue {
    SinkProjectedJsonValue::from_canonical_json_for_test(json)
}

fn prepared_payload(tool_call_id: &str, source: ToolPayloadSource) -> PreparedSinkToolPayload {
    PreparedSinkToolPayload::new_for_test(
        tool_call_id,
        source,
        Some(SinkProjectedToolInput::new_for_test(
            Some(projected_value(r#"{"path":"src/lib.rs"}"#)),
            false,
        )),
        Some(SinkProjectedToolOutput::new_for_test(
            Some(projected_value(r#"{"ok":true}"#)),
            vec![projected_value(r#"{"type":"text","text":"done"}"#)],
            vec![SinkProjectedToolLocation::new_for_test(
                "src/lib.rs",
                Some(17),
            )],
            false,
        )),
    )
}

#[test]
fn tool_payload_is_opt_in_enrichment_and_never_changes_the_canonical_fact() {
    let hub = hub();
    let current = act_scope("act-payload");
    let tool = tool_scope("act-payload", "tool-1");
    let start = accept(&hub, {
        let scope = tool.clone();
        move |identity| {
            DiagnosticEvent::SpanStarted(SpanStarted::new(
                header(identity, scope),
                SpanStartDetail::ToolCall(ToolCallDetail::new(
                    "Read".to_owned(),
                    ToolKind::Read,
                    ToolCallStatus::InProgress,
                    None,
                )),
                None,
            ))
        }
    });
    let payload = prepared_payload("tool-1", ToolPayloadSource::Started);
    let tools_only = capture(false, false, true, false, false, false, false, false);
    let without_fields = project_act_event(&start, &current, tools_only, Some(&payload)).unwrap();
    assert!(without_fields.captured_input().is_none());
    assert!(without_fields.captured_output().is_none());

    let inconsistent = capture(false, false, false, false, false, false, true, true);
    let (input, output) = projected_tool_payload(start.event(), inconsistent, Some(&payload));
    assert!(input.is_none() && output.is_none());
    assert!(project_act_event(&start, &current, inconsistent, Some(&payload)).is_none());

    let input_only = capture(false, false, true, false, false, false, true, false);
    let input = project_act_event(&start, &current, input_only, Some(&payload)).unwrap();
    assert_eq!(
        input
            .captured_input()
            .unwrap()
            .raw_input()
            .unwrap()
            .canonical_json(),
        r#"{"path":"src/lib.rs"}"#
    );
    assert!(input.captured_output().is_none());

    let output_only = capture(false, false, true, false, false, false, false, true);
    let output = project_act_event(&start, &current, output_only, Some(&payload)).unwrap();
    assert!(output.captured_input().is_none());
    let captured_output = output.captured_output().unwrap();
    assert_eq!(
        captured_output.raw_output().unwrap().canonical_json(),
        r#"{"ok":true}"#
    );
    assert_eq!(captured_output.content().len(), 1);
    assert_eq!(captured_output.locations()[0].path(), "src/lib.rs");
    assert_eq!(captured_output.locations()[0].line(), Some(17));

    let both = capture(false, false, true, false, false, false, true, true);
    let projected = project_act_event(&start, &current, both, Some(&payload)).unwrap();
    assert!(projected.canonical().same_fact(&start));
    assert_eq!(projected.canonical_bytes(), start.canonical_bytes());
    assert_eq!(
        Some(projected.clone()),
        project_act_event(&start, &current, both, Some(&payload))
    );

    let wrong_id = prepared_payload("tool-other", ToolPayloadSource::Started);
    let wrong_source = prepared_payload("tool-1", ToolPayloadSource::Updated);
    for mismatched in [&wrong_id, &wrong_source] {
        let projected = project_act_event(&start, &current, both, Some(mismatched)).unwrap();
        assert!(projected.captured_input().is_none());
        assert!(projected.captured_output().is_none());
    }
}

#[test]
fn real_a09_budget_drives_none_equal_and_over_budget_projection() {
    let fixture = include_str!("../../tests/fixtures/diagnostics/sink-projection.json");
    for case in [
        "no_payload",
        "equal_to_remaining_act_budget",
        "over_remaining_act_budget",
    ] {
        assert!(
            fixture.contains(&format!(r#""case": "{case}""#)),
            "missing executable fixture case {case}"
        );
    }

    let hub = hub();
    let current = act_scope("act-budget");
    let tool = tool_scope("act-budget", "tool-budget");
    let start = accept(&hub, move |identity| {
        DiagnosticEvent::SpanStarted(SpanStarted::new(
            header(identity, tool),
            SpanStartDetail::ToolCall(ToolCallDetail::new(
                "Read".to_owned(),
                ToolKind::Read,
                ToolCallStatus::InProgress,
                None,
            )),
            None,
        ))
    });
    let capture_input = capture(false, false, true, false, false, false, true, false);
    let mut budget = AgentToolPayloadActBudget::new();

    let no_payload = project_act_event(&start, &current, capture_input, None).unwrap();
    assert!(no_payload.captured_input().is_none());
    assert_eq!(budget.accepted_bytes(), 0);
    assert!(!budget.truncated());

    assert_eq!(
        ACT_TOOL_PAYLOAD_MAX_BYTES % TOOL_PAYLOAD_SNAPSHOT_MAX_BYTES,
        0
    );
    let snapshots_at_limit = ACT_TOOL_PAYLOAD_MAX_BYTES / TOOL_PAYLOAD_SNAPSHOT_MAX_BYTES;
    for _ in 0..snapshots_at_limit {
        let prepared = with_sized_tool_input_for_test(
            "provider-tool-budget",
            TOOL_PAYLOAD_SNAPSHOT_MAX_BYTES,
            |payload| prepare_sink_tool_payload("tool-budget", payload, &mut budget),
        );
        let projected =
            project_act_event(&start, &current, capture_input, Some(&prepared)).unwrap();
        let input = projected.captured_input().unwrap();
        assert_eq!(
            input.raw_input().unwrap().canonical_json().len(),
            TOOL_PAYLOAD_SNAPSHOT_MAX_BYTES
        );
        assert!(!input.truncated());
    }
    assert_eq!(budget.accepted_bytes(), ACT_TOOL_PAYLOAD_MAX_BYTES);
    assert_eq!(budget.remaining_bytes(), 0);
    assert!(!budget.truncated());

    let over = with_sized_tool_input_for_test(
        "provider-tool-budget",
        TOOL_PAYLOAD_SNAPSHOT_MAX_BYTES,
        |payload| prepare_sink_tool_payload("tool-budget", payload, &mut budget),
    );
    let projected = project_act_event(&start, &current, capture_input, Some(&over)).unwrap();
    let input = projected.captured_input().unwrap();
    assert!(input.raw_input().is_none());
    assert!(input.truncated());
    assert_eq!(budget.accepted_bytes(), ACT_TOOL_PAYLOAD_MAX_BYTES);
    assert_eq!(budget.remaining_bytes(), 0);
    assert!(budget.truncated());
}
