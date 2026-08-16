from __future__ import annotations

import asyncio
import importlib
import json
import sys
import threading
from pathlib import Path
from typing import Any

import pytest


ROOT = Path(__file__).resolve().parents[2]
MOCK_AGENT = ROOT / "tests" / "support" / "mock_acp_agent.py"
SETTLEMENT = ROOT / "rust" / "src" / "diagnostic_runtime" / "sink_settlement.rs"
BINDING = ROOT / "rust" / "src" / "diagnostic_runtime" / "sink_binding.rs"
SINK_MOD = ROOT / "rust" / "src" / "diagnostic_sink" / "mod.rs"
HARNESS_TIMEOUT = 5.0


def _native() -> Any:
    return importlib.import_module("troupe._runtime")


def _between(source: str, start: str, end: str) -> str:
    start_index = source.index(start)
    end_index = source.index(end, start_index)
    return source[start_index:end_index]


def _base_state(diagnostics: Any, sink: Any) -> str:
    descriptor = diagnostics.DiagnosticSink.__dict__["_DiagnosticSink__state"]
    return descriptor.__get__(sink, type(sink))


def _configure_agent(
    tmp_path: Path,
    *,
    scenario: str,
    results: list[dict[str, object]] | None = None,
) -> Path:
    events = tmp_path / "agent-events.jsonl"
    args = [
        str(MOCK_AGENT),
        "--events",
        str(events),
        "--scenario",
        scenario,
    ]
    if results is not None:
        args.extend(
            ["--results-json", json.dumps(results, separators=(",", ":"))]
        )
    _native()._agent_test_set_launch(program=sys.executable, args=args)
    return events


def _profile(troupe: Any, workspace: Path) -> Any:
    return troupe.AgentProfile(
        agent="codex",
        workspace=workspace,
        model="test-model",
        effort="max",
    )


@pytest.fixture(autouse=True)
def _reset_test_launch(request: pytest.FixtureRequest) -> Any:
    if request.node.name == "test_settlement_transaction_and_close_projection_are_source_frozen":
        yield
        return
    _native()._agent_test_reset_launch()
    yield
    _native()._agent_test_reset_launch()


def test_settlement_transaction_and_close_projection_are_source_frozen() -> None:
    settlement = SETTLEMENT.read_text(encoding="utf-8")
    binding = BINDING.read_text(encoding="utf-8")
    exports = SINK_MOD.read_text(encoding="utf-8")

    transaction = _between(
        settlement,
        "    fn settle(\n",
        "\n}\n\nfn lock",
    )
    assert transaction.index("sink.prepare_settlement()") < transaction.index(
        "prepare_expiry"
    )
    assert transaction.index("prepare_expiry") < transaction.index("authority.commit()")
    assert transaction.index("authority.commit()") < transaction.index(
        "ActSettlementSink::commit_settlement"
    )
    rejected = _between(
        transaction,
        "Some(ActSettlementSinkCommit::Rejected(error))",
        "Some(ActSettlementSinkCommit::CommittedWithFailure(error))",
    )
    assert "authority.rollback()" in rejected
    committed_failure = transaction[transaction.index(
        "Some(ActSettlementSinkCommit::CommittedWithFailure(error))"
    ) :]
    assert committed_failure.index("CoordinatorPhase::Settled") < committed_failure.index(
        "Err(error)"
    )
    assert "PreparedActAuthorityExpiry" in settlement
    assert "install_authority_expiry" in settlement
    assert "pub(crate) fn settle_authority_only" in settlement
    assert "authority_only_settlement_is_a_real_transaction_path" in settlement

    terminal = _between(binding, "fn settle_terminal(", "fn install_authority_expiry(")
    assert "BoundSinkSettlement" in terminal
    assert "settle_with_sink(&sink)" in terminal
    sink_commit = _between(
        binding,
        "fn commit_settlement(&self) -> ActSettlementSinkCommit",
        "impl ActEventSubscriber for ActSinkSubscriber",
    )
    assert sink_commit.index("commit_terminal_seal") < sink_commit.index(
        ".retire_expected("
    )
    assert sink_commit.index(".retire_expected(") < sink_commit.index(
        "start_close_waiter()"
    )
    delivery = _between(binding, "fn deliver_projected(", "fn settle_terminal(")
    assert delivery.index("try_enqueue_terminal") < delivery.index(
        "self.settle_terminal(act_outcome)"
    )

    waiter = _between(
        settlement,
        "pub(crate) fn start_close_waiter",
        "pub(crate) fn begin_runtime_shutdown",
    )
    assert "tokio::time::sleep(CLOSE_POLL_INTERVAL)" in waiter
    assert "timeout" not in waiter.lower()
    projection = _between(settlement, "fn materialize_summary(", "fn set_optional_u64(")
    for field in (
        "run_id",
        "act_id",
        "act_outcome",
        "close_reason",
        "complete",
        "delivered_events",
        "dropped_by_kind",
        "source_gaps",
        "truncated_payloads",
        "callback_failure",
        "callback_abandoned",
    ):
        assert f'"{field}"' in projection
    assert "token_usage" not in projection
    assert "SinkClosePoll" in exports
    assert "SinkSealError" in exports
    assert "SinkDeliverySummary" in exports


