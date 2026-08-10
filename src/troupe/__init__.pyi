from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
from os import PathLike
from re import Pattern
from typing import Any, Literal, NoReturn, TypeVar, final, overload
from typing_extensions import disjoint_base

from . import act_schema as act_schema

_EffectT = TypeVar("_EffectT", bound="Effect")
_JsonValue = None | bool | int | float | str | list["_JsonValue"] | dict[str, "_JsonValue"]

class AgentError(RuntimeError):
    code: str

class AgentSessionBusyError(AgentError): ...

class AgentSessionError(AgentError): ...

class AgentSessionStartError(AgentSessionError):
    phase: str

class AgentAuthenticationRequiredError(AgentSessionStartError): ...

class AgentSessionBrokenError(AgentSessionError): ...

class AgentTurnError(AgentError): ...

@final
class AgentResultIssue:
    def __new__(cls, _token: NoReturn, /) -> AgentResultIssue: ...
    @property
    def path(self) -> str: ...
    @property
    def code(self) -> str: ...
    @property
    def message(self) -> str: ...

class AgentResultError(AgentTurnError):
    issues: tuple[AgentResultIssue, ...]
    invalid_calls: int
    details_truncated: bool

class AgentResultMissingError(AgentResultError): ...

@dataclass(frozen=True, slots=True, kw_only=True)
class AgentProfile:
    agent: Literal["codex", "claude", "kimi"]
    workspace: str | PathLike[str]
    model: str
    effort: str | None
    def __post_init__(self) -> None: ...

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
    async def act(
        self,
        *,
        script: str,
        output_schema: dict[str, act_schema.FieldSpec],
    ) -> dict[str, _JsonValue]: ...
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
        agent_profile: AgentProfile,
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
    "AgentAuthenticationRequiredError",
    "AgentError",
    "AgentProfile",
    "AgentResultError",
    "AgentResultIssue",
    "AgentResultMissingError",
    "AgentSessionBrokenError",
    "AgentSessionBusyError",
    "AgentSessionError",
    "AgentSessionStartError",
    "AgentTurnError",
    "Cue",
    "CueContextError",
    "Effect",
    "EffectContextError",
    "Production",
    "act_schema",
]
