from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any

import pytest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT_RELATIVE = Path("scripts/test_diagnostics_python_quality.sh")
SCRIPT = ROOT / SCRIPT_RELATIVE
FIXTURE_RELATIVE = Path("tests/fixtures/release/python-quality.json")
ARTIFACT_RELATIVE = Path("tests/fixtures/artifact_layout/nodes/V14.json")
GATE_RELATIVE = Path("tests/fixtures/diagnostic_node_gates/V14.json")


FAKE_TOOL = r"""#!/usr/bin/env python3
import json
import os
import sys
from pathlib import Path

tool = Path(sys.argv[0]).name
arguments = sys.argv[1:]
if tool == "pytest":
    mode = "pytest"
elif arguments[:2] == ["-B", "-"]:
    mode = "origin"
elif arguments[:2] == ["-m", "mypy"]:
    mode = "mypy"
elif arguments[:2] == ["-m", "mypy.stubtest"]:
    mode = "stubtest"
elif arguments[:2] == ["-m", "doctest"]:
    mode = "doctest"
else:
    mode = "unexpected"

record = {
    "mode": mode,
    "argv": [tool, *arguments],
    "cwd": os.getcwd(),
    "path": os.environ.get("PATH"),
    "pyo3_python": os.environ.get("PYO3_PYTHON"),
    "project_environment": os.environ.get("UV_PROJECT_ENVIRONMENT"),
    "pythonpath": os.environ.get("PYTHONPATH"),
    "mypy_cache": os.environ.get("MYPY_CACHE_DIR"),
    "tmpdir": os.environ.get("TMPDIR"),
    "pytest_addopts": os.environ.get("PYTEST_ADDOPTS"),
    "cargo_offline": os.environ.get("CARGO_NET_OFFLINE"),
    "pip_no_index": os.environ.get("PIP_NO_INDEX"),
    "uv_offline": os.environ.get("UV_OFFLINE"),
    "http_proxy": os.environ.get("http_proxy"),
    "https_proxy": os.environ.get("https_proxy"),
    "no_proxy": os.environ.get("NO_PROXY"),
    "lower_no_proxy": os.environ.get("no_proxy"),
    "perfetto_cache": os.environ.get("TROUPE_PERFETTO_CACHE"),
    "playwright_cache": os.environ.get("TROUPE_PLAYWRIGHT_CACHE"),
    "npm_cache": os.environ.get("TROUPE_NPM_CACHE"),
    "implicit_npm_caches": sorted(
        name
        for name, value in os.environ.items()
        if name.lower() == "npm_config_cache" and value
    ),
}
with Path(os.environ["TROUPE_PYTHON_QUALITY_LOG"]).open("a", encoding="utf-8") as stream:
    stream.write(json.dumps(record) + "\n")

if mode != "origin":
    print(f"{mode}-stdout")
    print(f"{mode}-stderr", file=sys.stderr)
if os.environ.get("TROUPE_PYTHON_QUALITY_MUTATE") == mode:
    mutation = Path(
        os.environ.get(
            "TROUPE_PYTHON_QUALITY_MUTATE_PATH", "unexpected-quality-mutation"
        )
    )
    with mutation.open("a", encoding="utf-8") as stream:
        stream.write("changed\n")
failures = json.loads(os.environ.get("TROUPE_PYTHON_QUALITY_FAILURES", "{}"))
raise SystemExit(int(failures.get(mode, 0)))
"""


def _expected_modes() -> list[dict[str, object]]:
    return [
        {"name": "pytest", "argv": ["pytest", "-q"]},
        {
            "name": "mypy",
            "argv": [
                "python",
                "-m",
                "mypy",
                "--strict",
                "--show-error-codes",
                "tests/typing/positive.py",
            ],
        },
        {
            "name": "stubtest",
            "argv": ["python", "-m", "mypy.stubtest", "troupe", "--concise"],
        },
        {
            "name": "doctest",
            "argv": ["python", "-m", "doctest", "README.md"],
        },
    ]


