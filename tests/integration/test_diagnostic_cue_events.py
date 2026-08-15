from __future__ import annotations

import os
import subprocess
import sysconfig
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
PRODUCER = ROOT / "rust/src/diagnostic_runtime/cue_producer.rs"
ACTOR = ROOT / "rust/src/orchestration/actor.rs"
MAILBOX = ROOT / "rust/src/orchestration/mailbox.rs"
CUE_FUTURE = ROOT / "rust/src/orchestration/cue_future.rs"
SCENE_CONTEXT = ROOT / "rust/src/orchestration/scene_context.rs"


def _method(source: str, name: str, next_name: str) -> str:
    start = source.index(f"fn {name}(")
    end = source.index(f"fn {next_name}(", start)
    return source[start:end]


def test_cue_producer_emits_the_closed_lifecycle_and_counter_taxonomy() -> None:
    source = PRODUCER.read_text(encoding="utf-8")

    for required in (
        "InstantDetail::CueAdmitted",
        "InstantDetail::CueEnqueued",
        "InstantDetail::CueDispatched",
        "InstantDetail::CueCancelRequested",
        "SpanStartDetail::CueMailboxWait",
        "SpanStartDetail::CueExecution",
        "CounterKind::CueActive",
        "CounterKind::ActorMailboxDepth",
        "SchemaU64::new(1)",
        "SchemaU64::new(0)",
    ):
        assert required in source
    assert source.count("InstantDetail::CueAdmitted") == 1

    assert "instruction" not in source
    for forbidden in (
        "EffectDetail",
        "EffectLifecycle",
        "ActLifecycle",
        "ActCaller",
        "AgentTurn",
        "ToolCall",
        "AgentMessage",
    ):
        assert forbidden not in source


def test_each_operation_is_keyed_by_its_stable_cued_scope() -> None:
    source = PRODUCER.read_text(encoding="utf-8")

    assert "HashMap<usize, Arc<CueLifecycleProducer>>" in source
    assert "Arc::as_ptr(operation.diagnostic_cued()).addr()" in source
    assert "cue_id: Option<RunLocalId>" not in source
    assert "Some(cue_id)" in source
    assert "operation_key(operation)" in source
    assert "_owner: Arc<CuedScope>" in source
    assert "_owner: Arc::clone(operation.diagnostic_cued())" in source
    assert 'format!("cue-{value}")' in source
    assert "static NEXT_CUE_ID: AtomicU64" in source
    assert "std::ptr::from_ref(operation)" not in source
    assert ".cue_id(py)" not in source


def test_mailbox_hooks_bracket_real_serialization_transitions() -> None:
    actor = ACTOR.read_text(encoding="utf-8")
    producer = PRODUCER.read_text(encoding="utf-8")

    enqueue = _method(actor, "enqueue_operation", "finish_operation")
    assert enqueue.index("mailbox.enqueue(operation)") < enqueue.index(
        "mailbox_changed(&observed, CueMailboxHook::Enqueued, queued, running)"
    )
    assert enqueue.index(
        "mailbox_changed(&observed, CueMailboxHook::Enqueued, queued, running)"
    ) < enqueue.index("self.drain_from(operation)")

    retire = _method(actor, "unlink_terminal_operation", "finish_terminal_action")
    assert retire.index("mailbox.terminal_transition(operation)") < retire.index(
        "mailbox_changed(operation, CueMailboxHook::Retired, queued, running)"
    )
    assert "queued" in producer
    assert ".checked_add(usize::from(running))" in producer
    assert "self.actor_scope.clone()" in producer


def test_dispatch_return_and_cancel_links_are_backward_only() -> None:
    source = PRODUCER.read_text(encoding="utf-8")
    dispatched = _method(source, "dispatched", "cancel_requested")
    terminal = _method(source, "terminal", "retired")

    assert "CausalRelation::Dispatch" in dispatched
    assert "enqueued_sequence" in dispatched
    assert "CausalRelation::Return" in terminal
    assert "CausalRelation::Handoff" in terminal
    assert "state.cancel_sequence.unwrap_or(fallback_source)" in terminal
    assert "start_span_with_causes" in source
    assert "finish_span_with_causes" in source
    assert "emit_instant_with_causes" in source


def test_terminal_outcomes_close_the_real_open_span_and_do_not_read_payloads() -> None:
    source = PRODUCER.read_text(encoding="utf-8")
    mailbox = MAILBOX.read_text(encoding="utf-8")

    for code in (
        '"cue-cancelled"',
        '"cue-dispatch-failed"',
        '"cue-execution-failed"',
        '"cue-cleanup-failed"',
    ):
        assert code in source
    for outcome in (
        "CueTerminalOutcome::Completed",
        "CueTerminalOutcome::Failed",
        "CueTerminalOutcome::Cancelled",
        "CueTerminalOutcome::CleanupFailed",
    ):
        assert outcome in source

    terminal_hook = mailbox.index("cue_producer::terminal(")
    mailbox_retire = mailbox.index("actor.unlink_terminal_operation(self)", terminal_hook)
    assert terminal_hook < mailbox_retire
    assert "error().is_some()" in source
    assert "error.to_string()" not in source
    assert "format!(\"{error}" not in source


def test_caller_drop_and_scene_shutdown_use_the_existing_cancel_transition() -> None:
    cue_future = CUE_FUTURE.read_text(encoding="utf-8")
    mailbox = MAILBOX.read_text(encoding="utf-8")
    scene_context = SCENE_CONTEXT.read_text(encoding="utf-8")

    close = _method(cue_future, "close", "__await__")
    assert close.index("operation.request_cancel()") < close.index("self.finish()")
    cancel = _method(mailbox, "request_cancel", "completion_snapshot")
    assert cancel.index("CueHook::CancelRequested") < cancel.index(
        "self.perform_terminal_action(action)"
    )
    assert "CueTerminalOutcome::Cancelled" in cancel
    assert "for operation in operations" in scene_context
    assert "operation.request_cancel();" in scene_context


def test_native_slot_and_full_library_build_contracts() -> None:
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
            "--features",
            "agent-test-support,diagnostics-test-support",
            "--test",
            "diagnostic_native_slots",
            "noop_cue_terminal_does_not_evaluate_error_supplier",
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
