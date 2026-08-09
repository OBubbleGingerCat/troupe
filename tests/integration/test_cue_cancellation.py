from __future__ import annotations

import asyncio
import gc
import importlib
import sys
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
REUSE_ERROR = "cannot reuse already awaited coroutine"


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


async def _wait(event: asyncio.Event) -> None:
    await asyncio.wait_for(event.wait(), TIMEOUT)


async def _loop_barrier() -> None:
    reached = asyncio.Event()
    asyncio.get_running_loop().call_soon(reached.set)
    await _wait(reached)


def _assert_reuse(error: BaseException) -> None:
    assert type(error) is RuntimeError
    assert str(error) == REUSE_ERROR


def test_task_cancelled_before_first_poll_has_no_admission_and_reuses_never() -> None:
    actor_refs: list[weakref.ReferenceType[troupe.Actor]] = []
    seen: list[troupe.Cue] = []
    errors: list[BaseException] = []
    runtime = _native()._Runtime()

    class CreatedActor(troupe.Actor):
        def __init__(self) -> None:
            actor_refs.append(weakref.ref(self))

        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            seen.append(cue)
            return ()

    async def scenario() -> None:
        class CreatedProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, CreatedActor, "created-cancel")
                runner = handle.cue({"never": "copied"})
                caller = asyncio.create_task(runner)
                assert caller.cancel()
                with pytest.raises(asyncio.CancelledError):
                    await caller
                try:
                    await runner
                except BaseException as error:
                    errors.append(error)

                del caller, runner, handle
                await _loop_barrier()
                gc.collect()
                assert actor_refs[0]() is None
                assert self.get_actor("created-cancel") is None

                replacement = _cast(self, CreatedActor, "created-cancel")
                assert await replacement.cue({"used": True}) == ()
                runtime.request_shutdown()

        await _run(runtime, CreatedProduction([]))

    asyncio.run(scenario())
    assert len(errors) == 1
    _assert_reuse(errors[0])
    assert len(seen) == 1
    assert seen[0].id.endswith("-cue0")
    assert dict(seen[0].instruction) == {"used": True}


def test_running_cancel_targets_exact_task_once_and_waits_cleanup_and_slot() -> None:
    runtime = _native()._Runtime()
    scene_active = False
    actor_refs: list[weakref.ReferenceType[troupe.Actor]] = []
    actor_entered: asyncio.Event
    cleanup_entered: asyncio.Event
    cleanup_release: asyncio.Event
    successor_entered: asyncio.Event
    inner_tasks: list[asyncio.Task[Any]] = []
    cancel_calls: list[tuple[asyncio.Task[Any], object | None]] = []
    caught_cancellations: list[asyncio.CancelledError] = []
    exact_cancel_snapshots: list[bool] = []
    caught_cancel_counts: list[int] = []
    cue_ids: list[str] = []
    reuse_errors: list[BaseException] = []

    class RecordingTask(asyncio.Task[Any]):
        def cancel(self, msg: object | None = None) -> bool:
            if self in inner_tasks:
                cancel_calls.append((self, msg))
            return super().cancel(msg)

    class ExactActor(troupe.Actor):
        def __init__(self) -> None:
            actor_refs.append(weakref.ref(self))

        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            cue_ids.append(cue.id)
            if cue.instruction["label"] == "running":
                task = asyncio.current_task()
                assert task is not None
                inner_tasks.append(task)
                actor_entered.set()
                try:
                    await asyncio.Event().wait()
                except asyncio.CancelledError as error:
                    caught_cancellations.append(error)
                    cleanup_entered.set()
                    await cleanup_release.wait()
                    raise
            successor_entered.set()
            return ()

    async def scenario() -> None:
        nonlocal scene_active, actor_entered, cleanup_entered
        nonlocal cleanup_release, successor_entered
        loop = asyncio.get_running_loop()
        previous_factory = loop.get_task_factory()

        def task_factory(
            factory_loop: asyncio.AbstractEventLoop,
            coroutine: Coroutine[Any, Any, Any],
            **kwargs: Any,
        ) -> asyncio.Task[Any]:
            if scene_active and type(coroutine).__name__ == "_ScopeDriver":
                return RecordingTask(coroutine, loop=factory_loop, **kwargs)
            return asyncio.Task(coroutine, loop=factory_loop, **kwargs)

        loop.set_task_factory(task_factory)
        actor_entered = asyncio.Event()
        cleanup_entered = asyncio.Event()
        cleanup_release = asyncio.Event()
        successor_entered = asyncio.Event()

        class ExactProduction(troupe.Production):
            async def scene(self) -> None:
                nonlocal scene_active
                scene_active = True
                try:
                    handle = _cast(self, ExactActor, "exact-cancel")
                    runner = handle.cue({"label": "running"})
                    caller = asyncio.create_task(runner)
                    await _wait(actor_entered)
                    assert len(inner_tasks) == 1
                    del handle
                    await _loop_barrier()
                    gc.collect()
                    assert actor_refs[0]() is not None
                    assert caller.cancel("caller-stop")
                    await _wait(cleanup_entered)
                    successor_handle = self.get_actor("exact-cancel")
                    assert successor_handle is not None
                    successor = asyncio.create_task(
                        successor_handle.cue({"label": "successor"})
                    )
                    try:
                        await _loop_barrier()
                        assert not caller.done()
                        assert not successor.done()
                        assert not successor_entered.is_set()
                        assert cancel_calls == [(inner_tasks[0], None)]
                    finally:
                        cleanup_release.set()

                    with pytest.raises(asyncio.CancelledError):
                        await caller
                    assert await successor == ()
                    try:
                        await runner
                    except BaseException as error:
                        reuse_errors.append(error)
                    exact_cancel_snapshots.append(
                        cancel_calls == [(inner_tasks[0], None)]
                    )
                    caught_cancel_counts.append(len(caught_cancellations))
                    cancel_calls.clear()
                    inner_tasks.clear()
                    caught_cancellations.clear()
                    del caller, runner, successor, successor_handle
                    await _loop_barrier()
                    gc.collect()
                    assert actor_refs[0]() is None
                    assert self.get_actor("exact-cancel") is None
                    replacement = _cast(self, ExactActor, "exact-cancel")
                    assert replacement is not None
                    runtime.request_shutdown()
                finally:
                    cleanup_release.set()
                    scene_active = False

        try:
            await _run(runtime, ExactProduction([]))
        finally:
            cleanup_release.set()
            loop.set_task_factory(previous_factory)

    asyncio.run(scenario())
    assert exact_cancel_snapshots == [True]
    assert caught_cancel_counts == [1]
    assert len(reuse_errors) == 1
    _assert_reuse(reuse_errors[0])
    assert actor_refs[0]() is None
    prefix = cue_ids[0].rsplit("-cue", 1)[0]
    assert cue_ids == [f"{prefix}-cue0", f"{prefix}-cue1"]


