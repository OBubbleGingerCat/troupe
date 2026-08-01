from __future__ import annotations

import gc
import threading
import traceback
from collections.abc import Iterator
from contextlib import contextmanager

import pytest

import troupe


ACTOR_DIRECT_ERROR = "Actor instances can only be created by Production.cast_actor()"
HANDLE_DIRECT_ERROR = "ActorHandle cannot be constructed directly"
CUE_DIRECT_ERROR = "Cue cannot be constructed directly"
EFFECT_DIRECT_ERROR = "Effect instances can only be created by Actor.make_effect()"
ACTOR_TYPE_ERROR = "actor_type must be a subclass of Actor"
ACTOR_RESULT_ERROR = "actor_type did not construct the requested Actor instance"
CUE_CONTEXT_ERROR = "ActorHandle.cue() must be called within an active scene context"
EFFECT_CONTEXT_ERROR = (
    "Actor.make_effect() must be called on the current actor within its active cued "
    "context"
)


@contextmanager
def _raises_exact(
    error_type: type[BaseException],
    message: str,
) -> Iterator[None]:
    with pytest.raises(error_type) as captured:
        yield
    assert type(captured.value) is error_type
    assert str(captured.value) == message


def test_cast_actor_signature_constructor_metadata_and_argument_identity() -> None:
    production = troupe.Production([])
    created: list[troupe.Actor] = []
    positional = object()
    keyword = object()

    class RecordingActor(troupe.Actor):
        def __init__(self, value: object, *, option: object) -> None:
            created.append(self)
            self.value = value
            self.option = option
            self.seen_name = self.name
            self.seen_production = self.production

    first = production.cast_actor(
        RecordingActor,
        name="first",
        actor_args=(positional,),
        actor_kwargs={"option": keyword},
    )
    second = production.cast_actor(
        actor_type=RecordingActor,
        name="second",
        actor_args=(positional,),
        actor_kwargs={"option": keyword},
    )

    assert first.name == "first"
    assert second.name == "second"
    assert len(created) == 2
    assert created[0].value is positional
    assert created[0].option is keyword
    assert created[0].seen_name == "first"
    assert created[0].seen_production is production
    assert created[1].seen_name == "second"
    assert created[1].seen_production is production


def test_cast_actor_binding_and_container_errors_have_no_reservation_side_effect() -> None:
    production = troupe.Production([])
    constructor_calls = 0

    class CountingActor(troupe.Actor):
        def __init__(self) -> None:
            nonlocal constructor_calls
            constructor_calls += 1

    with pytest.raises(TypeError):
        production.cast_actor(  # type: ignore[call-overload]
            troupe.Actor,
            "positional-name",
            actor_args=(),
            actor_kwargs={},
        )
    with pytest.raises(TypeError):
        production.cast_actor(  # type: ignore[call-overload]
            name="missing-type",
            actor_args=(),
            actor_kwargs={},
        )
    with pytest.raises(TypeError):
        production.cast_actor(  # type: ignore[call-overload]
            troupe.Actor,
            name="missing-kwargs",
            actor_args=(),
        )
    with pytest.raises(TypeError):
        production.cast_actor(  # type: ignore[call-overload]
            troupe.Actor,
            name="extra",
            actor_args=(),
            actor_kwargs={},
            extra=True,
        )

    for invalid in (object(), 1, troupe.Production):
        with _raises_exact(TypeError, ACTOR_TYPE_ERROR):
            production.cast_actor(
                invalid,
                name="invalid-type",
                actor_args=(),
                actor_kwargs={},
            )

    with _raises_exact(TypeError, ACTOR_TYPE_ERROR):
        production.cast_actor(
            object(),
            name=object(),  # type: ignore[arg-type]
            actor_args=[],  # type: ignore[arg-type]
            actor_kwargs=[],  # type: ignore[arg-type]
        )

    for invalid_name in (object(), 1, None):
        with pytest.raises(TypeError):
            production.cast_actor(
                CountingActor,
                name=invalid_name,  # type: ignore[arg-type]
                actor_args=[],  # type: ignore[arg-type]
                actor_kwargs=[],  # type: ignore[arg-type]
            )

    with pytest.raises(TypeError):
        production.cast_actor(
            CountingActor,
            name="bad-args",
            actor_args=[],  # type: ignore[arg-type]
            actor_kwargs={},
        )
    with pytest.raises(TypeError):
        production.cast_actor(
            CountingActor,
            name="bad-kwargs",
            actor_args=(),
            actor_kwargs=[],  # type: ignore[arg-type]
        )
    with pytest.raises(TypeError):
        production.cast_actor(
            CountingActor,
            name="bad-key",
            actor_args=(),
            actor_kwargs={1: "value"},  # type: ignore[dict-item]
        )

    assert constructor_calls == 0
    for name in ("bad-args", "bad-kwargs", "bad-key"):
        assert production.get_actor(name) is None
        replacement = production.cast_actor(
            CountingActor,
            name=name,
            actor_args=(),
            actor_kwargs={},
        )
        assert replacement.name == name
    assert constructor_calls == 3


