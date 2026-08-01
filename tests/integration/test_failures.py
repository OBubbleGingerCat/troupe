from __future__ import annotations

import asyncio
import contextlib
import gc
import importlib
import traceback
import weakref
from types import ModuleType
from typing import Any

import pytest

import troupe


TIMEOUT = 5.0


def _native() -> ModuleType:
    return importlib.import_module("troupe._runtime")


async def _drive(runtime: Any, production: object) -> Any:
    future = runtime.run(production)
    try:
        return await asyncio.wait_for(asyncio.shield(future), TIMEOUT)
    finally:
        runtime.request_shutdown()
        if not future.done():
            with contextlib.suppress(BaseException):
                await asyncio.wait_for(asyncio.shield(future), TIMEOUT)


async def _capture_failure(runtime: Any, production: object) -> BaseException:
    with pytest.raises(_native().ProductionFailed) as captured:
        await _drive(runtime, production)
    return captured.value


def _assert_failure(error: BaseException, phases: tuple[str, ...]) -> tuple[Any, ...]:
    native = _native()

    assert type(error) is native.ProductionFailed
    assert isinstance(error, Exception)
    assert str(error) == "production lifecycle failed"
    assert type(error.failures) is tuple  # type: ignore[attr-defined]
    assert error.__dict__["failures"] is error.failures  # type: ignore[attr-defined]
    failures = error.failures  # type: ignore[attr-defined]
    assert tuple(failure.phase for failure in failures) == phases
    assert all(type(failure) is native.PhaseFailure for failure in failures)
    return failures


def test_start_failure_is_the_only_phase_and_skips_stop() -> None:
    class StartBoom(Exception):
        pass

    events: list[str] = []
    start_error = StartBoom("start failed")

    class StartFailure(troupe.Production):
        async def start(self) -> None:
            events.append("start")
            raise start_error

        async def scene(self) -> None:
            events.append("unexpected-scene")

        async def stop(self) -> None:
            events.append("unexpected-stop")

    error = asyncio.run(_capture_failure(_native()._Runtime(), StartFailure([])))

    failures = _assert_failure(error, ("start",))
    assert events == ["start"]
    assert failures[0].error is start_error


def test_scene_failure_stops_scenes_and_still_stops_once() -> None:
    class SceneBoom(Exception):
        pass

    events: list[str] = []

    class SceneFailure(troupe.Production):
        async def start(self) -> None:
            events.append("start")

        async def scene(self) -> None:
            events.append("scene")
            raise SceneBoom("scene failed")

        async def stop(self) -> None:
            events.append("stop")

    error = asyncio.run(_capture_failure(_native()._Runtime(), SceneFailure([])))

    failures = _assert_failure(error, ("scene",))
    assert events == ["start", "scene", "stop"]
    assert isinstance(failures[0].error, SceneBoom)


def test_scene_error_waits_for_cue_cleanup_without_adding_a_phase() -> None:
    scene_error = RuntimeError("root scene failed")
    cue_cleanup_error = RuntimeError("cue cleanup failed")
    events: list[str] = []
    cue_errors: list[BaseException] = []
    stop_before_release: list[bool] = []
    run_done_before_release: list[bool] = []
    actor_entered: asyncio.Event
    cleanup_entered: asyncio.Event
    cleanup_release: asyncio.Event
    caller_done: asyncio.Event
    stop_entered: asyncio.Event
    runtime = _native()._Runtime()

    class CleanupActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            actor_entered.set()
            try:
                await asyncio.Event().wait()
            except asyncio.CancelledError:
                events.append("cue-cleanup-start")
                cleanup_entered.set()
                await cleanup_release.wait()
                events.append("cue-cleanup-error")
                raise cue_cleanup_error

    class SceneFailureWithCue(troupe.Production):
        async def start(self) -> None:
            events.append("start")

        async def scene(self) -> None:
            events.append("scene")
            handle = self.cast_actor(
                CleanupActor,
                name="failure-cleanup",
                actor_args=(),
                actor_kwargs={},
            )

            async def consume() -> None:
                try:
                    await handle.cue({})
                except BaseException as error:
                    cue_errors.append(error)
                finally:
                    caller_done.set()

            asyncio.create_task(consume())
            await actor_entered.wait()
            raise scene_error

        async def stop(self) -> None:
            events.append("stop")
            stop_entered.set()

    async def scenario() -> BaseException:
        nonlocal actor_entered, cleanup_entered, cleanup_release
        nonlocal caller_done, stop_entered
        actor_entered = asyncio.Event()
        cleanup_entered = asyncio.Event()
        cleanup_release = asyncio.Event()
        caller_done = asyncio.Event()
        stop_entered = asyncio.Event()
        failure_task = asyncio.create_task(
            _capture_failure(runtime, SceneFailureWithCue([]))
        )
        try:
            await asyncio.wait_for(cleanup_entered.wait(), TIMEOUT)
            stop_before_release.append(stop_entered.is_set())
            run_done_before_release.append(failure_task.done())
        finally:
            cleanup_release.set()
        error = await failure_task
        await asyncio.wait_for(caller_done.wait(), TIMEOUT)
        return error

    error = asyncio.run(scenario())
    failures = _assert_failure(error, ("scene",))
    assert failures[0].error is scene_error
    assert stop_before_release == [False]
    assert run_done_before_release == [False]
    assert cue_errors == [cue_cleanup_error]
    assert events == [
        "start",
        "scene",
        "cue-cleanup-start",
        "cue-cleanup-error",
        "stop",
    ]
    assert traceback.extract_tb(scene_error.__traceback__)[-1].name == "scene"


