from __future__ import annotations

from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
PRODUCER = ROOT / "rust/src/diagnostic_runtime/act_producer.rs"
ACT_CALL = ROOT / "rust/src/act_call.rs"
AGENT_TURN = ROOT / "rust/crates/troupe-agent-runtime/src/session/turn.rs"


def _between(source: str, start: str, end: str) -> str:
    start_index = source.index(start)
    end_index = source.index(end, start_index)
    return source[start_index:end_index]


def test_act_identity_and_lifecycle_begin_only_after_real_actor_admission() -> None:
    call = ACT_CALL.read_text(encoding="utf-8")
    producer = PRODUCER.read_text(encoding="utf-8")
    driver = _between(call, "fn start_driver(&mut self", "fn fresh_waiter(")

    assert driver.index(".try_claim_admission()") < driver.index(
        "control.install_admission(admission)"
    )
    assert driver.index("control.install_admission(admission)") < driver.index(
        "act_producer::admitted(&binding, &cued, &control)"
    )
    assert driver.index("act_producer::admitted(&binding, &cued, &control)") < driver.index(
        "sink_binding::admit_act"
    )

    prepare = _between(producer, "fn prepare(", "fn start(&self)")
    assert prepare.index("diagnostic_session_metadata()") < prepare.index(
        "next_act_identity()?"
    )
    assert prepare.index("next_act_identity()?") < prepare.index(
        "UsageFinalizationIdentity::new"
    )
    assert 'format!("act-{value}")' in producer
    assert "static NEXT_ACT_ID: AtomicU64" in producer


def test_caller_and_remote_turn_are_independent_act_children() -> None:
    source = PRODUCER.read_text(encoding="utf-8")
    start = _between(source, "fn start(&self)", "fn driver_started(&self)")
    submitted = _between(
        source,
        "fn prompt_submitted(&self",
        "fn supervisor_handoff(&self",
    )

    lifecycle = start.index("SpanStartDetail::ActLifecycle")
    caller = start.index("SpanStartDetail::ActCaller")
    admitted = start.index("InstantDetail::ActAdmitted")
    assert lifecycle < caller < admitted
    assert "SpanStartDetail::ActLifecycle(session_detail(&self.session)),\n                None" in start
    assert "SpanStartDetail::ActCaller(EmptyDetail::new()),\n                Some(act_span_id)" in start
    assert "SpanStartDetail::AgentTurn(turn_session_detail(metadata)),\n                Some(act_span_id)" in submitted
    assert "Some(caller_span_id)" not in submitted
    assert "containing_span_id" not in prepare_parentage(source)


def prepare_parentage(source: str) -> str:
    prepare = _between(source, "fn prepare(", "fn start(&self)")
    return _between(prepare, "let act_scope", "Ok(Some(Self")


def test_real_boundaries_drive_the_closed_act_taxonomy() -> None:
    source = PRODUCER.read_text(encoding="utf-8")
    call = ACT_CALL.read_text(encoding="utf-8")
    agent_turn = AGENT_TURN.read_text(encoding="utf-8")

    for required in (
        "InstantDetail::ActAdmitted",
        "InstantDetail::ActWaitingReady",
        "InstantDetail::ActPromptSubmitted",
        "InstantDetail::ActCancelRequested",
        "InstantDetail::ActSupervisorHandoff",
        "InstantDetail::AgentTurnActivity",
        "InstantDetail::AgentTurnTerminal",
        "InstantDetail::AgentTurnSettled",
    ):
        assert required in source

    driver = _between(call, "fn start_driver(&mut self", "fn fresh_waiter(")
    assert driver.index("self.phase = ActCallPhase::Running") < driver.index(
        "ActHook::DriverStarted"
    )
    request_cancel = _between(call, "fn request_cancel(&self)", "fn cancel_signal(")
    assert request_cancel.index("ActHook::CancelRequested") < request_cancel.index(
        "control.request_cancel()"
    )
    assert agent_turn.index("state.phase = AgentTurnControlPhase::Submitted") < agent_turn.index(
        "observe_turn_submitted(state.diagnostic_context.as_ref())"
    )
    assert agent_turn.index("observe_turn_supervisor_handoff(context.as_ref())") < agent_turn.index(
        "self.supervisor_requested.cancel()"
    )

    bridge = _between(source, "pub(super) fn observe_agent(", "pub(super) fn diagnostic_identity(")
    for observation in (
        "AgentDiagnosticObservation::TurnSubmitted",
        "AgentDiagnosticObservation::TurnSupervisorHandoff",
        "AgentDiagnosticObservation::TurnTerminal",
    ):
        assert observation in bridge
    assert "AgentDiagnosticObservation::Candidate(_) => false" in bridge


def test_remote_turn_active_counter_has_strict_start_and_finish_cardinality() -> None:
    source = PRODUCER.read_text(encoding="utf-8")
    submitted = _between(
        source,
        "fn prompt_submitted(&self",
        "fn supervisor_handoff(&self",
    )
    terminal = _between(source, "fn turn_terminal(", "fn caller_finished(&self")

    turn_start = submitted.index("start_span_with_causes(")
    active_one = submitted.index("CounterKind::AgentTurnActive")
    activity = submitted.index("InstantDetail::AgentTurnActivity")
    assert turn_start < active_one < activity
    assert "SchemaU64::new(1)" in submitted

    terminal_instant = terminal.index("InstantDetail::AgentTurnTerminal")
    turn_finish = terminal.index("finish_span_with_causes(")
    active_zero = terminal.index("CounterKind::AgentTurnActive")
    assert terminal_instant < turn_finish < active_zero
    assert "SchemaU64::new(0)" in terminal
    assert source.count("CounterKind::AgentTurnActive") == 2

    not_submitted = terminal.index(
        "if settlement == UsageFinalizationSettlement::NotSubmitted"
    )
    remote_start_required = terminal.index("let (Some(turn_span_id)")
    assert not_submitted < remote_start_required < terminal_instant
    assert "state.prompt_sequence.is_some() || state.turn_span_id.is_some()" in terminal


