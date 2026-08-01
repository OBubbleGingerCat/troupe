from __future__ import annotations

import asyncio
import contextlib
import contextvars
import importlib
import sys
import threading
from pathlib import Path
from types import ModuleType
from typing import Any

import pytest

import troupe


ROOT = Path(__file__).resolve().parents[2]
RECORDING_PACKAGE = ROOT / "tests" / "fixtures" / "productions" / "recording_production"
TIMEOUT = 5.0


def _native() -> ModuleType:
    return importlib.import_module("troupe._runtime")


def _clear_recording_package() -> None:
    root = "recording_production"
    prefix = f"{root}."
    names = [
        name
        for name in sys.modules
        if name == root or str.startswith(name, prefix)
    ]
    for name in sorted(names, key=lambda value: str.count(value, "."), reverse=True):
        sys.modules.pop(name, None)


def _load_recording() -> Any:
    return _native()._load_production(str(RECORDING_PACKAGE), [])


async def _wait(event: asyncio.Event) -> None:
    await asyncio.wait_for(event.wait(), TIMEOUT)


async def _drive(runtime: Any, production: object) -> None:
    assert await runtime.run(production) is None


async def _await_future(future: Any) -> Any:
    return await asyncio.wait_for(asyncio.shield(future), TIMEOUT)


async def _drain(future: Any | None) -> None:
    if future is None or future.done():
        return
    with contextlib.suppress(BaseException):
        await _await_future(future)


def test_runtime_is_a_private_native_surface() -> None:
    native = _native()

    assert native._Runtime.__module__ == "troupe._runtime"
    assert "_Runtime" not in vars(troupe)
    assert troupe.__all__ == [
        "Actor",
        "ActorHandle",
        "Cue",
        "CueContextError",
        "Effect",
        "EffectContextError",
        "Production",
    ]


def test_serial_lifecycle_waits_at_every_phase_boundary() -> None:
    async def scenario() -> None:
        runtime = _native()._Runtime()
        production = _load_recording()
        run_task: asyncio.Task[None] | None = None
        try:
            run_task = asyncio.create_task(_drive(runtime, production))

            await _wait(production.start_entered)
            assert production.lifecycle_events == ["start"]
            assert production.active_hooks == 1
            assert not run_task.done()
            production.start_release.set()

            await _wait(production.scene_three_entered)
            assert production.lifecycle_events == [
                "start",
                "scene:1",
                "scene:2",
                "scene:3-enter",
            ]
            assert production.active_hooks == 1
            assert runtime.request_shutdown() is None
            assert not run_task.done()
            assert "stop" not in production.lifecycle_events
            production.scene_three_release.set()

            await _wait(production.stop_entered)
            assert production.lifecycle_events == [
                "start",
                "scene:1",
                "scene:2",
                "scene:3-enter",
                "scene:3-exit",
                "stop",
            ]
            assert production.active_hooks == 1
            assert not run_task.done()
            production.stop_release.set()

            await _await_future(run_task)
            assert production.lifecycle_events == [
                "start",
                "scene:1",
                "scene:2",
                "scene:3-enter",
                "scene:3-exit",
                "stop",
            ]
            assert production.active_hooks == 0
            assert production.max_active_hooks == 1
            assert production.scene_count == 3
        finally:
            production.start_release.set()
            production.scene_three_release.set()
            production.stop_release.set()
            await _drain(run_task)

    _clear_recording_package()
    try:
        asyncio.run(scenario())
    finally:
        _clear_recording_package()


def test_shutdown_requested_before_run_is_durable() -> None:
    async def scenario() -> None:
        runtime = _native()._Runtime()
        production = _load_recording()
        run_task: asyncio.Task[None] | None = None
        try:
            assert runtime.request_shutdown() is None
            run_task = asyncio.create_task(_drive(runtime, production))

            await _wait(production.start_entered)
            assert production.lifecycle_events == ["start"]
            assert not run_task.done()
            production.start_release.set()

            await _wait(production.stop_entered)
            assert production.lifecycle_events == ["start", "stop"]
            assert production.scene_count == 0
            assert not run_task.done()
            production.stop_release.set()

            await _await_future(run_task)
            assert production.lifecycle_events == ["start", "stop"]
            assert production.max_active_hooks == 1
        finally:
            production.start_release.set()
            production.scene_three_release.set()
            production.stop_release.set()
            await _drain(run_task)

    _clear_recording_package()
    try:
        asyncio.run(scenario())
    finally:
        _clear_recording_package()


