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


def test_cancelling_run_future_keeps_internal_cleanup_restore_and_unbind_alive() -> None:
    async def scenario() -> None:
        loop = asyncio.get_running_loop()
        previous_factory = loop.get_task_factory()
        loop_thread = threading.get_ident()
        cancel_records: list[CancelRecord] = []
        original_factory = _recording_task_factory(cancel_records)
        loop.set_task_factory(original_factory)
        actor_entered = asyncio.Event()
        cleanup_entered = asyncio.Event()
        cleanup_release = asyncio.Event()
        stop_entered = asyncio.Event()
        stop_release = asyncio.Event()
        stop_finished = asyncio.Event()
        root_finally = asyncio.Event()
        queued_caller_done = asyncio.Event()
        caller_errors: dict[str, BaseException] = {}
        root_cleanup_errors: list[BaseException] = []
        root_cues: list[troupe.Cue] = []
        queued_done_before_release: list[bool] = []
        running_cued_task: asyncio.Task[Any] | None = None
        factories: dict[str, Any] = {}

        class BlockingActor(troupe.Actor):
            async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
                nonlocal running_cued_task
                root_cues.append(cue)
                if cue.instruction["phase"] != "running":
                    return ()
                running_cued_task = asyncio.current_task()
                assert running_cued_task is not None
                actor_entered.set()
                try:
                    await asyncio.Event().wait()
                except asyncio.CancelledError:
                    factories["cleanup"] = loop.get_task_factory()
                    cleanup_entered.set()
                    await cleanup_release.wait()
                    raise

        class RootCleanupActor(troupe.Actor):
            async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
                root_cues.append(cue)
                return ()

        class CancelledOuterProduction(troupe.Production):
            async def scene(self) -> None:
                factories["runtime"] = loop.get_task_factory()
                assert factories["runtime"] is not original_factory
                handle = self.cast_actor(
                    BlockingActor,
                    name="outer-cancel",
                    actor_args=(),
                    actor_kwargs={},
                )
                cleanup_handle = self.cast_actor(
                    RootCleanupActor,
                    name="outer-cancel-root-cleanup",
                    actor_args=(),
                    actor_kwargs={},
                )

                async def consume(phase: str) -> None:
                    try:
                        await handle.cue({"phase": phase})
                    except BaseException as error:
                        caller_errors[phase] = error
                    finally:
                        if phase == "queued":
                            queued_caller_done.set()

                self.running_caller = asyncio.create_task(consume("running"))
                await actor_entered.wait()
                self.queued_caller = asyncio.create_task(consume("queued"))
                queued_admitted = asyncio.Event()
                loop.call_soon(queued_admitted.set)
                await queued_admitted.wait()
                self.scene_task = asyncio.current_task()
                try:
                    await asyncio.Event().wait()
                finally:
                    factories["root-finally"] = loop.get_task_factory()
                    try:
                        assert await cleanup_handle.cue(
                            {"phase": "root-finally"}
                        ) == ()
                    except BaseException as error:
                        root_cleanup_errors.append(error)
                    finally:
                        root_finally.set()

            async def stop(self) -> None:
                assert loop.get_task_factory() is original_factory
                stop_entered.set()
                await stop_release.wait()
                stop_finished.set()

        production = CancelledOuterProduction([])
        runtime = _native()._Runtime()
        outer: Any | None = None
        rebound: Any | None = None
        try:
            outer = runtime.run(production)
            await _wait(actor_entered)
            assert outer.cancel()
            with pytest.raises(asyncio.CancelledError):
                await outer

            await _wait(root_finally)
            assert root_cleanup_errors == []
            assert [cue.instruction["phase"] for cue in root_cues] == [
                "running",
                "root-finally",
            ]
            scene_source = root_cues[0].source
            assert root_cues[1].source == scene_source
            assert [cue.id for cue in root_cues] == [
                f"{scene_source}-cue0",
                f"{scene_source}-cue2",
            ]
            assert factories["root-finally"] is factories["runtime"]
            scene_task = production.scene_task
            assert scene_task is not None
            assert _scene_cancel_records(cancel_records, scene_task) == [
                (scene_task, loop_thread, None)
            ]
            await _wait(cleanup_entered)
            await _wait(queued_caller_done)
            queued_done_before_release.append(queued_caller_done.is_set())
            assert not stop_entered.is_set()
            assert factories["cleanup"] is factories["runtime"]
            assert loop.get_task_factory() is factories["runtime"]
            assert runtime.request_shutdown() is None
            assert runtime.request_shutdown() is None
            assert running_cued_task is not None
            assert [
                record for record in cancel_records if record[0] is running_cued_task
            ] == [(running_cued_task, loop_thread, None)]
            with pytest.raises(
                RuntimeError,
                match=r"^Production is already bound to an active runtime$",
            ):
                _native()._Runtime().run(production)

            cleanup_release.set()
            await _wait(stop_entered)
            assert loop.get_task_factory() is original_factory
            assert not stop_finished.is_set()
            stop_release.set()
            await _wait(stop_finished)

            rebound_ready: asyncio.Future[None] = loop.create_future()

            def try_rebind() -> None:
                nonlocal rebound
                if rebound_ready.done():
                    return
                candidate = _native()._Runtime()
                candidate.request_shutdown()
                try:
                    rebound = candidate.run(production)
                except RuntimeError as error:
                    assert str(error) == "Production is already bound to an active runtime"
                    loop.call_soon(try_rebind)
                    return
                rebound_ready.set_result(None)

            loop.call_soon(try_rebind)
            await asyncio.wait_for(rebound_ready, TIMEOUT)
            assert rebound is not None
            assert await _await_future(rebound) is None
            assert queued_done_before_release == [True]
            assert set(caller_errors) == {"running", "queued"}
            assert isinstance(caller_errors["running"], asyncio.CancelledError)
            assert isinstance(caller_errors["queued"], asyncio.CancelledError)
        finally:
            cleanup_release.set()
            stop_release.set()
            if rebound is not None and not rebound.done():
                with contextlib.suppress(BaseException):
                    await _await_future(rebound)
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