def _sandbox(tmp_path: Path) -> tuple[Path, dict[str, str], Path, Path]:
    repository = (tmp_path / "repository").resolve()
    script = repository / SCRIPT_RELATIVE
    script.parent.mkdir(parents=True)
    shutil.copy2(SCRIPT, script)
    script.chmod(0o755)
    (repository / "tests/typing").mkdir(parents=True)
    (repository / "tests/typing/positive.py").write_text("value: int = 1\n", encoding="ascii")
    (repository / "README.md").write_text("fixture\n", encoding="ascii")
    subprocess.run(["git", "init", "-q", str(repository)], check=True)
    subprocess.run(["git", "-C", str(repository), "add", "."], check=True)
    subprocess.run(
        [
            "git",
            "-C",
            str(repository),
            "-c",
            "user.name=V14 Test",
            "-c",
            "user.email=v14@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
        check=True,
    )

    venv = (tmp_path / "f03" / "venv").resolve()
    tools = venv / "bin"
    tools.mkdir(parents=True)
    for name in ("python", "pytest"):
        executable = tools / name
        executable.write_text(FAKE_TOOL, encoding="utf-8")
        executable.chmod(0o755)
    troupe = tools / "troupe"
    troupe.write_text("#!/bin/sh\nexit 0\n", encoding="ascii")
    troupe.chmod(0o755)

    log = tmp_path / "python-quality.jsonl"
    temporary = tmp_path / "temporary"
    temporary.mkdir()
    (temporary / "caller-owned").write_text("preserve\n", encoding="ascii")
    environment = dict(os.environ)
    environment.update(
        {
            "PATH": f"{tools}:{environment['PATH']}",
            "PYO3_PYTHON": str(tools / "python"),
            "TMPDIR": str(temporary),
            "TROUPE_GATE_TMP": str(temporary),
            "TROUPE_NPM_CACHE": str(tmp_path / "inherited-npm-cache"),
            "NpM_CoNfIg_CaChE": str(tmp_path / "implicit-npm-cache"),
            "TROUPE_PERFETTO_CACHE": str(tmp_path / "inherited-perfetto-cache"),
            "TROUPE_PLAYWRIGHT_CACHE": str(tmp_path / "inherited-playwright-cache"),
            "TROUPE_PYTHON_QUALITY_LOG": str(log),
            "UV_PROJECT_ENVIRONMENT": str(venv),
        }
    )
    environment.pop("PYTHONPATH", None)
    return repository, environment, log, venv


def _run(
    repository: Path,
    environment: dict[str, str],
    arguments: list[str] | None = None,
) -> subprocess.CompletedProcess[str]:
    effective_arguments = ["--all"] if arguments is None else arguments
    return subprocess.run(
        [str(repository / SCRIPT_RELATIVE), *effective_arguments],
        cwd=repository,
        env=environment,
        check=False,
        capture_output=True,
        text=True,
        timeout=30,
    )


def _records(log: Path) -> list[dict[str, Any]]:
    if not log.exists():
        return []
    return [json.loads(line) for line in log.read_text(encoding="utf-8").splitlines()]


def _summary(result: subprocess.CompletedProcess[str]) -> dict[str, Any]:
    lines = result.stdout.splitlines()
    assert len(lines) == 1
    value = json.loads(lines[0])
    assert isinstance(value, dict)
    return value


def test_checked_in_contract_and_descriptors_are_exact() -> None:
    assert json.loads((ROOT / FIXTURE_RELATIVE).read_text(encoding="utf-8")) == {
        "schema": "troupe.diagnostics.python-quality.v1",
        "isolation": {
            "provider": "F03",
            "wheel": "current-worktree",
            "origin": "validated",
            "network": "forbidden",
        },
        "modes": _expected_modes(),
    }
    assert json.loads((ROOT / ARTIFACT_RELATIVE).read_text(encoding="utf-8")) == {
        "state": "realized",
        "introduced": [
            "scripts/test_diagnostics_python_quality.sh",
            "tests/fixtures/release/python-quality.json",
            "tests/unit/test_diagnostics_python_quality_runner.py",
        ],
        "modified": [],
        "removed": [],
        "generated": [],
    }
    assert json.loads((ROOT / GATE_RELATIVE).read_text(encoding="utf-8")) == {
        "state": "realized",
        "argv": [
            ["pytest", "-q", "tests/unit/test_diagnostics_python_quality_runner.py"],
            ["scripts/test_diagnostics_python_quality.sh", "--all"],
        ],
        "env": {"TROUPE_GATE_TMP": "forbidden"},
        "maturin_features": [
            "agent-test-support",
            "diagnostics-test-support",
        ],
        "cache_requirements": [],
        "exclusive_resources": [],
    }


def test_all_runs_exact_offline_modes_and_emits_one_summary(tmp_path: Path) -> None:
    repository, environment, log, venv = _sandbox(tmp_path)

    result = _run(repository, environment)

    assert result.returncode == 0, result.stderr
    records = _records(log)
    assert [record["mode"] for record in records] == [
        "origin",
        "pytest",
        "mypy",
        "stubtest",
        "doctest",
    ]
    assert [record["argv"] for record in records[1:]] == [
        mode["argv"] for mode in _expected_modes()
    ]
    assert {record["cwd"] for record in records} == {str(repository)}
    assert all(record["path"].split(os.pathsep)[0] == str(venv / "bin") for record in records)
    assert {record["pyo3_python"] for record in records} == {str(venv / "bin/python")}
    assert {record["project_environment"] for record in records} == {str(venv)}
    assert {record["pythonpath"] for record in records} == {None}
    assert {record["pytest_addopts"] for record in records} == {"-p no:cacheprovider"}
    assert {record["cargo_offline"] for record in records} == {"true"}
    assert {record["pip_no_index"] for record in records} == {"1"}
    assert {record["uv_offline"] for record in records} == {"1"}
    assert {record["http_proxy"] for record in records} == {"http://127.0.0.1:9/"}
    assert {record["https_proxy"] for record in records} == {"http://127.0.0.1:9/"}
    assert {record["no_proxy"] for record in records} == {"localhost,127.0.0.1,::1"}
    assert {record["lower_no_proxy"] for record in records} == {
        "localhost,127.0.0.1,::1"
    }
    assert {record["perfetto_cache"] for record in records} == {None}
    assert {record["playwright_cache"] for record in records} == {None}
    assert {record["npm_cache"] for record in records} == {None}
    assert all(record["implicit_npm_caches"] == [] for record in records)
    mypy_caches = {record["mypy_cache"] for record in records}
    assert len(mypy_caches) == 1
    mypy_cache = Path(mypy_caches.pop())
    assert mypy_cache.parent.parent == Path(environment["TMPDIR"])
    child_tmpdirs = {Path(record["tmpdir"]) for record in records}
    assert len(child_tmpdirs) == 1
    assert child_tmpdirs.pop().parent == mypy_cache.parent
    assert not mypy_cache.parent.exists()
    assert {path.name for path in Path(environment["TMPDIR"]).iterdir()} == {"caller-owned"}
    summary = _summary(result)
    assert summary["mode"] == "all"
    assert summary["isolated_origin"] is True
    assert summary["offline"] is True
    assert summary["result"] == "passed"
    assert summary["first_failed_mode"] is None
    assert summary["checkout_unchanged"] is True
    assert [mode["name"] for mode in summary["modes"]] == [
        "pytest",
        "mypy",
        "stubtest",
        "doctest",
    ]
    assert all(mode["status"] == "passed" for mode in summary["modes"])
    assert "pytest-stdout" in result.stderr
    assert "doctest-stderr" in result.stderr


@pytest.mark.parametrize("mode", ["pytest", "mypy", "stubtest", "doctest"])
def test_each_mode_runs_individually(tmp_path: Path, mode: str) -> None:
    repository, environment, log, _ = _sandbox(tmp_path)

    result = _run(repository, environment, [f"--{mode}"])

    assert result.returncode == 0, result.stderr
    assert [record["mode"] for record in _records(log)] == ["origin", mode]
    summary = _summary(result)
    assert summary["mode"] == mode
    assert [value["name"] for value in summary["modes"]] == [mode]


def test_all_modes_run_but_first_failure_controls_exit(tmp_path: Path) -> None:
    repository, environment, log, _ = _sandbox(tmp_path)
    environment["TROUPE_PYTHON_QUALITY_FAILURES"] = json.dumps(
        {"mypy": 23, "stubtest": 29}
    )

    result = _run(repository, environment)

    assert result.returncode == 23
    assert [record["mode"] for record in _records(log)[1:]] == [
        "pytest",
        "mypy",
        "stubtest",
        "doctest",
    ]
    summary = _summary(result)
    assert summary["result"] == "failed"
    assert summary["first_failed_mode"] == "mypy"
    assert [mode["exit_code"] for mode in summary["modes"]] == [0, 23, 29, 0]


def test_origin_failure_stops_before_quality_modes(tmp_path: Path) -> None:
    repository, environment, log, _ = _sandbox(tmp_path)
    environment["TROUPE_PYTHON_QUALITY_FAILURES"] = json.dumps({"origin": 41})

    result = _run(repository, environment)

    assert result.returncode == 41
    assert result.stdout == ""
    assert "requires F03 isolated wheel environment" in result.stderr
    assert [record["mode"] for record in _records(log)] == ["origin"]
    assert {path.name for path in Path(environment["TMPDIR"]).iterdir()} == {"caller-owned"}


def test_repository_local_temporary_base_is_rejected_before_tools(tmp_path: Path) -> None:
    repository, environment, log, _ = _sandbox(tmp_path)
    local = repository / "temporary"
    local.mkdir()
    environment["TROUPE_GATE_TMP"] = str(local)

    result = _run(repository, environment)

    assert result.returncode == 1
    assert result.stdout == ""
    assert "must remain outside the repository" in result.stderr
    assert _records(log) == []


def test_checkout_mutation_is_blocking_after_all_mode_results(tmp_path: Path) -> None:
    repository, environment, log, _ = _sandbox(tmp_path)
    environment["TROUPE_PYTHON_QUALITY_MUTATE"] = "doctest"

    result = _run(repository, environment)

    assert result.returncode == 1
    assert len(_records(log)) == 5
    summary = _summary(result)
    assert summary["first_failed_mode"] == "checkout"
    assert summary["checkout_unchanged"] is False
    assert all(mode["status"] == "passed" for mode in summary["modes"])


def test_mutation_of_already_dirty_tracked_file_is_detected(tmp_path: Path) -> None:
    repository, environment, log, _ = _sandbox(tmp_path)
    readme = repository / "README.md"
    readme.write_text("caller-owned dirty state\n", encoding="ascii")
    environment.update(
        {
            "TROUPE_PYTHON_QUALITY_MUTATE": "doctest",
            "TROUPE_PYTHON_QUALITY_MUTATE_PATH": "README.md",
        }
    )

    result = _run(repository, environment)

    assert result.returncode == 1
    assert len(_records(log)) == 5
    assert _summary(result)["checkout_unchanged"] is False
    assert readme.read_text(encoding="ascii").endswith("changed\n")


@pytest.mark.parametrize(
    "arguments",
    [
        [],
        ["--all", "extra"],
        ["all"],
        ["--unknown"],
    ],
)
def test_invalid_cli_is_usage_error_without_running_tools(
    tmp_path: Path, arguments: list[str]
) -> None:
    repository, environment, log, _ = _sandbox(tmp_path)

    result = _run(repository, environment, arguments)

    assert result.returncode == 2
    assert result.stdout == ""
    assert "usage:" in result.stderr
    assert _records(log) == []


def test_runner_has_no_build_release_or_external_tool_boundary() -> None:
    source = SCRIPT.read_text(encoding="utf-8")

    for forbidden in (
        "maturin ",
        "cargo build",
        "cargo test",
        "uv sync",
        "uv pip",
        "pip install",
        "npm ",
        "playwright",
        "perfetto",
        "curl ",
        "wget ",
    ):
        assert forbidden not in source
    assert "UV_PROJECT_ENVIRONMENT" in source
    assert "PYO3_PYTHON" in source
    assert 'repository / ".venv"' in source
    assert "EXTENSION_SUFFIXES" in source
