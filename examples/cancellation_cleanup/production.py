from __future__ import annotations

import asyncio
from pathlib import Path

import troupe


class Worker(troupe.Actor):
    async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
        print("worker:start", flush=True)
        try:
            await asyncio.Event().wait()
        finally:
            await asyncio.sleep(0)
            print("worker:cleanup", flush=True)
        return ()


class Production(troupe.Production):
    def __init__(self, args: list[str]) -> None:
        profile = troupe.AgentProfile(
            agent="codex",
            workspace=Path.cwd(),
            model="gpt-5.6-sol",
            effort="medium",
        )
        self.worker = self.cast_actor(
            Worker,
            name="worker",
            agent_profile=profile,
            actor_args=(),
            actor_kwargs={},
        )

    async def scene(self) -> None:
        try:
            await self.worker.cue({})
        finally:
            print("scene:cleanup", flush=True)

    async def stop(self) -> None:
        print("production:stop", flush=True)
