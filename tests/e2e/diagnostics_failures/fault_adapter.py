"""Closed fault adapters used by the V06 child harness."""

from __future__ import annotations

import http.client
import json
import os
import selectors
import shutil
import signal
import socket
import subprocess
import sys
import textwrap
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Mapping
from urllib.parse import urlsplit


ROOT = Path(__file__).resolve().parents[3]
MANIFEST = ROOT / "rust/Cargo.toml"
READY_PREFIX = b"troupe: diagnostic ready "
TIMEOUT = 30.0


class FaultAdapterError(AssertionError):
    pass


def require(condition: object, detail: str) -> None:
    if not condition:
        raise FaultAdapterError(detail)


@dataclass(frozen=True, slots=True)
class CommandSpec:
    argv: tuple[str, ...]
    timeout_seconds: float = 180.0


@dataclass(frozen=True, slots=True)
class AdapterSpec:
    commands: tuple[CommandSpec, ...]
    internal: str | None = None


@dataclass(frozen=True, slots=True)
class AdapterResult:
    commands: tuple[str, ...]
    assertions: int


def _cargo_test(
    package: str,
    *,
    target: str | None = None,
    test_filter: str | None = None,
    features: str | None = None,
) -> CommandSpec:
    argv = [
        "cargo",
        "test",
        "--locked",
        "--manifest-path",
        str(MANIFEST),
        "--package",
        package,
    ]
    if features is not None:
        argv.extend(("--features", features))
    if target is not None:
        argv.extend(("--test", target))
    if test_filter is not None:
        argv.append(test_filter)
    argv.extend(("--", "--nocapture", "--test-threads=1"))
    return CommandSpec(tuple(argv))


def _runtime_test(target: str) -> CommandSpec:
    return _cargo_test("troupe-diagnostics-runtime", target=target)


def _troupe_lib(test_filter: str) -> CommandSpec:
    return _cargo_test(
        "troupe",
        test_filter=test_filter,
        features="agent-test-support,diagnostics-test-support",
    )


def _troupe_test(target: str) -> CommandSpec:
    return _cargo_test(
        "troupe",
        target=target,
        features="agent-test-support,diagnostics-test-support",
    )


ADAPTERS: Mapping[str, AdapterSpec] = {
    "startup_archive_lease": AdapterSpec(
        (_runtime_test("archive_layout"), _runtime_test("archive_lease"))
    ),
    "startup_store_registry_server": AdapterSpec(
        (
            _runtime_test("store_schema"),
            _runtime_test("registry_publish"),
            _runtime_test("server_shell"),
        )
    ),
    "runtime_admission_writer": AdapterSpec(
        (_runtime_test("store_admission"), _runtime_test("store_writer"))
    ),
    "runtime_progress_quota": AdapterSpec(
        (_runtime_test("store_progress"), _runtime_test("store_quota"))
    ),
    "runtime_active_queries": AdapterSpec(
        (_runtime_test("query_reader"), _runtime_test("server_query"))
    ),
    "runtime_sse": AdapterSpec((_runtime_test("server_sse"),)),
    "local_exporter": AdapterSpec((_runtime_test("server_dump"),)),
    "usage_terminal_matrix": AdapterSpec(
        (
            _troupe_test("diagnostic_usage_finalization"),
            _troupe_test("diagnostic_usage_slot"),
        )
    ),
    "sink_queue_dispatch": AdapterSpec(
        (
            _troupe_test("diagnostic_sink_queue"),
            _troupe_lib("diagnostic_sink::dispatcher::tests"),
            _troupe_lib("diagnostic_sink::shutdown::tests"),
            _troupe_lib(
                "diagnostic_runtime::load_producer::tests::"
                "act_subscriber_routing_is_shared_and_component_failure_can_bypass_it"
            ),
        )
    ),
    "sink_callback_live": AdapterSpec((), internal="sink_callback_live"),
    "shutdown_convergence": AdapterSpec(
        (
            _troupe_lib("diagnostic_runtime::shutdown::tests"),
            _troupe_lib("diagnostic_runtime::supervisor::tests"),
            _troupe_lib("diagnostic_runtime::bootstrap::tests::ordered_clean_shutdown"),
            _troupe_lib("diagnostic_runtime::bootstrap::tests::explicit_shutdown"),
        )
    ),
}


