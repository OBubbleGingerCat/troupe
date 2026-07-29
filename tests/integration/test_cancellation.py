from __future__ import annotations

import asyncio
import contextlib
import importlib
import sys
import threading
import traceback
from pathlib import Path
from types import ModuleType
from typing import Any, Callable, Coroutine

import pytest

import troupe


ROOT = Path(__file__).resolve().parents[2]
RECORDING_PACKAGE = ROOT / "tests" / "fixtures" / "productions" / "recording_production"
TIMEOUT = 5.0

CancelRecord = tuple[asyncio.Task[Any], int, object | None]
CancelFailure = Callable[[asyncio.Task[Any]], BaseException | None]
TaskFactory = Callable[
    [asyncio.AbstractEventLoop, Coroutine[Any, Any, Any]],
    asyncio.Task[Any],
]


class CancelDispatchBoom(Exception):
    pass


def _native() -> ModuleType:
    return importlib.import_module("troupe._runtime")


def _clear_recording_package() -> None:
    root = "recording_production"
    prefix = f"{root}."
    names = [name for name in sys.modules if name == root or name.startswith(prefix)]
    for name in sorted(names, key=lambda value: value.count("."), reverse=True):
        sys.modules.pop(name, None)


def _load_recording(mode: str) -> Any:
    return _native()._load_production(str(RECORDING_PACKAGE), [mode])


def _recording_task_factory(
    records: list[CancelRecord],
    cancel_failure: CancelFailure | None = None,
) -> TaskFactory:
    class RecordingTask(asyncio.Task):
        def cancel(self, msg: object | None = None) -> bool:
            records.append((self, threading.get_ident(), msg))
            if cancel_failure is not None:
                error = cancel_failure(self)
                if error is not None:
                    raise error
            return super().cancel(msg)

    def factory(
        loop: asyncio.AbstractEventLoop,
        coroutine: Coroutine[Any, Any, Any],
        **kwargs: Any,
    ) -> asyncio.Task[Any]:
        task_kwargs = {
            name: value
            for name, value in kwargs.items()
            if name in {"name", "context"} and value is not None
        }
        return RecordingTask(coroutine, loop=loop, **task_kwargs)

    return factory


async def _wait(event: asyncio.Event) -> None:
    await asyncio.wait_for(event.wait(), TIMEOUT)


async def _await_future(future: Any) -> Any:
    return await asyncio.wait_for(asyncio.shield(future), TIMEOUT)


async def _drain(runtime: Any, future: Any | None) -> None:
    runtime.request_shutdown()
    if future is None or future.done():
        return
    with contextlib.suppress(BaseException):
        await _await_future(future)


def _release_recording(production: Any) -> None:
    production.start_release.set()
    production.scene_blocker.set()
    production.cleanup_release.set()
    production.scene_three_release.set()
    production.stop_release.set()


def _scene_cancel_records(
    records: list[CancelRecord],
    scene_task: asyncio.Task[Any],
) -> list[CancelRecord]:
    return [record for record in records if record[0] is scene_task]


def test_shutdown_cancels_retained_scene_task_on_loop_before_stop() -> None:
    async def scenario() -> None:
        loop = asyncio.get_running_loop()
        loop_thread = threading.get_ident()
        records: list[CancelRecord] = []
        previous_factory = loop.get_task_factory()
        loop.set_task_factory(_recording_task_factory(records))
        runtime = _native()._Runtime()
        production = _load_recording("cancel-finally")
        future: Any | None = None
        try:
            future = runtime.run(production)
            await _wait(production.start_entered)
            production.start_release.set()
            await _wait(production.scene_entered)
            scene_task = production.scene_task
            assert scene_task is not None

            assert runtime.request_shutdown() is None
            await _wait(production.scene_finally)
            await _wait(production.stop_entered)

            assert not production.scene_blocker.is_set()
            assert production.lifecycle_events == [
                "start",
                "scene-enter",
                "scene-finally",
                "stop",
            ]
            assert production.stop_entry_active_hooks == [0]
            assert production.active_hooks == 1
            assert production.max_active_hooks == 1

            production.stop_release.set()
            assert await _await_future(future) is None
            assert production.active_hooks == 0
            assert production.cancel_count == 1
            assert _scene_cancel_records(records, scene_task) == [
                (scene_task, loop_thread, None)
            ]
        finally:
            _release_recording(production)
            await _drain(runtime, future)
            loop.set_task_factory(previous_factory)

    _clear_recording_package()
    try:
        asyncio.run(scenario())
    finally:
        _clear_recording_package()


