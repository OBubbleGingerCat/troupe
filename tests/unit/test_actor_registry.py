from __future__ import annotations

import gc
import re
import signal
import threading
import weakref

import pytest

import troupe


def _cast(production: troupe.Production, name: str) -> troupe.ActorHandle:
    return production.cast_actor(
        troupe.Actor,
        name=name,
        actor_args=(),
        actor_kwargs={},
    )


def test_exact_pattern_and_all_queries_use_final_result_shapes() -> None:
    production = troupe.Production([])
    initial = [_cast(production, name) for name in ("item-2", "ITEM-1", "item-10")]

    by_position = production.get_actor("item-2")
    by_keyword = production.get_actor(name="item-2")
    assert by_position is not None
    assert by_keyword is not None
    assert by_position.name == "item-2"
    assert by_keyword.name == "item-2"
    assert by_position is not by_keyword
    assert production.get_actor("missing") is None

    expression = re.compile(r"item-\d+", re.IGNORECASE)
    pattern_position = production.get_actor(expression)
    pattern_keyword = production.get_actor(pattern=expression)
    expected = ["ITEM-1", "item-10", "item-2"]
    assert [handle.name for handle in pattern_position] == expected
    assert [handle.name for handle in pattern_keyword] == expected
    assert all(
        positional is not keyword
        for positional, keyword in zip(pattern_position, pattern_keyword, strict=True)
    )
    first_miss = production.get_actor(re.compile(r"item"))
    second_miss = production.get_actor(pattern=re.compile(r"missing.*"))
    assert first_miss == []
    assert second_miss == []
    assert first_miss is not second_miss

    first_all = production.get_actors()
    second_all = production.get_actors()
    assert [handle.name for handle in first_all] == expected
    assert [handle.name for handle in second_all] == expected
    assert all(
        first is not second
        for first, second in zip(first_all, second_all, strict=True)
    )
    assert len(initial) == 3


def test_query_sorting_matches_python_str_order_for_unicode_and_digits() -> None:
    production = troupe.Production([])
    names = ["z10", "z2", "Alpha", "alpha", "e", "\u00e9", "\u4e2d"]
    handles = [_cast(production, name) for name in names]

    assert [handle.name for handle in production.get_actors()] == sorted(names)
    assert [handle.name for handle in production.get_actor(re.compile(r".*"))] == sorted(
        names
    )
    assert len(handles) == len(names)


def test_pattern_snapshot_pins_capabilities_during_python_reentry() -> None:
    production = troupe.Production([])
    captured: list[troupe.Actor] = []
    external_handles: list[troupe.ActorHandle] = []
    actor_refs: list[weakref.ReferenceType[troupe.Actor]] = []
    finalized: list[str] = []

    class CyclicActor(troupe.Actor):
        def __init__(self) -> None:
            captured.append(self)

    names = ["a" * 23, "a" * 24]
    for index, name in enumerate(names):
        handle = production.cast_actor(
            CyclicActor,
            name=name,
            actor_args=(),
            actor_kwargs={},
        )
        actor = captured.pop()
        actor_refs.append(weakref.ref(actor))
        weakref.finalize(actor, finalized.append, f"actor-{index}")
        actor.handle = handle
        external_handles.append(handle)
    del actor
    del handle

    collections: list[int] = []
    previous_handler = signal.getsignal(signal.SIGALRM)

    def collect_during_match(_signum: int, _frame: object) -> None:
        external_handles.clear()
        collections.append(gc.collect())

    signal.signal(signal.SIGALRM, collect_during_match)
    signal.setitimer(signal.ITIMER_REAL, 0.01)
    try:
        matches = production.get_actor(re.compile(r"^(?:(a+)+b|a+)$"))
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0.0)
        signal.signal(signal.SIGALRM, previous_handler)

    assert collections
    assert external_handles == []
    assert [match.name for match in matches] == names
    assert all(actor_ref() is not None for actor_ref in actor_refs)
    assert finalized == []

    del matches
    gc.collect()
    assert all(actor_ref() is None for actor_ref in actor_refs)
    assert sorted(finalized) == ["actor-0", "actor-1"]


