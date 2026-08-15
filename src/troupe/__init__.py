from dataclasses import dataclass as _dataclass
from os import PathLike as _PathLike
from typing import Literal as _Literal

from ._runtime import Actor as Actor
from ._runtime import ActorHandle as ActorHandle
from ._runtime import AgentAuthenticationRequiredError as AgentAuthenticationRequiredError
from ._runtime import AgentError as AgentError
from ._runtime import AgentResultError as AgentResultError
from ._runtime import AgentResultIssue as AgentResultIssue
from ._runtime import AgentResultMissingError as AgentResultMissingError
from ._runtime import AgentSessionBrokenError as AgentSessionBrokenError
from ._runtime import AgentSessionBusyError as AgentSessionBusyError
from ._runtime import AgentSessionError as AgentSessionError
from ._runtime import AgentSessionStartError as AgentSessionStartError
from ._runtime import AgentTurnError as AgentTurnError
from ._runtime import act_schema as act_schema
from ._runtime import Cue as Cue
from ._runtime import CueContextError as CueContextError
from ._runtime import diagnostics as diagnostics
from ._runtime import Effect as Effect
from ._runtime import EffectContextError as EffectContextError
from ._runtime import Production as Production


@_dataclass(frozen=True, slots=True, kw_only=True)
class AgentProfile:
    agent: _Literal["codex", "claude", "kimi"]
    workspace: str | _PathLike[str]
    model: str
    effort: str | None

    def __post_init__(self) -> None:
        if not isinstance(self.agent, str):
            raise TypeError("agent must be a str")
        if self.agent not in {"codex", "claude", "kimi"}:
            raise ValueError("agent must be one of: 'codex', 'claude', 'kimi'")
        if not isinstance(self.model, str):
            raise TypeError("model must be a str")
        if not self.model:
            raise ValueError("model must not be empty")
        if self.effort is not None and not isinstance(self.effort, str):
            raise TypeError("effort must be a str or None")
        if self.effort == "":
            raise ValueError("effort must not be empty")


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
    "diagnostics",
]
