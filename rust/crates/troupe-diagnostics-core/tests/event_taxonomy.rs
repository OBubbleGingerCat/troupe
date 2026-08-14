use std::{fs, process::Command, time::SystemTime};

use serde_json::{Value, json};
use troupe_diagnostics_core::{
    detail::{
        CanonicalInteger, CustomNumber, DiagnosticAttributes, DiagnosticComponentFailedDetail,
        DiagnosticDimensions, EmptyDetail, InstantDetail, PlanEntry, SpanStartDetail,
    },
    event::{
        ActTokenUsageFinalized, AgentMessageCompleted, AgentMessageDelta, AgentPlanSnapshot,
        CausalLink, ContextUsageSampled, CounterSampled, CustomCounterSampled,
        CustomInstantOccurred, CustomSpanFinished, CustomSpanStarted, DiagnosticEvent,
        DiagnosticEventHeader, DiagnosticEventKind, DiagnosticScope, InstantOccurred,
        ObservationGap, SpanFinished, SpanStarted,
    },
    id::{CanonicalUuid, RunLocalId},
    kinds::{
        CausalRelation, ComponentFailureErrorCode, ComponentFailureStage, ContextSampleOrigin,
        CounterKind, DiagnosticComponent, InstantKind, PlanEntryPriority, PlanEntryStatus,
        SpanKind, SpanOutcome, ToolCallStatus, ToolKind, UsageAvailability, UsageSource,
    },
    scalar::{SchemaU64, TokenCount},
    time::ElapsedNs,
};

fn scope() -> Value {
    json!({
        "scene_id": "scene-1",
        "actor_id": "actor-1",
        "cue_id": "cue-1",
        "effect_id": null,
        "act_id": "act-1",
        "tool_call_id": null,
        "session_generation": "2"
    })
}

fn event_json(kind: &str, payload: Value) -> Value {
    let mut value = json!({
        "schema_version": 1,
        "run_id": "12345678-1234-4234-9234-123456789abc",
        "sequence": "42",
        "elapsed_ns": "9007199254740993",
        "scope": scope(),
        "caused_by": [
            {"source_sequence": "7", "relation": "dispatch"},
            {"source_sequence": "9", "relation": "follows_from"}
        ],
        "kind": kind,
    });
    value
        .as_object_mut()
        .unwrap()
        .extend(payload.as_object().unwrap().clone());
    value
}

fn decode(kind: &str, payload: Value) -> Result<DiagnosticEvent, serde_json::Error> {
    serde_json::from_value(event_json(kind, payload))
}

fn kind_wire<T>(value: T, expected: &str)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let encoded = serde_json::to_string(&value).unwrap();
    assert_eq!(encoded, format!("\"{expected}\""));
    assert_eq!(serde_json::from_str::<T>(&encoded).unwrap(), value);
}

fn assert_event_round_trip(kind: DiagnosticEventKind, payload: Value) -> DiagnosticEvent {
    let event = decode(kind.as_str(), payload).unwrap();
    assert_eq!(event.kind(), kind);
    assert_eq!(event.header().sequence().get(), 42);
    assert_eq!(event.header().elapsed_ns().get(), 9_007_199_254_740_993);
    assert_eq!(event.header().caused_by().len(), 2);
    let encoded = serde_json::to_value(&event).unwrap();
    assert_eq!(encoded["kind"], kind.as_str());
    assert_eq!(
        serde_json::from_value::<DiagnosticEvent>(encoded).unwrap(),
        event
    );
    event
}

fn empty_detail() -> Value {
    json!({})
}

fn actor_detail() -> Value {
    json!({"display_name": "worker", "actor_type": "Worker"})
}

fn effect_detail() -> Value {
    json!({"effect_type": "ResultEffect"})
}

fn session_detail() -> Value {
    json!({
        "provider": "codex",
        "effective_model": "gpt-5",
        "effective_effort": "high"
    })
}

fn tool_detail() -> Value {
    json!({
        "title": "Read files",
        "tool_kind": "read",
        "status": "in_progress",
        "error_code": null
    })
}

