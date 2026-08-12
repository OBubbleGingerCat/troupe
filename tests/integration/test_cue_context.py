from __future__ import annotations

import asyncio
import contextlib
import contextvars
import gc
import importlib
import inspect
import os
import re
import signal
import threading
import traceback
import weakref
from collections.abc import Coroutine
from typing import Any

import pytest

import troupe
TEST_AGENT_PROFILE = troupe.AgentProfile(
    agent="codex",
    workspace="/tmp",
    model="test-model",
    effort=None,
)


TIMEOUT = 5.0
CUE_CONTEXT_ERROR = (
    "ActorHandle.cue() must be called within an active scene context"
)
EFFECT_CONTEXT_ERROR = (
    "Actor.make_effect() must be called on the current actor within its active cued context"
)
FACTORY_REPLACED_ERROR = (
    "Production event loop task factory was replaced while runtime was active"
)
BOUND_ERROR = "Production is already bound to an active runtime"


def _native() -> Any:
    return importlib.import_module("troupe._runtime")


def _cast(
    production: troupe.Production,
    actor_type: type[troupe.Actor],
    name: str,
) -> troupe.ActorHandle:
    return production.cast_actor(
        actor_type,
        name=name,
        agent_profile=TEST_AGENT_PROFILE,
        actor_args=(),
        actor_kwargs={},
    )


async def _run(runtime: Any, production: troupe.Production) -> None:
    await asyncio.wait_for(asyncio.shield(runtime.run(production)), TIMEOUT)


def _assert_context_error(error: BaseException) -> None:
    assert type(error) is troupe.CueContextError
    assert str(error) == CUE_CONTEXT_ERROR


def _assert_effect_context_error(error: BaseException) -> None:
    assert type(error) is troupe.EffectContextError
    assert str(error) == EFFECT_CONTEXT_ERROR


def _phase_errors(error: BaseException) -> dict[str, BaseException]:
    assert type(error) is _native().ProductionFailed
    failures = tuple(error.failures)
    phases = tuple(failure.phase for failure in failures)
    assert len(phases) == len(set(phases))
    return {failure.phase: failure.error for failure in failures}


async def _loop_barrier() -> None:
    reached = asyncio.Event()
    asyncio.get_running_loop().call_soon(reached.set)
    await reached.wait()


def test_sync_context_precedes_instruction_type_and_invalid_calls_do_nothing() -> None:
    seen: list[troupe.Cue] = []
    errors: dict[str, BaseException] = {}

    class HostileDict(dict[Any, Any]):
        def copy(self) -> dict[Any, Any]:
            raise AssertionError("invalid context copied instruction")

    class ProbeActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            seen.append(cue)
            return ()

    runtime = _native()._Runtime()

    class ContextProduction(troupe.Production):
        async def start(self) -> None:
            for key, instruction in (
                ("start-bad", object()),
                ("start-dict", HostileDict()),
            ):
                try:
                    handle.cue(instruction)  # type: ignore[arg-type]
                except BaseException as error:
                    errors[key] = error

        async def scene(self) -> None:
            for key, call in (
                ("missing", lambda: handle.cue()),
                ("extra", lambda: handle.cue({}, {})),
            ):
                try:
                    call()  # type: ignore[call-arg]
                except BaseException as error:
                    errors[key] = error

            try:
                handle.cue(object())  # type: ignore[arg-type]
            except BaseException as error:
                errors["legal-type"] = error
            assert await handle.cue({"valid": True}) == ()
            runtime.request_shutdown()

        async def stop(self) -> None:
            try:
                handle.cue(object())  # type: ignore[arg-type]
            except BaseException as error:
                errors["stop"] = error

    production = ContextProduction([])
    handle = _cast(production, ProbeActor, "context")
    try:
        handle.cue(object())  # type: ignore[arg-type]
    except BaseException as error:
        errors["outside-before"] = error

    asyncio.run(_run(runtime, production))

    try:
        handle.cue(object())  # type: ignore[arg-type]
    except BaseException as error:
        errors["outside-after"] = error

    for key in (
        "start-bad",
        "start-dict",
        "stop",
        "outside-before",
        "outside-after",
    ):
        _assert_context_error(errors[key])
    assert type(errors["missing"]) is TypeError
    assert type(errors["extra"]) is TypeError
    assert type(errors["legal-type"]) is TypeError
    assert len(seen) == 1
    assert seen[0].id.endswith("-cue0")


def test_cross_production_handle_fails_before_instruction_validation() -> None:
    errors: list[BaseException] = []
    own_cues: list[troupe.Cue] = []
    runtime = _native()._Runtime()

    class OtherActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            raise AssertionError("cross-production cue reached actor")

    other = troupe.Production([])
    other_handle = _cast(other, OtherActor, "other")

    class OwnActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            own_cues.append(cue)
            return ()

    class RunningProduction(troupe.Production):
        async def scene(self) -> None:
            own_handle = _cast(self, OwnActor, "own")
            try:
                other_handle.cue(object())  # type: ignore[arg-type]
            except BaseException as error:
                errors.append(error)
            assert await own_handle.cue({}) == ()
            runtime.request_shutdown()

    asyncio.run(_run(runtime, RunningProduction([])))
    assert len(errors) == 1
    _assert_context_error(errors[0])
    assert len(own_cues) == 1
    assert own_cues[0].id.endswith("-cue0")


def test_registered_scene_and_cued_descendants_are_legal_on_one_loop_thread() -> None:
    records: list[tuple[str, int, asyncio.AbstractEventLoop, asyncio.Task[Any]]] = []
    runtime = _native()._Runtime()
    handles: dict[str, troupe.ActorHandle] = {}

    class LeafActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            task = asyncio.current_task()
            assert task is not None
            records.append(
                (
                    cue.instruction["label"],
                    threading.get_ident(),
                    asyncio.get_running_loop(),
                    task,
                )
            )
            return ()

    class ParentActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            async def child() -> tuple[troupe.Effect, ...]:
                return await handles["cued-child"].cue({"label": "cued-child"})

            assert await asyncio.create_task(child()) == ()
            return ()

    class LineageProduction(troupe.Production):
        async def scene(self) -> None:
            scene_task = asyncio.current_task()
            assert scene_task is not None
            records.append(
                (
                    "scene",
                    threading.get_ident(),
                    asyncio.get_running_loop(),
                    scene_task,
                )
            )
            for name in ("root", "create-task", "loop-task", "cued-child"):
                handles[name] = _cast(self, LeafActor, name)
            parent = _cast(self, ParentActor, "parent")

            assert await handles["root"].cue({"label": "root"}) == ()
            assert await asyncio.create_task(
                handles["create-task"].cue({"label": "create-task"})
            ) == ()
            assert await asyncio.get_running_loop().create_task(
                handles["loop-task"].cue({"label": "loop-task"})
            ) == ()
            assert await parent.cue({"label": "parent"}) == ()
            runtime.request_shutdown()

    asyncio.run(_run(runtime, LineageProduction([])))

    loop_ids = {id(record[2]) for record in records}
    thread_ids = {record[1] for record in records}
    assert len(loop_ids) == 1
    assert len(thread_ids) == 1
    assert {record[0] for record in records} == {
        "scene",
        "root",
        "create-task",
        "loop-task",
        "cued-child",
    }
    scene_task = next(record[3] for record in records if record[0] == "scene")
    assert all(
        record[3] is not scene_task for record in records if record[0] != "scene"
    )


def test_unregistered_tasks_callbacks_threads_and_copied_context_cannot_forge_lineage() -> None:
    errors: dict[str, BaseException] = {}
    successes: list[troupe.Cue] = []
    runtime = _native()._Runtime()

    class LeafActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            successes.append(cue)
            return ()

    class ContextProduction(troupe.Production):
        async def scene(self) -> None:
            handle = _cast(self, LeafActor, "leaf")
            loop = asyncio.get_running_loop()

            async def probe(label: str) -> None:
                try:
                    runner = handle.cue(object())  # type: ignore[arg-type]
                except BaseException as error:
                    errors[label] = error
                else:
                    runner.close()
                    errors[label] = AssertionError(
                        "invalid context did not fail at handle.cue()"
                    )

            async def inert_probe() -> None:
                assert await handle.cue({"label": "inert-direct-await"}) == ()

            direct = asyncio.Task(probe("direct-task"), loop=loop)
            await direct

            callback_done = asyncio.Event()

            def callback() -> None:
                task = asyncio.create_task(probe("callback"))
                task.add_done_callback(lambda _: callback_done.set())

            loop.call_soon(callback)
            await callback_done.wait()

            copied = contextvars.copy_context()
            copied_done = asyncio.Event()

            def copied_callback() -> None:
                task = asyncio.create_task(probe("copied-callback"))
                task.add_done_callback(lambda _: copied_done.set())

            loop.call_soon(copied_callback, context=copied)
            await copied_done.wait()

            def thread_direct() -> None:
                try:
                    copied.run(handle.cue, object())  # type: ignore[arg-type]
                except BaseException as error:
                    errors["thread-direct"] = error

            await asyncio.to_thread(thread_direct)

            thread_callback_done = asyncio.Event()

            def thread_callback() -> None:
                task = asyncio.create_task(probe("thread-callback"))
                task.add_done_callback(lambda _: thread_callback_done.set())

            def schedule_thread_callback() -> None:
                loop.call_soon_threadsafe(thread_callback, context=copied)

            await asyncio.to_thread(schedule_thread_callback)
            await thread_callback_done.wait()

            def thread_round_trip() -> None:
                future = asyncio.run_coroutine_threadsafe(
                    probe("threadsafe-task"), loop
                )
                future.result(TIMEOUT)

            await asyncio.to_thread(thread_round_trip)

            inert = await asyncio.to_thread(
                inert_probe
            )
            await inert
            runtime.request_shutdown()

    asyncio.run(_run(runtime, ContextProduction([])))

    for label in (
        "direct-task",
        "callback",
        "copied-callback",
        "thread-direct",
        "thread-callback",
        "threadsafe-task",
    ):
        _assert_context_error(errors[label])
    assert len(successes) == 1
    assert successes[0].instruction["label"] == "inert-direct-await"
    assert successes[0].id.endswith("-cue0")


