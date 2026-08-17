from __future__ import annotations

import json
import os
import shutil
import signal
import stat
import subprocess
import sys
import time
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT_RELATIVE = Path("scripts/test_diagnostics_e2e.sh")
SCRIPT = ROOT / SCRIPT_RELATIVE

FAKE_GATE = r'''#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import shutil
import signal
import sys
from pathlib import Path


def append(record: dict[str, object]) -> None:
    with Path(os.environ["TROUPE_E2E_CHILD_LOG"]).open("a", encoding="utf-8") as stream:
        stream.write(json.dumps(record, sort_keys=True) + "\n")


if len(sys.argv) != 2 or sys.argv[1] not in {"V02", "V06"}:
    raise SystemExit(2)

node = sys.argv[1]
gate_tmp = Path(os.environ["TROUPE_GATE_TMP"])
owned = gate_tmp / f"owned-{node}-{os.getpid()}"
ready = gate_tmp / f"ready-{node}"
owned.mkdir()
append(
    {
        "argv": sys.argv[1:],
        "event": "start",
        "gate_tmp": str(gate_tmp),
        "node": node,
        "pid": os.getpid(),
    }
)
behavior = json.loads(os.environ.get("TROUPE_E2E_CHILD_BEHAVIOR", "{}"))
selected = behavior.get(node, 0)


def interrupted(signum: int, _frame: object) -> None:
    append({"event": "signal", "node": node, "signal": signum})
    raise SystemExit(128 + signum)


try:
    if selected == "wait":
        signal.signal(signal.SIGINT, interrupted)
        signal.signal(signal.SIGTERM, interrupted)
        ready.write_text("ready\n", encoding="ascii")
        while True:
            signal.pause()
    if type(selected) is not int or not 0 <= selected <= 255:
        raise SystemExit(2)
    raise SystemExit(selected)
finally:
    ready.unlink(missing_ok=True)
    shutil.rmtree(owned)
'''


def _sandbox(tmp_path: Path) -> tuple[Path, dict[str, str], Path, Path]:
    repository = tmp_path / "repository"
    scripts = repository / "scripts"
    scripts.mkdir(parents=True)
    shutil.copy2(SCRIPT, repository / SCRIPT_RELATIVE)
    fake_gate = scripts / "run_diagnostic_node_gate.sh"
    fake_gate.write_text(FAKE_GATE, encoding="utf-8")
    fake_gate.chmod(0o755)

    gate_tmp = tmp_path / "gate-tmp"
    gate_tmp.mkdir()
    log = tmp_path / "children.jsonl"
    environment = dict(os.environ)
    environment.pop("TROUPE_DIAGNOSTIC_BOOTSTRAP_GATE", None)
    environment.update(
        {
            "PYTHONDONTWRITEBYTECODE": "1",
            "TROUPE_E2E_CHILD_LOG": str(log),
            "TROUPE_GATE_TMP": str(gate_tmp),
        }
    )
    return repository, environment, log, gate_tmp