def test_wait_closed_is_repeatable_and_cancelled_waiter_is_isolated(
    tmp_path: Path,
) -> None:
    import troupe
    from troupe import diagnostics

    _configure_agent(tmp_path, scenario="act_submit_results", results=[{}])
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    callback_started = threading.Event()
    release_callback = threading.Event()
    callback_events: list[Any] = []
    summaries: list[Any] = []
    runtime = _native()._Runtime()

    class BlockingSink(diagnostics.DiagnosticSink):
        def on_event(self, event: diagnostics.DiagnosticEvent, /) -> None:
            callback_events.append(event)
            if not callback_started.is_set():
                callback_started.set()
                assert release_callback.wait(HARNESS_TIMEOUT)

    sink = BlockingSink()

    class CloseActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                assert (
                    await self.act(
                        script="Return an empty object.",
                        output_schema={},
                        diagnostic_sink=sink,
                    )
                    == {}
                )
                assert await asyncio.to_thread(callback_started.wait, 1.0)
                assert _base_state(diagnostics, sink) == "SEALED"

                cancelled = asyncio.create_task(sink.wait_closed())
                await asyncio.sleep(0)
                cancelled.cancel()
                with pytest.raises(asyncio.CancelledError):
                    await cancelled

                first = asyncio.create_task(sink.wait_closed())
                second = asyncio.create_task(sink.wait_closed())
                await asyncio.sleep(0)
                assert not first.done()
                assert not second.done()
                release_callback.set()
                summary_one, summary_two = await asyncio.gather(first, second)
                summary_three = await sink.wait_closed()
                summaries.extend((summary_one, summary_two, summary_three))
            finally:
                release_callback.set()
            return ()

    class CloseProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                CloseActor,
                name="sink-close",
                agent_profile=_profile(troupe, workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(CloseProduction([]))),
            HARNESS_TIMEOUT,
        )

    try:
        asyncio.run(scenario())
    finally:
        release_callback.set()

    assert _base_state(diagnostics, sink) == "CLOSED"
    assert len(summaries) == 3
    assert summaries[0] is summaries[1] is summaries[2]
    summary = summaries[0]
    assert summary.act_outcome == "completed"
    assert summary.close_reason == "act_finished"
    assert summary.complete is True
    assert summary.delivered_events == len(callback_events)
    assert summary.callback_failure is None
    assert summary.callback_abandoned is False
    assert [event.sequence for event in callback_events] == sorted(
        event.sequence for event in callback_events
    )


def test_callback_failure_closes_without_changing_act_result(tmp_path: Path) -> None:
    import troupe
    from troupe import diagnostics

    _configure_agent(tmp_path, scenario="act_submit_results", results=[{}])
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    summaries: list[Any] = []
    runtime = _native()._Runtime()

    class RaisingSink(diagnostics.DiagnosticSink):
        def on_event(self, event: diagnostics.DiagnosticEvent, /) -> None:
            del event
            raise ValueError("expected sink callback failure")

    sink = RaisingSink()

    class CallbackFailureActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            assert (
                await self.act(
                    script="Return an empty object.",
                    output_schema={},
                    diagnostic_sink=sink,
                )
                == {}
            )
            summaries.append(await sink.wait_closed())
            return ()

    class CallbackFailureProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                CallbackFailureActor,
                name="sink-callback-failure",
                agent_profile=_profile(troupe, workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(CallbackFailureProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert _base_state(diagnostics, sink) == "CLOSED"
    summary = summaries[0]
    assert summary.act_outcome == "completed"
    assert summary.close_reason == "callback_failed"
    assert summary.complete is False
    assert summary.callback_failure.kind == "raised"
    assert summary.callback_failure.exception_type == "ValueError"
    assert summary.callback_abandoned is False


def test_runtime_shutdown_abandons_blocking_callback_with_stable_summary(
    tmp_path: Path,
) -> None:
    import troupe
    from troupe import diagnostics

    _configure_agent(tmp_path, scenario="ready")
    _native()._agent_test_hold_opening()
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    callback_started = threading.Event()
    release_callback = threading.Event()
    runtime = _native()._Runtime()

    class BlockingSink(diagnostics.DiagnosticSink):
        def on_event(self, event: diagnostics.DiagnosticEvent, /) -> None:
            del event
            callback_started.set()
            release_callback.wait()

    sink = BlockingSink()

    class ShutdownActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            await self.act(
                script="This Act is cancelled before prompt submission.",
                output_schema={},
                diagnostic_sink=sink,
            )
            return ()

    class ShutdownProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                ShutdownActor,
                name="sink-shutdown",
                agent_profile=_profile(troupe, workspace),
                actor_args=(),
                actor_kwargs={},
            )
            task = asyncio.create_task(handle.cue({}))
            while _base_state(diagnostics, sink) != "BOUND":
                await asyncio.sleep(0)
            assert await asyncio.to_thread(callback_started.wait, 1.0)
            assert task.cancel("close Runtime")
            with pytest.raises(asyncio.CancelledError):
                await task
            _native()._agent_test_release_opening()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ShutdownProduction([]))),
            HARNESS_TIMEOUT,
        )

    async def await_summary() -> Any:
        return await asyncio.wait_for(sink.wait_closed(), HARNESS_TIMEOUT)

    try:
        asyncio.run(scenario())
        summary = asyncio.run(await_summary())
    finally:
        _native()._agent_test_release_opening()
        release_callback.set()

    assert _base_state(diagnostics, sink) == "CLOSED"
    assert summary.close_reason == "runtime_shutdown"
    assert summary.complete is False
    assert summary.callback_abandoned is True
    assert summary.callback_failure is None