@pytest.mark.skipif(not hasattr(os, "fork"), reason="Linux fork is required")
def test_fork_child_rejects_before_inherited_runtime_locks() -> None:
    result: dict[str, str] = {}
    cues: list[troupe.Cue] = []
    runtime = _native()._Runtime()

    class LeafActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            cues.append(cue)
            return ()

    class ForkProduction(troupe.Production):
        async def scene(self) -> None:
            handle = _cast(self, LeafActor, "fork")
            read_fd, write_fd = os.pipe()
            pid = os.fork()
            if pid == 0:
                os.close(read_fd)
                try:
                    handle.cue(object())  # type: ignore[arg-type]
                except BaseException as error:
                    payload = f"{type(error).__name__}:{error}".encode()
                else:
                    payload = b"unexpected-success"
                os.write(write_fd, payload)
                os.close(write_fd)
                os._exit(0)

            os.close(write_fd)
            loop = asyncio.get_running_loop()
            readable: asyncio.Future[None] = loop.create_future()

            def mark_readable() -> None:
                if not readable.done():
                    readable.set_result(None)

            def terminate_and_reap() -> None:
                with contextlib.suppress(ProcessLookupError):
                    os.kill(pid, signal.SIGKILL)
                with contextlib.suppress(ChildProcessError):
                    os.waitpid(pid, 0)

            loop.add_reader(read_fd, mark_readable)
            try:
                try:
                    await asyncio.wait_for(readable, TIMEOUT)
                    payload = os.read(read_fd, 4096)
                except BaseException:
                    terminate_and_reap()
                    raise
            finally:
                loop.remove_reader(read_fd)
                os.close(read_fd)
            deadline = loop.time() + TIMEOUT
            while True:
                waited, _ = os.waitpid(pid, os.WNOHANG)
                if waited == pid:
                    break
                if loop.time() >= deadline:
                    terminate_and_reap()
                    raise TimeoutError("fork child did not exit")
                await _loop_barrier()
            result["payload"] = payload.decode()
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    asyncio.run(_run(runtime, ForkProduction([])))
    assert result["payload"] == f"CueContextError:{CUE_CONTEXT_ERROR}"
    assert len(cues) == 1
    assert cues[0].id.endswith("-cue0")


def test_scene_and_cue_identity_source_and_transfer_follow_first_drive_task() -> None:
    cues: dict[str, troupe.Cue] = {}
    stored: dict[str, Any] = {}
    runtime = _native()._Runtime()

    class RecordingActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            cues[cue.instruction["label"]] = cue
            return ()

    class ForwardActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            cues[cue.instruction["label"]] = cue
            if cue.instruction["label"] == "root":
                assert await targets["downstream"].cue(
                    {"label": "downstream"}
                ) == ()
            elif cue.instruction["label"] == "take-root-runner":
                async def consume() -> None:
                    assert await stored.pop("root-runner") == ()

                await asyncio.create_task(consume())
            elif cue.instruction["label"] == "make-actor-runner":
                stored["actor-runner"] = targets["actor-to-root"].cue(
                    {"label": "actor-to-root"}
                )
            return ()

    targets: dict[str, troupe.ActorHandle] = {}

    class IdentityProduction(troupe.Production):
        def __init__(self, args: list[str]) -> None:
            self.scene_count = 0

        async def scene(self) -> None:
            self.scene_count += 1
            if self.scene_count == 1:
                for name in (
                    "root-target",
                    "downstream",
                    "child-target",
                    "actor-takes-root",
                    "actor-to-root",
                    "actor-makes-root",
                ):
                    actor_type = (
                        ForwardActor
                        if name in {"root-target", "actor-takes-root", "actor-makes-root"}
                        else RecordingActor
                    )
                    targets[name] = _cast(self, actor_type, name)

                assert await targets["root-target"].cue({"label": "root"}) == ()

                async def child() -> None:
                    assert await targets["child-target"].cue(
                        {"label": "scene-child"}
                    ) == ()

                await asyncio.create_task(child())

                stored["root-runner"] = targets["actor-to-root"].cue(
                    {"label": "root-to-actor"}
                )
                assert await targets["actor-takes-root"].cue(
                    {"label": "take-root-runner"}
                ) == ()

                assert await targets["actor-makes-root"].cue(
                    {"label": "make-actor-runner"}
                ) == ()
                assert await stored.pop("actor-runner") == ()
            else:
                assert await targets["root-target"].cue(
                    {"label": "second-scene"}
                ) == ()
                runtime.request_shutdown()

    asyncio.run(_run(runtime, IdentityProduction([])))

    root = cues["root"]
    downstream = cues["downstream"]
    child = cues["scene-child"]
    first_scene = root.source
    assert re.fullmatch(
        r"scene-[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}",
        first_scene,
    )
    assert root.id == f"{first_scene}-cue0"
    assert downstream.id == f"{first_scene}-cue1"
    assert child.id == f"{first_scene}-cue2"
    assert downstream.source == "root-target"
    assert root.source == child.source == first_scene
    assert cues["root-to-actor"].source == "actor-takes-root"
    assert cues["actor-to-root"].source == first_scene

    second = cues["second-scene"]
    assert second.source != first_scene
    assert second.id == f"{second.source}-cue0"


def test_root_and_cued_inline_drivers_reject_ready_descendants_after_terminal() -> None:
    errors: dict[str, BaseException] = {}
    cue_ids: list[str] = []
    tasks: list[asyncio.Task[Any]] = []
    runtime = _native()._Runtime()
    root_gate: asyncio.Event
    cued_gate: asyncio.Event
    target: troupe.ActorHandle

    async def late_probe(label: str, gate: asyncio.Event) -> None:
        await gate.wait()
        try:
            runner = target.cue(object())  # type: ignore[arg-type]
        except BaseException as error:
            errors[label] = error
        else:
            runner.close()
            errors[label] = AssertionError(
                "scope remained open after its owner coroutine returned"
            )

    class ParentActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            cue_ids.append(cue.id)
            child = asyncio.create_task(late_probe("cued-late", cued_gate))
            tasks.append(child)
            cued_gate.set()
            return ()

    class ProbeActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            cue_ids.append(cue.id)
            return ()

    class DriverProduction(troupe.Production):
        async def scene(self) -> None:
            nonlocal root_gate, cued_gate, target
            root_gate = asyncio.Event()
            cued_gate = asyncio.Event()
            target = _cast(self, ProbeActor, "late-target")
            parent = _cast(self, ParentActor, "late-parent")
            assert await parent.cue({}) == ()
            await _loop_barrier()
            assert await target.cue({"label": "after-cued-late"}) == ()
            child = asyncio.create_task(late_probe("root-late", root_gate))
            tasks.append(child)
            root_gate.set()
            runtime.request_shutdown()

        async def stop(self) -> None:
            await asyncio.gather(*tasks)

    asyncio.run(_run(runtime, DriverProduction([])))
    _assert_context_error(errors["cued-late"])
    _assert_context_error(errors["root-late"])
    scene_prefix = cue_ids[0].rsplit("-cue", 1)[0]
    assert cue_ids == [f"{scene_prefix}-cue0", f"{scene_prefix}-cue1"]


def test_scene_close_cancels_and_drains_running_cued_before_stop() -> None:
    actor_entered: asyncio.Event
    cleanup_entered: asyncio.Event
    cleanup_release: asyncio.Event
    stop_entered: asyncio.Event
    downstream_errors: list[BaseException] = []
    caller_errors: list[BaseException] = []
    cleanup_effects: list[troupe.Effect] = []
    runtime = _native()._Runtime()
    downstream: troupe.ActorHandle

    class DownstreamActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            raise AssertionError("cleanup downstream cue was admitted")

    class BlockingActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            actor_entered.set()
            try:
                await asyncio.Event().wait()
            except asyncio.CancelledError:
                cleanup_entered.set()

                async def inherited_cleanup_child() -> None:
                    cleanup_effects.append(
                        self.make_effect(
                            troupe.Effect,
                            effect_args=(),
                            effect_kwargs={},
                        )
                    )

                await asyncio.create_task(inherited_cleanup_child())
                try:
                    downstream.cue(object())  # type: ignore[arg-type]
                except BaseException as error:
                    downstream_errors.append(error)
                await cleanup_release.wait()
                raise

    class ClosingProduction(troupe.Production):
        def __init__(self, args: list[str]) -> None:
            self.scene_count = 0

        async def scene(self) -> None:
            nonlocal downstream
            self.scene_count += 1
            downstream = _cast(self, DownstreamActor, "cleanup-downstream")
            target = _cast(self, BlockingActor, "blocking")

            async def consume() -> None:
                try:
                    await target.cue({})
                except BaseException as error:
                    caller_errors.append(error)

            asyncio.create_task(consume())
            await actor_entered.wait()
            runtime.request_shutdown()

        async def stop(self) -> None:
            stop_entered.set()

    async def scenario() -> None:
        nonlocal actor_entered, cleanup_entered, cleanup_release, stop_entered
        actor_entered = asyncio.Event()
        cleanup_entered = asyncio.Event()
        cleanup_release = asyncio.Event()
        stop_entered = asyncio.Event()
        production = ClosingProduction([])
        run_task = asyncio.create_task(_run(runtime, production))
        await asyncio.wait_for(cleanup_entered.wait(), TIMEOUT)
        assert production.scene_count == 1
        assert not stop_entered.is_set()
        assert not run_task.done()
        cleanup_release.set()
        await asyncio.wait_for(run_task, TIMEOUT)

    asyncio.run(scenario())
    assert stop_entered.is_set()
    assert len(downstream_errors) == 1
    _assert_context_error(downstream_errors[0])
    assert len(cleanup_effects) == 1
    assert cleanup_effects[0].id.endswith("-cue0-effect0")
    assert cleanup_effects[0].owner == "blocking"
    assert len(caller_errors) == 1
    assert isinstance(caller_errors[0], asyncio.CancelledError)