@pytest.mark.parametrize("result_kind", ["bare-effect", "bad-item"])
def test_cancel_winner_discards_every_normal_return_without_validation(
    result_kind: str,
) -> None:
    runtime = _native()._Runtime()
    actor_entered: asyncio.Event
    cleanup_entered: asyncio.Event
    cleanup_release: asyncio.Event
    cue_ids: list[str] = []

    class ReturnActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            cue_ids.append(cue.id)
            if cue.instruction["label"] == "after":
                return ()
            actor_entered.set()
            try:
                await asyncio.Event().wait()
            except asyncio.CancelledError:
                cleanup_entered.set()
                await cleanup_release.wait()
                if result_kind == "valid":
                    return ()
                if result_kind == "bare-effect":
                    return self.make_effect(  # type: ignore[return-value]
                        troupe.Effect,
                        effect_args=(),
                        effect_kwargs={},
                    )
                return (object(),)  # type: ignore[return-value]

    async def scenario() -> None:
        nonlocal actor_entered, cleanup_entered, cleanup_release
        actor_entered = asyncio.Event()
        cleanup_entered = asyncio.Event()
        cleanup_release = asyncio.Event()

        class ReturnProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, ReturnActor, f"return-{result_kind}")
                caller = asyncio.create_task(handle.cue({"label": "cancel"}))
                await _wait(actor_entered)
                assert caller.cancel()
                await _wait(cleanup_entered)
                try:
                    assert not caller.done()
                finally:
                    cleanup_release.set()
                with pytest.raises(asyncio.CancelledError):
                    await caller
                assert await handle.cue({"label": "after"}) == ()
                runtime.request_shutdown()

        try:
            await _run(runtime, ReturnProduction([]))
        finally:
            cleanup_release.set()

    asyncio.run(scenario())
    prefix = cue_ids[0].rsplit("-cue", 1)[0]
    assert cue_ids == [f"{prefix}-cue0", f"{prefix}-cue1"]


