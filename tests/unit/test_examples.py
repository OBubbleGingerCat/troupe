from __future__ import annotations

import importlib.util
import json
import os
import re
import selectors
import shlex
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parents[2]
EXAMPLES = ROOT / "examples"
TIMEOUT = 5.0
READY_PREFIX = b"troupe: diagnostic ready "
EXAMPLE_NAMES = (
    "hello_actor",
    "repeating_scenes",
    "actor_pipeline",
    "cooperative_workers",
    "cancellation_cleanup",
)


def _environment() -> dict[str, str]:
    environment = os.environ.copy()
    for name in ("CONDA_PREFIX", "PYTHONHOME", "PYTHONPATH", "VIRTUAL_ENV"):
        environment.pop(name, None)
    environment["PYTHONDONTWRITEBYTECODE"] = "1"
    return environment


def _read_pipe_events(
    selector: selectors.BaseSelector,
    buffers: dict[int, bytearray],
    timeout: float,
) -> None:
    for key, _ in selector.select(timeout):
        try:
            chunk = os.read(key.fd, 65_536)
        except BlockingIOError:
            continue
        if chunk:
            buffers[key.fd].extend(chunk)
        else:
            selector.unregister(key.fd)


def _collect_output(
    process: subprocess.Popen[bytes],
    example: str,
    *,
    readiness_lines: int = 1,
    timeout: float = TIMEOUT,
) -> tuple[list[str], bytes]:
    assert process.stdout is not None
    assert process.stderr is not None
    stdout_fd = process.stdout.fileno()
    stderr_fd = process.stderr.fileno()
    buffers = {stdout_fd: bytearray(), stderr_fd: bytearray()}
    selector = selectors.DefaultSelector()
    for file_descriptor in buffers:
        os.set_blocking(file_descriptor, False)
        selector.register(file_descriptor, selectors.EVENT_READ)

    try:
        readiness_deadline = time.monotonic() + timeout
        while buffers[stdout_fd].count(b"\n") < readiness_lines:
            if stdout_fd not in selector.get_map():
                output = bytes(buffers[stdout_fd] + buffers[stderr_fd])
                raise AssertionError(
                    f"{example} exited before readiness: "
                    f"{output.decode(errors='replace')}"
                )
            remaining = readiness_deadline - time.monotonic()
            assert remaining > 0, (
                f"{example} did not produce a complete readiness line"
            )
            _read_pipe_events(selector, buffers, remaining)

        process.send_signal(signal.SIGINT)
        shutdown_deadline = time.monotonic() + timeout
        while selector.get_map():
            remaining = shutdown_deadline - time.monotonic()
            assert remaining > 0, f"{example} did not close its output streams"
            _read_pipe_events(selector, buffers, remaining)

        remaining = shutdown_deadline - time.monotonic()
        assert remaining > 0, f"{example} did not exit after SIGINT"
        try:
            process.wait(timeout=remaining)
        except subprocess.TimeoutExpired as error:
            raise AssertionError(f"{example} did not exit after SIGINT") from error
    finally:
        selector.close()

    return bytes(buffers[stdout_fd]).decode().splitlines(), bytes(buffers[stderr_fd])


def _without_ready(stderr: bytes, package: Path) -> bytes:
    ready, separator, remaining = stderr.partition(b"\n")
    assert separator == b"\n"
    assert ready.startswith(READY_PREFIX)
    assert READY_PREFIX not in remaining
    locator = json.loads(ready.removeprefix(READY_PREFIX))
    assert set(locator) == {
        "locator_schema_version",
        "run_id",
        "local_url",
        "advertise_url",
        "archive_directory",
        "security_scope",
    }
    assert locator["locator_schema_version"] == 1
    assert type(locator["run_id"]) is str and locator["run_id"]
    assert locator["local_url"].startswith("http://127.0.0.1:")
    assert locator["advertise_url"] is None
    archive = Path(locator["archive_directory"])
    assert archive.is_absolute()
    assert archive.is_relative_to((package / ".troupe").resolve())
    assert locator["security_scope"] == "trusted_network"
    return remaining


def _run_example(
    tmp_path: Path,
    example: str,
    *,
    readiness_lines: int = 1,
) -> list[str]:
    console = Path(sys.executable).with_name("troupe")
    assert console.is_file()
    examples_readme = (EXAMPLES / "README.md").read_text(encoding="utf-8")
    documented = [
        shlex.split(command)
        for command in re.findall(r"```console\n([^\n]+)\n```", examples_readme)
    ]
    matching = [
        command
        for command in documented
        if command[:3] == ["troupe", "--production", f"examples/{example}"]
    ]
    assert len(matching) == 1
    package = tmp_path / "examples" / example
    package.parent.mkdir()
    shutil.copytree(
        EXAMPLES / example,
        package,
        ignore=shutil.ignore_patterns(".troupe", "__pycache__"),
    )
    command = [str(console), *matching[0][1:]]
    process = subprocess.Popen(
        command,
        cwd=tmp_path,
        env=_environment(),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        bufsize=0,
    )
    try:
        lines, stderr = _collect_output(
            process,
            example,
            readiness_lines=readiness_lines,
        )
        assert process.returncode == 0, stderr.decode(errors="replace")
        assert _without_ready(stderr, package) == b""
        return lines
    finally:
        if process.poll() is None:
            process.kill()
        process.wait(timeout=TIMEOUT)
        assert process.stdout is not None
        assert process.stderr is not None
        process.stdout.close()
        process.stderr.close()