def test_normal_scene_return_cancels_queued_and_drains_before_next_scene() -> None:
    actor_entered: asyncio.Event
    cleanup_entered: asyncio.Event
    cleanup_release: asyncio.Event
    queued_caller_done: asyncio.Event
    second_scene_entered: asyncio.Event
    caller_errors: dict[str, BaseException] = {}
    actor_calls: list[str] = []
    queued_done_before_release: list[bool] = []
    next_scene_before_release: list[bool] = []
    run_done_before_release: list[bool] = []
    runtime = _native()._Runtime()

    class DrainingActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            label = cue.instruction["label"]
            actor_calls.append(label)
            if label == "running":
                actor_entered.set()
                try:
                    await asyncio.Event().wait()
                except asyncio.CancelledError:
                    cleanup_entered.set()
                    await cleanup_release.wait()
                    raise
            return ()

    async def scenario() -> None:
        nonlocal actor_entered, cleanup_entered, cleanup_release
        nonlocal queued_caller_done, second_scene_entered
        actor_entered = asyncio.Event()
        cleanup_entered = asyncio.Event()
        cleanup_release = asyncio.Event()
        queued_caller_done = asyncio.Event()
        second_scene_entered = asyncio.Event()

        class NormalReturnProduction(troupe.Production):
            def __init__(self, args: list[str]) -> None:
                self.scene_count = 0
                self.handle: troupe.ActorHandle | None = None
                self.callers: list[asyncio.Task[None]] = []

            async def scene(self) -> None:
                self.scene_count += 1
                if self.scene_count == 2:
                    second_scene_entered.set()
                    await asyncio.gather(*self.callers)
                    assert self.handle is not None
                    assert await self.handle.cue({"label": "after"}) == ()
                    runtime.request_shutdown()
                    return

                self.handle = _cast(self, DrainingActor, "normal-drain")

                async def consume(label: str) -> None:
                    assert self.handle is not None
                    try:
                        await self.handle.cue({"label": label})
                    except BaseException as error:
                        caller_errors[label] = error
                    finally:
                        if label == "queued":
                            queued_caller_done.set()

                self.callers.append(asyncio.create_task(consume("running")))
                await actor_entered.wait()
                self.callers.append(asyncio.create_task(consume("queued")))
                await _loop_barrier()

        production = NormalReturnProduction([])
        run_task = asyncio.create_task(_run(runtime, production))
        try:
            await asyncio.wait_for(cleanup_entered.wait(), TIMEOUT)
            await asyncio.wait_for(queued_caller_done.wait(), TIMEOUT)
            queued_done_before_release.append(queued_caller_done.is_set())
            next_scene_before_release.append(second_scene_entered.is_set())
            run_done_before_release.append(run_task.done())
        finally:
            cleanup_release.set()
        await run_task

    asyncio.run(scenario())
    assert queued_done_before_release == [True]
    assert next_scene_before_release == [False]
    assert run_done_before_release == [False]
    assert set(caller_errors) == {"running", "queued"}
    assert isinstance(caller_errors["running"], asyncio.CancelledError)
    assert isinstance(caller_errors["queued"], asyncio.CancelledError)
    assert actor_calls == ["running", "after"]


def test_caller_cancellation_waits_for_cued_cleanup_and_actor_reopens() -> None:
    actor_entered: asyncio.Event
    cleanup_entered: asyncio.Event
    cleanup_release: asyncio.Event
    cue_ids: list[str] = []
    runtime = _native()._Runtime()

    class CancellableActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            cue_ids.append(cue.id)
            if cue.instruction["label"] == "cancel":
                actor_entered.set()
                try:
                    await asyncio.Event().wait()
                except asyncio.CancelledError:
                    cleanup_entered.set()
                    await cleanup_release.wait()
                    raise
            return ()

    async def scenario() -> None:
        nonlocal actor_entered, cleanup_entered, cleanup_release
        actor_entered = asyncio.Event()
        cleanup_entered = asyncio.Event()
        cleanup_release = asyncio.Event()

        class CancellationProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, CancellableActor, "caller-cancel")
                caller = asyncio.create_task(handle.cue({"label": "cancel"}))
                await actor_entered.wait()
                assert caller.cancel()
                await cleanup_entered.wait()
                assert not caller.done()
                cleanup_release.set()
                with pytest.raises(asyncio.CancelledError):
                    await caller
                assert await handle.cue({"label": "after"}) == ()
                runtime.request_shutdown()

        await _run(runtime, CancellationProduction([]))

    asyncio.run(scenario())
    assert len(cue_ids) == 2
    scene_prefix = cue_ids[0].rsplit("-cue", 1)[0]
    assert cue_ids == [f"{scene_prefix}-cue0", f"{scene_prefix}-cue1"]


def test_task_registry_is_weak_while_scene_and_cued_scopes_remain_active() -> None:
    dead: dict[str, bool] = {}
    runtime = _native()._Runtime()

    class WeakActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            async def child() -> None:
                return None

            task = asyncio.create_task(child())
            reference = weakref.ref(task)
            await task
            del task
            await _loop_barrier()
            gc.collect()
            dead["cued"] = reference() is None
            return ()

    class WeakProduction(troupe.Production):
        async def scene(self) -> None:
            async def child() -> None:
                return None

            task = asyncio.create_task(child())
            reference = weakref.ref(task)
            await task
            del task
            await _loop_barrier()
            gc.collect()
            dead["scene"] = reference() is None
            handle = _cast(self, WeakActor, "weak")
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    asyncio.run(_run(runtime, WeakProduction([])))
    assert dead == {"scene": True, "cued": True}


def test_completed_run_releases_registered_tasks_and_pending_runner_graphs() -> None:
    task_refs: list[weakref.ReferenceType[asyncio.Task[Any]]] = []
    actor_refs: list[weakref.ReferenceType[troupe.Actor]] = []
    record_tasks = False

    class CycleActor(troupe.Actor):
        def __init__(self) -> None:
            actor_refs.append(weakref.ref(self))

    class ChildActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            async def child() -> None:
                return None

            await asyncio.create_task(child())
            return ()

    async def scenario() -> None:
        nonlocal record_tasks
        loop = asyncio.get_running_loop()
        previous = loop.get_task_factory()

        def original_factory(
            factory_loop: asyncio.AbstractEventLoop,
            coroutine: Coroutine[Any, Any, Any],
            **kwargs: Any,
        ) -> asyncio.Task[Any]:
            task = asyncio.Task(coroutine, loop=factory_loop, **kwargs)
            if record_tasks:
                task_refs.append(weakref.ref(task))
            return task

        loop.set_task_factory(original_factory)
        runtime = _native()._Runtime()

        class ReleaseProduction(troupe.Production):
            async def start(self) -> None:
                nonlocal record_tasks
                record_tasks = True

            async def scene(self) -> None:
                async def scene_child() -> None:
                    return None

                await asyncio.create_task(scene_child())
                handle = _cast(self, ChildActor, "release-child")
                assert await handle.cue({}) == ()

                cycle_handle = _cast(self, CycleActor, "release-cycle")
                instruction: dict[str, Any] = {}
                runner = cycle_handle.cue(instruction)
                instruction["runner"] = runner
                actor = actor_refs[-1]()
                assert actor is not None
                actor.runner = runner  # type: ignore[attr-defined]
                del actor, cycle_handle, instruction, runner
                runtime.request_shutdown()

        production = ReleaseProduction([])
        try:
            await _run(runtime, production)
            record_tasks = False
            await _loop_barrier()
            await _loop_barrier()
            gc.collect()
            assert len(task_refs) >= 4
            assert all(reference() is None for reference in task_refs)
            assert actor_refs[-1]() is None
            assert production.get_actor("release-cycle") is None

            rebound = _native()._Runtime()
            rebound.request_shutdown()
            await _run(rebound, production)
        finally:
            record_tasks = False
            loop.set_task_factory(previous)

    asyncio.run(scenario())


