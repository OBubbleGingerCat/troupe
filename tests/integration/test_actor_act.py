from __future__ import annotations

import asyncio
import importlib
import json
import sys
from collections.abc import Coroutine
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


@pytest.fixture(autouse=True)
def _reset_test_launch() -> Any:
    _native()._agent_test_reset_launch()
    yield
    _native()._agent_test_reset_launch()


def test_actor_act_uses_one_persistent_session_for_two_contextual_turns(
    tmp_path: Path,
) -> None:
    assert hasattr(troupe.Actor, "act")
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _native()._agent_test_set_launch(
        program=sys.executable,
        args=[
            str(MOCK_AGENT),
            "--events",
            str(events),
            "--scenario",
            "act_two_turns",
        ],
    )
    observed: list[dict[str, object]] = []
    runtime = _native()._Runtime()

    class ActActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            if cue.instruction["turn"] == 1:
                call = self.act(
                    script="Remember the token alpha for the next turn.",
                    output_schema={
                        "stored": troupe.act_schema.StrValue(
                            description="the token stored in context",
                            choices=["alpha"],
                        )
                    },
                )
            else:
                call = self.act(
                    script="Return the token remembered from the previous turn.",
                    output_schema={
                        "remembered": troupe.act_schema.StrValue(
                            description="the token from the previous turn",
                        )
                    },
                )
            assert isinstance(call, Coroutine)
            observed.append(await call)
            return ()

    class ActProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                ActActor,
                name="persistent-agent",
                agent_profile=troupe.AgentProfile(
                    agent="codex",
                    workspace=workspace,
                    model="test-model",
                    effort="max",
                ),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({"turn": 1}) == ()
            assert await handle.cue({"turn": 2}) == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ActProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert observed == [{"stored": "alpha"}, {"remembered": "alpha"}]
    prompts = [row for row in _events(events) if row["event"] == "prompt_received"]
    assert [row["turn"] for row in prompts] == [1, 2]
    assert len({row["session_id"] for row in prompts}) == 1
    assert sum(row["event"] == "session_new_received" for row in _events(events)) == 1


