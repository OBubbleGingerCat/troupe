from __future__ import annotations

import json
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
SUPPORT = ROOT / "tests" / "support"
sys.path.insert(0, str(SUPPORT))

from artifact_layout import load_artifact_layout, load_gate_descriptors  # noqa: E402
import diagnostic_bootstrap_gate as bootstrap  # noqa: E402


def test_f00_contract_uses_separate_indexed_artifact_and_gate_families() -> None:
    layout = load_artifact_layout(ROOT)
    gates = load_gate_descriptors(ROOT)

    assert len(layout.node_ids) == 145
    assert tuple(layout.fragments) == layout.node_ids
    assert tuple(gates) == layout.node_ids
    assert layout.fragments["F00"].state == "realized"
    assert gates["F00"].state == "realized"
    assert gates["F00"].argv == (
        (
            "pytest",
            "-q",
            "tests/unit/test_artifact_layout.py",
            "tests/unit/test_diagnostic_ownership.py",
            "tests/unit/test_diagnostic_bootstrap_gate.py",
            "tests/unit/test_release_script.py",
        ),
    )
    assert all(path.startswith("tests/") or path.startswith("scripts/") for path in layout.paths)


def _copy_contract(tmp_path: Path) -> Path:
    repository = tmp_path / "repository"
    artifact_target = repository / "tests/fixtures/artifact_layout"
    gate_target = repository / "tests/fixtures/diagnostic_node_gates"
    artifact_target.parent.mkdir(parents=True)
    shutil.copytree(ROOT / "tests/fixtures/artifact_layout", artifact_target)
    shutil.copytree(ROOT / "tests/fixtures/diagnostic_node_gates", gate_target)
    return repository


def test_runner_uses_fresh_external_environment_and_structured_argv(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    repository = _copy_contract(tmp_path)
    external = tmp_path / "external"
    external.mkdir()
    calls: list[tuple[list[str], Path, dict[str, str]]] = []

    def fake_run(
        command: list[str],
        *,
        cwd: Path,
        env: dict[str, str],
        check: bool,
    ) -> subprocess.CompletedProcess[str]:
        assert check is True
        calls.append((command, cwd, env.copy()))
        return subprocess.CompletedProcess(command, 0)

    monkeypatch.setattr(bootstrap.subprocess, "run", fake_run)
    bootstrap.run_bootstrap_gate(
        repository,
        "F00",
        environ={"PATH": "/usr/bin:/bin", "TROUPE_GATE_TMP": str(external)},
    )

    assert calls[0][0] == [
        "uv",
        "sync",
        "--frozen",
        "--all-groups",
        "--no-install-project",
    ]
    assert calls[1][0][1:] == [
        "-q",
        "tests/unit/test_artifact_layout.py",
        "tests/unit/test_diagnostic_ownership.py",
        "tests/unit/test_diagnostic_bootstrap_gate.py",
        "tests/unit/test_release_script.py",
    ]
    assert Path(calls[1][0][0]).name == "pytest"
    assert all(cwd == repository.resolve() for _, cwd, _ in calls)
    assert calls[0][2]["UV_PROJECT_ENVIRONMENT"].startswith(str(external))
    assert calls[0][2]["UV_CACHE_DIR"].startswith(str(external))
    assert list(external.iterdir()) == []


def test_runner_cleans_owned_temp_after_child_failure(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    repository = _copy_contract(tmp_path)
    external = tmp_path / "external"
    external.mkdir()
    call_count = 0

    def fake_run(
        command: list[str],
        *,
        cwd: Path,
        env: dict[str, str],
        check: bool,
    ) -> subprocess.CompletedProcess[str]:
        nonlocal call_count
        call_count += 1
        if call_count == 2:
            raise subprocess.CalledProcessError(23, command)
        return subprocess.CompletedProcess(command, 0)

    monkeypatch.setattr(bootstrap.subprocess, "run", fake_run)
    with pytest.raises(bootstrap.BootstrapGateError, match="exit code 23"):
        bootstrap.run_bootstrap_gate(
            repository,
            "F00",
            environ={"PATH": "/usr/bin:/bin", "TROUPE_GATE_TMP": str(external)},
        )
    assert list(external.iterdir()) == []


def test_runner_rejects_native_console_invocation(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    repository = _copy_contract(tmp_path)
    external = tmp_path / "external"
    external.mkdir()
    gate_path = repository / "tests/fixtures/diagnostic_node_gates/F00.json"
    gate = json.loads(gate_path.read_text(encoding="utf-8"))
    gate["argv"] = [["troupe", "diagnostic", "status"]]
    gate_path.write_text(json.dumps(gate, indent=2) + "\n", encoding="utf-8")

    monkeypatch.setattr(
        bootstrap.subprocess,
        "run",
        lambda command, **kwargs: subprocess.CompletedProcess(command, 0),
    )
    with pytest.raises(bootstrap.BootstrapGateError, match="must not invoke"):
        bootstrap.run_bootstrap_gate(
            repository,
            "F00",
            environ={"PATH": "/usr/bin:/bin", "TROUPE_GATE_TMP": str(external)},
        )
    assert list(external.iterdir()) == []
