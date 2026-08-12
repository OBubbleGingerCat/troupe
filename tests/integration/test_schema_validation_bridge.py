from __future__ import annotations

import asyncio
import contextvars
import importlib
import json
import os
import sys
from pathlib import Path
from typing import Any

import pytest

import troupe


ROOT = Path(__file__).resolve().parents[2]
MOCK_AGENT = ROOT / "tests" / "support" / "mock_acp_agent.py"
HARNESS_TIMEOUT = 5.0


def _native() -> Any:
    return importlib.import_module("troupe._runtime")


def _events(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def _configure_agent(
    tmp_path: Path,
    *,
    scenario: str,
    results: list[dict[str, object]],
    release: Path | None = None,
) -> tuple[Path, Path]:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    args = [
        str(MOCK_AGENT),
        "--events",
        str(events),
        "--scenario",
        scenario,
        "--results-json",
        json.dumps(results, separators=(",", ":")),
    ]
    if release is not None:
        args.extend(["--release", str(release)])
    _native()._agent_test_set_launch(
        program=sys.executable,
        args=args,
    )
    return events, workspace


def _profile(workspace: Path) -> troupe.AgentProfile:
    return troupe.AgentProfile(
        agent="codex",
        workspace=workspace,
        model="test-model",
        effort="max",
    )


def _run(runtime: Any, production: troupe.Production) -> None:
    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(production)),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())


async def _wait_for_event(path: Path, event: str) -> None:
    while not any(row["event"] == event for row in _events(path)):
        await asyncio.sleep(0)


async def _signal_fifo(path: Path) -> None:
    def write() -> None:
        with path.open("wb", buffering=0) as signal:
            signal.write(b"1")

    await asyncio.to_thread(write)


@pytest.fixture(autouse=True)
def _reset_test_launch() -> Any:
    _native()._agent_test_reset_launch()
    yield
    _native()._agent_test_reset_launch()


def test_sync_validation_inherits_context_and_receives_defensive_copy(
    tmp_path: Path,
) -> None:
    events, workspace = _configure_agent(
        tmp_path,
        scenario="act_submit_results",
        results=[{"payload": {"items": [1]}}],
    )
    marker: contextvars.ContextVar[str] = contextvars.ContextVar("schema_marker")
    callback_values: list[dict[str, object]] = []
    observed: list[dict[str, object]] = []
    runtime = _native()._Runtime()

    class MutableObject(troupe.act_schema.SchemaValue[dict[str, object]]):
        def __init__(self) -> None:
            super().__init__(description="mutable object", json_kind="object")

        def render_prompt(self) -> str:
            return "must contain an integer items array"

        def validate(self, value: dict[str, object]) -> None:
            assert marker.get() == "actor-context"
            callback_values.append(value)
            items = value["items"]
            assert isinstance(items, list)
            items.append(999)

    class ContextActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            marker.set("actor-context")
            observed.append(
                await self.act(
                    script="Return the payload.",
                    output_schema={"payload": MutableObject()},
                )
            )
            return ()

    class ContextProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                ContextActor,
                name="context",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    _run(runtime, ContextProduction([]))

    assert callback_values == [{"items": [1, 999]}]
    assert observed == [{"payload": {"items": [1]}}]
    tool_events = [row for row in _events(events) if row["event"] == "tool_result_received"]
    assert [(row["is_error"], row["text"]) for row in tool_events] == [
        (False, "result accepted")
    ]


