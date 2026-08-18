from __future__ import annotations

import asyncio
import json
import math
from pathlib import Path
from typing import cast

import troupe
from troupe import diagnostics

from .custom import record_batch
from .sink import EvaluationSink, JsonValue
from .views import DIAGNOSTIC_VIEWS


DEFAULT_INTERVAL_SECONDS = 30.0
PROBE_MARKER = "troupe-diagnostics-ready"


def parse_interval_seconds(args: list[str]) -> float:
    if len(args) > 1:
        raise ValueError("expected at most one argument: SCENE_INTERVAL_SECONDS")
    if not args:
        return DEFAULT_INTERVAL_SECONDS
    try:
        interval = float(args[0])
    except ValueError as error:
        raise ValueError("SCENE_INTERVAL_SECONDS must be a number") from error
    if not math.isfinite(interval) or interval < 0:
        raise ValueError("SCENE_INTERVAL_SECONDS must be finite and non-negative")
    return interval


class ObservedTurn(troupe.Effect):
    def __init__(
        self,
        scene_number: int,
        operation: str,
        result: dict[str, JsonValue],
        observation: dict[str, JsonValue],
    ) -> None:
        self.scene_number = scene_number
        self.operation = operation
        self.result = result
        self.observation = observation


class DiagnosticActor(troupe.Actor):
    @staticmethod
    def script(*, scene_number: int, operation: str) -> str:
        if operation == "probe":
            return (
                f"This is diagnostics showcase Scene {scene_number}. Use the shell tool "
                f"to run exactly `printf '{PROBE_MARKER}\\n'`. Do not modify any file. "
                "Then submit the Scene number, operation 'probe', and the exact printed "
                "marker through the Troupe result tool."
            )
        if operation == "recall":
            return (
                f"This is diagnostics showcase Scene {scene_number}. Do not use any tool. "
                "Recall the marker from the immediately preceding probe turn in this "
                "persistent Actor session, then submit the Scene number, operation "
                "'recall', and that marker through the Troupe result tool."
            )
        raise ValueError(f"unknown diagnostics operation: {operation!r}")

    async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
        scene_number = int(cue.instruction["scene_number"])
        operation = str(cue.instruction["operation"])
        planned_depth = int(cue.instruction["planned_depth"])
        sink = EvaluationSink()

        record_batch(queue_depth=planned_depth, region=operation)
        result = await self.act(
            script=self.script(
                scene_number=scene_number,
                operation=operation,
            ),
            output_schema={
                "scene": troupe.act_schema.Int64Value(
                    description="the current diagnostics showcase Scene number",
                    min=scene_number,
                    max=scene_number,
                ),
                "operation": troupe.act_schema.StrValue(
                    description="the operation requested by the current Cue",
                    choices=[operation],
                ),
                "marker": troupe.act_schema.StrValue(
                    description="the exact diagnostics probe marker",
                    choices=[PROBE_MARKER],
                ),
            },
            diagnostic_sink=sink,
        )

        summary = await sink.wait_closed()
        message_characters = len("".join(sink.message_text))
        context = sink.context_samples[-1] if sink.context_samples else None
        usage = sink.final_usage
        usage_report: dict[str, JsonValue] | None = None
        if usage is not None:
            usage_report = {
                "availability": usage.availability,
                "provider_total_tokens": usage.provider_total_tokens,
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "thought_tokens": usage.thought_tokens,
                "cached_read_tokens": usage.cached_read_tokens,
                "cached_write_tokens": usage.cached_write_tokens,
            }
        observation: dict[str, JsonValue] = {
            "act_id": summary.act_id,
            "sink_complete": summary.complete,
            "delivered_events": summary.delivered_events,
            "message_characters": message_characters,
            "tool_calls": sink.tool_calls,
            "context_used_tokens": (
                context.context_used_tokens if context is not None else None
            ),
            "context_window_tokens": (
                context.context_window_tokens if context is not None else None
            ),
            "usage": usage_report,
        }
        diagnostics.counter(
            "example.message_characters",
            message_characters,
            unit="characters",
            dimensions={"operation": operation},
        )
        diagnostics.event(
            "example.act_observed",
            attributes={
                "scene": scene_number,
                "operation": operation,
                "sink_complete": summary.complete,
                "tool_calls": sink.tool_calls,
                "usage_availability": (
                    usage.availability if usage is not None else "missing"
                ),
            },
        )
        return (
            self.make_effect(
                ObservedTurn,
                effect_args=(scene_number, operation, result, observation),
                effect_kwargs={},
            ),
        )


class Production(troupe.Production):
    diagnostic_views = DIAGNOSTIC_VIEWS

    def __init__(self, args: list[str]) -> None:
        self.interval_seconds = parse_interval_seconds(args)
        self.scene_number = 0
        self.completed_scenes = 0
        profile = troupe.AgentProfile(
            agent="codex",
            workspace=Path.cwd(),
            model="gpt-5.6-sol",
            effort="medium",
        )
        self.worker = self.cast_actor(
            DiagnosticActor,
            name="diagnostic-worker",
            agent_profile=profile,
            actor_args=(),
            actor_kwargs={},
        )

    async def start(self) -> None:
        diagnostics.event(
            "example.showcase_started",
            attributes={"interval_seconds": self.interval_seconds},
        )
        print(
            json.dumps(
                {
                    "diagnostics_showcase": "started",
                    "scene_interval_seconds": self.interval_seconds,
                    "warning": "runs two real provider turns per Scene until Ctrl+C",
                },
                sort_keys=True,
            ),
            flush=True,
        )

    async def scene(self) -> None:
        self.scene_number += 1
        scene_number = self.scene_number
        with diagnostics.span(
            "example.scene_cycle",
            attributes={"scene": scene_number},
        ):
            diagnostics.event(
                "example.scene_started",
                attributes={"scene": scene_number, "queued_cues": 2},
            )

        probe_task = asyncio.create_task(
            self.worker.cue(
                {
                    "scene_number": scene_number,
                    "operation": "probe",
                    "planned_depth": 2,
                }
            )
        )
        await asyncio.sleep(0)
        recall_task = asyncio.create_task(
            self.worker.cue(
                {
                    "scene_number": scene_number,
                    "operation": "recall",
                    "planned_depth": 1,
                }
            )
        )
        probe_effects, recall_effects = await asyncio.gather(
            probe_task,
            recall_task,
        )
        probe = cast(ObservedTurn, probe_effects[0])
        recall = cast(ObservedTurn, recall_effects[0])
        print(
            json.dumps(
                {
                    "scene": scene_number,
                    "turns": [
                        {
                            "operation": probe.operation,
                            "result": probe.result,
                            "diagnostics": probe.observation,
                        },
                        {
                            "operation": recall.operation,
                            "result": recall.result,
                            "diagnostics": recall.observation,
                        },
                    ],
                },
                sort_keys=True,
            ),
            flush=True,
        )
        await asyncio.sleep(self.interval_seconds)
        self.completed_scenes += 1
        diagnostics.counter(
            "example.completed_scenes",
            self.completed_scenes,
            unit="scenes",
        )
        diagnostics.event(
            "example.scene_completed",
            attributes={
                "scene": scene_number,
                "acts": 2,
                "sink_complete": bool(
                    probe.observation["sink_complete"]
                    and recall.observation["sink_complete"]
                ),
            },
        )

    async def stop(self) -> None:
        diagnostics.event(
            "example.showcase_stopping",
            attributes={
                "started_scenes": self.scene_number,
                "completed_scenes": self.completed_scenes,
            },
        )
