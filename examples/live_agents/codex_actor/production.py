from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
from typing import Any, cast

import troupe


PROFILE_ENV = "TROUPE_LIVE_CODEX_PROFILE"


class DisjointIntValue(troupe.act_schema.SchemaValue[int]):
    def __init__(self) -> None:
        super().__init__(
            description="an integer in either 2 through 4 or 8 through 10",
            json_kind="int64",
        )

    def render_prompt(self) -> str:
        return (
            "must be an integer in [2, 4] or [8, 10]; first submit 6 to "
            "observe validation rejection, then correct it to 8"
        )

    def validate(self, value: int, /) -> None:
        if not (2 <= value <= 4 or 8 <= value <= 10):
            raise troupe.act_schema.ValueRejected(
                "must be in either [2, 4] or [8, 10]"
            )


class Outcome(troupe.Effect):
    def __init__(self, payload: dict[str, Any]) -> None:
        self.payload = payload


class LiveCodexActor(troupe.Actor):
    def __init__(self, workspace: Path, seed_token: str) -> None:
        self.workspace = workspace
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
            result = await self.act(
                script=(
                    "Use Codex workspace tools to read seed.txt. Create artifact.txt "
                    "with exactly the line 'codex-workspace-ok'. Remember the token "
                    "for the next turn. Call the Troupe result tool first with status "
                    "'needs-human' so its choices validator rejects it, then correct "
                    "the same turn with status 'stored' and the token from seed.txt."
                ),
                output_schema={
                    "status": troupe.act_schema.StrValue(
                        description="the completed storage status",
                        choices=["stored"],
                    ),
                    "token": troupe.act_schema.StrValue(
                        description="the exact token read from seed.txt",
                        choices=[self.seed_token],
                    ),
                },
            )
            (self.workspace / "seed.txt").unlink()
            return self.outcome(result)

        if operation == "recall":
            result = await self.act(
                script=(
                    "Do not read files and do not ask a human. Recall the token from "
                    "the previous turn. For the custom confidence field, first call "
                    "the result tool with 6, observe the ValueRejected response, then "
                    "correct it to 8 in the same turn."
                ),
                output_schema={
                    "token": troupe.act_schema.StrValue(
                        description="the token remembered from the previous turn",
                        choices=[self.seed_token],
                    ),
                    "confidence": DisjointIntValue(),
                },
            )
            return self.outcome(result)

        if operation == "cancel":
            marker = self.workspace / "cancel-started.txt"
            call = asyncio.create_task(
                self.act(
                    script=(
                        "Use a shell command to create cancel-started.txt with the "
                        "single line 'started', then run 'sleep 120'. Do not call the "
                        "Troupe result tool unless the sleep command finishes."
                    ),
                    output_schema={
                        "finished": troupe.act_schema.BoolValue(
                            description="whether the long command finished",
                            choices=[True],
                        )
                    },
                )
            )
            while not marker.is_file():
                await asyncio.sleep(0.05)
            call.cancel()
            try:
                await call
            except asyncio.CancelledError:
                return self.outcome({"cancelled": True})
            raise AssertionError("the live cancellation turn completed before cancellation")

        if operation == "recover":
            try:
                result = await self.act(
                    script=(
                        "After the cancelled turn, return 'recovered' using the Troupe "
                        "result tool without asking a human."
                    ),
                    output_schema={
                        "status": troupe.act_schema.StrValue(
                            description="the post-cancellation session status",
                            choices=["recovered"],
                        )
                    },
                )
            except troupe.AgentSessionBrokenError as error:
                return self.outcome({"kind": "broken", "code": error.code})
            return self.outcome({"kind": "result", "value": result})

        raise AssertionError(f"unknown live Codex operation: {operation}")


class ProbeActor(troupe.Actor):
    async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
        del cue
        try:
            result = await self.act(
                script="Return the probe result through the Troupe result tool.",
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
    effort = value.get("effort")
    if not isinstance(workspace, str) or not isinstance(model, str):
        raise TypeError("live Codex workspace and model must be strings")
    if effort is not None and not isinstance(effort, str):
        raise TypeError("live Codex effort must be a string or null")
    return troupe.AgentProfile(
        agent="codex",
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
        self.workspace = Path(self.profile.workspace)
        actor_type: type[troupe.Actor]
        actor_args: tuple[Any, ...]
        if self.mode == "acceptance":
            actor_type = LiveCodexActor
            actor_args = (self.workspace, self.seed_token)
        else:
            actor_type = ProbeActor
            actor_args = ()
        self.actor = self.cast_actor(
            actor_type,
            name=f"codex-live-{self.mode}",
            agent_profile=self.profile,
            actor_args=actor_args,
            actor_kwargs={},
        )

    async def scene(self) -> None:
        if self.mode == "acceptance":
            payload: dict[str, Any] = {}
            for operation in ("remember", "recall", "cancel", "recover"):
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
