import re

from troupe import (
    AgentProfile,
    AgentResultIssue,
    Actor,
    ActorHandle,
    Cue,
    Effect,
    Production,
    act_schema,
)


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


class InvalidBuiltinSchemaSubclass(act_schema.StrValue):  # E: misc
    pass


class WrongSchemaValidateOverride(act_schema.SchemaValue[str]):
    def render_prompt(self) -> str:
        return "text"

    def validate(self, value: int, /) -> None:  # E: override
        _ = value


def wrong_schema_choices() -> act_schema.StrValue:
    return act_schema.StrValue(
        description="choice",
        choices=[1],  # E: list-item
    )


def wrong_field_required() -> act_schema.Field[str]:
    return act_schema.Field(
        act_schema.StrValue(description="value"),
        required=1,  # E: arg-type
    )


def object_value_rejects_non_json_values() -> None:
    value = act_schema.ObjectValue(description="object", fields={})
    value.validate({"invalid": object()})  # E: dict-item


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


async def positional_act_arguments(actor: Actor) -> object:
    return await actor.act(  # E: call-arg
        "script",
        {"value": act_schema.StrValue(description="value")},
    )


async def wrong_act_script(actor: Actor) -> object:
    return await actor.act(
        script=1,  # E: arg-type
        output_schema={"value": act_schema.StrValue(description="value")},
    )


async def wrong_act_schema_container(actor: Actor) -> object:
    return await actor.act(
        script="script",
        output_schema=[],  # E: arg-type
    )


async def wrong_act_schema_value(actor: Actor) -> object:
    return await actor.act(
        script="script",
        output_schema={"value": object()},  # E: dict-item
    )


def wrong_value_rejected_construction() -> None:
    act_schema.ValueRejected()  # E: call-arg
    act_schema.ValueRejected(1)  # E: arg-type
    act_schema.ValueRejected("first", "second")  # E: call-arg


def factory_only_result_issue() -> None:
    AgentResultIssue()  # E: call-arg


def wrong_pattern_keyword(production: Production) -> object:
    return production.get_actor(pattern="not-a-pattern")  # E: call-overload


def valid_pattern_shape_but_wrong_result(
    production: Production,
) -> ActorHandle | None:
    return production.get_actor(pattern=re.compile(".*"))  # E: return-value