def test_cancel_cleanup_failure_preserves_identity_traceback_and_context() -> None:
    runtime = _native()._Runtime()
    cleanup_error = RuntimeError("cleanup failed")
    actor_entered: asyncio.Event
    cleanup_entered: asyncio.Event
    cleanup_release: asyncio.Event
    caught: list[asyncio.CancelledError] = []
    observed: list[BaseException] = []

    class CleanupActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            if cue.instruction["label"] == "after":
                return ()
            actor_entered.set()
            try:
                await asyncio.Event().wait()
            except asyncio.CancelledError as error:
                caught.append(error)
                cleanup_entered.set()
                await cleanup_release.wait()
                raise cleanup_error

    async def scenario() -> None:
        nonlocal actor_entered, cleanup_entered, cleanup_release
        actor_entered = asyncio.Event()
        cleanup_entered = asyncio.Event()
        cleanup_release = asyncio.Event()

        class CleanupProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, CleanupActor, "cleanup-failure")
                caller = asyncio.create_task(handle.cue({"label": "cancel"}))
                await _wait(actor_entered)
                assert caller.cancel()
                await _wait(cleanup_entered)
                try:
                    assert not caller.done()
                finally:
                    cleanup_release.set()
                try:
                    await caller
                except BaseException as error:
                    observed.append(error)
                assert await handle.cue({"label": "after"}) == ()
                runtime.request_shutdown()

        try:
            await _run(runtime, CleanupProduction([]))
        finally:
            cleanup_release.set()

    asyncio.run(scenario())
    assert observed == [cleanup_error]
    assert len(caught) == 1
    assert cleanup_error.__context__ is caught[0]
    assert cleanup_error.__cause__ is None
    frame = traceback.extract_tb(cleanup_error.__traceback__)[-1]
    assert frame.name == "cued"
    assert frame.filename.endswith("test_cue_cancellation.py")


def test_unrequested_self_cancellation_stays_structured_and_actor_continues() -> None:
    runtime = _native()._Runtime()

    class SelfCancelled(asyncio.CancelledError):
        pass

    direct_error = SelfCancelled("direct self cancellation")
    scene_active = False
    current_label: dict[int, str] = {}
    cancel_labels: list[str] = []
    task_cancellations: list[asyncio.CancelledError] = []
    cue_ids: list[str] = []

    class RecordingTask(asyncio.Task[Any]):
        def cancel(self, msg: object | None = None) -> bool:
            label = current_label.get(id(self))
            if label is not None:
                cancel_labels.append(label)
            return super().cancel(msg)

    class SelfCancelActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            label = cue.instruction["label"]
            cue_ids.append(cue.id)
            task = asyncio.current_task()
            assert task is not None
            current_label[id(task)] = label
            if label == "direct":
                raise direct_error
            if label == "task":
                task.cancel()
                try:
                    await asyncio.Event().wait()
                except asyncio.CancelledError as error:
                    task_cancellations.append(error)
                    raise
            if label == "swallow":
                task.cancel()
                try:
                    await asyncio.Event().wait()
                except asyncio.CancelledError:
                    return ()
            return ()

    async def scenario() -> None:
        nonlocal scene_active
        loop = asyncio.get_running_loop()
        previous_factory = loop.get_task_factory()

        def task_factory(
            factory_loop: asyncio.AbstractEventLoop,
            coroutine: Coroutine[Any, Any, Any],
            **kwargs: Any,
        ) -> asyncio.Task[Any]:
            if scene_active and type(coroutine).__name__ == "_ScopeDriver":
                return RecordingTask(coroutine, loop=factory_loop, **kwargs)
            return asyncio.Task(coroutine, loop=factory_loop, **kwargs)

        loop.set_task_factory(task_factory)

        class SelfCancelProduction(troupe.Production):
            async def scene(self) -> None:
                nonlocal scene_active
                scene_active = True
                try:
                    handle = _cast(self, SelfCancelActor, "self-cancel")
                    direct_seen: BaseException | None = None
                    try:
                        await handle.cue({"label": "direct"})
                    except BaseException as error:
                        direct_seen = error
                    assert isinstance(direct_seen, asyncio.CancelledError)

                    runner = handle.cue({"label": "task"})
                    outer = asyncio.create_task(runner)
                    outer_seen: BaseException | None = None
                    try:
                        await outer
                    except BaseException as error:
                        outer_seen = error
                    assert isinstance(outer_seen, asyncio.CancelledError)
                    if sys.version_info[:2] == (3, 10):
                        assert len(task_cancellations) == 1
                        assert outer_seen is not task_cancellations[0]
                        assert all(
                            frame.name != "cued"
                            for frame in traceback.extract_tb(
                                outer_seen.__traceback__
                            )
                        )

                    assert await handle.cue({"label": "swallow"}) == ()
                    assert await handle.cue({"label": "after"}) == ()
                    runtime.request_shutdown()
                finally:
                    scene_active = False

        try:
            await _run(runtime, SelfCancelProduction([]))
        finally:
            loop.set_task_factory(previous_factory)

    asyncio.run(scenario())
    assert cancel_labels == ["task", "swallow"]
    prefix = cue_ids[0].rsplit("-cue", 1)[0]
    assert cue_ids == [f"{prefix}-cue{index}" for index in range(4)]


