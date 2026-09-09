"""A small, real Pi/DeepSeek Actor example.

The live-agent examples are intentionally explicit: they require a profile in
``TROUPE_LIVE_PI_PROFILE`` and are run by ``scripts/test_live_agent.sh pi``.
Pi is started by Troupe with only the structured-result extension enabled, so
this example does not rely on Pi's built-in file or shell tools.
"""

from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
from typing import Any, cast

import troupe
from troupe import diagnostics

PROFILE_ENV = "TROUPE_LIVE_PI_PROFILE"


class RecordingSink(diagnostics.DiagnosticSink):
    """Keep a compact, JSON-safe summary for the example report."""

    def __init__(self) -> None:
        super().__init__(
            capture=diagnostics.DiagnosticCapture(
                tool_inputs=True,
                tool_outputs=True,
            )
        )
        self.event_count = 0
        self.tool_event_count = 0

    def on_event(self, event: diagnostics.DiagnosticEvent, /) -> None:
        self.event_count += 1
        if isinstance(event, diagnostics.SpanStarted) and event.span_kind == "tool.call":
            self.tool_event_count += 1


class Outcome(troupe.Effect):
    def __init__(self, payload: dict[str, Any]) -> None:
        self.payload = payload


class LivePiActor(troupe.Actor):
    def __init__(self, seed_token: str) -> None:
        self.seed_token = seed_token

    def outcome(self, payload: dict[str, Any]) -> tuple[troupe.Effect, ...]:
        return (
            self.make_effect(
                Outcome,
                effect_args=(payload,),
                effect_kwargs={},
            ),
        )

    async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
        operation = str(cue.instruction["operation"])

        if operation == "remember":
            sink = RecordingSink()
            with diagnostics.span(
                "pi.example.remember",
                attributes={"backend": "pi", "model_family": "deepseek"},
            ):
                diagnostics.event(
                    "pi.example.context_seeded",
                    attributes={"operation": "remember"},
                )
                result = await self.act(
                    script=(
                        "This is the first turn of a context-retention example. The "
                        f"marker is {json.dumps(self.seed_token)}. Use only the "
                        "Troupe structured-result tool (do not call any other tool). "
                        "Submit status 'stored' and the marker exactly as given."
                    ),
                    output_schema={
                        "status": troupe.act_schema.StrValue(
                            description="the first-turn storage status",
                            choices=["stored"],
                        ),
                        "token": troupe.act_schema.StrValue(
                            description="the exact marker from the first turn",
                            choices=[self.seed_token],
                        ),
                    },
                    diagnostic_sink=sink,
                )
                diagnostics.event(
                    "pi.example.context_stored",
                    attributes={"operation": "remember"},
                )
            summary = await sink.wait_closed()
            return self.outcome(
                {
                    "result": result,
                    "diagnostics": {
                        "act_id": summary.act_id,
                        "complete": summary.complete,
                        "close_reason": summary.close_reason,
                        "delivered_events": summary.delivered_events,
                        "callback_failure": summary.callback_failure is not None,
                        "observed_events": sink.event_count,
                        "tool_span_events": sink.tool_event_count,
                    },
                }
            )

        if operation == "recall":
            with diagnostics.span(
                "pi.example.recall",
                attributes={"backend": "pi", "model_family": "deepseek"},
            ):
                diagnostics.event(
                    "pi.example.context_recall_requested",
                    attributes={"operation": "recall"},
                )
                result = await self.act(
                    script=(
                        "This is the second turn in the same persistent Actor session. "
                        "Do not call any tool except the Troupe structured-result "
                        "tool. Recall the marker and submit status 'recalled', the "
                        "exact marker, and confidence 8."
                    ),
                    output_schema={
                        "status": troupe.act_schema.StrValue(
                            description="the second-turn recall status",
                            choices=["recalled"],
                        ),
                        "token": troupe.act_schema.StrValue(
                            description="the marker recalled from the previous turn",
                            choices=[self.seed_token],
                        ),
                        "confidence": troupe.act_schema.Int64Value(
                            description="the confidence in the recalled marker",
                            min=0,
                            max=10,
                            choices=[8],
                        ),
                    },
                )
            return self.outcome({"result": result})

        raise AssertionError(f"unknown live Pi operation: {operation!r}")


class ProbeActor(troupe.Actor):
    async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
        del cue
        try:
            result = await self.act(
                script="Submit a probe result through the Troupe structured-result tool.",
                output_schema={
                    "status": troupe.act_schema.StrValue(
                        description="the probe status",
                        choices=["unexpected-success"],
                    )
                },
            )
        except troupe.AgentError as error:
            payload: dict[str, Any] = {
                "kind": "error",
                "type": type(error).__name__,
                "code": error.code,
            }
            if isinstance(error, troupe.AgentSessionStartError):
                payload["phase"] = error.phase
        else:
            payload = {"kind": "result", "value": result}
        return (
            self.make_effect(
                Outcome,
                effect_args=(payload,),
                effect_kwargs={},
            ),
        )


def _load_profile() -> troupe.AgentProfile:
    raw = os.environ.get(PROFILE_ENV)
    if raw is None:
        raise RuntimeError(f"{PROFILE_ENV} is required")
    value = json.loads(raw)
    if not isinstance(value, dict):
        raise TypeError(f"{PROFILE_ENV} must contain a JSON object")
    workspace = value.get("workspace")
    model = value.get("model")
    if "effort" not in value:
        raise TypeError(f"{PROFILE_ENV} must contain effort")
    effort = value["effort"]
    if not isinstance(workspace, str) or not isinstance(model, str):
        raise TypeError("live Pi workspace and model must be strings")
    if effort is not None and not isinstance(effort, str):
        raise TypeError("live Pi effort must be a string or null")
    return troupe.AgentProfile(
        agent="pi",
        workspace=workspace,
        model=model,
        effort=effort,
    )


class Production(troupe.Production):
    def __init__(self, args: list[str]) -> None:
        if len(args) != 3:
            raise ValueError("expected MODE REPORT_PATH SEED_TOKEN")
        self.mode = args[0]
        self.report_path = Path(args[1])
        self.seed_token = args[2]
        self.profile = _load_profile()
        actor_type: type[troupe.Actor]
        actor_args: tuple[Any, ...]
        if self.mode == "acceptance":
            actor_type = LivePiActor
            actor_args = (self.seed_token,)
        else:
            actor_type = ProbeActor
            actor_args = ()
        self.actor = self.cast_actor(
            actor_type,
            name=f"pi-live-{self.mode}",
            agent_profile=self.profile,
            actor_args=actor_args,
            actor_kwargs={},
        )

    async def scene(self) -> None:
        if self.mode == "acceptance":
            payload: dict[str, Any] = {}
            for operation in ("remember", "recall"):
                (effect,) = await self.actor.cue({"operation": operation})
                payload[operation] = cast(Outcome, effect).payload
        else:
            (effect,) = await self.actor.cue({"operation": "probe"})
            payload = cast(Outcome, effect).payload

        temporary = self.report_path.with_suffix(self.report_path.suffix + ".tmp")
        temporary.write_text(
            json.dumps(payload, sort_keys=True, separators=(",", ":")),
            encoding="utf-8",
        )
        os.replace(temporary, self.report_path)
        await asyncio.Event().wait()
