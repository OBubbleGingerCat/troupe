from __future__ import annotations

import os
import subprocess
import sysconfig
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
PRODUCER = ROOT / "rust/src/diagnostic_runtime/effect_producer.rs"
EFFECT = ROOT / "rust/src/orchestration/effect.rs"
MAILBOX = ROOT / "rust/src/orchestration/mailbox.rs"
CUE_PRODUCER = ROOT / "rust/src/diagnostic_runtime/cue_producer.rs"
CUE_FUTURE = ROOT / "rust/src/orchestration/cue_future.rs"


def _between(source: str, start: str, end: str) -> str:
    start_index = source.index(start)
    end_index = source.index(end, start_index)
    return source[start_index:end_index]


def test_effect_producer_emits_the_closed_lifecycle_taxonomy() -> None:
    source = PRODUCER.read_text(encoding="utf-8")

    for required in (
        "SpanStartDetail::EffectLifecycle",
        "InstantDetail::EffectCreated",
        "InstantDetail::EffectReturned",
        "InstantDetail::EffectConsumed",
        "EffectDetail::new(effect_type)",
        'format!("effect-{value}")',
        "static NEXT_EFFECT_ID: AtomicU64",
    ):
        assert required in source
    assert source.count("InstantDetail::EffectCreated") == 1
    assert source.count("InstantDetail::EffectReturned") == 1
    assert source.count("InstantDetail::EffectConsumed") == 1


def test_effect_scope_copies_canonical_owner_lineage_without_parsing_runtime_strings() -> None:
    source = PRODUCER.read_text(encoding="utf-8")
    active = source[source.index("mod active {") :]
    scope = _between(active, "fn effect_scope(", "fn next_effect_id(")

    for required in (
        ".scene_id()",
        ".actor_id()",
        ".cue_id()",
        "Some(effect_id)",
        "DiagnosticScope::new(",
    ):
        assert required in scope
    for forbidden in (
        "construction.id(",
        "construction.owner(",
        "effect.diagnostic_id(",
        "effect.diagnostic_owner(",
        ".split(",
        ".rsplit(",
        "cue_id(py)",
    ):
        assert forbidden not in active


def test_each_effect_identity_has_an_independent_owner_indexed_lifecycle() -> None:
    source = PRODUCER.read_text(encoding="utf-8")

    for required in (
        "HashMap<usize, Arc<EffectLifecycleProducer>>",
        "identity_key: usize",
        "owner_key: usize",
        "_identity: Arc<EffectIdentity>",
        "_owner: Arc<CuedScope>",
        "identity_key(construction.identity())",
        "identity_key(effect.diagnostic_identity())",
        "producer.owner_key == owner_key",
        "producers_for_owner(owner_key)",
    ):
        assert required in source
    assert "std::ptr::from_ref(effect)" not in source
    assert "runtime_effect_id" not in source


def test_return_consumption_and_cancellation_links_are_backward_only() -> None:
    source = PRODUCER.read_text(encoding="utf-8")
    returned = _between(source, "fn returned(&self)", "fn owner_terminal(")
    consumed = _between(source, "fn consume_and_finish(", "fn finish(")
    owner_terminal = _between(source, "fn owner_terminal(", "fn caller_finished(&self")

    assert "created_sequence" in returned
    assert "CausalRelation::Return" in returned
    assert "returned_sequence" in consumed
    assert "CausalRelation::Handoff" in consumed
    assert "EffectTerminal::completed(consumed_sequence)" in consumed
    assert "CueTerminalOutcome::Cancelled" in owner_terminal
    assert "OWNER_CANCELLED" in owner_terminal
    assert "CausalRelation::Handoff" in owner_terminal
    assert "caused_by(terminal.causal_source, terminal.relation)" in source


def test_all_provable_non_success_outcomes_are_stable_and_payload_free() -> None:
    source = PRODUCER.read_text(encoding="utf-8")
    active = source[source.index("mod active {") :]

    for code in (
        '"effect-construction-cancelled"',
        '"effect-construction-failed"',
        '"effect-not-returned"',
        '"effect-consumer-abandoned"',
        '"effect-owner-cancelled"',
        '"effect-owner-failed"',
        '"effect-owner-cleanup-failed"',
    ):
        assert code in active
    for outcome in (
        "CueTerminalOutcome::Completed",
        "CueTerminalOutcome::Failed",
        "CueTerminalOutcome::Cancelled",
        "CueTerminalOutcome::CleanupFailed",
        "SpanOutcome::Completed",
        "SpanOutcome::Failed",
        "SpanOutcome::Cancelled",
    ):
        assert outcome in active

    assert "is_cancelled_error" in active
    assert "Bound<'_, PyAny>" not in active
    assert "Py<PyAny>" not in active
    assert "error.to_string()" not in active
    assert 'format!("{error}' not in active
    assert "traceback" not in active
    assert "__dict__" not in active


