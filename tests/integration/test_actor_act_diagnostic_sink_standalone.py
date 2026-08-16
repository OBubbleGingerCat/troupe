from __future__ import annotations

import asyncio
import importlib
import json
import sys
from pathlib import Path
from typing import Any

import pytest


ROOT = Path(__file__).resolve().parents[2]
MOCK_AGENT = ROOT / "tests" / "support" / "mock_acp_agent.py"
BINDING = ROOT / "rust" / "src" / "diagnostic_runtime" / "sink_binding.rs"
HARNESS_TIMEOUT = 5.0


def _native() -> Any:
    return importlib.import_module("troupe._runtime")


def _events(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def _between(source: str, start: str, end: str) -> str:
    start_index = source.index(start)
    end_index = source.index(end, start_index)
    return source[start_index:end_index]


@pytest.fixture(autouse=True)
def _reset_test_launch(request: pytest.FixtureRequest) -> Any:
    if request.node.name == "test_standalone_b17_join_and_zero_allocation_path_are_source_frozen":
        yield
        return
    _native()._agent_test_reset_launch()
    yield
    _native()._agent_test_reset_launch()


def test_standalone_b17_join_and_zero_allocation_path_are_source_frozen() -> None:
    source = BINDING.read_text(encoding="utf-8")
    entry = _between(source, "pub(super) fn admit_act(", "pub(crate) fn production_capability(")
    standalone = _between(source, "fn standalone()", "fn prepare(")
    binding = _between(source, "fn bind(", "impl DiagnosticAdmissionCapability")
    delivery = _between(source, "fn deliver_projected(", "impl ActEventSubscriber")

    assert entry.index("if !binding.is_active()") < entry.index(
        "ActSinkAdmissionCapability::standalone()"
    )
    assert "UsageFinalizingObservationBridge::sink_only_with_subscribers" in standalone
    assert "CanonicalObservationBridge::sink_only_with_subscribers" not in standalone
    assert standalone.index("SinkOnlyDiagnosticHub::sink_only") < standalone.index(
        "UsageFinalizingObservationBridge::sink_only_with_subscribers"
    )
    assert standalone.index("UsageFinalizingObservationBridge::sink_only_with_subscribers") < (
        standalone.index("AgentDiagnosticObserver::new")
    )
    assert all(
        marker not in standalone
        for marker in (".troupe", "rusqlite", "TcpListener", "RegistryPublisher")
    )

    assert binding.index("reservation.publish") < binding.index("prepared.commit()")
    assert binding.index("prepared.commit()") < binding.index("usage.bind_act")
    assert "UsageObservationDisposition::LateIgnored" in binding
    assert delivery.index("try_enqueue_terminal") < delivery.index(
        "self.settle_terminal(act_outcome)"
    )


def test_standalone_full_chain_reuses_session_and_orders_each_act_terminal(
    tmp_path: Path,
) -> None:
    import troupe
    from troupe import diagnostics

    agent_events = tmp_path / "agent-events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _native()._agent_test_set_launch(
        program=sys.executable,
        args=[
            str(MOCK_AGENT),
            "--events",
            str(agent_events),
            "--scenario",
            "kimi_permission_matrix",
            "--provider",
            "kimi",
            "--mcp-revision",
            "2025-11-25",
        ],
    )
    callback_events: list[list[Any]] = [[], []]
    summaries: list[Any] = []
    observed: list[dict[str, object]] = []
    runtime = _native()._Runtime()

    class RecordingSink(diagnostics.DiagnosticSink):
        def __init__(self, index: int) -> None:
            super().__init__(
                capture=diagnostics.DiagnosticCapture(
                    agent_messages=True,
                    plans=True,
                    tool_calls=True,
                    result_validation=True,
                    usage=True,
                    custom_events=True,
                    tool_inputs=True,
                    tool_outputs=True,
                )
            )
            self.index = index

        def on_event(self, event: diagnostics.DiagnosticEvent, /) -> None:
            callback_events[self.index].append(event)

    sinks = [RecordingSink(0), RecordingSink(1)]

    class StandaloneActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            for sink in sinks:
                observed.append(
                    await self.act(
                        script="Return an empty object.",
                        output_schema={
                            "value": troupe.act_schema.Int64Value(
                                description="the completed task value"
                            )
                        },
                        diagnostic_sink=sink,
                    )
                )
                summaries.append(await sink.wait_closed())
            return ()

    class StandaloneProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                StandaloneActor,
                name="standalone-sink",
                agent_profile=troupe.AgentProfile(
                    agent="kimi",
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
            asyncio.shield(runtime.run(StandaloneProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert observed == [{"value": 11}, {"value": 11}]
    assert all(callback_events)
    combined = callback_events[0] + callback_events[1]
    sequences = [event.sequence for event in combined]
    assert sequences == sorted(sequences)
    assert len(sequences) == len(set(sequences))
    assert len({event.run_id for event in combined}) == 1
    assert len({event.scope.act_id for event in combined}) == 2
    assert None not in {event.scope.act_id for event in combined}

    for events in callback_events:
        act_starts = [
            event
            for event in events
            if event.kind == "span_started" and event.span_kind == "act.lifecycle"
        ]
        usages = [
            event for event in events if event.kind == "act_token_usage_finalized"
        ]
        assert len(act_starts) == 1
        assert len(usages) == 1
        act_finishes = [
            event
            for event in events
            if event.kind == "span_finished" and event.span_id == act_starts[0].sequence
        ]
        assert len(act_finishes) == 1
        assert usages[0].availability == "unavailable"
        assert usages[0].sequence < act_finishes[0].sequence

    result_transitions = {
        event.instant_kind
        for event in combined
        if event.kind == "instant_occurred" and event.instant_kind.startswith("result.")
    }
    assert {"result.submitted", "result.accepted"} <= result_transitions
    assert {"agent_message_delta", "agent_message_completed"} <= {
        event.kind for event in combined
    }
    assert any(
        event.kind == "span_started" and event.span_kind == "agent.thinking"
        for event in combined
    )
    assert any(
        event.kind == "span_started" and event.span_kind == "tool.call"
        for event in combined
    )
    assert len(summaries) == 2
    for summary in summaries:
        assert summary.act_outcome == "completed"
        assert summary.close_reason == "act_finished"
        assert summary.complete is True
        assert not hasattr(summary, "token_usage")

    agent_rows = _events(agent_events)
    prompts = [row for row in agent_rows if row["event"] == "prompt_received"]
    assert [row["turn"] for row in prompts] == [1, 2]
    assert len({row["session_id"] for row in prompts}) == 1
    assert sum(row["event"] == "session_new_received" for row in agent_rows) == 1
    assert not list(tmp_path.rglob(".troupe"))
    assert not list(tmp_path.rglob("*.sqlite"))


def test_none_path_does_not_create_standalone_diagnostics(tmp_path: Path) -> None:
    import troupe

    agent_events = tmp_path / "agent-events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _native()._agent_test_set_launch(
        program=sys.executable,
        args=[
            str(MOCK_AGENT),
            "--events",
            str(agent_events),
            "--scenario",
            "act_submit_results",
            "--results-json",
            "[{}]",
        ],
    )
    runtime = _native()._Runtime()

    class NoSinkActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            assert (
                await self.act(
                    script="Return an empty object.",
                    output_schema={},
                    diagnostic_sink=None,
                )
                == {}
            )
            return ()

    class NoSinkProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                NoSinkActor,
                name="no-standalone-sink",
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
            asyncio.shield(runtime.run(NoSinkProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert not list(tmp_path.rglob(".troupe"))
    assert not list(tmp_path.rglob("*.sqlite"))
