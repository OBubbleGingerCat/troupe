use std::{fs, path::PathBuf};

use agent_client_protocol::schema::v1::{
    Plan, SessionUpdate, ToolCall, ToolCallUpdate, ToolCallUpdateFields,
};
use serde_json::{Value, json};
use troupe_agent_runtime::{
    AgentDiagnosticCandidate,
    diagnostics::payload::{
        ACT_TOOL_PAYLOAD_MAX_BYTES, AGENT_TOOL_PAYLOAD_CANDIDATE_KIND, AgentToolPayloadActBudget,
        AgentToolPayloadCandidate, SinkOnlyToolPayload, TOOL_PAYLOAD_MAX_DEPTH,
        TOOL_PAYLOAD_MAX_NODES, TOOL_PAYLOAD_SNAPSHOT_MAX_BYTES, ToolPayloadCapturePolicy,
        ToolPayloadOmissionReason, ToolPayloadSource,
    },
};

fn capture(update: &SessionUpdate, input: bool, output: bool) -> Option<SinkOnlyToolPayload> {
    SinkOnlyToolPayload::from_acp(update, ToolPayloadCapturePolicy::new(input, output))
}

fn input_update(value: Value) -> SessionUpdate {
    SessionUpdate::ToolCall(ToolCall::new("tool-input", "input").raw_input(value))
}

fn output_update(value: Value) -> SessionUpdate {
    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        "tool-output",
        ToolCallUpdateFields::new().raw_output(value),
    ))
}

fn nested_array(depth: usize) -> Value {
    let mut value = Value::Null;
    for _ in 1..depth {
        value = Value::Array(vec![value]);
    }
    value
}

fn exact_input_update() -> SessionUpdate {
    input_update(Value::String(
        "x".repeat(TOOL_PAYLOAD_SNAPSHOT_MAX_BYTES - 2),
    ))
}

fn output_string_update(encoded_bytes: usize) -> SessionUpdate {
    let empty = output_update(Value::String(String::new()));
    let structural_bytes = capture(&empty, false, true)
        .unwrap()
        .output()
        .unwrap()
        .canonical_bytes();
    output_update(Value::String("x".repeat(encoded_bytes - structural_bytes)))
}

#[test]
fn bind_policy_is_closed_copyable_and_disabled_by_default() {
    let default = ToolPayloadCapturePolicy::default();
    assert!(!default.capture_input());
    assert!(!default.capture_output());
    assert!(!default.captures_payload());

    let input = ToolPayloadCapturePolicy::new(true, false);
    let output = ToolPayloadCapturePolicy::new(false, true);
    let both = ToolPayloadCapturePolicy::new(true, true);
    assert!(input.capture_input() && !input.capture_output());
    assert!(!output.capture_input() && output.capture_output());
    assert!(both.capture_input() && both.capture_output());
}

