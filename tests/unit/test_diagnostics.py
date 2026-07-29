from __future__ import annotations

import asyncio
import importlib
from types import ModuleType
from typing import Any

import pytest

import troupe


def _native() -> ModuleType:
    return importlib.import_module("troupe._runtime")


async def _capture_failure(production: troupe.Production) -> BaseException:
    runtime = _native()._Runtime()
    with pytest.raises(_native().ProductionFailed) as captured:
        await runtime.run(production)
    return captured.value


def _format(production: troupe.Production) -> str:
    failure = asyncio.run(_capture_failure(production))
    rendered = _native()._format_failure_for_test(failure)
    assert type(rendered) is str
    return rendered


class StartBoom(Exception):
    pass


class SceneBoom(Exception):
    pass


class StopBoom(Exception):
    pass


def test_start_failure_has_one_original_traceback_section() -> None:
    class StartFailure(troupe.Production):
        async def start(self) -> None:
            raise StartBoom("start marker")

        async def scene(self) -> None:
            raise AssertionError("scene must not run")

        async def stop(self) -> None:
            raise AssertionError("stop must not run")

    rendered = _format(StartFailure([]))

    assert rendered.count("troupe: production failed during start phase\n") == 1
    assert rendered.count("Traceback (most recent call last):") == 1
    assert "StartBoom: start marker" in rendered
    assert "in start" in rendered
    assert "ProductionFailed" not in rendered
    assert "scene phase" not in rendered
    assert "stop phase" not in rendered


def test_scene_failure_has_one_original_traceback_section() -> None:
    class SceneFailure(troupe.Production):
        async def scene(self) -> None:
            raise SceneBoom("scene marker")

    rendered = _format(SceneFailure([]))

    assert rendered.count("troupe: production failed during scene phase\n") == 1
    assert rendered.count("Traceback (most recent call last):") == 1
    assert "SceneBoom: scene marker" in rendered
    assert "in scene" in rendered
    assert "ProductionFailed" not in rendered
    assert "start phase" not in rendered
    assert "stop phase" not in rendered


def test_stop_failure_formats_a_retained_base_exception() -> None:
    class StopFailure(troupe.Production):
        async def scene(self) -> None:
            raise asyncio.CancelledError("scene completed")

        async def stop(self) -> None:
            raise asyncio.CancelledError("stop marker")

    rendered = _format(StopFailure([]))

    assert rendered.count("troupe: production failed during stop phase\n") == 1
    assert rendered.count("Traceback (most recent call last):") == 1
    assert "asyncio.exceptions.CancelledError: stop marker" in rendered
    assert "in stop" in rendered
    assert "ProductionFailed" not in rendered
    assert "start phase" not in rendered
    assert "scene phase" not in rendered


def test_scene_and_stop_failures_are_rendered_in_lifecycle_order() -> None:
    class DualFailure(troupe.Production):
        async def scene(self) -> None:
            raise SceneBoom("scene marker")

        async def stop(self) -> None:
            raise StopBoom("stop marker")

    rendered = _format(DualFailure([]))
    scene_header = "troupe: production failed during scene phase\n"
    stop_header = "troupe: production failed during stop phase\n"

    assert rendered.count(scene_header) == 1
    assert rendered.count(stop_header) == 1
    assert rendered.count("Traceback (most recent call last):") == 2
    assert rendered.index(scene_header) < rendered.index(stop_header)
    scene_section, stop_section = rendered.split(stop_header)
    assert "SceneBoom: scene marker" in scene_section
    assert scene_section.count("Traceback (most recent call last):") == 1
    assert "in scene" in scene_section
    assert "StopBoom" not in scene_section
    assert "StopBoom: stop marker" in stop_section
    assert stop_section.count("Traceback (most recent call last):") == 1
    assert "in stop" in stop_section
    assert "SceneBoom" not in stop_section
    assert "ProductionFailed" not in rendered