def test_native_allocation_gates_and_step_one_context_errors_are_synchronous() -> None:
    actor_init_calls = 0
    effect_init_calls = 0

    class DirectActor(troupe.Actor):
        def __init__(self) -> None:
            nonlocal actor_init_calls
            actor_init_calls += 1

    class DirectEffect(troupe.Effect):
        def __init__(self) -> None:
            nonlocal effect_init_calls
            effect_init_calls += 1

    class CooperativeActor(troupe.Actor):
        def __new__(cls) -> CooperativeActor:
            return super().__new__(cls)

    class CooperativeEffect(troupe.Effect):
        def __new__(cls) -> CooperativeEffect:
            return super().__new__(cls)

    with _raises_exact(TypeError, ACTOR_DIRECT_ERROR):
        troupe.Actor()
    with _raises_exact(TypeError, ACTOR_DIRECT_ERROR):
        DirectActor()
    with _raises_exact(TypeError, ACTOR_DIRECT_ERROR):
        CooperativeActor()
    with _raises_exact(TypeError, HANDLE_DIRECT_ERROR):
        troupe.ActorHandle()
    with _raises_exact(TypeError, CUE_DIRECT_ERROR):
        troupe.Cue()
    with _raises_exact(TypeError, EFFECT_DIRECT_ERROR):
        troupe.Effect()
    with _raises_exact(TypeError, EFFECT_DIRECT_ERROR):
        DirectEffect()
    with _raises_exact(TypeError, EFFECT_DIRECT_ERROR):
        CooperativeEffect()
    assert actor_init_calls == 0
    assert effect_init_calls == 0

    captured: list[troupe.Actor] = []

    class CapturingActor(troupe.Actor):
        def __init__(self) -> None:
            captured.append(self)

    handle = troupe.Production([]).cast_actor(
        CapturingActor,
        name="captured",
        actor_args=(),
        actor_kwargs={},
    )
    with _raises_exact(troupe.CueContextError, CUE_CONTEXT_ERROR):
        handle.cue(object())  # type: ignore[arg-type]
    with _raises_exact(troupe.EffectContextError, EFFECT_CONTEXT_ERROR):
        captured[0].make_effect(
            object(),  # type: ignore[arg-type]
            effect_args=object(),  # type: ignore[arg-type]
            effect_kwargs=object(),  # type: ignore[arg-type]
        )


def test_nested_cast_uses_independent_construction_permits() -> None:
    production = troupe.Production([])
    outer_instances: list[troupe.Actor] = []

    class Child(troupe.Actor):
        pass

    class Outer(troupe.Actor):
        def __init__(self) -> None:
            outer_instances.append(self)
            self.child = self.production.cast_actor(
                Child,
                name="child",
                actor_args=(),
                actor_kwargs={},
            )
            self.name_during_init = self.name

    outer = production.cast_actor(
        Outer,
        name="outer",
        actor_args=(),
        actor_kwargs={},
    )

    assert outer.name == "outer"
    assert outer_instances[0].name_during_init == "outer"
    assert outer_instances[0].child.name == "child"
    assert production.get_actor("outer") is not None
    assert production.get_actor("child") is not None


def test_exact_actor_layout_and_readonly_metadata_are_visible_through_gc_edge() -> None:
    production = troupe.Production([])
    handle = production.cast_actor(
        troupe.Actor,
        name="exact-base",
        actor_args=(),
        actor_kwargs={},
    )
    direct_referents = gc.get_referents(handle)
    visible_graph = direct_referents + [
        nested
        for referent in direct_referents
        for nested in gc.get_referents(referent)
    ]
    referents = {
        id(value): value for value in visible_graph if type(value) is troupe.Actor
    }

    assert len(referents) == 1
    actor = next(iter(referents.values()))
    del direct_referents
    del visible_graph
    assert actor.name == "exact-base"
    assert actor.production is production
    assert not hasattr(actor, "__dict__")
    with pytest.raises(AttributeError):
        actor.name = "replacement"  # type: ignore[misc]
    with pytest.raises(AttributeError):
        actor.production = production  # type: ignore[misc]

    del handle
    assert production.get_actor("exact-base") is None
    assert actor.name == "exact-base"
    assert actor.production is production


