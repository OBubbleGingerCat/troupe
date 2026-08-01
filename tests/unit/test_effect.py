from __future__ import annotations

import asyncio
import gc
import importlib
import os
import traceback
import weakref
from typing import Any

import pytest

import troupe


TIMEOUT = 5.0
DIRECT_ERROR = "Effect instances can only be created by Actor.make_effect()"
RESULT_ERROR = "effect_type did not construct the requested Effect instance"


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


def _assert_direct_error(call: Any) -> None:
    with pytest.raises(TypeError) as caught:
        call()
    assert str(caught.value) == DIRECT_ERROR


def test_factory_preserves_type_arguments_metadata_and_mutable_user_fields() -> None:
    initialized: list[tuple[str, str, object, object]] = []
    results: list[troupe.Effect] = []
    runtime = _native()._Runtime()

    class ExampleEffect(troupe.Effect):
        def __init__(self, value: object, *, flag: object) -> None:
            initialized.append((self.id, self.owner, value, flag))
            self.value = value
            self.flag = flag

    class FactoryActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            nested = ["initial"]
            base = self.make_effect(
                troupe.Effect,
                effect_args=(),
                effect_kwargs={},
            )
            base.label = "base"
            base.nested = nested
            positional = self.make_effect(
                ExampleEffect,
                effect_args=(cue.instruction["value"],),
                effect_kwargs={"flag": cue.instruction["flag"]},
            )
            keyword = self.make_effect(
                effect_type=ExampleEffect,
                effect_args=("keyword",),
                effect_kwargs={"flag": False},
            )
            keyword.temporary = "delete-me"
            del keyword.temporary
            nested.append("before-return")
            return base, positional, keyword

    async def scenario() -> None:
        class EffectProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, FactoryActor, "factory")
                results.extend(
                    await handle.cue({"value": object_value, "flag": flag_value})
                )
                runtime.request_shutdown()

        await _run(runtime, EffectProduction([]))

    object_value = object()
    flag_value = object()
    asyncio.run(scenario())

    assert [type(effect) for effect in results] == [
        troupe.Effect,
        ExampleEffect,
        ExampleEffect,
    ]
    prefix = results[0].id.rsplit("-effect", 1)[0]
    assert [effect.id for effect in results] == [
        f"{prefix}-effect0",
        f"{prefix}-effect1",
        f"{prefix}-effect2",
    ]
    assert [effect.owner for effect in results] == ["factory"] * 3
    assert initialized == [
        (f"{prefix}-effect1", "factory", object_value, flag_value),
        (f"{prefix}-effect2", "factory", "keyword", False),
    ]

    base, positional, keyword = results
    assert base.label == "base"
    assert base.nested == ["initial", "before-return"]
    base.nested.append("after-return")
    positional.value = "changed"
    positional.extra = {"mutable": True}
    del positional.flag
    assert base.nested == ["initial", "before-return", "after-return"]
    assert positional.value == "changed"
    assert positional.extra == {"mutable": True}
    assert not hasattr(positional, "flag")
    assert not hasattr(keyword, "temporary")

    for effect in results:
        original = (effect.id, effect.owner)
        for name, replacement in (("id", "changed"), ("owner", "changed")):
            with pytest.raises(AttributeError):
                setattr(effect, name, replacement)
            with pytest.raises(AttributeError):
                object.__setattr__(effect, name, replacement)
            effect.__dict__[name] = replacement
        assert (effect.id, effect.owner) == original
        del effect.__dict__["id"]
        del effect.__dict__["owner"]

    class InheritedNew(troupe.Effect):
        pass

    _assert_direct_error(troupe.Effect)
    _assert_direct_error(InheritedNew)