#[test]
fn taxonomy_has_exactly_fourteen_closed_snake_case_discriminants() {
    fn exhaustive(kind: DiagnosticEventKind) -> &'static str {
        match kind {
            DiagnosticEventKind::SpanStarted => "span_started",
            DiagnosticEventKind::SpanFinished => "span_finished",
            DiagnosticEventKind::InstantOccurred => "instant_occurred",
            DiagnosticEventKind::CounterSampled => "counter_sampled",
            DiagnosticEventKind::AgentMessageDelta => "agent_message_delta",
            DiagnosticEventKind::AgentMessageCompleted => "agent_message_completed",
            DiagnosticEventKind::AgentPlanSnapshot => "agent_plan_snapshot",
            DiagnosticEventKind::ContextUsageSampled => "context_usage_sampled",
            DiagnosticEventKind::ActTokenUsageFinalized => "act_token_usage_finalized",
            DiagnosticEventKind::ObservationGap => "observation_gap",
            DiagnosticEventKind::CustomSpanStarted => "custom_span_started",
            DiagnosticEventKind::CustomSpanFinished => "custom_span_finished",
            DiagnosticEventKind::CustomInstantOccurred => "custom_instant_occurred",
            DiagnosticEventKind::CustomCounterSampled => "custom_counter_sampled",
        }
    }

    assert_eq!(DiagnosticEventKind::ALL.len(), 14);
    for kind in DiagnosticEventKind::ALL {
        assert_eq!(kind.as_str(), exhaustive(kind));
        kind_wire(kind, exhaustive(kind));
    }
    assert!(serde_json::from_str::<DiagnosticEventKind>("\"future_event\"").is_err());
}

#[test]
fn all_span_kinds_have_closed_kind_coupled_start_details() {
    let cases = [
        (SpanKind::RunLifecycle, "run.lifecycle", empty_detail()),
        (
            SpanKind::ProductionPathResolution,
            "production.path_resolution",
            json!({"production_root": "/srv/demo", "package": "demo"}),
        ),
        (
            SpanKind::ProductionLoad,
            "production.load",
            json!({"package": "demo"}),
        ),
        (
            SpanKind::ProductionConstruct,
            "production.construct",
            json!({"package": "demo", "class_name": "Production"}),
        ),
        (
            SpanKind::ProductionStart,
            "production.start",
            empty_detail(),
        ),
        (SpanKind::ProductionStop, "production.stop", empty_detail()),
        (
            SpanKind::ProductionShutdown,
            "production.shutdown",
            empty_detail(),
        ),
        (SpanKind::SceneLifecycle, "scene.lifecycle", empty_detail()),
        (SpanKind::SceneDrain, "scene.drain", empty_detail()),
        (SpanKind::SceneCleanup, "scene.cleanup", empty_detail()),
        (
            SpanKind::ActorHandleLifetime,
            "actor.handle_lifetime",
            actor_detail(),
        ),
        (SpanKind::CueMailboxWait, "cue.mailbox_wait", empty_detail()),
        (SpanKind::CueExecution, "cue.execution", empty_detail()),
        (
            SpanKind::EffectLifecycle,
            "effect.lifecycle",
            effect_detail(),
        ),
        (
            SpanKind::AgentSessionOpening,
            "agent.session.opening",
            session_detail(),
        ),
        (
            SpanKind::AgentSessionLifecycle,
            "agent.session.lifecycle",
            session_detail(),
        ),
        (
            SpanKind::AgentSessionClosing,
            "agent.session.closing",
            session_detail(),
        ),
        (SpanKind::ActLifecycle, "act.lifecycle", session_detail()),
        (SpanKind::ActCaller, "act.caller", empty_detail()),
        (SpanKind::AgentTurn, "agent.turn", session_detail()),
        (SpanKind::AgentThinking, "agent.thinking", empty_detail()),
        (SpanKind::ToolCall, "tool.call", tool_detail()),
    ];

    assert_eq!(SpanKind::ALL.len(), cases.len());
    for (kind, wire, detail) in cases {
        assert_eq!(kind.as_str(), wire);
        kind_wire(kind, wire);
        let event = assert_event_round_trip(
            DiagnosticEventKind::SpanStarted,
            json!({
                "span_kind": wire,
                "parent_span_id": null,
                "detail": detail,
            }),
        );
        let encoded = serde_json::to_value(event).unwrap();
        assert_eq!(encoded["span_kind"], wire);
        assert_eq!(encoded["sequence"], "42");
        assert!(encoded.get("span_id").is_none());
        assert!(encoded.get("event_id").is_none());
        assert!(encoded.get("act_sequence").is_none());
    }

    assert!(
        decode(
            "span_started",
            json!({
                "span_kind": "future.span",
                "parent_span_id": null,
                "detail": {}
            })
        )
        .is_err()
    );
    assert!(
        decode(
            "span_started",
            json!({
                "span_kind": "run.lifecycle",
                "parent_span_id": null,
                "detail": {"provider": "raw"}
            })
        )
        .is_err()
    );
}