def test_shutdown_is_idempotent_from_a_real_python_thread() -> None:
    class ThreadProduction(troupe.Production):
        def __init__(self, args: list[str]) -> None:
            self.events: list[str] = []
            self.scene_entered = asyncio.Event()
            self.scene_release = asyncio.Event()

        async def start(self) -> None:
            self.events.append("start")

        async def scene(self) -> None:
            self.events.append("scene:1-enter")
            self.scene_entered.set()
            try:
                await self.scene_release.wait()
            except asyncio.CancelledError:
                await self.scene_release.wait()
            self.events.append("scene:1-exit")

        async def stop(self) -> None:
            self.events.append("stop")

    async def scenario() -> None:
        runtime = _native()._Runtime()
        production = ThreadProduction([])
        run_task: asyncio.Task[None] | None = None
        worker: threading.Thread | None = None
        results: list[object] = []
        errors: list[BaseException] = []
        try:
            run_task = asyncio.create_task(_drive(runtime, production))
            await _wait(production.scene_entered)
            loop = asyncio.get_running_loop()
            requests_done = asyncio.Event()

            def request_twice() -> None:
                try:
                    results.append(runtime.request_shutdown())
                    results.append(runtime.request_shutdown())
                except BaseException as error:
                    errors.append(error)
                finally:
                    loop.call_soon_threadsafe(requests_done.set)

            worker = threading.Thread(target=request_twice)
            worker.start()
            await _wait(requests_done)

            assert errors == []
            assert results == [None, None]
            assert production.events == ["start", "scene:1-enter"]
            assert not run_task.done()
            production.scene_release.set()

            await _await_future(run_task)
            worker.join(TIMEOUT)
            assert not worker.is_alive()
            assert production.events == [
                "start",
                "scene:1-enter",
                "scene:1-exit",
                "stop",
            ]
        finally:
            production.scene_release.set()
            await _drain(run_task)
            if worker is not None:
                worker.join(TIMEOUT)

    asyncio.run(scenario())


def test_runtime_run_is_synchronously_one_shot() -> None:
    class StartStopProduction(troupe.Production):
        def __init__(self, args: list[str]) -> None:
            self.events: list[str] = []

        async def start(self) -> None:
            self.events.append("start")

        async def scene(self) -> None:
            self.events.append("unexpected-scene")

        async def stop(self) -> None:
            self.events.append("stop")

    async def scenario() -> None:
        runtime = _native()._Runtime()
        production = StartStopProduction([])
        assert runtime.request_shutdown() is None
        future: Any | None = None
        try:
            future = runtime.run(production)
            assert production.events == []
            with pytest.raises(
                RuntimeError,
                match=r"^Runtime\.run\(\) may only be called once$",
            ):
                runtime.run(production)
            assert production.events == []

            assert await _await_future(future) is None
            with pytest.raises(
                RuntimeError,
                match=r"^Runtime\.run\(\) may only be called once$",
            ):
                runtime.run(production)
            assert production.events == ["start", "stop"]
        finally:
            await _drain(future)

    asyncio.run(scenario())


def test_failed_loop_capture_still_consumes_the_run() -> None:
    runtime = _native()._Runtime()
    production = troupe.Production([])

    with pytest.raises(RuntimeError, match=r"no running event loop"):
        runtime.run(production)
    with pytest.raises(
        RuntimeError,
        match=r"^Runtime\.run\(\) may only be called once$",
    ):
        runtime.run(production)


def test_runtime_future_owns_runtime_and_production() -> None:
    events: list[str] = []

    class OwnedProduction(troupe.Production):
        async def start(self) -> None:
            events.append("start")

        async def scene(self) -> None:
            events.append("unexpected-scene")

        async def stop(self) -> None:
            events.append("stop")

    async def scenario() -> None:
        runtime = _native()._Runtime()
        production = OwnedProduction([])
        runtime.request_shutdown()
        future: Any | None = None
        try:
            future = runtime.run(production)

            del runtime
            del production

            assert await _await_future(future) is None
            assert events == ["start", "stop"]
        finally:
            await _drain(future)

    asyncio.run(scenario())