def test_argument_validation_precedes_allocation_and_python_class_call() -> None:
    constructor_calls = 0
    errors: list[BaseException] = []
    result: list[troupe.Effect] = []
    runtime = _native()._Runtime()

    class ProbeEffect(troupe.Effect):
        def __init__(self) -> None:
            nonlocal constructor_calls
            constructor_calls += 1

    class ValidationActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            invalid_calls = (
                lambda: self.make_effect(
                    object,
                    effect_args=(),
                    effect_kwargs={},
                ),
                lambda: self.make_effect(
                    ProbeEffect,
                    effect_args=[],  # type: ignore[arg-type]
                    effect_kwargs={},
                ),
                lambda: self.make_effect(
                    ProbeEffect,
                    effect_args=(),
                    effect_kwargs=[],  # type: ignore[arg-type]
                ),
                lambda: self.make_effect(ProbeEffect),  # type: ignore[call-arg]
                lambda: self.make_effect(  # type: ignore[call-arg]
                    ProbeEffect,
                    (),
                    {},
                ),
            )
            for call in invalid_calls:
                try:
                    call()
                except BaseException as error:
                    errors.append(error)
                else:
                    raise AssertionError("invalid make_effect call succeeded")

            try:
                self.make_effect(
                    ProbeEffect,
                    effect_args=(),
                    effect_kwargs={1: "not-a-keyword"},  # type: ignore[dict-item]
                )
            except BaseException as error:
                errors.append(error)
            else:
                raise AssertionError("non-string kwargs key was accepted")

            effect = self.make_effect(
                ProbeEffect,
                effect_args=(),
                effect_kwargs={},
            )
            result.append(effect)
            return (effect,)

    async def scenario() -> None:
        class ValidationProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, ValidationActor, "validation")
                await handle.cue({})
                runtime.request_shutdown()

        await _run(runtime, ValidationProduction([]))

    asyncio.run(scenario())
    assert len(errors) == 6
    assert all(type(error) is TypeError for error in errors)
    assert str(errors[0]) == "effect_type must be a subclass of Effect"
    assert constructor_calls == 1
    assert len(result) == 1
    assert result[0].id.endswith("-effect1")


def test_constructor_failure_leaks_provisional_metadata_and_keeps_original_error() -> None:
    class EffectCtorBoom(Exception):
        pass

    boom = EffectCtorBoom("constructor failed")
    leaked: list[troupe.Effect] = []
    caught: list[BaseException] = []
    frames: list[list[str]] = []
    returned: list[troupe.Effect] = []
    runtime = _native()._Runtime()

    class LeakingEffect(troupe.Effect):
        def __new__(cls) -> LeakingEffect:
            instance = super().__new__(cls)
            leaked.append(instance)
            return instance

        def __init__(self) -> None:
            assert self.id.endswith("-effect0")
            assert self.owner == "leaking"
            self.before_failure = ["kept"]
            raise boom

    class GoodEffect(troupe.Effect):
        pass

    class LeakingActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            try:
                self.make_effect(
                    LeakingEffect,
                    effect_args=(),
                    effect_kwargs={},
                )
            except BaseException as error:
                caught.append(error)
                frames.append(
                    [frame.name for frame in traceback.extract_tb(error.__traceback__)]
                )
            else:
                raise AssertionError("failing constructor returned")
            effect = self.make_effect(
                GoodEffect,
                effect_args=(),
                effect_kwargs={},
            )
            returned.append(effect)
            return (effect,)

    async def scenario() -> None:
        class LeakingProduction(troupe.Production):
            async def scene(self) -> None:
                await _cast(self, LeakingActor, "leaking").cue({})
                runtime.request_shutdown()

        await _run(runtime, LeakingProduction([]))

    asyncio.run(scenario())
    assert caught == [boom]
    assert "__init__" in frames[0]
    assert len(leaked) == 1
    assert leaked[0].id.endswith("-effect0")
    assert leaked[0].owner == "leaking"
    assert leaked[0].before_failure == ["kept"]
    leaked[0].after_failure = "mutable"
    assert leaked[0].after_failure == "mutable"
    assert len(returned) == 1
    assert returned[0].id.endswith("-effect1")
    _assert_direct_error(LeakingEffect)