#[test]
fn all_instant_and_counter_kinds_are_closed_and_typed() {
    let result_detail = || json!({"issue": null, "error_code": null});
    let instant_cases = [
        (InstantKind::ActorCast, "actor.cast", actor_detail()),
        (InstantKind::CueAdmitted, "cue.admitted", empty_detail()),
        (InstantKind::CueEnqueued, "cue.enqueued", empty_detail()),
        (InstantKind::CueDispatched, "cue.dispatched", empty_detail()),
        (
            InstantKind::CueCancelRequested,
            "cue.cancel_requested",
            empty_detail(),
        ),
        (
            InstantKind::EffectCreated,
            "effect.created",
            effect_detail(),
        ),
        (
            InstantKind::EffectReturned,
            "effect.returned",
            effect_detail(),
        ),
        (
            InstantKind::EffectConsumed,
            "effect.consumed",
            effect_detail(),
        ),
        (
            InstantKind::AgentSessionReady,
            "agent.session.ready",
            session_detail(),
        ),
        (
            InstantKind::AgentSessionBroken,
            "agent.session.broken",
            json!({
                "provider": "codex",
                "effective_model": "gpt-5",
                "effective_effort": "high",
                "error_code": "transport_lost"
            }),
        ),
        (InstantKind::ActAdmitted, "act.admitted", empty_detail()),
        (
            InstantKind::ActWaitingReady,
            "act.waiting_ready",
            empty_detail(),
        ),
        (
            InstantKind::ActPromptSubmitted,
            "act.prompt_submitted",
            empty_detail(),
        ),
        (
            InstantKind::ActCancelRequested,
            "act.cancel_requested",
            empty_detail(),
        ),
        (
            InstantKind::ActSupervisorHandoff,
            "act.supervisor_handoff",
            empty_detail(),
        ),
        (
            InstantKind::AgentTurnActivity,
            "agent.turn.activity",
            empty_detail(),
        ),
        (
            InstantKind::AgentTurnTerminal,
            "agent.turn.terminal",
            json!({"error_code": null}),
        ),
        (
            InstantKind::AgentTurnSettled,
            "agent.turn.settled",
            json!({"error_code": null}),
        ),
        (InstantKind::ToolUpdated, "tool.updated", tool_detail()),
        (
            InstantKind::ResultSubmitted,
            "result.submitted",
            result_detail(),
        ),
        (
            InstantKind::ResultRejected,
            "result.rejected",
            json!({
                "issue": {"code": "out_of_range", "path": "/score"},
                "error_code": "invalid_result"
            }),
        ),
        (
            InstantKind::ResultRepairRequested,
            "result.repair_requested",
            result_detail(),
        ),
        (
            InstantKind::ResultAccepted,
            "result.accepted",
            result_detail(),
        ),
        (
            InstantKind::ResultMissing,
            "result.missing",
            result_detail(),
        ),
        (
            InstantKind::DiagnosticComponentFailed,
            "diagnostic.component_failed",
            json!({
                "component": "sink",
                "component_id": "sink-1",
                "stage": "callback",
                "error_code": "callback_raised",
                "related_event_sequence": "41"
            }),
        ),
    ];

    assert_eq!(InstantKind::ALL.len(), instant_cases.len());
    for (kind, wire, detail) in instant_cases {
        assert_eq!(kind.as_str(), wire);
        kind_wire(kind, wire);
        let event = assert_event_round_trip(
            DiagnosticEventKind::InstantOccurred,
            json!({
                "instant_kind": wire,
                "containing_span_id": "2",
                "detail": detail,
            }),
        );
        assert_eq!(serde_json::to_value(event).unwrap()["instant_kind"], wire);
    }

    let counters = [
        (CounterKind::ActorMailboxDepth, "actor.mailbox_depth"),
        (CounterKind::CueActive, "cue.active"),
        (CounterKind::AgentTurnActive, "agent.turn.active"),
        (
            CounterKind::ResultValidationRejections,
            "result.validation_rejections",
        ),
        (
            CounterKind::DiagnosticDroppedEvents,
            "diagnostic.dropped_events",
        ),
    ];
    assert_eq!(CounterKind::ALL.len(), counters.len());
    for (kind, wire) in counters {
        assert_eq!(kind.as_str(), wire);
        kind_wire(kind, wire);
        assert_event_round_trip(
            DiagnosticEventKind::CounterSampled,
            json!({"counter_kind": wire, "value": "0"}),
        );
    }

    assert!(
        decode(
            "instant_occurred",
            json!({
                "instant_kind": "future.instant",
                "containing_span_id": null,
                "detail": {}
            })
        )
        .is_err()
    );
    assert!(
        decode(
            "counter_sampled",
            json!({"counter_kind": "future.counter", "value": "1"})
        )
        .is_err()
    );
}