EXPECTED_ADAPTER_CHECKS: Mapping[str, frozenset[str]] = {
    "startup_archive_lease": frozenset(
        {
            "startup.state_path",
            "startup.write_probe",
            "startup.active_lease",
            "startup.pre_import",
            "startup.rollback_resources",
            "shutdown.hard_crash",
            "shutdown.no_residue",
        }
    ),
    "startup_store_registry_server": frozenset(
        {
            "startup.schema",
            "startup.initial_commit",
            "startup.bind",
            "startup.registry_publish",
            "runtime.server_execution_context",
            "runtime.listener",
            "runtime.storage_permission",
            "local.single_client",
            "local.single_request",
            "local.invalid_request",
            "shutdown.registry_unpublish",
        }
    ),
    "runtime_admission_writer": frozenset(
        {
            "runtime.hub",
            "runtime.admission_event_boundary",
            "runtime.admission_byte_boundary",
            "runtime.writer_queue",
            "runtime.writer_transaction",
            "runtime.writer_commit",
            "runtime.storage_disk",
            "shutdown.dense_prefix",
        }
    ),
    "runtime_progress_quota": frozenset(
        {
            "runtime.writer_stall",
            "runtime.run_quota",
            "shutdown.drain_timeout",
            "limits.default_ingress",
            "limits.batch",
            "limits.deadlines",
            "limits.deterministic_clock",
        }
    ),
    "runtime_active_queries": frozenset(
        {
            "runtime.active_reader_corruption",
            "runtime.active_reader_identity",
            "runtime.active_reader_dense_prefix",
            "runtime.query_execution_context",
            "local.archive_reader",
            "local.archive_query",
            "local.archive_store",
            "shutdown.archive_readable",
        }
    ),
    "runtime_sse": frozenset(
        {
            "runtime.sse_reader",
            "local.sse_slow_client",
            "local.sse_overflow",
        }
    ),
    "local_exporter": frozenset(
        {"local.exporter", "shutdown.stream_peer_disconnect"}
    ),
    "usage_terminal_matrix": frozenset(
        {
            "usage.prompt_not_submitted",
            "usage.session_terminal_unknown",
            "usage.authoritative_race",
            "usage.exactly_once",
            "usage.mandatory_ack",
            "usage.before_act_finish",
        }
    ),
    "sink_queue_dispatch": frozenset(
        {
            "local.sink_overflow",
            "sink.unexpected_enqueue_once",
            "sink.drop_counter",
            "sink.drop_summary",
            "sink.counter_failure_nonrecursive",
        }
    ),
    "sink_callback_live": frozenset(
        {
            "local.sink_callback",
            "sink.callback_component_once",
            "sink.failure_visible_store_http_cli",
            "sink.failure_not_redelivered",
            "sink.production_continues",
        }
    ),
    "shutdown_convergence": frozenset(
        {
            "runtime.first_cause",
            "runtime.stop_production",
            "shutdown.terminal_commit",
            "shutdown.resource_close",
            "shutdown.clean_shutdown",
            "shutdown.incomplete_on_failure",
        }
    ),
}


def assert_adapter_inventory(adapters: Mapping[str, frozenset[str]]) -> None:
    require(set(adapters) == set(ADAPTERS), "matrix and closed fault adapter inventory differ")
    require(adapters == EXPECTED_ADAPTER_CHECKS, "matrix check-to-adapter mapping drifted")


def run_adapter(
    adapter_id: str,
    root: Path,
    environment: Mapping[str, str],
    run_command: Callable[[CommandSpec], str],
) -> AdapterResult:
    spec = ADAPTERS.get(adapter_id)
    require(spec is not None, f"unknown fault adapter: {adapter_id}")
    commands = [run_command(command) for command in spec.commands]
    assertions = len(commands)
    if spec.internal == "sink_callback_live":
        internal = _sink_callback_live(root, environment)
        commands.extend(internal.commands)
        assertions += internal.assertions
    require(spec.internal in {None, "sink_callback_live"}, "unknown internal adapter")
    return AdapterResult(tuple(commands), assertions)


def _console(environment: Mapping[str, str]) -> Path:
    override = environment.get("TROUPE_E2E_CONSOLE")
    candidate = Path(override) if override else None
    if candidate is None:
        resolved = shutil.which("troupe", path=environment.get("PATH"))
        candidate = Path(resolved) if resolved else None
    require(candidate is not None and candidate.is_file(), "troupe console is unavailable")
    return candidate.resolve()


def _readline(
    pipe: object,
    process: subprocess.Popen[bytes],
    timeout: float,
) -> bytes:
    selector = selectors.DefaultSelector()
    selector.register(pipe, selectors.EVENT_READ)
    try:
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            require(remaining > 0, "timed out waiting for Production readiness")
            if selector.select(remaining):
                line = process.stderr.readline() if process.stderr is not None else b""
                require(line, f"Production exited before readiness with {process.poll()}")
                return line
    finally:
        selector.close()


