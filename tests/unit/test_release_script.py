from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import NamedTuple

import pytest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "test_linux_release.sh"
WORKFLOWS = ROOT / ".github" / "workflows"

VERSIONS = ["3.10", "3.11", "3.12", "3.13", "3.14"]
QUALITY_VERSIONS = ["3.10", "3.14"]
WHEEL_NAME = "troupe-0.1.0-cp310-abi3-manylinux_2_17_x86_64.whl"
CALLER_LIBRARY_PATH = "/caller/lib-one:/caller/lib-two"
CALLER_CONDA_PREFIX = "/caller/conda"
CALLER_PYTHON_HOME = sys.base_prefix


class Call(NamedTuple):
    tool: str
    python: str
    project: str
    preference: str
    pyo3_python: str
    library_path: str
    conda_present: str
    conda_prefix: str
    python_home_present: str
    python_home: str
    cwd: str
    arguments: list[str]


def _script() -> str:
    return SCRIPT.read_text(encoding="utf-8")


def _sandbox_script(tmp_path: Path) -> tuple[Path, Path]:
    sandbox = tmp_path / "repository"
    script = sandbox / "scripts" / SCRIPT.name
    script.parent.mkdir(parents=True)
    shutil.copy2(SCRIPT, script)
    script.chmod(0o755)
    return sandbox, script


def _fake_tools(tmp_path: Path, sandbox: Path) -> tuple[dict[str, str], Path, Path]:
    tools = tmp_path / "fake-bin"
    tools.mkdir()
    temporary = tmp_path / "temporary"
    temporary.mkdir()
    (temporary / "caller-owned-sentinel").write_text("preserve\n", encoding="utf-8")
    log = tmp_path / "calls.bin"
    implementation = """#!/usr/bin/env bash
set -euo pipefail
tool="$(basename "$0")"
env -u PYTHONHOME "$TROUPE_RELEASE_TEST_PYTHON" - "$TROUPE_RELEASE_TEST_LOG" "$tool" "${UV_PYTHON-}" "${UV_PROJECT_ENVIRONMENT-}" "${UV_PYTHON_PREFERENCE-}" "${PYO3_PYTHON-}" "${LD_LIBRARY_PATH-}" "${CONDA_PREFIX+x}" "${CONDA_PREFIX-}" "${PYTHONHOME+x}" "${PYTHONHOME-}" "$PWD" "$@" <<'PY'
import json
import sys

with open(sys.argv[1], "a", encoding="utf-8") as stream:
    stream.write(json.dumps(sys.argv[2:]) + chr(10))
PY
if [[ "$tool" == uv && "${1-}" == sync ]]; then
  if [[ -z "${TMPDIR-}" || -z "${UV_PROJECT_ENVIRONMENT-}" ]]; then
    exit 96
  fi
  case "$UV_PROJECT_ENVIRONMENT" in
    "$TMPDIR"/*) ;;
    *) exit 96 ;;
  esac
  managed_root="$TMPDIR/fake-managed/$UV_PYTHON"
  mkdir -p "$UV_PROJECT_ENVIRONMENT/bin" "$managed_root/bin" "$managed_root/lib"
  : > "$UV_PROJECT_ENVIRONMENT/sync-marker"
  printf '#!/usr/bin/env bash\\nexit 0\\n' > "$managed_root/bin/python"
  chmod +x "$managed_root/bin/python"
  ln -s "$managed_root/bin/python" "$UV_PROJECT_ENVIRONMENT/bin/python"
fi
if [[ "$tool" == cargo && ! -x "${PYO3_PYTHON-}" ]]; then
  exit 97
fi
if [[ "$tool" == cargo && "${1-}" == test ]]; then
  managed_python="$(readlink -f -- "$PYO3_PYTHON")"
  managed_home="$(dirname -- "$(dirname -- "$managed_python")")"
  if [[ "${PYTHONHOME-}" != "$managed_home" ]]; then
    exit 95
  fi
fi
if [[ "${TROUPE_RELEASE_FAIL_TOOL-}" == "$tool" \
      && "${TROUPE_RELEASE_FAIL_PYTHON-}" == "${UV_PYTHON-}" \
      && "${TROUPE_RELEASE_FAIL_COMMAND-}" == "${1-}" ]]; then
  exit "${TROUPE_RELEASE_FAIL_CODE:-23}"
fi
if [[ "$tool" == docker ]]; then
  mkdir -p "$TROUPE_RELEASE_TEST_ROOT/wheel-artifact"
  : > "$TROUPE_RELEASE_TEST_ROOT/wheel-artifact/$TROUPE_RELEASE_TEST_WHEEL"
  printf '%064d  %s\\n' 0 "$TROUPE_RELEASE_TEST_WHEEL" \
    > "$TROUPE_RELEASE_TEST_ROOT/wheel-artifact/SHA256SUMS"
fi
"""
    for name in ("uv", "cargo", "docker"):
        executable = tools / name
        executable.write_text(implementation, encoding="utf-8")
        executable.chmod(0o755)
    env = dict(os.environ)
    for name in list(env):
        if name in {
            "UV_PYTHON",
            "UV_PROJECT_ENVIRONMENT",
            "UV_PYTHON_PREFERENCE",
            "PYO3_PYTHON",
        } or name.startswith("TROUPE_RELEASE_"):
            env.pop(name)
    env.update(
        {
            "PATH": f"{tools}:/usr/bin:/bin",
            "LD_LIBRARY_PATH": CALLER_LIBRARY_PATH,
            "CONDA_PREFIX": CALLER_CONDA_PREFIX,
            "PYTHONHOME": CALLER_PYTHON_HOME,
            "TROUPE_RELEASE_TEST_LOG": str(log),
            "TROUPE_RELEASE_TEST_PYTHON": sys.executable,
            "TROUPE_RELEASE_TEST_ROOT": str(sandbox),
            "TROUPE_RELEASE_TEST_WHEEL": WHEEL_NAME,
            "TMPDIR": str(temporary),
        }
    )
    return env, log, tools