def test_swallowed_cancel_has_no_grace_and_other_actor_still_progresses() -> None:
    runtime = _native()._Runtime()
    cleanup_entered: asyncio.Event
    cleanup_release: asyncio.Event
    successor_entered: asyncio.Event
    other_entered: asyncio.Event

    class BlockingActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            if cue.instruction["label"] == "blocked":
                try:
                    await asyncio.Event().wait()
                except asyncio.CancelledError:
                    cleanup_entered.set()
                    await cleanup_release.wait()
                    return ()
            successor_entered.set()
            return ()

    class OtherActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            other_entered.set()
            return ()

    async def scenario() -> None:
        nonlocal cleanup_entered, cleanup_release, successor_entered, other_entered
        cleanup_entered = asyncio.Event()
        cleanup_release = asyncio.Event()
        successor_entered = asyncio.Event()
        other_entered = asyncio.Event()

        class NoGraceProduction(troupe.Production):
            async def scene(self) -> None:
                blocked = _cast(self, BlockingActor, "no-grace-blocked")
                other = _cast(self, OtherActor, "no-grace-other")
                caller = asyncio.create_task(blocked.cue({"label": "blocked"}))
                await _loop_barrier()
                assert caller.cancel()
                await _wait(cleanup_entered)
                successor = asyncio.create_task(
                    blocked.cue({"label": "successor"})
                )
                try:
                    assert await other.cue({}) == ()
                    await _wait(other_entered)
                    await _loop_barrier()
                    assert not caller.done()
                    assert not successor.done()
                    assert not successor_entered.is_set()
                finally:
                    cleanup_release.set()
                with pytest.raises(asyncio.CancelledError):
                    await caller
                assert await successor == ()
                runtime.request_shutdown()

        try:
            await _run(runtime, NoGraceProduction([]))
        finally:
            cleanup_release.set()

    asyncio.run(scenario())


@pytest.mark.parametrize("outcome", ["success", "error"])
def test_completion_commit_before_caller_cancel_keeps_committed_outcome(
    outcome: str,
) -> None:
    runtime = _native()._Runtime()
    committed_error = RuntimeError("committed failure")
    actor_entered: asyncio.Event
    actor_release: asyncio.Event
    inner_task: asyncio.Task[Any] | None = None
    observed: list[Any] = []

    class CommitActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            nonlocal inner_task
            inner_task = asyncio.current_task()
            assert inner_task is not None
            actor_entered.set()
            await actor_release.wait()
            if outcome == "error":
                raise committed_error
            return ()

    async def scenario() -> None:
        nonlocal actor_entered, actor_release
        actor_entered = asyncio.Event()
        actor_release = asyncio.Event()

        class CommitProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, CommitActor, f"commit-{outcome}")
                caller = asyncio.create_task(handle.cue({}))
                await _wait(actor_entered)
                assert inner_task is not None
                cancel_called = asyncio.Event()

                def cancel_after_runtime_observer(_: asyncio.Future[Any]) -> None:
                    caller.cancel()
                    cancel_called.set()

                inner_task.add_done_callback(cancel_after_runtime_observer)
                actor_release.set()
                await _wait(cancel_called)
                try:
                    observed.append(await caller)
                except BaseException as error:
                    observed.append(error)
                runtime.request_shutdown()

        try:
            await _run(runtime, CommitProduction([]))
        finally:
            actor_release.set()

    asyncio.run(scenario())
    if outcome == "success":
        assert observed == [()]
    else:
        assert observed == [committed_error]


def test_cancel_commits_before_pending_done_observer_for_already_done_task() -> None:
    runtime = _native()._Runtime()
    scene_active = False
    actor_entered: asyncio.Event
    actor_release: asyncio.Event
    inner_task: asyncio.Task[Any] | None = None
    cancel_observations: list[tuple[bool, object | None]] = []

    class DoneTask(asyncio.Task[Any]):
        def cancel(self, msg: object | None = None) -> bool:
            if self is inner_task:
                cancel_observations.append((self.done(), msg))
            return super().cancel(msg)

    class CancelFirstActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            nonlocal inner_task
            if cue.instruction["label"] == "after":
                return ()
            inner_task = asyncio.current_task()
            assert inner_task is not None
            actor_entered.set()
            await actor_release.wait()
            return None  # type: ignore[return-value]

    async def scenario() -> None:
        nonlocal scene_active, actor_entered, actor_release
        loop = asyncio.get_running_loop()
        previous_factory = loop.get_task_factory()

        def task_factory(
            factory_loop: asyncio.AbstractEventLoop,
            coroutine: Coroutine[Any, Any, Any],
            **kwargs: Any,
        ) -> asyncio.Task[Any]:
            if scene_active and type(coroutine).__name__ == "_ScopeDriver":
                return DoneTask(coroutine, loop=factory_loop, **kwargs)
            return asyncio.Task(coroutine, loop=factory_loop, **kwargs)

        loop.set_task_factory(task_factory)
        actor_entered = asyncio.Event()
        actor_release = asyncio.Event()

        class CancelFirstProduction(troupe.Production):
            async def scene(self) -> None:
                nonlocal scene_active
                scene_active = True
                try:
                    handle = _cast(self, CancelFirstActor, "cancel-first")
                    caller = asyncio.create_task(handle.cue({"label": "target"}))
                    await _wait(actor_entered)

                    # The cued Task wakeup is queued before the caller's cancel step.
                    actor_release.set()
                    assert caller.cancel("cancel-first")
                    with pytest.raises(asyncio.CancelledError):
                        await caller
                    assert await handle.cue({"label": "after"}) == ()
                    runtime.request_shutdown()
                finally:
                    actor_release.set()
                    scene_active = False

        try:
            await _run(runtime, CancelFirstProduction([]))
        finally:
            actor_release.set()
            loop.set_task_factory(previous_factory)

    asyncio.run(scenario())
    assert cancel_observations == [(True, None)]