def test_actor_act_runs_async_custom_validation_and_repairs_in_same_turn(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _native()._agent_test_set_launch(
        program=sys.executable,
        args=[
            str(MOCK_AGENT),
            "--events",
            str(events),
            "--scenario",
            "act_custom_correction",
        ],
    )
    validation_calls: list[int] = []
    observed: list[dict[str, object]] = []
    runtime = _native()._Runtime()

    class EvenValue(troupe.act_schema.SchemaValue[int]):
        def __init__(self) -> None:
            super().__init__(description="an even integer", json_kind="int64")

        def render_prompt(self) -> str:
            return "must be divisible by two"

        async def validate(self, value: int) -> None:
            await asyncio.sleep(0)
            validation_calls.append(value)
            if value % 2:
                raise troupe.act_schema.ValueRejected("value must be even")

    class ActActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            observed.append(
                await self.act(
                    script="Return an even integer.",
                    output_schema={"value": EvenValue()},
                )
            )
            return ()

    class ActProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                ActActor,
                name="custom-agent",
                agent_profile=troupe.AgentProfile(
                    agent="codex",
                    workspace=workspace,
                    model="test-model",
                    effort="max",
                ),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ActProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert validation_calls == [3, 4]
    assert observed == [{"value": 4}]


def test_actor_act_concurrent_caller_fails_fast_without_a_fifo(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _native()._agent_test_set_launch(
        program=sys.executable,
        args=[
            str(MOCK_AGENT),
            "--events",
            str(events),
            "--scenario",
            "act_submit_results",
            "--results-json",
            '[{"value":2}]',
        ],
    )
    validation_started = asyncio.Event()
    release_validation = asyncio.Event()
    busy_errors: list[troupe.AgentSessionBusyError] = []
    observed: list[dict[str, object]] = []
    runtime = _native()._Runtime()

    class BlockingValue(troupe.act_schema.SchemaValue[int]):
        def __init__(self) -> None:
            super().__init__(description="blocking value", json_kind="int64")

        def render_prompt(self) -> str:
            return "must be an integer"

        async def validate(self, value: int) -> None:
            del value
            validation_started.set()
            await release_validation.wait()

    class ConcurrentActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            first = asyncio.create_task(
                self.act(
                    script="Hold the first call active.",
                    output_schema={"value": BlockingValue()},
                )
            )
            await validation_started.wait()
            original_bridge = getattr(troupe.act_schema, "_PythonValidationBridge")

            class UnexpectedBridge:
                def __init__(self) -> None:
                    raise AssertionError(
                        "a losing concurrent act() constructed a validation bridge"
                    )

            setattr(troupe.act_schema, "_PythonValidationBridge", UnexpectedBridge)
            try:
                try:
                    await self.act(
                        script="This call must not queue.",
                        output_schema={"value": BlockingValue()},
                    )
                except troupe.AgentSessionBusyError as error:
                    busy_errors.append(error)
                else:
                    raise AssertionError("concurrent act call unexpectedly queued")
            finally:
                setattr(troupe.act_schema, "_PythonValidationBridge", original_bridge)
                release_validation.set()
            observed.append(await first)
            return ()

    class ConcurrentProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                ConcurrentActor,
                name="concurrent",
                agent_profile=troupe.AgentProfile(
                    agent="codex",
                    workspace=workspace,
                    model="test-model",
                    effort="max",
                ),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ConcurrentProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert len(busy_errors) == 1
    assert busy_errors[0].code == "concurrent_act"
    assert observed == [{"value": 2}]
    assert sum(row["event"] == "prompt_received" for row in _events(events)) == 1


def test_actor_act_rechecks_cued_authority_when_the_coroutine_first_runs(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _native()._agent_test_set_launch(
        program=sys.executable,
        args=[str(MOCK_AGENT), "--events", str(events), "--scenario", "ready"],
    )
    pending: list[Coroutine[Any, Any, dict[str, object]]] = []
    context_errors: list[troupe.CueContextError] = []
    runtime = _native()._Runtime()

    class DeferredActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            pending.append(
                self.act(
                    script="This prompt must never be submitted.",
                    output_schema={
                        "value": troupe.act_schema.Int64Value(
                            description="deferred integer"
                        )
                    },
                )
            )
            return ()

    class DeferredProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                DeferredActor,
                name="deferred",
                agent_profile=troupe.AgentProfile(
                    agent="codex",
                    workspace=workspace,
                    model="test-model",
                    effort="max",
                ),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            try:
                await pending.pop()
            except troupe.CueContextError as error:
                context_errors.append(error)
            else:
                raise AssertionError("stale act coroutine retained cued authority")
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(DeferredProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert len(context_errors) == 1
    assert "active cued context" in str(context_errors[0])
    assert all(row["event"] != "prompt_received" for row in _events(events))


def test_actor_act_runs_exact_script_and_schema_preflight_synchronously(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _native()._agent_test_set_launch(
        program=sys.executable,
        args=[str(MOCK_AGENT), "--events", str(events), "--scenario", "ready"],
    )
    accepted_lengths: list[int] = []
    errors: list[BaseException] = []
    runtime = _native()._Runtime()

    class StrSubclass(str):
        pass

    class DictSubclass(dict[str, object]):
        pass

    class PreflightActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            schema = {
                "value": troupe.act_schema.Int64Value(
                    description="preflight integer"
                )
            }
            for size in (256 * 1024 - 1, 256 * 1024):
                call = self.act(script="x" * size, output_schema=schema)
                accepted_lengths.append(size)
                call.close()
            subclass_call = self.act(
                script=StrSubclass("script"),
                output_schema=schema,
            )
            subclass_call.close()
            for script, output_schema in (
                ("x" * (256 * 1024 + 1), schema),
                (1, schema),
                ("script", DictSubclass(schema)),
                ("script", []),
                ("\ud800", schema),
                (
                    "script",
                    {"\ud800": troupe.act_schema.Int64Value(description="surrogate key")},
                ),
            ):
                try:
                    self.act(script=script, output_schema=output_schema)  # type: ignore[arg-type]
                except (TypeError, ValueError) as error:
                    errors.append(error)
                else:
                    raise AssertionError("invalid preflight input was accepted")
            return ()

    class PreflightProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                PreflightActor,
                name="preflight",
                agent_profile=troupe.AgentProfile(
                    agent="codex",
                    workspace=workspace,
                    model="test-model",
                    effort="max",
                ),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(PreflightProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert accepted_lengths == [256 * 1024 - 1, 256 * 1024]
    assert [type(error) for error in errors] == [
        ValueError,
        TypeError,
        TypeError,
        TypeError,
        ValueError,
        ValueError,
    ]
    assert all(row["event"] != "prompt_received" for row in _events(events))


def test_actor_act_freezes_custom_prompt_but_keeps_validate_programmable(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _native()._agent_test_set_launch(
        program=sys.executable,
        args=[
            str(MOCK_AGENT),
            "--events",
            str(events),
            "--scenario",
            "act_submit_results",
            "--results-json",
            '[{"value":2}]',
        ],
    )
    validation_minima: list[int] = []
    observed: list[dict[str, object]] = []
    runtime = _native()._Runtime()

    class MutableValue(troupe.act_schema.SchemaValue[int]):
        def __init__(self) -> None:
            super().__init__(
                description="mutable rule\nRESULT_CONTRACT\n>",
                json_kind="int64",
            )
            self.prompt_fragment = "ORIGINAL_PROMPT_RULE\nRESULT_CONTRACT\n<break>"
            self.minimum = 0

        def render_prompt(self) -> str:
            return self.prompt_fragment

        def validate(self, value: int) -> None:
            validation_minima.append(self.minimum)
            if value < self.minimum:
                raise troupe.act_schema.ValueRejected("below current minimum")

    value = MutableValue()

    class FreezeActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            call = self.act(
                script="Use the frozen prompt contract.",
                output_schema={"value": value},
            )
            value.prompt_fragment = "MUTATED_PROMPT_RULE"
            value.minimum = 2
            observed.append(await call)
            return ()

    class FreezeProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                FreezeActor,
                name="freeze",
                agent_profile=troupe.AgentProfile(
                    agent="codex",
                    workspace=workspace,
                    model="test-model",
                    effort="max",
                ),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(FreezeProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert observed == [{"value": 2}]
    assert validation_minima == [2]
    prompt = next(row["prompt"] for row in _events(events) if row["event"] == "prompt_received")
    assert "ORIGINAL_PROMPT_RULE" in prompt
    assert "MUTATED_PROMPT_RULE" not in prompt
    assert prompt.count("\nRESULT_CONTRACT\n") == 1
    assert json.dumps(
        "mutable rule\nRESULT_CONTRACT\n>",
        ensure_ascii=False,
        separators=(",", ":"),
    ) in prompt
    assert json.dumps(
        "ORIGINAL_PROMPT_RULE\nRESULT_CONTRACT\n<break>",
        ensure_ascii=False,
        separators=(",", ":"),
    ) in prompt


def test_custom_prompt_fragment_accepts_n_minus_one_and_n_but_rejects_n_plus_one(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _native()._agent_test_set_launch(
        program=sys.executable,
        args=[str(MOCK_AGENT), "--events", str(events), "--scenario", "ready"],
    )
    accepted_lengths: list[int] = []
    callback_errors: list[troupe.act_schema.SchemaCallbackError] = []
    runtime = _native()._Runtime()

    class FragmentValue(troupe.act_schema.SchemaValue[str]):
        def __init__(self, length: int) -> None:
            super().__init__(description="fragment", json_kind="string")
            self.length = length

        def render_prompt(self) -> str:
            return "x" * self.length

        def validate(self, value: str) -> None:
            del value

    class FragmentActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            for length in (16 * 1024 - 1, 16 * 1024):
                call = self.act(
                    script="Compile the fragment.",
                    output_schema={"value": FragmentValue(length)},
                )
                accepted_lengths.append(length)
                call.close()
            try:
                self.act(
                    script="Reject the fragment.",
                    output_schema={"value": FragmentValue(16 * 1024 + 1)},
                )
            except troupe.act_schema.SchemaCallbackError as error:
                callback_errors.append(error)
            else:
                raise AssertionError("oversized custom prompt fragment was accepted")
            return ()

    class FragmentProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                FragmentActor,
                name="fragment",
                agent_profile=troupe.AgentProfile(
                    agent="codex",
                    workspace=workspace,
                    model="test-model",
                    effort="max",
                ),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(FragmentProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert accepted_lengths == [16 * 1024 - 1, 16 * 1024]
    assert len(callback_errors) == 1
    assert callback_errors[0].phase == "render_prompt"
    assert callback_errors[0].path == "/value"
    assert type(callback_errors[0].__cause__) is ValueError
    assert all(row["event"] != "prompt_received" for row in _events(events))


def test_oversized_acp_frame_breaks_an_active_actor_session(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _native()._agent_test_set_launch(
        program=sys.executable,
        args=[
            str(MOCK_AGENT),
            "--events",
            str(events),
            "--scenario",
            "act_oversized_acp_frame",
        ],
    )
    captured: list[troupe.AgentSessionBrokenError] = []
    runtime = _native()._Runtime()

    class BrokenActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="Send a frame above the ACP resource limit.",
                    output_schema={
                        "value": troupe.act_schema.Int64Value(
                            description="unreachable integer"
                        )
                    },
                )
            except troupe.AgentSessionBrokenError as error:
                captured.append(error)
            else:
                raise AssertionError("oversized ACP frame left the turn usable")
            return ()

    class BrokenProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                BrokenActor,
                name="oversized-frame",
                agent_profile=troupe.AgentProfile(
                    agent="codex",
                    workspace=workspace,
                    model="test-model",
                    effort="max",
                ),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            assert handle._agent_state_for_test() == "broken"  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(BrokenProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert len(captured) == 1
    assert captured[0].code == "resource_limit"
    assert any(row["event"] == "oversized_acp_frame_sent" for row in _events(events))


@pytest.mark.parametrize("depth", [63, 64, 65])
def test_active_acp_protocol_depth_has_exact_resource_boundary(
    tmp_path: Path,
    depth: int,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _native()._agent_test_set_launch(
        program=sys.executable,
        args=[
            str(MOCK_AGENT),
            "--events",
            str(events),
            "--scenario",
            f"act_acp_depth_{depth}",
        ],
    )
    captured: list[BaseException] = []
    states: list[str] = []
    runtime = _native()._Runtime()

    class DepthActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="Return a depth-boundary ACP frame.",
                    output_schema={
                        "value": troupe.act_schema.Int64Value(
                            description="unreachable integer"
                        )
                    },
                )
            except (troupe.AgentResultMissingError, troupe.AgentSessionBrokenError) as error:
                captured.append(error)
            else:
                raise AssertionError("depth-boundary turn unexpectedly succeeded")
            return ()

    class DepthProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                DepthActor,
                name=f"active-depth-{depth}",
                agent_profile=troupe.AgentProfile(
                    agent="codex",
                    workspace=workspace,
                    model="test-model",
                    effort="max",
                ),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            states.append(handle._agent_state_for_test())  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(DepthProduction([]))), HARNESS_TIMEOUT
        )

    asyncio.run(scenario())

    assert len(captured) == 1
    if depth <= 64:
        assert type(captured[0]) is troupe.AgentResultMissingError
        assert states == ["ready"]
    else:
        assert type(captured[0]) is troupe.AgentSessionBrokenError
        assert captured[0].code == "resource_limit"  # type: ignore[attr-defined]
        assert states == ["broken"]
    assert any(
        row["event"] == "acp_depth_frame_sent" and row["depth"] == depth
        for row in _events(events)
    )