def test_callback_faults_are_wrapped_and_never_exposed_to_agent(tmp_path: Path) -> None:
    events, workspace = _configure_agent(
        tmp_path,
        scenario="act_submit_results",
        results=[{"value": 2}],
    )
    raised_cause = LookupError("private callback detail")
    captured: list[troupe.act_schema.SchemaCallbackError] = []
    runtime = _native()._Runtime()

    class FaultValue(troupe.act_schema.SchemaValue[int]):
        def __init__(self, mode: str) -> None:
            super().__init__(description=mode, json_kind="int64")
            self.mode = mode

        def render_prompt(self) -> str:
            return "must be an integer"

        async def validate(self, value: int) -> object:
            del value
            await asyncio.sleep(0)
            if self.mode == "raise":
                raise raised_cause
            if self.mode == "self_cancel":
                raise asyncio.CancelledError("callback cancelled itself")
            return "not None"

    class FaultActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            try:
                await self.act(
                    script="Exercise callback failure handling.",
                    output_schema={"value": FaultValue(cue.instruction["mode"])},
                )
            except troupe.act_schema.SchemaCallbackError as error:
                captured.append(error)
            else:
                raise AssertionError("callback fault unexpectedly produced a result")
            return ()

    class FaultProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                FaultActor,
                name="faults",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            for mode in ("raise", "non_none", "self_cancel"):
                assert await handle.cue({"mode": mode}) == ()
            runtime.request_shutdown()

    _run(runtime, FaultProduction([]))

    assert [error.phase for error in captured] == ["validate"] * 3
    assert [error.path for error in captured] == ["/value"] * 3
    assert captured[0].__cause__ is raised_cause
    assert type(captured[1].__cause__) is TypeError
    assert type(captured[2].__cause__) is asyncio.CancelledError
    tool_events = [row for row in _events(events) if row["event"] == "tool_result_received"]
    assert len(tool_events) == 3
    assert all(row["is_error"] is True for row in tool_events)
    assert {row["text"] for row in tool_events} == {
        "schema validation callback failed"
    }


def test_reused_custom_node_renders_once_and_validates_each_occurrence_in_order(
    tmp_path: Path,
) -> None:
    _, workspace = _configure_agent(
        tmp_path,
        scenario="act_submit_results",
        results=[{"first": 1, "items": [2, 3]}],
    )
    render_calls = 0
    validation_calls: list[int] = []
    observed: list[dict[str, object]] = []
    runtime = _native()._Runtime()

    class OrderedInt(troupe.act_schema.SchemaValue[int]):
        def __init__(self) -> None:
            super().__init__(description="ordered integer", json_kind="int64")

        def render_prompt(self) -> str:
            nonlocal render_calls
            render_calls += 1
            return "must preserve declaration order"

        def validate(self, value: int) -> None:
            validation_calls.append(value)

    shared = OrderedInt()

    class OrderedActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            observed.append(
                await self.act(
                    script="Return all ordered values.",
                    output_schema={
                        "first": shared,
                        "items": troupe.act_schema.ListValue(
                            shared,
                            description="remaining ordered integers",
                        ),
                    },
                )
            )
            return ()

    class OrderedProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                OrderedActor,
                name="ordered",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    _run(runtime, OrderedProduction([]))

    assert render_calls == 1
    assert validation_calls == [1, 2, 3]
    assert observed == [{"first": 1, "items": [2, 3]}]


def test_first_custom_rejection_stops_later_jobs_in_the_same_submission(
    tmp_path: Path,
) -> None:
    _, workspace = _configure_agent(
        tmp_path,
        scenario="act_submit_results",
        results=[{"first": 1, "items": [2, 3]}],
    )
    validation_calls: list[int] = []
    captured: list[troupe.AgentResultError] = []
    runtime = _native()._Runtime()

    class RejectFirst(troupe.act_schema.SchemaValue[int]):
        def __init__(self) -> None:
            super().__init__(description="rejected integer", json_kind="int64")

        def render_prompt(self) -> str:
            return "must pass the first custom check"

        def validate(self, value: int) -> None:
            validation_calls.append(value)
            raise troupe.act_schema.ValueRejected("first occurrence rejected")

    shared = RejectFirst()

    class FirstRejectionActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="Reject the first occurrence.",
                    output_schema={
                        "first": shared,
                        "items": troupe.act_schema.ListValue(
                            shared,
                            description="later occurrences",
                        ),
                    },
                )
            except troupe.AgentResultError as error:
                captured.append(error)
            else:
                raise AssertionError("rejected custom result unexpectedly succeeded")
            return ()

    class FirstRejectionProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                FirstRejectionActor,
                name="first-rejection",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    _run(runtime, FirstRejectionProduction([]))

    assert validation_calls == [1]
    assert len(captured) == 1
    assert [(issue.path, issue.code) for issue in captured[0].issues] == [
        ("/first", "custom_validation")
    ]