def test_second_caller_cancel_after_cleanup_shield_does_not_finish_early() -> None:
    runtime = _native()._Runtime()
    scene_active = False
    actor_entered: asyncio.Event
    cleanup_entered: asyncio.Event
    cleanup_release: asyncio.Event
    first_scene_returned: asyncio.Event
    second_scene_entered: asyncio.Event
    inner_task: asyncio.Task[Any] | None = None
    cancel_calls: list[asyncio.Task[Any]] = []
    early_done: list[bool] = []
    second_scene_snapshots: list[bool] = []
    cancel_snapshots: list[list[asyncio.Task[Any]]] = []
    observed: list[asyncio.CancelledError] = []

    class RecordingTask(asyncio.Task[Any]):
        def cancel(self, msg: object | None = None) -> bool:
            if self is inner_task:
                cancel_calls.append(self)
            return super().cancel(msg)

    class ShieldActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            nonlocal inner_task
            inner_task = asyncio.current_task()
            actor_entered.set()
            try:
                await asyncio.Event().wait()
            except asyncio.CancelledError:
                cleanup_entered.set()
                await cleanup_release.wait()
                raise

    async def scenario() -> None:
        nonlocal scene_active, actor_entered, cleanup_entered, cleanup_release
        nonlocal first_scene_returned, second_scene_entered
        loop = asyncio.get_running_loop()
        previous_factory = loop.get_task_factory()

        def task_factory(
            factory_loop: asyncio.AbstractEventLoop,
            coroutine: Coroutine[Any, Any, Any],
            **kwargs: Any,
        ) -> asyncio.Task[Any]:
            if scene_active and type(coroutine).__name__ == "_ScopeDriver":
                return RecordingTask(coroutine, loop=factory_loop, **kwargs)
            return asyncio.Task(coroutine, loop=factory_loop, **kwargs)

        loop.set_task_factory(task_factory)
        actor_entered = asyncio.Event()
        cleanup_entered = asyncio.Event()
        cleanup_release = asyncio.Event()
        first_scene_returned = asyncio.Event()
        second_scene_entered = asyncio.Event()

        class ShieldProduction(troupe.Production):
            def __init__(self, args: list[str]) -> None:
                self.scene_count = 0
                self.caller: asyncio.Task[tuple[troupe.Effect, ...]] | None = None

            async def scene(self) -> None:
                nonlocal scene_active
                self.scene_count += 1
                if self.scene_count == 2:
                    second_scene_entered.set()
                    runtime.request_shutdown()
                    return

                scene_active = True
                try:
                    handle = _cast(self, ShieldActor, "repeat-cancel")
                    self.caller = asyncio.create_task(handle.cue({}))
                    await _wait(actor_entered)
                    assert self.caller.cancel("first")
                    await _wait(cleanup_entered)
                    await _loop_barrier()
                    first_scene_returned.set()
                finally:
                    scene_active = False

        production = ShieldProduction([])
        run_task = asyncio.create_task(_run(runtime, production))
        caller: asyncio.Task[tuple[troupe.Effect, ...]] | None = None
        try:
            await _wait(first_scene_returned)
            await _loop_barrier()
            caller = production.caller
            assert caller is not None
            assert cancel_calls == [inner_task]
            caller.cancel("second")
            await _loop_barrier()
            early_done.append(caller.done())
            second_scene_snapshots.append(second_scene_entered.is_set())
            cancel_snapshots.append(list(cancel_calls))
        finally:
            cleanup_release.set()

        assert caller is not None
        try:
            await caller
        except asyncio.CancelledError as error:
            observed.append(error)
        else:
            raise AssertionError("caller-first cancellation unexpectedly succeeded")
        try:
            await run_task
        finally:
            loop.set_task_factory(previous_factory)

    asyncio.run(scenario())
    assert early_done == [False]
    assert second_scene_snapshots == [False]
    assert cancel_snapshots == [[inner_task]]
    assert cancel_calls == [inner_task]
    assert len(observed) == 1
    assert isinstance(observed[0], asyncio.CancelledError)


