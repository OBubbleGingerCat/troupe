from __future__ import annotations

import argparse
import asyncio
import importlib.resources
import json
import sys
from pathlib import Path

import troupe
import troupe_smoke_dependency
from wheel_smoke_production import config as absolute_config

from . import config as relative_config
from .workers import relative_value


def _append_event(path: Path, event: list[object]) -> None:
    events = json.loads(path.read_text(encoding="utf-8")) if path.exists() else []
    events.append(event)
    path.write_text(json.dumps(events), encoding="utf-8")


class Production(troupe.Production):
    def __init__(self, args: list[str]) -> None:
        parser = argparse.ArgumentParser()
        parser.add_argument("--events", type=Path, required=True)
        parser.add_argument("--value", type=int, required=True)
        parser.add_argument("input")
        self.options = parser.parse_args(args)
        self.raw_args = args
        _append_event(self.options.events, ["args", args])

    async def start(self) -> None:
        _append_event(self.options.events, ["start"])

    async def scene(self) -> None:
        dependency_path = Path(troupe_smoke_dependency.__file__).resolve()
        assert dependency_path.is_relative_to(Path(sys.prefix).resolve())
        assert troupe_smoke_dependency.VALUE == "dependency-ok"
        assert absolute_config is relative_config
        assert absolute_config.MODULE_VALUE == "module-ok"
        assert relative_value() == "module-ok"
        assert self.options.value == 7
        assert self.options.input == "input.txt"
        resource = importlib.resources.files(__package__).joinpath("resources/marker.txt")
        assert resource.read_text(encoding="utf-8").strip() == "resource-ok"
        _append_event(
            self.options.events,
            ["scene", "dependency-ok", "module-ok", "resource-ok"],
        )
        raise asyncio.CancelledError

    async def stop(self) -> None:
        _append_event(self.options.events, ["stop"])