def test_reentrant_and_bypassed_construction_leave_permanent_ordered_gaps() -> None:
    class OuterBoom(Exception):
        pass

    outer_boom = OuterBoom("outer")
    meta_boom = RuntimeError("metaclass")
    before_boom = LookupError("before native base")
    after_boom = OSError("after native base")
    actor: ReentrantActor
    old_effect: troupe.Effect
    inner_effects: list[troupe.Effect] = []
    outer_effects: list[troupe.Effect] = []
    consumed_then_replaced: list[troupe.Effect] = []
    after_base_leaks: list[troupe.Effect] = []
    caught: list[BaseException] = []
    frame_names: dict[int, list[str]] = {}
    returned: list[troupe.Effect] = []
    runtime = _native()._Runtime()

    class PlainEffect(troupe.Effect):
        pass

    class InnerEffect(troupe.Effect):
        pass

    class OuterEffect(troupe.Effect):
        def __init__(self) -> None:
            outer_effects.append(self)
            inner_effects.append(
                actor.make_effect(
                    InnerEffect,
                    effect_args=(),
                    effect_kwargs={},
                )
            )
            raise outer_boom

    class ReturnOldMeta(type):
        def __call__(cls, *args: object, **kwargs: object) -> object:
            return old_effect

    class ReturnOldEffect(troupe.Effect, metaclass=ReturnOldMeta):
        pass

    class BypassNewEffect(troupe.Effect):
        def __new__(cls) -> troupe.Effect:
            return old_effect

    class ConsumeThenReplaceMeta(type):
        def __call__(cls, *args: object, **kwargs: object) -> object:
            provisional = super().__call__(*args, **kwargs)
            consumed_then_replaced.append(provisional)
            return object()

    class ConsumeThenReplaceEffect(
        troupe.Effect,
        metaclass=ConsumeThenReplaceMeta,
    ):
        pass

    class RaisingMeta(type):
        def __call__(cls, *args: object, **kwargs: object) -> object:
            raise meta_boom

    class MetaFailureEffect(troupe.Effect, metaclass=RaisingMeta):
        pass

    class BeforeBaseFailureEffect(troupe.Effect):
        def __new__(cls) -> BeforeBaseFailureEffect:
            raise before_boom

    class AfterBaseFailureEffect(troupe.Effect):
        def __new__(cls) -> AfterBaseFailureEffect:
            provisional = super().__new__(cls)
            after_base_leaks.append(provisional)
            raise after_boom

    class ReentrantActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            nonlocal actor, old_effect
            actor = self
            old_effect = self.make_effect(
                PlainEffect,
                effect_args=(),
                effect_kwargs={},
            )
            returned.append(old_effect)

            cases: tuple[type[troupe.Effect], ...] = (
                OuterEffect,
                ReturnOldEffect,
                BypassNewEffect,
                ConsumeThenReplaceEffect,
                MetaFailureEffect,
                BeforeBaseFailureEffect,
                AfterBaseFailureEffect,
            )
            for effect_type in cases:
                try:
                    self.make_effect(
                        effect_type,
                        effect_args=(),
                        effect_kwargs={},
                    )
                except BaseException as error:
                    caught.append(error)
                    frame_names[id(error)] = [
                        frame.name
                        for frame in traceback.extract_tb(error.__traceback__)
                    ]
                else:
                    raise AssertionError(f"{effect_type.__name__} unexpectedly succeeded")

            returned.append(
                self.make_effect(
                    PlainEffect,
                    effect_args=(),
                    effect_kwargs={},
                )
            )
            return tuple(returned)

    async def scenario() -> None:
        class ReentrantProduction(troupe.Production):
            async def scene(self) -> None:
                await _cast(self, ReentrantActor, "reentrant").cue({})
                runtime.request_shutdown()

        await _run(runtime, ReentrantProduction([]))

    asyncio.run(scenario())
    assert [error is outer_boom for error in caught].count(True) == 1
    assert caught[4] is meta_boom
    assert caught[5] is before_boom
    assert caught[6] is after_boom
    assert [str(error) for error in caught[1:4]] == [RESULT_ERROR] * 3
    assert all(type(error) is TypeError for error in caught[1:4])
    assert "__call__" in frame_names[id(meta_boom)]
    assert "__new__" in frame_names[id(before_boom)]
    assert "__new__" in frame_names[id(after_boom)]

    assert outer_effects[0].id.endswith("-effect1")
    assert inner_effects[0].id.endswith("-effect2")
    assert consumed_then_replaced[0].id.endswith("-effect5")
    assert after_base_leaks[0].id.endswith("-effect8")
    assert [effect.id.rsplit("-effect", 1)[1] for effect in returned] == [
        "0",
        "9",
    ]
    for effect in (*outer_effects, *consumed_then_replaced, *after_base_leaks):
        assert effect.owner == "reentrant"
        effect.still_mutable = True
        assert effect.still_mutable is True


