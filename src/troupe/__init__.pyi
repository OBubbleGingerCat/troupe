from __future__ import annotations

from collections.abc import Mapping
from re import Pattern
from typing import Any, TypeVar, final, overload
from typing_extensions import disjoint_base

_EffectT = TypeVar("_EffectT", bound="Effect")

@disjoint_base
class Actor:
    def __init__(self) -> None: ...
    @property
    def name(self) -> str: ...
    @property
    def production(self) -> Production: ...
    def make_effect(
        self,
        effect_type: type[_EffectT],
        *,
        effect_args: tuple[Any, ...],
        effect_kwargs: dict[str, Any],
    ) -> _EffectT: ...
    async def cued(self, cue: Cue) -> tuple[Effect, ...]: ...

@final
class ActorHandle:
    @property
    def name(self) -> str: ...
    async def cue(self, instruction: dict[Any, Any]) -> tuple[Effect, ...]: ...

@final
class Cue:
    @property
    def id(self) -> str: ...
    @property
    def instruction(self) -> Mapping[Any, Any]: ...
    @property
    def source(self) -> str: ...

class CueContextError(RuntimeError): ...

@disjoint_base
class Effect:
    @property
    def id(self) -> str: ...
    @property
    def owner(self) -> str: ...

class EffectContextError(RuntimeError): ...

@disjoint_base
class Production:
    def __new__(cls, args: list[str], /) -> Production: ...
    def cast_actor(
        self,
        actor_type: type[Actor],
        *,
        name: str,
        actor_args: tuple[Any, ...],
        actor_kwargs: dict[str, Any],
    ) -> ActorHandle: ...
    @overload
    def get_actor(self, name: str) -> ActorHandle | None: ...
    @overload
    def get_actor(self, pattern: Pattern[str]) -> list[ActorHandle]: ...
    def get_actors(self) -> list[ActorHandle]: ...
    async def start(self) -> None: ...
    async def scene(self) -> None: ...
    async def stop(self) -> None: ...

__all__ = [
    "Actor",
    "ActorHandle",
    "Cue",
    "CueContextError",
    "Effect",
    "EffectContextError",
    "Production",
]
