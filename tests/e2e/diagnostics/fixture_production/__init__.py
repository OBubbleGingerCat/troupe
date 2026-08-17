from __future__ import annotations

import asyncio
import dataclasses
from decimal import Decimal
import json
import os
from pathlib import Path
import sys
from typing import Any, cast
from uuid import UUID

import troupe
from troupe import diagnostics
import troupe._runtime as _runtime


def _append(path: Path, value: dict[str, object]) -> None:
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    try:
        os.write(descriptor, encoded + b"\n")
    finally:
        os.close(descriptor)


def _atomic_json(path: Path, value: dict[str, object]) -> None:
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(
        json.dumps(value, sort_keys=True, separators=(",", ":")),
        encoding="utf-8",
    )
    os.replace(temporary, path)


def _plain(value: object) -> object:
    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    if isinstance(value, (UUID, Decimal)):
        return str(value)
    if isinstance(value, tuple):
        return [_plain(item) for item in value]
    if dataclasses.is_dataclass(value):
        return {
            field.name: _plain(getattr(value, field.name))
            for field in dataclasses.fields(value)
        }
    return repr(value)


def _scope(event: diagnostics.DiagnosticEvent) -> dict[str, object]:
    scope = event.scope
    return {
        "scene_id": scope.scene_id,
        "actor_id": scope.actor_id,
        "cue_id": scope.cue_id,
        "effect_id": scope.effect_id,
        "act_id": scope.act_id,
        "tool_call_id": scope.tool_call_id,
        "session_generation": scope.session_generation,
    }


class RecordingSink(diagnostics.DiagnosticSink):
    def __init__(self, path: Path, turn: int, capture_payloads: bool) -> None:
        super().__init__(
            capture=diagnostics.DiagnosticCapture(
                tool_inputs=capture_payloads,
                tool_outputs=capture_payloads,
            )
        )
        self.path = path
        self.turn = turn

    def on_event(self, event: diagnostics.DiagnosticEvent, /) -> None:
        row: dict[str, object] = {
            "record": "event",
            "turn": self.turn,
            "kind": event.kind,
            "sequence": str(event.sequence),
            "scope": _scope(event),
        }
        for name in (
            "span_kind",
            "instant_kind",
            "counter_kind",
            "value",
            "availability",
            "unavailable_reason",
            "context_used_tokens",
            "context_window_tokens",
        ):
            if hasattr(event, name):
                row[name] = _plain(getattr(event, name))
        detail = getattr(event, "detail", None)
        if detail is not None:
            row["detail"] = _plain(detail)
        _append(self.path, row)


class TurnEffect(troupe.Effect):
    def __init__(self, turn: int, result: dict[str, Any]) -> None:
        self.turn = turn
        self.result = result


class ObservedActor(troupe.Actor):
    def __init__(self, sink_path: Path, capture_payloads: bool) -> None:
        self.sink_path = sink_path
        self.capture_payloads = capture_payloads

    async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
        turn = int(cue.instruction["turn"])
        sink = RecordingSink(self.sink_path, turn, self.capture_payloads)
        with diagnostics.span("e2e.turn", attributes={"turn": turn}):
            diagnostics.counter(
                "e2e.queue_depth",
                turn,
                unit="items",
                dimensions={"provider": str(cue.instruction["provider"])},
            )
            diagnostics.event(
                "e2e.turn_ready",
                attributes={"turn": turn, "phase": "before_act"},
            )
            result = await self.act(
                script=f"Complete deterministic diagnostics turn {turn}.",
                output_schema={
                    "value": troupe.act_schema.Int64Value(
                        description="the one-based turn number",
                        min=turn,
                        max=turn,
                    )
                },
                diagnostic_sink=sink,
            )
        summary = await sink.wait_closed()
        assert result == {"value": turn}
        _append(
            self.sink_path,
            {
                "record": "summary",
                "turn": turn,
                "act_id": summary.act_id,
                "act_outcome": summary.act_outcome,
                "close_reason": summary.close_reason,
                "complete": summary.complete,
                "delivered_events": summary.delivered_events,
                "first_delivered_sequence": summary.first_delivered_sequence,
                "last_delivered_sequence": summary.last_delivered_sequence,
                "result": result,
            },
        )
        return (
            self.make_effect(
                TurnEffect,
                effect_args=(turn, result),
                effect_kwargs={},
            ),
        )


