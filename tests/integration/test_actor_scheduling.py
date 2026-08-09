from __future__ import annotations

import asyncio
import gc
import importlib
import os
import threading
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


async def _loop_barrier() -> None:
    reached = asyncio.Event()
    asyncio.get_running_loop().call_soon(reached.set)
    await reached.wait()


def test_same_actor_fifo_follows_admission_not_caller_task_creation() -> None:
    creation_order: list[int] = []
    execution_log: list[str] = []
    cue_ids: dict[int, str] = {}
    runtime = _native()._Runtime()

    admission_gates: dict[int, asyncio.Event] = {}
    entered: dict[int, asyncio.Event] = {}
    releases: dict[int, asyncio.Event] = {}

    class TaggedEffect(troupe.Effect):
        def __init__(self, label: int) -> None:
            self.label = label

    class OrderedActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            label = cue.instruction["label"]
            cue_ids[label] = cue.id
            execution_log.append(f"start-{label}")
            entered[label].set()
            await releases[label].wait()
            execution_log.append(f"end-{label}")
            effect = self.make_effect(
                TaggedEffect,
                effect_args=(label,),
                effect_kwargs={},
            )
            return (effect,)

    async def scenario() -> None:
        for label in range(3):
            admission_gates[label] = asyncio.Event()
            entered[label] = asyncio.Event()
            releases[label] = asyncio.Event()

        class OrderedProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, OrderedActor, "ordered")

                async def invoke(label: int) -> tuple[troupe.Effect, ...]:
                    await admission_gates[label].wait()
                    return await handle.cue({"label": label})

                callers: dict[int, asyncio.Task[tuple[troupe.Effect, ...]]] = {}
                for label in (0, 1, 2):
                    creation_order.append(label)
                    callers[label] = asyncio.create_task(invoke(label))

                admission_gates[2].set()
                await entered[2].wait()

                admission_gates[0].set()
                await _loop_barrier()
                admission_gates[1].set()
                await _loop_barrier()
                assert not entered[0].is_set()
                assert not entered[1].is_set()

                releases[2].set()
                await entered[0].wait()
                assert not entered[1].is_set()
                releases[0].set()
                await entered[1].wait()
                releases[1].set()

                results = await asyncio.gather(
                    callers[0],
                    callers[1],
                    callers[2],
                )
                assert [result[0].label for result in results] == [0, 1, 2]
                runtime.request_shutdown()

        await _run(runtime, OrderedProduction([]))

    asyncio.run(scenario())
    assert creation_order == [0, 1, 2]
    assert execution_log == [
        "start-2",
        "end-2",
        "start-0",
        "end-0",
        "start-1",
        "end-1",
    ]
    prefix = cue_ids[2].rsplit("-cue", 1)[0]
    assert cue_ids == {
        2: f"{prefix}-cue0",
        0: f"{prefix}-cue1",
        1: f"{prefix}-cue2",
    }


def test_cancelled_queued_request_is_removed_without_reordering_or_id_reuse() -> None:
    runtime = _native()._Runtime()
    actor_refs: list[weakref.ReferenceType[troupe.Actor]] = []
    execution_log: list[str] = []
    cue_ids: dict[int, str] = {}
    results: list[object] = []
    cancelled_before_release: list[bool] = []
    third_started_before_release: list[bool] = []

    first_entered: asyncio.Event
    first_release: asyncio.Event
    third_entered: asyncio.Event

    class QueueActor(troupe.Actor):
        def __init__(self) -> None:
            actor_refs.append(weakref.ref(self))

        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            label = cue.instruction["label"]
            cue_ids[label] = cue.id
            execution_log.append(f"start-{label}")
            if label == 0:
                first_entered.set()
                await first_release.wait()
            elif label == 2:
                third_entered.set()
            execution_log.append(f"end-{label}")
            return ()

    async def scenario() -> None:
        nonlocal first_entered, first_release, third_entered
        first_entered = asyncio.Event()
        first_release = asyncio.Event()
        third_entered = asyncio.Event()

        class QueueProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, QueueActor, "cancelled-middle")
                second_handle = self.get_actor("cancelled-middle")
                third_handle = self.get_actor("cancelled-middle")
                assert second_handle is not None
                assert third_handle is not None

                first = asyncio.create_task(handle.cue({"label": 0}))
                await first_entered.wait()
                second = asyncio.create_task(second_handle.cue({"label": 1}))
                await _loop_barrier()
                third = asyncio.create_task(third_handle.cue({"label": 2}))
                await _loop_barrier()

                del handle, second_handle, third_handle
                gc.collect()
                assert actor_refs[0]() is not None

                assert second.cancel("remove-queued")
                with pytest.raises(asyncio.CancelledError):
                    await asyncio.wait_for(asyncio.shield(second), TIMEOUT)
                cancelled_before_release.append(second.done())
                third_started_before_release.append(third_entered.is_set())

                first_release.set()
                results.extend(
                    await asyncio.gather(first, second, third, return_exceptions=True)
                )
                await _loop_barrier()
                gc.collect()
                assert actor_refs[0]() is None
                assert self.get_actor("cancelled-middle") is None
                replacement = _cast(self, QueueActor, "cancelled-middle")
                assert replacement is not None
                runtime.request_shutdown()

        try:
            await _run(runtime, QueueProduction([]))
        finally:
            first_release.set()

    asyncio.run(scenario())
    assert cancelled_before_release == [True]
    assert third_started_before_release == [False]
    assert results[0] == ()
    assert isinstance(results[1], asyncio.CancelledError)
    assert results[2] == ()
    assert execution_log == ["start-0", "end-0", "start-2", "end-2"]
    prefix = cue_ids[0].rsplit("-cue", 1)[0]
    assert cue_ids == {0: f"{prefix}-cue0", 2: f"{prefix}-cue2"}
    assert actor_refs[0]() is None