def test_active_binding_is_exclusive_and_rebinds_after_stop_on_new_loop() -> None:
    start_entered: asyncio.Event
    start_release: asyncio.Event
    stop_entered: asyncio.Event
    stop_release: asyncio.Event
    events: list[str] = []
    cue_loops: list[asyncio.AbstractEventLoop] = []

    class BindingActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            cue_loops.append(asyncio.get_running_loop())
            return ()

    class BindingProduction(troupe.Production):
        def __init__(self, args: list[str]) -> None:
            self.runtime: Any = None
            self.handle: troupe.ActorHandle | None = None

        async def start(self) -> None:
            events.append("start")
            start_entered.set()
            await start_release.wait()

        async def scene(self) -> None:
            events.append("scene")
            if self.handle is None:
                self.handle = _cast(self, BindingActor, "rebound-actor")
            assert await self.handle.cue({}) == ()
            self.runtime.request_shutdown()

        async def stop(self) -> None:
            events.append("stop")
            stop_entered.set()
            await stop_release.wait()

    production = BindingProduction([])
    runtime_one = _native()._Runtime()

    async def first_run() -> None:
        nonlocal start_entered, start_release, stop_entered, stop_release
        start_entered = asyncio.Event()
        start_release = asyncio.Event()
        stop_entered = asyncio.Event()
        stop_release = asyncio.Event()
        production.runtime = runtime_one
        first = runtime_one.run(production)
        await start_entered.wait()
        with pytest.raises(RuntimeError, match=f"^{re.escape(BOUND_ERROR)}$"):
            _native()._Runtime().run(production)
        start_release.set()
        await stop_entered.wait()
        with pytest.raises(RuntimeError, match=f"^{re.escape(BOUND_ERROR)}$"):
            _native()._Runtime().run(production)
        stop_release.set()
        await asyncio.wait_for(asyncio.shield(first), TIMEOUT)

    asyncio.run(first_run())

    async def second_run() -> None:
        runtime = _native()._Runtime()
        production.runtime = runtime
        start_release.set()
        stop_release.set()
        await _run(runtime, production)

    asyncio.run(second_run())
    assert events == ["start", "scene", "stop", "start", "scene", "stop"]
    assert len(cue_loops) == 2
    assert cue_loops[0] is not cue_loops[1]


def test_task_factory_delegates_across_scenes_and_restores_before_stop() -> None:
    factory_phase: contextvars.ContextVar[str] = contextvars.ContextVar(
        "factory_phase",
        default="runtime",
    )
    records: list[tuple[str, dict[str, Any], asyncio.Task[Any]]] = []
    factories: dict[str, Any] = {}
    observations: dict[str, list[asyncio.Task[Any]]] = {
        "scene_tasks": [],
        "cued_tasks": [],
    }
    provided_contexts: dict[str, list[contextvars.Context]] = {}
    events: list[str] = []

    class IdentityTask(asyncio.Task[Any]):
        def __hash__(self) -> int:
            return id(self)

        def __eq__(self, other: object) -> bool:
            return self is other

        def get_coro(self) -> Coroutine[Any, Any, Any]:
            raise AssertionError("runtime dispatched overridden Task.get_coro()")

    async def scenario() -> None:
        loop = asyncio.get_running_loop()
        previous = loop.get_task_factory()

        def original_factory(
            factory_loop: asyncio.AbstractEventLoop,
            coroutine: Coroutine[Any, Any, Any],
            **kwargs: Any,
        ) -> asyncio.Task[Any]:
            task = IdentityTask(coroutine, loop=factory_loop, **kwargs)
            records.append((factory_phase.get(), dict(kwargs), task))
            return task

        loop.set_task_factory(original_factory)
        factories["original"] = original_factory
        runtime = _native()._Runtime()

        async def child() -> None:
            assert asyncio.current_task() is not None

        def create_child(label: str) -> asyncio.Task[None]:
            kwargs: dict[str, Any] = {"name": label}
            if "context" in inspect.signature(loop.create_task).parameters:
                context = contextvars.copy_context()
                provided_contexts.setdefault(label, []).append(context)
                kwargs["context"] = context
            return loop.create_task(child(), **kwargs)

        class FactoryActor(troupe.Actor):
            async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
                factories.setdefault("wrapper", loop.get_task_factory())
                assert loop.get_task_factory() is factories["wrapper"]
                task = asyncio.current_task()
                assert task is not None
                observations["cued_tasks"].append(task)
                token = factory_phase.set("cued-child")
                try:
                    await create_child("cued-child")
                finally:
                    factory_phase.reset(token)
                return ()

        class FactoryProduction(troupe.Production):
            def __init__(self, args: list[str]) -> None:
                self.scene_count = 0

            async def start(self) -> None:
                events.append("start")
                assert loop.get_task_factory() is original_factory
                token = factory_phase.set("start-child")
                try:
                    await create_child("start-child")
                finally:
                    factory_phase.reset(token)

            async def scene(self) -> None:
                self.scene_count += 1
                events.append(f"scene:{self.scene_count}")
                wrapper = factories.setdefault("wrapper", loop.get_task_factory())
                assert wrapper is not original_factory
                assert loop.get_task_factory() is wrapper
                task = asyncio.current_task()
                assert task is not None
                observations["scene_tasks"].append(task)

                token = factory_phase.set("scene-child")
                try:
                    await create_child("scene-child")
                finally:
                    factory_phase.reset(token)
                handle = _cast(self, FactoryActor, f"factory-{self.scene_count}")
                token = factory_phase.set("cued-task")
                try:
                    assert await handle.cue({}) == ()
                finally:
                    factory_phase.reset(token)
                if self.scene_count == 2:
                    runtime.request_shutdown()

            async def stop(self) -> None:
                events.append("stop")
                assert loop.get_task_factory() is original_factory

        try:
            await _run(runtime, FactoryProduction([]))
            assert loop.get_task_factory() is original_factory
        finally:
            loop.set_task_factory(previous)

    asyncio.run(scenario())

    assert events == ["start", "scene:1", "scene:2", "stop"]
    phases = [phase for phase, _, _ in records]
    assert phases.count("start-child") == 1
    assert phases.count("scene-child") == 2
    assert phases.count("cued-child") == 2
    start_kwargs = next(kwargs for phase, kwargs, _ in records if phase == "start-child")
    scene_kwargs = next(kwargs for phase, kwargs, _ in records if phase == "scene-child")
    assert set(scene_kwargs) == set(start_kwargs)
    if "name" in start_kwargs:
        assert start_kwargs["name"] == "start-child"
        assert scene_kwargs["name"] == "scene-child"
    if "context" in start_kwargs:
        assert start_kwargs["context"] is provided_contexts["start-child"][0]
        assert scene_kwargs["context"] is provided_contexts["scene-child"][0]
    delegated_tasks = [task for _, _, task in records]
    assert all(
        any(task is delegated for delegated in delegated_tasks)
        for task in observations["scene_tasks"] + observations["cued_tasks"]
    )
    assert sum(phase == "cued-task" for phase, _, _ in records) == 2
    assert all(type(task) is IdentityTask for _, _, task in records)


def test_start_failure_never_installs_task_factory_wrapper() -> None:
    start_error = RuntimeError("start failed")
    observations: dict[str, Any] = {}

    async def scenario() -> None:
        loop = asyncio.get_running_loop()
        previous = loop.get_task_factory()

        def original_factory(
            factory_loop: asyncio.AbstractEventLoop,
            coroutine: Coroutine[Any, Any, Any],
            **kwargs: Any,
        ) -> asyncio.Task[Any]:
            return asyncio.Task(coroutine, loop=factory_loop, **kwargs)

        loop.set_task_factory(original_factory)
        runtime = _native()._Runtime()

        class StartFailure(troupe.Production):
            async def start(self) -> None:
                observations["start_factory"] = loop.get_task_factory()
                raise start_error

            async def scene(self) -> None:
                raise AssertionError("scene followed failed start")

            async def stop(self) -> None:
                observations["stop_called"] = True

        try:
            with pytest.raises(_native().ProductionFailed) as captured:
                await _run(runtime, StartFailure([]))
            failures = _phase_errors(captured.value)
            assert set(failures) == {"start"}
            assert failures["start"] is start_error
            observations["after_factory"] = loop.get_task_factory()
            observations["original_factory"] = original_factory
        finally:
            loop.set_task_factory(previous)

    asyncio.run(scenario())
    assert observations["start_factory"] is observations["original_factory"]
    assert observations["after_factory"] is observations["original_factory"]
    assert "stop_called" not in observations


