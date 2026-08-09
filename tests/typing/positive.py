import re

from troupe import AgentProfile, Actor, ActorHandle, Cue, Effect, Production
from typing_extensions import assert_type


class ExampleEffect(Effect):
    def __init__(self, value: int) -> None:
        self.value = value


class LabelEffect(Effect):
    def __init__(self, label: str) -> None:
        self.label = label


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
        _ = (instruction_value, source, cue_id, effect_id, owner)
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
