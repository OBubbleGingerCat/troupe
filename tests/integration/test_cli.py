from __future__ import annotations

import asyncio
import importlib
import json
import os
import selectors
import shutil
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path
from types import ModuleType
from typing import Any, Iterator

import pytest


ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "tests" / "fixtures" / "productions"
RECORDING_PACKAGE = FIXTURES / "recording_production"
IMPORT_FAILURE_PACKAGE = FIXTURES / "import_failure"
CONSTRUCTION_FAILURE_PACKAGE = FIXTURES / "construction_failure"
CONSOLE = Path(sys.executable).parent / "troupe"
TIMEOUT = 10.0
LOAD_HEADER = "troupe: failed to load production"
PHASE_PREFIX = "troupe: production failed during "
READY_PREFIX = "troupe: diagnostic ready "


def _native() -> ModuleType:
    return importlib.import_module("troupe._runtime")


def _clear_package(root: str) -> None:
    prefix = f"{root}."
    names = [name for name in sys.modules if name == root or name.startswith(prefix)]
    for name in sorted(names, key=lambda value: value.count("."), reverse=True):
        sys.modules.pop(name, None)


def _records(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def _copy_production(tmp_path: Path, source: Path) -> Path:
    destination = tmp_path / "productions" / source.name
    if not destination.exists():
        destination.parent.mkdir(exist_ok=True)
        shutil.copytree(
            source,
            destination,
            ignore=shutil.ignore_patterns(".troupe", "__pycache__"),
        )
    return destination


def _without_ready(stderr: str) -> str:
    ready, separator, remaining = stderr.partition("\n")
    assert separator == "\n"
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
    assert Path(locator["archive_directory"]).is_absolute()
    assert locator["security_scope"] == "trusted_network"
    return remaining


def _without_ready_bytes(stderr: bytes) -> bytes:
    return _without_ready(stderr.decode("utf-8")).encode("utf-8")


class FakeSignals:
    def __init__(self) -> None:
        self.originals = {signal.SIGINT: object(), signal.SIGTERM: object()}
        self.current = dict(self.originals)
        self.calls: list[tuple[signal.Signals, object, object]] = []

    def signal(self, signum: signal.Signals, handler: object) -> object:
        previous = self.current[signum]
        self.calls.append((signum, handler, previous))
        self.current[signum] = handler
        return previous

    def assert_never_touched(self) -> None:
        assert self.calls == []

    def assert_installed_and_restored(self) -> None:
        assert set(self.current) == {signal.SIGINT, signal.SIGTERM}
        for signum, original in self.originals.items():
            matching = [call for call in self.calls if call[0] == signum]
            assert len(matching) == 2
            assert matching[0][2] is original
            assert matching[1][1] is original
            assert self.current[signum] is original


def _call_main(monkeypatch: pytest.MonkeyPatch, argv: list[str], fake: FakeSignals) -> Any:
    monkeypatch.setattr(sys, "argv", argv)
    monkeypatch.setattr(signal, "signal", fake.signal)
    return _native().main()


@pytest.mark.parametrize(
    ("argv", "expected", "stream"),
    [
        (["troupe", "--help"], 0, "stdout"),
        (["troupe"], 2, "stderr"),
        (["troupe", "--unknown"], 2, "stderr"),
    ],
)
def test_same_process_help_and_usage_do_not_install_signals(
    monkeypatch: pytest.MonkeyPatch,
    capfd: pytest.CaptureFixture[str],
    argv: list[str],
    expected: int,
    stream: str,
) -> None:
    fake = FakeSignals()

    result = _call_main(monkeypatch, argv, fake)
    captured = capfd.readouterr()

    assert type(result) is int
    assert result == expected
    fake.assert_never_touched()
    if stream == "stdout":
        assert "--production" in captured.out
        assert "PACKAGE_DIR" in captured.out
        assert captured.err == ""
    else:
        assert captured.out == ""
        assert "Usage:" in captured.err


def test_same_process_loader_failure_does_not_install_signals(
    monkeypatch: pytest.MonkeyPatch,
    capfd: pytest.CaptureFixture[str],
    tmp_path: Path,
) -> None:
    missing = tmp_path / "missing_production"
    fake = FakeSignals()

    result = _call_main(
        monkeypatch,
        ["troupe", "--production", str(missing)],
        fake,
    )
    captured = capfd.readouterr()

    assert type(result) is int
    assert result == 1
    fake.assert_never_touched()
    assert captured.out == ""
    assert captured.err.count(LOAD_HEADER) == 1
    assert str(missing.resolve()) in captured.err
    assert "path-not-directory" in captured.err
    assert PHASE_PREFIX not in captured.err


@pytest.mark.parametrize(("argument", "code"), [("--help", 0), ("--bad-option", 2)])
def test_production_argparse_system_exit_propagates_before_signal_install(
    monkeypatch: pytest.MonkeyPatch,
    capfd: pytest.CaptureFixture[str],
    tmp_path: Path,
    argument: str,
    code: int,
) -> None:
    fake = FakeSignals()
    package = _copy_production(tmp_path, RECORDING_PACKAGE)
    _clear_package("recording_production")
    try:
        with pytest.raises(SystemExit) as captured_exit:
            _call_main(
                monkeypatch,
                [
                    "troupe",
                    "--production",
                    str(package),
                    "--",
                    argument,
                ],
                fake,
            )
        captured = capfd.readouterr()
        stderr = _without_ready(captured.err)

        assert captured_exit.value.code == code
        fake.assert_never_touched()
        assert LOAD_HEADER not in stderr
        assert PHASE_PREFIX not in stderr
        if code == 0:
            assert "usage: recording-production" in captured.out
            assert stderr == ""
        else:
            assert captured.out == ""
            assert "recording-production: error:" in stderr
    finally:
        _clear_package("recording_production")


def test_same_process_normal_and_lifecycle_failure_restore_both_signals(
    monkeypatch: pytest.MonkeyPatch,
    capfd: pytest.CaptureFixture[str],
    tmp_path: Path,
) -> None:
    package = _copy_production(tmp_path, RECORDING_PACKAGE)
    for mode, expected in (("cli-normal", 0), ("dual-fails", 1)):
        events = tmp_path / f"{mode}.jsonl"
        fake = FakeSignals()
        _clear_package("recording_production")
        try:
            result = _call_main(
                monkeypatch,
                [
                    "troupe",
                    "--production",
                    str(package),
                    "--",
                    "--events",
                    str(events),
                    mode,
                ],
                fake,
            )
            captured = capfd.readouterr()
        finally:
            _clear_package("recording_production")

        assert type(result) is int
        assert result == expected
        fake.assert_installed_and_restored()
        assert captured.out == ""
        stderr = _without_ready(captured.err)
        if mode == "cli-normal":
            assert stderr == ""
        else:
            assert stderr.count(PHASE_PREFIX) == 2
            assert "scene phase" in stderr
            assert "stop phase" in stderr


def test_main_uses_one_policy_loop_on_the_python_main_thread(
    monkeypatch: pytest.MonkeyPatch,
    capfd: pytest.CaptureFixture[str],
    tmp_path: Path,
) -> None:
    events = tmp_path / "loop.jsonl"
    package = _copy_production(tmp_path, RECORDING_PACKAGE)
    fake = FakeSignals()
    original_policy = asyncio.get_event_loop_policy()
    original_run = asyncio.run

    class CountingPolicy(asyncio.DefaultEventLoopPolicy):
        def __init__(self) -> None:
            super().__init__()
            self.loops: list[asyncio.AbstractEventLoop] = []

        def new_event_loop(self) -> asyncio.AbstractEventLoop:
            loop = super().new_event_loop()
            self.loops.append(loop)
            return loop

    policy = CountingPolicy()

    def forbidden_asyncio_run(*args: object, **kwargs: object) -> None:
        raise AssertionError("native main must not call asyncio.run")

    _clear_package("recording_production")
    try:
        asyncio.set_event_loop_policy(policy)
        monkeypatch.setattr(asyncio, "run", forbidden_asyncio_run)
        result = _call_main(
            monkeypatch,
            [
                "troupe",
                "--production",
                str(package),
                "--",
                "--events",
                str(events),
                "loop-record",
            ],
            fake,
        )
        captured = capfd.readouterr()
    finally:
        asyncio.set_event_loop_policy(original_policy)
        monkeypatch.setattr(asyncio, "run", original_run)
        _clear_package("recording_production")

    assert type(result) is int
    assert result == 0
    assert captured.out == ""
    assert _without_ready(captured.err) == ""
    fake.assert_installed_and_restored()
    assert len(policy.loops) == 1
    hooks = [record for record in _records(events) if record["event"] in {"start", "scene", "stop"}]
    assert [record["event"] for record in hooks] == ["start", "scene", "stop"]
    assert {record["thread"] for record in hooks} == {threading.main_thread().ident}
    assert {record["loop"] for record in hooks} == {id(policy.loops[0])}


def test_unexpected_loop_bridge_error_restores_signals_and_preserves_identity(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    events = tmp_path / "unused.jsonl"
    package = _copy_production(tmp_path, RECORDING_PACKAGE)
    fake = FakeSignals()
    loop_error = RuntimeError("loop marker")
    original_policy = asyncio.get_event_loop_policy()
    operations: list[str] = []

    class FailingPolicy(asyncio.DefaultEventLoopPolicy):
        def new_event_loop(self) -> asyncio.AbstractEventLoop:
            operations.append("new-loop")
            raise loop_error

    _clear_package("recording_production")
    try:
        asyncio.set_event_loop_policy(FailingPolicy())
        with pytest.raises(RuntimeError) as captured:
            _call_main(
                monkeypatch,
                [
                    "troupe",
                    "--production",
                    str(package),
                    "--",
                    "--events",
                    str(events),
                    "cli-normal",
                ],
                fake,
            )
    finally:
        asyncio.set_event_loop_policy(original_policy)
        _clear_package("recording_production")

    assert captured.value is loop_error
    assert operations == ["new-loop"]
    fake.assert_installed_and_restored()
    assert all(call[0] in {signal.SIGINT, signal.SIGTERM} for call in fake.calls[:2])


def test_partial_signal_install_restores_the_first_handler_without_starting_loop(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    package = _copy_production(tmp_path, RECORDING_PACKAGE)
    install_error = RuntimeError("signal install marker")
    originals = {signal.SIGINT: object(), signal.SIGTERM: object()}
    current = dict(originals)
    calls: list[tuple[signal.Signals, object]] = []
    successful_install: signal.Signals | None = None
    rejected_once = False
    loop_calls: list[str] = []

    def partial_signal(signum: signal.Signals, handler: object) -> object:
        nonlocal successful_install, rejected_once
        calls.append((signum, handler))
        if successful_install is None:
            successful_install = signum
        elif signum != successful_install and not rejected_once:
            rejected_once = True
            raise install_error
        previous = current[signum]
        current[signum] = handler
        return previous

    class CountingPolicy(asyncio.DefaultEventLoopPolicy):
        def new_event_loop(self) -> asyncio.AbstractEventLoop:
            loop_calls.append("new-loop")
            return super().new_event_loop()

    original_policy = asyncio.get_event_loop_policy()
    _clear_package("recording_production")
    try:
        asyncio.set_event_loop_policy(CountingPolicy())
        monkeypatch.setattr(signal, "signal", partial_signal)
        monkeypatch.setattr(
            sys,
            "argv",
            [
                "troupe",
                "--production",
                str(package),
                "--",
                "cli-normal",
            ],
        )
        with pytest.raises(RuntimeError) as captured:
            _native().main()
    finally:
        asyncio.set_event_loop_policy(original_policy)
        _clear_package("recording_production")

    assert captured.value is install_error
    assert rejected_once
    assert successful_install in {signal.SIGINT, signal.SIGTERM}
    assert current[successful_install] is originals[successful_install]
    assert calls[-1] == (successful_install, originals[successful_install])
    assert loop_calls == []


def _run_console(
    args: list[str],
    *,
    cwd: Path,
) -> subprocess.CompletedProcess[str]:
    assert CONSOLE.is_file(), "maturin develop must install the troupe console script"
    return subprocess.run(
        [str(CONSOLE), *args],
        cwd=cwd,
        text=True,
        encoding="utf-8",
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        timeout=TIMEOUT,
    )


def test_direct_console_help_usage_and_raw_args_from_outside_repository(tmp_path: Path) -> None:
    package = _copy_production(tmp_path, RECORDING_PACKAGE)
    help_result = _run_console(["--help"], cwd=tmp_path)
    missing_result = _run_console([], cwd=tmp_path)
    unknown_result = _run_console(["--unknown"], cwd=tmp_path)
    events = tmp_path / "args.jsonl"
    raw = ["--flag", "value", "input.txt", "--", "tail"]
    raw_result = _run_console(
        [
            "--production",
            str(package),
            "--",
            "--events",
            str(events),
            "raw-args",
            *raw,
        ],
        cwd=tmp_path,
    )

    assert help_result.returncode == 0
    assert "--production" in help_result.stdout
    assert "PACKAGE_DIR" in help_result.stdout
    assert help_result.stderr == ""
    for result in (missing_result, unknown_result):
        assert result.returncode == 2
        assert result.stdout == ""
        assert "Usage:" in result.stderr
    assert raw_result.returncode == 0
    assert raw_result.stdout == ""
    assert _without_ready(raw_result.stderr) == ""
    args_records = [record for record in _records(events) if record["event"] == "args"]
    assert args_records == [{"event": "args", "args": raw}]


def test_direct_console_phase_diagnostics_and_lifecycle_order(tmp_path: Path) -> None:
    package = _copy_production(tmp_path, RECORDING_PACKAGE)
    start_events = tmp_path / "start.jsonl"
    start_result = _run_console(
        [
            "--production",
            str(package),
            "--",
            "--events",
            str(start_events),
            "start-fails",
        ],
        cwd=tmp_path,
    )
    start_stderr = _without_ready(start_result.stderr)
    assert start_result.returncode == 1
    assert start_result.stdout == ""
    assert start_stderr.count("troupe: production failed during start phase") == 1
    assert start_stderr.count("Traceback (most recent call last):") == 1
    assert "StartBoom: start marker" in start_stderr
    assert "production.py" in start_stderr
    assert "in start" in start_stderr
    assert "ProductionFailed" not in start_stderr
    assert [record["event"] for record in _records(start_events)] == ["start"]

    dual_events = tmp_path / "dual.jsonl"
    dual_result = _run_console(
        [
            "--production",
            str(package),
            "--",
            "--events",
            str(dual_events),
            "dual-fails",
        ],
        cwd=tmp_path,
    )
    dual_stderr = _without_ready(dual_result.stderr)
    assert dual_result.returncode == 1
    assert dual_result.stdout == ""
    scene_header = "troupe: production failed during scene phase"
    stop_header = "troupe: production failed during stop phase"
    assert dual_stderr.count(scene_header) == 1
    assert dual_stderr.count(stop_header) == 1
    assert dual_stderr.count("Traceback (most recent call last):") == 2
    assert dual_stderr.index(scene_header) < dual_stderr.index(stop_header)
    scene_section, stop_section = dual_stderr.split(stop_header)
    assert "SceneBoom: scene marker" in scene_section
    assert scene_section.count("Traceback (most recent call last):") == 1
    assert "in scene" in scene_section
    assert "StopBoom" not in scene_section
    assert "StopBoom: stop marker" in stop_section
    assert stop_section.count("Traceback (most recent call last):") == 1
    assert "in stop" in stop_section
    assert "SceneBoom" not in stop_section
    assert "ProductionFailed" not in dual_stderr
    assert [record["event"] for record in _records(dual_events)] == ["start", "scene", "stop"]


@pytest.mark.parametrize(("argument", "code"), [("--help", 0), ("--bad-option", 2)])
def test_direct_console_preserves_production_argparse(
    tmp_path: Path,
    argument: str,
    code: int,
) -> None:
    package = _copy_production(tmp_path, RECORDING_PACKAGE)
    result = _run_console(
        ["--production", str(package), "--", argument],
        cwd=tmp_path,
    )
    stderr = _without_ready(result.stderr)

    assert result.returncode == code
    assert LOAD_HEADER not in stderr
    assert PHASE_PREFIX not in stderr
    if code == 0:
        assert "usage: recording-production" in result.stdout
        assert stderr == ""
    else:
        assert result.stdout == ""
        assert "recording-production: error:" in stderr


def test_direct_console_preserves_surrogateescape_argv(tmp_path: Path) -> None:
    assert CONSOLE.is_file(), "maturin develop must install the troupe console script"
    package = _copy_production(tmp_path, RECORDING_PACKAGE)
    result = subprocess.run(
        [
            os.fsencode(CONSOLE),
            b"--production",
            os.fsencode(package),
            b"--",
            b"\xff",
        ],
        cwd=tmp_path,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        timeout=TIMEOUT,
    )

    assert result.returncode == 0
    assert result.stdout == b""
    assert _without_ready_bytes(result.stderr) == b""


@pytest.mark.parametrize(
    ("package", "reason", "marker", "frame"),
    [
        (IMPORT_FAILURE_PACKAGE, "import-failed", "ImportBoom: import marker", "fail_during_import"),
        (
            CONSTRUCTION_FAILURE_PACKAGE,
            "construction-failed",
            "ConstructBoom: construct marker",
            "in __init__",
        ),
    ],
)
def test_direct_console_loader_cause_diagnostics(
    tmp_path: Path,
    package: Path,
    reason: str,
    marker: str,
    frame: str,
) -> None:
    package = _copy_production(tmp_path, package)
    result = _run_console(["--production", str(package)], cwd=tmp_path)
    stderr = _without_ready(result.stderr)

    assert result.returncode == 1
    assert result.stdout == ""
    assert stderr.count(LOAD_HEADER) == 1
    assert reason in stderr
    assert marker in stderr
    assert frame in stderr
    assert "The above exception was the direct cause" in stderr
    assert "troupe._runtime.ProductionLoadError:" in stderr
    assert PHASE_PREFIX not in stderr


def test_direct_console_missing_directory_diagnostic(tmp_path: Path) -> None:
    missing = tmp_path / "not-created"
    result = _run_console(["--production", str(missing)], cwd=tmp_path)

    assert result.returncode == 1
    assert result.stdout == ""
    assert result.stderr.count(LOAD_HEADER) == 1
    assert str(missing.resolve()) in result.stderr
    assert "path-not-directory" in result.stderr
    assert "troupe._runtime.ProductionLoadError:" in result.stderr
    assert PHASE_PREFIX not in result.stderr


class SignalChild:
    def __init__(self, process: subprocess.Popen[bytes], event_fd: int, control_fd: int) -> None:
        self.process = process
        self.event_fd = event_fd
        self.control_fd = control_fd
        self.deadline = time.monotonic() + TIMEOUT
        self.selector = selectors.DefaultSelector()
        self.selector.register(event_fd, selectors.EVENT_READ)
        self.buffer = bytearray()
        self.pending: list[dict[str, Any]] = []
        self.seen: list[dict[str, Any]] = []

    def _remaining(self) -> float:
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise AssertionError(f"timed out; events={self.seen!r}")
        return remaining

    def _parse_complete_records(self) -> None:
        while b"\n" in self.buffer:
            line, _, rest = self.buffer.partition(b"\n")
            self.buffer[:] = rest
            self.pending.append(json.loads(line))

    def next_event(self, expected: str) -> dict[str, Any]:
        while not self.pending:
            ready = self.selector.select(self._remaining())
            if not ready:
                raise AssertionError(f"timed out waiting for {expected!r}; events={self.seen!r}")
            chunk = os.read(self.event_fd, 4096)
            if not chunk:
                raise AssertionError(
                    f"child event pipe closed before {expected!r}; events={self.seen!r}"
                )
            self.buffer.extend(chunk)
            self._parse_complete_records()
        record = self.pending.pop(0)
        self.seen.append(record)
        assert record["event"] == expected
        return record

    def command(self, value: bytes) -> None:
        os.write(self.control_fd, value + b"\n")

    def finish(self) -> tuple[int, bytes, bytes]:
        stdout, stderr = self.process.communicate(timeout=self._remaining())
        while True:
            ready = self.selector.select(self._remaining())
            if not ready:
                raise AssertionError(f"timed out draining event pipe; events={self.seen!r}")
            chunk = os.read(self.event_fd, 4096)
            if not chunk:
                break
            self.buffer.extend(chunk)
            self._parse_complete_records()
        if self.buffer:
            raise AssertionError(f"partial trailing event record: {bytes(self.buffer)!r}")
        self.seen.extend(self.pending)
        self.pending.clear()
        return self.process.returncode, stdout, stderr

    def close(self) -> None:
        self.selector.close()
        os.close(self.event_fd)
        os.close(self.control_fd)
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=max(0.0, self.deadline - time.monotonic()))
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=1.0)


def _signal_child(tmp_path: Path, mode: str) -> Iterator[SignalChild]:
    assert CONSOLE.is_file(), "maturin develop must install the troupe console script"
    package = _copy_production(tmp_path, RECORDING_PACKAGE)
    event_read, event_write = os.pipe()
    control_read, control_write = os.pipe()
    process: subprocess.Popen[bytes] | None = None
    child: SignalChild | None = None
    try:
        process = subprocess.Popen(
            [
                str(CONSOLE),
                "--production",
                str(package),
                "--",
                "--events-fd",
                str(event_write),
                str(control_read),
                mode,
            ],
            cwd=tmp_path,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            pass_fds=(event_write, control_read),
            close_fds=True,
        )
        os.close(event_write)
        event_write = -1
        os.close(control_read)
        control_read = -1
        child = SignalChild(process, event_read, control_write)
        event_read = -1
        control_write = -1
        yield child
    finally:
        if child is not None:
            child.close()
        else:
            for descriptor in (event_read, event_write, control_read, control_write):
                if descriptor >= 0:
                    os.close(descriptor)
            if process is not None and process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=TIMEOUT)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=TIMEOUT)