@pytest.mark.parametrize(
    "root_outcome",
    ["normal", "cancelled", "ordinary"],
)
def test_replaced_task_factory_is_restored_with_defined_error_priority(
    root_outcome: str,
) -> None:
    class SceneBoom(Exception):
        pass

    scene_boom = SceneBoom("scene failed")
    original_context = ValueError("original context")
    original_cause = KeyError("original cause")
    scene_boom.__context__ = original_context
    scene_boom.__cause__ = original_cause
    observations: dict[str, Any] = {}
    actor_entered: asyncio.Event
    cleanup_entered: asyncio.Event
    cleanup_release: asyncio.Event
    stop_entered: asyncio.Event
    caller_errors: list[BaseException] = []

    async def scenario() -> None:
        nonlocal actor_entered, cleanup_entered, cleanup_release, stop_entered
        actor_entered = asyncio.Event()
        cleanup_entered = asyncio.Event()
        cleanup_release = asyncio.Event()
        stop_entered = asyncio.Event()
        loop = asyncio.get_running_loop()
        previous = loop.get_task_factory()

        def original_factory(
            factory_loop: asyncio.AbstractEventLoop,
            coroutine: Coroutine[Any, Any, Any],
            **kwargs: Any,
        ) -> asyncio.Task[Any]:
            return asyncio.Task(coroutine, loop=factory_loop, **kwargs)

        def replacement_factory(
            factory_loop: asyncio.AbstractEventLoop,
            coroutine: Coroutine[Any, Any, Any],
            **kwargs: Any,
        ) -> asyncio.Task[Any]:
            return asyncio.Task(coroutine, loop=factory_loop, **kwargs)

        loop.set_task_factory(original_factory)
        observations["original_factory"] = original_factory
        observations["replacement_factory"] = replacement_factory
        runtime = _native()._Runtime()

        class NeverActor(troupe.Actor):
            async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
                raise AssertionError("replacement-created Task reached Actor")

        class DrainActor(troupe.Actor):
            async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
                actor_entered.set()
                try:
                    await asyncio.Event().wait()
                except asyncio.CancelledError:
                    cleanup_entered.set()
                    observations["cleanup_factory"] = loop.get_task_factory()
                    await cleanup_release.wait()
                    raise

        class ReplacingProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, NeverActor, f"replacement-{root_outcome}")
                drain_handle = _cast(
                    self,
                    DrainActor,
                    f"replacement-drain-{root_outcome}",
                )

                async def consume() -> None:
                    try:
                        await drain_handle.cue({})
                    except BaseException as error:
                        caller_errors.append(error)

                asyncio.create_task(consume())
                await actor_entered.wait()
                observations["runtime_factory"] = loop.get_task_factory()
                assert observations["runtime_factory"] is not original_factory
                loop.set_task_factory(replacement_factory)

                async def replacement_child() -> None:
                    try:
                        handle.cue(object())  # type: ignore[arg-type]
                    except BaseException as error:
                        observations["replacement_child_error"] = error

                await asyncio.create_task(replacement_child())
                if root_outcome == "cancelled":
                    raise asyncio.CancelledError
                if root_outcome == "ordinary":
                    try:
                        raise scene_boom
                    except SceneBoom:
                        observations["root_traceback"] = scene_boom.__traceback__
                        raise

            async def stop(self) -> None:
                stop_entered.set()
                observations["stop_factory"] = loop.get_task_factory()

        run_task = asyncio.create_task(_run(runtime, ReplacingProduction([])))
        cleanup_waiter = asyncio.create_task(cleanup_entered.wait())
        try:
            done, _ = await asyncio.wait(
                {cleanup_waiter, run_task},
                timeout=TIMEOUT,
                return_when=asyncio.FIRST_COMPLETED,
            )
            assert cleanup_waiter in done, "run ended before cue cleanup began"
            await cleanup_waiter
            assert not stop_entered.is_set()
            assert not run_task.done()
            assert loop.get_task_factory() is observations["runtime_factory"]
            cleanup_release.set()
            with pytest.raises(_native().ProductionFailed) as captured:
                await asyncio.wait_for(run_task, TIMEOUT)
            observations["failures"] = _phase_errors(captured.value)
            observations["restored"] = loop.get_task_factory()
        finally:
            cleanup_release.set()
            if not run_task.done():
                run_task.cancel()
            with contextlib.suppress(BaseException):
                await run_task
            if not cleanup_waiter.done():
                cleanup_waiter.cancel()
            with contextlib.suppress(BaseException):
                await cleanup_waiter
            loop.set_task_factory(previous)

    asyncio.run(scenario())

    assert observations["cleanup_factory"] is observations["runtime_factory"]
    assert observations["stop_factory"] is observations["original_factory"]
    assert observations["restored"] is observations["original_factory"]
    assert len(caller_errors) == 1
    assert isinstance(caller_errors[0], asyncio.CancelledError)
    _assert_context_error(observations["replacement_child_error"])
    failures = observations["failures"]
    assert set(failures) == {"scene"}
    error = failures["scene"]
    if root_outcome == "ordinary":
        assert error is scene_boom
        assert error.__context__ is original_context
        assert error.__cause__ is original_cause
        assert error.__traceback__ is observations["root_traceback"]
        frame = traceback.extract_tb(error.__traceback__)[-1]
        assert frame.name == "scene"
    else:
        assert type(error) is RuntimeError
        assert str(error) == FACTORY_REPLACED_ERROR


def test_delegate_replacing_task_factory_is_detected_and_restored() -> None:
    observations: dict[str, Any] = {}

    async def scenario() -> None:
        loop = asyncio.get_running_loop()
        previous = loop.get_task_factory()
        armed = False

        def replacement_factory(
            factory_loop: asyncio.AbstractEventLoop,
            coroutine: Coroutine[Any, Any, Any],
            **kwargs: Any,
        ) -> asyncio.Task[Any]:
            return asyncio.Task(coroutine, loop=factory_loop, **kwargs)

        def original_factory(
            factory_loop: asyncio.AbstractEventLoop,
            coroutine: Coroutine[Any, Any, Any],
            **kwargs: Any,
        ) -> asyncio.Task[Any]:
            nonlocal armed
            if armed and factory_loop.get_task_factory() is not original_factory:
                armed = False
                factory_loop.set_task_factory(replacement_factory)
            return asyncio.Task(coroutine, loop=factory_loop, **kwargs)

        loop.set_task_factory(original_factory)
        observations["original_factory"] = original_factory
        runtime = _native()._Runtime()

        class DelegateProduction(troupe.Production):
            async def start(self) -> None:
                nonlocal armed
                assert loop.get_task_factory() is original_factory
                armed = True

            async def scene(self) -> None:
                raise AssertionError("replaced delegate allowed the scene to run")

            async def stop(self) -> None:
                observations["stop_factory"] = loop.get_task_factory()

        try:
            with pytest.raises(_native().ProductionFailed) as captured:
                await _run(runtime, DelegateProduction([]))
            observations["failures"] = _phase_errors(captured.value)
            observations["restored"] = loop.get_task_factory()
        finally:
            loop.set_task_factory(previous)

    asyncio.run(scenario())

    error = observations["failures"]["scene"]
    assert type(error) is RuntimeError
    assert str(error) == FACTORY_REPLACED_ERROR
    assert observations["stop_factory"] is observations["original_factory"]
    assert observations["restored"] is observations["original_factory"]


def test_callback_factory_replacement_after_root_terminal_is_detected() -> None:
    events: list[str] = []
    observations: dict[str, Any] = {}

    async def scenario() -> None:
        loop = asyncio.get_running_loop()
        previous = loop.get_task_factory()

        def original_factory(
            factory_loop: asyncio.AbstractEventLoop,
            coroutine: Coroutine[Any, Any, Any],
            **kwargs: Any,
        ) -> asyncio.Task[Any]:
            return asyncio.Task(coroutine, loop=factory_loop, **kwargs)

        def replacement_factory(
            factory_loop: asyncio.AbstractEventLoop,
            coroutine: Coroutine[Any, Any, Any],
            **kwargs: Any,
        ) -> asyncio.Task[Any]:
            return asyncio.Task(coroutine, loop=factory_loop, **kwargs)

        loop.set_task_factory(original_factory)
        runtime = _native()._Runtime()

        class CallbackReplacementProduction(troupe.Production):
            async def start(self) -> None:
                events.append("start")

            async def scene(self) -> None:
                events.append("scene")
                loop.call_soon(runtime.request_shutdown)
                loop.call_soon(loop.set_task_factory, replacement_factory)

            async def stop(self) -> None:
                events.append("stop")
                observations["stop_factory"] = loop.get_task_factory()

        try:
            with pytest.raises(_native().ProductionFailed) as captured:
                await _run(runtime, CallbackReplacementProduction([]))
            observations["failures"] = _phase_errors(captured.value)
            observations["restored"] = loop.get_task_factory()
        finally:
            loop.set_task_factory(previous)

    asyncio.run(scenario())

    assert events == ["start", "scene", "stop"]
    error = observations["failures"]["scene"]
    assert type(error) is RuntimeError
    assert str(error) == FACTORY_REPLACED_ERROR
    assert observations["stop_factory"] is observations["restored"]


def test_lazy_delegate_does_not_register_a_task_for_another_coroutine() -> None:
    errors: list[BaseException] = []

    async def scenario() -> None:
        loop = asyncio.get_running_loop()
        previous = loop.get_task_factory()
        armed = False
        target: troupe.ActorHandle

        async def substitute() -> None:
            try:
                unexpected = target.cue({})
            except BaseException as error:
                errors.append(error)
            else:
                unexpected.close()
                errors.append(AssertionError("unrelated Task received Scene lineage"))

        def mismatched_factory(
            factory_loop: asyncio.AbstractEventLoop,
            coroutine: Coroutine[Any, Any, Any],
            **kwargs: Any,
        ) -> asyncio.Task[Any]:
            nonlocal armed
            if armed:
                armed = False
                coroutine.close()
                return asyncio.Task(substitute(), loop=factory_loop)
            return asyncio.Task(coroutine, loop=factory_loop, **kwargs)

        loop.set_task_factory(mismatched_factory)
        runtime = _native()._Runtime()

        class TargetActor(troupe.Actor):
            pass

        class LazyMismatchProduction(troupe.Production):
            async def scene(self) -> None:
                nonlocal armed, target
                target = _cast(self, TargetActor, "lazy-mismatch")

                async def replaced() -> None:
                    raise AssertionError("the replaced coroutine ran")

                armed = True
                await asyncio.create_task(replaced())
                runtime.request_shutdown()

        try:
            await _run(runtime, LazyMismatchProduction([]))
        finally:
            loop.set_task_factory(previous)

    asyncio.run(scenario())
    assert len(errors) == 1
    _assert_context_error(errors[0])


