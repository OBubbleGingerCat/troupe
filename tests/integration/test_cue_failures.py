from __future__ import annotations

import asyncio
import importlib
import traceback
from collections.abc import Coroutine
from typing import Any

import pytest

import troupe


TIMEOUT = 5.0
OUTER_ERROR = "Actor.cued() must return a tuple of Effect instances"


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
        actor_args=(),
        actor_kwargs={},
    )


async def _run(runtime: Any, production: troupe.Production) -> None:
    await asyncio.wait_for(asyncio.shield(runtime.run(production)), TIMEOUT)


async def _loop_barrier() -> None:
    reached = asyncio.Event()
    asyncio.get_running_loop().call_soon(reached.set)
    await reached.wait()


def test_waiter_construction_failure_rolls_back_before_actor_dispatch() -> None:
    calls: list[tuple[str, str]] = []
    waiter_error = RuntimeError("shield construction failed")
    runtime = _native()._Runtime()

    class WaiterActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            calls.append((cue.instruction["label"], cue.id))
            return ()

    async def scenario() -> None:
        class WaiterProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, WaiterActor, "waiter")
                original_shield = asyncio.shield

                def fail_shield(awaitable: Any) -> Any:
                    assert isinstance(awaitable, asyncio.Future)
                    raise waiter_error

                asyncio.shield = fail_shield
                try:
                    with pytest.raises(RuntimeError) as captured:
                        await handle.cue({"label": "rolled-back"})
                    assert captured.value is waiter_error
                finally:
                    asyncio.shield = original_shield

                await _loop_barrier()
                assert calls == []
                assert await handle.cue({"label": "delivered"}) == ()
                runtime.request_shutdown()

        await _run(runtime, WaiterProduction([]))

    asyncio.run(scenario())
    assert len(calls) == 1
    assert calls[0][0] == "delivered"
    assert calls[0][1].endswith("-cue0")


def test_result_validation_preserves_order_duplicates_and_foreign_effects() -> None:
    stored: dict[str, troupe.Effect] = {}
    hook_calls: list[str] = []
    observations: list[tuple[str, list[tuple[str, str, object]]]] = []
    runtime = _native()._Runtime()

    class TaggedEffect(troupe.Effect):
        def __init__(self, tag: str) -> None:
            self.tag = tag

    class HostileEffect(TaggedEffect):
        def __eq__(self, other: object) -> bool:
            hook_calls.append("eq")
            raise AssertionError("result validation called equality")

        def __hash__(self) -> int:
            hook_calls.append("hash")
            raise AssertionError("result validation called hash")

        def __iter__(self) -> Any:
            hook_calls.append("iter")
            raise AssertionError("result validation iterated an Effect")

        def consume(self) -> None:
            hook_calls.append("consume")
            raise AssertionError("result validation consumed an Effect")

    class ValueActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            action = cue.instruction["action"]
            if action == "create":
                first = self.make_effect(
                    TaggedEffect,
                    effect_args=("a-first",),
                    effect_kwargs={},
                )
                second = self.make_effect(
                    TaggedEffect,
                    effect_args=("a-second",),
                    effect_kwargs={},
                )
                stored["first"] = first
                stored["second"] = second
                return first, second
            if action == "duplicate":
                return stored["first"], stored["first"]
            if action == "foreign":
                return (stored["second"],)
            if action == "hostile":
                hostile = self.make_effect(
                    HostileEffect,
                    effect_args=("hostile",),
                    effect_kwargs={},
                )
                stored["hostile"] = hostile
                return (hostile,)
            if action == "reuse":
                return (stored[cue.instruction["key"]],)
            return ()

    def record(label: str, values: tuple[troupe.Effect, ...]) -> None:
        observations.append(
            (
                label,
                [
                    (
                        value.id,
                        value.owner,
                        getattr(value, "tag", None),
                    )
                    for value in values
                ],
            )
        )

    async def scenario() -> None:
        class ValueProduction(troupe.Production):
            async def scene(self) -> None:
                base = _cast(self, troupe.Actor, "base")
                first = _cast(self, ValueActor, "first-owner")
                second = _cast(self, ValueActor, "second-owner")
                record("empty", await base.cue({}))
                record("create", await first.cue({"action": "create"}))
                record("duplicate", await first.cue({"action": "duplicate"}))
                record("foreign", await second.cue({"action": "foreign"}))
                record("hostile", await second.cue({"action": "hostile"}))
                record(
                    "reuse-first",
                    await second.cue({"action": "reuse", "key": "first"}),
                )
                record(
                    "reuse-hostile",
                    await first.cue({"action": "reuse", "key": "hostile"}),
                )
                runtime.request_shutdown()

        await _run(runtime, ValueProduction([]))

    asyncio.run(scenario())
    by_label = dict(observations)
    assert by_label["empty"] == []
    assert [item[2] for item in by_label["create"]] == ["a-first", "a-second"]
    assert [item[2] for item in by_label["duplicate"]] == ["a-first", "a-first"]
    assert len(by_label["duplicate"]) == 2
    assert by_label["foreign"] == [by_label["create"][1]]
    assert by_label["reuse-first"] == [by_label["create"][0]]
    assert by_label["reuse-hostile"] == [by_label["hostile"][0]]
    assert by_label["foreign"][0][1] == "first-owner"
    assert by_label["hostile"][0][1] == "second-owner"
    assert hook_calls == []