#[test]
fn stable_acp_selection_excludes_envelopes_and_protocol_metadata_without_redaction() {
    let update: SessionUpdate = serde_json::from_value(json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "tool-sensitive",
        "title": "envelope-title-must-not-be-captured",
        "kind": "read",
        "status": "in_progress",
        "rawInput": {
            "_meta": {"caller_owned": "preserve-me"},
            "api_key": "input-secret",
            "password": "input-password"
        },
        "rawOutput": {
            "credential": "output-secret",
            "token": "output-token"
        },
        "content": [{
            "type": "content",
            "content": {
                "type": "text",
                "text": "visible-content",
                "_meta": {"nested_protocol_secret": "drop-me"}
            },
            "_meta": {"content_protocol_secret": "drop-me-too"}
        }],
        "locations": [{
            "path": "/workspace/visible.rs",
            "line": 17,
            "_meta": {"location_protocol_secret": "drop-location-meta"}
        }],
        "_meta": {"envelope_secret": "drop-envelope-meta"}
    }))
    .unwrap();

    let payload = capture(&update, true, true).unwrap();
    assert_eq!(payload.tool_call_id(), "tool-sensitive");
    assert_eq!(payload.source(), ToolPayloadSource::Started);

    let input = payload.input().unwrap();
    assert_eq!(
        input.raw_input().unwrap().as_json(),
        &json!({
            "_meta": {"caller_owned": "preserve-me"},
            "api_key": "input-secret",
            "password": "input-password"
        })
    );
    assert!(!input.truncated());

    let output = payload.output().unwrap();
    assert_eq!(
        output.raw_output().unwrap().as_json(),
        &json!({
            "credential": "output-secret",
            "token": "output-token"
        })
    );
    assert_eq!(output.content().len(), 1);
    assert_eq!(
        output.content()[0].as_json(),
        &json!({
            "content": {"text": "visible-content", "type": "text"},
            "type": "content"
        })
    );
    let content = serde_json::to_string(output.content()[0].as_json()).unwrap();
    assert!(content.contains("visible-content"));
    assert!(!content.contains("protocol_secret"));
    assert!(!content.contains("_meta"));
    assert_eq!(output.locations()[0].path(), "/workspace/visible.rs");
    assert_eq!(output.locations()[0].line(), Some(17));
    assert!(!output.truncated());

    let debug = format!("{payload:?}");
    for excluded in [
        "input-secret",
        "input-password",
        "output-secret",
        "output-token",
        "visible-content",
    ] {
        assert!(!debug.contains(excluded));
    }
}

#[test]
fn direction_policy_and_source_presence_are_enforced_before_retention() {
    let update = SessionUpdate::ToolCall(
        ToolCall::new("tool-directions", "directions")
            .raw_input(json!({"input": true}))
            .raw_output(json!({"output": true})),
    );

    let input_only = capture(&update, true, false).unwrap();
    assert!(input_only.input().is_some());
    assert!(input_only.output().is_none());

    let output_only = capture(&update, false, true).unwrap();
    assert!(output_only.input().is_none());
    assert!(output_only.output().is_some());

    assert!(capture(&update, false, false).is_none());
    assert!(capture(&SessionUpdate::Plan(Plan::new(Vec::new())), true, true).is_none());

    let metadata_only = SessionUpdate::ToolCall(ToolCall::new("tool-empty", "empty"));
    assert!(capture(&metadata_only, true, true).is_none());

    let explicit_empty_output = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        "tool-empty-output",
        ToolCallUpdateFields::new()
            .content(Vec::new())
            .locations(Vec::new()),
    ));
    let payload = capture(&explicit_empty_output, false, true).unwrap();
    assert_eq!(payload.source(), ToolPayloadSource::Updated);
    let output = payload.output().unwrap();
    assert!(output.content().is_empty());
    assert!(output.locations().is_empty());
    assert!(!output.truncated());
}

#[test]
fn typed_depth_equal_is_accepted_and_one_over_is_atomically_omitted() {
    let equal = input_update(nested_array(TOOL_PAYLOAD_MAX_DEPTH));
    let payload = capture(&equal, true, false).unwrap();
    let input = payload.input().unwrap();
    assert!(input.raw_input().is_some());
    assert!(!input.truncated());

    let over = input_update(nested_array(TOOL_PAYLOAD_MAX_DEPTH + 1));
    let payload = capture(&over, true, false).unwrap();
    let input = payload.input().unwrap();
    assert!(input.raw_input().is_none());
    assert!(input.truncated());
    assert_eq!(
        input.omission_reason(),
        Some(ToolPayloadOmissionReason::DepthLimit)
    );
    assert_eq!(input.canonical_bytes(), 0);

    let equal_output = output_update(nested_array(TOOL_PAYLOAD_MAX_DEPTH - 1));
    let payload = capture(&equal_output, false, true).unwrap();
    assert!(payload.output().unwrap().raw_output().is_some());

    let over_output = output_update(nested_array(TOOL_PAYLOAD_MAX_DEPTH));
    let payload = capture(&over_output, false, true).unwrap();
    let output = payload.output().unwrap();
    assert!(output.raw_output().is_none());
    assert_eq!(
        output.omission_reason(),
        Some(ToolPayloadOmissionReason::DepthLimit)
    );
}

