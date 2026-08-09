import re

from troupe import AgentProfile, Actor, ActorHandle, Cue, Effect, Production


PROFILE = AgentProfile(
    agent="codex",
    workspace="/tmp",
    model="test-model",
    effort=None,
)


class ExampleActor(Actor):
    async def cued(self, cue: Cue) -> list[Effect]:  # E: override
        return []


class InvalidHandleSubclass(ActorHandle):  # E: misc
    pass


class InvalidCueSubclass(Cue):  # E: misc
    pass


def missing_argument() -> Production:
    return Production()  # E: call-arg


def too_many_arguments() -> Production:
    return Production([], [])  # E: call-arg


def keyword_argument() -> Production:
    return Production(args=[])  # E: call-arg


def non_list_argument() -> Production:
    return Production(())  # E: arg-type


def non_string_item() -> Production:
    return Production([1])  # E: list-item


def missing_args_attribute(production: Production) -> object:
    return production.args  # E: attr-defined


class WrongHookReturn(Production):
    async def start(self) -> int:  # E: override
        return 1


def positional_name(production: Production) -> ActorHandle:
    return production.cast_actor(  # E: call-arg
        ExampleActor,
        "actor",
        agent_profile=PROFILE,
        actor_args=(),
        actor_kwargs={},
    )


def missing_actor_type(production: Production) -> ActorHandle:
    return production.cast_actor(  # E: call-arg
        name="actor",
        agent_profile=PROFILE,
        actor_args=(),
        actor_kwargs={},
    )


def wrong_actor_type(production: Production) -> ActorHandle:
    return production.cast_actor(
        Production,  # E: arg-type
        name="actor",
        agent_profile=PROFILE,
        actor_args=(),
        actor_kwargs={},
    )


def wrong_actor_args(production: Production) -> ActorHandle:
    return production.cast_actor(
        ExampleActor,
        name="actor",
        agent_profile=PROFILE,
        actor_args=[],  # E: arg-type
        actor_kwargs={},
    )


def wrong_actor_kwargs(production: Production) -> ActorHandle:
    return production.cast_actor(
        ExampleActor,
        name="actor",
        agent_profile=PROFILE,
        actor_args=(),
        actor_kwargs=[],  # E: arg-type
    )


def missing_agent_profile(production: Production) -> ActorHandle:
    return production.cast_actor(  # E: call-arg
        ExampleActor,
        name="actor",
        actor_args=(),
        actor_kwargs={},
    )


def wrong_agent_profile(production: Production) -> ActorHandle:
    return production.cast_actor(
        ExampleActor,
        name="actor",
        agent_profile="codex",  # E: arg-type
        actor_args=(),
        actor_kwargs={},
    )


def wrong_query(production: Production) -> object:
    return production.get_actor(1)  # E: call-overload


def readonly_actor(actor: Actor, replacement: Production) -> None:
    actor.name = "replacement"  # E: misc
    actor.production = replacement  # E: misc


def readonly_handle(handle: ActorHandle) -> None:
    handle.name = "replacement"  # E: misc


def readonly_cue(cue: Cue) -> None:
    cue.id = "replacement"  # E: misc
    cue.instruction = {}  # E: misc
    cue.source = "replacement"  # E: misc


def readonly_effect(effect: Effect) -> None:
    effect.id = "replacement"  # E: misc
    effect.owner = "replacement"  # E: misc


def wrong_effect_type(actor: Actor) -> object:
    return actor.make_effect(  # E: type-var
        Actor,
        effect_args=(),
        effect_kwargs={},
    )


def wrong_effect_args(actor: Actor) -> Effect:
    return actor.make_effect(
        Effect,
        effect_args=[],  # E: arg-type
        effect_kwargs={},
    )


def wrong_effect_kwargs(actor: Actor) -> Effect:
    return actor.make_effect(
        Effect,
        effect_args=(),
        effect_kwargs=[],  # E: arg-type
    )


def positional_effect_arguments(actor: Actor) -> Effect:
    return actor.make_effect(Effect, (), {})  # E: call-arg


def wrong_pattern_keyword(production: Production) -> object:
    return production.get_actor(pattern="not-a-pattern")  # E: call-overload


def valid_pattern_shape_but_wrong_result(
    production: Production,
) -> ActorHandle | None:
    return production.get_actor(pattern=re.compile(".*"))  # E: return-value