@pytest.mark.skipif(
    not hasattr(asyncio, "eager_task_factory"),
    reason="the official eager task factory starts in Python 3.12",
)
def test_eager_factory_preserves_registered_lineage_without_permit_theft() -> None:
    successes: list[str] = []
    errors: list[BaseException] = []

    async def scenario() -> None:
        loop = asyncio.get_running_loop()
        previous = loop.get_task_factory()
        steal_on_delegate = False
        direct_tasks: list[asyncio.Task[Any]] = []
        steal_target: troupe.ActorHandle

        async def direct_probe() -> None:
            try:
                steal_target.cue(object())  # type: ignore[arg-type]
            except BaseException as error:
                errors.append(error)

        def eager_delegate(
            factory_loop: asyncio.AbstractEventLoop,
            coroutine: Coroutine[Any, Any, Any],
            **kwargs: Any,
        ) -> asyncio.Task[Any]:
            nonlocal steal_on_delegate
            if steal_on_delegate:
                steal_on_delegate = False
                direct_tasks.append(
                    asyncio.Task(
                        direct_probe(),
                        loop=factory_loop,
                        eager_start=True,
                    )
                )
            return asyncio.eager_task_factory(  # type: ignore[attr-defined]
                factory_loop,
                coroutine,
                **kwargs,
            )

        loop.set_task_factory(eager_delegate)
        runtime = _native()._Runtime()

        class EagerActor(troupe.Actor):
            async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
                successes.append(cue.instruction["label"])

                async def grandchild() -> None:
                    assert await leaf.cue({"label": "grandchild"}) == ()

                await asyncio.create_task(grandchild())
                return ()

        class LeafActor(troupe.Actor):
            async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
                successes.append(cue.instruction["label"])
                return ()

        target: troupe.ActorHandle
        leaf: troupe.ActorHandle

        class EagerProduction(troupe.Production):
            async def scene(self) -> None:
                nonlocal leaf, steal_on_delegate, steal_target, target
                target = _cast(self, EagerActor, "eager-parent")
                leaf = _cast(self, LeafActor, "eager-leaf")
                steal_target = leaf

                async def child() -> None:
                    assert await target.cue({"label": "child"}) == ()

                steal_on_delegate = True
                await asyncio.create_task(child())
                await asyncio.gather(*direct_tasks)
                runtime.request_shutdown()

        try:
            await _run(runtime, EagerProduction([]))
        finally:
            loop.set_task_factory(previous)

    asyncio.run(scenario())
    assert successes == ["child", "grandchild"]
    assert len(errors) == 1
    _assert_context_error(errors[0])


@pytest.mark.skipif(
    not hasattr(asyncio, "eager_task_factory"),
    reason="the official eager task factory starts in Python 3.12",
)
@pytest.mark.parametrize("delegate_outcome", ["different-task", "error"])
def test_eager_delegate_revokes_lineage_after_delegate_exit(
    delegate_outcome: str,
) -> None:
    errors: list[BaseException] = []
    delegate_error = RuntimeError("eager delegate failed")

    async def scenario() -> None:
        loop = asyncio.get_running_loop()
        previous = loop.get_task_factory()
        armed = False
        release = asyncio.Event()
        actual_tasks: list[asyncio.Task[Any]] = []
        target: troupe.ActorHandle

        async def substitute() -> None:
            return None

        def eager_mismatch_factory(
            factory_loop: asyncio.AbstractEventLoop,
            coroutine: Coroutine[Any, Any, Any],
            **kwargs: Any,
        ) -> asyncio.Task[Any]:
            nonlocal armed
            if armed:
                armed = False
                actual_tasks.append(
                    asyncio.Task(
                        coroutine,
                        loop=factory_loop,
                        eager_start=True,
                        **kwargs,
                    )
                )
                if delegate_outcome == "error":
                    raise delegate_error
                return asyncio.Task(substitute(), loop=factory_loop)
            return asyncio.eager_task_factory(  # type: ignore[attr-defined]
                factory_loop,
                coroutine,
                **kwargs,
            )

        loop.set_task_factory(eager_mismatch_factory)
        runtime = _native()._Runtime()

        class TargetActor(troupe.Actor):
            pass

        class EagerMismatchProduction(troupe.Production):
            async def scene(self) -> None:
                nonlocal armed, target
                target = _cast(self, TargetActor, "eager-mismatch")

                async def escaped() -> None:
                    probe = target.cue({})
                    probe.close()
                    await release.wait()
                    try:
                        unexpected = target.cue({})
                    except BaseException as error:
                        errors.append(error)
                    else:
                        unexpected.close()
                        errors.append(AssertionError("eager Task retained Scene lineage"))

                armed = True
                try:
                    returned = asyncio.create_task(escaped())
                except RuntimeError as error:
                    assert delegate_outcome == "error"
                    assert error is delegate_error
                else:
                    assert delegate_outcome == "different-task"
                    await returned
                assert len(actual_tasks) == 1
                release.set()
                await actual_tasks[0]
                runtime.request_shutdown()

        try:
            await _run(runtime, EagerMismatchProduction([]))
        finally:
            release.set()
            await asyncio.gather(*actual_tasks, return_exceptions=True)
            loop.set_task_factory(previous)

    asyncio.run(scenario())
    assert len(errors) == 1
    _assert_context_error(errors[0])


def test_first_drive_rejects_unregistered_task_and_other_loop_without_ids() -> None:
    errors: dict[str, BaseException] = {}
    cues: list[troupe.Cue] = []
    runtime = _native()._Runtime()

    class RecordingActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            cues.append(cue)
            return ()

    async def scenario() -> None:
        class TransferProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, RecordingActor, "transfer")
                loop = asyncio.get_running_loop()

                async def consume(label: str, runner: Any) -> None:
                    try:
                        await runner
                    except BaseException as error:
                        errors[label] = error

                direct_runner = handle.cue({"label": "direct"})
                await asyncio.Task(
                    consume("direct", direct_runner),
                    loop=loop,
                )

                other_loop_runner = handle.cue({"label": "other-loop"})

                def run_other_loop() -> None:
                    asyncio.run(consume("other-loop", other_loop_runner))

                await asyncio.to_thread(run_other_loop)
                assert await handle.cue({"label": "valid"}) == ()
                runtime.request_shutdown()

        await _run(runtime, TransferProduction([]))

    asyncio.run(scenario())
    _assert_context_error(errors["direct"])
    _assert_context_error(errors["other-loop"])
    assert len(cues) == 1
    assert cues[0].instruction["label"] == "valid"
    assert cues[0].id.endswith("-cue0")


def test_runner_first_driven_after_scene_close_is_rejected_without_admission() -> None:
    actor_refs: list[weakref.ReferenceType[troupe.Actor]] = []
    errors: list[BaseException] = []
    kept_runners: list[Any] = []
    cues: list[troupe.Cue] = []
    survivor_done: asyncio.Event
    runtime = _native()._Runtime()

    class RecordingActor(troupe.Actor):
        def __init__(self) -> None:
            actor_refs.append(weakref.ref(self))

        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            cues.append(cue)
            return ()

    class ClosingProduction(troupe.Production):
        def __init__(self, args: list[str]) -> None:
            self.scene_count = 0

        async def scene(self) -> None:
            nonlocal survivor_done
            self.scene_count += 1
            if self.scene_count == 1:
                gate = asyncio.Event()
                survivor_done = asyncio.Event()
                handle = _cast(self, RecordingActor, "closing-transfer")
                runner = handle.cue({"label": "closed"})
                kept_runners.append(runner)
                del handle

                async def survivor() -> None:
                    await gate.wait()
                    try:
                        await runner
                    except BaseException as error:
                        errors.append(error)
                    finally:
                        survivor_done.set()

                asyncio.create_task(survivor())
                asyncio.get_running_loop().call_soon(gate.set)
                return

            await survivor_done.wait()
            await _loop_barrier()
            gc.collect()
            assert actor_refs[0]() is None
            assert self.get_actor("closing-transfer") is None
            handle = _cast(self, RecordingActor, "closing-transfer")
            assert await handle.cue({"label": "valid"}) == ()
            runtime.request_shutdown()

    asyncio.run(_run(runtime, ClosingProduction([])))
    assert len(errors) == 1
    _assert_context_error(errors[0])
    assert len(kept_runners) == 1
    assert len(cues) == 1
    assert cues[0].instruction["label"] == "valid"
    assert cues[0].id.endswith("-cue0")


def test_actor_source_is_a_value_snapshot_and_does_not_retain_sender() -> None:
    sender_refs: list[weakref.ReferenceType[troupe.Actor]] = []
    cues: list[troupe.Cue] = []
    observations: dict[str, Any] = {}
    runtime = _native()._Runtime()
    downstream: troupe.ActorHandle

    class StatefulName(str):
        pass

    class DownstreamActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            cues.append(cue)
            return ()

    class SenderActor(troupe.Actor):
        def __init__(self) -> None:
            sender_refs.append(weakref.ref(self))

        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            assert await downstream.cue({"label": "downstream"}) == ()
            return ()

    async def scenario() -> None:
        nonlocal downstream

        class SourceProduction(troupe.Production):
            async def scene(self) -> None:
                nonlocal downstream
                downstream = _cast(self, DownstreamActor, "source-target")
                name = StatefulName("sender-\ud800-stateful")
                sender = _cast(self, SenderActor, name)
                actor = sender_refs[-1]()
                assert actor is not None
                name.owner = actor  # type: ignore[attr-defined]
                assert await sender.cue({}) == ()

                del actor, sender, name
                await _loop_barrier()
                gc.collect()
                observations["dead"] = sender_refs[-1]() is None
                observations["missing"] = (
                    self.get_actor("sender-\ud800-stateful") is None
                )
                observations["reused"] = _cast(
                    self,
                    SenderActor,
                    "sender-\ud800-stateful",
                ).name
                runtime.request_shutdown()

        await _run(runtime, SourceProduction([]))

    asyncio.run(scenario())
    assert observations == {
        "dead": True,
        "missing": True,
        "reused": "sender-\ud800-stateful",
    }
    assert len(cues) == 1
    assert type(cues[0].source) is str
    assert cues[0].source == "sender-\ud800-stateful"