#[test]
fn typed_node_equal_is_accepted_and_one_over_is_atomically_omitted() {
    let equal_nodes = Value::Array(
        (0..TOOL_PAYLOAD_MAX_NODES - 1)
            .map(|_| Value::Array(Vec::new()))
            .collect(),
    );
    let equal = capture(&input_update(equal_nodes), true, false).unwrap();
    let input = equal.input().unwrap();
    assert!(input.raw_input().is_some());
    assert!(!input.truncated());

    let over_nodes = Value::Array(
        (0..TOOL_PAYLOAD_MAX_NODES)
            .map(|_| Value::Array(Vec::new()))
            .collect(),
    );
    let over = capture(&input_update(over_nodes), true, false).unwrap();
    let input = over.input().unwrap();
    assert!(input.raw_input().is_none());
    assert_eq!(
        input.omission_reason(),
        Some(ToolPayloadOmissionReason::NodeLimit)
    );

    let equal_output_nodes = Value::Array(
        (0..TOOL_PAYLOAD_MAX_NODES - 2)
            .map(|_| Value::Array(Vec::new()))
            .collect(),
    );
    let equal = capture(&output_update(equal_output_nodes), false, true).unwrap();
    assert!(equal.output().unwrap().raw_output().is_some());

    let over_output_nodes = Value::Array(
        (0..TOOL_PAYLOAD_MAX_NODES - 1)
            .map(|_| Value::Array(Vec::new()))
            .collect(),
    );
    let over = capture(&output_update(over_output_nodes), false, true).unwrap();
    let output = over.output().unwrap();
    assert!(output.raw_output().is_none());
    assert_eq!(
        output.omission_reason(),
        Some(ToolPayloadOmissionReason::NodeLimit)
    );
}

#[test]
fn input_snapshot_equal_is_accepted_and_one_byte_over_has_no_partial_json() {
    let exact = capture(&exact_input_update(), true, false).unwrap();
    let input = exact.input().unwrap();
    assert_eq!(input.canonical_bytes(), TOOL_PAYLOAD_SNAPSHOT_MAX_BYTES);
    assert_eq!(
        serde_json::to_vec(input.raw_input().unwrap().as_json())
            .unwrap()
            .len(),
        TOOL_PAYLOAD_SNAPSHOT_MAX_BYTES
    );
    assert!(!input.truncated());

    let over = input_update(Value::String(
        "x".repeat(TOOL_PAYLOAD_SNAPSHOT_MAX_BYTES - 1),
    ));
    let over = capture(&over, true, false).unwrap();
    let input = over.input().unwrap();
    assert!(input.raw_input().is_none());
    assert_eq!(input.canonical_bytes(), 0);
    assert_eq!(
        input.omission_reason(),
        Some(ToolPayloadOmissionReason::SnapshotByteLimit)
    );
}

#[test]
fn output_snapshot_equal_is_accepted_and_one_byte_over_omits_every_field() {
    let exact = output_string_update(TOOL_PAYLOAD_SNAPSHOT_MAX_BYTES);
    let exact = capture(&exact, false, true).unwrap();
    let output = exact.output().unwrap();
    assert_eq!(output.canonical_bytes(), TOOL_PAYLOAD_SNAPSHOT_MAX_BYTES);
    assert!(output.raw_output().is_some());
    assert!(!output.truncated());

    let over = output_string_update(TOOL_PAYLOAD_SNAPSHOT_MAX_BYTES + 1);
    let over = capture(&over, false, true).unwrap();
    let output = over.output().unwrap();
    assert!(output.raw_output().is_none());
    assert!(output.content().is_empty());
    assert!(output.locations().is_empty());
    assert_eq!(output.canonical_bytes(), 0);
    assert_eq!(
        output.omission_reason(),
        Some(ToolPayloadOmissionReason::SnapshotByteLimit)
    );
}

