from __future__ import annotations

import dataclasses
import gc
import os
import weakref
from pathlib import Path
from typing import Any

import pytest

import troupe


class MutablePath:
    def __init__(self, value: object) -> None:
        self.value = value

    def __fspath__(self) -> Any:
        return self.value


def _profile(workspace: object) -> troupe.AgentProfile:
    return troupe.AgentProfile(
        agent="codex",
        workspace=workspace,  # type: ignore[arg-type]
        model="test-model",
        effort=None,
    )


def test_cast_agent_profile_is_public_keyword_only_immutable_and_slotted(
    tmp_path: Path,
) -> None:
    assert "AgentProfile" in troupe.__all__
    assert troupe.AgentProfile.__module__ == "troupe"
    assert dataclasses.is_dataclass(troupe.AgentProfile)
    assert [field.name for field in dataclasses.fields(troupe.AgentProfile)] == [
        "agent",
        "workspace",
        "model",
        "effort",
    ]
    assert troupe.AgentProfile.__match_args__ == ()
    assert troupe.AgentProfile.__slots__ == (
        "agent",
        "workspace",
        "model",
        "effort",
    )

    for agent in ("codex", "claude", "kimi"):
        profile = troupe.AgentProfile(
            agent=agent,  # type: ignore[arg-type]
            workspace=tmp_path,
            model="model-id",
            effort="high",
        )
        assert profile.agent == agent
        assert profile.workspace is tmp_path
        assert profile.model == "model-id"
        assert profile.effort == "high"
        assert not hasattr(profile, "__dict__")
        assert profile == troupe.AgentProfile(
            agent=agent,  # type: ignore[arg-type]
            workspace=tmp_path,
            model="model-id",
            effort="high",
        )
        assert hash(profile) == hash(
            troupe.AgentProfile(
                agent=agent,  # type: ignore[arg-type]
                workspace=tmp_path,
                model="model-id",
                effort="high",
            )
        )
        assert repr(profile).startswith(f"AgentProfile(agent={agent!r}, workspace=")
        with pytest.raises(dataclasses.FrozenInstanceError):
            profile.model = "replacement"  # type: ignore[misc]

    with pytest.raises(TypeError):
        troupe.AgentProfile("codex", tmp_path, "model", None)  # type: ignore[misc]
    for missing in ("agent", "workspace", "model", "effort"):
        values: dict[str, object] = {
            "agent": "codex",
            "workspace": tmp_path,
            "model": "model",
            "effort": None,
        }
        values.pop(missing)
        with pytest.raises(TypeError):
            troupe.AgentProfile(**values)  # type: ignore[arg-type]


def test_cast_agent_profile_pathlike_cycle_is_collectable(tmp_path: Path) -> None:
    class CyclicPath:
        profile: troupe.AgentProfile | None = None

        def __fspath__(self) -> str:
            return str(tmp_path)

    workspace = CyclicPath()
    profile = troupe.AgentProfile(
        agent="codex",
        workspace=workspace,
        model="model",
        effort=None,
    )
    workspace.profile = profile
    witness = weakref.ref(workspace)

    del profile
    del workspace
    gc.collect()

    assert witness() is None


@pytest.mark.parametrize("agent", ["", "other", 1, None])
def test_cast_agent_profile_rejects_unsupported_agent(agent: object, tmp_path: Path) -> None:
    with pytest.raises((TypeError, ValueError)):
        troupe.AgentProfile(
            agent=agent,  # type: ignore[arg-type]
            workspace=tmp_path,
            model="model",
            effort=None,
        )


@pytest.mark.parametrize("model", ["", 1, None])
def test_cast_agent_profile_rejects_invalid_model(model: object, tmp_path: Path) -> None:
    with pytest.raises((TypeError, ValueError)):
        troupe.AgentProfile(
            agent="codex",
            workspace=tmp_path,
            model=model,  # type: ignore[arg-type]
            effort=None,
        )


@pytest.mark.parametrize("effort", ["", 1, object()])
def test_cast_agent_profile_rejects_invalid_effort(effort: object, tmp_path: Path) -> None:
    with pytest.raises((TypeError, ValueError)):
        troupe.AgentProfile(
            agent="codex",
            workspace=tmp_path,
            model="model",
            effort=effort,  # type: ignore[arg-type]
        )


def test_cast_requires_exact_agent_profile_before_actor_construction(
    tmp_path: Path,
) -> None:
    production = troupe.Production([])
    constructor_calls = 0

    class CountingActor(troupe.Actor):
        def __init__(self) -> None:
            nonlocal constructor_calls
            constructor_calls += 1

    with pytest.raises(TypeError):
        production.cast_actor(  # type: ignore[call-overload]
            CountingActor,
            name="missing-profile",
            actor_args=(),
            actor_kwargs={},
        )
    for invalid in (None, object(), "codex", {}):
        with pytest.raises(TypeError):
            production.cast_actor(
                CountingActor,
                name="invalid-profile",
                agent_profile=invalid,  # type: ignore[arg-type]
                actor_args=(),
                actor_kwargs={},
            )

    assert constructor_calls == 0
    assert production.get_actor("missing-profile") is None
    assert production.get_actor("invalid-profile") is None


@pytest.mark.parametrize(
    "workspace_factory",
    [
        lambda root: "relative/path",
        lambda root: root / "missing",
        lambda root: root / "file.txt",
        lambda root: "bad\0path",
        lambda root: MutablePath(os.fsencode(root)),
        lambda root: MutablePath(object()),
    ],
)
def test_cast_rejects_invalid_workspace_before_publication(
    tmp_path: Path,
    workspace_factory: Any,
) -> None:
    (tmp_path / "file.txt").write_text("not a directory", encoding="utf-8")
    production = troupe.Production([])
    constructor_calls = 0

    class CountingActor(troupe.Actor):
        def __init__(self) -> None:
            nonlocal constructor_calls
            constructor_calls += 1

    profile = _profile(workspace_factory(tmp_path))
    with pytest.raises((TypeError, ValueError, OSError)):
        production.cast_actor(
            CountingActor,
            name="invalid-workspace",
            agent_profile=profile,
            actor_args=(),
            actor_kwargs={},
        )

    assert constructor_calls == 0
    assert production.get_actor("invalid-workspace") is None
