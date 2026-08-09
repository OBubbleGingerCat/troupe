from __future__ import annotations

import asyncio
import json
from pathlib import Path

import troupe


class Worker(troupe.Actor):
    async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
        label = str(cue.instruction["label"])
        timeline = cue.instruction["timeline"]
        entered = cue.instruction["entered"]
        release = cue.instruction["release"]

        timeline.append(f"start:{self.name}:{label}")
        if entered is not None:
            entered.set()
        if release is not None:
            await release.wait()
        timeline.append(f"end:{self.name}:{label}")
        return ()


class Production(troupe.Production):
    def __init__(self, args: list[str]) -> None:
        profile = troupe.AgentProfile(
            agent="codex",
            workspace=Path.cwd(),
            model="gpt-5.6-sol",
            effort="medium",
        )
        self.left = self.cast_actor(
            Worker,
            name="left",
            agent_profile=profile,
            actor_args=(),
            actor_kwargs={},
        )
        self.right = self.cast_actor(
            Worker,
            name="right",
            agent_profile=profile,
            actor_args=(),
            actor_kwargs={},
        )

    async def scene(self) -> None:
        timeline: list[str] = []
        left_entered = asyncio.Event()
        right_entered = asyncio.Event()
        left_release = asyncio.Event()
        right_release = asyncio.Event()

        def start(
            worker: troupe.ActorHandle,
            label: int,
            entered: asyncio.Event | None = None,
            release: asyncio.Event | None = None,
        ) -> asyncio.Task[tuple[troupe.Effect, ...]]:
            return asyncio.create_task(
                worker.cue(
                    {
                        "label": label,
                        "timeline": timeline,
                        "entered": entered,
                        "release": release,
                    }
                )
            )

        left_first = start(self.left, 1, left_entered, left_release)
        await left_entered.wait()

        right_first = start(self.right, 1, right_entered, right_release)
        await right_entered.wait()

        submitted: list[str] = []
        submitted.append("left:2")
        left_second = start(self.left, 2)
        submitted.append("left:3")
        left_third = start(self.left, 3)

        admission_barrier = asyncio.Event()
        asyncio.get_running_loop().call_soon(admission_barrier.set)
        await admission_barrier.wait()
        assert timeline == ["start:left:1", "start:right:1"]

        right_release.set()
        await right_first
        left_release.set()
        await asyncio.gather(left_first, left_second, left_third)

        print(
            json.dumps(
                {
                    "submitted": submitted,
                    "timeline": timeline,
                },
                sort_keys=True,
            ),
            flush=True,
        )
        await asyncio.Event().wait()
