from __future__ import annotations

import json
import re
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
BRIDGE = ROOT / "rust/src/diagnostic_runtime/observation_bridge.rs"
FIXTURE = ROOT / "tests/fixtures/diagnostics/agent-observations.json"


def _source() -> str:
    return BRIDGE.read_text(encoding="utf-8")


def _fixture() -> dict[str, object]:
    return json.loads(FIXTURE.read_text(encoding="utf-8"))


def _between(source: str, start: str, end: str) -> str:
    start_index = source.index(start)
    return source[start_index : source.index(end, start_index)]


def test_fixture_closes_the_owned_observation_taxonomy() -> None:
    fixture = _fixture()
    assert [row["owner"] for row in fixture["candidate_mappings"]] == [
        "A01",
        "A02",
        "A05",
        "A06",
        "A07",
        "A03",
        "A08",
    ]
    assert fixture["noncanonical_candidates"] == [
        {
            "owner": "A04",
            "input": "AgentTurnUsageCandidate",
            "disposition": "DeferredUsage",
        },
        {
            "owner": "A09",
            "input": "AgentToolPayloadCandidate",
            "disposition": "SinkOnlyPayload",
        },
    ]


def test_session_observations_map_to_the_frozen_lifecycle_facts() -> None:
    source = _source()
    fixture = _fixture()
    expected = {
        "session_opening": ["SpanStartDetail::AgentSessionLifecycle"],
        "session_opening_attempt": ["SpanStartDetail::AgentSessionOpening"],
        "session_ready": [
            "InstantDetail::AgentSessionReady",
            "self.admit_span_finish(",
        ],
        "session_broken": ["InstantDetail::AgentSessionBroken"],
        "session_closing": ["SpanStartDetail::AgentSessionClosing"],
        "session_closed": ["self.admit_span_finish(", "self.admit_span_finish("],
    }

    for row in fixture["session_lifecycle"]:
        name = row["observation"]
        body = _between(source, f"fn {name}(", f"fn {_next_session_function(name)}(")
        cursor = 0
        for token in expected[name]:
            cursor = body.index(token, cursor) + len(token)

    opening_attempt = _between(
        source, "fn session_opening_attempt(", "fn session_ready("
    )
    closing = _between(source, "fn session_closing(", "fn session_closed(")
    assert opening_attempt.count("SpanStartDetail::AgentSessionOpening") == 1
    assert opening_attempt.count("self.admit_span_finish(") == 1
    assert opening_attempt.index("generation <= *previous_generation") < opening_attempt.index(
        "session.opening.take()"
    )
    assert closing.count("SpanStartDetail::AgentSessionClosing") == 1
    assert closing.count("self.admit_span_finish(") == 1
    assert "if let Some((_, opening))" in closing

    ready = _between(source, "fn session_ready(", "fn session_broken(")
    assert ready.index("*opening_generation != generation") < ready.index(
        ".opening\n                .take()"
    )


def _next_session_function(name: str) -> str:
    names = [
        "session_opening",
        "session_opening_attempt",
        "session_ready",
        "session_broken",
        "session_closing",
        "session_closed",
        "observe_candidate",
    ]
    return names[names.index(name) + 1]


def test_session_generation_model_effort_and_error_stay_typed() -> None:
    source = _source()
    assert "session_scope(metadata.context(), Some(generation))?" in source
    assert "generation.map(SchemaU64::new)" in source
    assert "metadata.provider().name().to_owned()" in source
    assert "effective_model(metadata)" in source
    assert "effective_effort(metadata)" in source
    broken = _between(source, "fn session_broken(", "fn session_closing(")
    assert "AgentSessionBrokenDetail::new" in broken
    assert "error_code.to_owned()" in broken


def test_remote_terminal_flushes_all_normalizers_before_b05_closes_turn() -> None:
    source = _source()
    observe = _between(source, "pub(crate) fn observe(", "fn session_opening(")
    terminal_dispatch = _between(
        observe,
        "AgentDiagnosticObservation::TurnTerminal",
        "AgentDiagnosticObservation::Candidate",
    )
    terminal = _between(source, "fn turn_terminal(", "fn flush_messages(")

    assert terminal_dispatch.index(
        "self.turn_terminal(metadata, *outcome)?"
    ) < terminal_dispatch.index(
        "act_producer::observe_agent(observation)"
    )
    assert terminal.index("messages") < terminal.index("thinking") < terminal.index("tools")
    assert terminal.count(".turn_terminal(") == 3
    assert ".turn_terminal(elapsed_ns, false)" in terminal
    assert "pending_rejection_sequence.is_some()" in terminal


def test_terminal_state_is_removed_only_after_every_projection_succeeds() -> None:
    terminal = _between(_source(), "fn turn_terminal(", "fn flush_messages(")
    cleanup = "state.turns.remove(act_id)"

    assert terminal.count(cleanup) == 1
    cleanup_index = terminal.index(cleanup)
    for projection in (
        "self.project_message_candidates(&mut state, act_id, messages)?",
        "self.project_thinking_candidates(&mut state, act_id, thinking, Some(outcome))?",
        "self.project_tool_candidates(&mut state, act_id, tools)?",
    ):
        assert terminal.index(projection) < cleanup_index