def _calls(log: Path) -> list[Call]:
    if not log.exists():
        return []
    calls = []
    for line in log.read_text(encoding="utf-8").splitlines():
        fields = json.loads(line)
        calls.append(
            Call(
                fields[0],
                fields[1],
                fields[2],
                fields[3],
                fields[4],
                fields[5],
                fields[6],
                fields[7],
                fields[8],
                fields[9],
                fields[10],
                fields[11:],
            )
        )
    return calls


def _run(
    script: Path,
    arguments: list[str],
    *,
    cwd: Path,
    env: dict[str, str],
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [str(script), *arguments],
        cwd=cwd,
        env=env,
        check=False,
        capture_output=True,
        text=True,
    )


def _make_artifact(sandbox: Path) -> tuple[Path, Path]:
    artifact = sandbox / "wheel-artifact"
    artifact.mkdir()
    wheel = artifact / WHEEL_NAME
    checksum = artifact / "SHA256SUMS"
    wheel.touch()
    checksum.write_text(f"{'0' * 64}  {WHEEL_NAME}\n", encoding="ascii")
    return wheel, checksum


def _assert_tmpdir_preserved(env: dict[str, str]) -> None:
    temporary = Path(env["TMPDIR"])
    assert temporary.is_dir()
    assert (temporary / "caller-owned-sentinel").read_text(encoding="utf-8") == "preserve\n"
    expected = {"caller-owned-sentinel"}
    if (temporary / "fake-managed").exists():
        expected.add("fake-managed")
    assert {path.name for path in temporary.iterdir()} == expected


def _assert_managed_pythons_preserved(
    env: dict[str, str],
    versions: list[str],
) -> None:
    managed = Path(env["TMPDIR"]) / "fake-managed"
    for version in versions:
        assert (managed / version / "bin" / "python").is_file()
        assert (managed / version / "lib").is_dir()


def _assert_quality_command_group(
    calls: list[Call],
) -> None:
    assert [(call.tool, call.arguments) for call in calls] == [
        ("uv", ["sync", "--frozen", "--all-groups"]),
        ("cargo", ["fmt", "--check", "--manifest-path", "rust/Cargo.toml"]),
        (
            "cargo",
            [
                "clippy",
                "--locked",
                "--manifest-path",
                "rust/Cargo.toml",
                "--all-targets",
                "--all-features",
                "--",
                "-D",
                "warnings",
            ],
        ),
        ("cargo", ["test", "--locked", "--manifest-path", "rust/Cargo.toml"]),
        (
            "uv",
            [
                "run",
                "--no-sync",
                "maturin",
                "develop",
                "--uv",
                "--locked",
                "--manifest-path",
                "rust/Cargo.toml",
            ],
        ),
        ("uv", ["run", "--no-sync", "pytest", "-q"]),
        ("uv", ["run", "--no-sync", "python", "-m", "doctest", "README.md"]),
    ]