@pytest.mark.parametrize("signum", [signal.SIGINT, signal.SIGTERM])
def test_linux_signal_cancels_scene_before_stop(
    tmp_path: Path,
    signum: signal.Signals,
) -> None:
    child_iterator = _signal_child(tmp_path, "signal-finally")
    child = next(child_iterator)
    try:
        child.next_event("start")
        child.next_event("scene-enter")
        os.kill(child.process.pid, signum)
        child.next_event("scene-finally")
        child.next_event("stop")
        returncode, stdout, stderr = child.finish()

        assert returncode == 0
        assert stdout == b""
        assert _without_ready_bytes(stderr) == b""
        assert [record["event"] for record in child.seen] == [
            "start",
            "scene-enter",
            "scene-finally",
            "stop",
        ]
    finally:
        try:
            next(child_iterator)
        except StopIteration:
            pass


def test_repeated_signal_is_idempotent_while_scene_cleans_up(tmp_path: Path) -> None:
    child_iterator = _signal_child(tmp_path, "signal-repeat")
    child = next(child_iterator)
    try:
        child.next_event("start")
        child.next_event("scene-enter")
        os.kill(child.process.pid, signal.SIGINT)
        cancel = child.next_event("cancel-caught")
        assert cancel["count"] == 1

        os.kill(child.process.pid, signal.SIGTERM)
        child.command(b"probe")
        child.next_event("probe-ack")
        assert child.process.poll() is None
        child.command(b"release")
        cleanup = child.next_event("scene-cleanup")
        child.next_event("stop")
        returncode, stdout, stderr = child.finish()

        assert cleanup["count"] == 1
        assert returncode == 0
        assert stdout == b""
        assert _without_ready_bytes(stderr) == b""
        assert [record["event"] for record in child.seen] == [
            "start",
            "scene-enter",
            "cancel-caught",
            "probe-ack",
            "scene-cleanup",
            "stop",
        ]
    finally:
        try:
            next(child_iterator)
        except StopIteration:
            pass