#[test]
fn envelope_scope_and_causality_use_canonical_closed_wire_values() {
    let event = assert_event_round_trip(
        DiagnosticEventKind::SpanFinished,
        json!({"span_id": "1", "outcome": "failed", "error_code": "user_error"}),
    );
    let encoded = serde_json::to_value(event).unwrap();
    assert!(encoded.get("detail").is_none());
    assert!(encoded.get("span_kind").is_none());
    assert_eq!(encoded["scope"]["session_generation"], "2");

    let relations = [
        (CausalRelation::Dispatch, "dispatch"),
        (CausalRelation::Return, "return"),
        (CausalRelation::Handoff, "handoff"),
        (CausalRelation::Retry, "retry"),
        (CausalRelation::FollowsFrom, "follows_from"),
    ];
    assert_eq!(CausalRelation::ALL.len(), relations.len());
    for (relation, wire) in relations {
        assert_eq!(relation.as_str(), wire);
        kind_wire(relation, wire);
    }

    let mut invalid = event_json(
        "span_finished",
        json!({"span_id": "1", "outcome": "completed", "error_code": null}),
    );
    invalid["sequence"] = json!(42);
    assert!(serde_json::from_value::<DiagnosticEvent>(invalid).is_err());
    let mut invalid = event_json(
        "span_finished",
        json!({"span_id": "1", "outcome": "completed", "error_code": null}),
    );
    invalid["sequence"] = json!("0");
    assert!(serde_json::from_value::<DiagnosticEvent>(invalid).is_err());
    let mut invalid = event_json(
        "span_finished",
        json!({"span_id": "1", "outcome": "completed", "error_code": null}),
    );
    invalid["schema_version"] = json!(2);
    assert!(serde_json::from_value::<DiagnosticEvent>(invalid).is_err());
    assert!(
        serde_json::from_value::<DiagnosticEvent>(event_json(
            "span_finished",
            json!({"span_id": "1", "outcome": "unknown", "error_code": null})
        ))
        .is_err()
    );
}

#[test]
fn remaining_variant_payloads_preserve_zero_unknown_and_unicode() {
    let variants = [
        (
            DiagnosticEventKind::AgentMessageDelta,
            json!({
                "message_id": "message-1",
                "source_message_id": null,
                "text_delta": "hello, 世界"
            }),
        ),
        (
            DiagnosticEventKind::AgentMessageCompleted,
            json!({
                "message_id": "message-1",
                "utf8_bytes": "13",
                "unicode_scalar_count": "9",
                "truncated": false
            }),
        ),
        (
            DiagnosticEventKind::AgentPlanSnapshot,
            json!({
                "entries": [
                    {"content": "inspect", "priority": "high", "status": "in_progress"},
                    {"content": "report", "priority": "low", "status": "pending"}
                ],
                "truncated": false
            }),
        ),
        (
            DiagnosticEventKind::ContextUsageSampled,
            json!({
                "context_used_tokens": "0",
                "context_window_tokens": "200000",
                "cumulative_cost_amount": null,
                "cumulative_cost_currency": null,
                "sample_origin": "provider",
                "observed_elapsed_ns": null
            }),
        ),
        (
            DiagnosticEventKind::ActTokenUsageFinalized,
            json!({
                "availability": "available",
                "source": "acp.prompt_response.usage",
                "unavailable_reason": null,
                "provider_total_tokens": "0",
                "input_tokens": "0",
                "output_tokens": "0",
                "thought_tokens": null,
                "cached_read_tokens": null,
                "cached_write_tokens": null
            }),
        ),
        (
            DiagnosticEventKind::ObservationGap,
            json!({
                "producer": "agent_observer",
                "component": "message_normalizer",
                "reason": "source_truncated",
                "dropped_count": null,
                "affected_elapsed": null,
                "affected_kind": null,
                "affected_scope": null
            }),
        ),
        (
            DiagnosticEventKind::CustomSpanStarted,
            json!({
                "name": "orders.select_supplier",
                "parent_span_id": null,
                "attributes": {
                    "region": {"type": "string", "value": "华东"},
                    "retry": {"type": "integer", "value": "0"}
                }
            }),
        ),
        (
            DiagnosticEventKind::CustomSpanFinished,
            json!({
                "span_id": "31",
                "outcome": "completed"
            }),
        ),
        (
            DiagnosticEventKind::CustomInstantOccurred,
            json!({
                "name": "orders.rejected",
                "containing_span_id": null,
                "severity": null,
                "attributes": {
                    "retryable": {"type": "boolean", "value": false},
                    "reason": {"type": "string", "value": "capacity"},
                    "tags": {
                        "type": "list",
                        "value": [
                            {"type": "string", "value": "priority"},
                            {"type": "null"}
                        ]
                    }
                }
            }),
        ),
        (
            DiagnosticEventKind::CustomCounterSampled,
            json!({
                "name": "orders.pending",
                "value": {"type": "decimal", "value": "1.25"},
                "unit": "items",
                "dimensions": {
                    "region": {"type": "string", "value": "east"}
                }
            }),
        ),
    ];

    for (kind, payload) in variants {
        assert_event_round_trip(kind, payload);
    }
}

