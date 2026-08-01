from __future__ import annotations

import asyncio

import troupe


class Production(troupe.Production):
    def __init__(self, args: list[str]) -> None:
        self.scene_number = 0

    async def scene(self) -> None:
        self.scene_number += 1
        print(f"scene:{self.scene_number}", flush=True)
        await asyncio.sleep(0.5)