def test_duplicate_and_reentrant_same_name_do_not_call_second_constructor() -> None:
    production = troupe.Production([])
    constructor_calls = 0
    reentrant_errors: list[BaseException] = []

    class NamedActor(troupe.Actor):
        def __init__(self, reenter: bool = False) -> None:
            nonlocal constructor_calls
            constructor_calls += 1
            if reenter:
                try:
                    self.production.cast_actor(
                        NamedActor,
                        name=self.name,
                        actor_args=(),
                        actor_kwargs={},
                    )
                except BaseException as error:
                    reentrant_errors.append(error)

    handle = production.cast_actor(
        NamedActor,
        name="duplicate",
        actor_args=(True,),
        actor_kwargs={},
    )
    assert constructor_calls == 1
    assert len(reentrant_errors) == 1
    assert type(reentrant_errors[0]) is ValueError
    assert str(reentrant_errors[0]) == "Actor name is already in use: 'duplicate'"

    with _raises_exact(ValueError, "Actor name is already in use: 'duplicate'"):
        production.cast_actor(
            NamedActor,
            name="duplicate",
            actor_args=(),
            actor_kwargs={},
        )
    assert constructor_calls == 1
    assert handle.name == "duplicate"


def test_constructor_failure_preserves_error_and_leaves_leaked_self_detached() -> None:
    class CtorBoom(Exception):
        pass

    production = troupe.Production([])
    leaked: list[troupe.Actor] = []
    visibility: list[troupe.ActorHandle | None] = []
    boom = CtorBoom("constructor failed")

    class FailingActor(troupe.Actor):
        def __init__(self) -> None:
            leaked.append(self)
            visibility.append(self.production.get_actor(self.name))
            raise boom

    with pytest.raises(CtorBoom) as captured:
        production.cast_actor(
            FailingActor,
            name="retry",
            actor_args=(),
            actor_kwargs={},
        )

    assert captured.value is boom
    assert traceback.extract_tb(boom.__traceback__)[-1].name == "__init__"
    assert visibility == [None]
    assert production.get_actor("retry") is None
    assert leaked[0].name == "retry"
    assert leaked[0].production is production

    replacement = production.cast_actor(
        troupe.Actor,
        name="retry",
        actor_args=(),
        actor_kwargs={},
    )
    assert production.get_actor("retry") is not None
    del leaked[:]
    gc.collect()
    assert production.get_actor("retry") is not None
    assert replacement.name == "retry"


def test_custom_new_must_return_the_current_permitted_instance() -> None:
    production = troupe.Production([])
    existing_instances: list[troupe.Actor] = []
    leaked_provisionals: list[troupe.Actor] = []
    replacements: list[troupe.ActorHandle] = []

    class Existing(troupe.Actor):
        def __init__(self) -> None:
            existing_instances.append(self)

    existing_handle = production.cast_actor(
        Existing,
        name="existing",
        actor_args=(),
        actor_kwargs={},
    )

    class SkipsBase(troupe.Actor):
        def __new__(cls) -> object:
            return object()

    class ReturnsOld(troupe.Actor):
        def __new__(cls, old: troupe.Actor) -> troupe.Actor:
            return old

    class ConsumesPermitThenReturnsOther(troupe.Actor):
        def __new__(cls) -> object:
            provisional = super().__new__(cls)
            leaked_provisionals.append(provisional)
            return object()

    for actor_type, actor_args, name in (
        (SkipsBase, (), "skipped"),
        (ReturnsOld, (existing_instances[0],), "returned-old"),
        (ConsumesPermitThenReturnsOther, (), "returned-other"),
    ):
        with _raises_exact(TypeError, ACTOR_RESULT_ERROR):
            production.cast_actor(
                actor_type,
                name=name,
                actor_args=actor_args,
                actor_kwargs={},
            )
        assert production.get_actor(name) is None
        replacements.append(
            production.cast_actor(
                troupe.Actor,
                name=name,
                actor_args=(),
                actor_kwargs={},
            )
        )
        assert replacements[-1].name == name

    assert production.get_actor("existing") is not None
    assert existing_handle.name == "existing"
    assert leaked_provisionals[0].name == "returned-other"
    assert leaked_provisionals[0].production is production
    leaked_provisionals.clear()
    gc.collect()
    assert production.get_actor("returned-other") is not None


