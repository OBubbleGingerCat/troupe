from __future__ import annotations

import asyncio
import importlib
import json
import os
import signal
import socket
import sys
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

import pytest

import troupe


ROOT = Path(__file__).resolve().parents[2]
MOCK_AGENT = ROOT / "tests" / "support" / "mock_acp_agent.py"
HARNESS_TIMEOUT = 8.0


def _native() -> Any:
    return importlib.import_module("troupe._runtime")


def _events(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def _process_is_running(pid: int) -> bool:
    try:
        status = Path(f"/proc/{pid}/stat").read_text(encoding="ascii")
    except (FileNotFoundError, ProcessLookupError):
        return False
    state = status[status.rfind(")") + 2 :].split(maxsplit=1)[0]
    return state not in {"X", "Z"}


async def _wait_for_attempts(events: Path, count: int) -> None:
    while sum(row["event"] == "process_started" for row in _events(events)) < count:
        await asyncio.sleep(0)


async def _wait_for_event(path: Path, event: str) -> None:
    while not any(row["event"] == event for row in _events(path)):
        await asyncio.sleep(0)


async def _wait_for_process_reap(pid: int) -> None:
    while Path(f"/proc/{pid}").exists():
        await asyncio.sleep(0)


async def _signal_fifo(path: Path) -> None:
    def write() -> None:
        with path.open("wb", buffering=0) as signal:
            signal.write(b"1")

    await asyncio.to_thread(write)


async def _wait_for_backoff_arrivals(count: int) -> dict[str, object]:
    while True:
        state = _native()._agent_test_opening_backoff_state()
        if state["arrivals"] >= count:
            return state
        await asyncio.sleep(0)


async def _wait_for_readiness_gate(latch: str, state: str) -> None:
    while not _native()._agent_test_readiness_gate_states()[latch][state]:
        await asyncio.sleep(0)


def _configure_mock(
    events: Path,
    scenario: str,
    attempt_file: Path,
    release: Path | None = None,
    transient_opening_errors: list[tuple[str, int]] | None = None,
) -> None:
    args = [
        str(MOCK_AGENT),
        "--events",
        str(events),
        "--scenario",
        scenario,
        "--attempt-file",
        str(attempt_file),
    ]
    if release is not None:
        args.extend(["--release", str(release)])
    launch: dict[str, object] = {
        "program": sys.executable,
        "args": args,
    }
    if transient_opening_errors is not None:
        launch["transient_opening_errors"] = transient_opening_errors
    _native()._agent_test_set_launch(
        **launch,
    )


def _profile(workspace: Path) -> troupe.AgentProfile:
    return troupe.AgentProfile(
        agent="codex",
        workspace=workspace,
        model="test-model",
        effort="max",
    )


@pytest.fixture(autouse=True)
def _reset_test_launch() -> Any:
    _native()._agent_test_reset_launch()
    yield
    _native()._agent_test_reset_launch()


def test_ambiguous_opening_crashes_retry_only_after_each_backoff_release(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, "opening_crash_twice_then_ready", attempts)
    _native()._agent_test_hold_opening_backoff(random_words=[126, 251])
    runtime = _native()._Runtime()
    snapshots: list[dict[str, object]] = []

    class RetryProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                troupe.Actor,
                name="opening-retry",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            first_backoff = await _wait_for_backoff_arrivals(1)
            assert first_backoff["delays_ms"] == [125]
            assert len([row for row in _events(events) if row["event"] == "process_started"]) == 1

            _native()._agent_test_release_opening_backoff()
            await _wait_for_attempts(events, 2)
            second_backoff = await _wait_for_backoff_arrivals(2)
            assert second_backoff["delays_ms"] == [125, 250]
            assert len([row for row in _events(events) if row["event"] == "process_started"]) == 2

            _native()._agent_test_release_opening_backoff()
            snapshots.append(await handle._agent_ready_for_test())  # type: ignore[attr-defined]
            assert len([row for row in _events(events) if row["event"] == "process_started"]) == 3
            await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(RetryProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert len(snapshots) == 1
    assert snapshots[0]["state"] == "ready"
    assert snapshots[0]["generation"] == 3


def test_typed_transient_opening_failure_retries_past_crash_loop_threshold(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(
        events,
        "opening_transient_four_times_then_ready",
        attempts,
        transient_opening_errors=[("session_new", -32099)],
    )
    _native()._agent_test_hold_opening_backoff(
        random_words=[126, 251, 501, 1001]
    )
    runtime = _native()._Runtime()
    snapshots: list[dict[str, object]] = []

    class TransientProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                troupe.Actor,
                name="typed-transient-opening",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            for arrival in range(1, 5):
                state = await _wait_for_backoff_arrivals(arrival)
                assert state["arrivals"] == arrival
                _native()._agent_test_release_opening_backoff()
            snapshots.append(await handle._agent_ready_for_test())  # type: ignore[attr-defined]
            await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(TransientProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert snapshots[0]["generation"] == 5
    assert _native()._agent_test_opening_backoff_state()["delays_ms"] == [
        125,
        250,
        500,
        1000,
    ]


def test_npx_transient_acquisition_retries_past_crash_loop_threshold(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, "npx_transient_four_times_then_ready", attempts)
    _native()._agent_test_hold_opening_backoff(
        random_words=[126, 251, 501, 1001]
    )
    runtime = _native()._Runtime()
    snapshots: list[dict[str, object]] = []

    class NpxTransientProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                troupe.Actor,
                name="npx-transient-opening",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            for arrival in range(1, 5):
                state = await _wait_for_backoff_arrivals(arrival)
                assert state["arrivals"] == arrival
                _native()._agent_test_release_opening_backoff()
            snapshots.append(await handle._agent_ready_for_test())  # type: ignore[attr-defined]
            await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(NpxTransientProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert snapshots[0]["generation"] == 5
    assert _native()._agent_test_opening_backoff_state()["delays_ms"] == [
        125,
        250,
        500,
        1000,
    ]
    assert len([row for row in _events(events) if row["event"] == "process_started"]) == 5


def test_npx_missing_package_failure_is_cached_for_shared_waiters(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, "npx_package_version_missing", attempts)
    runtime = _native()._Runtime()
    failures: list[BaseException] = []

    class NpxMissingProduction(troupe.Production):
        async def scene(self) -> None:
            first = self.cast_actor(
                troupe.Actor,
                name="npx-missing-first",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            second = self.cast_actor(
                troupe.Actor,
                name="npx-missing-second",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            failures.extend(
                await asyncio.gather(
                    first._agent_ready_for_test(),  # type: ignore[attr-defined]
                    second._agent_ready_for_test(),  # type: ignore[attr-defined]
                    return_exceptions=True,
                )
            )
            await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(NpxMissingProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert len(failures) == 2
    assert all(type(error) is troupe.AgentSessionStartError for error in failures)
    assert [error.code for error in failures] == [  # type: ignore[attr-defined]
        "preparation_failed",
        "preparation_failed",
    ]
    assert [error.phase for error in failures] == [  # type: ignore[attr-defined]
        "preparation",
        "preparation",
    ]
    assert len([row for row in _events(events) if row["event"] == "process_started"]) == 1


def test_clean_eof_while_waiting_for_mcp_readiness_starts_backoff(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, "opening_eof_before_mcp_ready", attempts)
    _native()._agent_test_hold_opening_backoff(random_words=[126])
    runtime = _native()._Runtime()

    class EofProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                troupe.Actor,
                name="opening-eof-before-mcp",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            await _wait_for_event(events, "opening_stdout_closing_before_mcp_ready")
            state = await _wait_for_backoff_arrivals(1)
            assert state["delays_ms"] == [125]
            assert handle._agent_state_for_test() == "backing_off"  # type: ignore[attr-defined]
            await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(EofProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())


def test_protocol_error_while_waiting_for_mcp_readiness_is_terminal(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    release = tmp_path / "protocol-error.fifo"
    os.mkfifo(release)
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(
        events,
        "opening_protocol_error_before_mcp_ready",
        attempts,
        release,
    )
    _native()._agent_test_hold_configuration_ready()
    _native()._agent_test_hold_mcp_ready()
    _native()._agent_test_hold_opening_backoff(random_words=[126])
    runtime = _native()._Runtime()
    failures: list[troupe.AgentSessionStartError] = []

    class ProtocolProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                troupe.Actor,
                name="opening-protocol-before-mcp",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            await _wait_for_readiness_gate("configuration", "arrived")
            _native()._agent_test_release_configuration_ready()
            await _wait_for_readiness_gate("configuration", "completed")
            await _signal_fifo(release)
            await _wait_for_event(events, "opening_protocol_error_sent_before_mcp_ready")
            with pytest.raises(troupe.AgentSessionStartError) as raised:
                await handle._agent_ready_for_test()  # type: ignore[attr-defined]
            failures.append(raised.value)
            assert _native()._agent_test_opening_backoff_state()["arrivals"] == 0
            await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ProtocolProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert failures[0].code == "protocol_incompatible"
    assert failures[0].phase == "mcp_ready"


def test_third_identical_ambiguous_opening_crash_latches_fresh_crash_loop_errors(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, "opening_eof_crash_loop", attempts)
    _native()._agent_test_hold_opening_backoff(random_words=[126, 251])
    runtime = _native()._Runtime()
    failures: list[BaseException] = []

    class CrashLoopProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                troupe.Actor,
                name="opening-crash-loop",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            await _wait_for_backoff_arrivals(1)
            _native()._agent_test_release_opening_backoff()
            await _wait_for_backoff_arrivals(2)
            _native()._agent_test_release_opening_backoff()
            await _wait_for_attempts(events, 3)

            outcomes = await asyncio.gather(
                handle._agent_ready_for_test(),  # type: ignore[attr-defined]
                handle._agent_ready_for_test(),  # type: ignore[attr-defined]
                return_exceptions=True,
            )
            failures.extend(outcomes)
            await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(CrashLoopProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert len(failures) == 2
    assert all(type(error) is troupe.AgentSessionStartError for error in failures)
    assert failures[0] is not failures[1]
    assert [error.code for error in failures] == [  # type: ignore[attr-defined]
        "crash_loop",
        "crash_loop",
    ]
    assert [error.phase for error in failures] == [  # type: ignore[attr-defined]
        "initialize",
        "initialize",
    ]
    assert len([row for row in _events(events) if row["event"] == "process_started"]) == 3


def test_silent_opening_never_enters_backoff(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    release = tmp_path / "initialize.release"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _native()._agent_test_set_launch(
        program=sys.executable,
        args=[
            str(MOCK_AGENT),
            "--events",
            str(events),
            "--scenario",
            "hold_initialize",
            "--release",
            str(release),
            "--attempt-file",
            str(attempts),
        ],
    )
    _native()._agent_test_hold_opening_backoff(random_words=[126])
    runtime = _native()._Runtime()

    class SilentProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                troupe.Actor,
                name="silent-opening",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            while not any(row["event"] == "initialize_blocked" for row in _events(events)):
                await asyncio.sleep(0)
            for _ in range(10):
                await asyncio.sleep(0)
            state = _native()._agent_test_opening_backoff_state()
            assert state["arrivals"] == 0
            assert state["delays_ms"] == []
            assert len([row for row in _events(events) if row["event"] == "process_started"]) == 1
            await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
            assert handle._agent_state_for_test() == "closed"  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(SilentProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())


def test_post_ready_exact_config_snapshot_keeps_the_same_session(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    release = tmp_path / "config.fifo"
    os.mkfifo(release)
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, "post_ready_config_exact", attempts, release)
    runtime = _native()._Runtime()

    class ExactConfigProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                troupe.Actor,
                name="exact-config-update",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            before = await handle._agent_ready_for_test()  # type: ignore[attr-defined]
            await _signal_fifo(release)
            await _wait_for_event(events, "post_ready_config_update_sent")
            for _ in range(10):
                await asyncio.sleep(0)
            after = await handle._agent_ready_for_test()  # type: ignore[attr-defined]
            assert after == before
            assert handle._agent_state_for_test() == "ready"  # type: ignore[attr-defined]
            await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ExactConfigProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())


@pytest.mark.parametrize(
    "scenario_name",
    [
        "post_ready_config_model_drift",
        "post_ready_config_model_malformed",
        "post_ready_session_update_malformed",
    ],
)
def test_post_ready_invalid_known_config_latches_protocol_violation_with_fresh_errors(
    tmp_path: Path,
    scenario_name: str,
) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    release = tmp_path / "config.fifo"
    os.mkfifo(release)
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, scenario_name, attempts, release)
    runtime = _native()._Runtime()
    failures: list[troupe.AgentSessionBrokenError] = []

    class BrokenConfigActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="This prompt must never be submitted.",
                    output_schema={
                        "value": troupe.act_schema.Int64Value(
                            description="an unreachable value"
                        )
                    },
                )
            except troupe.AgentSessionBrokenError as error:
                failures.append(error)
            return ()

    class BrokenConfigProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                BrokenConfigActor,
                name="broken-config-update",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            await handle._agent_ready_for_test()  # type: ignore[attr-defined]
            await _signal_fifo(release)
            await _wait_for_event(events, "post_ready_config_update_sent")
            for _ in range(1_000):
                if handle._agent_state_for_test() != "ready":  # type: ignore[attr-defined]
                    break
                await asyncio.sleep(0)
            assert handle._agent_state_for_test() == "broken"  # type: ignore[attr-defined]
            assert await handle.cue({"attempt": 1}) == ()
            assert await handle.cue({"attempt": 2}) == ()
            await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(BrokenConfigProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert len(failures) == 2
    assert failures[0] is not failures[1]
    assert [error.code for error in failures] == [
        "protocol_violation",
        "protocol_violation",
    ]
    assert not any(row["event"] == "prompt_received" for row in _events(events))


def test_config_drift_before_turn_commit_fails_the_current_turn(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    release = tmp_path / "config.fifo"
    os.mkfifo(release)
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, "post_ready_config_during_turn", attempts, release)
    runtime = _native()._Runtime()
    failures: list[troupe.AgentSessionBrokenError] = []

    class DriftDuringTurnActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="Wait while the frozen configuration is checked.",
                    output_schema={
                        "value": troupe.act_schema.Int64Value(
                            description="an unreachable value"
                        )
                    },
                )
            except troupe.AgentSessionBrokenError as error:
                failures.append(error)
            return ()

    class DriftDuringTurnProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                DriftDuringTurnActor,
                name="config-drift-during-turn",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            cue = asyncio.create_task(handle.cue({"attempt": 1}))
            await _wait_for_event(events, "prompt_received")
            await _signal_fifo(release)
            assert await cue == ()
            assert handle._agent_state_for_test() == "broken"  # type: ignore[attr-defined]
            await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(DriftDuringTurnProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert len(failures) == 1
    assert failures[0].code == "protocol_violation"


def test_turn_commit_before_config_drift_preserves_result_and_breaks_session(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    release = tmp_path / "config.fifo"
    os.mkfifo(release)
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, "post_ready_config_after_result", attempts, release)
    _native()._agent_test_hold_turn_outcome()
    runtime = _native()._Runtime()
    results: list[dict[str, object]] = []

    class CommittedBeforeDriftActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            results.append(
                await self.act(
                    script="Remember the token alpha.",
                    output_schema={
                        "stored": troupe.act_schema.StrValue(
                            description="the stored token",
                            choices=["alpha"],
                        )
                    },
                )
            )
            return ()

    class CommittedBeforeDriftProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                CommittedBeforeDriftActor,
                name="commit-before-config-drift",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            cue = asyncio.create_task(handle.cue({"attempt": 1}))
            while not _native()._agent_test_turn_gate_states()["outcome"]["arrived"]:
                await asyncio.sleep(0)
            await _signal_fifo(release)
            while handle._agent_state_for_test() == "ready":  # type: ignore[attr-defined]
                await asyncio.sleep(0)
            _native()._agent_test_release_turn_outcome()
            assert await cue == ()
            assert handle._agent_state_for_test() == "broken"  # type: ignore[attr-defined]
            await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(CommittedBeforeDriftProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert results == [{"stored": "alpha"}]


def test_global_result_listener_loss_breaks_and_cleans_every_ready_session(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, "default", attempts)
    runtime = _native()._Runtime()
    failures: list[troupe.AgentSessionBrokenError] = []
    pids: list[int] = []

    class ListenerLossActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="This prompt must not reach a failed result channel.",
                    output_schema={
                        "value": troupe.act_schema.Int64Value(
                            description="an unreachable value"
                        )
                    },
                )
            except troupe.AgentSessionBrokenError as error:
                failures.append(error)
            return ()

    class ListenerLossProduction(troupe.Production):
        async def scene(self) -> None:
            first = self.cast_actor(
                ListenerLossActor,
                name="listener-loss-first",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            second = self.cast_actor(
                ListenerLossActor,
                name="listener-loss-second",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            ready = await asyncio.gather(
                first._agent_ready_for_test(),  # type: ignore[attr-defined]
                second._agent_ready_for_test(),  # type: ignore[attr-defined]
            )
            pids.extend(int(snapshot["pid"]) for snapshot in ready)
            self._agent_fail_result_listener_for_test()  # type: ignore[attr-defined]
            while any(
                handle._agent_state_for_test() == "ready"  # type: ignore[attr-defined]
                for handle in (first, second)
            ):
                await asyncio.sleep(0)
            assert await first.cue({"attempt": 1}) == ()
            assert await second.cue({"attempt": 1}) == ()
            await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ListenerLossProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert len(failures) == 2
    assert [failure.code for failure in failures] == [
        "result_channel_lost",
        "result_channel_lost",
    ]
    assert not any(row["event"] == "prompt_received" for row in _events(events))
    for pid in pids:
        with pytest.raises(ProcessLookupError):
            os.kill(pid, 0)


@pytest.mark.parametrize(
    ("scenario_name", "expected_code"),
    [
        ("act_post_ready_auth_required", "authentication_lost"),
        ("act_post_ready_uncertain_error", "uncertain_settlement"),
    ],
)
def test_post_ready_terminal_prompt_error_breaks_and_latches_the_session(
    tmp_path: Path,
    scenario_name: str,
    expected_code: str,
) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, scenario_name, attempts)
    runtime = _native()._Runtime()
    failures: list[troupe.AgentSessionBrokenError] = []

    class TerminalErrorActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="Observe a terminal prompt error.",
                    output_schema={
                        "value": troupe.act_schema.Int64Value(
                            description="an unreachable integer"
                        )
                    },
                )
            except troupe.AgentSessionBrokenError as error:
                failures.append(error)
            return ()

    class TerminalErrorProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                TerminalErrorActor,
                name=f"terminal-prompt-{scenario_name}",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({"attempt": 1}) == ()
            assert handle._agent_state_for_test() == "broken"  # type: ignore[attr-defined]
            assert await handle.cue({"attempt": 2}) == ()
            await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(TerminalErrorProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert len(failures) == 2
    assert failures[0] is not failures[1]
    assert [failure.code for failure in failures] == [expected_code, expected_code]
    assert sum(row["event"] == "prompt_received" for row in _events(events)) == 1


def test_production_shutdown_completes_an_active_actor_act_caller(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, "act_cancel_then_reuse", attempts)
    runtime = _native()._Runtime()
    failures: list[troupe.AgentSessionBrokenError] = []

    class PendingActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="Remain active until Production shutdown.",
                    output_schema={
                        "value": troupe.act_schema.Int64Value(
                            description="an unreachable integer"
                        )
                    },
                )
            except troupe.AgentSessionBrokenError as error:
                failures.append(error)
            return ()

    class ShutdownProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                PendingActor,
                name="shutdown-active-act",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            cue = asyncio.create_task(handle.cue({"attempt": 1}))
            await _wait_for_event(events, "prompt_received")
            await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
            assert await cue == ()
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ShutdownProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert len(failures) == 1
    assert failures[0].code == "transport_lost"


@pytest.mark.parametrize(
    ("scenario_name", "shares_agent_process_group"),
    [
        ("spawn_descendant", True),
        ("spawn_detached_descendant", False),
    ],
)
def test_production_shutdown_terminates_the_whole_agent_process_tree(
    tmp_path: Path,
    scenario_name: str,
    shares_agent_process_group: bool,
) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, scenario_name, attempts)
    runtime = _native()._Runtime()

    class ProcessGroupProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                troupe.Actor,
                name="process-group-cleanup",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            ready = await handle._agent_ready_for_test()  # type: ignore[attr-defined]
            agent_pid = int(ready["pid"])
            await _wait_for_event(events, "descendant_started")
            descendant_pid = int(
                next(
                    row["descendant_pid"]
                    for row in _events(events)
                    if row["event"] == "descendant_started"
                )
            )
            assert os.getpgid(agent_pid) == agent_pid
            assert (os.getpgid(descendant_pid) == agent_pid) is shares_agent_process_group

            try:
                await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
                with pytest.raises(ProcessLookupError):
                    os.kill(agent_pid, 0)
                assert not _process_is_running(descendant_pid)
            finally:
                if _process_is_running(descendant_pid):
                    os.kill(descendant_pid, 9)
            assert not _process_is_running(descendant_pid)
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ProcessGroupProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())


def test_agent_process_exit_still_terminates_a_previously_observed_detached_descendant(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    release = tmp_path / "exit.fifo"
    os.mkfifo(release)
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(
        events,
        "spawn_detached_descendant_then_exit",
        attempts,
        release,
    )
    runtime = _native()._Runtime()

    class ProcessExitProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                troupe.Actor,
                name="process-exit-cleanup",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            ready = await handle._agent_ready_for_test()  # type: ignore[attr-defined]
            agent_pid = int(ready["pid"])
            await _wait_for_event(events, "descendant_started")
            descendant_pid = int(
                next(
                    row["descendant_pid"]
                    for row in _events(events)
                    if row["event"] == "descendant_started"
                )
            )
            assert os.getpgid(descendant_pid) != agent_pid

            try:
                await _signal_fifo(release)
                while handle._agent_state_for_test() == "ready":  # type: ignore[attr-defined]
                    await asyncio.sleep(0)
                assert handle._agent_state_for_test() == "broken"  # type: ignore[attr-defined]
                await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
                assert not _process_is_running(descendant_pid)
            finally:
                if _process_is_running(descendant_pid):
                    os.kill(descendant_pid, 9)
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ProcessExitProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())


def test_pre_ready_root_exit_cannot_orphan_an_unobserved_detached_descendant(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    release = tmp_path / "exit.fifo"
    os.mkfifo(release)
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(
        events,
        "spawn_unobserved_detached_before_initialize_then_exit",
        attempts,
        release,
    )
    _native()._agent_test_hold_opening_backoff(random_words=[126])
    runtime = _native()._Runtime()

    class PreReadyExitProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                troupe.Actor,
                name="pre-ready-detached-exit",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            await _wait_for_event(events, "descendant_started")
            descendant_pid = int(
                next(
                    row["descendant_pid"]
                    for row in _events(events)
                    if row["event"] == "descendant_started"
                )
            )
            try:
                await _signal_fifo(release)
                await _wait_for_event(events, "process_exiting_before_initialize")
                await _wait_for_backoff_arrivals(1)
                await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
                assert not _process_is_running(descendant_pid)
            finally:
                if _process_is_running(descendant_pid):
                    os.kill(descendant_pid, signal.SIGKILL)
            del handle
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(PreReadyExitProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())


def test_post_ready_root_exit_cannot_orphan_a_new_detached_descendant(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    release = tmp_path / "exit.fifo"
    os.mkfifo(release)
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(
        events,
        "spawn_late_detached_descendant_then_exit",
        attempts,
        release,
    )
    runtime = _native()._Runtime()

    class PostReadyExitProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                troupe.Actor,
                name="post-ready-detached-exit",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            await handle._agent_ready_for_test()  # type: ignore[attr-defined]
            await _signal_fifo(release)
            await _wait_for_event(events, "descendant_started")
            descendant_pid = int(
                next(
                    row["descendant_pid"]
                    for row in _events(events)
                    if row["event"] == "descendant_started"
                )
            )
            try:
                while handle._agent_state_for_test() == "ready":  # type: ignore[attr-defined]
                    await asyncio.sleep(0)
                assert handle._agent_state_for_test() == "broken"  # type: ignore[attr-defined]
                await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
                assert not _process_is_running(descendant_pid)
            finally:
                if _process_is_running(descendant_pid):
                    os.kill(descendant_pid, signal.SIGKILL)
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(PostReadyExitProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())


def test_guardian_reaps_an_adopted_orphan_while_the_agent_remains_ready(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, "spawn_short_lived_orphan", attempts)
    runtime = _native()._Runtime()

    class OrphanReapProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                troupe.Actor,
                name="short-lived-orphan",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            await handle._agent_ready_for_test()  # type: ignore[attr-defined]
            await _wait_for_event(events, "short_lived_orphan_started")
            descendant_pid = int(
                next(
                    row["descendant_pid"]
                    for row in _events(events)
                    if row["event"] == "short_lived_orphan_started"
                )
            )
            await _wait_for_process_reap(descendant_pid)
            assert handle._agent_state_for_test() == "ready"  # type: ignore[attr-defined]
            await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(OrphanReapProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())


def test_ordinary_updates_are_discarded_without_blocking_the_turn(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, "act_ordinary_update_burst", attempts)
    runtime = _native()._Runtime()
    observed: list[dict[str, object]] = []

    class UpdateActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            observed.append(
                await self.act(
                    script="Remember the token alpha while processing updates.",
                    output_schema={
                        "stored": troupe.act_schema.StrValue(
                            description="the remembered token",
                            choices=["alpha"],
                        )
                    },
                )
            )
            return ()

    class UpdateProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                UpdateActor,
                name="ordinary-update-discard",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            assert handle._agent_state_for_test() == "ready"  # type: ignore[attr-defined]
            await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(UpdateProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert observed == [{"stored": "alpha"}]
    update_event = next(
        row for row in _events(events) if row["event"] == "ordinary_updates_sent"
    )
    assert update_event["count"] == 256


def test_ordinary_update_after_terminal_response_breaks_before_reuse(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    release = tmp_path / "late-update.fifo"
    os.mkfifo(release)
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, "act_late_update_after_terminal", attempts, release)
    runtime = _native()._Runtime()
    results: list[dict[str, object]] = []
    failures: list[troupe.AgentSessionBrokenError] = []

    class LateUpdateActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            if cue.instruction["attempt"] == 1:
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
            else:
                try:
                    await self.act(
                        script="This second prompt must never be submitted.",
                        output_schema={
                            "stored": troupe.act_schema.StrValue(
                                description="an unreachable token"
                            )
                        },
                    )
                except troupe.AgentSessionBrokenError as error:
                    failures.append(error)
            return ()

    class LateUpdateProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                LateUpdateActor,
                name="late-ordinary-update",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({"attempt": 1}) == ()
            assert results == [{"stored": "alpha"}]
            assert handle._agent_state_for_test() == "ready"  # type: ignore[attr-defined]
            await _signal_fifo(release)
            await _wait_for_event(events, "late_ordinary_update_sent")
            for _ in range(1_000):
                if handle._agent_state_for_test() != "ready":  # type: ignore[attr-defined]
                    break
                await asyncio.sleep(0)
            assert handle._agent_state_for_test() == "broken"  # type: ignore[attr-defined]
            assert await handle.cue({"attempt": 2}) == ()
            await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(LateUpdateProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert results == [{"stored": "alpha"}]
    assert len(failures) == 1
    assert failures[0].code == "protocol_violation"
    assert sum(row["event"] == "prompt_received" for row in _events(events)) == 1


def test_idle_permission_request_after_terminal_response_breaks_before_reuse(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    release = tmp_path / "late-permission.fifo"
    os.mkfifo(release)
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(
        events,
        "act_late_permission_after_terminal",
        attempts,
        release,
    )
    runtime = _native()._Runtime()
    results: list[dict[str, object]] = []
    failures: list[troupe.AgentSessionBrokenError] = []

    class IdlePermissionActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            if cue.instruction["attempt"] == 1:
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
            else:
                try:
                    await self.act(
                        script="This second prompt must never be submitted.",
                        output_schema={
                            "stored": troupe.act_schema.StrValue(
                                description="an unreachable token"
                            )
                        },
                    )
                except troupe.AgentSessionBrokenError as error:
                    failures.append(error)
            return ()

    class IdlePermissionProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                IdlePermissionActor,
                name="idle-late-permission",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({"attempt": 1}) == ()
            assert results == [{"stored": "alpha"}]
            assert handle._agent_state_for_test() == "ready"  # type: ignore[attr-defined]
            await _signal_fifo(release)
            await _wait_for_event(events, "late_permission_request_sent")
            for _ in range(1_000):
                if handle._agent_state_for_test() == "broken":  # type: ignore[attr-defined]
                    break
                await asyncio.sleep(0)
            assert handle._agent_state_for_test() == "broken"  # type: ignore[attr-defined]
            assert await handle.cue({"attempt": 2}) == ()
            await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(IdlePermissionProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert results == [{"stored": "alpha"}]
    assert len(failures) == 1
    assert failures[0].code == "protocol_violation"
    assert sum(row["event"] == "prompt_received" for row in _events(events)) == 1


def test_permission_request_after_same_batch_terminal_response_breaks_current_turn(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, "act_terminal_then_permission_batch", attempts)
    _native()._agent_test_hold_turn_settlement()
    runtime = _native()._Runtime()
    results: list[dict[str, object]] = []
    failures: list[troupe.AgentSessionBrokenError] = []

    class BatchedPermissionActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
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

    class BatchedPermissionProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                BatchedPermissionActor,
                name="batched-late-permission",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            first = asyncio.create_task(handle.cue({"attempt": 1}))
            await _wait_for_event(events, "terminal_then_permission_batch_sent")
            try:
                for _ in range(1_000):
                    if handle._agent_state_for_test() == "broken":  # type: ignore[attr-defined]
                        break
                    await asyncio.sleep(0)
                assert handle._agent_state_for_test() == "broken"  # type: ignore[attr-defined]
            finally:
                _native()._agent_test_release_turn_settlement()
            assert await first == ()
            assert await handle.cue({"attempt": 2}) == ()
            await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(BatchedPermissionProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert results == []
    assert len(failures) == 2
    assert [failure.code for failure in failures] == [
        "protocol_violation",
        "protocol_violation",
    ]
    assert sum(row["event"] == "prompt_received" for row in _events(events)) == 1


@pytest.mark.skipif(not hasattr(os, "fork"), reason="Linux fork is required")
def test_fork_child_closes_inherited_agent_fds_without_killing_parent_session(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    attempts = tmp_path / "attempts"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    _configure_mock(events, "default", attempts)
    runtime = _native()._Runtime()
    observed: dict[str, bool] = {}

    class ForkCleanupProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                troupe.Actor,
                name="fork-fd-cleanup",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            ready = await handle._agent_ready_for_test()  # type: ignore[attr-defined]
            endpoint = urlsplit(str(ready["endpoint"]))
            assert endpoint.hostname == "127.0.0.1"
            assert endpoint.port is not None
            agent_pid = int(ready["pid"])

            child_read, parent_write = os.pipe()
            child_pid = os.fork()
            if child_pid == 0:
                os.close(parent_write)
                try:
                    os.read(child_read, 1)
                finally:
                    os.close(child_read)
                os._exit(0)

            os.close(child_read)
            try:
                await self._agent_shutdown_for_test()  # type: ignore[attr-defined]
                try:
                    os.kill(child_pid, 0)
                    observed["child_alive"] = True
                except ProcessLookupError:
                    observed["child_alive"] = False
                try:
                    os.kill(agent_pid, 0)
                    observed["agent_reaped"] = False
                except ProcessLookupError:
                    observed["agent_reaped"] = True
                with socket.socket() as connection_probe:
                    observed["listener_closed"] = connection_probe.connect_ex(
                        (endpoint.hostname, endpoint.port)
                    ) != 0
                with socket.socket() as bind_probe:
                    bind_probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                    try:
                        bind_probe.bind((endpoint.hostname, endpoint.port))
                        observed["port_rebound"] = True
                    except OSError:
                        observed["port_rebound"] = False
            finally:
                try:
                    os.write(parent_write, b"1")
                except BrokenPipeError:
                    pass
                os.close(parent_write)
                waited_pid, status = await asyncio.to_thread(os.waitpid, child_pid, 0)
                assert waited_pid == child_pid
                assert os.waitstatus_to_exitcode(status) == 0
            runtime.request_shutdown()

    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(ForkCleanupProduction([]))),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())
    assert observed == {
        "child_alive": True,
        "agent_reaped": True,
        "listener_closed": True,
        "port_rebound": True,
    }