def test_caught_cancellation_waits_for_cleanup_and_repeated_shutdown_is_idle() -> None:
    async def scenario() -> None:
        loop = asyncio.get_running_loop()
        loop_thread = threading.get_ident()
        records: list[CancelRecord] = []
        previous_factory = loop.get_task_factory()
        loop.set_task_factory(_recording_task_factory(records))
        runtime = _native()._Runtime()
        production = _load_recording("swallow-cancel")
        future: Any | None = None
        try:
            future = runtime.run(production)
            await _wait(production.start_entered)
            production.start_release.set()
            await _wait(production.scene_entered)
            scene_task = production.scene_task
            assert scene_task is not None

            assert runtime.request_shutdown() is None
            await _wait(production.cancel_caught)
            assert production.cancel_count == 1
            assert not future.done()
            assert not production.stop_entered.is_set()

            assert runtime.request_shutdown() is None
            loop_barrier = asyncio.Event()
            loop.call_soon(loop_barrier.set)
            await _wait(loop_barrier)
            assert not future.done()
            assert not production.stop_entered.is_set()

            production.cleanup_release.set()
            await _wait(production.stop_entered)
            assert production.lifecycle_events == [
                "start",
                "scene-enter",
                "cancel-caught",
                "scene-cleanup",
                "stop",
            ]
            assert production.stop_entry_active_hooks == [0]
            assert production.active_hooks == 1
            assert production.max_active_hooks == 1

            production.stop_release.set()
            assert await _await_future(future) is None
            assert production.cancel_count == 1
            assert production.active_hooks == 0
            assert _scene_cancel_records(records, scene_task) == [
                (scene_task, loop_thread, None)
            ]
        finally:
            _release_recording(production)
            await _drain(runtime, future)
            loop.set_task_factory(previous_factory)

    _clear_recording_package()
    try:
        asyncio.run(scenario())
    finally:
        _clear_recording_package()


def test_cancellation_cleanup_failure_is_scene_phase_then_stop() -> None:
    async def scenario() -> None:
        runtime = _native()._Runtime()
        production = _load_recording("cleanup-fails")
        future: Any | None = None
        try:
            future = runtime.run(production)
            await _wait(production.start_entered)
            production.start_release.set()
            await _wait(production.scene_entered)

            assert runtime.request_shutdown() is None
            await _wait(production.cancel_caught)
            await _wait(production.stop_entered)
            assert not production.scene_blocker.is_set()
            assert production.lifecycle_events == [
                "start",
                "scene-enter",
                "scene-cleanup",
                "stop",
            ]
            assert production.stop_entry_active_hooks == [0]

            production.stop_release.set()
            with pytest.raises(_native().ProductionFailed) as captured:
                await _await_future(future)

            failures = captured.value.failures
            assert tuple(failure.phase for failure in failures) == ("scene",)
            assert failures[0].error is production.cleanup_error
            frame = traceback.extract_tb(production.cleanup_error.__traceback__)[-1]
            assert frame.name == "scene"
            assert frame.filename.endswith("recording_production/production.py")
            assert production.cancel_count == 1
            assert production.active_hooks == 0
            assert production.max_active_hooks == 1
        finally:
            _release_recording(production)
            await _drain(runtime, future)

    _clear_recording_package()
    try:
        asyncio.run(scenario())
    finally:
        _clear_recording_package()


def test_runtime_cancels_only_the_top_level_scene_task() -> None:
    async def scenario() -> None:
        loop = asyncio.get_running_loop()
        loop_thread = threading.get_ident()
        records: list[CancelRecord] = []
        previous_factory = loop.get_task_factory()
        loop.set_task_factory(_recording_task_factory(records))
        runtime = _native()._Runtime()

        class ChildOwner(troupe.Production):
            def __init__(self, args: list[str]) -> None:
                self.events: list[str] = []
                self.scene_entered = asyncio.Event()
                self.scene_blocker = asyncio.Event()
                self.scene_finally = asyncio.Event()
                self.child_entered = asyncio.Event()
                self.child_blocker = asyncio.Event()
                self.stop_entered = asyncio.Event()
                self.scene_task: asyncio.Task[Any] | None = None
                self.child_task: asyncio.Task[Any] | None = None
                self.scene_active = False
                self.stop_count = 0

            async def start(self) -> None:
                self.events.append("start")

            async def _child(self) -> None:
                self.events.append("child-enter")
                self.child_entered.set()
                try:
                    await self.child_blocker.wait()
                finally:
                    self.events.append("child-finally")

            async def scene(self) -> None:
                self.scene_active = True
                self.events.append("scene-enter")
                self.scene_task = asyncio.current_task()
                self.scene_entered.set()
                child = asyncio.create_task(self._child())
                self.child_task = child
                try:
                    await self.scene_blocker.wait()
                finally:
                    try:
                        assert [record for record in records if record[0] is child] == []
                    finally:
                        try:
                            child.cancel()
                            with contextlib.suppress(asyncio.CancelledError):
                                await child
                        finally:
                            self.events.append("scene-finally")
                            self.scene_active = False
                            self.scene_finally.set()

            async def stop(self) -> None:
                assert not self.scene_active
                self.stop_count += 1
                self.events.append("stop")
                self.stop_entered.set()

        production = ChildOwner([])
        future: Any | None = None
        try:
            future = runtime.run(production)
            await _wait(production.scene_entered)
            await _wait(production.child_entered)
            scene_task = production.scene_task
            child_task = production.child_task
            assert scene_task is not None
            assert child_task is not None

            assert runtime.request_shutdown() is None
            await _wait(production.scene_finally)
            await _wait(production.stop_entered)
            assert await _await_future(future) is None

            assert production.events == [
                "start",
                "scene-enter",
                "child-enter",
                "child-finally",
                "scene-finally",
                "stop",
            ]
            assert production.stop_count == 1
            assert _scene_cancel_records(records, scene_task) == [
                (scene_task, loop_thread, None)
            ]
            assert [record for record in records if record[0] is child_task] == [
                (child_task, loop_thread, None)
            ]
        finally:
            runtime.request_shutdown()
            production.scene_blocker.set()
            production.child_blocker.set()
            await _drain(runtime, future)
            loop.set_task_factory(previous_factory)

    asyncio.run(scenario())