def test_custom_new_can_call_base_and_reenter_a_nested_cast() -> None:
    production = troupe.Production([])
    nested: list[troupe.ActorHandle] = []

    class Child(troupe.Actor):
        pass

    class CustomNew(troupe.Actor):
        def __new__(cls, owner: troupe.Production) -> CustomNew:
            instance = super().__new__(cls)
            nested.append(
                owner.cast_actor(
                    Child,
                    name="new-child",
                    actor_args=(),
                    actor_kwargs={},
                )
            )
            return instance

        def __init__(self, owner: troupe.Production) -> None:
            self.owner_argument = owner

    handle = production.cast_actor(
        CustomNew,
        name="custom-new",
        actor_args=(production,),
        actor_kwargs={},
    )

    assert handle.name == "custom-new"
    assert nested[0].name == "new-child"


def test_class_call_failures_before_base_preserve_original_error_and_roll_back() -> None:
    class NewBoom(Exception):
        pass

    production = troupe.Production([])
    new_boom = NewBoom("new failed before native allocation")
    meta_boom = NewBoom("metaclass failed before native allocation")

    class FailingBeforeBase(troupe.Actor):
        def __new__(cls) -> FailingBeforeBase:
            raise new_boom

    class FailingMeta(type(troupe.Actor)):
        def __call__(cls) -> object:
            raise meta_boom

    class FailingMetaActor(troupe.Actor, metaclass=FailingMeta):
        pass

    for actor_type, name, boom, frame in (
        (FailingBeforeBase, "before-base-retry", new_boom, "__new__"),
        (FailingMetaActor, "before-meta-retry", meta_boom, "__call__"),
    ):
        with pytest.raises(NewBoom) as captured:
            production.cast_actor(
                actor_type,
                name=name,
                actor_args=(),
                actor_kwargs={},
            )

        assert captured.value is boom
        assert traceback.extract_tb(boom.__traceback__)[-1].name == frame
        assert production.get_actor(name) is None
        replacement = production.cast_actor(
            troupe.Actor,
            name=name,
            actor_args=(),
            actor_kwargs={},
        )
        assert replacement.name == name


def test_custom_new_failure_after_base_leaves_provisional_wrapper_detached() -> None:
    class NewBoom(Exception):
        pass

    production = troupe.Production([])
    provisional: list[troupe.Actor] = []
    boom = NewBoom("new failed")

    class FailingNew(troupe.Actor):
        def __new__(cls) -> FailingNew:
            instance = super().__new__(cls)
            provisional.append(instance)
            raise boom

    with pytest.raises(NewBoom) as captured:
        production.cast_actor(
            FailingNew,
            name="new-retry",
            actor_args=(),
            actor_kwargs={},
        )

    assert captured.value is boom
    assert traceback.extract_tb(boom.__traceback__)[-1].name == "__new__"
    assert production.get_actor("new-retry") is None
    assert provisional[0].name == "new-retry"
    assert provisional[0].production is production

    successor = production.cast_actor(
        troupe.Actor,
        name="new-retry",
        actor_args=(),
        actor_kwargs={},
    )
    provisional.clear()
    gc.collect()
    current = production.get_actor("new-retry")
    assert current is not None
    assert current.name == successor.name == "new-retry"


def test_equal_but_distinct_python_strings_share_one_registry_key() -> None:
    production = troupe.Production([])
    first_name = "".join(["equal", "-name"])
    second_name = "".join(["equal-", "name"])
    assert first_name == second_name
    assert first_name is not second_name

    handle = production.cast_actor(
        troupe.Actor,
        name=first_name,
        actor_args=(),
        actor_kwargs={},
    )
    queried = production.get_actor(second_name)
    assert queried is not None
    assert queried.name == second_name
    with _raises_exact(ValueError, "Actor name is already in use: 'equal-name'"):
        production.cast_actor(
            troupe.Actor,
            name=second_name,
            actor_args=(),
            actor_kwargs={},
        )
    assert handle.name == first_name


