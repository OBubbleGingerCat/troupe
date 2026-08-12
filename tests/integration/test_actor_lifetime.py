from __future__ import annotations

import asyncio
import gc
import importlib
import weakref
from typing import Any

import troupe
TEST_AGENT_PROFILE = troupe.AgentProfile(
    agent="codex",
    workspace="/tmp",
    model="test-model",
    effort=None,
)


TIMEOUT = 5.0


def _collect() -> None:
    for _ in range(3):
        gc.collect()


async def _run(runtime: Any, production: troupe.Production) -> None:
    await asyncio.wait_for(asyncio.shield(runtime.run(production)), TIMEOUT)


async def _loop_barrier() -> None:
    reached = asyncio.Event()
    asyncio.get_running_loop().call_soon(reached.set)
    await reached.wait()


def test_leaked_raw_actor_does_not_extend_logical_capability_lifetime() -> None:
    production = troupe.Production([])
    leaked: list[troupe.Actor] = []

    class LeakingActor(troupe.Actor):
        def __init__(self) -> None:
            leaked.append(self)

    handle = production.cast_actor(
        LeakingActor,
        name="leaked",
        agent_profile=TEST_AGENT_PROFILE,
        actor_args=(),
        actor_kwargs={},
    )
    raw_actor = leaked[0]
    del handle

    successor = production.cast_actor(
        troupe.Actor,
        name="leaked",
        agent_profile=TEST_AGENT_PROFILE,
        actor_args=(),
        actor_kwargs={},
    )
    assert successor.name == "leaked"
    assert raw_actor.name == "leaked"
    assert raw_actor.production is production


def test_actor_keeps_original_production_wrapper_alive() -> None:
    captured: list[troupe.Actor] = []

    class CustomProduction(troupe.Production):
        pass

    class CapturingActor(troupe.Actor):
        def __init__(self) -> None:
            captured.append(self)

    production = CustomProduction([])
    production_ref = weakref.ref(production)
    handle = production.cast_actor(
        CapturingActor,
        name="owned",
        agent_profile=TEST_AGENT_PROFILE,
        actor_args=(),
        actor_kwargs={},
    )
    del production
    _collect()

    owner = production_ref()
    assert owner is not None
    assert captured[0].production is owner
    assert handle.name == "owned"


def test_parent_owned_child_follows_parent_physical_graph_without_external_raw_actor() -> None:
    production = troupe.Production([])
    parent_refs: list[weakref.ReferenceType[troupe.Actor]] = []

    class Child(troupe.Actor):
        pass

    class Parent(troupe.Actor):
        def __init__(self) -> None:
            parent_refs.append(weakref.ref(self))
            self.child = self.production.cast_actor(
                Child,
                name="child",
                agent_profile=TEST_AGENT_PROFILE,
                actor_args=(),
                actor_kwargs={},
            )

    parent = production.cast_actor(
        Parent,
        name="parent",
        agent_profile=TEST_AGENT_PROFILE,
        actor_args=(),
        actor_kwargs={},
    )
    child_probe = production.get_actor("child")
    assert child_probe is not None
    del child_probe
    del parent
    _collect()

    assert parent_refs[0]() is None
    assert production.get_actor("parent") is None
    assert production.get_actor("child") is None


def test_separately_retained_child_outlives_parent() -> None:
    production = troupe.Production([])

    class Child(troupe.Actor):
        pass

    class Parent(troupe.Actor):
        def __init__(self) -> None:
            self.child = self.production.cast_actor(
                Child,
                name="retained-child",
                agent_profile=TEST_AGENT_PROFILE,
                actor_args=(),
                actor_kwargs={},
            )

    parent = production.cast_actor(
        Parent,
        name="short-parent",
        agent_profile=TEST_AGENT_PROFILE,
        actor_args=(),
        actor_kwargs={},
    )
    child = production.get_actor("retained-child")
    assert child is not None
    del parent
    _collect()

    assert production.get_actor("short-parent") is None
    assert production.get_actor("retained-child") is not None
    assert child.name == "retained-child"


