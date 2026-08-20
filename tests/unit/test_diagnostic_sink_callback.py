from __future__ import annotations

from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SINK = ROOT / "rust" / "src" / "diagnostic_sink"


def _source(name: str) -> str:
    return (SINK / name).read_text(encoding="utf-8")


def test_dispatcher_uses_one_named_thread_and_one_private_asyncio_loop() -> None:
    thread = _source("thread.rs")
    callback = _source("callback.rs")

    assert '"troupe-diagnostic-callback"' in thread
    assert thread.count("thread::Builder::new()") == 1
    assert "asyncio.new_event_loop()" in callback
    assert "asyncio.set_event_loop(loop)" in callback
    assert "contextvars.Context()" in callback
    assert "threading.current_thread().daemon" in callback


def test_each_sink_has_one_serial_task_and_callback_faults_are_local() -> None:
    callback = _source("callback.rs")
    dispatcher = _source("dispatcher.rs")

    assert "async def _drive_sink" in callback
    assert "tasks[sink_id]" in callback
    assert "await result" in callback
    assert "except BaseException as error" in callback
    assert "result is not None" in callback
    assert "CallbackFailureKind" in dispatcher
    assert "OnceLock<CallbackFailure>" in dispatcher
    assert "try_discard_queued" in dispatcher


def test_agent_enqueue_is_rust_only_and_never_invokes_the_callback() -> None:
    dispatcher = _source("dispatcher.rs")
    start = dispatcher.index("pub(crate) fn try_enqueue(")
    end = dispatcher.index("\n    }", start) + len("\n    }")
    enqueue = dispatcher[start:end]

    assert "try_admit" in enqueue
    for forbidden in ("Python::attach", "call1", "call_method", ".await", "callback"):
        assert forbidden not in enqueue


def test_k01_does_not_admit_canonical_facts_or_own_shutdown_policy() -> None:
    combined = "\n".join(
        _source(name) for name in ("thread.rs", "dispatcher.rs", "callback.rs")
    )

    for forbidden in (
        "DiagnosticHub",
        "ObservationGap",
        "diagnostic.dropped_events",
        "DiagnosticSinkSummary",
        "shutdown deadline",
    ):
        assert forbidden not in combined