#[test]
fn per_act_equal_is_accepted_and_next_snapshot_is_atomically_omitted() {
    let update = exact_input_update();
    let snapshots_at_limit = ACT_TOOL_PAYLOAD_MAX_BYTES / TOOL_PAYLOAD_SNAPSHOT_MAX_BYTES;
    assert_eq!(snapshots_at_limit, 16);

    let mut budget = AgentToolPayloadActBudget::new();
    for _ in 0..snapshots_at_limit {
        let mut payload = capture(&update, true, false).unwrap();
        payload.apply_act_budget(&mut budget);
        assert!(payload.act_budget_applied());
        assert!(payload.input().unwrap().raw_input().is_some());

        let accepted = budget.accepted_bytes();
        payload.apply_act_budget(&mut budget);
        assert_eq!(budget.accepted_bytes(), accepted);
    }
    assert_eq!(budget.accepted_bytes(), ACT_TOOL_PAYLOAD_MAX_BYTES);
    assert_eq!(budget.remaining_bytes(), 0);
    assert!(!budget.truncated());

    let mut over = capture(&update, true, false).unwrap();
    over.apply_act_budget(&mut budget);
    let input = over.input().unwrap();
    assert!(input.raw_input().is_none());
    assert_eq!(input.canonical_bytes(), 0);
    assert_eq!(
        input.omission_reason(),
        Some(ToolPayloadOmissionReason::ActByteLimit)
    );
    assert_eq!(budget.accepted_bytes(), ACT_TOOL_PAYLOAD_MAX_BYTES);
    assert!(budget.truncated());
}

#[test]
fn candidate_is_typed_sink_only_and_source_checks_precede_payload_selection() {
    fn assert_candidate<T: AgentDiagnosticCandidate>() {}
    assert_candidate::<AgentToolPayloadCandidate>();
    assert_eq!(
        AGENT_TOOL_PAYLOAD_CANDIDATE_KIND,
        "agent_tool_payload_sidecar"
    );
    assert_eq!(ToolPayloadSource::Started.as_str(), "started");
    assert_eq!(ToolPayloadSource::Updated.as_str(), "updated");
    assert_eq!(
        ToolPayloadOmissionReason::ActByteLimit.as_str(),
        "act_byte_limit"
    );

    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = fs::read_to_string(crate_root.join("src/diagnostics/payload.rs")).unwrap();
    let context_guard = source
        .find("let Some(turn) = context.turn else")
        .expect("turn context guard");
    let observe_source = &source[context_guard..];
    let policy_guard = context_guard
        + observe_source
            .find("if !policy.captures_payload()")
            .expect("capture policy guard");
    let source_selection = source
        .rfind("SinkOnlyToolPayload::from_acp(update, policy)")
        .expect("ACP source selection");
    assert!(context_guard < policy_guard && policy_guard < source_selection);

    for required in [
        "turn.runtime_metadata().cloned()",
        "AgentDiagnosticObservation::Candidate",
        "AgentToolPayloadCandidate",
        "is_sink_only",
        "tool_content_json",
        "TOOL_PAYLOAD_MAX_DEPTH: usize = 32",
        "TOOL_PAYLOAD_MAX_NODES: usize = 65_536",
    ] {
        assert!(
            source.contains(required),
            "missing payload contract: {required}"
        );
    }
    for forbidden in [
        "troupe_diagnostics_core",
        "troupe_diagnostics_runtime",
        "troupe_diagnostics_perfetto",
        "DiagnosticEvent",
        "Serialize for SinkOnlyJsonValue",
        "redact_credential",
        "scan_credential",
    ] {
        assert!(
            !source.contains(forbidden),
            "sink-only payload must not contain {forbidden}"
        );
    }
}