def test_terminal_without_candidate_state_is_a_noop_before_clock_or_lineage() -> None:
    terminal = _between(_source(), "fn turn_terminal(", "fn flush_messages(")
    no_state = _between(
        terminal,
        "let mut state = lock(&self.state);",
        "let elapsed_ns = self.now()?.get();",
    )

    assert "if !state.turns.contains_key(act_id)" in no_state
    assert "return Ok(())" in no_state
    assert terminal.index("return Ok(())") < terminal.index("let lineage = act_lineage(metadata)?")


def test_all_owned_candidate_families_use_one_canonical_hub_path() -> None:
    source = _source()
    fixture = _fixture()
    candidate_dispatch = _between(source, "fn observe_candidate(", "fn message_observed(")

    for row in fixture["candidate_mappings"]:
        assert f"downcast_ref::<{row['input']}>()" in candidate_dispatch
        if row["normalizer"] is not None:
            assert row["normalizer"] in source
        for fact in row["canonical"]:
            canonical_type = fact.split(":", 1)[0]
            assert canonical_type in source

    assert "ProductionDiagnosticHub" in source
    assert "SinkOnlyDiagnosticHub" in source
    assert "self.admission.admit(" in source
    assert "ProductionAdmission" in source
    assert "SinkOnlyAdmission" in source


def test_sequence_allocation_happens_after_coalescing_and_normalization() -> None:
    source = _source()
    message = _between(source, "fn message_observed(", "fn thinking_observed(")
    thinking = _between(source, "fn thinking_observed(", "fn tool_observed(")
    tool = _between(source, "fn tool_observed(", "fn plan_observed(")

    assert message.index(".observe_chunk(observation, elapsed_ns)") < message.index(
        "project_message_candidates"
    )
    assert thinking.index(".observe_observation(observation, elapsed_ns)") < thinking.index(
        "project_thinking_candidates"
    )
    assert tool.index(".observe(observation, elapsed_ns)") < tool.index(
        "project_tool_candidates"
    )
    assert "identity.sequence()" in source
    assert "DiagnosticEventHeader::new" in source


def test_result_rejections_are_strictly_paired_and_monotonic() -> None:
    source = _source()
    result = _between(source, "fn result_observed(", "fn turn_terminal(")

    assert "turn.pending_rejection_sequence = Some(sequence)" in result
    assert re.search(r"last_rejection_count\s*\.checked_add\(1\)", result)
    assert "counter.value() != expected" in result
    assert re.search(r"pending_rejection_sequence\s*\.take\(\)", result)
    assert result.index("self.admit_instant(") < result.index("self.admit_counter(")
    assert "lineage.act_scope().clone()" in result
    assert "counter.counter_kind() != CounterKind::ResultValidationRejections" in result
    result_detail = _between(source, "fn result_instant(", "fn tool_detail(")
    assert "InstantKind::ResultRejected" in result_detail
    assert "InstantDetail::ResultRejected" in result_detail


def test_cost_uses_a_carried_forward_context_sample_with_a_causal_link() -> None:
    source = _source()
    context = _between(source, "fn emit_context_sample(", "fn cost_observed(")
    cost = _between(source, "fn cost_observed(", "fn result_observed(")

    assert "state.latest_context.insert(" in context
    assert "observed_elapsed_ns" in context
    assert "ContextSampleOrigin::Provider" in context
    assert "ContextSampleOrigin::CarriedForward" in cost
    assert "Some(ElapsedNs::new(context.observed_elapsed_ns))" in cost
    assert "follows_from(previous)" in cost
    assert "subscriber" not in context
    assert "subscriber" not in cost


def test_a04_and_a09_are_typed_noncanonical_dispositions() -> None:
    source = _source()
    dispatch = _between(source, "fn observe_candidate(", "fn message_observed(")

    assert dispatch.index("AgentTurnUsageCandidate") < dispatch.index("DeferredUsage")
    assert dispatch.index("AgentToolPayloadCandidate") < dispatch.index("SinkOnlyPayload")
    assert ".usage()" not in source
    assert ".payload()" not in source
    assert "ActTokenUsageFinalized" not in source
    assert "SinkOnlyJsonValue" not in source


def test_per_act_subscriber_lookup_never_applies_to_session_scope() -> None:
    source = _source()
    admission = _between(source, "fn admit_event<F>(", "fn admit_span_start(")

    assert "scope.act_id().and_then" in admission
    assert "lookup.subscriber_for(act_id.as_str())" in admission
    assert "production_with_subscribers" in source
    assert "trait ActObservationSubscriberLookup" in source
    assert "session_scope(" in source
    assert "None,\n            generation.map(SchemaU64::new)" in source


def test_global_bridge_never_reads_forbidden_content_fields() -> None:
    source = _source()
    for forbidden in _fixture()["forbidden_global_content"]:
        assert forbidden not in source
    for forbidden in ("serde_json", "traceback", "credential", "result.value", "thought.text"):
        assert forbidden not in source
    assert "CANDIDATE_UNSUPPORTED" in source
    assert "Err(CANDIDATE_UNSUPPORTED)" in source