@pytest.mark.parametrize(
    "cancel_error",
    [
        pytest.param(CancelDispatchBoom("cancel dispatch failed"), id="ordinary"),
        pytest.param(
            asyncio.CancelledError("cancel dispatch cancelled"),
            id="cancelled-error",
        ),
    ],
)
def test_cancel_dispatch_failure_waits_for_scene_before_stop(
    cancel_error: BaseException,
) -> None:
    async def scenario() -> None:
        loop = asyncio.get_running_loop()
        loop_thread = threading.get_ident()
        records: list[CancelRecord] = []
        cancel_attempted = asyncio.Event()
        runtime = _native()._Runtime()

        class ActiveScene(troupe.Production):
            def __init__(self, args: list[str]) -> None:
                self.events: list[str] = []
                self.scene_entered = asyncio.Event()
                self.scene_blocker = asyncio.Event()
                self.scene_finally = asyncio.Event()
                self.stop_entered = asyncio.Event()
                self.stop_release = asyncio.Event()
                self.scene_task: asyncio.Task[Any] | None = None
                self.scene_active = False
                self.stop_saw_scene_active: bool | None = None

            async def start(self) -> None:
                self.events.append("start")

            async def scene(self) -> None:
                self.scene_active = True
                self.scene_task = asyncio.current_task()
                self.events.append("scene-enter")
                self.scene_entered.set()
                try:
                    await self.scene_blocker.wait()
                finally:
                    self.events.append("scene-finally")
                    self.scene_active = False
                    self.scene_finally.set()

            async def stop(self) -> None:
                self.stop_saw_scene_active = self.scene_active
                self.events.append("stop")
                self.stop_entered.set()
                await self.stop_release.wait()

        production = ActiveScene([])

        def fail_scene_cancel(task: asyncio.Task[Any]) -> BaseException | None:
            if task is production.scene_task:
                cancel_attempted.set()
                return cancel_error
            return None

        previous_factory = loop.get_task_factory()
        loop.set_task_factory(_recording_task_factory(records, fail_scene_cancel))
        future: Any | None = None
        try:
            assert cancel_error.__traceback__ is None
            future = runtime.run(production)
            await _wait(production.scene_entered)
            scene_task = production.scene_task
            assert scene_task is not None

            assert runtime.request_shutdown() is None
            await _wait(cancel_attempted)
            loop_barrier = asyncio.Event()
            loop.call_soon(loop_barrier.set)
            await _wait(loop_barrier)
            assert not future.done()
            assert not production.stop_entered.is_set()
            assert production.scene_active

            production.scene_blocker.set()
            await _wait(production.scene_finally)
            await _wait(production.stop_entered)
            assert production.events == [
                "start",
                "scene-enter",
                "scene-finally",
                "stop",
            ]
            assert production.stop_saw_scene_active is False

            production.stop_release.set()
            with pytest.raises(_native().ProductionFailed) as captured:
                await _await_future(future)

            failures = captured.value.failures
            assert tuple(failure.phase for failure in failures) == ("scene",)
            assert failures[0].error is cancel_error
            frame = traceback.extract_tb(cancel_error.__traceback__)[-1]
            assert frame.name == "cancel"
            assert frame.filename.endswith("test_cancellation.py")
            assert _scene_cancel_records(records, scene_task) == [
                (scene_task, loop_thread, None)
            ]
        finally:
            runtime.request_shutdown()
            production.scene_blocker.set()
            production.stop_release.set()
            await _drain(runtime, future)
            scene_task = production.scene_task
            if scene_task is not None and not scene_task.done():
                with contextlib.suppress(BaseException):
                    await _await_future(scene_task)
            loop.set_task_factory(previous_factory)

    asyncio.run(scenario())