def test_query_rejects_unsupported_objects() -> None:
    production = troupe.Production([])

    class PatternDuck:
        def fullmatch(self, value: str) -> object:
            return value

    for value in (object(), 1, None, type, PatternDuck()):
        with pytest.raises(TypeError):
            production.get_actor(value)  # type: ignore[call-overload]

    with pytest.raises(TypeError):
        production.get_actor()  # type: ignore[call-overload]
    with pytest.raises(TypeError):
        production.get_actor("a", "b")  # type: ignore[call-overload]
    with pytest.raises(TypeError):
        production.get_actor(name="a", pattern=re.compile("a"))  # type: ignore[call-overload]
    with pytest.raises(TypeError):
        production.get_actor(name=re.compile("a"))  # type: ignore[call-overload]
    with pytest.raises(TypeError):
        production.get_actor(pattern="a")  # type: ignore[call-overload]


def test_each_query_returns_fresh_strong_handle_wrappers() -> None:
    production = troupe.Production([])
    initial = _cast(production, "actor")
    first = production.get_actor("actor")
    second = production.get_actor("actor")

    assert first is not None
    assert second is not None
    assert first is not second
    assert first is not initial
    assert second is not initial
    assert first.name == second.name == initial.name == "actor"
    assert bool(first)
    assert "__bool__" not in troupe.ActorHandle.__dict__
    assert not hasattr(first, "__dict__")


def test_last_handle_removes_actor_and_allows_immediate_name_reuse() -> None:
    production = troupe.Production([])
    initial = _cast(production, "reusable")
    queried = production.get_actor("reusable")
    assert queried is not None

    del initial
    gc.collect()
    survivor = production.get_actor("reusable")
    assert survivor is not None
    del survivor

    del queried
    successor = _cast(production, "reusable")
    assert successor.name == "reusable"
    assert production.get_actor("reusable") is not None


def test_queries_are_cross_thread_and_return_main_thread_usable_handles() -> None:
    production = troupe.Production([])
    originals = [_cast(production, name) for name in ("thread-a", "thread-b")]
    returned: list[object] = []
    errors: list[BaseException] = []

    def query() -> None:
        try:
            returned.extend(
                [
                    production.get_actor("thread-a"),
                    production.get_actor(re.compile(r"thread-.*")),
                    production.get_actors(),
                ]
            )
        except BaseException as error:
            errors.append(error)

    worker = threading.Thread(target=query)
    worker.start()
    worker.join(5.0)

    assert not worker.is_alive()
    assert errors == []
    exact = returned[0]
    pattern = returned[1]
    all_handles = returned[2]
    assert isinstance(exact, troupe.ActorHandle)
    assert exact.name == "thread-a"
    assert [handle.name for handle in pattern] == ["thread-a", "thread-b"]
    assert [handle.name for handle in all_handles] == ["thread-a", "thread-b"]
    assert len(originals) == 2


def test_old_detach_cannot_remove_a_same_name_successor() -> None:
    production = troupe.Production([])
    leaked: list[troupe.Actor] = []

    class LeakingActor(troupe.Actor):
        def __init__(self) -> None:
            leaked.append(self)

    old_handle = production.cast_actor(
        LeakingActor,
        name="aba",
        actor_args=(),
        actor_kwargs={},
    )
    del old_handle
    gc.collect()
    assert production.get_actor("aba") is None

    successor = _cast(production, "aba")
    leaked.clear()
    gc.collect()
    current = production.get_actor("aba")
    assert current is not None
    assert current.name == successor.name == "aba"


def test_different_productions_have_independent_name_registries() -> None:
    first_production = troupe.Production([])
    second_production = troupe.Production([])
    first = _cast(first_production, "shared")
    second = _cast(second_production, "shared")

    assert first.name == second.name == "shared"
    assert first_production.get_actor("shared") is not None
    assert second_production.get_actor("shared") is not None