def test_python_overrides_are_dynamically_dispatched() -> None:
    events: list[str] = []

    async def scenario() -> None:
        runtime = _native()._Runtime()
        future: Any | None = None

        class OverrideProduction(troupe.Production):
            async def start(self) -> None:
                events.append("override-start")

            async def scene(self) -> None:
                events.append("override-scene")
                runtime.request_shutdown()

            async def stop(self) -> None:
                events.append("override-stop")

        production = OverrideProduction([])
        try:
            future = runtime.run(production)
            assert await _await_future(future) is None
        finally:
            await _drain(future)

    asyncio.run(scenario())
    assert events == ["override-start", "override-scene", "override-stop"]


def test_hooks_use_captured_loop_thread_context_and_explicit_scene_tasks() -> None:
    marker: contextvars.ContextVar[str] = contextvars.ContextVar("troupe_marker")

    async def scenario() -> None:
        loop = asyncio.get_running_loop()
        main_thread = threading.get_ident()
        runtime = _native()._Runtime()
        lookup_records: list[tuple[str, int, int, str]] = []
        body_records: list[tuple[str, int, int, str, asyncio.Task[Any] | None]] = []
        factory_records: list[tuple[asyncio.Task[Any], int, int, str]] = []
        explicit_scene_tasks: list[asyncio.Task[Any]] = []
        scene_body_tasks: list[asyncio.Task[Any] | None] = []

        def location() -> tuple[int, int, str]:
            return (
                id(asyncio.get_running_loop()),
                threading.get_ident(),
                marker.get(),
            )

        class ContextProduction(troupe.Production):
            def __init__(self, args: list[str]) -> None:
                self.scene_number = 0

            def __getattribute__(self, name: str) -> Any:
                if name in {"start", "scene", "stop"}:
                    lookup_records.append((name, *location()))
                return object.__getattribute__(self, name)

            async def start(self) -> None:
                body_records.append(("start", *location(), asyncio.current_task()))

            async def scene(self) -> None:
                self.scene_number += 1
                task = asyncio.current_task()
                scene_body_tasks.append(task)
                body_records.append(("scene", *location(), task))
                if self.scene_number == 3:
                    runtime.request_shutdown()

            async def stop(self) -> None:
                body_records.append(("stop", *location(), asyncio.current_task()))

        production = ContextProduction([])
        previous_factory = loop.get_task_factory()
        original_create_task = asyncio.create_task
        future: Any | None = None

        def task_factory(
            factory_loop: asyncio.AbstractEventLoop,
            coroutine: Any,
            **kwargs: Any,
        ) -> asyncio.Task[Any]:
            task_kwargs = {
                name: value
                for name, value in kwargs.items()
                if name in {"name", "context"} and value is not None
            }
            task = asyncio.Task(coroutine, loop=factory_loop, **task_kwargs)
            factory_records.append((task, *location()))
            return task

        def recording_create_task(coroutine: Any, *args: Any, **kwargs: Any) -> Any:
            task = original_create_task(coroutine, *args, **kwargs)
            explicit_scene_tasks.append(task)
            return task

        loop.set_task_factory(task_factory)
        asyncio.create_task = recording_create_task
        token = marker.set("captured-A")
        try:
            future = runtime.run(production)
            assert lookup_records == []
            marker.set("caller-B")

            assert await _await_future(future) is None
        finally:
            asyncio.create_task = original_create_task
            loop.set_task_factory(previous_factory)
            await _drain(future)
            marker.reset(token)

        assert [name for name, *_ in lookup_records] == [
            "start",
            "scene",
            "scene",
            "scene",
            "stop",
        ]
        assert [name for name, *_ in body_records] == [
            "start",
            "scene",
            "scene",
            "scene",
            "stop",
        ]
        assert len(explicit_scene_tasks) == 3
        assert all(
            explicit is current
            for explicit, current in zip(explicit_scene_tasks, scene_body_tasks)
        )

        factory_by_task = {
            id(task): (loop_id, thread_id, context)
            for task, loop_id, thread_id, context in factory_records
        }
        relevant_factory_records = [
            factory_by_task[id(task)]
            for *_, task in body_records
            if task is not None
        ]
        assert len(relevant_factory_records) == 5
        for record in [
            *(record[1:] for record in lookup_records),
            *(record[1:4] for record in body_records),
            *relevant_factory_records,
        ]:
            assert record == (id(loop), main_thread, "captured-A")

    asyncio.run(scenario())