def test_scene_cancel_then_repeated_caller_cancel_uses_same_cleanup_signal() -> None:
    runtime = _native()._Runtime()
    scene_active = False
    actor_entered: asyncio.Event
    cleanup_entered: asyncio.Event
    cleanup_release: asyncio.Event
    second_scene_entered: asyncio.Event
    inner_task: asyncio.Task[Any] | None = None
    cancel_calls: list[asyncio.Task[Any]] = []
    early_done: list[bool] = []
    second_scene_snapshots: list[bool] = []
    cancel_snapshots: list[list[asyncio.Task[Any]]] = []
    observed: list[asyncio.CancelledError] = []

    class RecordingTask(asyncio.Task[Any]):
        def cancel(self, msg: object | None = None) -> bool:
            if self is inner_task:
                cancel_calls.append(self)
            return super().cancel(msg)

    class SceneFirstActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            nonlocal inner_task
            inner_task = asyncio.current_task()
            actor_entered.set()
            try:
                await asyncio.Event().wait()
            except asyncio.CancelledError:
                cleanup_entered.set()
                await cleanup_release.wait()
                raise

    async def scenario() -> None:
        nonlocal scene_active, actor_entered, cleanup_entered
        nonlocal cleanup_release, second_scene_entered
        loop = asyncio.get_running_loop()
        previous_factory = loop.get_task_factory()

        def task_factory(
            factory_loop: asyncio.AbstractEventLoop,
            coroutine: Coroutine[Any, Any, Any],
            **kwargs: Any,
        ) -> asyncio.Task[Any]:
            if scene_active and type(coroutine).__name__ == "_ScopeDriver":
                return RecordingTask(coroutine, loop=factory_loop, **kwargs)
            return asyncio.Task(coroutine, loop=factory_loop, **kwargs)

        loop.set_task_factory(task_factory)
        actor_entered = asyncio.Event()
        cleanup_entered = asyncio.Event()
        cleanup_release = asyncio.Event()
        second_scene_entered = asyncio.Event()

        class SceneFirstProduction(troupe.Production):
            def __init__(self, args: list[str]) -> None:
                self.scene_count = 0
                self.caller: asyncio.Task[tuple[troupe.Effect, ...]] | None = None

            async def scene(self) -> None:
                nonlocal scene_active
                self.scene_count += 1
                if self.scene_count == 2:
                    second_scene_entered.set()
                    runtime.request_shutdown()
                    return
                scene_active = True
                handle = _cast(self, SceneFirstActor, "scene-first")
                self.caller = asyncio.create_task(handle.cue({}))
                await _wait(actor_entered)
                scene_active = False

        production = SceneFirstProduction([])
        run_task = asyncio.create_task(_run(runtime, production))
        caller: asyncio.Task[tuple[troupe.Effect, ...]] | None = None
        try:
            await _wait(cleanup_entered)
            caller = production.caller
            assert caller is not None
            assert cancel_calls == [inner_task]
            assert caller.cancel("first-caller")
            await _loop_barrier()
            caller.cancel("second-caller")
            await _loop_barrier()
            early_done.append(caller.done())
            second_scene_snapshots.append(second_scene_entered.is_set())
            cancel_snapshots.append(list(cancel_calls))
        finally:
            cleanup_release.set()

        assert caller is not None
        try:
            await caller
        except asyncio.CancelledError as error:
            observed.append(error)
        else:
            raise AssertionError("scene-first cancellation unexpectedly succeeded")
        try:
            await run_task
        finally:
            loop.set_task_factory(previous_factory)

    asyncio.run(scenario())
    assert early_done == [False]
    assert second_scene_snapshots == [False]
    assert cancel_snapshots == [[inner_task]]
    assert cancel_calls == [inner_task]
    assert len(observed) == 1
    assert isinstance(observed[0], asyncio.CancelledError)