def test_native_issue_skips_every_custom_callback(tmp_path: Path) -> None:
    _, workspace = _configure_agent(
        tmp_path,
        scenario="act_submit_results",
        results=[{"native": "wrong", "custom": 2}],
    )
    validation_calls: list[int] = []
    captured: list[troupe.AgentResultError] = []
    runtime = _native()._Runtime()

    class CustomInt(troupe.act_schema.SchemaValue[int]):
        def __init__(self) -> None:
            super().__init__(description="custom integer", json_kind="int64")

        def render_prompt(self) -> str:
            return "must pass custom validation"

        def validate(self, value: int) -> None:
            validation_calls.append(value)

    class SkipActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="Fail native validation first.",
                    output_schema={
                        "native": troupe.act_schema.Int64Value(
                            description="native integer"
                        ),
                        "custom": CustomInt(),
                    },
                )
            except troupe.AgentResultError as error:
                captured.append(error)
            else:
                raise AssertionError("native-invalid result unexpectedly succeeded")
            return ()

    class SkipProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                SkipActor,
                name="skip-custom",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    _run(runtime, SkipProduction([]))

    assert validation_calls == []
    assert len(captured) == 1
    assert [(issue.path, issue.code) for issue in captured[0].issues] == [
        ("/native", "type_mismatch")
    ]


@pytest.mark.parametrize(
    ("json_kind", "candidate", "python_type"),
    [
        ("string", "value", str),
        ("int64", 7, int),
        ("float64", 1, float),
        ("float64", 1.5, float),
        ("bool", True, bool),
        ("array", [1], list),
        ("object", {"nested": 1}, dict),
    ],
)
def test_each_custom_json_kind_reaches_callback_as_native_python_type(
    tmp_path: Path,
    json_kind: str,
    candidate: object,
    python_type: type[object],
) -> None:
    _, workspace = _configure_agent(
        tmp_path,
        scenario="act_submit_results",
        results=[{"value": candidate}],
    )
    callback_values: list[object] = []
    observed: list[dict[str, object]] = []
    runtime = _native()._Runtime()

    class CustomKind(troupe.act_schema.SchemaValue[object]):
        def __init__(self) -> None:
            super().__init__(description="custom kind", json_kind=json_kind)

        def render_prompt(self) -> str:
            return "must match the native JSON kind"

        def validate(self, value: object) -> None:
            callback_values.append(value)

    class KindActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            observed.append(
                await self.act(
                    script="Return the requested JSON kind.",
                    output_schema={"value": CustomKind()},
                )
            )
            return ()

    class KindProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                KindActor,
                name="kind",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    _run(runtime, KindProduction([]))

    assert len(callback_values) == 1
    assert type(callback_values[0]) is python_type
    assert observed == [{"value": candidate}]


@pytest.mark.parametrize("json_kind", ["array", "object"])
def test_custom_containers_preserve_every_unsigned_json_integer_exactly(
    tmp_path: Path,
    json_kind: str,
) -> None:
    integers = [2**63 - 1, 2**63, 2**64 - 1]
    candidate: object = (
        integers
        if json_kind == "array"
        else {"signed_max": integers[0], "unsigned_mid": integers[1], "unsigned_max": integers[2]}
    )
    _, workspace = _configure_agent(
        tmp_path,
        scenario="act_submit_results",
        results=[{"value": candidate}],
    )
    callback_values: list[object] = []
    observed: list[dict[str, object]] = []
    runtime = _native()._Runtime()

    class ExactContainer(troupe.act_schema.SchemaValue[object]):
        def __init__(self) -> None:
            super().__init__(description="lossless container", json_kind=json_kind)

        def render_prompt(self) -> str:
            return "must preserve every JSON integer token exactly"

        def validate(self, value: object) -> None:
            callback_values.append(value)

    class ExactActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            observed.append(
                await self.act(
                    script="Return all integer boundary values.",
                    output_schema={"value": ExactContainer()},
                )
            )
            return ()

    class ExactProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                ExactActor,
                name=f"unsigned-{json_kind}",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    _run(runtime, ExactProduction([]))

    assert callback_values == [candidate]
    assert observed == [{"value": candidate}]