def _wait_file(path: Path, process: subprocess.Popen[bytes]) -> None:
    deadline = time.monotonic() + TIMEOUT
    while not path.is_file():
        if process.poll() is not None:
            stdout, stderr = process.communicate()
            raise FaultAdapterError(
                "Production exited before sink settlement: "
                f"exit={process.returncode}, stdout={stdout!r}, stderr={stderr!r}"
            )
        require(time.monotonic() < deadline, "timed out waiting for sink settlement")
        time.sleep(0.01)


def _http_events(base_url: str) -> list[dict[str, object]]:
    parsed = urlsplit(base_url)
    require(parsed.hostname is not None and parsed.port is not None, "bad live URL")
    path = f"{parsed.path.rstrip('/')}/api/v1/events?after=0"
    connection = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=TIMEOUT)
    try:
        connection.request(
            "GET",
            path,
            headers={"Accept": "application/json", "Connection": "close"},
        )
        response = connection.getresponse()
        body = response.read()
        require(response.status == 200, f"live events returned {response.status}: {body[:300]!r}")
        value = json.loads(body)
        require(
            isinstance(value, dict) and isinstance(value.get("events"), list),
            "bad live events",
        )
        return value["events"]
    finally:
        connection.close()


def _cli_jsonl(
    console: Path,
    target: tuple[str, str],
    environment: Mapping[str, str],
) -> list[dict[str, object]]:
    result = subprocess.run(
        [
            str(console),
            "diagnostic",
            "events",
            *target,
            "--after",
            "0",
            "--format",
            "jsonl",
        ],
        cwd=ROOT,
        env=dict(environment),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=TIMEOUT,
    )
    require(result.returncode == 0, f"diagnostic events failed: {result.stderr!r}")
    require(result.stderr == b"", f"diagnostic events wrote stderr: {result.stderr!r}")
    rows = [json.loads(line) for line in result.stdout.decode().splitlines()]
    require(all(isinstance(row, dict) for row in rows), "CLI JSONL contains a non-object")
    return rows


def _component_failures(events: list[dict[str, object]]) -> list[dict[str, object]]:
    return [
        event
        for event in events
        if event.get("kind") == "instant_occurred"
        and event.get("instant_kind") == "diagnostic.component_failed"
    ]


