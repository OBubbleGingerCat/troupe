from __future__ import annotations

import os
import subprocess
import sysconfig
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
PRODUCER = ROOT / "rust/src/diagnostic_runtime/scene_drain_producer.rs"
SCENE_CONTEXT = ROOT / "rust/src/orchestration/scene_context.rs"


def _between(source: str, start: str, end: str) -> str:
    start_index = source.index(start)
    end_index = source.index(end, start_index)
    return source[start_index:end_index]


def test_scene_drain_producer_uses_the_closed_span_taxonomy() -> None:
    source = PRODUCER.read_text(encoding="utf-8")

    for required in (
        "SpanStartDetail::SceneDrain",
        "SpanStartDetail::SceneCleanup",
        "Some(self.lineage.scene_span_id())",
        "start_span_with_causes",
        "finish_span_with_causes",
        "CausalRelation::FollowsFrom",
        "CausalRelation::Handoff",
    ):
        assert required in source

    for forbidden in (
        "ProductionShutdown",
        "DiagnosticRuntimeGuard",
        "writer_drain",
        "registry",
        "stream_closed",
    ):
        assert forbidden not in source


def test_terminal_classification_separates_business_and_cleanup_outcomes() -> None:
    source = PRODUCER.read_text(encoding="utf-8")
    returned = _between(source, "fn returned_terminal(", "fn is_stop_iteration(")
    driver = _between(source, "fn driver_observation(", "fn returned_terminal(")

    for code in (
        '"scene-drain-cancelled"',
        '"scene-drain-failed"',
        '"scene-cleanup-failed"',
    ):
        assert code in source
    assert "is_stop_iteration(error)" in returned
    assert "is_cancelled_error(error)" in returned
    assert "TerminalSpan::failed(DRAIN_FAILED)" in returned
    assert "SceneDriverExit::Closed" in driver
    assert "drain: TerminalSpan::cancelled(DRAIN_CANCELLED)" in driver
    assert "TerminalSpan::failed(CLEANUP_FAILED)" in driver

    for forbidden in (
        "error.to_string()",
        'format!("{error}',
        "traceback",
        "raw_exception",
        "exception_message",
    ):
        assert forbidden not in source


def test_cancellation_propagation_changes_causality_not_business_outcome() -> None:
    source = PRODUCER.read_text(encoding="utf-8")
    cancellation = _between(
        source, "fn cancellation_started(&self)", "fn cleanup_finished(&self)"
    )
    cleanup = _between(
        source, "fn cleanup_finished(&self)", "fn fail_diagnostic("
    )

    assert "state.cancellation_started = true" in cancellation
    assert "TerminalSpan::failed" not in cancellation
    assert "if state.cancellation_started" in cleanup
    assert "CausalRelation::Handoff" in cleanup
    assert "CausalRelation::FollowsFrom" in cleanup


def test_lost_driver_terminal_emits_gap_instead_of_fake_finish() -> None:
    source = PRODUCER.read_text(encoding="utf-8")
    admission = _between(source, "fn admission_closed(&self)", "fn cleanup_started(&self)")
    cleanup = _between(
        source, "fn cleanup_finished(&self)", "fn fail_diagnostic("
    )

    assert "DriverObservation::Lost" in admission
    assert "emit_observation_gap" in admission
    assert "DiagnosticEventKind::SpanFinished" in admission
    assert "scene-driver-terminal-unobserved" in source
    assert "(None, DriverObservation::Lost) => None" in cleanup


def test_native_hooks_preserve_cleanup_and_scene_terminal_order() -> None:
    source = SCENE_CONTEXT.read_text(encoding="utf-8")

    owner = _between(source, "fn close_owner(", "fn drive(")
    assert owner.index("scene_drain_producer::driver_exited") < owner.index(
        "scene.close();"
    )

    close = _between(source, "pub(crate) fn close(&self)", "fn cancel_operations(")
    admission = close.index("SceneDrainHook::AdmissionClosed")
    cleanup = close.index("SceneDrainHook::CleanupStarted")
    cancellation = close.index("SceneDrainHook::CancellationStarted")
    cancel_existing = close.index("self.cancel_operations(operations)")
    assert admission < cleanup < cancellation < cancel_existing

    closed = _between(source, "fn observe_closed(&self)", "fn new(")
    assert closed.index("SceneDrainHook::CleanupFinished") < closed.index(
        "SceneHook::SceneFinished"
    )
    operation_finished = _between(
        source, "pub(crate) fn operation_finished(", "pub(crate) async fn wait_closed("
    )
    assert operation_finished.index("actor_mailbox_unlinked_for_test") < (
        operation_finished.index("self.observe_closed()")
    )


def test_native_scene_drain_contract() -> None:
    command = [
        "cargo",
        "test",
        "--locked",
        "--manifest-path",
        "rust/Cargo.toml",
        "--package",
        "troupe",
        "diagnostic_runtime::scene_drain_producer::tests",
        "--",
        "--nocapture",
    ]
    environment = os.environ.copy()
    libdir = sysconfig.get_config_var("LIBDIR")
    if libdir:
        current = environment.get("LD_LIBRARY_PATH")
        environment["LD_LIBRARY_PATH"] = (
            f"{libdir}{os.pathsep}{current}" if current else str(libdir)
        )
    subprocess.run(command, cwd=ROOT, env=environment, check=True)
