from __future__ import annotations

import argparse
import asyncio
import importlib.resources
import json
import re
import sys
import threading
from pathlib import Path

import troupe
import troupe_smoke_dependency
from wheel_smoke_production import config as absolute_config

from . import config as relative_config
from .workers import relative_value


TIMEOUT = 5.0


async def _wait(awaitable):
    return await asyncio.wait_for(awaitable, TIMEOUT)


async def _run_successor(handle, instruction, started):
    started.set()
    return await handle.cue(instruction)


def _append_event(path: Path, event: list[object]) -> None:
    events = json.loads(path.read_text(encoding="utf-8")) if path.exists() else []
    events.append(event)
    path.write_text(json.dumps(events), encoding="utf-8")


def _actor_round_trip_event(
    *,
    router,
    worker,
    constructors,
    exact,
    pattern,
    root_cue,
    downstream_cue,
    result,
    threads,
):
    assert router.name == "router"
    assert worker.name == "worker"
    assert constructors == [
        ["router", "router", True],
        ["worker", "worker", True],
    ]
    assert exact.name == "router"
    assert [handle.name for handle in pattern] == ["router", "worker"]
    assert root_cue.source.startswith("scene-")
    assert root_cue.id == f"{root_cue.source}-cue0"
    assert downstream_cue.id == f"{root_cue.source}-cue1"
    assert downstream_cue.source == "router"
    assert type(result) is tuple
    assert len(result) == 1
    effect = result[0]
    assert effect.id == f"{downstream_cue.id}-effect0"
    assert effect.owner == "worker"
    assert effect.value == "mutated"
    assert len(threads) == 8
    assert all(type(thread_id) is int for thread_id in threads)
    assert len(set(threads)) == 1
    return [
        "actor-round-trip",
        {
            "constructors": constructors,
            "queries": {
                "exact": exact.name,
                "pattern": [handle.name for handle in pattern],
            },
            "root_cue": {"id": root_cue.id, "source": root_cue.source},
            "downstream_cue": {
                "id": downstream_cue.id,
                "source": downstream_cue.source,
            },
            "effect": {
                "id": effect.id,
                "owner": effect.owner,
                "value": effect.value,
            },
            "result": {
                "type": type(result).__name__,
                "items": [
                    [item.id, item.owner, item.value]
                    for item in result
                ],
            },
            "threads": threads,
        },
    ]


def _cancellation_event(
    *,
    admitted_snapshot,
    caller_done,
    successor_done,
    successor_entered,
    other_actor_result,
    caller_completion_saw_release,
    successor_completion_saw_release,
    caller_outcome,
    successor_result,
):
    assert admitted_snapshot == "before-release"
    assert caller_done is False
    assert successor_done is False
    assert successor_entered is False
    assert other_actor_result == ()
    assert caller_completion_saw_release is True
    assert successor_completion_saw_release is True
    assert caller_outcome == "CancelledError"
    assert successor_result == ()
    return [
        "cancellation",
        {
            "admitted_snapshot": admitted_snapshot,
            "pre_release": {
                "caller_done": caller_done,
                "successor_done": successor_done,
                "successor_entered": successor_entered,
            },
            "other_actor_result": list(other_actor_result),
            "completion_saw_release": {
                "caller": caller_completion_saw_release,
                "successor": successor_completion_saw_release,
            },
            "caller_outcome": caller_outcome,
            "successor_result": list(successor_result),
        },
    ]


class Message(troupe.Effect):
    pass


class Observations:
    def __init__(self):
        self.production = None
        self.constructors = []
        self.threads = [threading.get_ident()]
        self.root_cue = None
        self.downstream_cue = None
        self.caller_entered = asyncio.Event()
        self.caller_completed_event = asyncio.Event()
        self.successor_entered = asyncio.Event()
        self.successor_completed_event = asyncio.Event()
        self.release = asyncio.Event()
        self.admitted_snapshot = None
        self.caller_completion_saw_release = None
        self.successor_completion_saw_release = None

    def record_constructor(self, actor):
        entry = [actor.name, actor.name, actor.production is self.production]
        if actor.name == "router":
            self.constructors.insert(0, entry)
        else:
            self.constructors.append(entry)
        self.threads.append(threading.get_ident())

    def record_cue(self):
        self.threads.append(threading.get_ident())

    def caller_completed(self, _task):
        self.caller_completion_saw_release = self.release.is_set()
        self.caller_completed_event.set()

    def successor_completed(self, _task):
        self.successor_completion_saw_release = self.release.is_set()
        self.successor_completed_event.set()


class Worker(troupe.Actor):
    def __init__(self, observations):
        self.observations = observations
        observations.record_constructor(self)

    async def cued(self, cue):
        self.observations.record_cue()
        if cue.instruction["kind"] == "round-trip":
            self.observations.downstream_cue = cue
            effect = self.make_effect(
                Message,
                effect_args=(),
                effect_kwargs={},
            )
            effect.value = "mutated"
            return (effect,)
        return ()


