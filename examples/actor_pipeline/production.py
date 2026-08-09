from __future__ import annotations

import asyncio
import json
from pathlib import Path
import re
from typing import cast

import troupe


class FormattedMessage(troupe.Effect):
    def __init__(self, text: str, source: str) -> None:
        self.text = text
        self.source = source


class Formatter(troupe.Actor):
    async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
        message = self.make_effect(
            FormattedMessage,
            effect_args=(str(cue.instruction["text"]).upper(), cue.source),
            effect_kwargs={},
        )
        return (message,)


class Router(troupe.Actor):
    def __init__(self, formatter: troupe.ActorHandle) -> None:
        self.formatter = formatter

    async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
        return await self.formatter.cue({"text": cue.instruction["text"]})


class Production(troupe.Production):
    def __init__(self, args: list[str]) -> None:
        self.text = " ".join(args) or "hello troupe"
        profile = troupe.AgentProfile(
            agent="codex",
            workspace=Path.cwd(),
            model="gpt-5.6-sol",
            effort="medium",
        )
        self.formatter = self.cast_actor(
            Formatter,
            name="formatter",
            agent_profile=profile,
            actor_args=(),
            actor_kwargs={},
        )
        self.router = self.cast_actor(
            Router,
            name="router",
            agent_profile=profile,
            actor_args=(self.formatter,),
            actor_kwargs={},
        )

    async def scene(self) -> None:
        (effect,) = await self.router.cue({"text": self.text})
        message = cast(FormattedMessage, effect)
        actors = self.get_actor(re.compile(r"(?:formatter|router)"))
        print(
            json.dumps(
                {
                    "actors": [actor.name for actor in actors],
                    "message": message.text,
                    "owner": message.owner,
                    "source": message.source,
                },
                sort_keys=True,
            ),
            flush=True,
        )
        await asyncio.Event().wait()
