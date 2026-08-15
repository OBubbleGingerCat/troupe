from __future__ import annotations

import asyncio
import importlib
import inspect
import json
import sys
from pathlib import Path
from typing import Any

import pytest

import troupe
from troupe import diagnostics


ROOT = Path(__file__).resolve().parents[2]
MOCK_AGENT = ROOT / "tests" / "support" / "mock_acp_agent.py"
HARNESS_TIMEOUT = 5.0


def _native() -> Any:
    return importlib.import_module("troupe._runtime")


def _events(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def _base_state(sink: diagnostics.DiagnosticSink) -> str:
    descriptor = diagnostics.DiagnosticSink.__dict__["_DiagnosticSink__state"]
    return descriptor.__get__(sink, type(sink))


@pytest.fixture(autouse=True)
def _reset_test_launch() -> Any:
    _native()._agent_test_reset_launch()
    yield
    _native()._agent_test_reset_launch()


def test_actor_act_signature_exposes_keyword_only_optional_sink() -> None:
    parameters = inspect.signature(troupe.Actor.act).parameters
    assert tuple(parameters) == (
        "self",
        "script",
        "output_schema",
        "diagnostic_sink",
    )
    assert parameters["self"].kind is inspect.Parameter.POSITIONAL_ONLY
    assert all(
        parameters[name].kind is inspect.Parameter.KEYWORD_ONLY
        for name in ("script", "output_schema", "diagnostic_sink")
    )
    assert parameters["diagnostic_sink"].default is None


def test_actor_act_preflights_exact_initialized_sink_before_other_work(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _native()._agent_test_set_launch(
        program=sys.executable,
        args=[str(MOCK_AGENT), "--events", str(events), "--scenario", "ready"],
    )
    runtime = _native()._Runtime()
    actors: list[troupe.Actor] = []
    valid_sinks: list[diagnostics.DiagnosticSink] = []

    class RecordingSink(diagnostics.DiagnosticSink):
        def on_event(self, event: diagnostics.DiagnosticEvent, /) -> None:
            del event

    class MissingSuperSink(diagnostics.DiagnosticSink):
        def __init__(self) -> None:
            pass

        def on_event(self, event: diagnostics.DiagnosticEvent, /) -> None:
            del event

    class PoisonedOverridesSink(RecordingSink):
        @property
        def capture(self) -> diagnostics.DiagnosticCapture:
            raise AssertionError("public capture override was invoked")

        @property
        def state(self) -> str:
            raise AssertionError("public state override was invoked")

        def _diagnostic_require_lock(self) -> object:
            raise AssertionError("private override was invoked")

    class DuckSink:
        capture = diagnostics.DiagnosticCapture()
        state = "UNBOUND"

    class VirtualSink:
        pass

    diagnostics.DiagnosticSink.register(VirtualSink)
    assert isinstance(VirtualSink(), diagnostics.DiagnosticSink)

    class PreflightActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            actors.append(self)

            for invalid in (object(), DuckSink(), VirtualSink()):
                with pytest.raises(TypeError, match="DiagnosticSink"):
                    self.act(
                        script=object(),
                        output_schema=object(),
                        diagnostic_sink=invalid,
                    )

            missing = MissingSuperSink()
            with pytest.raises(diagnostics.DiagnosticSinkStateError) as captured:
                self.act(
                    script=object(),
                    output_schema=object(),
                    diagnostic_sink=missing,
                )
            assert captured.value.code == "uninitialized"

            schema_failure_sink = RecordingSink()
            with pytest.raises(TypeError):
                self.act(
                    script="schema failure",
                    output_schema=[],
                    diagnostic_sink=schema_failure_sink,
                )
            assert _base_state(schema_failure_sink) == "UNBOUND"

            capture = diagnostics.DiagnosticCapture(
                agent_messages=False,
                result_validation=False,
                custom_events=False,
                tool_inputs=True,
            )
            valid = PoisonedOverridesSink(capture=capture)
            call = self.act(
                script="never awaited",
                output_schema={
                    "value": troupe.act_schema.Int64Value(description="unused value")
                },
                diagnostic_sink=valid,
            )
            assert _base_state(valid) == "UNBOUND"
            call.close()
            assert _base_state(valid) == "UNBOUND"
            valid_sinks.append(valid)

            no_sink_call = self.act(
                script="None keeps existing behavior",
                output_schema={},
                diagnostic_sink=None,
            )
            no_sink_call.close()
            return ()

    class PreflightProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                PreflightActor,
                name="sink-preflight",
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

    actor = actors.pop()
    with pytest.raises(TypeError, match="DiagnosticSink"):
        actor.act(
            script=object(),
            output_schema=object(),
            diagnostic_sink=object(),
        )
    context_failure_sink = RecordingSink()
    with pytest.raises(troupe.CueContextError):
        actor.act(
            script="outside cue",
            output_schema={},
            diagnostic_sink=context_failure_sink,
        )
    assert _base_state(context_failure_sink) == "UNBOUND"
    assert _base_state(valid_sinks.pop()) == "UNBOUND"
    assert all(row["event"] != "prompt_received" for row in _events(events))