def _assert_quality_calls(
    calls: list[Call],
    caller_library_path: str = CALLER_LIBRARY_PATH,
    caller_python_home: str | None = CALLER_PYTHON_HOME,
) -> list[Path]:
    caller_python_home_present = "x" if caller_python_home is not None else ""
    caller_python_home_value = caller_python_home or ""
    assert calls[0]._replace(cwd="") == Call(
        "uv",
        "",
        "",
        "",
        "",
        caller_library_path,
        "x",
        CALLER_CONDA_PREFIX,
        caller_python_home_present,
        caller_python_home_value,
        "",
        ["python", "install", *QUALITY_VERSIONS],
    )
    assert len(calls) == 15
    environments: list[Path] = []
    for index, version in enumerate(QUALITY_VERSIONS):
        group = calls[1 + index * 7 : 1 + (index + 1) * 7]
        _assert_quality_command_group(group)
        assert {call.python for call in group} == {version}
        assert {call.preference for call in group} == {"only-managed"}
        projects = {call.project for call in group}
        assert len(projects) == 1
        environment = Path(projects.pop())
        assert environment.is_absolute()
        assert environment.name == version
        assert {call.pyo3_python for call in group} == {
            str(environment / "bin" / "python")
        }
        assert group[0].library_path == caller_library_path
        managed_library = environment.parent.parent / "fake-managed" / version / "lib"
        expected_library_path = str(managed_library)
        if caller_library_path:
            expected_library_path = f"{expected_library_path}:{caller_library_path}"
        assert {call.library_path for call in group[1:]} == {
            expected_library_path
        }
        assert [call.conda_present for call in group] == [
            "x",
            "x",
            "x",
            "x",
            "",
            "x",
            "x",
        ]
        assert [call.conda_prefix for call in group] == [
            CALLER_CONDA_PREFIX,
            CALLER_CONDA_PREFIX,
            CALLER_CONDA_PREFIX,
            CALLER_CONDA_PREFIX,
            "",
            CALLER_CONDA_PREFIX,
            CALLER_CONDA_PREFIX,
        ]
        managed_home = environment.parent.parent / "fake-managed" / version
        assert [call.python_home_present for call in group] == [
            caller_python_home_present,
            caller_python_home_present,
            caller_python_home_present,
            "x",
            caller_python_home_present,
            caller_python_home_present,
            caller_python_home_present,
        ]
        assert [call.python_home for call in group] == [
            caller_python_home_value,
            caller_python_home_value,
            caller_python_home_value,
            str(managed_home),
            caller_python_home_value,
            caller_python_home_value,
            caller_python_home_value,
        ]
        environments.append(environment)
    assert len({environment.parent for environment in environments}) == 1
    return environments


def test_release_checks_are_script_owned_and_no_workflow_is_committed() -> None:
    assert not [path for path in WORKFLOWS.rglob("*") if path.is_file()]
    assert SCRIPT.is_file()
    assert SCRIPT.stat().st_mode & 0o111
    assert _script().startswith("#!/usr/bin/env bash\nset -euo pipefail\n")
    subprocess.run(["bash", "-n", str(SCRIPT)], cwd=ROOT, check=True)


@pytest.mark.parametrize(
    ("caller_library_path", "caller_python_home"),
    [(CALLER_LIBRARY_PATH, CALLER_PYTHON_HOME), ("", None)],
)
def test_quality_mode_runs_the_exact_local_gate_from_an_external_cwd(
    tmp_path: Path,
    caller_library_path: str,
    caller_python_home: str | None,
) -> None:
    sandbox, script = _sandbox_script(tmp_path)
    env, log, _ = _fake_tools(tmp_path, sandbox)
    if caller_library_path:
        env["LD_LIBRARY_PATH"] = caller_library_path
    else:
        env.pop("LD_LIBRARY_PATH")
    if caller_python_home is not None:
        env["PYTHONHOME"] = caller_python_home
    else:
        env.pop("PYTHONHOME")
    external = tmp_path / "external"
    external.mkdir()

    completed = _run(script, ["quality"], cwd=external, env=env)

    assert completed.returncode == 0, completed.stderr
    calls = _calls(log)
    environments = _assert_quality_calls(
        calls,
        caller_library_path,
        caller_python_home,
    )
    assert {call.cwd for call in calls} == {str(sandbox)}
    assert environments[0].parent.parent == Path(env["TMPDIR"])
    assert all(not environment.exists() for environment in environments)
    assert not environments[0].parent.exists()
    _assert_tmpdir_preserved(env)
    _assert_managed_pythons_preserved(env, QUALITY_VERSIONS)