def test_actor_handle_cycle_is_collectable_and_detaches_once() -> None:
    production = troupe.Production([])
    captured: list[troupe.Actor] = []
    finalized: list[str] = []

    class CyclicActor(troupe.Actor):
        def __init__(self) -> None:
            captured.append(self)

    handle = production.cast_actor(
        CyclicActor,
        name="cycle",
        agent_profile=TEST_AGENT_PROFILE,
        actor_args=(),
        actor_kwargs={},
    )
    actor = captured.pop()
    weakref.finalize(actor, finalized.append, "actor")
    actor.handle = handle
    actor.self = actor
    del actor
    del handle
    _collect()

    assert finalized == ["actor"]
    replacement = production.cast_actor(
        troupe.Actor,
        name="cycle",
        agent_profile=TEST_AGENT_PROFILE,
        actor_args=(),
        actor_kwargs={},
    )
    assert replacement.name == "cycle"


def test_multiple_handles_share_one_collectable_capability_cycle() -> None:
    production = troupe.Production([])
    captured: list[troupe.Actor] = []
    finalized: list[str] = []

    class CyclicActor(troupe.Actor):
        def __init__(self) -> None:
            captured.append(self)

    cast_handle = production.cast_actor(
        CyclicActor,
        name="multi-handle-cycle",
        agent_profile=TEST_AGENT_PROFILE,
        actor_args=(),
        actor_kwargs={},
    )
    query_handle = production.get_actor("multi-handle-cycle")
    assert query_handle is not None
    assert query_handle is not cast_handle

    actor = captured.pop()
    actor_ref = weakref.ref(actor)
    weakref.finalize(actor, finalized.append, "actor")
    actor.cast_handle = cast_handle
    actor.query_handle = query_handle

    del actor
    del cast_handle
    _collect()

    assert actor_ref() is not None
    assert finalized == []

    del query_handle
    _collect()

    assert actor_ref() is None
    assert finalized == ["actor"]
    replacement = production.cast_actor(
        troupe.Actor,
        name="multi-handle-cycle",
        agent_profile=TEST_AGENT_PROFILE,
        actor_args=(),
        actor_kwargs={},
    )
    assert replacement.name == "multi-handle-cycle"


def test_str_subclass_name_cycle_is_collectable() -> None:
    production = troupe.Production([])
    finalized: list[str] = []

    class CyclicName(str):
        pass

    name = CyclicName("name-cycle")
    name_ref = weakref.ref(name)
    weakref.finalize(name, finalized.append, "name")
    handle = production.cast_actor(
        troupe.Actor,
        name=name,
        agent_profile=TEST_AGENT_PROFILE,
        actor_args=(),
        actor_kwargs={},
    )
    name.handle = handle

    del handle
    del name
    _collect()

    assert name_ref() is None
    assert finalized == ["name"]
    replacement = production.cast_actor(
        troupe.Actor,
        name="name-cycle",
        agent_profile=TEST_AGENT_PROFILE,
        actor_args=(),
        actor_kwargs={},
    )
    assert replacement.name == "name-cycle"


def test_production_handle_actor_cycle_is_collectable() -> None:
    finalized: list[str] = []
    actor_refs: list[weakref.ReferenceType[troupe.Actor]] = []

    class CyclicProduction(troupe.Production):
        pass

    class OwnedActor(troupe.Actor):
        def __init__(self) -> None:
            actor_refs.append(weakref.ref(self))

    production = CyclicProduction([])
    production_ref = weakref.ref(production)
    weakref.finalize(production, finalized.append, "production")
    production.actor = production.cast_actor(
        OwnedActor,
        name="owned-cycle",
        agent_profile=TEST_AGENT_PROFILE,
        actor_args=(),
        actor_kwargs={},
    )
    del production
    _collect()

    assert production_ref() is None
    assert actor_refs[0]() is None
    assert finalized == ["production"]


def test_cast_and_query_are_available_before_during_and_after_runtime() -> None:
    native = importlib.import_module("troupe._runtime")
    phases: list[str] = []

    class PhaseActor(troupe.Actor):
        pass

    class PhaseProduction(troupe.Production):
        def cast_for_phase(self, phase: str) -> None:
            handle = self.cast_actor(
                PhaseActor,
                name=phase,
                agent_profile=TEST_AGENT_PROFILE,
                actor_args=(),
                actor_kwargs={},
            )
            setattr(self, f"{phase}_handle", handle)
            queried = self.get_actor(phase)
            assert queried is not None
            assert queried.name == phase
            phases.append(phase)

        async def start(self) -> None:
            self.cast_for_phase("start")

        async def scene(self) -> None:
            self.cast_for_phase("scene")
            runtime.request_shutdown()

        async def stop(self) -> None:
            self.cast_for_phase("stop")

    async def run() -> None:
        nonlocal runtime
        runtime = native._Runtime()
        future: Any = runtime.run(production)
        assert await future is None

    runtime: Any
    production = PhaseProduction([])
    production.cast_for_phase("before")
    asyncio.run(run())
    production.cast_for_phase("after")

    assert phases == ["before", "start", "scene", "stop", "after"]
    assert [handle.name for handle in production.get_actors()] == sorted(phases)


