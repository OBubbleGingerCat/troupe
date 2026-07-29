from __future__ import annotations

import asyncio
import importlib
import inspect
import sys
from types import ModuleType

import pytest


def _modules() -> tuple[ModuleType, ModuleType]:
    troupe = importlib.import_module("troupe")
    runtime = importlib.import_module("troupe._runtime")
    return troupe, runtime


def test_public_symbol_is_the_native_class() -> None:
    troupe, runtime = _modules()

    assert troupe.Production is runtime.Production
    assert troupe.Production.__module__ == "troupe"
    assert troupe.__all__ == ["Production"]
    assert {name for name in vars(troupe) if not name.startswith("_")} == {
        "Production"
    }


def test_constructor_accepts_only_one_positional_list_of_strings() -> None:
    troupe, _ = _modules()
    production_type = troupe.Production

    assert isinstance(production_type([]), production_type)
    assert isinstance(production_type(["--value", "1"]), production_type)
    assert isinstance(production_type(["\udcff"]), production_type)

    with pytest.raises(TypeError):
        production_type()
    with pytest.raises(TypeError):
        production_type([], [])
    with pytest.raises(TypeError):
        production_type(args=[])

    for invalid in ((), "value", None, 1):
        with pytest.raises(TypeError):
            production_type(invalid)

    for invalid in ([1], [None], ["valid", object()]):
        with pytest.raises(TypeError):
            production_type(invalid)


def test_base_class_does_not_retain_args() -> None:
    troupe, _ = _modules()
    value = "".join(["not", "-interned"])
    args = [value]
    args_references = sys.getrefcount(args)
    value_references = sys.getrefcount(value)

    production = troupe.Production(args)

    assert not hasattr(production, "args")
    assert sys.getrefcount(args) == args_references
    assert sys.getrefcount(value) == value_references


def test_default_hooks_return_awaitables_with_exact_results() -> None:
    troupe, _ = _modules()
    production = troupe.Production([])

    async def exercise() -> None:
        start = production.start()
        scene = production.scene()
        stop = production.stop()

        assert inspect.isawaitable(start)
        assert inspect.isawaitable(scene)
        assert inspect.isawaitable(stop)
        assert await start is None
        with pytest.raises(
            NotImplementedError,
            match=r"^Production\.scene\(\) is not implemented$",
        ):
            await scene
        assert await stop is None

    asyncio.run(exercise())


def test_python_subclass_owns_init_and_overrides_scene() -> None:
    troupe, _ = _modules()
    events: list[str] = []
    init_calls: list[list[str]] = []

    class CustomProduction(troupe.Production):
        def __init__(self, args: list[str]) -> None:
            init_calls.append(args)
            self.received = args

        async def scene(self) -> None:
            events.append("scene")

    args = ["--value", "1"]
    production = CustomProduction(args)

    assert type(production) is CustomProduction
    assert init_calls == [args]
    assert init_calls[0] is args
    assert production.received is args

    with pytest.raises(TypeError):
        CustomProduction(())  # type: ignore[arg-type]
    assert init_calls == [args]

    async def exercise() -> None:
        assert await production.start() is None
        assert await production.scene() is None
        assert await production.stop() is None

    asyncio.run(exercise())
    assert events == ["scene"]