fn context_payload(amount: Value, currency: Value) -> Value {
    json!({
        "context_used_tokens": "1",
        "context_window_tokens": "2",
        "cumulative_cost_amount": amount,
        "cumulative_cost_currency": currency,
        "sample_origin": "carried_forward",
        "observed_elapsed_ns": "10"
    })
}

#[test]
fn context_cost_is_an_exact_optional_pair() {
    assert!(
        decode(
            "context_usage_sampled",
            context_payload(json!("1.25"), json!("USD"))
        )
        .is_ok()
    );
    assert!(
        decode(
            "context_usage_sampled",
            context_payload(Value::Null, Value::Null)
        )
        .is_ok()
    );
    assert!(
        decode(
            "context_usage_sampled",
            context_payload(json!("1.25"), Value::Null)
        )
        .is_err()
    );
    assert!(
        decode(
            "context_usage_sampled",
            context_payload(Value::Null, json!("USD"))
        )
        .is_err()
    );
    assert!(
        decode(
            "context_usage_sampled",
            context_payload(json!("1.250"), json!("USD"))
        )
        .is_err()
    );
    assert!(
        decode(
            "context_usage_sampled",
            context_payload(json!("1.25"), json!("usd"))
        )
        .is_err()
    );
    assert!(
        decode(
            "context_usage_sampled",
            context_payload(json!("-1.25"), json!("USD"))
        )
        .is_err()
    );

    let mut only_used = context_payload(Value::Null, Value::Null);
    only_used["context_window_tokens"] = Value::Null;
    assert!(decode("context_usage_sampled", only_used).is_ok());
    let mut only_window = context_payload(Value::Null, Value::Null);
    only_window["context_used_tokens"] = Value::Null;
    assert!(decode("context_usage_sampled", only_window).is_ok());
    let mut invalid_occupancy = context_payload(Value::Null, Value::Null);
    invalid_occupancy["context_used_tokens"] = json!("3");
    invalid_occupancy["context_window_tokens"] = json!("2");
    assert!(decode("context_usage_sampled", invalid_occupancy).is_err());
}

fn usage_payload(
    availability: &str,
    source: Value,
    unavailable_reason: Value,
    values: [Value; 6],
) -> Value {
    let [total, input, output, thought, cached_read, cached_write] = values;
    json!({
        "availability": availability,
        "source": source,
        "unavailable_reason": unavailable_reason,
        "provider_total_tokens": total,
        "input_tokens": input,
        "output_tokens": output,
        "thought_tokens": thought,
        "cached_read_tokens": cached_read,
        "cached_write_tokens": cached_write
    })
}

fn no_usage_values() -> [Value; 6] {
    std::array::from_fn(|_| Value::Null)
}

