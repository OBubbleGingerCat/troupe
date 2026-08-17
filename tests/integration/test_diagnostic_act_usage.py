from __future__ import annotations

from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
USAGE = ROOT / "rust/src/diagnostic_runtime/usage_finalization.rs"
ACT = ROOT / "rust/src/diagnostic_runtime/act_producer.rs"
BRIDGE = ROOT / "rust/src/diagnostic_runtime/observation_bridge.rs"
A04 = ROOT / "rust/crates/troupe-agent-runtime/src/diagnostics/usage.rs"
AGENT_SESSION = ROOT / "rust/crates/troupe-agent-runtime/src/diagnostics/session.rs"
S11 = ROOT / "rust/crates/troupe-diagnostics-runtime/src/store/projector/usage.rs"


def _source(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def _between(source: str, start: str, end: str) -> str:
    start_index = source.index(start)
    end_index = source.index(end, start_index)
    return source[start_index:end_index]


def test_b17_is_the_only_native_terminal_usage_event_owner() -> None:
    owners: list[Path] = []
    for path in sorted((ROOT / "rust/src").rglob("*.rs")):
        if "ActTokenUsageFinalized::new(" in _source(path):
            owners.append(path.relative_to(ROOT))

    assert owners == [Path("rust/src/diagnostic_runtime/usage_finalization.rs")]
    source = _source(USAGE)
    assert source.count("ActTokenUsageFinalized::new(") == 1
    assert source.count("DiagnosticEvent::ActTokenUsageFinalized(") == 1


def test_runtime_router_uses_the_executable_machine_and_b05_ack() -> None:
    source = _source(USAGE)
    act = _source(ACT)
    drive = _between(source, "fn drive_entry(", "fn slot_snapshot(")
    effects = _between(source, "struct RuntimeEffects", "fn admit_usage(")
    router_drive = _between(
        source,
        "fn drive(&self, act_id:",
        "fn record_invariant_error(",
    )
    finish = _between(act, "fn maybe_finish(&self", "fn fail_diagnostic(")

    assert "machine: FinalizationMachine" in source
    assert "entry.machine.drive(snapshot, &mut effects)" in drive
    assert "impl FinalizationEffects for RuntimeEffects" in effects
    assert "admit_usage(" in effects
    assert "slot.acknowledge(sequence)" in effects
    assert router_drive.index("state.acts.remove(act_id)") < router_drive.index(
        "act_producer::usage_finalized(ack)"
    )
    assert finish.index("Some(usage_sequence)") < finish.index("finish_span_with_causes(")
    assert "follows_from(usage_sequence)" in finish
    assert "ActTokenUsageFinalized" not in act


def test_b05_slot_and_b12_bridge_share_the_b17_linearization() -> None:
    source = _source(USAGE)
    act = _source(ACT)
    observe = _between(
        source,
        "pub(crate) fn observe(",
        "impl AgentDiagnosticDestination",
    )

    assert "UsageFinalizationBridge::new(" in source
    assert "register_slot," in source
    assert "settlement_ready," in source
    assert "producer.take_usage_slot()" in act
    assert "bridge.register_slot(slot)" in act
    assert "producer.take_settlement_notification()" in act
    assert "bridge.settlement_ready(producer.act_id.as_str())" in act
    assert observe.index("self.canonical.observe(observation)?") < observe.index(
        "observe_candidate(candidate)"
    )
    assert "ObservationDisposition::DeferredUsage" in observe


def test_turn_terminal_updates_the_slot_before_a04_publishes_its_candidate() -> None:
    bridge = _source(BRIDGE)
    session = _source(AGENT_SESSION)
    canonical_terminal = _between(
        bridge,
        "AgentDiagnosticObservation::TurnTerminal {",
        "AgentDiagnosticObservation::Candidate(candidate)",
    )
    agent_terminal = _between(
        session,
        "pub(crate) fn observe_turn_terminal(",
        "#[cfg(test)]\nmod tests",
    )

    assert canonical_terminal.index("self.turn_terminal(metadata, *outcome)?") < (
        canonical_terminal.index("act_producer::observe_agent(observation)")
    )
    assert agent_terminal.index(
        "observer.observe(AgentDiagnosticObservation::TurnTerminal"
    ) < agent_terminal.index("super::usage::observe_turn_terminal(context, observation)")


def test_a04_projection_has_six_exact_fields_and_no_derived_accounting() -> None:
    source = _source(USAGE)
    projection = _between(source, "fn from_candidate(", "fn unavailable(")
    constructor = _between(source, "fn admit_usage(", "pub(crate) enum UsageObservationDisposition")
    shape = _between(source, "struct FinalUsage", "impl FinalUsage")

    for field in (
        "provider_total_tokens",
        "input_tokens",
        "output_tokens",
        "thought_tokens",
        "cached_read_tokens",
        "cached_write_tokens",
    ):
        assert f"usage.{field}().cloned()" in projection
        assert f"usage.{field}," in constructor
    assert shape.count("Option<TokenCount>") == 6
    assert "availability: usage.availability()" in projection
    assert "source: usage.source()" in projection
    assert "unavailable_reason: usage.unavailable_reason()" in projection

    for forbidden in (
        "tokenizer",
        "context_used_tokens",
        "context_window_tokens",
        "carried_forward",
        "session_counter",
        "context_delta",
        "u64::try_from",
        "parse::<u64>",
    ):
        assert forbidden not in source.lower()
    assert "SchemaU64" not in projection
    assert ".get()" not in projection


def test_a04_is_deferred_and_s11_act_totals_read_only_the_terminal_fact() -> None:
    bridge = _source(BRIDGE)
    a04 = _source(A04)
    projector = _source(S11)
    candidate = _between(projector, "fn candidate_for_event(", "fn validate_position(")
    context_branch = _between(
        candidate,
        "DiagnosticEvent::ContextUsageSampled(context)",
        "DiagnosticEvent::ActTokenUsageFinalized(event)",
    )

    assert "pub struct AgentTurnUsageCandidate" in a04
    assert "AgentTurnUsageCandidate" in bridge
    assert "ObservationDisposition::DeferredUsage" in bridge
    assert "ActTokenUsageFinalized" not in bridge
    assert candidate.count("DiagnosticEvent::ActTokenUsageFinalized") == 1
    assert "ProjectedActUsage::from_event(event, act_id)" in candidate
    assert "aggregate" not in context_branch
    assert "AgentTurnUsageCandidate" not in candidate