def test_different_actors_cooperate_on_one_loop_thread() -> None:
    runtime = _native()._Runtime()
    scene_active = False
    cued_task_types: list[str] = []
    locations: dict[str, tuple[int, int, int]] = {}
    results: dict[str, tuple[troupe.Effect, ...]] = {}

    entered: dict[str, asyncio.Event] = {}
    releases: dict[str, asyncio.Event] = {}

    class TaggedEffect(troupe.Effect):
        def __init__(self, label: str) -> None:
            self.label = label

    class CooperativeActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            label = cue.instruction["label"]
            locations[label] = (
                os.getpid(),
                threading.get_ident(),
                id(asyncio.get_running_loop()),
            )
            entered[label].set()
            await releases[label].wait()
            effect = self.make_effect(
                TaggedEffect,
                effect_args=(label,),
                effect_kwargs={},
            )
            return (effect,)

    async def scenario() -> None:
        nonlocal scene_active
        loop = asyncio.get_running_loop()
        previous_factory = loop.get_task_factory()

        def recording_factory(
            factory_loop: asyncio.AbstractEventLoop,
            coroutine: Coroutine[Any, Any, Any],
            **kwargs: Any,
        ) -> asyncio.Task[Any]:
            if scene_active and type(coroutine).__name__ == "_ScopeDriver":
                cued_task_types.append(type(coroutine).__name__)
            return asyncio.Task(coroutine, loop=factory_loop, **kwargs)

        loop.set_task_factory(recording_factory)
        entered["a"] = asyncio.Event()
        entered["b"] = asyncio.Event()
        releases["a"] = asyncio.Event()
        releases["b"] = asyncio.Event()

        class CooperativeProduction(troupe.Production):
            async def scene(self) -> None:
                nonlocal scene_active
                scene_active = True
                try:
                    locations["scene"] = (
                        os.getpid(),
                        threading.get_ident(),
                        id(asyncio.get_running_loop()),
                    )
                    first = _cast(self, CooperativeActor, "cooperative-a")
                    second = _cast(self, CooperativeActor, "cooperative-b")
                    first_call = asyncio.create_task(first.cue({"label": "a"}))
                    second_call = asyncio.create_task(second.cue({"label": "b"}))

                    await asyncio.gather(entered["a"].wait(), entered["b"].wait())
                    releases["b"].set()
                    results["b"] = await second_call
                    assert not first_call.done()
                    releases["a"].set()
                    results["a"] = await first_call
                    runtime.request_shutdown()
                finally:
                    scene_active = False

        try:
            await _run(runtime, CooperativeProduction([]))
        finally:
            loop.set_task_factory(previous_factory)

    asyncio.run(scenario())
    assert len(set(locations.values())) == 1
    assert cued_task_types == ["_ScopeDriver", "_ScopeDriver"]
    assert results["a"][0].label == "a"
    assert results["b"][0].label == "b"