def test_quality_mode_propagates_failure_without_running_later_commands(
    tmp_path: Path,
) -> None:
    sandbox, script = _sandbox_script(tmp_path)
    env, log, _ = _fake_tools(tmp_path, sandbox)
    env.update(
        {
            "TROUPE_RELEASE_FAIL_TOOL": "cargo",
            "TROUPE_RELEASE_FAIL_PYTHON": "3.10",
            "TROUPE_RELEASE_FAIL_COMMAND": "clippy",
            "TROUPE_RELEASE_FAIL_CODE": "17",
        }
    )

    completed = _run(script, ["quality"], cwd=tmp_path, env=env)

    assert completed.returncode == 17
    calls = _calls(log)
    assert [(call.python, call.tool, call.arguments[0]) for call in calls] == [
        ("", "uv", "python"),
        ("3.10", "uv", "sync"),
        ("3.10", "cargo", "fmt"),
        ("3.10", "cargo", "clippy"),
    ]
    assert all(not Path(call.project).exists() for call in calls if call.project)
    assert Path(calls[1].project).is_absolute()
    assert Path(calls[1].project).parent.parent == Path(env["TMPDIR"])
    assert not Path(calls[1].project).parent.exists()
    _assert_tmpdir_preserved(env)
    _assert_managed_pythons_preserved(env, ["3.10"])


def test_build_mode_executes_one_exact_fail_fast_manylinux_container(
    tmp_path: Path,
) -> None:
    sandbox, script = _sandbox_script(tmp_path)
    env, log, tools = _fake_tools(tmp_path, sandbox)

    completed = _run(script, ["build"], cwd=tmp_path, env=env)

    assert completed.returncode == 0, completed.stderr
    assert (sandbox / "wheel-artifact" / WHEEL_NAME).is_file()
    calls = _calls(log)
    assert len(calls) == 1
    call = calls[0]
    assert (call.tool, call.python, call.project, call.preference) == (
        "docker",
        "",
        "",
        "",
    )
    assert call.pyo3_python == ""
    assert call.library_path == CALLER_LIBRARY_PATH
    assert call.conda_present == "x"
    assert call.conda_prefix == CALLER_CONDA_PREFIX
    assert call.python_home_present == "x"
    assert call.python_home == CALLER_PYTHON_HOME
    assert call.cwd == str(sandbox)
    arguments = call.arguments
    assert arguments[:-4] == [
        "run",
        "--rm",
        "--entrypoint",
        "/bin/bash",
        "-w",
        "/io",
        "-v",
        f"{sandbox}:/io",
        "-v",
        f"{tools / 'uv'}:/usr/local/bin/uv:ro",
        "-e",
        "UV_PYTHON=/opt/python/cp310-cp310/bin/python",
        "-e",
        "UV_PROJECT_ENVIRONMENT=/tmp/troupe-venv",
        "ghcr.io/pyo3/maturin:v1.14.1",
    ]
    assert arguments[-4:-1] == ["-euo", "pipefail", "-c"]
    assert [line.strip() for line in arguments[-1].splitlines() if line.strip()] == [
        "/usr/local/bin/uv sync --frozen --all-groups --no-install-project &&",
        "test ! -e /io/wheel-artifact &&",
        (
            "/usr/local/bin/uv run --no-sync python scripts/verify_wheel.py "
            "--build --release --target x86_64-unknown-linux-gnu "
            "--manylinux 2_17 --output-dir wheel-artifact"
        ),
    ]
    _assert_tmpdir_preserved(env)


def test_build_mode_propagates_docker_failure_and_exposes_no_artifact(
    tmp_path: Path,
) -> None:
    sandbox, script = _sandbox_script(tmp_path)
    env, log, _ = _fake_tools(tmp_path, sandbox)
    env.update(
        {
            "TROUPE_RELEASE_FAIL_TOOL": "docker",
            "TROUPE_RELEASE_FAIL_PYTHON": "",
            "TROUPE_RELEASE_FAIL_COMMAND": "run",
            "TROUPE_RELEASE_FAIL_CODE": "19",
        }
    )

    completed = _run(script, ["build"], cwd=tmp_path, env=env)

    assert completed.returncode == 19
    assert [call.tool for call in _calls(log)] == ["docker"]
    assert not (sandbox / "wheel-artifact").exists()
    _assert_tmpdir_preserved(env)