@pytest.mark.parametrize("failed_phase", ["start", "scene", "stop"])
def test_production_rebinds_after_each_lifecycle_failure(
    failed_phase: str,
) -> None:
    events: list[str] = []
    factories: dict[str, Any] = {}
    first_error = RuntimeError(f"{failed_phase} failed")

    class FailingProduction(troupe.Production):
        def __init__(self, args: list[str]) -> None:
            self.fail = True
            self.runtime: Any = None

        async def start(self) -> None:
            events.append("start")
            if self.fail:
                factories["start"] = asyncio.get_running_loop().get_task_factory()
            if self.fail and failed_phase == "start":
                raise first_error

        async def scene(self) -> None:
            events.append("scene")
            if self.fail:
                factories["scene"] = asyncio.get_running_loop().get_task_factory()
            if self.fail and failed_phase == "scene":
                raise first_error
            self.runtime.request_shutdown()

        async def stop(self) -> None:
            events.append("stop")
            if self.fail:
                factories["stop"] = asyncio.get_running_loop().get_task_factory()
            if self.fail and failed_phase == "stop":
                raise first_error

    production = FailingProduction([])

    async def first_run() -> None:
        loop = asyncio.get_running_loop()
        previous_factory = loop.get_task_factory()

        def entry_factory(
            factory_loop: asyncio.AbstractEventLoop,
            coroutine: Coroutine[Any, Any, Any],
            **kwargs: Any,
        ) -> asyncio.Task[Any]:
            return asyncio.Task(coroutine, loop=factory_loop, **kwargs)

        loop.set_task_factory(entry_factory)
        factories["entry"] = entry_factory
        runtime = _native()._Runtime()
        production.runtime = runtime
        try:
            with pytest.raises(_native().ProductionFailed) as captured:
                await _run(runtime, production)
            failures = _phase_errors(captured.value)
            assert set(failures) == {failed_phase}
            assert failures[failed_phase] is first_error
            factories["after"] = loop.get_task_factory()
        finally:
            loop.set_task_factory(previous_factory)

    asyncio.run(first_run())
    assert factories["start"] is factories["entry"]
    assert factories["after"] is factories["entry"]
    if failed_phase == "start":
        assert "scene" not in factories
        assert "stop" not in factories
    else:
        assert factories["scene"] is not factories["entry"]
        assert factories["stop"] is factories["entry"]

    async def second_run() -> None:
        runtime = _native()._Runtime()
        production.runtime = runtime
        production.fail = False
        runtime.request_shutdown()
        await _run(runtime, production)

    asyncio.run(second_run())
    assert events.count("start") == 2
    assert events.count("stop") == (1 if failed_phase == "start" else 2)


def test_make_effect_context_precedes_arguments_across_lifecycle_and_actor_identity() -> None:
    actors: dict[str, troupe.Actor] = {}
    errors: dict[str, BaseException] = {}
    effects: list[troupe.Effect] = []
    constructor_calls = 0
    runtime = _native()._Runtime()

    class ProbeEffect(troupe.Effect):
        def __init__(self) -> None:
            nonlocal constructor_calls
            constructor_calls += 1

    def invalid_call(actor: troupe.Actor, label: str) -> None:
        try:
            actor.make_effect(
                object,
                effect_args=[],  # type: ignore[arg-type]
                effect_kwargs=[],  # type: ignore[arg-type]
            )
        except BaseException as error:
            errors[label] = error
        else:
            raise AssertionError(f"invalid context {label} succeeded")

    class ContextActor(troupe.Actor):
        def __init__(self) -> None:
            actors[self.name] = self

        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            invalid_call(actors["other"], "other-actor")
            effect = self.make_effect(
                ProbeEffect,
                effect_args=(),
                effect_kwargs={},
            )
            effects.append(effect)
            return (effect,)

    class ContextProduction(troupe.Production):
        async def start(self) -> None:
            invalid_call(actors["target"], "start")

        async def scene(self) -> None:
            invalid_call(actors["target"], "scene")
            result = await target.cue({})
            assert [effect.id for effect in result] == [effect.id for effect in effects]
            runtime.request_shutdown()

        async def stop(self) -> None:
            invalid_call(actors["target"], "stop")

    production = ContextProduction([])
    target = _cast(production, ContextActor, "target")
    other = _cast(production, ContextActor, "other")
    invalid_call(actors["target"], "outside-before")
    asyncio.run(_run(runtime, production))
    invalid_call(actors["target"], "outside-after")

    assert other.name == "other"
    for label in (
        "outside-before",
        "start",
        "scene",
        "other-actor",
        "stop",
        "outside-after",
    ):
        _assert_effect_context_error(errors[label])
    assert constructor_calls == 1
    assert len(effects) == 1
    assert effects[0].id.endswith("-cue0-effect0")
    assert effects[0].owner == "target"


def test_make_effect_lineage_is_registered_shared_ordered_and_expires() -> None:
    errors: dict[str, BaseException] = {}
    effects: list[troupe.Effect] = []
    expired_tasks: list[asyncio.Task[None]] = []
    expired_gate: asyncio.Event
    runtime = _native()._Runtime()

    class OrderedEffect(troupe.Effect):
        def __init__(self, label: str) -> None:
            self.label = label

    class LineageActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            if cue.instruction["case"] == "expire":

                async def expired_child() -> None:
                    await expired_gate.wait()
                    try:
                        self.make_effect(
                            OrderedEffect,
                            effect_args=("expired",),
                            effect_kwargs={},
                        )
                    except BaseException as error:
                        errors["expired"] = error

                expired_tasks.append(asyncio.create_task(expired_child()))
                return ()

            async def invalid_probe(label: str) -> None:
                try:
                    self.make_effect(
                        object,
                        effect_args=[],  # type: ignore[arg-type]
                        effect_kwargs=[],  # type: ignore[arg-type]
                    )
                except BaseException as error:
                    errors[label] = error

            loop = asyncio.get_running_loop()
            await asyncio.Task(invalid_probe("unregistered"), loop=loop)

            def thread_probe() -> None:
                try:
                    self.make_effect(
                        object,
                        effect_args=[],  # type: ignore[arg-type]
                        effect_kwargs=[],  # type: ignore[arg-type]
                    )
                except BaseException as error:
                    errors["thread"] = error

            await asyncio.to_thread(thread_probe)
            first_gate = asyncio.Event()
            second_gate = asyncio.Event()

            async def child(label: str, gate: asyncio.Event) -> None:
                await gate.wait()
                effects.append(
                    self.make_effect(
                        OrderedEffect,
                        effect_args=(label,),
                        effect_kwargs={},
                    )
                )

            first_child = asyncio.create_task(child("first-child", first_gate))
            second_child = asyncio.create_task(child("second-child", second_gate))
            effects.append(
                self.make_effect(
                    OrderedEffect,
                    effect_args=("root",),
                    effect_kwargs={},
                )
            )
            first_gate.set()
            await first_child
            second_gate.set()
            await second_child
            return tuple(effects)

    async def scenario() -> None:
        nonlocal expired_gate
        expired_gate = asyncio.Event()

        class LineageProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, LineageActor, "lineage")
                result = await handle.cue({"case": "ordered"})
                assert [effect.label for effect in result] == [
                    "root",
                    "first-child",
                    "second-child",
                ]
                assert await handle.cue({"case": "expire"}) == ()
                expired_gate.set()
                await expired_tasks[0]
                runtime.request_shutdown()

        await _run(runtime, LineageProduction([]))

    asyncio.run(scenario())
    _assert_effect_context_error(errors["unregistered"])
    _assert_effect_context_error(errors["thread"])
    _assert_effect_context_error(errors["expired"])
    prefix = effects[0].id.rsplit("-effect", 1)[0]
    assert [effect.id for effect in effects] == [
        f"{prefix}-effect0",
        f"{prefix}-effect1",
        f"{prefix}-effect2",
    ]
    assert [effect.label for effect in effects] == [
        "root",
        "first-child",
        "second-child",
    ]