def test_blocking_python_stalls_loop_callbacks_and_other_actor_progress() -> None:
    runtime = _native()._Runtime()
    order: list[str] = []
    blocking_started = threading.Event()
    blocking_release = threading.Event()
    callback_ran = threading.Event()

    a_entered: asyncio.Event
    b_entered: asyncio.Event
    begin_block: asyncio.Event
    resume_b: asyncio.Event

    class BlockingActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            label = cue.instruction["label"]
            if label == "a":
                a_entered.set()
                await begin_block.wait()
                order.append("a-block-start")
                blocking_started.set()
                if not blocking_release.wait(TIMEOUT):
                    raise AssertionError("worker did not release blocking Actor")
                order.append("a-block-end")
            else:
                b_entered.set()
                await resume_b.wait()
                order.append("b-resumed")
            return ()

    async def scenario() -> None:
        nonlocal a_entered, b_entered, begin_block, resume_b
        a_entered = asyncio.Event()
        b_entered = asyncio.Event()
        begin_block = asyncio.Event()
        resume_b = asyncio.Event()
        loop = asyncio.get_running_loop()

        def loop_callback() -> None:
            order.append("loop-callback")
            callback_ran.set()
            resume_b.set()

        def worker() -> bool:
            if not blocking_started.wait(TIMEOUT):
                raise AssertionError("Actor did not enter blocking Python code")
            loop.call_soon_threadsafe(loop_callback)
            callback_was_early = callback_ran.is_set()
            blocking_release.set()
            return callback_was_early

        class BlockingProduction(troupe.Production):
            async def scene(self) -> None:
                first = _cast(self, BlockingActor, "blocking-a")
                second = _cast(self, BlockingActor, "blocking-b")
                first_call = asyncio.create_task(first.cue({"label": "a"}))
                second_call = asyncio.create_task(second.cue({"label": "b"}))
                await asyncio.gather(a_entered.wait(), b_entered.wait())

                worker_call = asyncio.create_task(asyncio.to_thread(worker))
                await _loop_barrier()
                begin_block.set()
                assert not await worker_call
                assert await first_call == ()
                assert await second_call == ()
                runtime.request_shutdown()

        await _run(runtime, BlockingProduction([]))

    asyncio.run(scenario())
    assert callback_ran.is_set()
    assert order.index("a-block-end") < order.index("loop-callback")
    assert order.index("loop-callback") < order.index("b-resumed")


def test_cued_task_callback_registration_bypasses_task_overrides() -> None:
    runtime = _native()._Runtime()
    callback_boom = RuntimeError("dynamic Task observer must not run")
    user_boom = RuntimeError("ordinary cued failure")
    override_calls: list[str] = []
    execution_log: list[str] = []
    scene_active = False

    entered: dict[int, asyncio.Event] = {}
    releases: dict[int, asyncio.Event] = {}

    class CallbackTrapTask(asyncio.Task[Any]):
        def add_done_callback(self, *args: object, **kwargs: object) -> None:
            override_calls.append("add_done_callback")
            raise callback_boom

        def result(self) -> Any:
            override_calls.append("result")
            raise callback_boom

    class CallbackActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            label = cue.instruction["label"]
            execution_log.append(f"start-{label}")
            entered[label].set()
            await releases[label].wait()
            execution_log.append(f"end-{label}")
            if label == 1:
                raise user_boom
            return ()

    async def scenario() -> None:
        nonlocal scene_active
        loop = asyncio.get_running_loop()
        previous_factory = loop.get_task_factory()

        def callback_trap_factory(
            factory_loop: asyncio.AbstractEventLoop,
            coroutine: Coroutine[Any, Any, Any],
            **kwargs: Any,
        ) -> asyncio.Task[Any]:
            if scene_active and type(coroutine).__name__ == "_ScopeDriver":
                return CallbackTrapTask(coroutine, loop=factory_loop, **kwargs)
            return asyncio.Task(coroutine, loop=factory_loop, **kwargs)

        loop.set_task_factory(callback_trap_factory)
        entered[0] = asyncio.Event()
        entered[1] = asyncio.Event()
        releases[0] = asyncio.Event()
        releases[1] = asyncio.Event()

        class CallbackProduction(troupe.Production):
            async def scene(self) -> None:
                nonlocal scene_active
                scene_active = True
                try:
                    handle = _cast(self, CallbackActor, "callback")
                    calls = [
                        asyncio.create_task(handle.cue({"label": label}))
                        for label in (0, 1)
                    ]
                    await entered[0].wait()
                    await _loop_barrier()
                    second_entered_early = entered[1].is_set()
                    releases[0].set()
                    releases[1].set()
                    results = await asyncio.gather(*calls, return_exceptions=True)

                    assert not second_entered_early
                    assert results[0] == ()
                    assert results[1] is user_boom
                    assert user_boom.__traceback__ is not None
                    runtime.request_shutdown()
                finally:
                    scene_active = False

        try:
            await _run(runtime, CallbackProduction([]))
        finally:
            releases[0].set()
            releases[1].set()
            await _loop_barrier()
            loop.set_task_factory(previous_factory)

    asyncio.run(scenario())
    assert override_calls == []
    assert execution_log == ["start-0", "end-0", "start-1", "end-1"]