@pytest.mark.parametrize(
    ("json_kind", "candidate"),
    [
        ("string", 1),
        ("int64", 1.0),
        ("float64", "1"),
        ("bool", 1),
        ("array", {}),
        ("object", []),
    ],
)
def test_each_custom_json_kind_mismatch_is_rejected_before_callback(
    tmp_path: Path,
    json_kind: str,
    candidate: object,
) -> None:
    _, workspace = _configure_agent(
        tmp_path,
        scenario="act_submit_results",
        results=[{"value": candidate}],
    )
    callback_values: list[object] = []
    captured: list[troupe.AgentResultError] = []
    runtime = _native()._Runtime()

    class CustomKind(troupe.act_schema.SchemaValue[object]):
        def __init__(self) -> None:
            super().__init__(description="custom kind", json_kind=json_kind)

        def render_prompt(self) -> str:
            return "must match the native JSON kind"

        def validate(self, value: object) -> None:
            callback_values.append(value)

    class KindActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="Return a mismatched JSON kind.",
                    output_schema={"value": CustomKind()},
                )
            except troupe.AgentResultError as error:
                captured.append(error)
            else:
                raise AssertionError("custom kind mismatch unexpectedly succeeded")
            return ()

    class KindProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                KindActor,
                name="kind-mismatch",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    _run(runtime, KindProduction([]))

    assert callback_values == []
    assert len(captured) == 1
    assert [(issue.path, issue.code) for issue in captured[0].issues] == [
        ("/value", "type_mismatch")
    ]


def test_concurrent_submissions_are_serialized_within_one_result_slot(
    tmp_path: Path,
) -> None:
    events, workspace = _configure_agent(
        tmp_path,
        scenario="act_submit_concurrent",
        results=[{"value": 2}, {"value": 4}],
    )
    started = asyncio.Event()
    release = asyncio.Event()
    validation_calls: list[int] = []
    observed: list[dict[str, object]] = []
    runtime = _native()._Runtime()

    class BlockingEven(troupe.act_schema.SchemaValue[int]):
        def __init__(self) -> None:
            super().__init__(description="even integer", json_kind="int64")

        def render_prompt(self) -> str:
            return "must be even"

        async def validate(self, value: int) -> None:
            validation_calls.append(value)
            started.set()
            await release.wait()

    class SerialActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue

            async def release_first() -> None:
                await started.wait()
                assert len(validation_calls) == 1
                release.set()

            releaser = asyncio.create_task(release_first())
            observed.append(
                await self.act(
                    script="Return one even integer.",
                    output_schema={"value": BlockingEven()},
                )
            )
            await releaser
            return ()

    class SerialProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                SerialActor,
                name="serial",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    _run(runtime, SerialProduction([]))

    assert len(validation_calls) == 1
    assert observed in ([{"value": 2}], [{"value": 4}])
    tool_events = [row for row in _events(events) if row["event"] == "tool_result_received"]
    assert sorted(row["is_error"] for row in tool_events) == [False, True]


def test_authoritative_settlement_cancels_the_actual_callback_bridge(
    tmp_path: Path,
) -> None:
    events, workspace = _configure_agent(
        tmp_path,
        scenario="act_settle_during_callback",
        results=[],
    )
    started_fifo = workspace / "callback-started.fifo"
    finished_fifo = workspace / "tool-result-finished.fifo"
    os.mkfifo(started_fifo)
    os.mkfifo(finished_fifo)
    callback_cancelled = asyncio.Event()
    captured: list[troupe.AgentResultMissingError] = []
    runtime = _native()._Runtime()

    class PendingValue(troupe.act_schema.SchemaValue[int]):
        def __init__(self) -> None:
            super().__init__(description="pending integer", json_kind="int64")

        def render_prompt(self) -> str:
            return "must remain pending until authoritative settlement"

        async def validate(self, value: int) -> None:
            del value
            try:
                with started_fifo.open("wb", buffering=0) as signal:
                    signal.write(b"1")
                await asyncio.Event().wait()
            finally:
                callback_cancelled.set()

    class SettlementActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="End the turn while validation remains pending.",
                    output_schema={"value": PendingValue()},
                )
            except troupe.AgentResultMissingError as error:
                captured.append(error)
            else:
                raise AssertionError("settlement unexpectedly accepted a pending result")
            await callback_cancelled.wait()
            with finished_fifo.open("rb", buffering=0) as signal:
                assert signal.read(1) == b"1"
            return ()

    class SettlementProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                SettlementActor,
                name="settlement-close",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    _run(runtime, SettlementProduction([]))

    assert len(captured) == 1
    tool_event = next(row for row in _events(events) if row["event"] == "tool_result_received")
    assert tool_event["is_error"] is True
    assert tool_event["text"] == "turn is settling"


