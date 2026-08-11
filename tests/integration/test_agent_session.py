from __future__ import annotations

import asyncio
import gc
import importlib
import json
import os
import socket
import sys
from pathlib import Path
from typing import Any
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


def _profile(workspace: Path, *, effort: str | None = "max") -> troupe.AgentProfile:
    return troupe.AgentProfile(
        agent="codex",
        workspace=workspace,
        model="test-model",
        effort=effort,
    )


def _configure_mock(
    events: Path,
    *,
    scenario: str = "ready",
    release: Path | None = None,
    legacy_mode_id: str | None = None,
) -> None:
    args = [str(MOCK_AGENT), "--events", str(events), "--scenario", scenario]
    if release is not None:
        args.extend(["--release", str(release)])
    launch: dict[str, object] = {
        "program": sys.executable,
        "args": args,
    }
    if legacy_mode_id is not None:
        launch["legacy_mode_id"] = legacy_mode_id
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
