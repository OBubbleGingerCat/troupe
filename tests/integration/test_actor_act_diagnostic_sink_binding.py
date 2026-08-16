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
SINK_BINDING = ROOT / "rust" / "src" / "diagnostic_runtime" / "sink_binding.rs"
HARNESS_TIMEOUT = 5.0


def _native() -> Any:
    return importlib.import_module("troupe._runtime")


def _events(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def _base_state(diagnostics: Any, sink: Any) -> str:
    descriptor = diagnostics.DiagnosticSink.__dict__["_DiagnosticSink__state"]
    return descriptor.__get__(sink, type(sink))


def _between(source: str, start: str, end: str) -> str:
    start_index = source.index(start)
    end_index = source.index(end, start_index)
    return source[start_index:end_index]


@pytest.fixture(autouse=True)
def _reset_test_launch(request: pytest.FixtureRequest) -> Any:
    if request.node.name == "test_binding_transaction_and_inactive_fast_path_are_source_frozen":
        yield
        return
    _native()._agent_test_reset_launch()
    yield
    _native()._agent_test_reset_launch()


def test_admission_binds_once_and_rejects_reuse_before_prompt_submission(
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
            "ready",
        ],
    )
    _native()._agent_test_hold_opening()
    callback_started = threading.Event()
    callback_events: list[Any] = []
    reuse_codes: list[str] = []
    runtime = _native()._Runtime()

    class RecordingSink(diagnostics.DiagnosticSink):
        def on_event(self, event: diagnostics.DiagnosticEvent, /) -> None:
            callback_events.append(event)
            callback_started.set()

    sink = RecordingSink(
        capture=diagnostics.DiagnosticCapture(
            agent_messages=False,
            plans=False,
            tool_calls=True,
            result_validation=False,
            usage=False,
            custom_events=False,
            tool_inputs=True,
            tool_outputs=True,
        )
    )

    class BindingActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            await self.act(
                script="This turn remains before prompt submission.",
                output_schema={},
                diagnostic_sink=sink,
            )
            return ()

    class ReuseActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            with pytest.raises(diagnostics.DiagnosticSinkStateError) as captured:
                await self.act(
                    script="This prompt must never be submitted.",
                    output_schema={},
                    diagnostic_sink=sink,
                )
            reuse_codes.append(captured.value.code)
            return ()

    class SinkProduction(troupe.Production):
        async def scene(self) -> None:
            binding = self.cast_actor(
                BindingActor,
                name="sink-binding",
                agent_profile=troupe.AgentProfile(
                    agent="codex",
                    workspace=workspace,
                    model="test-model",
                    effort="max",
                ),
                actor_args=(),
                actor_kwargs={},
            )
            first = asyncio.create_task(binding.cue({}))
            while _base_state(diagnostics, sink) != "BOUND":
                await asyncio.sleep(0)
            assert await asyncio.to_thread(callback_started.wait, 1.0)

            reuse = self.cast_actor(
                ReuseActor,
                name="sink-reuse",
                agent_profile=troupe.AgentProfile(
                    agent="codex",
                    workspace=workspace,
                    model="test-model",
                    effort="max",
                ),
                actor_args=(),
                actor_kwargs={},
            )
            assert await reuse.cue({}) == ()
            assert all(
                row["event"] != "prompt_received" for row in _events(agent_events)
            )

            assert first.cancel("binding test complete")
            with pytest.raises(asyncio.CancelledError):
                await first
            _native()._agent_test_release_opening()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(SinkProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert reuse_codes == ["already_bound"]
    assert _base_state(diagnostics, sink) == "BOUND"
    assert callback_events
    act_ids = {event.scope.act_id for event in callback_events}
    assert len(act_ids) == 1
    assert None not in act_ids
    sequences = [event.sequence for event in callback_events]
    assert sequences == sorted(sequences)
    assert len(sequences) == len(set(sequences))
    assert all(row["event"] != "prompt_received" for row in _events(agent_events))


def test_binding_transaction_and_inactive_fast_path_are_source_frozen() -> None:
    source = SINK_BINDING.read_text(encoding="utf-8")
    entry = _between(source, "pub(super) fn admit_act(", "pub(crate) fn production_capability(")
    production = _between(
        source,
        "pub(crate) fn production_capability(",
        "pub(crate) struct ActSinkAdmissionCapability",
    )
    standalone = _between(source, "fn standalone()", "fn prepare(")
    binding = _between(source, "fn bind(", "impl DiagnosticAdmissionCapability")
    reservation = _between(source, "struct SubscriberReservation", "struct ActSinkSubscriber")
    delivery = _between(source, "fn deliver_projected(", "impl ActEventSubscriber")

    assert entry.index("if !binding.is_active()") < entry.index(
        "ActSinkAdmissionCapability::standalone()"
    )
    assert entry.index("runtime_producer::producer_for_binding(run)") < entry.index(
        "ActSinkAdmissionCapability::standalone()"
    )
    assert "context.install_act_subscriber_lookup(lookup)?" in production
    assert "profile: DiagnosticAdmissionProfile::ProductionDurable" in production
    assert "standalone: None" in production
    assert "SinkOnlyDiagnosticHub::sink_only(run_id, StandaloneReserver)" in standalone
    assert "DiagnosticRunContext::sink_only" in standalone
    assert "CanonicalObservationBridge::sink_only_with_subscribers" in standalone
    assert "profile: DiagnosticAdmissionProfile::SinkOnlyVolatile" in standalone
    assert all(
        marker not in standalone
        for marker in (".troupe", "rusqlite", "TcpListener", "RegistryPublisher")
    )

    assert binding.index("let (capture, request) = binding.into_parts()") < binding.index(
        "let prepared = self.prepare"
    )
    success = binding[binding.index("let prepared = self.prepare") :]
    ordered = (
        "self.prepare(run, cued, control)?",
        "registry.reserve(act_id.as_str())?",
        ".register_sink(py, act_id.clone(), callback)",
        ".install_failure_observer",
        ".install_diagnostic_context(context)",
        "bind_method.call1((sink,))?",
        "reservation.publish",
        "prepared.commit()",
    )
    positions = [success.index(fragment) for fragment in ordered]
    assert positions == sorted(positions)
    assert "if !self.published" in reservation
    assert "entries).remove(&self.act_id)" in reservation
    assert ".standalone\n                .as_ref()" in binding
    assert ".map(|resources| resources.observer.clone())" in binding
    assert "ToolPayloadCapturePolicy::new(capture.tool_inputs, capture.tool_outputs)" in binding
    assert "_DiagnosticSink__lock" in binding
    assert "project_act_event(&canonical, &self.act_scope, self.capture" in delivery
    assert "AdmissionClass::Structural" in delivery
    assert "self.record_drops(sequence, &outcome)" in delivery
    assert delivery.count("self.failure_facts.report_enqueue(Some(sequence))") == 2
    assert delivery.count("Err(DeliveryFailure::new(SINK_DELIVERY_FAILED))") == 2
    assert "emit_instant_without_act_subscriber" in source
    assert "subscriber_for(&self, act_id: &str)" in source
    assert "self.bound(act_id)" in source
