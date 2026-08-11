from __future__ import annotations

import asyncio
import gc
import importlib
import json
import os
import socket
import sys
from pathlib import Path
from typing import Any, Literal
from urllib.parse import urlsplit

import pytest

import troupe


ROOT = Path(__file__).resolve().parents[2]
MOCK_AGENT = ROOT / "tests" / "support" / "mock_acp_agent.py"
HARNESS_TIMEOUT = 5.0


class SessionActor(troupe.Actor):
    pass


def _native() -> Any:
    return importlib.import_module("troupe._runtime")


def _profile(
    workspace: Path,
    *,
    effort: str | None = "max",
    agent: Literal["codex", "claude", "kimi"] = "codex",
) -> troupe.AgentProfile:
    return troupe.AgentProfile(
        agent=agent,
        workspace=workspace,
        model="test-model",
        effort=effort,
    )


def _configure_mock(
    events: Path,
    *,
    scenario: str = "ready",
    provider: Literal["codex", "claude", "kimi"] | None = None,
    release: Path | None = None,
    legacy_mode_id: str | None = None,
    authoritative_prompt_error_codes: list[int] | None = None,
) -> None:
    args = [str(MOCK_AGENT), "--events", str(events), "--scenario", scenario]
    if provider is None:
        provider = "claude" if scenario.startswith("claude_") else "codex"
    args.extend(["--provider", provider])
    if provider in {"claude", "kimi"}:
        args.extend(["--mcp-revision", "2025-11-25"])
    if release is not None:
        args.extend(["--release", str(release)])
    launch: dict[str, object] = {
        "program": sys.executable,
        "args": args,
    }
    if legacy_mode_id is not None:
        launch["legacy_mode_id"] = legacy_mode_id
    if authoritative_prompt_error_codes is not None:
        launch["authoritative_prompt_error_codes"] = authoritative_prompt_error_codes
    _native()._agent_test_set_launch(
        **launch,
    )


def _cast(
    production: troupe.Production,
    workspace: Path,
    name: str,
) -> troupe.ActorHandle:
    return production.cast_actor(
        SessionActor,
        name=name,
        agent_profile=_profile(workspace),
        actor_args=(),
        actor_kwargs={},
    )


