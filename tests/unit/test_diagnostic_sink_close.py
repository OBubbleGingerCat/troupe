from __future__ import annotations

from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SINK = ROOT / "rust" / "src" / "diagnostic_sink"


def _source(name: str) -> str:
    return (SINK / name).read_text(encoding="utf-8")


def test_seal_is_one_shot_and_rejects_every_later_enqueue() -> None:
    seal = _source("seal.rs")

    assert "compare_exchange" in seal
    assert "SinkSealError::AlreadySealed" in seal
    assert "SinkEnqueueRejection::Sealed" in seal
    assert "SinkEnqueueRejection::Closed" in seal
    assert "try_enqueue" in seal
    assert "try_enqueue_terminal" in seal
    assert "terminal_accounted" in seal


def test_summary_is_latched_from_delivery_facts_without_accounting_fields() -> None:
    summary = _source("summary.rs")
    seal = _source("seal.rs")

    assert "OnceLock<Arc<SinkDeliverySummary>>" in seal
    assert "DeliveryProgress" in summary
    assert "Vec<DropDelta>" in summary
    assert "CallbackFailure" in summary
    assert "SinkCloseReason" in summary
    assert "complete" in summary
    for forbidden in ("token_usage", "usage_event", "event_pointer"):
        assert forbidden not in summary


def test_shutdown_uses_one_absolute_deadline_and_never_blindly_joins() -> None:
    shutdown = _source("shutdown.rs")

    assert "shutdown_until" in shutdown
    assert "deadline: Instant" in shutdown
    assert "is_finished()" in shutdown
    assert "thread.join()" in shutdown
    assert "drop(thread)" in shutdown
    assert "callback_abandoned" in shutdown
    assert "Duration::from_millis" in shutdown


def test_k02_does_not_admit_canonical_facts_or_own_python_waiters() -> None:
    combined = "\n".join(
        _source(name) for name in ("seal.rs", "summary.rs", "shutdown.rs")
    )

    for forbidden in (
        "ProductionDiagnosticHub",
        "DiagnosticEventHeader",
        "diagnostic.component_failed",
        "_diagnostic_close",
        "wait_closed",
        "act_call.rs",
    ):
        assert forbidden not in combined