def test_compatibility_mode_runs_five_isolated_pairs_on_one_artifact(
    tmp_path: Path,
) -> None:
    sandbox, script = _sandbox_script(tmp_path)
    wheel, checksum = _make_artifact(sandbox)
    env, log, _ = _fake_tools(tmp_path, sandbox)

    completed = _run(script, ["compatibility"], cwd=tmp_path, env=env)

    assert completed.returncode == 0, completed.stderr
    calls = _calls(log)
    assert calls[0]._replace(cwd="") == Call(
        "uv",
        "",
        "",
        "",
        "",
        CALLER_LIBRARY_PATH,
        "x",
        CALLER_CONDA_PREFIX,
        "x",
        CALLER_PYTHON_HOME,
        "",
        ["python", "install", *VERSIONS],
    )
    pairs = calls[1:]
    assert len(pairs) == 10
    environments: list[Path] = []
    for index, version in enumerate(VERSIONS):
        sync = pairs[index * 2]
        verify = pairs[index * 2 + 1]
        assert (sync.tool, sync.python, sync.preference, sync.arguments) == (
            "uv",
            version,
            "only-managed",
            ["sync", "--frozen", "--all-groups", "--no-install-project"],
        )
        assert (
            sync.pyo3_python,
            sync.library_path,
            sync.conda_present,
            sync.conda_prefix,
            sync.python_home_present,
            sync.python_home,
        ) == (
            "",
            CALLER_LIBRARY_PATH,
            "x",
            CALLER_CONDA_PREFIX,
            "x",
            CALLER_PYTHON_HOME,
        )
        assert (verify.tool, verify.python, verify.project, verify.preference) == (
            "uv",
            version,
            sync.project,
            "only-managed",
        )
        assert (
            verify.pyo3_python,
            verify.library_path,
            verify.conda_present,
            verify.conda_prefix,
            verify.python_home_present,
            verify.python_home,
        ) == (
            "",
            CALLER_LIBRARY_PATH,
            "x",
            CALLER_CONDA_PREFIX,
            "x",
            CALLER_PYTHON_HOME,
        )
        assert verify.arguments == [
            "run",
            "--no-sync",
            "python",
            "scripts/verify_wheel.py",
            "--wheel",
            str(wheel.relative_to(sandbox)),
            "--sha256-file",
            str(checksum.relative_to(sandbox)),
        ]
        environment = Path(sync.project)
        assert environment.is_absolute()
        assert environment.name == version
        environments.append(environment)
    assert len(set(environments)) == 5
    assert len({environment.parent for environment in environments}) == 1
    assert {call.cwd for call in calls} == {str(sandbox)}
    assert environments[0].parent.parent == Path(env["TMPDIR"])
    assert all(not environment.exists() for environment in environments)
    assert not environments[0].parent.exists()
    assert "docker" not in [call.tool for call in calls]
    _assert_tmpdir_preserved(env)
    _assert_managed_pythons_preserved(env, VERSIONS)


@pytest.mark.parametrize("failure_command", ["sync", "run"])
def test_compatibility_failure_stops_later_versions_and_cleans_environments(
    tmp_path: Path,
    failure_command: str,
) -> None:
    sandbox, script = _sandbox_script(tmp_path)
    _make_artifact(sandbox)
    env, log, _ = _fake_tools(tmp_path, sandbox)
    env.update(
        {
            "TROUPE_RELEASE_FAIL_TOOL": "uv",
            "TROUPE_RELEASE_FAIL_PYTHON": "3.12",
            "TROUPE_RELEASE_FAIL_COMMAND": failure_command,
            "TROUPE_RELEASE_FAIL_CODE": "29",
        }
    )

    completed = _run(script, ["compatibility"], cwd=tmp_path, env=env)

    assert completed.returncode == 29
    calls = _calls(log)
    expected = [
        ("", "python"),
        ("3.10", "sync"),
        ("3.10", "run"),
        ("3.11", "sync"),
        ("3.11", "run"),
        ("3.12", "sync"),
    ]
    if failure_command == "run":
        expected.append(("3.12", "run"))
    assert [(call.python, call.arguments[0]) for call in calls] == expected
    project_environments = [Path(call.project) for call in calls if call.project]
    assert project_environments
    assert all(path.is_absolute() for path in project_environments)
    assert project_environments[0].parent.parent == Path(env["TMPDIR"])
    assert all(not path.exists() for path in project_environments)
    assert not project_environments[0].parent.exists()
    _assert_tmpdir_preserved(env)
    _assert_managed_pythons_preserved(env, ["3.10", "3.11", "3.12"])