def test_invalid_outer_and_item_shapes_fail_only_the_current_request() -> None:
    cue_ids: list[str] = []
    errors: list[BaseException] = []
    successes: list[troupe.Effect] = []
    runtime = _native()._Runtime()

    class ShapeActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> Any:
            cue_ids.append(cue.id)
            action = cue.instruction["action"]
            if action == "none":
                return None
            if action == "list":
                return []
            if action == "generator":
                return (value for value in ())
            if action == "naked":
                return self.make_effect(
                    troupe.Effect,
                    effect_args=(),
                    effect_kwargs={},
                )
            if action == "bad-zero":
                return (object(),)
            if action == "bad-one":
                valid = self.make_effect(
                    troupe.Effect,
                    effect_args=(),
                    effect_kwargs={},
                )
                return valid, object()
            effect = self.make_effect(
                troupe.Effect,
                effect_args=(),
                effect_kwargs={},
            )
            return (effect,)

    async def scenario() -> None:
        class ShapeProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, ShapeActor, "shape")
                for action in (
                    "none",
                    "list",
                    "generator",
                    "naked",
                    "bad-zero",
                    "bad-one",
                ):
                    try:
                        await handle.cue({"action": action})
                    except BaseException as error:
                        errors.append(error)
                    else:
                        raise AssertionError(f"invalid {action} result succeeded")
                successes.extend(await handle.cue({"action": "valid"}))
                runtime.request_shutdown()

        await _run(runtime, ShapeProduction([]))

    asyncio.run(scenario())
    assert len(errors) == 6
    assert [str(error) for error in errors[:4]] == [OUTER_ERROR] * 4
    assert str(errors[4]) == "Actor.cued() return item at index 0 is not an Effect"
    assert str(errors[5]) == "Actor.cued() return item at index 1 is not an Effect"
    assert all(type(error) is TypeError for error in errors)
    assert len(successes) == 1
    assert successes[0].owner == "shape"
    scene_prefix = cue_ids[0].rsplit("-cue", 1)[0]
    assert cue_ids == [f"{scene_prefix}-cue{index}" for index in range(7)]


def test_user_failure_and_validation_failure_preserve_ids_and_actor_usability() -> None:
    boom = RuntimeError("ordinary cued failure")
    caught: list[BaseException] = []
    frames: list[list[str]] = []
    cue_ids: list[str] = []
    result: list[troupe.Effect] = []
    runtime = _native()._Runtime()

    class SequentialActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> Any:
            cue_ids.append(cue.id)
            action = cue.instruction["action"]
            if action == "user-error":
                raise boom
            if action == "invalid":
                return None
            effect = self.make_effect(
                troupe.Effect,
                effect_args=(),
                effect_kwargs={},
            )
            return (effect,)

    async def scenario() -> None:
        class SequentialProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, SequentialActor, "sequential")
                for action in ("user-error", "invalid"):
                    try:
                        await handle.cue({"action": action})
                    except BaseException as error:
                        caught.append(error)
                        frames.append(
                            [
                                frame.name
                                for frame in traceback.extract_tb(error.__traceback__)
                            ]
                        )
                    else:
                        raise AssertionError(f"{action} unexpectedly succeeded")
                result.extend(await handle.cue({"action": "success"}))
                assert self.get_actor("sequential") is not None
                runtime.request_shutdown()

        await _run(runtime, SequentialProduction([]))

    asyncio.run(scenario())
    assert caught[0] is boom
    assert "cued" in frames[0]
    assert type(caught[1]) is TypeError
    assert str(caught[1]) == OUTER_ERROR
    scene_prefix = cue_ids[0].rsplit("-cue", 1)[0]
    assert cue_ids == [
        f"{scene_prefix}-cue0",
        f"{scene_prefix}-cue1",
        f"{scene_prefix}-cue2",
    ]
    assert len(result) == 1
    assert result[0].id == f"{cue_ids[2]}-effect0"