def test_terminal_and_authoritative_settlement_close_inside_the_turn_span() -> None:
    source = PRODUCER.read_text(encoding="utf-8")
    terminal = _between(source, "fn turn_terminal(", "fn caller_finished(&self")

    terminal_instant = terminal.index("InstantDetail::AgentTurnTerminal")
    authoritative = terminal.index(
        "settlement == UsageFinalizationSettlement::Authoritative"
    )
    settled_instant = terminal.index("InstantDetail::AgentTurnSettled")
    finish = terminal.index("finish_span_with_causes(")
    inactive = terminal.index("SchemaU64::new(0)")
    assert terminal_instant < authoritative < settled_instant < finish < inactive
    assert "UsageFinalizationSettlement::Unknown" in source
    assert "else {\n                terminal_sequence\n            }" in terminal


def test_caller_cancellation_and_remote_settlement_remain_independent() -> None:
    source = PRODUCER.read_text(encoding="utf-8")
    handoff = _between(source, "fn supervisor_handoff(&self", "fn turn_terminal(")
    caller = _between(source, "fn caller_finished(&self", "fn take_usage_slot(")
    finish = _between(source, "fn maybe_finish(&self", "fn fail_diagnostic(")

    assert "state.cancel_sequence.unwrap_or(activity_sequence)" in handoff
    assert "CausalRelation::Handoff" in handoff
    assert "state.caller_terminal = Some(terminal)" in caller
    assert "state.settlement = Some(settlement)" in caller
    assert "state.turn_terminal" in finish
    assert "state.caller_terminal" in finish
    assert "act_terminal(caller, remote)" in finish
    assert "remote.outcome != SpanOutcome::Completed" in source
    assert source.index("remote.outcome != SpanOutcome::Completed") < source.index(
        "caller.outcome == SpanOutcome::Failed"
    )
    assert "else if let Some(remote) = remote" in source


def test_act_finish_waits_for_linear_usage_ack_and_never_emits_usage() -> None:
    source = PRODUCER.read_text(encoding="utf-8")
    usage_ack = _between(source, "fn usage_finalized(&self", "fn snapshot(&self")
    finish = _between(source, "fn maybe_finish(&self", "fn fail_diagnostic(")

    assert "state.usage_slot.take()" in source
    assert "state.usage_slot.is_some()" in usage_ack
    assert "state.usage_ack_sequence.is_some()" in usage_ack
    assert "state.settlement.is_none()" in usage_ack
    assert "Some(usage_sequence)" in finish
    assert "state.usage_ack_sequence" in finish
    assert finish.index("Some(usage_sequence)") < finish.index("finish_span_with_causes(")
    assert "follows_from(usage_sequence)" in finish
    assert "ActTokenUsageFinalized" not in source
    assert "AgentTurnUsageCandidate" not in source


def test_usage_slot_handoff_covers_pre_submission_without_lock_reentry() -> None:
    source = PRODUCER.read_text(encoding="utf-8")
    admitted = _between(source, "pub(super) fn admitted(", "pub(super) fn observe(")
    caller = _between(
        source,
        "pub(super) fn caller_finished(",
        "pub(super) fn observe_agent(",
    )
    observations = _between(
        source,
        "pub(super) fn observe_agent(",
        "pub(super) fn diagnostic_identity(",
    )
    register = _between(source, "fn register_usage_slot(", "fn notify_settlement_ready(")
    notify = _between(source, "fn notify_settlement_ready(", "fn next_act_identity(")

    assert admitted.index("producer.start()") < admitted.index("register_usage_slot(&producer)")
    assert "producer.take_usage_slot()" in register
    assert "bridge.register_slot(slot)" in register
    assert caller.index("producer.caller_finished") < caller.index(
        "notify_settlement_ready(&producer)"
    )
    assert observations.index("producer.turn_terminal") < observations.index(
        "notify_settlement_ready(&producer)"
    )
    assert "producer.take_settlement_notification()" in notify
    assert "bridge.settlement_ready(producer.act_id.as_str())" in notify
    assert "lock(&producer.state)" not in register
    assert "lock(&producer.state)" not in notify


def test_act_scope_is_canonical_and_does_not_reparse_python_payloads() -> None:
    source = PRODUCER.read_text(encoding="utf-8")
    scope = _between(source, "fn scope_for_act(", "fn scope_for_turn(")

    for required in (
        "cue_scope",
        ".scene_id()",
        ".actor_id()",
        ".cue_id()",
        "Some(act_id)",
        "generation.map(SchemaU64::new)",
    ):
        assert required in scope
    for forbidden in ("prompt", "script", "output_schema", ".split("):
        assert forbidden not in scope
    for forbidden in ("error.to_string()", "traceback", "serde_json"):
        assert forbidden not in source