@pytest.mark.parametrize("task_outcome", ["normal", "error", "cancelled"])
@pytest.mark.parametrize("dispatch_kind", ["ordinary", "cancelled"])
def test_task_cancel_dispatch_failure_waits_and_uses_exact_priority(
    task_outcome: str,
    dispatch_kind: str,
) -> None:
    runtime = _native()._Runtime()
    dispatch_error: BaseException
    if dispatch_kind == "ordinary":
        dispatch_error = RuntimeError("cancel dispatch failed")
    else:
        dispatch_error = asyncio.CancelledError("cancel dispatch cancelled")
    task_error = RuntimeError("task cleanup failed")
    scene_active = False
    fail_cancel = False
    actor_entered: asyncio.Event
    actor_release: asyncio.Event
    inner_task: asyncio.Task[Any] | None = None
    cancel_calls: list[asyncio.Task[Any]] = []
    observed: list[BaseException] = []
    loop_errors: list[dict[str, Any]] = []

    class DispatchTask(asyncio.Task[Any]):
        def cancel(self, msg: object | None = None) -> bool:
            if self is inner_task and fail_cancel:
                cancel_calls.append(self)
                raise dispatch_error
            return super().cancel(msg)

    class DispatchActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            nonlocal inner_task
            if cue.instruction["label"] == "after":
                return ()
            inner_task = asyncio.current_task()
            actor_entered.set()
            await actor_release.wait()
            if task_outcome == "error":
                raise task_error
            if task_outcome == "cancelled":
                raise asyncio.CancelledError("task ended cancelled")
            return None  # type: ignore[return-value]

    async def scenario() -> None:
        nonlocal scene_active, fail_cancel, actor_entered, actor_release
        loop = asyncio.get_running_loop()
        previous_factory = loop.get_task_factory()
        previous_handler = loop.get_exception_handler()

        def task_factory(
            factory_loop: asyncio.AbstractEventLoop,
            coroutine: Coroutine[Any, Any, Any],
            **kwargs: Any,
        ) -> asyncio.Task[Any]:
            if scene_active and type(coroutine).__name__ == "_ScopeDriver":
                return DispatchTask(coroutine, loop=factory_loop, **kwargs)
            return asyncio.Task(coroutine, loop=factory_loop, **kwargs)

        loop.set_task_factory(task_factory)
        loop.set_exception_handler(lambda _loop, context: loop_errors.append(context))
        actor_entered = asyncio.Event()
        actor_release = asyncio.Event()

        class DispatchProduction(troupe.Production):
            async def scene(self) -> None:
                nonlocal scene_active, fail_cancel
                scene_active = True
                try:
                    handle = _cast(
                        self,
                        DispatchActor,
                        f"dispatch-{dispatch_kind}-{task_outcome}",
                    )
                    caller = asyncio.create_task(handle.cue({"label": "target"}))
                    await _wait(actor_entered)
                    fail_cancel = True
                    assert caller.cancel()
                    try:
                        await _loop_barrier()
                        assert not caller.done()
                    finally:
                        actor_release.set()
                    try:
                        await caller
                    except BaseException as error:
                        observed.append(error)
                    fail_cancel = False
                    assert await handle.cue({"label": "after"}) == ()
                    runtime.request_shutdown()
                finally:
                    fail_cancel = False
                    actor_release.set()
                    scene_active = False

        try:
            await _run(runtime, DispatchProduction([]))
            await _loop_barrier()
            gc.collect()
            await _loop_barrier()
        finally:
            fail_cancel = False
            actor_release.set()
            loop.set_task_factory(previous_factory)
            loop.set_exception_handler(previous_handler)

    asyncio.run(scenario())
    if task_outcome == "error":
        assert observed == [task_error]
    elif dispatch_kind == "ordinary":
        assert observed == [dispatch_error]
    else:
        assert len(observed) == 1
        assert isinstance(observed[0], asyncio.CancelledError)
    assert cancel_calls == [inner_task]
    if task_outcome == "error":
        frame = traceback.extract_tb(task_error.__traceback__)[-1]
        assert frame.name == "cued"
        assert task_error.__context__ is None
        assert task_error.__cause__ is None
    elif dispatch_kind == "ordinary":
        frame = traceback.extract_tb(dispatch_error.__traceback__)[-1]
        assert frame.name == "cancel"
        assert dispatch_error.__context__ is None
        assert dispatch_error.__cause__ is None
    assert loop_errors == []


def test_caller_cancel_cleanup_can_cue_another_actor_while_scene_open() -> None:
    runtime = _native()._Runtime()
    actor_entered: asyncio.Event
    downstream_seen: list[troupe.Cue] = []
    root_seen: list[troupe.Cue] = []
    downstream: troupe.ActorHandle

    class DownstreamActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            downstream_seen.append(cue)
            return ()

    class SourceActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            root_seen.append(cue)
            actor_entered.set()
            try:
                await asyncio.Event().wait()
            except asyncio.CancelledError:
                assert await downstream.cue({"label": "cleanup"}) == ()
                raise

    async def scenario() -> None:
        nonlocal actor_entered, downstream
        actor_entered = asyncio.Event()

        class DownstreamProduction(troupe.Production):
            async def scene(self) -> None:
                nonlocal downstream
                downstream = _cast(self, DownstreamActor, "cleanup-target")
                source = _cast(self, SourceActor, "cleanup-source")
                caller = asyncio.create_task(source.cue({"label": "root"}))
                await _wait(actor_entered)
                assert caller.cancel()
                with pytest.raises(asyncio.CancelledError):
                    await caller
                runtime.request_shutdown()

        await _run(runtime, DownstreamProduction([]))

    asyncio.run(scenario())
    assert len(root_seen) == len(downstream_seen) == 1
    root = root_seen[0]
    child = downstream_seen[0]
    assert root.id == f"{root.source}-cue0"
    assert child.id == f"{root.source}-cue1"
    assert child.source == "cleanup-source"