#[test]
fn terminal_usage_rejects_every_illegal_availability_combination() {
    let source = json!("acp.prompt_response.usage");

    assert!(
        decode(
            "act_token_usage_finalized",
            usage_payload(
                "available",
                source.clone(),
                Value::Null,
                [
                    json!("3"),
                    json!("1"),
                    json!("2"),
                    Value::Null,
                    Value::Null,
                    Value::Null
                ]
            )
        )
        .is_ok()
    );
    assert!(
        decode(
            "act_token_usage_finalized",
            usage_payload(
                "partial",
                source.clone(),
                Value::Null,
                [
                    Value::Null,
                    json!("1"),
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null
                ]
            )
        )
        .is_ok()
    );
    for reason in [
        "prompt_not_submitted",
        "source_unsupported",
        "usage_not_reported",
        "turn_settlement_unknown",
    ] {
        assert!(
            decode(
                "act_token_usage_finalized",
                usage_payload("unavailable", Value::Null, json!(reason), no_usage_values(),)
            )
            .is_ok(),
            "{reason}"
        );
    }

    let invalid = [
        usage_payload(
            "available",
            source.clone(),
            Value::Null,
            [
                json!("3"),
                json!("1"),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        ),
        usage_payload(
            "available",
            source.clone(),
            json!("usage_not_reported"),
            [
                json!("3"),
                json!("1"),
                json!("2"),
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        ),
        usage_payload("partial", source.clone(), Value::Null, no_usage_values()),
        usage_payload(
            "partial",
            source.clone(),
            Value::Null,
            [
                json!("3"),
                json!("1"),
                json!("2"),
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        ),
        usage_payload(
            "partial",
            Value::Null,
            Value::Null,
            [
                Value::Null,
                json!("1"),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        ),
        usage_payload(
            "unavailable",
            Value::Null,
            json!("usage_not_reported"),
            [
                json!("0"),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        ),
        usage_payload(
            "unavailable",
            source,
            json!("usage_not_reported"),
            no_usage_values(),
        ),
        usage_payload("unavailable", Value::Null, Value::Null, no_usage_values()),
        usage_payload(
            "future",
            Value::Null,
            json!("usage_not_reported"),
            no_usage_values(),
        ),
    ];
    for payload in invalid {
        assert!(decode("act_token_usage_finalized", payload).is_err());
    }

    let huge = "184467440737095516160000000000000000000000000000000000000000";
    assert!(
        decode(
            "act_token_usage_finalized",
            usage_payload(
                "partial",
                json!("acp.prompt_response.usage"),
                Value::Null,
                [
                    json!(huge),
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null
                ]
            )
        )
        .is_ok()
    );
    for invalid_token in [json!(-1), json!(false), json!(1), json!("01"), json!("-1")] {
        assert!(
            decode(
                "act_token_usage_finalized",
                usage_payload(
                    "partial",
                    json!("acp.prompt_response.usage"),
                    Value::Null,
                    [
                        invalid_token,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null
                    ]
                )
            )
            .is_err()
        );
    }
}

fn component_failure(stage: &str, error_code: &str) -> Value {
    json!({
        "instant_kind": "diagnostic.component_failed",
        "containing_span_id": null,
        "detail": {
            "component": "sink",
            "component_id": "sink-1",
            "stage": stage,
            "error_code": error_code,
            "related_event_sequence": null
        }
    })
}

#[test]
fn component_failure_is_typed_bounded_and_contains_no_raw_failure_data() {
    for (stage, code) in [
        ("enqueue", "delivery_queue_unavailable"),
        ("callback", "callback_raised"),
        ("callback", "callback_invalid_return"),
    ] {
        assert!(decode("instant_occurred", component_failure(stage, code)).is_ok());
    }

    for (stage, code) in [
        ("callback", "delivery_queue_unavailable"),
        ("enqueue", "callback_raised"),
        ("future", "callback_raised"),
        ("callback", "future_error"),
    ] {
        assert!(decode("instant_occurred", component_failure(stage, code)).is_err());
    }

    for forbidden in [
        "exception",
        "traceback",
        "payload",
        "provider_raw",
        "credential",
        "script",
        "reasoning",
        "validated_result",
    ] {
        let mut payload = component_failure("callback", "callback_raised");
        payload["detail"]
            .as_object_mut()
            .unwrap()
            .insert(forbidden.to_owned(), json!("secret"));
        assert!(decode("instant_occurred", payload).is_err(), "{forbidden}");
    }
}

#[test]
fn plan_gap_and_custom_payloads_reject_unknown_or_ambiguous_values() {
    for field in ["priority", "status"] {
        let mut payload = json!({
            "entries": [{"content": "step", "priority": "high", "status": "pending"}],
            "truncated": false
        });
        payload["entries"][0][field] = json!("future");
        assert!(decode("agent_plan_snapshot", payload).is_err(), "{field}");
    }

    let gap = json!({
        "producer": "agent_observer",
        "component": null,
        "reason": "source_truncated",
        "dropped_count": "0",
        "affected_elapsed": {"start_ns": "0", "end_ns": "0"},
        "affected_kind": "agent_message_delta",
        "affected_scope": scope()
    });
    assert!(decode("observation_gap", gap.clone()).is_ok());
    let mut unknown_gap_kind = gap;
    unknown_gap_kind["affected_kind"] = json!("future_event");
    assert!(decode("observation_gap", unknown_gap_kind).is_err());

    for invalid_name in ["single", "Upper.case", "troupe.reserved", "two..parts"] {
        assert!(
            decode(
                "custom_instant_occurred",
                json!({
                    "name": invalid_name,
                    "containing_span_id": null,
                    "severity": "info",
                    "attributes": {}
                })
            )
            .is_err(),
            "{invalid_name}"
        );
    }
    assert!(
        decode(
            "custom_span_finished",
            json!({
                "span_id": "31",
                "outcome": "completed",
                "attributes": {}
            })
        )
        .is_err()
    );
    assert!(
        decode(
            "custom_counter_sampled",
            json!({
                "name": "orders.pending",
                "value": {"type": "integer", "value": "-2"},
                "unit": null,
                "dimensions": {}
            })
        )
        .is_ok()
    );
    for value in [
        json!(1.25),
        json!({"type": "integer", "value": "1.0"}),
        json!({"type": "decimal", "value": "1.250"}),
        json!({"type": "future", "value": "1"}),
    ] {
        assert!(
            decode(
                "custom_counter_sampled",
                json!({
                    "name": "orders.pending",
                    "value": value,
                    "unit": null,
                    "dimensions": {}
                })
            )
            .is_err()
        );
    }
}

fn public_scope() -> DiagnosticScope {
    DiagnosticScope::new(
        Some(RunLocalId::parse("scene-1").unwrap()),
        Some(RunLocalId::parse("actor-1").unwrap()),
        Some(RunLocalId::parse("cue-1").unwrap()),
        None,
        Some(RunLocalId::parse("act-1").unwrap()),
        None,
        Some(SchemaU64::new(2)),
    )
}

fn public_header() -> DiagnosticEventHeader {
    DiagnosticEventHeader::new(
        CanonicalUuid::parse("12345678-1234-4234-9234-123456789abc").unwrap(),
        SchemaU64::new(42),
        ElapsedNs::new(9_007_199_254_740_993),
        public_scope(),
        vec![CausalLink::new(SchemaU64::new(7), CausalRelation::Dispatch)],
    )
    .unwrap()
}

#[test]
fn downstream_can_construct_and_inspect_all_fourteen_validated_variants() {
    let header = public_header();
    let input_tokens = TokenCount::parse("1").unwrap();
    let output_tokens = TokenCount::parse("2").unwrap();
    let total_tokens = TokenCount::parse("3").unwrap();

    let events = [
        DiagnosticEvent::SpanStarted(SpanStarted::new(
            header.clone(),
            SpanStartDetail::RunLifecycle(EmptyDetail::new()),
            None,
        )),
        DiagnosticEvent::SpanFinished(SpanFinished::new(
            header.clone(),
            SchemaU64::new(42),
            SpanOutcome::Completed,
            None,
        )),
        DiagnosticEvent::InstantOccurred(InstantOccurred::new(
            header.clone(),
            InstantDetail::CueAdmitted(EmptyDetail::new()),
            Some(SchemaU64::new(42)),
        )),
        DiagnosticEvent::CounterSampled(CounterSampled::new(
            header.clone(),
            CounterKind::CueActive,
            SchemaU64::new(1),
        )),
        DiagnosticEvent::AgentMessageDelta(AgentMessageDelta::new(
            header.clone(),
            RunLocalId::parse("message-1").unwrap(),
            None,
            "hello".to_owned(),
        )),
        DiagnosticEvent::AgentMessageCompleted(AgentMessageCompleted::new(
            header.clone(),
            RunLocalId::parse("message-1").unwrap(),
            SchemaU64::new(5),
            SchemaU64::new(5),
            false,
        )),
        DiagnosticEvent::AgentPlanSnapshot(AgentPlanSnapshot::new(
            header.clone(),
            vec![PlanEntry::new(
                "inspect".to_owned(),
                PlanEntryPriority::High,
                PlanEntryStatus::InProgress,
            )],
            false,
        )),
        DiagnosticEvent::ContextUsageSampled(
            ContextUsageSampled::new(
                header.clone(),
                Some(SchemaU64::new(1)),
                Some(SchemaU64::new(2)),
                None,
                None,
                ContextSampleOrigin::Provider,
                None,
            )
            .unwrap(),
        ),
        DiagnosticEvent::ActTokenUsageFinalized(
            ActTokenUsageFinalized::new(
                header.clone(),
                UsageAvailability::Available,
                Some(UsageSource::AcpPromptResponseUsage),
                None,
                Some(total_tokens),
                Some(input_tokens),
                Some(output_tokens),
                None,
                None,
                None,
            )
            .unwrap(),
        ),
        DiagnosticEvent::ObservationGap(ObservationGap::new(
            header.clone(),
            "agent_observer".to_owned(),
            None,
            "source_truncated".to_owned(),
            None,
            None,
            None,
            None,
        )),
        DiagnosticEvent::CustomSpanStarted(
            CustomSpanStarted::new(
                header.clone(),
                "orders.select_supplier".to_owned(),
                None,
                DiagnosticAttributes::new(),
            )
            .unwrap(),
        ),
        DiagnosticEvent::CustomSpanFinished(CustomSpanFinished::new(
            header.clone(),
            SchemaU64::new(42),
            SpanOutcome::Completed,
        )),
        DiagnosticEvent::CustomInstantOccurred(
            CustomInstantOccurred::new(
                header.clone(),
                "orders.rejected".to_owned(),
                None,
                None,
                DiagnosticAttributes::new(),
            )
            .unwrap(),
        ),
        DiagnosticEvent::CustomCounterSampled(
            CustomCounterSampled::new(
                header,
                "orders.pending".to_owned(),
                CustomNumber::Integer(CanonicalInteger::parse("1").unwrap()),
                Some("items".to_owned()),
                DiagnosticDimensions::new(),
            )
            .unwrap(),
        ),
    ];

    assert_eq!(events.len(), DiagnosticEventKind::ALL.len());
    for (event, expected_kind) in events.into_iter().zip(DiagnosticEventKind::ALL) {
        assert_eq!(event.kind(), expected_kind);
        assert_eq!(event.header().schema_version(), 1);
        assert_eq!(
            event.header().scope().scene_id().unwrap().as_str(),
            "scene-1"
        );
        assert_eq!(
            event.header().caused_by()[0].relation(),
            CausalRelation::Dispatch
        );
        let encoded = serde_json::to_value(&event).unwrap();
        assert_eq!(
            serde_json::from_value::<DiagnosticEvent>(encoded).unwrap(),
            event
        );
    }

    let context_error = ContextUsageSampled::new(
        public_header(),
        Some(SchemaU64::new(3)),
        Some(SchemaU64::new(2)),
        None,
        None,
        ContextSampleOrigin::Provider,
        None,
    )
    .unwrap_err();
    assert_eq!(
        context_error.message(),
        "context used tokens exceed the context window"
    );

    let header_error = DiagnosticEventHeader::new(
        CanonicalUuid::parse("12345678-1234-4234-9234-123456789abc").unwrap(),
        SchemaU64::new(0),
        ElapsedNs::new(0),
        public_scope(),
        Vec::new(),
    )
    .unwrap_err();
    assert_eq!(
        header_error.message(),
        "diagnostic event sequence must start at one"
    );

    let custom_error = CustomSpanStarted::new(
        public_header(),
        "troupe.reserved".to_owned(),
        None,
        DiagnosticAttributes::new(),
    )
    .unwrap_err();
    assert_eq!(
        custom_error.message(),
        "custom diagnostic payload is invalid"
    );

    let detail_error = DiagnosticComponentFailedDetail::new(
        RunLocalId::parse("sink-1").unwrap(),
        ComponentFailureStage::Enqueue,
        ComponentFailureErrorCode::CallbackRaised,
        None,
    )
    .unwrap_err();
    assert_eq!(
        detail_error.message(),
        "component failure stage and error code do not match"
    );
    let valid_detail = DiagnosticComponentFailedDetail::new(
        RunLocalId::parse("sink-1").unwrap(),
        ComponentFailureStage::Callback,
        ComponentFailureErrorCode::CallbackRaised,
        Some(SchemaU64::new(41)),
    )
    .unwrap();
    assert_eq!(valid_detail.component(), DiagnosticComponent::Sink);
    assert_eq!(valid_detail.component_id().as_str(), "sink-1");
}

#[test]
fn unknown_event_variant_and_unknown_enum_constructor_fail_closed() {
    assert!(decode("future_event", json!({})).is_err());
    kind_wire(ToolKind::Other, "other");
    kind_wire(ToolCallStatus::Failed, "failed");

    let deps = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_owned();
    let core_rlib = fs::read_dir(&deps)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("libtroupe_diagnostics_core-") && name.ends_with(".rlib")
                })
        })
        .max_by_key(|path| {
            path.metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap()
        })
        .expect("diagnostics core rlib is next to the integration test binary");
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let scratch = std::env::temp_dir().join(format!("troupe-c01-{nonce}-{}", std::process::id()));
    fs::create_dir(&scratch).unwrap();

    let compile = |crate_name: &str, source_text: &str| {
        let source = scratch.join(format!("{crate_name}.rs"));
        fs::write(&source, source_text).unwrap();
        Command::new("rustc")
            .arg("--edition=2024")
            .arg(format!("--crate-name={crate_name}"))
            .arg(&source)
            .arg("--extern")
            .arg(format!("troupe_diagnostics_core={}", core_rlib.display()))
            .arg("-L")
            .arg(format!("dependency={}", deps.display()))
            .arg("--out-dir")
            .arg(&scratch)
            .output()
            .unwrap()
    };

    let unknown = compile(
        "c01_unknown_kind",
        "use troupe_diagnostics_core::kinds::SpanKind;\nfn main() { let _ = SpanKind::Unknown; }\n",
    );
    assert!(!unknown.status.success());
    let stderr = String::from_utf8(unknown.stderr).unwrap();
    assert!(
        stderr.contains("no variant or associated item named `Unknown`"),
        "{stderr}"
    );

    let unchecked = compile(
        "c01_unchecked_construction",
        concat!(
            "use troupe_diagnostics_core::{\n",
            "    detail::CanonicalInteger,\n",
            "    event::DiagnosticScope,\n",
            "};\n",
            "fn main() {\n",
            "    let _ = CanonicalInteger(\"01\".to_owned());\n",
            "    let _ = DiagnosticScope {\n",
            "        scene_id: None, actor_id: None, cue_id: None, effect_id: None,\n",
            "        act_id: None, tool_call_id: None, session_generation: None,\n",
            "    };\n",
            "}\n",
        ),
    );
    fs::remove_dir_all(&scratch).unwrap();

    assert!(!unchecked.status.success());
    let stderr = String::from_utf8(unchecked.stderr).unwrap();
    assert!(stderr.contains("private"), "{stderr}");
}
