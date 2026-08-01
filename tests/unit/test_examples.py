from __future__ import annotations

import json
import os
import re
import selectors
import shlex
import signal
import subprocess
import sys
import time
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parents[2]
EXAMPLES = ROOT / "examples"
TIMEOUT = 5.0
EXAMPLE_NAMES = (
    "hello_actor",
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
        while b"\n" not in buffers[stdout_fd]:
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


def _run_example(
    example: str,
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
    command = [str(console), *matching[0][1:]]
    process = subprocess.Popen(
        command,
        cwd=ROOT,
        env=_environment(),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        bufsize=0,
    )
    try:
        lines, stderr = _collect_output(process, example)
        assert process.returncode == 0, stderr.decode(errors="replace")
        assert stderr == b""
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
    for name in EXAMPLE_NAMES:
        package = EXAMPLES / name
        assert name.isidentifier()
        assert (package / "__init__.py").read_bytes() == b""
        assert (package / "production.py").is_file()
        assert f"troupe --production examples/{name}" in examples_readme


def test_hello_actor_runs_through_the_literal_console() -> None:
    assert _run_example("hello_actor") == ["Hello, Ada!"]


def test_actor_pipeline_reports_query_and_provenance() -> None:
    lines = _run_example("actor_pipeline")
    assert len(lines) == 1
    payload = json.loads(lines[0])

    assert payload == {
        "actors": ["formatter", "router"],
        "message": "HELLO TROUPE",
        "owner": "formatter",
        "source": "router",
    }


def test_cooperative_workers_show_cross_actor_progress_and_fifo() -> None:
    lines = _run_example("cooperative_workers")
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


def test_cancellation_cleanup_finishes_before_stop() -> None:
    assert _run_example("cancellation_cleanup") == [
        "worker:start",
        "worker:cleanup",
        "scene:cleanup",
        "production:stop",
    ]