def _events(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def _process_is_running(pid: int) -> bool:
    try:
        status = Path(f"/proc/{pid}/stat").read_text(encoding="ascii")
    except FileNotFoundError:
        return False
    state = status[status.rfind(")") + 2 :].split(maxsplit=1)[0]
    return state not in {"X", "Z"}


async def _wait_for_process_exit(pid: int) -> None:
    while _process_is_running(pid):
        await asyncio.sleep(0)


def _open_directory_fds(path: Path) -> list[int]:
    identity = path.stat()
    descriptors = []
    for name in os.listdir("/proc/self/fd"):
        if not name.isdigit():
            continue
        try:
            candidate = os.stat(f"/proc/self/fd/{name}")
        except OSError:
            continue
        if (candidate.st_dev, candidate.st_ino) == (
            identity.st_dev,
            identity.st_ino,
        ):
            descriptors.append(int(name))
    return sorted(descriptors)


async def _wait_for_event(path: Path, event: str, count: int = 1) -> None:
    while sum(item["event"] == event for item in _events(path)) < count:
        await asyncio.sleep(0)


async def _signal_fifo(path: Path) -> None:
    def write() -> None:
        with path.open("wb", buffering=0) as signal:
            signal.write(b"1")

    await asyncio.to_thread(write)


async def _wait_for_readiness_gate(latch: str, state: str) -> None:
    while not _native()._agent_test_readiness_gate_states()[latch][state]:
        await asyncio.sleep(0)


async def _ready(handle: troupe.ActorHandle) -> dict[str, Any]:
    return await handle._agent_ready_for_test()  # type: ignore[attr-defined,no-any-return]


@pytest.fixture(autouse=True)
def _reset_test_launch() -> Any:
    _native()._agent_test_reset_launch()
    yield
    _native()._agent_test_reset_launch()


def test_cast_returns_while_opening_and_all_waiters_observe_one_ready(
    tmp_path: Path,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        release = tmp_path / "release"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(events, scenario="hold_before_ready", release=release)

        production = troupe.Production([])
        handle = _cast(production, workspace, "opening")
        assert handle._agent_state_for_test() == "opening"  # type: ignore[attr-defined]
        await _wait_for_event(events, "ready_blocked")

        first = asyncio.create_task(_ready(handle))
        second = asyncio.create_task(_ready(handle))
        await asyncio.sleep(0)
        assert not first.done()
        assert not second.done()

        release.touch()
        first_ready, second_ready = await asyncio.gather(first, second)
        assert first_ready == second_ready
        assert first_ready["state"] == "ready"
        assert first_ready["model"] == "test-model"
        assert first_ready["effort"] == "max"
        assert handle._agent_state_for_test() == "ready"  # type: ignore[attr-defined]

        applied = [
            item["config_id"]
            for item in _events(events)
            if item["event"] == "config_applied"
        ]
        assert applied == ["mode", "model", "reasoning_effort"]

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_ready_retains_initialize_agent_info_and_capability_snapshot(
    tmp_path: Path,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(events)

        ready = await _ready(_cast(troupe.Production([]), workspace, "snapshot"))

        assert ready["agent_info"] == {
            "name": "troupe-mock",
            "title": "Troupe Mock Agent",
            "version": "1",
        }
        assert ready["capabilities"] == {
            "load_session": True,
            "mcp_http": True,
        }
        assert "auth_methods" not in ready

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_opening_session_scoped_update_does_not_require_an_active_turn(
    tmp_path: Path,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(events, scenario="opening_session_scoped_update")

        handle = _cast(troupe.Production([]), workspace, "opening-update")
        ready = await _ready(handle)

        assert ready["state"] == "ready"
        assert any(
            row["event"] == "opening_session_scoped_update_sent"
            for row in _events(events)
        )
        assert handle._agent_state_for_test() == "ready"  # type: ignore[attr-defined]

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_codex_launch_scrubs_an_ambient_codex_path_override(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        forbidden = tmp_path / "unverified-codex"
        monkeypatch.setenv("CODEX_PATH", str(forbidden))
        _configure_mock(events, scenario="codex_path_scrubbed")

        ready = await _ready(_cast(troupe.Production([]), workspace, "codex-path"))

        assert ready["state"] == "ready"
        observed = [
            row for row in _events(events) if row["event"] == "codex_path_observed"
        ]
        assert observed == [
            {
                "event": "codex_path_observed",
                "pid": observed[0]["pid"],
                "value": None,
            }
        ]

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_ready_two_actors_get_distinct_processes_sessions_and_routes(tmp_path: Path) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        workspace_identity = workspace.stat()
        _configure_mock(events)

        production = troupe.Production([])
        first = _cast(production, workspace, "first")
        second = _cast(production, workspace, "second")
        first_ready, second_ready = await asyncio.gather(_ready(first), _ready(second))

        assert first_ready["pid"] != second_ready["pid"]
        assert first_ready["session_id"] != second_ready["session_id"]
        assert first_ready["server_name"] != second_ready["server_name"]
        assert first_ready["endpoint"] == second_ready["endpoint"]

        rows = _events(events)
        assert sum(row["event"] == "process_started" for row in rows) == 2
        for pid in {row["pid"] for row in rows if row["event"] == "process_started"}:
            actor_rows = [row for row in rows if row["pid"] == pid]
            names = [
                row["server_name"]
                for row in actor_rows
                if row["event"] == "session_new_received"
            ]
            assert len(names) == 1
            session_new = next(
                row for row in actor_rows if row["event"] == "session_new_received"
            )
            assert (session_new["cwd_dev"], session_new["cwd_ino"]) == (
                workspace_identity.st_dev,
                workspace_identity.st_ino,
            )

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_eager_mcp_discovery_completes_before_session_new_response(
    tmp_path: Path,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(events, scenario="single_connection_discovery")

        ready = await _ready(_cast(troupe.Production([]), workspace, "eager"))
        assert ready["state"] == "ready"
        order = [row["event"] for row in _events(events)]
        assert order.index("mcp_initialize") < order.index("session_new_responding")
        assert order.index("mcp_initialized") < order.index("session_new_responding")
        assert order.index("mcp_tools_list") < order.index("session_new_responding")
        connection_ports = [
            row["connection_port"]
            for row in _events(events)
            if row["event"]
            in {"mcp_initialize", "mcp_initialized", "mcp_tools_list"}
        ]
        assert len(connection_ports) == 3
        assert len(set(connection_ports)) == 1
        reused = next(
            row
            for row in _events(events)
            if row["event"] == "mcp_post_ready_connection_reused"
        )
        assert reused["connection_port"] == connection_ports[0]

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


@pytest.mark.parametrize(
    ("scenario_name", "expected_timeline", "first_latch"),
    [
        (
            "single_connection_discovery",
            ["mcp", "session_new", "mode", "model", "reasoning_effort"],
            "mcp",
        ),
        (
            "mcp_before_configuration",
            ["session_new", "mcp", "mode", "model", "reasoning_effort"],
            "mcp",
        ),
        (
            "mcp_between_configuration",
            ["session_new", "mode", "mcp", "model", "reasoning_effort"],
            "mcp",
        ),
        (
            "mcp_after_configuration",
            ["session_new", "mode", "model", "reasoning_effort", "mcp"],
            "configuration",
        ),
    ],
)
def test_ready_joins_mcp_and_configuration_for_every_supported_ordering(
    tmp_path: Path,
    scenario_name: str,
    expected_timeline: list[str],
    first_latch: str,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(events, scenario=scenario_name)
        _native()._agent_test_hold_configuration_ready()
        _native()._agent_test_hold_mcp_ready()

        handle = _cast(
            troupe.Production([]), workspace, f"ordering-{scenario_name}"
        )
        readiness = asyncio.create_task(_ready(handle))
        await _wait_for_event(events, "mcp_tools_list")
        await _wait_for_event(events, "config_applied", count=3)
        await _wait_for_readiness_gate("configuration", "arrived")
        await _wait_for_readiness_gate("mcp", "arrived")
        assert handle._agent_state_for_test() == "opening"  # type: ignore[attr-defined]
        assert not readiness.done()

        if first_latch == "mcp":
            _native()._agent_test_release_mcp_ready()
        else:
            _native()._agent_test_release_configuration_ready()
        await _wait_for_readiness_gate(first_latch, "completed")
        assert handle._agent_state_for_test() == "opening"  # type: ignore[attr-defined]
        assert not readiness.done()

        if first_latch == "mcp":
            _native()._agent_test_release_configuration_ready()
        else:
            _native()._agent_test_release_mcp_ready()
        ready = await readiness
        assert ready["state"] == "ready"
        rows = _events(events)
        timeline = []
        for row in rows:
            if row["event"] == "session_new_responding":
                timeline.append("session_new")
            elif row["event"] == "mcp_tools_list":
                timeline.append("mcp")
            elif row["event"] == "config_applied":
                timeline.append(row["config_id"])
        assert timeline == expected_timeline
        assert not any(
            row["event"] == "acp_request" and row["method"] == "session/prompt"
            for row in rows
        )

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_config_drift_after_configuration_before_mcp_ready_fails_opening(
    tmp_path: Path,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        release = tmp_path / "pre-ready-config.fifo"
        os.mkfifo(release)
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(
            events,
            scenario="pre_ready_config_model_drift",
            release=release,
        )
        _native()._agent_test_hold_configuration_ready()
        _native()._agent_test_hold_mcp_ready()

        handle = _cast(troupe.Production([]), workspace, "pre-ready-config-drift")
        await _wait_for_readiness_gate("configuration", "arrived")
        await _signal_fifo(release)
        await _wait_for_event(events, "pre_ready_config_update_sent")

        try:
            with pytest.raises(troupe.AgentSessionStartError) as raised:
                await _ready(handle)
            assert raised.value.code == "configuration_invalid"
            assert raised.value.phase == "configure"
        finally:
            _native()._agent_test_release_configuration_ready()
            _native()._agent_test_release_mcp_ready()

        with pytest.raises(troupe.AgentSessionStartError) as raised:
            await _ready(handle)
        assert raised.value.code == "configuration_invalid"
        assert raised.value.phase == "configure"
        assert handle._agent_state_for_test() == "start_failed"  # type: ignore[attr-defined]
        assert not any(row["event"] == "prompt_received" for row in _events(events))

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_config_drift_in_same_batch_as_final_response_fails_opening(
    tmp_path: Path,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(events, scenario="final_config_response_then_drift")

        handle = _cast(troupe.Production([]), workspace, "final-config-drift")
        with pytest.raises(troupe.AgentSessionStartError) as raised:
            await _ready(handle)
        assert raised.value.code == "configuration_invalid"
        assert raised.value.phase == "configure"
        assert handle._agent_state_for_test() == "start_failed"  # type: ignore[attr-defined]
        assert not any(row["event"] == "prompt_received" for row in _events(events))

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


@pytest.mark.parametrize(
    "scenario_name",
    [
        "malformed_initialize_response_envelope",
        "unknown_initialize_response_id",
    ],
)
def test_invalid_json_rpc_response_fails_opening(
    tmp_path: Path,
    scenario_name: str,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(events, scenario=scenario_name)

        handle = _cast(troupe.Production([]), workspace, "invalid-response")
        with pytest.raises(troupe.AgentSessionStartError) as raised:
            await asyncio.wait_for(_ready(handle), 0.5)
        assert raised.value.code == "protocol_incompatible"
        assert raised.value.phase == "initialize"
        assert handle._agent_state_for_test() == "start_failed"  # type: ignore[attr-defined]

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_mcp_http_10_is_rejected_without_contaminating_discovery(tmp_path: Path) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(events, scenario="http_10_before_discovery")

        assert (await _ready(_cast(troupe.Production([]), workspace, "http-10")))[
            "state"
        ] == "ready"
        rejection = next(
            row for row in _events(events) if row["event"] == "mcp_http_10_rejected"
        )
        assert rejection["status"] == 505
        assert sum(row["event"] == "mcp_initialize" for row in _events(events)) == 1

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_eager_mcp_response_does_not_wait_for_readiness_ack(tmp_path: Path) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(events)
        _native()._agent_test_hold_mcp_ready()

        handle = _cast(troupe.Production([]), workspace, "withheld-mcp-ack")
        await _wait_for_event(events, "mcp_tools_list")
        await _wait_for_event(events, "config_applied", count=3)
        assert handle._agent_state_for_test() == "opening"  # type: ignore[attr-defined]

        _native()._agent_test_release_mcp_ready()
        assert (await _ready(handle))["state"] == "ready"

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_ready_rejects_stale_mcp_generation_pollution() -> None:
    assert _native()._agent_test_result_generation_isolation() == {
        "old_generation": 1,
        "old_phase": "revoked",
        "stale_bearer_bound": False,
        "successor_generation": 2,
        "successor_phase": "new",
        "successor_bearer_bound": True,
        "successor_connections_after_release": 0,
    }


def test_ready_with_no_effort_override_keeps_the_agent_reported_value(
    tmp_path: Path,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(events)
        production = troupe.Production([])
        handle = production.cast_actor(
            SessionActor,
            name="no-effort-override",
            agent_profile=_profile(workspace, effort=None),
            actor_args=(),
            actor_kwargs={},
        )

        ready = await _ready(handle)
        assert ready["effort"] == "medium"
        assert [
            row["config_id"]
            for row in _events(events)
            if row["event"] == "config_applied"
        ] == ["mode", "model"]

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_ready_applies_the_registry_legacy_mode_before_model_and_effort(
    tmp_path: Path,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(events, scenario="legacy_mode", legacy_mode_id="agent")

        assert (await _ready(_cast(troupe.Production([]), workspace, "legacy-mode")))[
            "state"
        ] == "ready"
        applications = [
            (row["event"], row.get("config_id"), row.get("mode_id"))
            for row in _events(events)
            if row["event"] in {"legacy_mode_applied", "config_applied"}
        ]
        assert applications == [
            ("legacy_mode_applied", None, "agent"),
            ("config_applied", "model", None),
            ("config_applied", "reasoning_effort", None),
        ]

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_legacy_mode_drift_before_model_selection_fails_opening(
    tmp_path: Path,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(
            events,
            scenario="legacy_mode_drift_before_model",
            legacy_mode_id="agent",
        )

        handle = _cast(troupe.Production([]), workspace, "legacy-mode-drift")
        with pytest.raises(troupe.AgentSessionStartError) as raised:
            await _ready(handle)
        assert raised.value.code == "configuration_invalid"
        assert raised.value.phase == "configure"
        assert handle._agent_state_for_test() == "start_failed"  # type: ignore[attr-defined]
        assert any(
            row["event"] == "legacy_mode_drift_sent" for row in _events(events)
        )
        assert not any(row["event"] == "prompt_received" for row in _events(events))

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_opening_legacy_mode_requires_the_exact_advertised_mode_id(
    tmp_path: Path,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(
            events,
            scenario="legacy_mode_missing",
            legacy_mode_id="missing",
        )

        with pytest.raises(troupe.AgentSessionStartError) as raised:
            await _ready(_cast(troupe.Production([]), workspace, "legacy-mode-missing"))
        assert raised.value.code == "configuration_invalid"
        assert raised.value.phase == "configure"
        assert not any(
            row["event"] == "acp_request" and row["method"] == "session/set_mode"
            for row in _events(events)
        )

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_auth_required_ignores_advertised_methods_without_retry(
    tmp_path: Path,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(
            events,
            scenario="auth_once",
        )

        handle = _cast(troupe.Production([]), workspace, "auth-terminal")
        for _ in range(2):
            with pytest.raises(troupe.AgentAuthenticationRequiredError) as raised:
                await _ready(handle)
            assert raised.value.code == "authentication_required"
            assert raised.value.phase == "session_new"
        rows = _events(events)
        assert sum(row["event"] == "session_new_received" for row in rows) == 1
        assert not any(
            row["event"] == "acp_request" and row["method"] == "authenticate"
            for row in rows
        )
        assert handle._agent_state_for_test() == "auth_required"  # type: ignore[attr-defined]

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_auth_required_closes_an_accepted_route_connection_before_publication(
    tmp_path: Path,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(
            events,
            scenario="auth_once_partial_connection",
        )

        handle = _cast(troupe.Production([]), workspace, "auth-connection")
        with pytest.raises(troupe.AgentAuthenticationRequiredError):
            await _ready(handle)
        order = [row["event"] for row in _events(events)]
        assert order.index("old_route_partial_connection_open") < order.index(
            "auth_required_sent"
        )
        assert order.index("auth_required_sent") < order.index(
            "old_route_connection_closed_after_auth_required"
        )
        assert not any(
            row["event"] == "acp_request" and row["method"] == "authenticate"
            for row in _events(events)
        )

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_codex_adapter_handles_the_pinned_permission_matrix_in_one_turn(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, scenario="codex_permission_matrix")
    observed: list[dict[str, object]] = []
    runtime = _native()._Runtime()

    class CodexPermissionActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            observed.append(
                await self.act(
                    script="Complete the task without asking a human for approval.",
                    output_schema={
                        "value": troupe.act_schema.Int64Value(
                            description="the completed task value"
                        )
                    },
                )
            )
            return ()

    class CodexPermissionProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                CodexPermissionActor,
                name="codex-permissions",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(CodexPermissionProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert observed == [{"value": 7}]
    responses = [
        row
        for row in _events(events)
        if row["event"] == "codex_permission_response_received"
    ]
    assert [row["permission_case"] for row in responses] == [
        "command",
        "permissions",
        "mcp_tool",
        "plan_review",
        "provider_question",
        "unknown",
        "ambiguous",
    ]


def test_codex_usage_limit_error_is_authoritative_and_keeps_the_session(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(
        events,
        scenario="codex_usage_limit_then_success",
        authoritative_prompt_error_codes=[],
    )
    errors: list[troupe.AgentTurnError] = []
    observed: list[dict[str, object]] = []
    states: list[str] = []
    runtime = _native()._Runtime()

    class CodexUsageActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            if cue.instruction["turn"] == 1:
                try:
                    await self.act(
                        script="Attempt the task under the current quota.",
                        output_schema={
                            "value": troupe.act_schema.Int64Value(
                                description="the task value"
                            )
                        },
                    )
                except troupe.AgentTurnError as error:
                    errors.append(error)
                else:
                    raise AssertionError("usage limit unexpectedly returned a result")
            else:
                observed.append(
                    await self.act(
                        script="Retry the task in the same session.",
                        output_schema={
                            "value": troupe.act_schema.Int64Value(
                                description="the task value"
                            )
                        },
                    )
                )
            return ()

    class CodexUsageProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                CodexUsageActor,
                name="codex-usage",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({"turn": 1}) == ()
            states.append(handle._agent_state_for_test())  # type: ignore[attr-defined]
            assert await handle.cue({"turn": 2}) == ()
            states.append(handle._agent_state_for_test())  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(CodexUsageProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert len(errors) == 1
    assert type(errors[0]) is troupe.AgentTurnError
    assert errors[0].code == "request_failed"
    assert observed == [{"value": 7}]
    assert states == ["ready", "ready"]
    prompts = [row for row in _events(events) if row["event"] == "prompt_received"]
    assert len(prompts) == 2
    assert len({row["session_id"] for row in prompts}) == 1


@pytest.mark.parametrize(
    ("scenario_name", "expected_code"),
    [
        ("codex_authentication_lost", "authentication_lost"),
        ("codex_uncertain_prompt_error", "uncertain_settlement"),
    ],
)
def test_codex_non_authoritative_prompt_errors_break_and_latch_the_session(
    tmp_path: Path,
    scenario_name: str,
    expected_code: str,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(
        events,
        scenario=scenario_name,
        authoritative_prompt_error_codes=[],
    )
    errors: list[troupe.AgentSessionBrokenError] = []
    states: list[str] = []
    runtime = _native()._Runtime()

    class CodexBrokenActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="Attempt the Codex task.",
                    output_schema={
                        "value": troupe.act_schema.Int64Value(
                            description="the task value"
                        )
                    },
                )
            except troupe.AgentSessionBrokenError as error:
                errors.append(error)
            else:
                raise AssertionError("broken Codex session unexpectedly succeeded")
            return ()

    class CodexBrokenProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                CodexBrokenActor,
                name=f"codex-broken-{scenario_name}",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            for turn in (1, 2):
                assert await handle.cue({"turn": turn}) == ()
                states.append(handle._agent_state_for_test())  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(CodexBrokenProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert [error.code for error in errors] == [expected_code, expected_code]
    assert all(error.__cause__ is None for error in errors)
    assert states == ["broken", "broken"]
    assert sum(row["event"] == "prompt_received" for row in _events(events)) == 1


def test_claude_adapter_handles_the_pinned_permission_matrix_in_one_turn(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, scenario="claude_permission_matrix")
    observed: list[dict[str, object]] = []
    runtime = _native()._Runtime()

    class ClaudePermissionActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            observed.append(
                await self.act(
                    script="Complete the task without human interaction.",
                    output_schema={
                        "value": troupe.act_schema.Int64Value(
                            description="the completed task value"
                        )
                    },
                )
            )
            return ()

    class ClaudePermissionProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                ClaudePermissionActor,
                name="claude-permissions",
                agent_profile=_profile(workspace, agent="claude"),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ClaudePermissionProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert observed == [{"value": 8}]
    responses = [
        row
        for row in _events(events)
        if row["event"] == "claude_permission_response_received"
    ]
    assert [row["permission_case"] for row in responses] == [
        "tool",
        "exit_plan_mode",
        "unknown",
        "ambiguous",
    ]
    assert [
        row["config_id"]
        for row in _events(events)
        if row["event"] == "config_applied"
    ] == ["mode", "model", "effort"]
    assert [
        row["mode"]
        for row in _events(events)
        if row["event"] == "claude_mode_update_sent"
    ] == ["plan", "default"]
    assert [
        row["mode"]
        for row in _events(events)
        if row["event"] == "claude_config_mode_snapshot_sent"
    ] == ["plan", "default"]


def test_claude_turn_cannot_settle_until_plan_mode_is_restored(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    release = workspace / "settle.fifo"
    os.mkfifo(release)
    _configure_mock(
        events,
        scenario="claude_plan_mode_not_restored",
        release=release,
    )
    errors: list[troupe.AgentSessionBrokenError] = []
    states: list[str] = []
    runtime = _native()._Runtime()

    class ClaudePlanActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="Complete the task after leaving plan mode.",
                    output_schema={
                        "value": troupe.act_schema.Int64Value(
                            description="the completed task value"
                        )
                    },
                )
            except troupe.AgentSessionBrokenError as error:
                errors.append(error)
            return ()

    class ClaudePlanProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                ClaudePlanActor,
                name="claude-plan-not-restored",
                agent_profile=_profile(workspace, agent="claude"),
                actor_args=(),
                actor_kwargs={},
            )
            first = asyncio.create_task(handle.cue({}))
            while True:
                state = handle._agent_state_for_test()  # type: ignore[attr-defined]
                permission_answered = any(
                    row["event"] == "claude_unrestored_permission_response_received"
                    for row in _events(events)
                )
                if state == "broken" or (state == "active" and permission_answered):
                    break
                await asyncio.sleep(0)
            assert handle._agent_state_for_test() == "active"  # type: ignore[attr-defined]
            await _signal_fifo(release)
            assert await first == ()
            states.append(handle._agent_state_for_test())  # type: ignore[attr-defined]
            assert await handle.cue({}) == ()
            states.append(handle._agent_state_for_test())  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ClaudePlanProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert [error.code for error in errors] == [
        "protocol_violation",
        "protocol_violation",
    ]
    assert states == ["broken", "broken"]
    assert sum(row["event"] == "prompt_received" for row in _events(events)) == 1


def test_claude_cannot_restore_plan_mode_after_the_prompt_response(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    restore = workspace / "restore.fifo"
    os.mkfifo(restore)
    _configure_mock(
        events,
        scenario="claude_plan_mode_restored_after_response",
        release=restore,
    )
    _native()._agent_test_hold_turn_settlement()
    errors: list[troupe.AgentSessionBrokenError] = []
    results: list[dict[str, object]] = []
    states: list[str] = []
    runtime = _native()._Runtime()

    class ClaudeLateRestoreActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                results.append(
                    await self.act(
                        script="Complete the task after leaving plan mode.",
                        output_schema={
                            "value": troupe.act_schema.Int64Value(
                                description="the completed task value"
                            )
                        },
                    )
                )
            except troupe.AgentSessionBrokenError as error:
                errors.append(error)
            return ()

    class ClaudeLateRestoreProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                ClaudeLateRestoreActor,
                name="claude-late-plan-restore",
                agent_profile=_profile(workspace, agent="claude"),
                actor_args=(),
                actor_kwargs={},
            )
            first = asyncio.create_task(handle.cue({}))
            while not _native()._agent_test_turn_gate_states()["settlement"]["arrived"]:
                await asyncio.sleep(0)
            await _signal_fifo(restore)
            while (
                handle._agent_state_for_test() == "active"  # type: ignore[attr-defined]
                and handle._agent_mode_transition_for_test() != "stable"  # type: ignore[attr-defined]
            ):
                await asyncio.sleep(0)
            _native()._agent_test_release_turn_settlement()
            assert await first == ()
            states.append(handle._agent_state_for_test())  # type: ignore[attr-defined]
            assert await handle.cue({}) == ()
            states.append(handle._agent_state_for_test())  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ClaudeLateRestoreProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert results == []
    assert [error.code for error in errors] == [
        "protocol_violation",
        "protocol_violation",
    ]
    assert states == ["broken", "broken"]
    assert sum(row["event"] == "prompt_received" for row in _events(events)) == 1


def test_claude_allows_an_absent_effort_option_when_effort_is_unspecified(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, scenario="claude_effort_option_absent")

    async def scenario() -> None:
        production = troupe.Production([])
        handle = production.cast_actor(
            SessionActor,
            name="claude-without-effort-option",
            agent_profile=_profile(workspace, agent="claude", effort=None),
            actor_args=(),
            actor_kwargs={},
        )

        ready = await _ready(handle)
        assert ready["state"] == "ready"
        assert ready["model"] == "test-model"
        assert ready["effort"] is None
        assert [
            row["config_id"]
            for row in _events(events)
            if row["event"] == "config_applied"
        ] == ["mode", "model"]

        await production._agent_shutdown_for_test()  # type: ignore[attr-defined]

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_claude_rate_limit_is_authoritative_and_keeps_the_session(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(
        events,
        scenario="claude_rate_limit_then_success",
        authoritative_prompt_error_codes=[],
    )
    errors: list[troupe.AgentTurnError] = []
    observed: list[dict[str, object]] = []
    states: list[str] = []
    runtime = _native()._Runtime()

    class ClaudeRateLimitActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            try:
                observed.append(
                    await self.act(
                        script=f"Complete Claude turn {cue.instruction['turn']}.",
                        output_schema={
                            "value": troupe.act_schema.Int64Value(
                                description="the task value"
                            )
                        },
                    )
                )
            except troupe.AgentTurnError as error:
                errors.append(error)
            return ()

    class ClaudeRateLimitProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                ClaudeRateLimitActor,
                name="claude-rate-limit",
                agent_profile=_profile(workspace, agent="claude"),
                actor_args=(),
                actor_kwargs={},
            )
            for turn in (1, 2):
                assert await handle.cue({"turn": turn}) == ()
                states.append(handle._agent_state_for_test())  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ClaudeRateLimitProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert [error.code for error in errors] == ["request_failed"]
    assert observed == [{"value": 7}]
    assert states == ["ready", "ready"]
    prompts = [row for row in _events(events) if row["event"] == "prompt_received"]
    assert len(prompts) == 2
    assert len({row["session_id"] for row in prompts}) == 1


@pytest.mark.parametrize(
    ("scenario_name", "expected_code"),
    [
        ("claude_authentication_lost", "authentication_lost"),
        ("claude_uncertain_prompt_error", "uncertain_settlement"),
    ],
)
def test_claude_non_authoritative_errors_break_and_latch_the_session(
    tmp_path: Path,
    scenario_name: str,
    expected_code: str,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(
        events,
        scenario=scenario_name,
        authoritative_prompt_error_codes=[],
    )
    errors: list[troupe.AgentSessionBrokenError] = []
    states: list[str] = []
    runtime = _native()._Runtime()

    class ClaudeBrokenActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="Attempt the Claude task.",
                    output_schema={
                        "value": troupe.act_schema.Int64Value(
                            description="the task value"
                        )
                    },
                )
            except troupe.AgentSessionBrokenError as error:
                errors.append(error)
            return ()

    class ClaudeBrokenProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                ClaudeBrokenActor,
                name=f"claude-broken-{scenario_name}",
                agent_profile=_profile(workspace, agent="claude"),
                actor_args=(),
                actor_kwargs={},
            )
            for turn in (1, 2):
                assert await handle.cue({"turn": turn}) == ()
                states.append(handle._agent_state_for_test())  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ClaudeBrokenProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert [error.code for error in errors] == [expected_code, expected_code]
    assert states == ["broken", "broken"]
    assert sum(row["event"] == "prompt_received" for row in _events(events)) == 1


def test_claude_synthetic_cancel_breaks_and_latches_without_replacement(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    settlement_release = workspace / "settlement.fifo"
    os.mkfifo(settlement_release)
    _configure_mock(
        events,
        scenario="claude_synthetic_cancel_detached_descendant",
        release=settlement_release,
    )
    errors: list[troupe.AgentSessionBrokenError] = []
    runtime = _native()._Runtime()

    class ClaudeCancelActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="Run until cancelled.",
                    output_schema={
                        "value": troupe.act_schema.Int64Value(
                            description="the task value"
                        )
                    },
                )
            except troupe.AgentSessionBrokenError as error:
                errors.append(error)
            return ()

    class ClaudeCancelProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                ClaudeCancelActor,
                name="claude-synthetic-cancel",
                agent_profile=_profile(workspace, agent="claude"),
                actor_args=(),
                actor_kwargs={},
            )
            first = asyncio.create_task(handle.cue({"turn": 1}))
            await _wait_for_event(events, "prompt_received")
            await _wait_for_event(events, "descendant_started")
            descendant_pid = int(
                next(
                    row["descendant_pid"]
                    for row in _events(events)
                    if row["event"] == "descendant_started"
                )
            )
            try:
                assert _process_is_running(descendant_pid)
                assert first.cancel()
                with pytest.raises(asyncio.CancelledError):
                    await first
                await _wait_for_event(events, "cancel_received")
                assert handle._agent_state_for_test() == "cancelling"  # type: ignore[attr-defined]

                second = asyncio.create_task(handle.cue({"turn": 2}))
                await asyncio.sleep(0)
                assert not second.done()
                await _signal_fifo(settlement_release)
                assert await second == ()
                assert handle._agent_state_for_test() == "broken"  # type: ignore[attr-defined]
                await asyncio.wait_for(_wait_for_process_exit(descendant_pid), 1.0)
                assert await handle.cue({"turn": 3}) == ()
            finally:
                if _process_is_running(descendant_pid):
                    os.kill(descendant_pid, 9)
                runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ClaudeCancelProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert [error.code for error in errors] == [
        "uncertain_settlement",
        "uncertain_settlement",
    ]
    rows = _events(events)
    assert sum(row["event"] == "prompt_received" for row in rows) == 1
    assert sum(row["event"] == "session_new_received" for row in rows) == 1


def test_claude_cancel_racing_with_normal_end_turn_keeps_the_session(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    settlement_release = workspace / "settlement.fifo"
    os.mkfifo(settlement_release)
    _configure_mock(
        events,
        scenario="claude_cancel_end_turn_race",
        release=settlement_release,
    )
    observed: list[dict[str, object]] = []
    runtime = _native()._Runtime()

    class ClaudeCancelRaceActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            observed.append(
                await self.act(
                    script=f"Complete turn {cue.instruction['turn']}.",
                    output_schema={
                        "value": troupe.act_schema.Int64Value(
                            description="the task value"
                        )
                    },
                )
            )
            return ()

    class ClaudeCancelRaceProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                ClaudeCancelRaceActor,
                name="claude-cancel-end-turn-race",
                agent_profile=_profile(workspace, agent="claude"),
                actor_args=(),
                actor_kwargs={},
            )
            first = asyncio.create_task(handle.cue({"turn": 1}))
            await _wait_for_event(events, "prompt_received")
            assert first.cancel()
            with pytest.raises(asyncio.CancelledError):
                await first
            await _wait_for_event(events, "cancel_received")
            await _signal_fifo(settlement_release)
            assert await handle.cue({"turn": 2}) == ()
            assert handle._agent_state_for_test() == "ready"  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ClaudeCancelRaceProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert observed == [{"value": 2}]
    prompts = [row for row in _events(events) if row["event"] == "prompt_received"]
    assert len(prompts) == 2
    assert len({row["session_id"] for row in prompts}) == 1


def test_kimi_adapter_handles_pinned_permissions_and_typed_updates(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, scenario="kimi_permission_matrix", provider="kimi")
    observed: list[dict[str, object]] = []
    runtime = _native()._Runtime()

    class KimiPermissionActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            observed.append(
                await self.act(
                    script="Complete the task without human interaction.",
                    output_schema={
                        "value": troupe.act_schema.Int64Value(
                            description="the completed task value"
                        )
                    },
                )
            )
            return ()

    class KimiPermissionProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                KimiPermissionActor,
                name="kimi-permissions",
                agent_profile=_profile(workspace, agent="kimi"),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(KimiPermissionProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert observed == [{"value": 11}]
    rows = _events(events)
    responses = [
        row for row in rows if row["event"] == "kimi_permission_response_received"
    ]
    assert [row["permission_case"] for row in responses] == [
        "tool",
        "question",
        "plan_review",
        "plan_options",
        "unknown",
        "ambiguous",
    ]
    assert [
        row["config_id"] for row in rows if row["event"] == "config_applied"
    ] == ["mode", "model", "thinking"]
    assert [
        row["update"] for row in rows if row["event"] == "kimi_typed_update_sent"
    ] == [
        "agent_thought_chunk",
        "agent_message_chunk",
        "tool_call",
        "tool_call_update",
        "available_commands_update",
    ]


def test_kimi_allows_absent_thinking_option_when_effort_is_unspecified(
    tmp_path: Path,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(
            events,
            scenario="kimi_thinking_option_absent",
            provider="kimi",
        )

        handle = troupe.Production([]).cast_actor(
            SessionActor,
            name="kimi-without-thinking-option",
            agent_profile=_profile(workspace, agent="kimi", effort=None),
            actor_args=(),
            actor_kwargs={},
        )
        ready = await _ready(handle)

        assert ready["state"] == "ready"
        assert ready["model"] == "test-model"
        assert ready["effort"] is None
        assert [
            row["config_id"]
            for row in _events(events)
            if row["event"] == "config_applied"
        ] == ["mode", "model"]

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


@pytest.mark.parametrize(
    ("scenario_name", "expected_config_ids"),
    [
        ("kimi_model_domain_invalid", ["mode"]),
        ("kimi_thinking_domain_invalid", ["mode", "model"]),
    ],
)
def test_kimi_rejects_invalid_model_and_thinking_domains_during_opening(
    tmp_path: Path,
    scenario_name: str,
    expected_config_ids: list[str],
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(events, scenario=scenario_name, provider="kimi")

        handle = troupe.Production([]).cast_actor(
            SessionActor,
            name=f"kimi-invalid-{scenario_name}",
            agent_profile=_profile(workspace, agent="kimi"),
            actor_args=(),
            actor_kwargs={},
        )
        for _ in range(2):
            with pytest.raises(troupe.AgentSessionStartError) as raised:
                await _ready(handle)
            assert raised.value.code == "configuration_invalid"
            assert raised.value.phase == "configure"
        assert [
            row["config_id"]
            for row in _events(events)
            if row["event"] == "config_applied"
        ] == expected_config_ids
        assert not any(
            row["event"] == "prompt_received" for row in _events(events)
        )

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


@pytest.mark.parametrize(
    "scenario_name",
    ["kimi_agent_version_mismatch", "kimi_agent_info_missing"],
)
def test_kimi_rejects_an_unpinned_agent_info_version_during_initialize(
    tmp_path: Path,
    scenario_name: str,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(
            events,
            scenario=scenario_name,
            provider="kimi",
        )

        handle = troupe.Production([]).cast_actor(
            SessionActor,
            name="kimi-version-mismatch",
            agent_profile=_profile(workspace, agent="kimi"),
            actor_args=(),
            actor_kwargs={},
        )
        for _ in range(2):
            with pytest.raises(troupe.AgentSessionStartError) as raised:
                await _ready(handle)
            assert raised.value.code == "protocol_incompatible"
            assert raised.value.phase == "initialize"
        assert not any(
            row["event"] == "acp_request" and row["method"] == "session/new"
            for row in _events(events)
        )

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_kimi_rejects_unsupported_reverse_request_without_losing_the_turn(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(
        events,
        scenario="kimi_unsupported_reverse_request",
        provider="kimi",
    )
    observed: list[dict[str, object]] = []
    runtime = _native()._Runtime()

    class KimiUnsupportedRequestActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            observed.append(
                await self.act(
                    script="Complete the task without a terminal reverse request.",
                    output_schema={
                        "value": troupe.act_schema.Int64Value(
                            description="the completed task value"
                        )
                    },
                )
            )
            return ()

    class KimiUnsupportedRequestProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                KimiUnsupportedRequestActor,
                name="kimi-unsupported-reverse-request",
                agent_profile=_profile(workspace, agent="kimi"),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            assert handle._agent_state_for_test() == "ready"  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(KimiUnsupportedRequestProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert observed == [{"value": 12}]
    assert [
        (row["code"], row["message"])
        for row in _events(events)
        if row["event"] == "kimi_unsupported_reverse_response_received"
    ] == [(-32601, "Method not found")]


def test_kimi_unsupported_reverse_request_after_terminal_breaks_the_session(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(
        events,
        scenario="kimi_terminal_then_unsupported_reverse_batch",
        provider="kimi",
    )
    _native()._agent_test_hold_turn_response_flush()
    runtime = _native()._Runtime()
    results: list[dict[str, object]] = []
    failures: list[troupe.AgentSessionBrokenError] = []

    class KimiLateUnsupportedActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                results.append(
                    await self.act(
                        script="Remember the token alpha.",
                        output_schema={
                            "stored": troupe.act_schema.StrValue(
                                description="the remembered token",
                                choices=["alpha"],
                            )
                        },
                    )
                )
            except troupe.AgentSessionBrokenError as error:
                failures.append(error)
            return ()

    class KimiLateUnsupportedProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                KimiLateUnsupportedActor,
                name="kimi-late-unsupported-reverse",
                agent_profile=_profile(workspace, agent="kimi"),
                actor_args=(),
                actor_kwargs={},
            )
            first = asyncio.create_task(handle.cue({}))
            await _wait_for_event(events, "terminal_then_unsupported_reverse_batch_sent")
            for _ in range(1_000):
                if _native()._agent_test_turn_gate_states()["response_flush"][
                    "arrived"
                ]:
                    break
                await asyncio.sleep(0)
            assert _native()._agent_test_turn_gate_states()["response_flush"][
                "arrived"
            ]
            for _ in range(1_000):
                if first.done():
                    break
                await asyncio.sleep(0)
            try:
                assert not first.done()
            finally:
                _native()._agent_test_release_turn_response_flush()
            assert await first == ()
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(KimiLateUnsupportedProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert results == []
    assert [failure.code for failure in failures] == [
        "protocol_violation",
        "protocol_violation",
    ]


def test_kimi_authoritative_cancelled_response_keeps_the_same_session(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    settlement_release = workspace / "settlement.fifo"
    os.mkfifo(settlement_release)
    _configure_mock(
        events,
        scenario="act_cancel_then_reuse",
        provider="kimi",
        release=settlement_release,
    )
    observed: list[dict[str, object]] = []
    runtime = _native()._Runtime()

    class KimiCancelActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            observed.append(
                await self.act(
                    script=f"Complete Kimi turn {cue.instruction['turn']}.",
                    output_schema={
                        "value": troupe.act_schema.Int64Value(
                            description="the completed turn number"
                        )
                    },
                )
            )
            return ()

    class KimiCancelProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                KimiCancelActor,
                name="kimi-cancel-reuse",
                agent_profile=_profile(workspace, agent="kimi"),
                actor_args=(),
                actor_kwargs={},
            )
            first = asyncio.create_task(handle.cue({"turn": 1}))
            await _wait_for_event(events, "prompt_received")
            assert first.cancel()
            with pytest.raises(asyncio.CancelledError):
                await first
            await _wait_for_event(events, "cancel_received")
            await _signal_fifo(settlement_release)
            assert await handle.cue({"turn": 2}) == ()
            assert handle._agent_state_for_test() == "ready"  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(KimiCancelProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())

    assert observed == [{"value": 2}]
    prompts = [row for row in _events(events) if row["event"] == "prompt_received"]
    assert len(prompts) == 2
    assert len({row["session_id"] for row in prompts}) == 1


@pytest.mark.parametrize(
    ("scenario_name", "expected_code", "expected_phase"),
    [
        ("oversized_acp_frame", "resource_limit", "initialize"),
        ("oversized_acp_frame_session_new", "resource_limit", "session_new"),
        ("oversized_acp_frame_configure", "resource_limit", "configure"),
        ("no_http_mcp", "protocol_incompatible", "initialize"),
        ("session_new_error", "protocol_incompatible", "session_new"),
        ("configuration_invalid", "configuration_invalid", "configure"),
        ("mode_drift_after_model", "configuration_invalid", "configure"),
        ("mode_domain_invalid", "configuration_invalid", "configure"),
        ("model_domain_invalid", "configuration_invalid", "configure"),
        ("effort_domain_invalid", "configuration_invalid", "configure"),
    ],
)
def test_opening_failure_is_latched_with_its_startup_code_and_phase(
    tmp_path: Path,
    scenario_name: str,
    expected_code: str,
    expected_phase: str,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(events, scenario=scenario_name)

        handle = _cast(troupe.Production([]), workspace, "classified-failure")
        for _ in range(2):
            with pytest.raises(troupe.AgentSessionStartError) as raised:
                await _ready(handle)
            assert type(raised.value) is troupe.AgentSessionStartError
            assert raised.value.code == expected_code
            assert raised.value.phase == expected_phase

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_opening_acp_resource_limit_uses_mcp_ready_phase_after_configuration(
    tmp_path: Path,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        release = tmp_path / "release-mcp-ready-frame"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(
            events,
            scenario="oversized_acp_frame_mcp_ready",
            release=release,
        )
        _native()._agent_test_hold_configuration_ready()
        _native()._agent_test_hold_mcp_ready()

        handle = _cast(troupe.Production([]), workspace, "mcp-ready-resource")
        await _wait_for_event(events, "mcp_ready_frame_blocked")
        await _wait_for_readiness_gate("configuration", "arrived")
        await _wait_for_readiness_gate("mcp", "arrived")
        _native()._agent_test_release_configuration_ready()
        await _wait_for_readiness_gate("configuration", "completed")
        release.touch()

        for _ in range(2):
            with pytest.raises(troupe.AgentSessionStartError) as raised:
                await _ready(handle)
            assert raised.value.code == "resource_limit"
            assert raised.value.phase == "mcp_ready"

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


@pytest.mark.parametrize("depth", [63, 64, 65])
def test_opening_acp_protocol_depth_has_exact_resource_boundary(
    tmp_path: Path,
    depth: int,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(events, scenario=f"opening_acp_depth_{depth}")

        handle = _cast(troupe.Production([]), workspace, f"opening-depth-{depth}")
        if depth <= 64:
            ready = await _ready(handle)
            assert ready["state"] == "ready"
            assert handle._agent_state_for_test() == "ready"  # type: ignore[attr-defined]
        else:
            with pytest.raises(troupe.AgentSessionStartError) as raised:
                await _ready(handle)
            assert raised.value.code == "resource_limit"
            assert raised.value.phase == "initialize"
            assert handle._agent_state_for_test() == "start_failed"  # type: ignore[attr-defined]

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_opening_background_spawn_failure_is_a_latched_start_error(tmp_path: Path) -> None:
    async def scenario() -> None:
        executable = tmp_path / "invalid-executable"
        executable.write_text("not an executable image", encoding="utf-8")
        executable.chmod(0o700)
        _native()._agent_test_set_launch(
            program=str(executable),
            args=[],
        )
        _native()._agent_test_hold_opening()
        workspace = tmp_path / "workspace"
        workspace.mkdir()

        handle = _cast(troupe.Production([]), workspace, "spawn-failure")
        executable.unlink()
        _native()._agent_test_release_opening()
        for _ in range(2):
            with pytest.raises(troupe.AgentSessionStartError) as raised:
                await _ready(handle)
            assert raised.value.code == "spawn_failed"
            assert raised.value.phase == "spawn"
        assert handle._agent_state_for_test() == "start_failed"  # type: ignore[attr-defined]

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_capability_destruction_kills_opening_process(tmp_path: Path) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        release = tmp_path / "never-release"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(events, scenario="hold_before_ready", release=release)

        production = troupe.Production([])
        handle = _cast(production, workspace, "closing")
        await _wait_for_event(events, "ready_blocked")
        pid = next(row["pid"] for row in _events(events) if row["event"] == "process_started")
        del handle
        for _ in range(3):
            gc.collect()
        while True:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                break
            await asyncio.sleep(0)
        assert production.get_actor("closing") is None

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_opening_production_shutdown_reaps_child_and_rejects_later_cast(
    tmp_path: Path,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        release = tmp_path / "never-release"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(events, scenario="hold_before_ready", release=release)

        production = troupe.Production([])
        opening = _cast(production, workspace, "shutdown-opening")
        await _wait_for_event(events, "ready_blocked")
        pid = next(row["pid"] for row in _events(events) if row["event"] == "process_started")
        endpoint = urlsplit(
            next(row["url"] for row in _events(events) if row["event"] == "session_new_received")
        )

        shutdown = asyncio.ensure_future(production._agent_shutdown_for_test())  # type: ignore[attr-defined]
        while not production._agent_is_shutting_down_for_test():  # type: ignore[attr-defined]
            await asyncio.sleep(0)
        with pytest.raises(troupe.AgentSessionStartError) as raised:
            _cast(production, workspace, "shutdown-rejected")
        assert raised.value.code == "preparation_failed"
        assert raised.value.phase == "preparation"
        await shutdown

        with pytest.raises(troupe.AgentSessionStartError):
            await _ready(opening)
        with pytest.raises(ProcessLookupError):
            os.kill(pid, 0)
        with socket.socket() as probe:
            assert probe.connect_ex((endpoint.hostname, endpoint.port)) != 0
        assert production.get_actor("shutdown-rejected") is None

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_ready_production_shutdown_releases_workspace_fd_while_handle_lives(
    tmp_path: Path,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(events)
        assert _open_directory_fds(workspace) == []

        production = troupe.Production([])
        handle = _cast(production, workspace, "shutdown-ready-fd")
        await _ready(handle)
        assert len(_open_directory_fds(workspace)) == 1

        await production._agent_shutdown_for_test()  # type: ignore[attr-defined]

        assert handle.name == "shutdown-ready-fd"
        assert _open_directory_fds(workspace) == []

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_cast_unavailable_launcher_fails_before_actor_publication(tmp_path: Path) -> None:
    missing = tmp_path / "missing-launcher"
    _native()._agent_test_set_launch(
        program=str(missing),
        args=[],
    )
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    production = troupe.Production([])
    constructor_calls = 0

    class CountingActor(troupe.Actor):
        def __init__(self) -> None:
            nonlocal constructor_calls
            constructor_calls += 1

    with pytest.raises(troupe.AgentSessionStartError) as raised:
        production.cast_actor(
            CountingActor,
            name="unavailable",
            agent_profile=_profile(workspace),
            actor_args=(),
            actor_kwargs={},
        )
    assert raised.value.code == "launcher_unavailable"
    assert raised.value.phase == "preparation"
    assert constructor_calls == 0
    assert production.get_actor("unavailable") is None


def test_workspace_rename_replace_fails_the_next_opening_checkpoint(
    tmp_path: Path,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        (workspace / "identity").write_text("original", encoding="utf-8")
        _configure_mock(events)
        _native()._agent_test_hold_opening()

        handle = _cast(troupe.Production([]), workspace, "workspace-replaced")
        moved = tmp_path / "moved-workspace"
        workspace.rename(moved)
        workspace.mkdir()
        (workspace / "identity").write_text("replacement", encoding="utf-8")
        _native()._agent_test_release_opening()

        with pytest.raises(troupe.AgentSessionStartError) as raised:
            await _ready(handle)
        assert raised.value.code == "preparation_failed"
        assert raised.value.phase == "preparation"
        assert not events.exists()

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_opening_workspace_rename_replace_after_spawn_keeps_child_on_lease_and_stops_before_session_new(
    tmp_path: Path,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        release = tmp_path / "release-initialize"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        original = workspace.stat()
        _configure_mock(
            events,
            scenario="hold_initialize",
            release=release,
        )

        handle = _cast(troupe.Production([]), workspace, "workspace-after-spawn")
        await _wait_for_event(events, "initialize_blocked")
        process = next(row for row in _events(events) if row["event"] == "process_started")
        assert (process["cwd_dev"], process["cwd_ino"]) == (
            original.st_dev,
            original.st_ino,
        )

        moved = tmp_path / "moved-workspace"
        workspace.rename(moved)
        workspace.mkdir()
        replacement = workspace.stat()
        assert (replacement.st_dev, replacement.st_ino) != (
            process["cwd_dev"],
            process["cwd_ino"],
        )
        release.touch()

        with pytest.raises(troupe.AgentSessionStartError) as raised:
            await _ready(handle)
        assert raised.value.code == "preparation_failed"
        assert raised.value.phase == "session_new"
        assert not any(
            row["event"] == "session_new_received" for row in _events(events)
        )

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))


def test_opening_session_survives_runtime_rebind_and_actor_destruction_reaps_it(
    tmp_path: Path,
) -> None:
    async def scenario() -> None:
        events = tmp_path / "events.jsonl"
        release = tmp_path / "release-opening"
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        _configure_mock(events, scenario="hold_before_ready", release=release)

        class ReboundProduction(troupe.Production):
            def __init__(self, args: list[str]) -> None:
                self.runtime: Any = None

            async def scene(self) -> None:
                await _wait_for_event(events, "ready_blocked")
                self.runtime.request_shutdown()

        production = ReboundProduction([])
        handle = _cast(production, workspace, "rebound-opening")
        first_runtime = _native()._Runtime()
        production.runtime = first_runtime
        await first_runtime.run(production)
        assert handle._agent_state_for_test() == "opening"  # type: ignore[attr-defined]

        release.touch()
        first_ready = await _ready(handle)
        second_runtime = _native()._Runtime()
        production.runtime = second_runtime
        await second_runtime.run(production)
        second_ready = await _ready(handle)
        assert second_ready == first_ready
        assert sum(
            row["event"] == "process_started" for row in _events(events)
        ) == 1

        pid = first_ready["pid"]
        del handle
        del production
        for _ in range(3):
            gc.collect()
        while True:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                break
            await asyncio.sleep(0)

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))