class Router(troupe.Actor):
    def __init__(self, observations, worker):
        self.observations = observations
        self.worker = worker
        observations.record_constructor(self)

    async def cued(self, cue):
        self.observations.record_cue()
        kind = cue.instruction["kind"]
        if kind == "round-trip":
            self.observations.root_cue = cue
            return await _wait(self.worker.cue({"kind": "round-trip"}))
        if kind == "cancel":
            self.observations.caller_entered.set()
            try:
                await _wait(self.observations.release.wait())
            finally:
                await _wait(self.observations.release.wait())
            return ()
        self.observations.admitted_snapshot = cue.instruction["snapshot"]
        self.observations.successor_entered.set()
        return ()


class Production(troupe.Production):
    def __init__(self, args: list[str]) -> None:
        parser = argparse.ArgumentParser()
        parser.add_argument("--events", type=Path, required=True)
        parser.add_argument("--value", type=int, required=True)
        parser.add_argument("input")
        self.options = parser.parse_args(args)
        self.raw_args = args
        _append_event(self.options.events, ["args", args])

    async def start(self) -> None:
        _append_event(self.options.events, ["start"])

    async def scene(self) -> None:
        dependency_path = Path(troupe_smoke_dependency.__file__).resolve()
        assert dependency_path.is_relative_to(Path(sys.prefix).resolve())
        assert troupe_smoke_dependency.VALUE == "dependency-ok"
        assert absolute_config is relative_config
        assert absolute_config.MODULE_VALUE == "module-ok"
        assert relative_value() == "module-ok"
        assert self.options.value == 7
        assert self.options.input == "input.txt"
        resource = importlib.resources.files(__package__).joinpath("resources/marker.txt")
        assert resource.read_text(encoding="utf-8").strip() == "resource-ok"
        _append_event(
            self.options.events,
            ["scene", "dependency-ok", "module-ok", "resource-ok"],
        )

        observations = Observations()
        observations.production = self
        worker = self.cast_actor(
            Worker,
            name="worker",
            actor_args=(observations,),
            actor_kwargs={},
        )
        router = self.cast_actor(
            Router,
            name="router",
            actor_args=(observations, worker),
            actor_kwargs={},
        )
        exact = self.get_actor("router")
        pattern = self.get_actor(re.compile(r"(?:router|worker)"))
        result = await _wait(router.cue({"kind": "round-trip"}))

        caller = asyncio.create_task(router.cue({"kind": "cancel"}))
        caller.add_done_callback(observations.caller_completed)
        await _wait(observations.caller_entered.wait())
        caller.cancel()
        successor_instruction = {
            "kind": "successor",
            "snapshot": "before-release",
        }
        successor_started = asyncio.Event()
        successor = asyncio.create_task(
            _run_successor(router, successor_instruction, successor_started)
        )
        successor.add_done_callback(observations.successor_completed)
        await _wait(successor_started.wait())
        successor_instruction["snapshot"] = "after-admission"
        other_actor_result = await _wait(worker.cue({"kind": "progress"}))
        barrier = asyncio.Event()
        asyncio.get_running_loop().call_soon(barrier.set)
        await _wait(barrier.wait())
        caller_done = caller.done()
        successor_done = successor.done()
        successor_entered = observations.successor_entered.is_set()

        observations.release.set()
        await _wait(observations.caller_completed_event.wait())
        caller_outcome = "CancelledError" if caller.cancelled() else "success"
        successor_result = await _wait(successor)
        await _wait(observations.successor_completed_event.wait())

        admitted_snapshot = observations.admitted_snapshot
        caller_completion_saw_release = observations.caller_completion_saw_release
        constructors = observations.constructors
        downstream_cue = observations.downstream_cue
        root_cue = observations.root_cue
        successor_completion_saw_release = observations.successor_completion_saw_release
        threads = observations.threads
        _append_event(
            self.options.events,
            _actor_round_trip_event(
                router=router,
                worker=worker,
                constructors=constructors,
                exact=exact,
                pattern=pattern,
                root_cue=root_cue,
                downstream_cue=downstream_cue,
                result=result,
                threads=threads,
            ),
        )
        _append_event(
            self.options.events,
            _cancellation_event(
                admitted_snapshot=admitted_snapshot,
                caller_done=caller_done,
                successor_done=successor_done,
                successor_entered=successor_entered,
                other_actor_result=other_actor_result,
                caller_completion_saw_release=caller_completion_saw_release,
                successor_completion_saw_release=successor_completion_saw_release,
                caller_outcome=caller_outcome,
                successor_result=successor_result,
            ),
        )
        await _wait(caller)

    async def stop(self) -> None:
        _append_event(self.options.events, ["stop"])
