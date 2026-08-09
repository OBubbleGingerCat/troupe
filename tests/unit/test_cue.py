from __future__ import annotations

import asyncio
import collections.abc
import gc
import importlib
import inspect
import types
import weakref
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


async def _run(runtime: Any, production: troupe.Production) -> None:
    future = runtime.run(production)
    await asyncio.wait_for(asyncio.shield(future), TIMEOUT)


async def _loop_barrier() -> None:
    reached = asyncio.Event()
    asyncio.get_running_loop().call_soon(reached.set)
    await reached.wait()


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


def test_cue_runner_is_gc_visible_coroutine_and_reuses_never() -> None:
    observations: dict[str, Any] = {}

    class RecordingActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            observations["cue"] = cue
            observations["cued_loop"] = asyncio.get_running_loop()
            observations["cued_task"] = asyncio.current_task()
            return ()

    async def scenario() -> None:
        runtime = _native()._Runtime()

        class ProbeProduction(troupe.Production):
            async def scene(self) -> None:
                observations["scene_loop"] = asyncio.get_running_loop()
                observations["scene_task"] = asyncio.current_task()
                handle = _cast(self, RecordingActor, "recording")

                runner = handle.cue({"action": "direct"})
                observations["tracked"] = gc.is_tracked(runner)
                observations["awaitable"] = inspect.isawaitable(runner)
                observations["abc"] = isinstance(
                    runner, collections.abc.Coroutine
                )
                observations["asyncio_coro"] = asyncio.iscoroutine(runner)
                observations["direct"] = await runner
                try:
                    await runner
                except BaseException as error:
                    observations["reuse"] = error

                task_runner = handle.cue({"action": "task"})
                observations["task"] = await asyncio.create_task(task_runner)
                try:
                    await task_runner
                except BaseException as error:
                    observations["task_reuse"] = error

                base = _cast(self, troupe.Actor, "base")
                observations["base"] = await base.cue({})
                runtime.request_shutdown()

        await _run(runtime, ProbeProduction([]))

    asyncio.run(scenario())

    assert observations["tracked"] is True
    assert observations["awaitable"] is True
    assert observations["abc"] is True
    assert observations["asyncio_coro"] is True
    assert observations["direct"] == ()
    assert observations["task"] == ()
    assert observations["base"] == ()
    assert observations["scene_loop"] is observations["cued_loop"]
    assert observations["scene_task"] is not observations["cued_task"]
    for key in ("reuse", "task_reuse"):
        error = observations[key]
        assert type(error) is RuntimeError
        assert str(error) == REUSE_ERROR

    native = _native()
    expected_native_types = {
        "Actor",
        "ActorHandle",
        "AgentAuthenticationRequiredError",
        "AgentError",
        "AgentSessionError",
        "AgentSessionStartError",
        "Cue",
        "CueContextError",
        "Effect",
        "EffectContextError",
        "PhaseFailure",
        "Production",
        "ProductionFailed",
        "ProductionLoadError",
        "_Runtime",
    }
    assert {
        name for name, value in vars(native).items() if isinstance(value, type)
    } == expected_native_types
    assert all(
        name in expected_native_types
        or type(value).__module__ != "troupe._runtime"
        for name, value in vars(native).items()
    )
    assert all("CueCall" not in name for name in troupe.__all__)


def test_unstarted_close_has_no_admission_and_is_terminal() -> None:
    seen: list[str] = []
    observations: dict[str, Any] = {}

    class IdActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            seen.append(cue.id)
            return ()

    async def scenario() -> None:
        runtime = _native()._Runtime()

        class ProbeProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, IdActor, "id-actor")
                runner = handle.cue({"unused": True})
                assert runner.close() is None
                try:
                    await runner
                except BaseException as error:
                    observations["reuse"] = error
                assert await handle.cue({"used": True}) == ()
                runtime.request_shutdown()

        await _run(runtime, ProbeProduction([]))

    asyncio.run(scenario())

    error = observations["reuse"]
    assert type(error) is RuntimeError
    assert str(error) == REUSE_ERROR
    assert len(seen) == 1
    assert seen[0].endswith("-cue0")