def test_output_collector_times_out_on_a_partial_line() -> None:
    process = subprocess.Popen(
        [
            sys.executable,
            "-c",
            "import os, time; os.write(1, b'partial'); "
            "os.write(2, b'ready\\n'); time.sleep(60)",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        bufsize=0,
    )
    assert process.stderr is not None
    assert process.stderr.readline() == b"ready\n"
    started = time.monotonic()
    try:
        with pytest.raises(AssertionError, match="complete readiness line"):
            _collect_output(process, "partial-output", timeout=0.1)
    finally:
        if process.poll() is None:
            process.kill()
        process.wait(timeout=TIMEOUT)
        assert process.stdout is not None
        assert process.stderr is not None
        process.stdout.close()
        process.stderr.close()
    assert time.monotonic() - started < 2.0


def test_examples_are_documented_public_production_packages() -> None:
    root_readme = (ROOT / "README.md").read_text(encoding="utf-8")
    examples_readme = (EXAMPLES / "README.md").read_text(encoding="utf-8")

    assert "[Progressive examples](examples/README.md)" in root_readme
    for prerequisite in ("Node.js", "npm", "`npx`", "logged in"):
        assert prerequisite in examples_readme
    for name in EXAMPLE_NAMES:
        package = EXAMPLES / name
        assert name.isidentifier()
        assert (package / "__init__.py").read_bytes() == b""
        assert (package / "production.py").is_file()
        assert f"troupe --production examples/{name}" in examples_readme


def test_diagnostics_complex_example_uses_a_sustainable_default_interval(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.syspath_prepend(str(ROOT))
    from examples.diagnostics.production import (
        DEFAULT_COMPLEX_INTERVAL_SECONDS,
        DEFAULT_INTERVAL_SECONDS,
        parse_interval_seconds,
    )

    assert parse_interval_seconds(
        [],
        default=DEFAULT_COMPLEX_INTERVAL_SECONDS,
    ) == 30.0
    assert parse_interval_seconds([], default=DEFAULT_INTERVAL_SECONDS) == 30.0
    examples_readme = (EXAMPLES / "README.md").read_text(encoding="utf-8")
    assert "-- complex\n" in examples_readme
    assert "30-second Scene interval" in examples_readme


def test_mixed_repository_repair_live_example_and_oracle_are_wired() -> None:
    production = EXAMPLES / "live_agents" / "mixed_repository_repair"
    runner = ROOT / "scripts" / "test_live_mixed_agents.sh"
    oracle = ROOT / "tests" / "live" / "mixed_agent_oracle.py"
    live_readme = EXAMPLES / "live_agents" / "README.md"

    assert (production / "__init__.py").read_bytes() == b""
    source = (production / "production.py").read_text(encoding="utf-8")
    assert all(
        marker in source
        for marker in (
            "TROUPE_LIVE_CODEX_PROFILE",
            "TROUPE_LIVE_CLAUDE_PROFILE",
            "TROUPE_LIVE_KIMI_PROFILE",
            "codex-investigator",
            "claude-reviewer",
            "kimi-repairer",
            '"operation": "recall"',
            "ObjectValue",
        )
    )
    assert runner.is_file() and os.access(runner, os.X_OK)
    runner_source = runner.read_text(encoding="utf-8")
    assert "mixed_agent_oracle.py" in runner_source
    assert "maturin build" in runner_source
    assert "agent-test-support" not in runner_source
    assert oracle.is_file()
    assert "mixed_repository_repair" in oracle.read_text(encoding="utf-8")
    assert "scripts/test_live_mixed_agents.sh" in live_readme.read_text(encoding="utf-8")


def test_mixed_oracle_waits_for_failed_production_descendants(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    live_tests = ROOT / "tests" / "live"
    monkeypatch.syspath_prepend(str(live_tests))
    spec = importlib.util.spec_from_file_location(
        "troupe_mixed_agent_oracle_test",
        live_tests / "mixed_agent_oracle.py",
    )
    assert spec is not None and spec.loader is not None
    oracle = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(oracle)

    workspace = tmp_path / "workspace"
    repository = workspace / "repository"
    workspace.mkdir()
    repository.mkdir()
    child_pid = workspace / "child.pid"
    settings = workspace / "settings.json"
    settings.write_text("{}\n", encoding="utf-8")
    script = (
        "import pathlib, subprocess, sys, time\n"
        "child = subprocess.Popen(\n"
        "    [sys.executable, '-c', 'import time; time.sleep(0.4)'],\n"
        "    start_new_session=True,\n"
        ")\n"
        "pathlib.Path(sys.argv[1]).write_text(str(child.pid), encoding='ascii')\n"
        "time.sleep(0.2)\n"
        "raise SystemExit(3)\n"
    )

    def production_command(**_: object) -> tuple[list[str], dict[str, str], tuple[Path, bytes]]:
        return (
            [sys.executable, "-c", script, str(child_pid)],
            _environment(),
            (settings, settings.read_bytes()),
        )

    monkeypatch.setattr(oracle, "_production_command", production_command)
    with pytest.raises(
        oracle.AcceptanceFailure,
        match="exited before publishing its report",
    ):
        oracle._run_production(
            workspace=workspace,
            repository=repository,
            report=workspace / "report.json",
            profiles={},
        )

    pid = int(child_pid.read_text(encoding="ascii"))
    assert oracle._process_identity(pid) is None
    assert settings.read_text(encoding="utf-8") == "{}\n"


def test_mixed_oracle_cleans_descendants_before_reporting_settings_drift(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    live_tests = ROOT / "tests" / "live"
    monkeypatch.syspath_prepend(str(live_tests))
    spec = importlib.util.spec_from_file_location(
        "troupe_mixed_agent_oracle_settings_test",
        live_tests / "mixed_agent_oracle.py",
    )
    assert spec is not None and spec.loader is not None
    oracle = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(oracle)

    workspace = tmp_path / "workspace"
    repository = workspace / "repository"
    workspace.mkdir()
    repository.mkdir()
    child_pid = workspace / "child.pid"
    settings = workspace / "settings.json"
    settings.write_text("{}\n", encoding="utf-8")
    script = (
        "import pathlib, subprocess, sys, time\n"
        "child = subprocess.Popen(\n"
        "    [sys.executable, '-c', 'import signal; signal.pause()'],\n"
        "    start_new_session=True,\n"
        ")\n"
        "pathlib.Path(sys.argv[1]).write_text(str(child.pid), encoding='ascii')\n"
        "pathlib.Path(sys.argv[2]).write_text('{\\\"changed\\\": true}\\n', encoding='utf-8')\n"
        "time.sleep(0.2)\n"
        "raise SystemExit(3)\n"
    )

    def production_command(**_: object) -> tuple[list[str], dict[str, str], tuple[Path, bytes]]:
        return (
            [sys.executable, "-c", script, str(child_pid), str(settings)],
            _environment(),
            (settings, settings.read_bytes()),
        )

    monkeypatch.setattr(oracle, "_production_command", production_command)
    monkeypatch.setattr(oracle, "PROCESS_CLEANUP_SECONDS", 0.05)
    pid: int | None = None
    try:
        with pytest.raises(oracle.AcceptanceFailure):
            oracle._run_production(
                workspace=workspace,
                repository=repository,
                report=workspace / "report.json",
                profiles={},
            )
        pid = int(child_pid.read_text(encoding="ascii"))
        assert oracle._process_identity(pid) is None
    finally:
        if pid is not None and oracle._process_identity(pid) is not None:
            os.kill(pid, signal.SIGKILL)


def test_hello_actor_runs_through_the_literal_console(tmp_path: Path) -> None:
    assert _run_example(tmp_path, "hello_actor") == ["Hello, Ada!"]


def test_repeating_scenes_run_as_distinct_scene_calls(tmp_path: Path) -> None:
    lines = _run_example(tmp_path, "repeating_scenes", readiness_lines=3)

    assert len(lines) >= 3
    assert lines == [f"scene:{number}" for number in range(1, len(lines) + 1)]


def test_actor_pipeline_reports_query_and_provenance(tmp_path: Path) -> None:
    lines = _run_example(tmp_path, "actor_pipeline")
    assert len(lines) == 1
    payload = json.loads(lines[0])

    assert payload == {
        "actors": ["formatter", "router"],
        "message": "HELLO TROUPE",
        "owner": "formatter",
        "source": "router",
    }


def test_cooperative_workers_show_cross_actor_progress_and_fifo(tmp_path: Path) -> None:
    lines = _run_example(tmp_path, "cooperative_workers")
    assert len(lines) == 1
    assert json.loads(lines[0]) == {
        "submitted": ["left:2", "left:3"],
        "timeline": [
            "start:left:1",
            "start:right:1",
            "end:right:1",
            "end:left:1",
            "start:left:2",
            "end:left:2",
            "start:left:3",
            "end:left:3",
        ],
    }


def test_cancellation_cleanup_finishes_before_stop(tmp_path: Path) -> None:
    assert _run_example(tmp_path, "cancellation_cleanup") == [
        "worker:start",
        "worker:cleanup",
        "scene:cleanup",
        "production:stop",
    ]