def test_pending_requests_from_all_handles_share_one_mailbox_and_retain_actor() -> None:
    runtime = importlib.import_module("troupe._runtime")._Runtime()
    actor_refs: list[weakref.ReferenceType[troupe.Actor]] = []
    execution_log: list[str] = []
    observed: dict[str, str] = {}

    admission_gates: dict[str, asyncio.Event] = {}
    entered: dict[str, asyncio.Event] = {}
    releases: dict[str, asyncio.Event] = {}

    class OriginEffect(troupe.Effect):
        def __init__(self, origin: str) -> None:
            self.origin = origin

    class SharedActor(troupe.Actor):
        def __init__(self) -> None:
            actor_refs.append(weakref.ref(self))

        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            origin = cue.instruction["origin"]
            execution_log.append(f"start-{origin}")
            entered[origin].set()
            await releases[origin].wait()
            execution_log.append(f"end-{origin}")
            effect = self.make_effect(
                OriginEffect,
                effect_args=(origin,),
                effect_kwargs={},
            )
            return (effect,)

    async def scenario() -> None:
        origins = ("cast", "query-a", "query-b", "temporary")
        for origin in origins:
            admission_gates[origin] = asyncio.Event()
            entered[origin] = asyncio.Event()
            releases[origin] = asyncio.Event()

        class SharedProduction(troupe.Production):
            async def scene(self) -> None:
                cast_handle = self.cast_actor(
                    SharedActor,
                    name="shared-mailbox",
                    agent_profile=TEST_AGENT_PROFILE,
                    actor_args=(),
                    actor_kwargs={},
                )
                query_a = self.get_actor("shared-mailbox")
                query_b = self.get_actor("shared-mailbox")
                assert query_a is not None
                assert query_b is not None

                async def invoke(
                    origin: str,
                    handle: troupe.ActorHandle,
                ) -> tuple[troupe.Effect, ...]:
                    await admission_gates[origin].wait()
                    runner = handle.cue({"origin": origin})
                    del handle
                    return await runner

                async def invoke_temporary() -> tuple[troupe.Effect, ...]:
                    await admission_gates["temporary"].wait()
                    return await self.get_actor("shared-mailbox").cue(  # type: ignore[union-attr]
                        {"origin": "temporary"}
                    )

                calls = {
                    "cast": asyncio.create_task(invoke("cast", cast_handle)),
                    "query-a": asyncio.create_task(invoke("query-a", query_a)),
                    "query-b": asyncio.create_task(invoke("query-b", query_b)),
                    "temporary": asyncio.create_task(invoke_temporary()),
                }

                admission_gates["cast"].set()
                await entered["cast"].wait()
                for origin in ("query-b", "temporary", "query-a"):
                    admission_gates[origin].set()
                    await _loop_barrier()
                    assert not entered[origin].is_set()

                del cast_handle, query_a, query_b
                _collect()
                assert actor_refs[0]() is not None

                for current, following in (
                    ("cast", "query-b"),
                    ("query-b", "temporary"),
                    ("temporary", "query-a"),
                ):
                    releases[current].set()
                    await entered[following].wait()
                releases["query-a"].set()

                results = await asyncio.gather(
                    calls["cast"],
                    calls["query-a"],
                    calls["query-b"],
                    calls["temporary"],
                )
                for origin, result in zip(origins, results, strict=True):
                    observed[origin] = result[0].origin

                del results, calls
                _collect()
                assert actor_refs[0]() is None
                assert self.get_actor("shared-mailbox") is None
                self.replacement = self.cast_actor(
                    troupe.Actor,
                    name="shared-mailbox",
                    agent_profile=TEST_AGENT_PROFILE,
                    actor_args=(),
                    actor_kwargs={},
                )
                runtime.request_shutdown()

        await _run(runtime, SharedProduction([]))

    asyncio.run(scenario())
    assert observed == {
        "cast": "cast",
        "query-a": "query-a",
        "query-b": "query-b",
        "temporary": "temporary",
    }
    assert execution_log == [
        "start-cast",
        "end-cast",
        "start-query-b",
        "end-query-b",
        "start-temporary",
        "end-temporary",
        "start-query-a",
        "end-query-a",
    ]