def _sink_callback_live(
    root: Path,
    inherited_environment: Mapping[str, str],
) -> AdapterResult:
    console = _console(inherited_environment)
    package = root / "production"
    package.mkdir()
    workspace = root / "agent-workspace"
    workspace.mkdir()
    stage = root / "sink-summary.json"
    config = root / "config.json"
    agent_events = root / "agent-events.jsonl"
    mock = ROOT / "tests/support/mock_acp_agent.py"
    source = textwrap.dedent(
        """
        from __future__ import annotations

        import asyncio
        import json
        from pathlib import Path
        import sys

        import troupe
        from troupe import diagnostics
        import troupe._runtime as _runtime


        class RaisingSink(diagnostics.DiagnosticSink):
            def __init__(self) -> None:
                super().__init__()
                self.calls = 0

            def on_event(self, event: diagnostics.DiagnosticEvent, /) -> None:
                del event
                self.calls += 1
                raise ValueError("v06 callback fault")


        class FaultActor(troupe.Actor):
            def __init__(self, stage: str) -> None:
                self.stage = Path(stage)

            async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
                del cue
                sink = RaisingSink()
                result = await self.act(
                    script="Return an empty object.",
                    output_schema={},
                    diagnostic_sink=sink,
                )
                summary = await sink.wait_closed()
                temporary = self.stage.with_suffix(".tmp")
                temporary.write_text(
                    json.dumps(
                        {
                            "result": result,
                            "calls": sink.calls,
                            "close_reason": summary.close_reason,
                            "complete": summary.complete,
                            "callback_kind": summary.callback_failure.kind,
                            "callback_type": summary.callback_failure.exception_type,
                        },
                        sort_keys=True,
                        separators=(",", ":"),
                    ),
                    encoding="utf-8",
                )
                temporary.replace(self.stage)
                return ()


        class Production(troupe.Production):
            def __init__(self, args: list[str]) -> None:
                values = json.loads(Path(args[0]).read_text(encoding="utf-8"))
                _runtime._agent_test_set_launch(
                    program=sys.executable,
                    args=[
                        values["mock"],
                        "--events",
                        values["agent_events"],
                        "--scenario",
                        "act_submit_results",
                        "--results-json",
                        "[{}]",
                    ],
                )
                profile = troupe.AgentProfile(
                    agent="codex",
                    workspace=values["workspace"],
                    model="test-model",
                    effort="max",
                )
                self.actor = self.cast_actor(
                    FaultActor,
                    name="v06-sink-callback",
                    agent_profile=profile,
                    actor_args=(values["stage"],),
                    actor_kwargs={},
                )

            async def scene(self) -> None:
                assert await self.actor.cue({}) == ()
                await asyncio.Event().wait()
        """
    ).lstrip()
    (package / "__init__.py").write_text("", encoding="ascii")
    (package / "production.py").write_text(source, encoding="utf-8")
    config.write_text(
        json.dumps(
            {
                "workspace": str(workspace),
                "stage": str(stage),
                "mock": str(mock),
                "agent_events": str(agent_events),
            },
            sort_keys=True,
            separators=(",", ":"),
        ),
        encoding="utf-8",
    )
    environment = dict(inherited_environment)
    for name in tuple(environment):
        if name.casefold() in {"all_proxy", "http_proxy", "https_proxy", "no_proxy"}:
            environment.pop(name)

    process = subprocess.Popen(
        [
            str(console),
            "--production",
            str(package),
            "--diagnostic-bind-host",
            "127.0.0.1",
            "--diagnostic-port",
            "0",
            "--",
            str(config),
        ],
        cwd=root,
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    assertions = 0
    try:
        require(process.stderr is not None, "Production stderr is unavailable")
        ready = _readline(process.stderr, process, TIMEOUT).rstrip(b"\n")
        require(ready.startswith(READY_PREFIX), f"bad Production ready line: {ready!r}")
        locator = json.loads(ready.removeprefix(READY_PREFIX))
        base_url = locator["local_url"]
        archive = Path(locator["archive_directory"])
        assertions += 3
        _wait_file(stage, process)
        summary = json.loads(stage.read_text(encoding="utf-8"))
        require(summary["result"] == {}, "sink callback changed Actor.act result")
        require(summary["calls"] == 1, "component failure was redelivered to the failed sink")
        require(summary["close_reason"] == "callback_failed", "sink close reason drifted")
        require(summary["complete"] is False, "failed sink was reported complete")
        require(summary["callback_kind"] == "raised", "callback failure kind drifted")
        require(summary["callback_type"] == "ValueError", "callback exception type drifted")
        require(process.poll() is None, "sink callback failure stopped Production")
        assertions += 7

        deadline = time.monotonic() + TIMEOUT
        http_events: list[dict[str, object]] = []
        while time.monotonic() < deadline:
            http_events = _http_events(base_url)
            if len(_component_failures(http_events)) == 1:
                break
            time.sleep(0.01)
        http_failure = _component_failures(http_events)
        require(len(http_failure) == 1, "HTTP did not expose exactly one component failure")
        cli_events = _cli_jsonl(console, ("--url", base_url), environment)
        cli_failure = _component_failures(cli_events)
        require(cli_failure == http_failure, "live CLI and HTTP component failure differ")
        detail = http_failure[0].get("detail")
        require(isinstance(detail, dict), "component failure has no typed detail")
        require(detail.get("stage") == "callback", "component failure stage drifted")
        require(detail.get("error_code") == "callback_raised", "component failure code drifted")
        assertions += 5

        process.send_signal(signal.SIGINT)
        stdout, stderr = process.communicate(timeout=TIMEOUT)
        require(process.returncode == 0, f"Production exited {process.returncode}: {stderr!r}")
        require(stdout == b"", f"Production wrote stdout: {stdout!r}")
        require(stderr == b"", f"Production wrote trailing stderr: {stderr!r}")
        archive_events = _cli_jsonl(console, ("--archive", str(archive)), environment)
        require(
            _component_failures(archive_events) == http_failure,
            "archive lost component failure",
        )
        assertions += 4

        status = subprocess.run(
            [
                str(console),
                "diagnostic",
                "status",
                "--archive",
                str(archive),
                "--format",
                "json",
            ],
            cwd=ROOT,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=TIMEOUT,
        )
        require(status.returncode == 0 and status.stderr == b"", "archive status failed")
        status_value = json.loads(status.stdout)
        require(status_value["lifecycle"]["clean_shutdown"] is True, "archive is not clean")
        instances = package / ".troupe/diagnostics/instances"
        require(not instances.exists() or not list(instances.iterdir()), "registry entry remains")
        parsed = urlsplit(base_url)
        try:
            socket.create_connection((parsed.hostname, parsed.port), timeout=0.2).close()
        except OSError:
            pass
        else:
            raise FaultAdapterError("diagnostic listener remains after Production exit")
        assertions += 4
    finally:
        if process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGTERM)
                process.communicate(timeout=3)
            except (OSError, subprocess.TimeoutExpired):
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except OSError:
                    pass
                process.communicate()
    return AdapterResult(("internal:sink_callback_live",), assertions)