def _run(
    repository: Path,
    environment: dict[str, str],
    arguments: list[str],
    *,
    timeout: float = 10,
) -> tuple[int, int, str, str]:
    process = subprocess.Popen(
        [str(repository / SCRIPT_RELATIVE), *arguments],
        cwd=repository,
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    stdout, stderr = process.communicate(timeout=timeout)
    return process.returncode, process.pid, stdout, stderr


def _records(log: Path) -> list[dict[str, object]]:
    if not log.exists():
        return []
    return [json.loads(line) for line in log.read_text(encoding="utf-8").splitlines()]


@pytest.mark.parametrize(
    ("mode", "node", "exit_code"),
    [
        ("--happy-path", "V02", 0),
        ("--failures", "V06", 1),
        ("--happy-path", "V02", 2),
        ("--failures", "V06", 130),
    ],
)
def test_single_modes_exec_exact_gate_and_preserve_exit(
    tmp_path: Path,
    mode: str,
    node: str,
    exit_code: int,
) -> None:
    repository, environment, log, gate_tmp = _sandbox(tmp_path)
    environment["TROUPE_E2E_CHILD_BEHAVIOR"] = json.dumps({node: exit_code})

    actual, process_pid, stdout, stderr = _run(repository, environment, [mode])

    assert (actual, stdout, stderr) == (exit_code, "", "")
    assert _records(log) == [
        {
            "argv": [node],
            "event": "start",
            "gate_tmp": str(gate_tmp),
            "node": node,
            "pid": process_pid,
        }
    ]
    assert list(gate_tmp.iterdir()) == []


def test_all_runs_both_in_fixed_order_and_preserves_first_failure(tmp_path: Path) -> None:
    repository, environment, log, gate_tmp = _sandbox(tmp_path)
    environment["TROUPE_E2E_CHILD_BEHAVIOR"] = json.dumps({"V02": 23, "V06": 29})

    actual, _process_pid, stdout, stderr = _run(repository, environment, ["--all"])

    assert (actual, stdout, stderr) == (23, "", "")
    starts = [record for record in _records(log) if record["event"] == "start"]
    assert [record["node"] for record in starts] == ["V02", "V06"]
    assert [record["argv"] for record in starts] == [["V02"], ["V06"]]
    assert all(record["gate_tmp"] == str(gate_tmp) for record in starts)
    assert list(gate_tmp.iterdir()) == []


def test_all_returns_zero_only_when_both_children_pass(tmp_path: Path) -> None:
    repository, environment, log, gate_tmp = _sandbox(tmp_path)

    actual, _process_pid, stdout, stderr = _run(repository, environment, ["--all"])

    assert (actual, stdout, stderr) == (0, "", "")
    assert [record["node"] for record in _records(log)] == ["V02", "V06"]
    assert list(gate_tmp.iterdir()) == []


@pytest.mark.parametrize(
    "arguments",
    [
        [],
        ["--unknown"],
        ["--happy-path", "--failures"],
        ["--all", "extra"],
    ],
)
def test_argument_surface_is_closed(tmp_path: Path, arguments: list[str]) -> None:
    repository, environment, log, gate_tmp = _sandbox(tmp_path)

    actual, _process_pid, stdout, stderr = _run(repository, environment, arguments)

    assert (actual, stdout) == (2, "")
    assert stderr == "usage: test_diagnostics_e2e.sh (--happy-path|--failures|--all)\n"
    assert _records(log) == []
    assert list(gate_tmp.iterdir()) == []


def test_all_forwards_interrupt_and_waits_for_child_cleanup(tmp_path: Path) -> None:
    repository, environment, log, gate_tmp = _sandbox(tmp_path)
    environment["TROUPE_E2E_CHILD_BEHAVIOR"] = json.dumps({"V02": "wait"})
    process = subprocess.Popen(
        [str(repository / SCRIPT_RELATIVE), "--all"],
        cwd=repository,
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    deadline = time.monotonic() + 5
    ready = gate_tmp / "ready-V02"
    try:
        while not ready.exists():
            assert process.poll() is None
            assert time.monotonic() < deadline
            time.sleep(0.01)
        process.send_signal(signal.SIGINT)
        stdout, stderr = process.communicate(timeout=5)
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=5)

    assert (process.returncode, stdout, stderr) == (130, "", "")
    records = _records(log)
    assert [record["event"] for record in records] == ["start", "signal"]
    assert records[1] == {"event": "signal", "node": "V02", "signal": signal.SIGINT}
    assert list(gate_tmp.iterdir()) == []


def test_script_is_executable_and_does_not_own_child_assertions() -> None:
    assert stat.S_IMODE(SCRIPT.stat().st_mode) == 0o755
    source = SCRIPT.read_text(encoding="utf-8")
    assert "run_diagnostic_node_gate.sh" in source
    assert "tests/e2e/diagnostics/" not in source
    assert "tests/e2e/diagnostics_failures/" not in source


def _bootstrap_fake_child(arguments: list[str]) -> int:
    if (
        os.environ.get("TROUPE_DIAGNOSTIC_BOOTSTRAP_GATE") != "1"
        or len(arguments) != 2
        or arguments[0] != "--fake-child"
        or arguments[1] not in {"V02", "V06"}
    ):
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(_bootstrap_fake_child(sys.argv[1:]))