DIAGNOSTIC_VIEWS: tuple[diagnostics.ViewSpec, ...] = (
    diagnostics.TimelineView(
        id="cue_timeline",
        title="Cue execution",
        query=diagnostics.TimelineQuery(
            source=diagnostics.SpanSource(kind="cue.execution"),
            filters=(diagnostics.OutcomeFilter(value="completed"),),
            group_by=diagnostics.GroupBy(dimension="actor"),
        ),
        time_range="run",
        scope="run",
    ),
    diagnostics.MetricView(
        id="act_input_tokens",
        title="Act input tokens",
        query=diagnostics.MetricQuery(
            source=diagnostics.ActTokenMetric(metric="input_tokens"),
            reducer="sum",
            group_by=diagnostics.GroupBy(dimension="act"),
        ),
        time_range="run",
        scope="selection",
    ),
    diagnostics.TableView(
        id="act_usage",
        title="Act usage",
        query=diagnostics.TableQuery(
            source=diagnostics.ActTokenUsageRows(),
            columns=(
                diagnostics.TableColumn(column="sequence"),
                diagnostics.TableColumn(column="act_id"),
                diagnostics.TableColumn(column="token", metric="input_tokens"),
            ),
            page_size=100,
        ),
        time_range="viewport",
        scope="run",
    ),
    diagnostics.TimeSeriesView(
        id="queue_depth",
        title="Queue depth",
        query=diagnostics.TimeSeriesQuery(
            source=diagnostics.CounterValue(
                selector=diagnostics.CounterSource(name="e2e.queue_depth")
            ),
            reducer="max",
            group_by=diagnostics.GroupBy(dimension="custom_dimension", key="provider"),
        ),
        time_range="viewport",
        scope="selection",
    ),
)


class Production(troupe.Production):
    diagnostic_views = DIAGNOSTIC_VIEWS

    def __init__(self, args: list[str]) -> None:
        if len(args) != 1:
            raise ValueError("expected one JSON configuration path")
        config = json.loads(Path(args[0]).read_text(encoding="utf-8"))
        self.provider = str(config["provider"])
        self.stage_path = Path(config["stage_path"])
        self.trigger_path = Path(config["trigger_path"])
        self.done_path = Path(config["done_path"])
        self.terminal_trigger_path = Path(config["terminal_trigger_path"])
        self.terminal_ready_path = Path(config["terminal_ready_path"])
        sink_path = Path(config["sink_path"])
        _runtime._agent_test_set_launch(
            program=sys.executable,
            args=[
                str(config["mock_path"]),
                "--provider",
                self.provider,
                "--mcp-revision",
                str(config["mcp_revision"]),
                "--events",
                str(config["agent_events_path"]),
            ],
        )
        profile = troupe.AgentProfile(
            agent=cast(Any, self.provider),
            workspace=str(config["workspace"]),
            model="test-model",
            effort="max",
        )
        self.actor = self.cast_actor(
            ObservedActor,
            name=f"observed-{self.provider}",
            agent_profile=profile,
            actor_args=(sink_path, bool(config["capture_tool_payloads"])),
            actor_kwargs={},
        )
        self.scene_number = 0

    async def _turn(self, number: int) -> None:
        (effect,) = await self.actor.cue({"turn": number, "provider": self.provider})
        turn_effect = cast(TurnEffect, effect)
        assert turn_effect.turn == number
        assert turn_effect.result == {"value": number}

    async def scene(self) -> None:
        self.scene_number += 1
        if self.scene_number == 1:
            await self._turn(1)
            _atomic_json(self.stage_path, {"scene": 1, "turn": 1})
            while not self.trigger_path.exists():
                await asyncio.sleep(0.01)
            await self._turn(2)
            return
        if self.scene_number == 2:
            await self._turn(3)
            _atomic_json(self.done_path, {"scene": 2, "turn": 3})
            while not self.terminal_trigger_path.exists():
                await asyncio.sleep(0.01)
            diagnostics.event(
                "e2e.terminal_follow_ready",
                attributes={"scene": 2, "phase": "before_shutdown"},
            )
            _atomic_json(self.terminal_ready_path, {"scene": 2, "ready": True})
            await asyncio.Event().wait()
        raise AssertionError(f"unexpected scene number {self.scene_number}")


def _record_import() -> None:
    raw = os.environ.get("TROUPE_DIAGNOSTICS_E2E_IMPORT_MARKER")
    if raw is None:
        return
    path = Path(raw)
    current = int(path.read_text(encoding="ascii")) if path.exists() else 0
    path.write_text(str(current + 1), encoding="ascii")


_record_import()