def test_cued_lineage_expires_after_failure_and_completed_cancel_cleanup() -> None:
    boom = RuntimeError("cued failed")
    constructor_calls = 0
    effect_errors: dict[str, BaseException] = {}
    cue_errors: dict[str, BaseException] = {}
    gates: dict[str, asyncio.Event] = {}
    stale_tasks: dict[str, asyncio.Task[None]] = {}
    downstream_cues: list[troupe.Cue] = []
    cancel_entered: asyncio.Event
    downstream: troupe.ActorHandle
    runtime = _native()._Runtime()

    class StaleEffect(troupe.Effect):
        def __init__(self) -> None:
            nonlocal constructor_calls
            constructor_calls += 1

    class TerminalActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            mode = cue.instruction["mode"]

            async def stale_child() -> None:
                await gates[mode].wait()
                try:
                    self.make_effect(
                        StaleEffect,
                        effect_args=(),
                        effect_kwargs={},
                    )
                except BaseException as error:
                    effect_errors[mode] = error
                try:
                    runner = downstream.cue({"mode": mode})
                except BaseException as error:
                    cue_errors[mode] = error
                else:
                    runner.close()
                    cue_errors[mode] = AssertionError(
                        "stale cued descendant admitted a downstream cue"
                    )

            stale_tasks[mode] = asyncio.create_task(stale_child())
            if mode == "failure":
                raise boom
            cancel_entered.set()
            await asyncio.Event().wait()
            return ()

    class DownstreamActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            downstream_cues.append(cue)
            return ()

    async def scenario() -> None:
        nonlocal cancel_entered
        gates["failure"] = asyncio.Event()
        gates["cancel"] = asyncio.Event()
        cancel_entered = asyncio.Event()

        class TerminalProduction(troupe.Production):
            async def scene(self) -> None:
                nonlocal downstream
                downstream = _cast(self, DownstreamActor, "terminal-downstream")
                handle = _cast(self, TerminalActor, "terminal-lineage")
                with pytest.raises(RuntimeError) as caught:
                    await handle.cue({"mode": "failure"})
                assert caught.value is boom
                gates["failure"].set()
                await stale_tasks["failure"]

                caller = asyncio.create_task(handle.cue({"mode": "cancel"}))
                await cancel_entered.wait()
                assert caller.cancel()
                with pytest.raises(asyncio.CancelledError):
                    await caller
                gates["cancel"].set()
                await stale_tasks["cancel"]
                assert await downstream.cue({"mode": "valid"}) == ()
                runtime.request_shutdown()

        await _run(runtime, TerminalProduction([]))

    asyncio.run(scenario())
    for mode in ("failure", "cancel"):
        _assert_effect_context_error(effect_errors[mode])
        _assert_context_error(cue_errors[mode])
    assert constructor_calls == 0
    assert len(downstream_cues) == 1
    assert downstream_cues[0].instruction["mode"] == "valid"
    assert downstream_cues[0].id.endswith("-cue2")


def test_make_effect_normalizes_lineage_lookup_errors_without_consuming_index() -> None:
    lookup_boom = RuntimeError("current task lookup failed")
    original_current_task = asyncio.current_task
    lookup_raises = False
    errors: list[BaseException] = []
    effects: list[troupe.Effect] = []
    runtime = _native()._Runtime()

    def controlled_current_task(*args: object, **kwargs: object) -> object:
        if lookup_raises:
            raise lookup_boom
        return original_current_task(*args, **kwargs)

    class LookupActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            nonlocal lookup_raises
            lookup_raises = True
            try:
                self.make_effect(
                    troupe.Effect,
                    effect_args=(),
                    effect_kwargs={},
                )
            except BaseException as error:
                errors.append(error)
            finally:
                lookup_raises = False

            effect = self.make_effect(
                troupe.Effect,
                effect_args=(),
                effect_kwargs={},
            )
            effects.append(effect)
            return (effect,)

    async def scenario() -> None:
        class LookupProduction(troupe.Production):
            async def scene(self) -> None:
                await _cast(self, LookupActor, "lookup").cue({})
                runtime.request_shutdown()

        asyncio.current_task = controlled_current_task  # type: ignore[assignment]
        try:
            await _run(runtime, LookupProduction([]))
        finally:
            asyncio.current_task = original_current_task

    asyncio.run(scenario())
    assert len(errors) == 1
    assert errors[0] is not lookup_boom
    _assert_effect_context_error(errors[0])
    assert len(effects) == 1
    assert effects[0].id.endswith("-effect0")


def test_make_effect_rejects_mutable_task_loop_and_thread_authority_spoofs() -> None:
    errors: dict[str, BaseException] = {}
    forged: list[troupe.Effect] = []
    effects: list[troupe.Effect] = []
    runtime = _native()._Runtime()

    class AuthorityActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            root_task = asyncio.current_task()
            assert root_task is not None
            root_loop = asyncio.get_running_loop()
            root_thread = threading.get_ident()

            async def direct_task_attack() -> None:
                original = asyncio.current_task
                asyncio.current_task = (  # type: ignore[assignment]
                    lambda *args, **kwargs: root_task
                )
                try:
                    forged.append(
                        self.make_effect(
                            troupe.Effect,
                            effect_args=(),
                            effect_kwargs={},
                        )
                    )
                except BaseException as error:
                    errors["direct-task"] = error
                finally:
                    asyncio.current_task = original

            await asyncio.Task(direct_task_attack(), loop=root_loop)

            original_running_loop = asyncio.get_running_loop
            asyncio.get_running_loop = lambda: root_loop  # type: ignore[assignment]
            try:
                forged.append(
                    self.make_effect(
                        troupe.Effect,
                        effect_args=(),
                        effect_kwargs={},
                    )
                )
            except BaseException as error:
                errors["loop-lookup"] = error
            finally:
                asyncio.get_running_loop = original_running_loop

            def thread_attack() -> None:
                original_current_task = asyncio.current_task
                original_get_running_loop = asyncio.get_running_loop
                original_get_ident = threading.get_ident
                asyncio.current_task = (  # type: ignore[assignment]
                    lambda *args, **kwargs: root_task
                )
                asyncio.get_running_loop = lambda: root_loop  # type: ignore[assignment]
                threading.get_ident = lambda: root_thread  # type: ignore[assignment]
                try:
                    forged.append(
                        self.make_effect(
                            troupe.Effect,
                            effect_args=(),
                            effect_kwargs={},
                        )
                    )
                except BaseException as error:
                    errors["thread"] = error
                finally:
                    threading.get_ident = original_get_ident
                    asyncio.get_running_loop = original_get_running_loop
                    asyncio.current_task = original_current_task

            await asyncio.to_thread(thread_attack)
            effect = self.make_effect(
                troupe.Effect,
                effect_args=(),
                effect_kwargs={},
            )
            effects.append(effect)
            return (effect,)

    async def scenario() -> None:
        class AuthorityProduction(troupe.Production):
            async def scene(self) -> None:
                await _cast(self, AuthorityActor, "authority").cue({})
                runtime.request_shutdown()

        await _run(runtime, AuthorityProduction([]))

    asyncio.run(scenario())
    assert forged == []
    for label in ("direct-task", "loop-lookup", "thread"):
        _assert_effect_context_error(errors[label])
    assert len(effects) == 1
    assert effects[0].id.endswith("-effect0")


def test_make_effect_uses_capability_identity_across_same_name_reincarnation() -> None:
    actors: list[troupe.Actor] = []
    errors: list[BaseException] = []
    effects: list[troupe.Effect] = []
    handles: dict[str, troupe.ActorHandle] = {}
    runtime = _native()._Runtime()

    class ReusedActor(troupe.Actor):
        def __init__(self) -> None:
            actors.append(self)

        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            try:
                actors[0].make_effect(
                    troupe.Effect,
                    effect_args=(),
                    effect_kwargs={},
                )
            except BaseException as error:
                errors.append(error)
            effect = self.make_effect(
                troupe.Effect,
                effect_args=(),
                effect_kwargs={},
            )
            effects.append(effect)
            return (effect,)

    async def scenario() -> None:
        class ReuseProduction(troupe.Production):
            async def scene(self) -> None:
                handles.pop("old")
                await _loop_barrier()
                gc.collect()
                assert self.get_actor("same-name") is None
                replacement = _cast(self, ReusedActor, "same-name")
                result = await replacement.cue({})
                assert [effect.id for effect in result] == [
                    effect.id for effect in effects
                ]
                runtime.request_shutdown()

        production = ReuseProduction([])
        handles["old"] = _cast(production, ReusedActor, "same-name")
        await _run(runtime, production)

    asyncio.run(scenario())
    assert len(actors) == 2
    assert len(errors) == 1
    _assert_effect_context_error(errors[0])
    assert len(effects) == 1
    assert effects[0].id.endswith("-effect0")
    assert effects[0].owner == "same-name"


@pytest.mark.skipif(not hasattr(os, "fork"), reason="Linux fork is required")
def test_make_effect_rejects_fork_child_before_inherited_runtime_state() -> None:
    payloads: list[str] = []
    effects: list[troupe.Effect] = []
    runtime = _native()._Runtime()

    class ForkActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            read_fd, write_fd = os.pipe()
            pid = os.fork()
            if pid == 0:
                os.close(read_fd)
                try:
                    try:
                        self.make_effect(
                            object,
                            effect_args=[],  # type: ignore[arg-type]
                            effect_kwargs=[],  # type: ignore[arg-type]
                        )
                    except BaseException as error:
                        payload = f"{type(error).__name__}|{error}"
                    else:
                        payload = "NO_ERROR"
                    os.write(write_fd, payload.encode("utf-8"))
                finally:
                    os.close(write_fd)
                    os._exit(0)

            os.close(write_fd)
            try:
                data = await asyncio.to_thread(os.read, read_fd, 4096)
                _, status = await asyncio.to_thread(os.waitpid, pid, 0)
            finally:
                os.close(read_fd)
            assert os.waitstatus_to_exitcode(status) == 0
            payloads.append(data.decode("utf-8"))
            effect = self.make_effect(
                troupe.Effect,
                effect_args=(),
                effect_kwargs={},
            )
            effects.append(effect)
            return (effect,)

    async def scenario() -> None:
        class ForkProduction(troupe.Production):
            async def scene(self) -> None:
                await _cast(self, ForkActor, "fork-effect").cue({})
                runtime.request_shutdown()

        await _run(runtime, ForkProduction([]))

    asyncio.run(scenario())
    assert payloads == [f"EffectContextError|{EFFECT_CONTEXT_ERROR}"]
    assert len(effects) == 1
    assert effects[0].id.endswith("-effect0")