@pytest.mark.parametrize(
    ("mode", "versions"),
    [("quality", QUALITY_VERSIONS), ("compatibility", VERSIONS)],
)
def test_mode_propagates_managed_python_install_failure(
    tmp_path: Path,
    mode: str,
    versions: list[str],
) -> None:
    sandbox, script = _sandbox_script(tmp_path)
    if mode == "compatibility":
        _make_artifact(sandbox)
    env, log, _ = _fake_tools(tmp_path, sandbox)
    env.update(
        {
            "TROUPE_RELEASE_FAIL_TOOL": "uv",
            "TROUPE_RELEASE_FAIL_PYTHON": "",
            "TROUPE_RELEASE_FAIL_COMMAND": "python",
            "TROUPE_RELEASE_FAIL_CODE": "37",
        }
    )

    completed = _run(script, [mode], cwd=tmp_path, env=env)

    assert completed.returncode == 37
    assert [call.arguments for call in _calls(log)] == [["python", "install", *versions]]
    _assert_tmpdir_preserved(env)


@pytest.mark.parametrize("wheel_count", [0, 2])
def test_compatibility_rejects_wrong_wheel_cardinality_before_any_tool(
    tmp_path: Path,
    wheel_count: int,
) -> None:
    sandbox, script = _sandbox_script(tmp_path)
    artifact = sandbox / "wheel-artifact"
    artifact.mkdir()
    (artifact / "SHA256SUMS").touch()
    for index in range(wheel_count):
        (artifact / f"troupe-{index}-cp310-abi3-manylinux_2_17_x86_64.whl").touch()
    env, log, _ = _fake_tools(tmp_path, sandbox)

    completed = _run(script, ["compatibility"], cwd=tmp_path, env=env)

    assert completed.returncode != 0
    assert _calls(log) == []
    _assert_tmpdir_preserved(env)


def test_compatibility_rejects_missing_checksum_before_any_tool(tmp_path: Path) -> None:
    sandbox, script = _sandbox_script(tmp_path)
    artifact = sandbox / "wheel-artifact"
    artifact.mkdir()
    (artifact / WHEEL_NAME).touch()
    env, log, _ = _fake_tools(tmp_path, sandbox)

    completed = _run(script, ["compatibility"], cwd=tmp_path, env=env)

    assert completed.returncode != 0
    assert _calls(log) == []
    _assert_tmpdir_preserved(env)


def test_all_and_default_modes_compose_each_boundary_once_in_order(
    tmp_path: Path,
) -> None:
    for arguments in (["all"], []):
        case = tmp_path / ("explicit" if arguments else "default")
        case.mkdir()
        sandbox, script = _sandbox_script(case)
        env, log, _ = _fake_tools(case, sandbox)

        completed = _run(script, arguments, cwd=tmp_path, env=env)

        assert completed.returncode == 0, completed.stderr
        calls = _calls(log)
        _assert_quality_calls(calls[:15])
        assert calls[15].tool == "docker"
        assert calls[16].arguments == ["python", "install", *VERSIONS]
        assert [call.python for call in calls[17::2]] == VERSIONS
        assert [call.python for call in calls[18::2]] == VERSIONS
        assert {call.pyo3_python for call in calls[15:]} == {""}
        assert {call.library_path for call in calls[15:]} == {CALLER_LIBRARY_PATH}
        assert {call.conda_present for call in calls[15:]} == {"x"}
        assert {call.conda_prefix for call in calls[15:]} == {CALLER_CONDA_PREFIX}
        assert {call.python_home_present for call in calls[15:]} == {"x"}
        assert {call.python_home for call in calls[15:]} == {CALLER_PYTHON_HOME}
        assert len(calls) == 27
        _assert_tmpdir_preserved(env)
        _assert_managed_pythons_preserved(env, VERSIONS)