def test_success_requires_consumption_while_abandonment_never_emits_consumed() -> None:
    source = PRODUCER.read_text(encoding="utf-8")
    owner_terminal = _between(source, "fn owner_terminal(", "fn caller_finished(&self")
    caller_finished = _between(source, "fn caller_finished(&self", "fn cleared(&self)")
    cleared = _between(source, "fn cleared(&self)", "fn consume_and_finish(")
    abandoned = _between(source, "fn abandon_and_finish(", "fn consume_and_finish(")

    assert "state.returned_sequence" in owner_terminal
    assert "Some(CueCallerOutcome::Consumed)" in owner_terminal
    assert "Some(CueCallerOutcome::Abandoned)" in owner_terminal
    assert "self.consume_and_finish(&mut state)" in owner_terminal
    assert "state.caller_outcome = Some(outcome)" in caller_finished
    assert "state.owner_terminal == Some(CueTerminalOutcome::Completed)" in caller_finished
    assert "CueCallerOutcome::Consumed => self.consume_and_finish" in caller_finished
    assert "CueCallerOutcome::Abandoned" in caller_finished
    assert "self.abandon_and_finish" in caller_finished
    assert "CONSUMER_ABANDONED" in cleared
    assert "state.cleared = true" in cleared
    assert "state.owner_terminal == Some(CueTerminalOutcome::Completed)" in cleared
    assert "NOT_RETURNED" not in cleared
    assert "CONSUMER_ABANDONED" in abandoned
    assert "EffectTerminal::cancelled" in abandoned
    assert "InstantDetail::EffectConsumed" not in abandoned


def test_cue_call_reports_typed_consumption_or_abandonment_at_every_exit() -> None:
    source = CUE_FUTURE.read_text(encoding="utf-8")
    finish = _between(source, "fn finish(&mut self", "fn source_for_lineage(")
    finish_from_operation = _between(
        source,
        "fn finish_from_operation(",
        "fn replace_shield_and_wait(",
    )
    close = _between(source, "fn close(&mut self)", "fn __await__")
    clear = _between(source, "fn __clear__(&mut self)", "}\n}")

    assert "CueHook::CallerFinished(outcome)" in finish
    assert "result.is_ok()" in finish_from_operation
    assert "CueCallerOutcome::Consumed" in finish_from_operation
    assert "CueCallerOutcome::Abandoned" in finish_from_operation
    assert "self.finish(outcome)" in finish_from_operation
    for exit_path in (close, clear):
        assert "operation.request_cancel()" in exit_path
        assert "self.finish(CueCallerOutcome::Abandoned)" in exit_path


def test_terminal_callbacks_only_use_registered_effect_state() -> None:
    source = PRODUCER.read_text(encoding="utf-8")
    terminal = _between(
        source,
        "pub(super) fn cue_terminal(",
        "pub(super) fn caller_finished(",
    )
    caller = _between(
        source,
        "pub(super) fn caller_finished(",
        "fn effect_detail(",
    )

    for callback in (terminal, caller):
        assert "producers_for_owner(owner_key)" in callback
        assert "lineage_snapshot" not in callback
        assert "diagnostic_cued" not in callback
        assert "cue_producer::" not in callback


def test_native_hooks_preserve_constructor_return_and_caller_ordering() -> None:
    effect = EFFECT.read_text(encoding="utf-8")
    mailbox = MAILBOX.read_text(encoding="utf-8")
    cue = CUE_PRODUCER.read_text(encoding="utf-8")

    construction = _between(effect, "pub(crate) fn construct_effect(", "fn lock<")
    assert construction.index("construction_started(&construction)") < construction.index(
        "effect_type.call(args, Some(kwargs))"
    )
    assert construction.index("effect_type.call(args, Some(kwargs))") < construction.index(
        "construction_finished(&construction"
    )

    validation = _between(mailbox, "fn validate_cued_result(", "fn trusted_task_result(")
    assert validation.index("cast::<Effect>()") < validation.index("EffectHook::Returned")
    assert validation.index("EffectHook::Returned") < validation.index("Ok(tuple.clone().unbind())")

    cue_terminal = _between(cue, "fn terminal(&self", "fn retired(&self")
    assert cue_terminal.index("effect_producer::cue_terminal") < cue_terminal.index(
        "finish_span_with_causes"
    )
    caller = _between(cue, "CueHook::CallerFinished(outcome) =>", "}\n        }\n    }")
    assert caller.index("effect_producer::caller_finished") < caller.index(
        "producer.caller_finished()"
    )


def test_native_effect_unit_and_active_library_contracts() -> None:
    environment = os.environ.copy()
    libdir = sysconfig.get_config_var("LIBDIR")
    if libdir:
        current = environment.get("LD_LIBRARY_PATH")
        environment["LD_LIBRARY_PATH"] = (
            f"{libdir}{os.pathsep}{current}" if current else str(libdir)
        )

    subprocess.run(
        [
            "cargo",
            "test",
            "--locked",
            "--manifest-path",
            "rust/Cargo.toml",
            "--package",
            "troupe",
            "orchestration::effect::tests",
            "--",
            "--nocapture",
        ],
        cwd=ROOT,
        env=environment,
        check=True,
    )
    subprocess.run(
        [
            "cargo",
            "check",
            "--locked",
            "--manifest-path",
            "rust/Cargo.toml",
            "--package",
            "troupe",
            "--features",
            "agent-test-support,diagnostics-test-support",
            "--lib",
        ],
        cwd=ROOT,
        env=environment,
        check=True,
    )