def test_str_subclass_name_never_dispatches_user_repr() -> None:
    repr_calls = 0

    class Name(str):
        def __repr__(self) -> str:
            nonlocal repr_calls
            repr_calls += 1
            raise AssertionError("name repr must not be dispatched")

    production = troupe.Production([])
    ordinary = Name("ordinary")
    handle = production.cast_actor(
        troupe.Actor,
        name=ordinary,
        actor_args=(),
        actor_kwargs={},
    )
    assert handle.name == "ordinary"
    assert production.get_actor("ordinary") is not None

    with _raises_exact(ValueError, "Actor name is already in use: 'ordinary'"):
        production.cast_actor(
            troupe.Actor,
            name=ordinary,
            actor_args=(),
            actor_kwargs={},
        )

    reserved = Name("scene-123e4567-e89b-12d3-a456-426614174000")
    with _raises_exact(
        ValueError,
        "Actor name is reserved for scene identities: "
        "'scene-123e4567-e89b-12d3-a456-426614174000'",
    ):
        production.cast_actor(
            troupe.Actor,
            name=reserved,
            actor_args=(),
            actor_kwargs={},
        )

    assert repr_calls == 0


@pytest.mark.parametrize(
    "name",
    [
        "scene-123e4567-e89b-12d3-a456-426614174000",
        "scene-00000000-0000-0000-0000-000000000000",
        "scene-ffffffff-ffff-ffff-ffff-ffffffffffff",
    ],
)
def test_canonical_scene_names_are_reserved_before_constructor(name: str) -> None:
    production = troupe.Production([])
    constructor_calls = 0

    class CountingActor(troupe.Actor):
        def __init__(self) -> None:
            nonlocal constructor_calls
            constructor_calls += 1

    with _raises_exact(
        ValueError,
        f"Actor name is reserved for scene identities: {name!r}",
    ):
        production.cast_actor(
            CountingActor,
            name=name,
            actor_args=(),
            actor_kwargs={},
        )
    assert constructor_calls == 0


@pytest.mark.parametrize(
    "name",
    [
        "scene-manager",
        "scene-123E4567-E89B-12D3-A456-426614174000",
        "scene-123e4567e89b12d3a456426614174000",
        "scene-123e4567-e89b-12d3-a456-42661417400",
        "scene-123e4567-e89b-12d3-a456-426614174000-extra",
        "scene-\udcff",
        "actor-\udcff",
    ],
)
def test_near_miss_and_surrogate_names_are_ordinary_python_strings(name: str) -> None:
    production = troupe.Production([])
    handle = production.cast_actor(
        troupe.Actor,
        name=name,
        actor_args=(),
        actor_kwargs={},
    )

    queried = production.get_actor(name)
    assert queried is not None
    assert queried.name == name
    assert handle.name == name


def test_same_name_concurrent_cast_reserves_before_user_constructor() -> None:
    production = troupe.Production([])
    entered = threading.Event()
    release = threading.Event()
    constructor_calls = 0
    results: list[troupe.ActorHandle] = []
    errors: list[BaseException] = []

    class BlockingActor(troupe.Actor):
        def __init__(self) -> None:
            nonlocal constructor_calls
            constructor_calls += 1
            entered.set()
            assert release.wait(5.0)

    def cast() -> None:
        try:
            results.append(
                production.cast_actor(
                    BlockingActor,
                    name="contended",
                    actor_args=(),
                    actor_kwargs={},
                )
            )
        except BaseException as error:
            errors.append(error)

    winner = threading.Thread(target=cast)
    loser = threading.Thread(target=cast)
    winner.start()
    assert entered.wait(5.0)
    loser.start()
    loser.join(5.0)
    assert not loser.is_alive()
    assert constructor_calls == 1
    assert len(errors) == 1
    assert type(errors[0]) is ValueError
    assert str(errors[0]) == "Actor name is already in use: 'contended'"

    release.set()
    winner.join(5.0)
    assert not winner.is_alive()
    assert len(results) == 1
    assert results[0].name == "contended"


def test_different_names_can_construct_concurrently() -> None:
    production = troupe.Production([])
    constructors_met = threading.Barrier(2)
    results: list[troupe.ActorHandle] = []
    errors: list[BaseException] = []

    class ConcurrentActor(troupe.Actor):
        def __init__(self) -> None:
            constructors_met.wait(5.0)

    def cast(name: str) -> None:
        try:
            results.append(
                production.cast_actor(
                    ConcurrentActor,
                    name=name,
                    actor_args=(),
                    actor_kwargs={},
                )
            )
        except BaseException as error:
            errors.append(error)

    threads = [threading.Thread(target=cast, args=(name,)) for name in ("a", "b")]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(5.0)

    assert all(not thread.is_alive() for thread in threads)
    assert errors == []
    assert sorted(handle.name for handle in results) == ["a", "b"]