def test_all_mode_stops_before_compatibility_when_build_fails(tmp_path: Path) -> None:
    sandbox, script = _sandbox_script(tmp_path)
    env, log, _ = _fake_tools(tmp_path, sandbox)
    env.update(
        {
            "TROUPE_RELEASE_FAIL_TOOL": "docker",
            "TROUPE_RELEASE_FAIL_PYTHON": "",
            "TROUPE_RELEASE_FAIL_COMMAND": "run",
            "TROUPE_RELEASE_FAIL_CODE": "31",
        }
    )

    completed = _run(script, ["all"], cwd=tmp_path, env=env)

    assert completed.returncode == 31
    calls = _calls(log)
    _assert_quality_calls(calls[:15])
    assert [call.tool for call in calls[15:]] == ["docker"]
    _assert_tmpdir_preserved(env)


def test_all_mode_stops_before_build_when_quality_fails(tmp_path: Path) -> None:
    sandbox, script = _sandbox_script(tmp_path)
    env, log, _ = _fake_tools(tmp_path, sandbox)
    env.update(
        {
            "TROUPE_RELEASE_FAIL_TOOL": "cargo",
            "TROUPE_RELEASE_FAIL_PYTHON": "3.10",
            "TROUPE_RELEASE_FAIL_COMMAND": "clippy",
            "TROUPE_RELEASE_FAIL_CODE": "41",
        }
    )

    completed = _run(script, ["all"], cwd=tmp_path, env=env)

    assert completed.returncode == 41
    calls = _calls(log)
    assert [(call.python, call.tool, call.arguments[0]) for call in calls] == [
        ("", "uv", "python"),
        ("3.10", "uv", "sync"),
        ("3.10", "cargo", "fmt"),
        ("3.10", "cargo", "clippy"),
    ]
    assert "docker" not in [call.tool for call in calls]
    _assert_tmpdir_preserved(env)


def test_all_mode_propagates_compatibility_failure(tmp_path: Path) -> None:
    sandbox, script = _sandbox_script(tmp_path)
    env, log, _ = _fake_tools(tmp_path, sandbox)
    env.update(
        {
            "TROUPE_RELEASE_FAIL_TOOL": "uv",
            "TROUPE_RELEASE_FAIL_PYTHON": "3.12",
            "TROUPE_RELEASE_FAIL_COMMAND": "run",
            "TROUPE_RELEASE_FAIL_CODE": "43",
        }
    )

    completed = _run(script, ["all"], cwd=tmp_path, env=env)

    assert completed.returncode == 43
    calls = _calls(log)
    _assert_quality_calls(calls[:15])
    assert calls[15].tool == "docker"
    assert [(call.python, call.arguments[0]) for call in calls[16:]] == [
        ("", "python"),
        ("3.10", "sync"),
        ("3.10", "run"),
        ("3.11", "sync"),
        ("3.11", "run"),
        ("3.12", "sync"),
        ("3.12", "run"),
    ]
    project_paths = [Path(call.project) for call in calls if call.project]
    assert project_paths
    assert all(path.is_absolute() for path in project_paths)
    assert {path.parent.parent for path in project_paths} == {Path(env["TMPDIR"])}
    assert all(not path.exists() for path in project_paths)
    assert all(not parent.exists() for parent in {path.parent for path in project_paths})
    _assert_tmpdir_preserved(env)


def test_modes_reject_unknown_or_extra_arguments_without_running_tools(
    tmp_path: Path,
) -> None:
    sandbox, script = _sandbox_script(tmp_path)
    env, log, _ = _fake_tools(tmp_path, sandbox)
    arguments_to_reject = [["unknown"]] + [
        [mode, "extra"] for mode in ("quality", "build", "compatibility", "all")
    ]
    for arguments in arguments_to_reject:
        completed = _run(script, arguments, cwd=tmp_path, env=env)
        assert completed.returncode == 2
        assert "quality|build|compatibility|all" in completed.stderr
    assert _calls(log) == []
    _assert_tmpdir_preserved(env)


def test_release_script_contains_no_out_of_scope_target() -> None:
    text = _script().lower()
    assert "x86_64-unknown-linux-gnu" in text
    for forbidden in (
        "musllinux",
        "aarch64",
        "arm64",
        "i686",
        "cp313t",
        "cp314t",
        "macos",
        "windows",
    ):
        assert forbidden not in text