def test_instruction_snapshot_is_lazy_shallow_and_read_only() -> None:
    entered: asyncio.Event
    release: asyncio.Event
    seen: dict[str, Any] = {}

    class SnapshotActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            seen[cue.instruction["case"]] = cue
            if cue.instruction["case"] == "after-admission":
                entered.set()
                await release.wait()
            return ()

    async def scenario() -> None:
        nonlocal entered, release
        entered = asyncio.Event()
        release = asyncio.Event()
        runtime = _native()._Runtime()

        class ProbeProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, SnapshotActor, "snapshot")

                before = {"case": "before-poll", "value": 1}
                runner = handle.cue(before)
                before["value"] = 2
                assert await runner == ()

                nested: list[str] = ["before"]
                after = {
                    "case": "after-admission",
                    "top": "frozen",
                    "nested": nested,
                }
                task = asyncio.create_task(handle.cue(after))
                await entered.wait()
                after["top"] = "changed"
                nested.append("shared")
                release.set()
                assert await task == ()
                runtime.request_shutdown()

        await _run(runtime, ProbeProduction([]))

    asyncio.run(scenario())

    before_cue = seen["before-poll"]
    assert before_cue.instruction["value"] == 2
    after_cue = seen["after-admission"]
    assert type(after_cue.instruction) is types.MappingProxyType
    assert after_cue.instruction["top"] == "frozen"
    assert after_cue.instruction["nested"] == ["before", "shared"]


def test_dict_subclass_uses_native_copy_without_user_dispatch() -> None:
    seen: list[troupe.Cue] = []

    class HostileDict(dict[Any, Any]):
        def copy(self) -> dict[Any, Any]:
            raise AssertionError("dict subclass copy override was called")

    class CopyActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            seen.append(cue)
            return ()

    async def scenario() -> None:
        runtime = _native()._Runtime()

        class ProbeProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, CopyActor, "copy")
                instruction = HostileDict(value=[])
                assert await handle.cue(instruction) == ()
                instruction["late"] = True
                runtime.request_shutdown()

        await _run(runtime, ProbeProduction([]))

    asyncio.run(scenario())

    assert len(seen) == 1
    assert dict(seen[0].instruction) == {"value": []}


def test_cue_properties_and_mapping_cannot_be_overwritten() -> None:
    seen: list[troupe.Cue] = []

    class CueActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            seen.append(cue)
            return ()

    async def scenario() -> None:
        runtime = _native()._Runtime()

        class ProbeProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, CueActor, "cue-properties")
                assert await handle.cue({"value": 1}) == ()
                runtime.request_shutdown()

        await _run(runtime, ProbeProduction([]))

    asyncio.run(scenario())
    cue = seen[0]

    with pytest.raises(TypeError):
        cue.instruction["value"] = 2  # type: ignore[index]
    with pytest.raises(TypeError):
        del cue.instruction["value"]  # type: ignore[attr-defined]
    with pytest.raises(AttributeError):
        cue.instruction.update({"value": 2})  # type: ignore[attr-defined]

    for name, value in (
        ("id", "changed"),
        ("instruction", {}),
        ("source", "changed"),
    ):
        with pytest.raises(AttributeError):
            setattr(cue, name, value)
        with pytest.raises(AttributeError):
            object.__setattr__(cue, name, value)
        with pytest.raises(AttributeError):
            delattr(cue, name)
    assert not hasattr(cue, "__dict__")


def test_pending_runner_cycles_collect_without_admission() -> None:
    actor_refs: list[weakref.ReferenceType[troupe.Actor]] = []
    payload_refs: list[weakref.ReferenceType[object]] = []
    ids: list[str] = []
    observations: dict[str, Any] = {}

    class Payload:
        pass

    class CycleActor(troupe.Actor):
        def __init__(self) -> None:
            actor_refs.append(weakref.ref(self))

        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            observations["calls"] = observations.get("calls", 0) + 1
            ids.append(cue.id)
            return ()

    async def scenario() -> None:
        runtime = _native()._Runtime()

        class ProbeProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, CycleActor, "cycle")
                payload = Payload()
                payload_refs.append(weakref.ref(payload))
                instruction: dict[str, Any] = {"payload": payload}
                runner = handle.cue(instruction)
                instruction["runner"] = runner
                actor = actor_refs[-1]()
                assert actor is not None
                actor.runner = runner  # type: ignore[attr-defined]

                del actor, handle, payload, instruction, runner
                gc.collect()
                await _loop_barrier()
                gc.collect()
                observations["actor_dead"] = actor_refs[-1]() is None
                observations["payload_dead"] = payload_refs[-1]() is None
                observations["missing"] = self.get_actor("cycle") is None
                replacement = _cast(self, CycleActor, "cycle")
                observations["replacement"] = replacement.name
                assert await replacement.cue({}) == ()
                runtime.request_shutdown()

        await _run(runtime, ProbeProduction([]))

    asyncio.run(scenario())

    assert observations.get("calls", 0) == 1
    assert observations["actor_dead"] is True
    assert observations["payload_dead"] is True
    assert observations["missing"] is True
    assert observations["replacement"] == "cycle"
    assert len(ids) == 1 and ids[0].endswith("-cue0")


