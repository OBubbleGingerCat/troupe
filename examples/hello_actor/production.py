from __future__ import annotations

import asyncio
from typing import cast

import troupe


class Greeting(troupe.Effect):
    def __init__(self, text: str) -> None:
        self.text = text


class Greeter(troupe.Actor):
    async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
        greeting = self.make_effect(
            Greeting,
            effect_args=(f"Hello, {cue.instruction['name']}!",),
            effect_kwargs={},
        )
        return (greeting,)


class Production(troupe.Production):
    def __init__(self, args: list[str]) -> None:
        self.person = " ".join(args) or "world"
        self.greeter = self.cast_actor(
            Greeter,
            name="greeter",
            actor_args=(),
            actor_kwargs={},
        )

    async def scene(self) -> None:
        (effect,) = await self.greeter.cue({"name": self.person})
        greeting = cast(Greeting, effect)
        print(greeting.text, flush=True)
        await asyncio.Event().wait()