def test_dynamic_lookup_call_nonawaitable_and_task_factory_errors_are_local() -> None:
    mode = "success"
    lookup_boom = LookupError("lookup")
    call_boom = RuntimeError("call")
    factory_boom = OSError("task factory")
    errors: dict[str, BaseException] = {}
    frames: dict[str, list[str]] = {}
    successes: list[str] = []
    runtime = _native()._Runtime()

    class DynamicActor(troupe.Actor):
        def __getattribute__(self, name: str) -> Any:
            if name == "cued" and mode == "lookup":
                raise lookup_boom
            return super().__getattribute__(name)

        def cued(self, cue: troupe.Cue) -> Coroutine[Any, Any, tuple[troupe.Effect, ...]] | object:
            if mode == "call":
                raise call_boom
            if mode == "nonawaitable":
                return object()

            async def invoke() -> tuple[troupe.Effect, ...]:
                successes.append(cue.id)
                return ()

            return invoke()

    async def scenario() -> None:
        nonlocal mode
        loop = asyncio.get_running_loop()
        previous_factory = loop.get_task_factory()

        def original_factory(
            factory_loop: asyncio.AbstractEventLoop,
            coroutine: Coroutine[Any, Any, Any],
            **kwargs: Any,
        ) -> asyncio.Task[Any]:
            if mode == "factory" and type(coroutine).__name__ == "_ScopeDriver":
                raise factory_boom
            return asyncio.Task(coroutine, loop=factory_loop, **kwargs)

        loop.set_task_factory(original_factory)

        class DynamicProduction(troupe.Production):
            async def scene(self) -> None:
                nonlocal mode
                handle = _cast(self, DynamicActor, "dynamic")
                for failure_mode in (
                    "lookup",
                    "call",
                    "nonawaitable",
                    "factory",
                ):
                    mode = failure_mode
                    try:
                        await handle.cue({"mode": failure_mode})
                    except BaseException as error:
                        errors[failure_mode] = error
                        frames[failure_mode] = [
                            frame.name
                            for frame in traceback.extract_tb(error.__traceback__)
                        ]
                    else:
                        raise AssertionError(f"{failure_mode} unexpectedly succeeded")
                mode = "success"
                assert await handle.cue({"mode": "success"}) == ()
                runtime.request_shutdown()

        try:
            await _run(runtime, DynamicProduction([]))
        finally:
            loop.set_task_factory(previous_factory)

    asyncio.run(scenario())
    assert errors["lookup"] is lookup_boom
    assert errors["call"] is call_boom
    assert errors["factory"] is factory_boom
    assert type(errors["nonawaitable"]) in (AttributeError, TypeError)
    assert "__getattribute__" in frames["lookup"]
    assert "cued" in frames["call"]
    assert len(successes) == 1
    assert successes[0].endswith("-cue4")


def test_queued_failures_release_the_exact_fifo_slot_and_continue() -> None:
    boom = RuntimeError("queued ordinary failure")
    execution_log: list[str] = []
    cue_ids: list[str] = []
    outcomes: list[object] = []
    runtime = _native()._Runtime()

    entered: dict[str, asyncio.Event] = {}
    release_first: asyncio.Event

    class TaggedEffect(troupe.Effect):
        def __init__(self, label: str) -> None:
            self.label = label

    class QueuedFailureActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> Any:
            action = cue.instruction["action"]
            cue_ids.append(cue.id)
            execution_log.append(f"start-{action}")
            entered[action].set()
            try:
                if action == "user-error":
                    await release_first.wait()
                    raise boom
                if action == "invalid":
                    return None
                effect = self.make_effect(
                    TaggedEffect,
                    effect_args=(action,),
                    effect_kwargs={},
                )
                return (effect,)
            finally:
                execution_log.append(f"end-{action}")

    async def scenario() -> None:
        nonlocal release_first
        release_first = asyncio.Event()
        for action in ("user-error", "invalid", "success", "continued"):
            entered[action] = asyncio.Event()

        class QueuedFailureProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, QueuedFailureActor, "queued-failure")
                calls = [
                    asyncio.create_task(handle.cue({"action": action}))
                    for action in ("user-error", "invalid", "success")
                ]
                await entered["user-error"].wait()
                await _loop_barrier()
                assert not entered["invalid"].is_set()
                assert not entered["success"].is_set()

                release_first.set()
                outcomes.extend(await asyncio.gather(*calls, return_exceptions=True))
                outcomes.append(await handle.cue({"action": "continued"}))
                runtime.request_shutdown()

        await _run(runtime, QueuedFailureProduction([]))

    asyncio.run(scenario())
    assert outcomes[0] is boom
    assert "cued" in [
        frame.name for frame in traceback.extract_tb(boom.__traceback__)
    ]
    assert type(outcomes[1]) is TypeError
    assert str(outcomes[1]) == OUTER_ERROR
    assert [outcome[0].label for outcome in outcomes[2:]] == [  # type: ignore[index]
        "success",
        "continued",
    ]
    assert execution_log == [
        "start-user-error",
        "end-user-error",
        "start-invalid",
        "end-invalid",
        "start-success",
        "end-success",
        "start-continued",
        "end-continued",
    ]
    prefix = cue_ids[0].rsplit("-cue", 1)[0]
    assert cue_ids == [f"{prefix}-cue{index}" for index in range(4)]