def test_pending_runner_holds_handle_until_terminal_then_releases_it() -> None:
    actor_refs: list[weakref.ReferenceType[troupe.Actor]] = []
    ids: list[str] = []
    observations: dict[str, Any] = {}

    class HeldActor(troupe.Actor):
        def __init__(self) -> None:
            actor_refs.append(weakref.ref(self))

        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            ids.append(cue.id)
            return ()

    async def scenario() -> None:
        runtime = _native()._Runtime()

        class ProbeProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, HeldActor, "held")
                runner = handle.cue({})
                del handle
                gc.collect()
                observations["alive_before"] = actor_refs[-1]() is not None
                observations["indexed_before"] = self.get_actor("held") is not None
                assert await runner == ()
                await _loop_barrier()
                gc.collect()
                observations["dead_after"] = actor_refs[-1]() is None
                observations["missing_after"] = self.get_actor("held") is None
                observations["reused"] = _cast(self, HeldActor, "held").name
                runtime.request_shutdown()

        await _run(runtime, ProbeProduction([]))

    asyncio.run(scenario())

    assert observations == {
        "alive_before": True,
        "indexed_before": True,
        "dead_after": True,
        "missing_after": True,
        "reused": "held",
    }
    assert len(ids) == 1 and ids[0].endswith("-cue0")


@pytest.mark.parametrize("release", ["close", "gc"])
def test_unstarted_runner_releases_its_internal_handle(
    release: str,
) -> None:
    actor_refs: list[weakref.ReferenceType[troupe.Actor]] = []
    ids: list[str] = []
    observations: dict[str, Any] = {}

    class HeldActor(troupe.Actor):
        def __init__(self) -> None:
            actor_refs.append(weakref.ref(self))

        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            ids.append(cue.id)
            return ()

    async def scenario() -> None:
        runtime = _native()._Runtime()

        class ProbeProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, HeldActor, f"unstarted-{release}")
                runner = handle.cue({})
                del handle
                gc.collect()
                observations["alive_before"] = actor_refs[-1]() is not None

                if release == "close":
                    assert runner.close() is None
                else:
                    del runner
                gc.collect()
                await _loop_barrier()
                gc.collect()

                observations["dead_after"] = actor_refs[-1]() is None
                observations["missing_after"] = (
                    self.get_actor(f"unstarted-{release}") is None
                )
                replacement = _cast(
                    self,
                    HeldActor,
                    f"unstarted-{release}",
                )
                observations["reused"] = replacement.name
                assert await replacement.cue({}) == ()
                if release == "close":
                    del runner
                runtime.request_shutdown()

        await _run(runtime, ProbeProduction([]))

    asyncio.run(scenario())

    assert observations == {
        "alive_before": True,
        "dead_after": True,
        "missing_after": True,
        "reused": f"unstarted-{release}",
    }
    assert len(ids) == 1 and ids[0].endswith("-cue0")


def test_cue_instruction_cycle_is_collectable_after_terminal() -> None:
    payload_refs: list[weakref.ReferenceType[object]] = []

    class Payload:
        pass

    class CycleActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            payload = cue.instruction["payload"]
            payload.cue = cue
            return ()

    async def scenario() -> None:
        runtime = _native()._Runtime()

        class ProbeProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, CycleActor, "cue-cycle")
                payload = Payload()
                payload_refs.append(weakref.ref(payload))
                assert await handle.cue({"payload": payload}) == ()
                del payload
                await _loop_barrier()
                gc.collect()
                runtime.request_shutdown()

        await _run(runtime, ProbeProduction([]))

    asyncio.run(scenario())
    gc.collect()
    assert payload_refs[0]() is None


def test_effect_provenance_uses_the_admitted_cue_id_snapshot() -> None:
    seen: list[tuple[troupe.Cue, troupe.Effect]] = []

    class ProvenanceActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            effect = self.make_effect(
                troupe.Effect,
                effect_args=(),
                effect_kwargs={},
            )
            seen.append((cue, effect))
            return (effect,)

    async def scenario() -> None:
        runtime = _native()._Runtime()

        class ProvenanceProduction(troupe.Production):
            async def scene(self) -> None:
                handle = _cast(self, ProvenanceActor, "provenance")
                result = await handle.cue({})
                assert [effect.id for effect in result] == [seen[0][1].id]
                runtime.request_shutdown()

        await _run(runtime, ProvenanceProduction([]))

    asyncio.run(scenario())
    assert len(seen) == 1
    cue, effect = seen[0]
    assert effect.id == f"{cue.id}-effect0"
    assert effect.owner == "provenance"