@pytest.mark.skipif(not hasattr(os, "fork"), reason="Linux fork is required")
def test_constructor_fork_cannot_consume_the_parent_effect_permit() -> None:
    payloads: list[str] = []
    effects: list[troupe.Effect] = []
    read_fd, write_fd = os.pipe()
    runtime = _native()._Runtime()

    class ForkingEffect(troupe.Effect):
        def __new__(cls) -> ForkingEffect:
            pid = os.fork()
            if pid == 0:
                os.close(read_fd)
                try:
                    try:
                        child_effect = super().__new__(cls)
                    except BaseException as error:
                        payload = f"{type(error).__name__}|{error}"
                    else:
                        payload = f"CREATED|{child_effect.id}|{child_effect.owner}"
                    os.write(write_fd, payload.encode("utf-8"))
                finally:
                    os.close(write_fd)
                    os._exit(0)

            os.close(write_fd)
            data = os.read(read_fd, 4096)
            _, status = os.waitpid(pid, 0)
            assert os.waitstatus_to_exitcode(status) == 0
            payloads.append(data.decode("utf-8"))
            return super().__new__(cls)

    class ForkingActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            effect = self.make_effect(
                ForkingEffect,
                effect_args=(),
                effect_kwargs={},
            )
            effects.append(effect)
            return (effect,)

    async def scenario() -> None:
        class ForkingProduction(troupe.Production):
            async def scene(self) -> None:
                await _cast(self, ForkingActor, "constructor-fork").cue({})
                runtime.request_shutdown()

        await _run(runtime, ForkingProduction([]))

    try:
        asyncio.run(scenario())
    finally:
        os.close(read_fd)
    assert payloads == [f"TypeError|{DIRECT_ERROR}"]
    assert len(effects) == 1
    assert effects[0].id.endswith("-effect0")
    assert effects[0].owner == "constructor-fork"


def test_effect_value_snapshots_do_not_retain_actor_capability_or_cue() -> None:
    actor_refs: list[weakref.ReferenceType[troupe.Actor]] = []
    effects: list[troupe.Effect] = []
    observations: dict[str, Any] = {}
    runtime = _native()._Runtime()

    class SnapshotActor(troupe.Actor):
        def __init__(self) -> None:
            actor_refs.append(weakref.ref(self))

        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            effect = self.make_effect(
                troupe.Effect,
                effect_args=(),
                effect_kwargs={},
            )
            effects.append(effect)
            return (effect,)

    async def scenario() -> None:
        class SnapshotProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, SnapshotActor, "snapshot-owner")
                await handle.cue({})
                original = (effects[0].id, effects[0].owner)
                del handle
                await _loop_barrier()
                gc.collect()
                observations["actor_dead"] = actor_refs[0]() is None
                observations["missing"] = self.get_actor("snapshot-owner") is None
                replacement = _cast(self, SnapshotActor, "snapshot-owner")
                observations["replacement"] = replacement.name
                observations["metadata"] = (effects[0].id, effects[0].owner)
                observations["original"] = original
                runtime.request_shutdown()

        await _run(runtime, SnapshotProduction([]))

    asyncio.run(scenario())
    assert observations == {
        "actor_dead": True,
        "missing": True,
        "replacement": "snapshot-owner",
        "metadata": observations["original"],
        "original": observations["original"],
    }