def test_custom_validation_can_run_concurrently_for_different_actors(
    tmp_path: Path,
) -> None:
    events, workspace = _configure_agent(
        tmp_path,
        scenario="act_submit_results",
        results=[{"value": 2}],
    )
    release = asyncio.Event()
    started: list[int] = []
    observed: list[dict[str, object]] = []
    runtime = _native()._Runtime()

    class CrossActorValue(troupe.act_schema.SchemaValue[int]):
        def __init__(self) -> None:
            super().__init__(description="cross actor value", json_kind="int64")

        def render_prompt(self) -> str:
            return "must validate independently"

        async def validate(self, value: int) -> None:
            started.append(value)
            if len(started) == 2:
                release.set()
            await release.wait()

    shared = CrossActorValue()

    class ParallelActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            observed.append(
                await self.act(
                    script="Return the shared value.",
                    output_schema={"value": shared},
                )
            )
            return ()

    class ParallelProduction(troupe.Production):
        async def scene(self) -> None:
            first = self.cast_actor(
                ParallelActor,
                name="first",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            second = self.cast_actor(
                ParallelActor,
                name="second",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await asyncio.gather(first.cue({}), second.cue({})) == [(), ()]
            runtime.request_shutdown()

    _run(runtime, ParallelProduction([]))

    assert started == [2, 2]
    assert observed == [{"value": 2}, {"value": 2}]
    tool_events = [row for row in _events(events) if row["event"] == "tool_result_received"]
    assert len(tool_events) == 2
    assert all(row["is_error"] is False for row in tool_events)


def test_value_rejected_is_bounded_and_end_turn_reports_invalid_result(
    tmp_path: Path,
) -> None:
    events, workspace = _configure_agent(
        tmp_path,
        scenario="act_submit_results",
        results=[{"value": 3}],
    )
    captured: list[troupe.AgentResultError] = []
    runtime = _native()._Runtime()

    class AlwaysRejected(troupe.act_schema.SchemaValue[int]):
        def __init__(self) -> None:
            super().__init__(description="rejected integer", json_kind="int64")

        def render_prompt(self) -> str:
            return "must satisfy the private business rule"

        def validate(self, value: int) -> None:
            del value
            raise troupe.act_schema.ValueRejected("界" * 2_000)

    class RejectedActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="Submit a value.",
                    output_schema={"value": AlwaysRejected()},
                )
            except troupe.AgentResultError as error:
                captured.append(error)
            else:
                raise AssertionError("invalid result unexpectedly succeeded")
            return ()

    class RejectedProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                RejectedActor,
                name="rejected",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    _run(runtime, RejectedProduction([]))

    assert len(captured) == 1
    error = captured[0]
    assert type(error) is troupe.AgentResultError
    assert error.code == "invalid_result"
    assert error.invalid_calls == 1
    assert error.details_truncated is True
    assert len(error.issues) == 1
    assert error.issues[0].path == "/value"
    assert error.issues[0].code == "custom_validation"
    assert len(error.issues[0].message.encode("utf-8")) <= 4 * 1024
    tool_event = next(row for row in _events(events) if row["event"] == "tool_result_received")
    assert tool_event["is_error"] is True
    assert len(tool_event["text"].encode("utf-8")) <= 32 * 1024


def test_run_binding_teardown_uses_a_fresh_bridge_after_rebind(
    tmp_path: Path,
) -> None:
    settlement_release = tmp_path / "settlement.fifo"
    os.mkfifo(settlement_release)
    events, workspace = _configure_agent(
        tmp_path,
        scenario="act_cancel_during_callback_then_reuse",
        results=[],
        release=settlement_release,
    )
    first_callback_entered: asyncio.Event
    first_callback_cancelled: asyncio.Event
    first_callback_release: asyncio.Event
    first_late_returns: list[int] = []
    second_validations: list[int] = []
    observed: list[dict[str, object]] = []
    scene_number = 0
    handle: troupe.ActorHandle | None = None
    active_runtime: Any = None

    class FirstBindingValue(troupe.act_schema.SchemaValue[int]):
        def __init__(self) -> None:
            super().__init__(description="first binding value", json_kind="int64")

        def render_prompt(self) -> str:
            return "must remain pending across RunBinding teardown"

        async def validate(self, value: int, /) -> None:
            first_callback_entered.set()
            try:
                await asyncio.Event().wait()
            except asyncio.CancelledError:
                first_callback_cancelled.set()
                await first_callback_release.wait()
            first_late_returns.append(value)

    class SecondBindingValue(troupe.act_schema.SchemaValue[int]):
        def __init__(self) -> None:
            super().__init__(description="second binding value", json_kind="int64")

        def render_prompt(self) -> str:
            return "must validate on the fresh bridge"

        def validate(self, value: int, /) -> None:
            second_validations.append(value)

    class ReboundActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            turn = int(cue.instruction["turn"])
            schema: troupe.act_schema.SchemaValue[int]
            schema = FirstBindingValue() if turn == 1 else SecondBindingValue()
            observed.append(
                await self.act(
                    script=f"Return the value for binding {turn}.",
                    output_schema={"value": schema},
                )
            )
            return ()

    class ReboundProduction(troupe.Production):
        async def scene(self) -> None:
            nonlocal handle, scene_number
            scene_number += 1
            if scene_number == 1:
                handle = self.cast_actor(
                    ReboundActor,
                    name="run-binding-bridge",
                    agent_profile=_profile(workspace),
                    actor_args=(),
                    actor_kwargs={},
                )
                asyncio.create_task(handle.cue({"turn": 1}))
                await asyncio.Event().wait()
                raise AssertionError("the first RunBinding scene was not cancelled")

            assert handle is not None
            assert await handle.cue({"turn": 2}) == ()
            await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
            active_runtime.request_shutdown()

    async def scenario() -> None:
        nonlocal active_runtime
        nonlocal first_callback_cancelled, first_callback_entered, first_callback_release
        first_callback_entered = asyncio.Event()
        first_callback_cancelled = asyncio.Event()
        first_callback_release = asyncio.Event()
        production = ReboundProduction([])
        first_runtime = _native()._Runtime()
        active_runtime = first_runtime
        first_run = asyncio.ensure_future(first_runtime.run(production))
        await first_callback_entered.wait()

        first_runtime.request_shutdown()
        await first_run
        await first_callback_cancelled.wait()
        assert first_late_returns == []
        await _wait_for_event(events, "cancel_received")
        await _signal_fifo(settlement_release)

        second_runtime = _native()._Runtime()
        active_runtime = second_runtime
        await second_runtime.run(production)
        assert observed == [{"value": 2}]
        assert second_validations == [2]
        assert first_late_returns == []

        first_callback_release.set()
        while first_late_returns != [1]:
            await asyncio.sleep(0)
        assert observed == [{"value": 2}]

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_production_teardown_does_not_join_a_validator_that_ignored_cancel(
    tmp_path: Path,
) -> None:
    _, workspace = _configure_agent(
        tmp_path,
        scenario="act_cancel_during_callback_then_reuse",
        results=[],
    )
    callback_entered: asyncio.Event
    callback_cancelled: asyncio.Event
    callback_release: asyncio.Event
    late_returns: list[int] = []
    runtime = _native()._Runtime()

    class IgnoredCancelValue(troupe.act_schema.SchemaValue[int]):
        def __init__(self) -> None:
            super().__init__(description="pending teardown value", json_kind="int64")

        def render_prompt(self) -> str:
            return "must remain pending until Production teardown"

        async def validate(self, value: int, /) -> None:
            callback_entered.set()
            try:
                await asyncio.Event().wait()
            except asyncio.CancelledError:
                callback_cancelled.set()
                await callback_release.wait()
            late_returns.append(value)

    class TeardownActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            await self.act(
                script="Keep custom validation pending.",
                output_schema={"value": IgnoredCancelValue()},
            )
            return ()

    class TeardownProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                TeardownActor,
                name="production-bridge-teardown",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            cue = asyncio.create_task(handle.cue({}))
            await callback_entered.wait()

            await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
            await callback_cancelled.wait()
            assert late_returns == []
            assert cue.done()
            with pytest.raises(troupe.AgentSessionBrokenError) as raised:
                await cue
            assert raised.value.code == "transport_lost"

            callback_release.set()
            while late_returns != [1]:
                await asyncio.sleep(0)
            runtime.request_shutdown()

    async def scenario() -> None:
        nonlocal callback_entered, callback_cancelled, callback_release
        callback_entered = asyncio.Event()
        callback_cancelled = asyncio.Event()
        callback_release = asyncio.Event()
        await runtime.run(TeardownProduction([]))

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))