def test_stop_failure_after_normal_shutdown_is_not_retried() -> None:
    class StopBoom(Exception):
        pass

    events: list[str] = []
    runtime = _native()._Runtime()

    class StopFailure(troupe.Production):
        async def start(self) -> None:
            events.append("start")

        async def scene(self) -> None:
            events.append("scene")
            runtime.request_shutdown()

        async def stop(self) -> None:
            events.append("stop")
            raise StopBoom("stop failed")

    error = asyncio.run(_capture_failure(runtime, StopFailure([])))

    failures = _assert_failure(error, ("stop",))
    assert events == ["start", "scene", "stop"]
    assert isinstance(failures[0].error, StopBoom)


def test_scene_and_stop_failures_keep_order_identity_and_tracebacks() -> None:
    class SceneBoom(Exception):
        pass

    class StopBoom(Exception):
        pass

    events: list[str] = []
    scene_error = SceneBoom("scene failed")
    stop_error = StopBoom("stop failed")
    assert scene_error.__traceback__ is None
    assert stop_error.__traceback__ is None

    class DualFailure(troupe.Production):
        async def start(self) -> None:
            events.append("start")

        async def scene(self) -> None:
            events.append("scene")
            raise scene_error

        async def stop(self) -> None:
            events.append("stop")
            raise stop_error

    error = asyncio.run(_capture_failure(_native()._Runtime(), DualFailure([])))

    failures = _assert_failure(error, ("scene", "stop"))
    assert type(error.failures) is tuple  # type: ignore[attr-defined]
    assert events == ["start", "scene", "stop"]
    assert failures[0].error is scene_error
    assert failures[1].error is stop_error

    scene_frame = traceback.extract_tb(scene_error.__traceback__)[-1]
    stop_frame = traceback.extract_tb(stop_error.__traceback__)[-1]
    assert scene_frame.name == "scene"
    assert stop_frame.name == "stop"
    assert scene_frame.filename.endswith("test_failures.py")
    assert stop_frame.filename.endswith("test_failures.py")


def test_scene_cancelled_error_is_normal_and_stops_once() -> None:
    events: list[str] = []

    class CancelledScene(troupe.Production):
        async def start(self) -> None:
            events.append("start")

        async def scene(self) -> None:
            events.append("scene")
            raise asyncio.CancelledError("scene stopped")

        async def stop(self) -> None:
            events.append("stop")

    result = asyncio.run(_drive(_native()._Runtime(), CancelledScene([])))

    assert result is None
    assert events == ["start", "scene", "stop"]


def test_start_cancelled_error_is_a_start_failure() -> None:
    events: list[str] = []
    start_error = asyncio.CancelledError("start cancelled")

    class CancelledStart(troupe.Production):
        async def start(self) -> None:
            events.append("start")
            raise start_error

        async def scene(self) -> None:
            events.append("unexpected-scene")

        async def stop(self) -> None:
            events.append("unexpected-stop")

    error = asyncio.run(_capture_failure(_native()._Runtime(), CancelledStart([])))

    failures = _assert_failure(error, ("start",))
    assert events == ["start"]
    assert isinstance(failures[0].error, asyncio.CancelledError)


def test_stop_cancelled_error_is_a_stop_failure() -> None:
    events: list[str] = []
    stop_error = asyncio.CancelledError("stop cancelled")
    runtime = _native()._Runtime()

    class CancelledStop(troupe.Production):
        async def start(self) -> None:
            events.append("start")

        async def scene(self) -> None:
            events.append("scene")
            runtime.request_shutdown()

        async def stop(self) -> None:
            events.append("stop")
            raise stop_error

    error = asyncio.run(_capture_failure(runtime, CancelledStop([])))

    failures = _assert_failure(error, ("stop",))
    assert events == ["start", "scene", "stop"]
    assert isinstance(failures[0].error, asyncio.CancelledError)


def test_failure_types_are_private_native_and_phase_values_are_frozen() -> None:
    class FreezeBoom(Exception):
        pass

    original = FreezeBoom("frozen")

    class FreezeFailure(troupe.Production):
        async def start(self) -> None:
            raise original

    native = _native()
    error = asyncio.run(_capture_failure(native._Runtime(), FreezeFailure([])))
    phase_failure = _assert_failure(error, ("start",))[0]

    assert native.PhaseFailure.__module__ == "troupe._runtime"
    assert native.ProductionFailed.__module__ == "troupe._runtime"
    assert "PhaseFailure" not in vars(troupe)
    assert "ProductionFailed" not in vars(troupe)
    assert type(phase_failure) is native.PhaseFailure

    with pytest.raises(AttributeError):
        phase_failure.phase = "stop"
    assert phase_failure.phase == "start"
    assert phase_failure.error is original

    with pytest.raises(AttributeError):
        phase_failure.error = RuntimeError("replacement")
    assert phase_failure.phase == "start"
    assert phase_failure.error is original


def test_phase_failure_participates_in_cyclic_gc() -> None:
    class CycleBoom(Exception):
        pass

    class CycleFailure(troupe.Production):
        async def start(self) -> None:
            raise CycleBoom("cycle")

    async def create_cycle() -> tuple[bool, weakref.ReferenceType[BaseException]]:
        error = await _capture_failure(_native()._Runtime(), CycleFailure([]))
        phase_failure = _assert_failure(error, ("start",))[0]
        original = phase_failure.error
        original.phase_failure = phase_failure
        return gc.is_tracked(phase_failure), weakref.ref(original)

    tracked, reference = asyncio.run(create_cycle())
    try:
        for _ in range(3):
            gc.collect()
        assert reference() is None
        assert tracked
    finally:
        remaining = reference()
        if remaining is not None:
            del remaining.phase_failure
            del remaining
            gc.collect()
