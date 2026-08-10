from __future__ import annotations

import asyncio
import importlib
import json
import os
import sys
import threading
from pathlib import Path
from typing import Any

import pytest

import troupe


ROOT = Path(__file__).resolve().parents[2]
MOCK_AGENT = ROOT / "tests" / "support" / "mock_acp_agent.py"
HARNESS_TIMEOUT = 8.0


def _native() -> Any:
    return importlib.import_module("troupe._runtime")


def _events(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


async def _wait_for_event(path: Path, event: str, count: int = 1) -> None:
    while sum(item["event"] == event for item in _events(path)) < count:
        await asyncio.sleep(0)


async def _loop_barrier() -> None:
    reached = asyncio.Event()
    asyncio.get_running_loop().call_soon(reached.set)
    await reached.wait()


async def _signal_fifo(path: Path) -> None:
    def write() -> None:
        with path.open("wb", buffering=0) as signal:
            signal.write(b"1")

    await asyncio.to_thread(write)


def _configure_mock(
    events: Path,
    scenario: str,
    *,
    settlement_release: Path | None = None,
) -> None:
    args = [str(MOCK_AGENT), "--events", str(events), "--scenario", scenario]
    if settlement_release is not None:
        args.extend(["--release", str(settlement_release)])
    _native()._agent_test_set_launch(program=sys.executable, args=args)


def _profile(workspace: Path) -> troupe.AgentProfile:
    return troupe.AgentProfile(
        agent="codex",
        workspace=workspace,
        model="test-model",
        effort="max",
    )


def _schema() -> dict[str, troupe.act_schema.FieldSpec]:
    return {
        "value": troupe.act_schema.Int64Value(description="the turn number"),
    }


@pytest.fixture(autouse=True)
def _reset_test_launch() -> Any:
    _native()._agent_test_reset_launch()
    yield
    _native()._agent_test_reset_launch()


def test_cancel_before_submission_rolls_back_without_touching_remote_turn(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, "act_two_turns")
    _native()._agent_test_hold_opening()
    runtime = _native()._Runtime()
    entered: asyncio.Event
    observed: list[dict[str, object]] = []

    class CancelActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            entered.set()
            result = await self.act(
                script="Remember the token alpha for the next turn.",
                output_schema={
                    "stored": troupe.act_schema.StrValue(
                        description="the stored token",
                        choices=["alpha"],
                    )
                },
            )
            observed.append(result)
            return ()

    class CancelProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                CancelActor,
                name="cancel-before-submit",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            first = asyncio.create_task(handle.cue({"turn": 1}))
            await entered.wait()
            assert first.cancel("before-submit")
            with pytest.raises(asyncio.CancelledError):
                await first
            assert not events.exists()

            _native()._agent_test_release_opening()
            assert await handle.cue({"turn": 2}) == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        nonlocal entered
        entered = asyncio.Event()
        await asyncio.wait_for(
            asyncio.shield(runtime.run(CancelProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert observed == [{"stored": "alpha"}]
    rows = _events(events)
    assert sum(row["event"] == "prompt_received" for row in rows) == 1
    assert all(row["event"] != "cancel_received" for row in rows)


def test_submitted_cancel_handoffs_before_writer_or_remote_settlement(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    settlement_release = workspace / "settlement.fifo"
    os.mkfifo(settlement_release)
    _configure_mock(events, "act_cancel_then_reuse", settlement_release=settlement_release)
    _native()._agent_test_hold_turn_submission()
    runtime = _native()._Runtime()
    observed: list[dict[str, object]] = []

    class CancelActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            result = await self.act(
                script=f"Return result for {cue.instruction['turn']}",
                output_schema=_schema(),
            )
            observed.append(result)
            return ()

    class CancelProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                CancelActor,
                name="submitted-cancel",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            first = asyncio.create_task(handle.cue({"turn": 1}))
            while not _native()._agent_test_turn_gate_states()["submission"]["arrived"]:
                await asyncio.sleep(0)
            assert _events(events) == [] or all(
                row["event"] != "prompt_received" for row in _events(events)
            )

            assert first.cancel("submitted-cancel")
            with pytest.raises(asyncio.CancelledError):
                await first
            assert handle._agent_state_for_test() == "cancelling"  # type: ignore[attr-defined]

            second = asyncio.create_task(handle.cue({"turn": 2}))
            await _loop_barrier()
            assert not second.done()
            assert all(row["event"] != "prompt_received" for row in _events(events))

            _native()._agent_test_release_turn_submission()
            await _wait_for_event(events, "cancel_received")
            assert not second.done()
            assert sum(
                row["event"] == "prompt_received" for row in _events(events)
            ) == 1

            await _signal_fifo(settlement_release)
            assert await second == ()
            assert handle._agent_state_for_test() == "ready"  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(CancelProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert observed == [{"value": 2}]
    rows = _events(events)
    assert sum(row["event"] == "cancel_received" for row in rows) == 1
    assert [row["turn"] for row in rows if row["event"] == "prompt_received"] == [1, 2]


def test_cancel_discards_an_accepted_value_until_authoritative_settlement(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    settlement_release = workspace / "settlement.fifo"
    os.mkfifo(settlement_release)
    _configure_mock(
        events,
        "act_cancel_after_result_then_reuse",
        settlement_release=settlement_release,
    )
    runtime = _native()._Runtime()
    observed: list[dict[str, object]] = []

    class ResultActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            observed.append(
                await self.act(
                    script=f"Return result for {cue.instruction['turn']}",
                    output_schema=_schema(),
                )
            )
            return ()

    class ResultProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                ResultActor,
                name="accepted-cancel",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            first = asyncio.create_task(handle.cue({"turn": 1}))
            await _wait_for_event(events, "result_submitted")
            assert first.cancel()
            with pytest.raises(asyncio.CancelledError):
                await first
            assert observed == []
            assert handle._agent_state_for_test() == "cancelling"  # type: ignore[attr-defined]

            second = asyncio.create_task(handle.cue({"turn": 2}))
            await _wait_for_event(events, "cancel_received")
            await _loop_barrier()
            assert not second.done()
            await _signal_fifo(settlement_release)
            assert await second == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ResultProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert observed == [{"value": 2}]
    assert sum(row["event"] == "cancel_received" for row in _events(events)) == 1


def test_cancel_handoff_does_not_join_an_async_validator_that_swallows_cancel(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    settlement_release = workspace / "settlement.fifo"
    os.mkfifo(settlement_release)
    _configure_mock(
        events,
        "act_cancel_during_callback_then_reuse",
        settlement_release=settlement_release,
    )
    runtime = _native()._Runtime()
    callback_entered: asyncio.Event
    callback_cancelled: asyncio.Event
    callback_release: asyncio.Event
    late_returns: list[int] = []
    observed: list[dict[str, object]] = []

    class PendingValue(troupe.act_schema.SchemaValue[int]):
        def __init__(self) -> None:
            super().__init__(description="a pending integer", json_kind="int64")

        def render_prompt(self) -> str:
            return "must be an integer"

        async def validate(self, value: int, /) -> None:
            callback_entered.set()
            try:
                await asyncio.Event().wait()
            except asyncio.CancelledError:
                callback_cancelled.set()
                await callback_release.wait()
            late_returns.append(value)

    class CallbackActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            schema: dict[str, troupe.act_schema.FieldSpec]
            if cue.instruction["turn"] == 1:
                schema = {"value": PendingValue()}
            else:
                schema = _schema()
            observed.append(
                await self.act(
                    script=f"Return result for {cue.instruction['turn']}",
                    output_schema=schema,
                )
            )
            return ()

    class CallbackProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                CallbackActor,
                name="callback-cancel",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            first = asyncio.create_task(handle.cue({"turn": 1}))
            await callback_entered.wait()
            assert first.cancel()
            with pytest.raises(asyncio.CancelledError):
                await first
            await callback_cancelled.wait()
            assert late_returns == []
            assert handle._agent_state_for_test() == "cancelling"  # type: ignore[attr-defined]

            await _wait_for_event(events, "callback_tool_result_finished")
            callback_result = next(
                row for row in _events(events) if row["event"] == "callback_tool_result_finished"
            )
            assert callback_result["is_error"] is True
            await _wait_for_event(events, "cancel_received")
            await _signal_fifo(settlement_release)
            assert await handle.cue({"turn": 2}) == ()

            callback_release.set()
            await _loop_barrier()
            await _loop_barrier()
            assert late_returns == [1]
            runtime.request_shutdown()

    async def scenario() -> None:
        nonlocal callback_entered, callback_cancelled, callback_release
        callback_entered = asyncio.Event()
        callback_cancelled = asyncio.Event()
        callback_release = asyncio.Event()
        await asyncio.wait_for(
            asyncio.shield(runtime.run(CallbackProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert observed == [{"value": 2}]


def test_ninth_invalid_result_releases_caller_and_supervisor_settles_turn(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    settlement_release = workspace / "settlement.fifo"
    os.mkfifo(settlement_release)
    _configure_mock(
        events,
        "act_result_rejected_then_reuse",
        settlement_release=settlement_release,
    )
    runtime = _native()._Runtime()
    failures: list[troupe.AgentResultError] = []
    observed: list[dict[str, object]] = []

    class RejectedActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            try:
                observed.append(
                    await self.act(
                        script=f"Return result for {cue.instruction['turn']}",
                        output_schema=_schema(),
                    )
                )
            except troupe.AgentResultError as error:
                failures.append(error)
            return ()

    class RejectedProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                RejectedActor,
                name="rejected-result",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({"turn": 1}) == ()
            assert len(failures) == 1
            assert failures[0].code == "too_many_invalid_results"
            assert failures[0].invalid_calls == 9
            assert handle._agent_state_for_test() == "cancelling"  # type: ignore[attr-defined]
            await _wait_for_event(events, "cancel_received")

            second = asyncio.create_task(handle.cue({"turn": 2}))
            await _loop_barrier()
            assert not second.done()
            await _signal_fifo(settlement_release)
            assert await second == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(RejectedProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert observed == [{"value": 2}]
    assert [
        row["invalid_call"]
        for row in _events(events)
        if row["event"] == "invalid_result_submitted"
    ] == list(range(1, 10))


def test_supervisor_cancels_reverse_requests_before_remote_settlement(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    settlement_release = workspace / "settlement.fifo"
    os.mkfifo(settlement_release)
    _configure_mock(
        events,
        "act_cancel_permission_then_reuse",
        settlement_release=settlement_release,
    )
    runtime = _native()._Runtime()
    observed: list[dict[str, object]] = []

    class PermissionActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            observed.append(
                await self.act(
                    script=f"Return result for {cue.instruction['turn']}",
                    output_schema=_schema(),
                )
            )
            return ()

    class PermissionProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                PermissionActor,
                name="permission-cancel",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            first = asyncio.create_task(handle.cue({"turn": 1}))
            await _wait_for_event(events, "prompt_received")
            assert first.cancel()
            with pytest.raises(asyncio.CancelledError):
                await first

            await _wait_for_event(events, "permission_cancelled_response_received")
            assert handle._agent_state_for_test() == "cancelling"  # type: ignore[attr-defined]
            await _signal_fifo(settlement_release)
            assert await handle.cue({"turn": 2}) == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(PermissionProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert observed == [{"value": 2}]


def test_cancelling_an_availability_waiter_does_not_touch_the_owned_turn(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    settlement_release = workspace / "settlement.fifo"
    os.mkfifo(settlement_release)
    _configure_mock(events, "act_cancel_then_reuse", settlement_release=settlement_release)
    runtime = _native()._Runtime()
    observed: list[dict[str, object]] = []

    class WaitingActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            observed.append(
                await self.act(
                    script=f"Return result for {cue.instruction['turn']}",
                    output_schema=_schema(),
                )
            )
            return ()

    class WaitingProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                WaitingActor,
                name="cancel-waiter",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            first = asyncio.create_task(handle.cue({"turn": 1}))
            await _wait_for_event(events, "prompt_received")
            assert first.cancel()
            with pytest.raises(asyncio.CancelledError):
                await first
            await _wait_for_event(events, "cancel_received")

            waiting = asyncio.create_task(handle.cue({"turn": 2}))
            await _loop_barrier()
            assert not waiting.done()
            assert waiting.cancel()
            with pytest.raises(asyncio.CancelledError):
                await waiting
            assert sum(row["event"] == "cancel_received" for row in _events(events)) == 1
            assert sum(row["event"] == "prompt_received" for row in _events(events)) == 1

            await _signal_fifo(settlement_release)
            assert await handle.cue({"turn": 3}) == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(WaitingProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert observed == [{"value": 2}]


def test_cued_scope_handoffs_an_escaped_submitted_act_without_remote_wait(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    settlement_release = workspace / "settlement.fifo"
    os.mkfifo(settlement_release)
    _configure_mock(events, "act_cancel_then_reuse", settlement_release=settlement_release)
    _native()._agent_test_hold_turn_submission()
    runtime = _native()._Runtime()
    escaped: list[asyncio.Task[dict[str, object]]] = []
    observed: list[dict[str, object]] = []

    class EscapedActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            if cue.instruction["turn"] == 1:
                task = asyncio.create_task(
                    self.act(script="Keep running.", output_schema=_schema())
                )
                escaped.append(task)
                while not _native()._agent_test_turn_gate_states()["submission"]["arrived"]:
                    await asyncio.sleep(0)
                return ()
            observed.append(
                await self.act(script="Return the next result.", output_schema=_schema())
            )
            return ()

    class EscapedProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                EscapedActor,
                name="escaped-act",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({"turn": 1}) == ()
            assert handle._agent_state_for_test() == "cancelling"  # type: ignore[attr-defined]
            with pytest.raises(asyncio.CancelledError):
                await escaped[0]

            _native()._agent_test_release_turn_submission()
            await _wait_for_event(events, "cancel_received")
            await _signal_fifo(settlement_release)
            assert await handle.cue({"turn": 2}) == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(EscapedProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert observed == [{"value": 2}]


def test_transport_loss_during_supervisor_settlement_breaks_later_turns(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, "act_cancel_transport_lost")
    runtime = _native()._Runtime()
    later_failures: list[troupe.AgentSessionBrokenError] = []

    class BrokenActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            try:
                await self.act(
                    script=f"Return result for {cue.instruction['turn']}",
                    output_schema=_schema(),
                )
            except troupe.AgentSessionBrokenError as error:
                later_failures.append(error)
            return ()

    class BrokenProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                BrokenActor,
                name="cancel-transport-loss",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            first = asyncio.create_task(handle.cue({"turn": 1}))
            await _wait_for_event(events, "prompt_received")
            assert first.cancel()
            with pytest.raises(asyncio.CancelledError):
                await first
            await _wait_for_event(events, "transport_closing_after_cancel")
            while handle._agent_state_for_test() == "cancelling":  # type: ignore[attr-defined]
                await asyncio.sleep(0)
            assert handle._agent_state_for_test() == "broken"  # type: ignore[attr-defined]
            assert await handle.cue({"turn": 2}) == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(BrokenProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert len(later_failures) == 1
    assert later_failures[0].code == "transport_lost"


def test_committed_result_precedes_late_python_task_cancellation(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, "act_two_turns")
    _native()._agent_test_hold_turn_outcome()
    runtime = _native()._Runtime()
    act_tasks: list[asyncio.Task[dict[str, object]]] = []
    observed: list[dict[str, object]] = []

    class CommittedActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            task = asyncio.create_task(
                self.act(
                    script="Remember the token alpha for the next turn.",
                    output_schema={
                        "stored": troupe.act_schema.StrValue(
                            description="the stored token",
                            choices=["alpha"],
                        )
                    },
                )
            )
            act_tasks.append(task)
            observed.append(await task)
            return ()

    class CommittedProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                CommittedActor,
                name="committed-result",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            cue = asyncio.create_task(handle.cue({"turn": 1}))
            while not _native()._agent_test_turn_gate_states()["outcome"]["arrived"]:
                await asyncio.sleep(0)
            assert not act_tasks[0].done()
            assert act_tasks[0].cancel("too late")
            _native()._agent_test_release_turn_outcome()
            assert await cue == ()
            assert not act_tasks[0].cancelled()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(CommittedProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert observed == [{"stored": "alpha"}]


def test_sync_validator_returns_before_queued_cancellation_can_handoff(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    settlement_release = workspace / "settlement.fifo"
    os.mkfifo(settlement_release)
    _configure_mock(
        events,
        "act_cancel_during_callback_then_reuse",
        settlement_release=settlement_release,
    )
    runtime = _native()._Runtime()
    validator_entered = threading.Event()
    validator_release = threading.Event()
    ordering: list[str] = []
    observed: list[dict[str, object]] = []

    class BlockingValue(troupe.act_schema.SchemaValue[int]):
        def __init__(self) -> None:
            super().__init__(description="a blocking integer", json_kind="int64")

        def render_prompt(self) -> str:
            return "must be an integer"

        def validate(self, value: int, /) -> None:
            del value
            ordering.append("validator_entered")
            validator_entered.set()
            validator_release.wait()
            ordering.append("validator_returned")

    class BlockingActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            schema: dict[str, troupe.act_schema.FieldSpec]
            if cue.instruction["turn"] == 1:
                schema = {"value": BlockingValue()}
            else:
                schema = _schema()
            observed.append(
                await self.act(
                    script=f"Return result for {cue.instruction['turn']}",
                    output_schema=schema,
                )
            )
            return ()

    class BlockingProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                BlockingActor,
                name="sync-callback-cancel",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            first = asyncio.create_task(handle.cue({"turn": 1}))
            loop = asyncio.get_running_loop()

            def dispatch_cancel() -> None:
                ordering.append("cancel_dispatched")
                first.cancel()

            def queue_cancel() -> None:
                assert validator_entered.wait(HARNESS_TIMEOUT)
                loop.call_soon_threadsafe(dispatch_cancel)
                validator_release.set()

            canceller = threading.Thread(target=queue_cancel)
            canceller.start()
            with pytest.raises(asyncio.CancelledError):
                await first
            canceller.join()
            assert ordering == [
                "validator_entered",
                "validator_returned",
                "cancel_dispatched",
            ]

            await _wait_for_event(events, "cancel_received")
            await _signal_fifo(settlement_release)
            assert await handle.cue({"turn": 2}) == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(BlockingProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert observed == [{"value": 2}]


def test_cancel_before_atomic_result_commit_wins_the_caller_outcome(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, "act_two_turns")
    _native()._agent_test_hold_turn_settlement()
    runtime = _native()._Runtime()
    observed: list[dict[str, object]] = []
    cancelled: list[bool] = []
    act_tasks: list[asyncio.Task[dict[str, object]]] = []

    class CommitActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            task = asyncio.create_task(
                self.act(
                    script="Remember the token alpha.",
                    output_schema={
                        "stored": troupe.act_schema.StrValue(
                            description="the stored token",
                            choices=["alpha"],
                        )
                    },
                )
            )
            act_tasks.append(task)
            try:
                observed.append(await task)
            except asyncio.CancelledError:
                cancelled.append(True)
                raise
            return ()

    class CommitProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                CommitActor,
                name="cancel-before-result-commit",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            cue = asyncio.create_task(handle.cue({}))
            while not _native()._agent_test_turn_gate_states()["settlement"]["arrived"]:
                await asyncio.sleep(0)
            assert act_tasks[0].cancel("before-result-commit")
            await _loop_barrier()
            _native()._agent_test_release_turn_settlement()
            with pytest.raises(asyncio.CancelledError):
                await cue
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(CommitProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert cancelled == [True]
    assert observed == []


def test_committed_outcome_keeps_caller_admission_until_publication(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, "act_two_turns")
    _native()._agent_test_hold_turn_outcome()
    runtime = _native()._Runtime()
    first_results: list[dict[str, object]] = []
    sibling_done: list[bool] = []
    sibling_busy: list[bool] = []

    class AdmissionActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            first = asyncio.create_task(
                self.act(
                    script="Remember the token alpha.",
                    output_schema={
                        "stored": troupe.act_schema.StrValue(
                            description="the stored token",
                            choices=["alpha"],
                        )
                    },
                )
            )
            while not _native()._agent_test_turn_gate_states()["outcome"]["arrived"]:
                await asyncio.sleep(0)
            sibling = asyncio.create_task(
                self.act(script="Do not queue this turn.", output_schema=_schema())
            )
            await _loop_barrier()
            sibling_done.append(sibling.done())
            if sibling.done():
                try:
                    await sibling
                except troupe.AgentSessionBusyError:
                    sibling_busy.append(True)
                else:
                    sibling_busy.append(False)
            else:
                sibling.cancel()
                with pytest.raises(asyncio.CancelledError):
                    await sibling
            _native()._agent_test_release_turn_outcome()
            first_results.append(await first)
            return ()

    class AdmissionProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                AdmissionActor,
                name="outcome-admission",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(AdmissionProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert sibling_done == [True]
    assert sibling_busy == [True]
    assert first_results == [{"stored": "alpha"}]


def test_process_exit_fails_a_turn_queued_before_worker_intake(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    release = workspace / "exit.fifo"
    os.mkfifo(release)
    _configure_mock(events, "exit_before_turn_intake", settlement_release=release)
    _native()._agent_test_hold_turn_intake()
    runtime = _native()._Runtime()
    failures: list[troupe.AgentSessionBrokenError] = []
    completed: list[bool] = []

    class ExitActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(script="Never submitted.", output_schema=_schema())
            except troupe.AgentSessionBrokenError as error:
                failures.append(error)
            return ()

    class ExitProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                ExitActor,
                name="exit-before-intake",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            await handle._agent_ready_for_test()  # type: ignore[attr-defined]
            while not _native()._agent_test_turn_gate_states()["intake"]["arrived"]:
                await asyncio.sleep(0)
            cue = asyncio.create_task(handle.cue({}))
            while not handle._agent_has_queued_turn_for_test():  # type: ignore[attr-defined]
                await asyncio.sleep(0)
            await _signal_fifo(release)
            await _wait_for_event(events, "process_exiting_before_turn_intake")
            while handle._agent_state_for_test() != "broken":  # type: ignore[attr-defined]
                await asyncio.sleep(0)
            assert await cue == ()
            completed.append(True)
            _native()._agent_test_release_turn_intake()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ExitProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert completed == [True]
    assert len(failures) == 1
    assert failures[0].code == "transport_lost"


def test_terminal_failure_before_turn_registration_cannot_orphan_active_caller(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    release = workspace / "exit.fifo"
    os.mkfifo(release)
    _configure_mock(events, "exit_before_turn_intake", settlement_release=release)
    _native()._agent_test_hold_turn_registration()
    runtime = _native()._Runtime()
    failures: list[troupe.AgentSessionBrokenError] = []

    class ExitActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(script="Armed but not registered.", output_schema=_schema())
            except troupe.AgentSessionBrokenError as error:
                failures.append(error)
            return ()

    class ExitProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                ExitActor,
                name="exit-before-registration",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            await handle._agent_ready_for_test()  # type: ignore[attr-defined]
            cue = asyncio.create_task(handle.cue({}))
            while not _native()._agent_test_turn_gate_states()["registration"][
                "arrived"
            ]:
                await asyncio.sleep(0)
            assert handle._agent_state_for_test() == "active"  # type: ignore[attr-defined]
            assert not handle._agent_has_queued_turn_for_test()  # type: ignore[attr-defined]
            await _signal_fifo(release)
            await _wait_for_event(events, "process_exiting_before_turn_intake")
            while handle._agent_state_for_test() != "broken":  # type: ignore[attr-defined]
                await asyncio.sleep(0)
            _native()._agent_test_release_turn_registration()
            assert await cue == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ExitProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert len(failures) == 1
    assert failures[0].code == "transport_lost"


def test_terminal_failure_before_result_commit_wins_the_caller_outcome(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, "act_two_turns")
    _native()._agent_test_hold_turn_settlement()
    _native()._agent_test_hold_turn_terminal_delivery()
    runtime = _native()._Runtime()
    results: list[dict[str, object]] = []
    failures: list[troupe.AgentSessionBrokenError] = []

    class ExitActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                results.append(
                    await self.act(
                        script="Remember the token alpha.",
                        output_schema={
                            "stored": troupe.act_schema.StrValue(
                                description="the stored token",
                                choices=["alpha"],
                            )
                        },
                    )
                )
            except troupe.AgentSessionBrokenError as error:
                failures.append(error)
            return ()

    class ExitProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                ExitActor,
                name="exit-before-result-commit",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            cue = asyncio.create_task(handle.cue({}))
            while not _native()._agent_test_turn_gate_states()["settlement"][
                "arrived"
            ]:
                await asyncio.sleep(0)
            terminal = asyncio.create_task(
                asyncio.to_thread(handle._agent_fail_transport_for_test)  # type: ignore[attr-defined]
            )
            while not _native()._agent_test_turn_gate_states()["terminal_delivery"][
                "arrived"
            ]:
                await asyncio.sleep(0)
            assert handle._agent_state_for_test() == "broken"  # type: ignore[attr-defined]

            _native()._agent_test_release_turn_settlement()
            while not cue.done():
                await asyncio.sleep(0)
            _native()._agent_test_release_turn_terminal_delivery()
            await terminal
            assert await cue == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ExitProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert results == []
    assert len(failures) == 1
    assert failures[0].code == "transport_lost"


def test_terminal_failure_tombstones_result_before_a_late_callback_fault(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, "act_cancel_during_callback_then_reuse")
    _native()._agent_test_hold_turn_terminal_delivery()
    runtime = _native()._Runtime()
    callback_entered: asyncio.Event
    callback_release: asyncio.Event
    captured: list[BaseException] = []
    callback_cause = LookupError("late callback fault")

    class FailingValue(troupe.act_schema.SchemaValue[int]):
        def __init__(self) -> None:
            super().__init__(description="a pending integer", json_kind="int64")

        def render_prompt(self) -> str:
            return "must be an integer"

        async def validate(self, value: int, /) -> None:
            del value
            callback_entered.set()
            await callback_release.wait()
            raise callback_cause

    class ExitActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="Hold validation while the transport fails.",
                    output_schema={"value": FailingValue()},
                )
            except (
                troupe.AgentSessionBrokenError,
                troupe.act_schema.SchemaCallbackError,
            ) as error:
                captured.append(error)
            return ()

    class ExitProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                ExitActor,
                name="terminal-before-callback-fault",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            cue = asyncio.create_task(handle.cue({}))
            await callback_entered.wait()
            terminal = asyncio.create_task(
                asyncio.to_thread(handle._agent_fail_transport_for_test)  # type: ignore[attr-defined]
            )
            while not _native()._agent_test_turn_gate_states()["terminal_delivery"][
                "arrived"
            ]:
                await asyncio.sleep(0)

            callback_release.set()
            await _wait_for_event(events, "callback_tool_result_finished")
            _native()._agent_test_release_turn_terminal_delivery()
            await terminal
            assert await cue == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        nonlocal callback_entered, callback_release
        callback_entered = asyncio.Event()
        callback_release = asyncio.Event()
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ExitProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert len(captured) == 1
    assert type(captured[0]) is troupe.AgentSessionBrokenError
    assert captured[0].code == "transport_lost"  # type: ignore[attr-defined]


def test_terminal_failure_before_cancellation_handoff_wins_the_caller_outcome(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    settlement_release = workspace / "settlement.fifo"
    os.mkfifo(settlement_release)
    _configure_mock(
        events,
        "act_cancel_then_reuse",
        settlement_release=settlement_release,
    )
    _native()._agent_test_hold_turn_terminal_delivery()
    runtime = _native()._Runtime()
    failures: list[troupe.AgentSessionBrokenError] = []

    class ExitActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(script="Keep this turn pending.", output_schema=_schema())
            except troupe.AgentSessionBrokenError as error:
                failures.append(error)
            return ()

    class ExitProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                ExitActor,
                name="terminal-before-cancel",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            cue = asyncio.create_task(handle.cue({}))
            await _wait_for_event(events, "prompt_received")
            terminal = asyncio.create_task(
                asyncio.to_thread(handle._agent_fail_transport_for_test)  # type: ignore[attr-defined]
            )
            while not _native()._agent_test_turn_gate_states()["terminal_delivery"][
                "arrived"
            ]:
                await asyncio.sleep(0)
            assert handle._agent_state_for_test() == "broken"  # type: ignore[attr-defined]

            assert cue.cancel("after-terminal")
            await _loop_barrier()
            _native()._agent_test_release_turn_terminal_delivery()
            await terminal
            assert await cue == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ExitProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert len(failures) == 1
    assert failures[0].code == "transport_lost"


def test_idle_clean_eof_breaks_the_ready_session_without_an_act(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    release = workspace / "eof.fifo"
    os.mkfifo(release)
    _configure_mock(events, "idle_clean_eof", settlement_release=release)
    runtime = _native()._Runtime()

    class EofProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                troupe.Actor,
                name="idle-clean-eof",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            await handle._agent_ready_for_test()  # type: ignore[attr-defined]
            await _signal_fifo(release)
            await _wait_for_event(events, "idle_stdout_closed")
            while handle._agent_state_for_test() == "ready":  # type: ignore[attr-defined]
                await asyncio.sleep(0)
            assert handle._agent_state_for_test() == "broken"  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(EofProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