@pytest.mark.parametrize("outcome", ["success", "failure"])
def test_terminal_outcome_and_traceback_cycles_are_collectable(outcome: str) -> None:
    runtime = _native()._Runtime()
    references: list[weakref.ReferenceType[Any]] = []
    collected: list[bool] = []

    class Marker:
        pass

    class CycleError(RuntimeError):
        pass

    class CycleEffect(troupe.Effect):
        pass

    marker_for_actor: Marker | None = None
    error_for_actor: CycleError | None = None

    class CycleActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            assert marker_for_actor is not None
            if outcome == "failure":
                assert error_for_actor is not None
                raise error_for_actor
            effect = self.make_effect(
                CycleEffect,
                effect_args=(),
                effect_kwargs={},
            )
            effect.marker = marker_for_actor
            return (effect,)

    async def scenario() -> None:
        nonlocal marker_for_actor, error_for_actor

        class CycleProduction(troupe.Production):
            async def scene(self) -> None:
                nonlocal marker_for_actor, error_for_actor
                handle = _cast(self, CycleActor, f"terminal-cycle-{outcome}")
                marker = Marker()
                marker_for_actor = marker
                runner = handle.cue({})
                marker.runner = runner
                references.append(weakref.ref(marker))

                if outcome == "failure":
                    error = CycleError("terminal cycle")
                    error_for_actor = error
                    error.marker = marker
                    error.runner = runner
                    references.append(weakref.ref(error))
                    try:
                        await runner
                    except CycleError as caught:
                        assert caught is error
                    else:
                        raise AssertionError("cycle failure unexpectedly succeeded")
                    del error
                else:
                    result = await runner
                    assert len(result) == 1
                    assert result[0].marker is marker
                    del result

                marker_for_actor = None
                error_for_actor = None
                del handle, marker, runner
                await _loop_barrier()
                gc.collect()
                await _loop_barrier()
                gc.collect()
                collected.append(all(reference() is None for reference in references))
                runtime.request_shutdown()

        await _run(runtime, CycleProduction([]))

    asyncio.run(scenario())
    assert collected == [True]


def test_repeated_cleanup_shield_callback_cycle_is_collectable() -> None:
    runtime = _native()._Runtime()
    actor_entered: asyncio.Event
    cleanup_entered: asyncio.Event
    cleanup_release: asyncio.Event
    reference: weakref.ReferenceType[Any]
    collected: list[bool] = []

    class Marker:
        pass

    class ShieldCycleActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            if cue.instruction["label"] == "after":
                return ()
            assert cue.instruction["marker"] is not None
            actor_entered.set()
            try:
                await asyncio.Event().wait()
            except asyncio.CancelledError:
                cleanup_entered.set()
                await cleanup_release.wait()
                raise

    async def scenario() -> None:
        nonlocal actor_entered, cleanup_entered, cleanup_release, reference
        actor_entered = asyncio.Event()
        cleanup_entered = asyncio.Event()
        cleanup_release = asyncio.Event()

        class ShieldCycleProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, ShieldCycleActor, "shield-cycle")
                marker = Marker()
                instruction: dict[str, Any] = {"label": "target", "marker": marker}
                runner = handle.cue(instruction)
                marker.runner = runner
                reference = weakref.ref(marker)
                caller = asyncio.create_task(runner)
                await _wait(actor_entered)
                assert caller.cancel("first")
                await _wait(cleanup_entered)
                await _loop_barrier()
                caller.cancel("second")
                await _loop_barrier()
                cleanup_release.set()
                try:
                    await caller
                except asyncio.CancelledError:
                    pass
                assert await handle.cue({"label": "after"}) == ()

                del caller, handle, instruction, marker, runner
                await _loop_barrier()
                gc.collect()
                await _loop_barrier()
                gc.collect()
                collected.append(reference() is None)
                runtime.request_shutdown()

        try:
            await _run(runtime, ShieldCycleProduction([]))
        finally:
            cleanup_release.set()

    asyncio.run(scenario())
    assert collected == [True]
