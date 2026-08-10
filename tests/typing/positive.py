import re
from collections.abc import Awaitable

from troupe import AgentProfile, Actor, ActorHandle, Cue, Effect, Production, act_schema
from typing_extensions import assert_type


class ExampleEffect(Effect):
    def __init__(self, value: int) -> None:
        self.value = value


class LabelEffect(Effect):
    def __init__(self, label: str) -> None:
        self.label = label


class EvenIntValue(act_schema.SchemaValue[int]):
    def __init__(self) -> None:
        super().__init__(description="an even integer", json_kind="int64")

    def render_prompt(self) -> str:
        return "must be even"

    def validate(self, value: int, /) -> None:
        if value % 2:
            raise act_schema.ValueRejected("value must be even")


class AsyncStringValue(act_schema.SchemaValue[str]):
    def __init__(self) -> None:
        super().__init__(description="an async string", json_kind="string")

    def render_prompt(self) -> str:
        return "must pass the asynchronous check"

    async def validate(self, value: str, /) -> None:
        _ = value


even: act_schema.SchemaValue[int] = EvenIntValue()
async_string: act_schema.SchemaValue[str] = AsyncStringValue()
integer_list = act_schema.ListValue(even, description="integer list")
nullable_string = act_schema.NullableValue(async_string)
optional_string = act_schema.Field(async_string, required=False)
assert_type(integer_list, act_schema.ListValue[int])
assert_type(nullable_string, act_schema.NullableValue[str])
assert_type(optional_string, act_schema.Field[str])
assert_type(integer_list.validate([2]), None | Awaitable[None])

OUTPUT_SCHEMA: dict[str, act_schema.FieldSpec] = {
    "values": integer_list,
    "note": optional_string,
    "nullable": nullable_string,
}


class ExampleActor(Actor):
    def __init__(self, value: int, *, label: str) -> None:
        self.value = value
        self.label = label

    async def cued(self, cue: Cue) -> tuple[Effect, ...]:
        effect: ExampleEffect = self.make_effect(
            ExampleEffect,
            effect_args=(self.value,),
            effect_kwargs={},
        )
        assert_type(effect, ExampleEffect)
        label: LabelEffect = self.make_effect(
            effect_type=LabelEffect,
            effect_args=(self.label,),
            effect_kwargs={},
        )
        assert_type(label, LabelEffect)
        base: Effect = self.make_effect(
            Effect,
            effect_args=(),
            effect_kwargs={},
        )
        assert_type(base, Effect)
        instruction_value: object = cue.instruction["value"]
        source: str = cue.source
        cue_id: str = cue.id
        effect_id: str = effect.id
        owner: str = effect.owner
        result = await self.act(
            script="Return the typed result.",
            output_schema=OUTPUT_SCHEMA,
        )
        result_value: object = result["values"]
        _ = (instruction_value, source, cue_id, effect_id, owner, result_value)
        return effect, label, base


class ExampleProduction(Production):
    def __init__(self, args: list[str]) -> None:
        self.received = args

    async def start(self) -> None:
        return None

    async def scene(self) -> None:
        return None

    async def stop(self) -> None:
        return None


async def exercise() -> None:
    _ = OUTPUT_SCHEMA
    production = ExampleProduction(["--value", "1"])
    await production.start()
    await production.scene()
    await production.stop()

    base = Production(["\udcff"])
    await base.start()
    await base.stop()

    profile = AgentProfile(
        agent="codex",
        workspace="/tmp",
        model="test-model",
        effort=None,
    )

    positional: ActorHandle = production.cast_actor(
        ExampleActor,
        name="positional",
        agent_profile=profile,
        actor_args=(1,),
        actor_kwargs={"label": "first"},
    )
    keyword: ActorHandle = production.cast_actor(
        actor_type=ExampleActor,
        name="keyword",
        agent_profile=profile,
        actor_args=(2,),
        actor_kwargs={"label": "second"},
    )
    exact: ActorHandle | None = production.get_actor(name="positional")
    matched: list[ActorHandle] = production.get_actor(
        pattern=re.compile(r"positional|keyword")
    )
    all_handles: list[ActorHandle] = production.get_actors()
    effects: tuple[Effect, ...] = await positional.cue({"value": keyword.name})
    _ = (exact, matched, all_handles, effects)
