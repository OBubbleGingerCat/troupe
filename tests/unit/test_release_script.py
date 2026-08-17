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
SUPPORT = ROOT / "tests" / "support"
sys.path.insert(0, str(SUPPORT))

from artifact_layout import load_artifact_layout  # noqa: E402


BASE_ARTIFACTS = load_artifact_layout(ROOT).base

VERSIONS = ["3.10", "3.11", "3.12", "3.13", "3.14"]
QUALITY_VERSIONS = ["3.10", "3.14"]
QUALITY_GROUP_SIZE = 9
QUALITY_CALL_COUNT = 1 + len(QUALITY_VERSIONS) * QUALITY_GROUP_SIZE
WHEEL_NAME = BASE_ARTIFACTS.release_wheel_name
CALLER_LIBRARY_PATH = "/caller/lib-one:/caller/lib-two"
CALLER_CONDA_PREFIX = "/caller/conda"
CALLER_PYTHON_HOME = sys.base_prefix
ATTEMPT_ID = "00000000-0000-4000-8000-000000000001"
DIAGNOSTIC_ORDER = [
    "V00",
    "V04",
    "V13",
    "V05",
    "V07",
    "V08",
    "V09",
    "V10",
    "V14",
    "V15",
]


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
    (sandbox / "tests" / "fixtures" / "productions").mkdir(parents=True)
    shutil.copy2(SCRIPT, script)
    script.chmod(0o755)
    subprocess.run(["git", "init", "-q", str(sandbox)], check=True)
    return sandbox, script


def _fake_tools(tmp_path: Path, sandbox: Path) -> tuple[dict[str, str], Path, Path]:
    tools = tmp_path / "fake-bin"
    tools.mkdir()
    temporary = tmp_path / "temporary"
    temporary.mkdir()
    (temporary / "caller-owned-sentinel").write_text("preserve\n", encoding="utf-8")
    log = tmp_path / "calls.bin"
    timeline = tmp_path / "timeline.log"
    diagnostic_log = tmp_path / "diagnostics.jsonl"
    timeout_log = tmp_path / "timeouts.jsonl"
    checkout_uid = sandbox.stat().st_uid + 100_000
    implementation = """#!/usr/bin/env bash
set -euo pipefail
tool="$(basename "$0")"
printf 'tool:%s:%s:%q\n' "$tool" "${UV_PYTHON-}" "$*" >> "$TROUPE_RELEASE_TEST_TIMELINE"
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
      && "${TROUPE_RELEASE_FAIL_COMMAND-}" == "${1-}" \
      && ( -z "${TROUPE_RELEASE_FAIL_ARGUMENTS-}" \
           || " $* " == *"${TROUPE_RELEASE_FAIL_ARGUMENTS}"* ) ]]; then
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
    diagnostic_implementation = r"""#!/usr/bin/env bash
set -euo pipefail
tool="$(basename "$0")"
label="$tool"
case "$tool" in
  run_diagnostic_node_gate.sh) label="${1-}" ;;
  test_diagnostics_wheel.sh) label=V07 ;;
  test_diagnostics_python_compat.sh) label=V08 ;;
  test_diagnostics_frontend_release.sh) label=V09 ;;
  test_diagnostics_rust_quality.sh) label=V10 ;;
  test_diagnostics_perfetto_release.sh) label=V15 ;;
esac
printf 'tool:diagnostic:%s:%q\n' "$label" "$*" >> "$TROUPE_RELEASE_TEST_TIMELINE"
env -u PYTHONHOME "$TROUPE_RELEASE_TEST_PYTHON" - "$TROUPE_RELEASE_DIAGNOSTIC_LOG" "$tool" "$label" "$@" <<'PY'
import json
import os
import sys

with open(sys.argv[1], "a", encoding="utf-8") as stream:
    stream.write(json.dumps({
        "tool": sys.argv[2],
        "label": sys.argv[3],
        "argv": sys.argv[4:],
        "gate_tmp": os.environ.get("TROUPE_GATE_TMP"),
        "npm_cache": os.environ.get("TROUPE_NPM_CACHE"),
        "playwright_cache": os.environ.get("TROUPE_PLAYWRIGHT_CACHE"),
        "perfetto_cache": os.environ.get("TROUPE_PERFETTO_CACHE"),
    }, sort_keys=True) + "\n")
PY
if [[ "$label" == V05 && -n "${TROUPE_GATE_TMP-}" ]]; then
  : > "$TROUPE_GATE_TMP/V05-performance-raw.json"
fi
if [[ "$label" == V07 ]]; then
  while [[ $# -gt 0 ]]; do
    if [[ "$1" == --report ]]; then
      : > "$2"
      break
    fi
    shift
  done
fi
if [[ "${TROUPE_RELEASE_FAIL_DIAGNOSTIC-}" == "$label" ]]; then
  exit "${TROUPE_RELEASE_FAIL_CODE:-23}"
fi
"""
    for name in (
        "run_diagnostic_node_gate.sh",
        "test_diagnostics_wheel.sh",
        "test_diagnostics_python_compat.sh",
        "test_diagnostics_frontend_release.sh",
        "test_diagnostics_rust_quality.sh",
        "test_diagnostics_perfetto_release.sh",
    ):
        executable = sandbox / "scripts" / name
        executable.write_text(diagnostic_implementation, encoding="utf-8")
        executable.chmod(0o755)
    timeout = tools / "timeout"
    timeout.write_text(
        r"""#!/usr/bin/env bash
set -euo pipefail
env -u PYTHONHOME "$TROUPE_RELEASE_TEST_PYTHON" - "$TROUPE_RELEASE_TIMEOUT_LOG" "$@" <<'PY'
import json
import sys

with open(sys.argv[1], "a", encoding="utf-8") as stream:
    stream.write(json.dumps(sys.argv[2:]) + "\n")
PY
if [[ $# -lt 5 || "$1" != --foreground || "$2" != --signal=TERM || "$3" != --kill-after=10s ]]; then
  exit 98
fi
shift 4
exec "$@"
""",
        encoding="utf-8",
    )
    timeout.chmod(0o755)
    find = tools / "find"
    find.write_text(
        """#!/usr/bin/env bash
set -euo pipefail
printf 'audit:find:%s\n' "$*" >> "$TROUPE_RELEASE_TEST_TIMELINE"
if [[ "${TROUPE_RELEASE_AUDIT_FAILURE-}" == cache && " $* " == *" __pycache__ "* ]]; then
  printf '%s\n' "$TROUPE_RELEASE_TEST_ROOT/tests/fixtures/productions/bad/__pycache__"
  exit 0
fi
if [[ " $* " == *" ! -uid $TROUPE_RELEASE_TEST_ROOT_UID "* ]]; then
  if [[ "${TROUPE_RELEASE_AUDIT_FAILURE-}" == owner ]]; then
    printf '%s\n' "$TROUPE_RELEASE_TEST_ROOT/wrong-owner"
  fi
  exit 0
fi
exec /usr/bin/find "$@"
""",
        encoding="utf-8",
    )
    find.chmod(0o755)
    git = tools / "git"
    git.write_text(
        """#!/usr/bin/env bash
set -euo pipefail
printf 'audit:git:%s\n' "$*" >> "$TROUPE_RELEASE_TEST_TIMELINE"
exec /usr/bin/git "$@"
""",
        encoding="utf-8",
    )
    git.chmod(0o755)
    stat_tool = tools / "stat"
    stat_tool.write_text(
        """#!/usr/bin/env bash
set -euo pipefail
printf 'audit:stat:%s\n' "$*" >> "$TROUPE_RELEASE_TEST_TIMELINE"
if [[ "$#" -eq 3 && "$1" == -c && "$2" == %u \
      && "$3" == "$TROUPE_RELEASE_TEST_ROOT" ]]; then
  printf '%s\n' "$TROUPE_RELEASE_TEST_ROOT_UID"
  exit 0
fi
exec /usr/bin/stat "$@"
""",
        encoding="utf-8",
    )
    stat_tool.chmod(0o755)
    env = dict(os.environ)
    for name in list(env):
        if name in {
            "TROUPE_DIAGNOSTIC_BOOTSTRAP_GATE",
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
            "TROUPE_RELEASE_TEST_TIMELINE": str(timeline),
            "TROUPE_RELEASE_TEST_PYTHON": sys.executable,
            "TROUPE_RELEASE_TEST_ROOT": str(sandbox),
            "TROUPE_RELEASE_TEST_ROOT_UID": str(checkout_uid),
            "TROUPE_RELEASE_TEST_WHEEL": WHEEL_NAME,
            "TROUPE_RELEASE_DIAGNOSTIC_LOG": str(diagnostic_log),
            "TROUPE_RELEASE_TIMEOUT_LOG": str(timeout_log),
            "TMPDIR": str(temporary),
        }
    )
    for name, directory in {
        "TROUPE_NPM_CACHE": tmp_path / "npm-cache",
        "TROUPE_PLAYWRIGHT_CACHE": tmp_path / "playwright-cache",
        "TROUPE_PERFETTO_CACHE": tmp_path / "perfetto-cache",
    }.items():
        directory.mkdir()
        env[name] = str(directory.resolve())
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


def _timeline(env: dict[str, str]) -> list[str]:
    path = Path(env["TROUPE_RELEASE_TEST_TIMELINE"])
    return path.read_text(encoding="utf-8").splitlines() if path.exists() else []


def _diagnostic_calls(env: dict[str, str]) -> list[dict[str, object]]:
    path = Path(env["TROUPE_RELEASE_DIAGNOSTIC_LOG"])
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def _timeout_calls(env: dict[str, str]) -> list[list[str]]:
    path = Path(env["TROUPE_RELEASE_TIMEOUT_LOG"])
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


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
    assert (temporary / "caller-owned-sentinel").read_text(
        encoding="utf-8"
    ) == "preserve\n"
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
        ("cargo", ["fmt", "--check", "--all", "--manifest-path", "rust/Cargo.toml"]),
        (
            "cargo",
            [
                "clippy",
                "--locked",
                "--manifest-path",
                "rust/Cargo.toml",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--",
                "-D",
                "warnings",
            ],
        ),
        (
            "cargo",
            [
                "test",
                "--locked",
                "--manifest-path",
                "rust/Cargo.toml",
                "--workspace",
            ],
        ),
        (
            "uv",
            [
                "run",
                "--no-sync",
                "maturin",
                "develop",
                "--uv",
                "--locked",
                "--features",
                "agent-test-support",
                "--manifest-path",
                "rust/Cargo.toml",
            ],
        ),
        ("uv", ["run", "--no-sync", "pytest", "-q"]),
        (
            "uv",
            [
                "run",
                "--no-sync",
                "python",
                "-m",
                "mypy",
                "--strict",
                "--show-error-codes",
                "tests/typing/positive.py",
            ],
        ),
        (
            "uv",
            [
                "run",
                "--no-sync",
                "python",
                "-m",
                "mypy.stubtest",
                "troupe",
                "--concise",
            ],
        ),
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
    assert len(calls) == QUALITY_CALL_COUNT
    environments: list[Path] = []
    for index, version in enumerate(QUALITY_VERSIONS):
        group = calls[
            1 + index * QUALITY_GROUP_SIZE : 1 + (index + 1) * QUALITY_GROUP_SIZE
        ]
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
        assert {call.library_path for call in group[1:]} == {expected_library_path}
        assert [call.conda_present for call in group] == [
            "x",
            "x",
            "x",
            "x",
            "",
            "x",
            "x",
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


@pytest.mark.parametrize(
    ("module", "argument_marker"),
    [
        ("mypy", "-m mypy --strict"),
        ("mypy.stubtest", "-m mypy.stubtest troupe"),
    ],
)
def test_quality_mode_runs_typing_commands_and_propagates_each_failure(
    tmp_path: Path,
    module: str,
    argument_marker: str,
) -> None:
    sandbox, script = _sandbox_script(tmp_path)
    env, log, _ = _fake_tools(tmp_path, sandbox)
    env.update(
        {
            "TROUPE_RELEASE_FAIL_TOOL": "uv",
            "TROUPE_RELEASE_FAIL_PYTHON": "3.10",
            "TROUPE_RELEASE_FAIL_COMMAND": "run",
            "TROUPE_RELEASE_FAIL_ARGUMENTS": argument_marker,
            "TROUPE_RELEASE_FAIL_CODE": "18",
        }
    )

    completed = _run(script, ["quality"], cwd=tmp_path, env=env)

    assert completed.returncode == 18
    calls = _calls(log)
    assert calls[-1].tool == "uv"
    assert calls[-1].python == "3.10"
    assert calls[-1].arguments[:4] == ["run", "--no-sync", "python", "-m"]
    assert module in calls[-1].arguments
    assert all(call.python != "3.14" for call in calls)
    assert not any(call.arguments[-1:] == ["README.md"] for call in calls)
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
        "HTTP_PROXY",
        "-e",
        "HTTPS_PROXY",
        "-e",
        "NO_PROXY",
        "-e",
        "ALL_PROXY",
        "-e",
        "http_proxy",
        "-e",
        "https_proxy",
        "-e",
        "no_proxy",
        "-e",
        "all_proxy",
        "-e",
        "PYTHONDONTWRITEBYTECODE=1",
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


@pytest.mark.parametrize("failure", ["diff", "cache", "owner"])
def test_successful_mode_fails_when_the_checkout_audit_fails(
    tmp_path: Path,
    failure: str,
) -> None:
    sandbox, script = _sandbox_script(tmp_path)
    env, log, _ = _fake_tools(tmp_path, sandbox)
    if failure == "diff":
        tracked = sandbox / "tracked.txt"
        tracked.write_text("clean\n", encoding="utf-8")
        subprocess.run(["git", "-C", str(sandbox), "add", "tracked.txt"], check=True)
        tracked.write_text("trailing whitespace \n", encoding="utf-8")
    elif failure == "cache":
        (sandbox / "tests" / "fixtures" / "productions" / "bad" / "__pycache__").mkdir(
            parents=True
        )
    elif failure == "owner":
        env["TROUPE_RELEASE_AUDIT_FAILURE"] = "owner"
    else:
        raise AssertionError(f"unknown audit failure: {failure}")

    completed = _run(script, ["build"], cwd=tmp_path, env=env)

    assert completed.returncode != 0
    assert [call.tool for call in _calls(log)] == ["docker"]
    timeline = _timeline(env)
    assert timeline[0].startswith("tool:docker:")
    assert all(entry.startswith("audit:") for entry in timeline[1:])
    if failure == "owner":
        assert f"audit:stat:-c %u {sandbox}" in timeline
        assert any(
            entry.startswith(f"audit:find:{sandbox} ")
            and f"! -uid {env['TROUPE_RELEASE_TEST_ROOT_UID']}" in entry
            for entry in timeline
        )
    _assert_tmpdir_preserved(env)


@pytest.mark.parametrize("mode", ["quality", "compatibility", "all"])
def test_every_release_mode_finishes_with_the_checkout_audit(
    tmp_path: Path,
    mode: str,
) -> None:
    sandbox, script = _sandbox_script(tmp_path)
    if mode == "compatibility":
        _make_artifact(sandbox)
    env, _, _ = _fake_tools(tmp_path, sandbox)

    completed = _run(script, [mode], cwd=tmp_path, env=env)

    assert completed.returncode == 0, completed.stderr
    timeline = _timeline(env)
    first_audit = next(
        index for index, entry in enumerate(timeline) if entry.startswith("audit:")
    )
    assert first_audit > 0
    assert all(entry.startswith("tool:") for entry in timeline[:first_audit])
    assert all(entry.startswith("audit:") for entry in timeline[first_audit:])
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
    assert [call.arguments for call in _calls(log)] == [
        ["python", "install", *versions]
    ]
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
        _assert_quality_calls(calls[:QUALITY_CALL_COUNT])
        assert calls[QUALITY_CALL_COUNT].tool == "docker"
        assert calls[QUALITY_CALL_COUNT + 1].arguments == [
            "python",
            "install",
            *VERSIONS,
        ]
        assert [call.python for call in calls[QUALITY_CALL_COUNT + 2 :: 2]] == VERSIONS
        assert [call.python for call in calls[QUALITY_CALL_COUNT + 3 :: 2]] == VERSIONS
        assert {call.pyo3_python for call in calls[QUALITY_CALL_COUNT:]} == {""}
        assert {call.library_path for call in calls[QUALITY_CALL_COUNT:]} == {
            CALLER_LIBRARY_PATH
        }
        assert {call.conda_present for call in calls[QUALITY_CALL_COUNT:]} == {"x"}
        assert {call.conda_prefix for call in calls[QUALITY_CALL_COUNT:]} == {
            CALLER_CONDA_PREFIX
        }
        assert {call.python_home_present for call in calls[QUALITY_CALL_COUNT:]} == {
            "x"
        }
        assert {call.python_home for call in calls[QUALITY_CALL_COUNT:]} == {
            CALLER_PYTHON_HOME
        }
        assert len(calls) == QUALITY_CALL_COUNT + 12
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
    _assert_quality_calls(calls[:QUALITY_CALL_COUNT])
    assert [call.tool for call in calls[QUALITY_CALL_COUNT:]] == ["docker"]
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
    _assert_quality_calls(calls[:QUALITY_CALL_COUNT])
    assert calls[QUALITY_CALL_COUNT].tool == "docker"
    assert [
        (call.python, call.arguments[0]) for call in calls[QUALITY_CALL_COUNT + 1 :]
    ] == [
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
    assert all(
        not parent.exists() for parent in {path.parent for path in project_paths}
    )
    _assert_tmpdir_preserved(env)


def test_modes_reject_unknown_or_extra_arguments_without_running_tools(
    tmp_path: Path,
) -> None:
    sandbox, script = _sandbox_script(tmp_path)
    env, log, _ = _fake_tools(tmp_path, sandbox)
    arguments_to_reject = [["unknown"]] + [
        [mode, "extra"]
        for mode in ("quality", "build", "compatibility", "diagnostics", "all")
    ]
    for arguments in arguments_to_reject:
        completed = _run(script, arguments, cwd=tmp_path, env=env)
        assert completed.returncode == 2
        assert "quality|build|compatibility|diagnostics|all" in completed.stderr
    assert _calls(log) == []
    _assert_tmpdir_preserved(env)


def test_diagnostics_mode_dispatches_exact_frozen_children_and_cleans_owned_reports(
    tmp_path: Path,
) -> None:
    sandbox, script = _sandbox_script(tmp_path)
    env, log, _ = _fake_tools(tmp_path, sandbox)

    completed = _run(script, ["diagnostics"], cwd=tmp_path, env=env)

    assert completed.returncode == 0, completed.stderr
    assert _calls(log) == []
    calls = _diagnostic_calls(env)
    assert [call["label"] for call in calls] == DIAGNOSTIC_ORDER
    assert [call["argv"] for call in calls] == [
        ["V00"],
        ["V04"],
        ["V13"],
        ["V05"],
        ["--offline", "--smoke", "active,archive", "--report", calls[4]["argv"][4]],
        ["--versions", "3.10,3.11,3.12,3.13,3.14", "--build-current-wheel-once"],
        [
            "--clean",
            "--check-generated",
            "--forbid-update",
            "--npm-cache",
            env["TROUPE_NPM_CACHE"],
        ],
        ["--all", "--locked", "--deny-warnings"],
        ["V14"],
        [
            "--offline",
            "--all-layers",
            "--perfetto-cache",
            env["TROUPE_PERFETTO_CACHE"],
            "--browser-cache",
            env["TROUPE_PLAYWRIGHT_CACHE"],
        ],
    ]
    assert [Path(call["gate_tmp"]) if call["gate_tmp"] else None for call in calls] == [
        None,
        None,
        None,
        Path(str(calls[3]["gate_tmp"])),
        Path(str(calls[4]["gate_tmp"])),
        None,
        None,
        None,
        None,
        None,
    ]
    v05_root = Path(str(calls[3]["gate_tmp"]))
    v07_root = Path(str(calls[4]["gate_tmp"]))
    assert v05_root != v07_root
    assert v05_root.parent == Path(env["TMPDIR"])
    assert v07_root.parent == Path(env["TMPDIR"])
    assert calls[4]["argv"][4] == str(v07_root / "V07-wheel-report.json")
    assert not v05_root.exists() and not v07_root.exists()
    assert [call[3] for call in _timeout_calls(env)] == [
        "900s",
        "900s",
        "900s",
        "900s",
        "1800s",
        "1800s",
        "900s",
        "1800s",
        "1800s",
        "900s",
    ]
    assert all(
        call[:3] == ["--foreground", "--signal=TERM", "--kill-after=10s"]
        for call in _timeout_calls(env)
    )
    for call in calls:
        assert call["npm_cache"] == env["TROUPE_NPM_CACHE"]
        assert call["playwright_cache"] == env["TROUPE_PLAYWRIGHT_CACHE"]
        assert call["perfetto_cache"] == env["TROUPE_PERFETTO_CACHE"]
    _assert_tmpdir_preserved(env)


def test_diagnostics_failure_is_fail_fast_and_cleans_only_owned_roots(
    tmp_path: Path,
) -> None:
    sandbox, script = _sandbox_script(tmp_path)
    env, log, _ = _fake_tools(tmp_path, sandbox)
    env.update(
        {
            "TROUPE_RELEASE_FAIL_DIAGNOSTIC": "V07",
            "TROUPE_RELEASE_FAIL_CODE": "47",
        }
    )

    completed = _run(script, ["diagnostics"], cwd=tmp_path, env=env)

    assert completed.returncode == 47
    assert _calls(log) == []
    calls = _diagnostic_calls(env)
    assert [call["label"] for call in calls] == DIAGNOSTIC_ORDER[:5]
    roots = {Path(str(call["gate_tmp"])) for call in calls if call["gate_tmp"]}
    assert len(roots) == 2
    assert all(not root.exists() for root in roots)
    _assert_tmpdir_preserved(env)


def test_final_all_routes_only_persistent_reports_to_fresh_attempt_root(
    tmp_path: Path,
) -> None:
    sandbox, script = _sandbox_script(tmp_path)
    env, _, _ = _fake_tools(tmp_path, sandbox)
    evidence = (tmp_path / "evidence" / "attempts" / ATTEMPT_ID).resolve()
    evidence.mkdir(parents=True)

    completed = _run(
        script,
        ["all", "--diagnostics-evidence-root", str(evidence)],
        cwd=tmp_path,
        env=env,
    )

    assert completed.returncode == 0, completed.stderr
    calls = _diagnostic_calls(env)
    assert [call["label"] for call in calls] == DIAGNOSTIC_ORDER
    assert calls[3]["gate_tmp"] == str(evidence)
    assert calls[4]["gate_tmp"] == str(evidence)
    assert all(call["gate_tmp"] is None for call in calls[:3] + calls[5:])
    assert {path.name for path in evidence.iterdir()} == {
        "V05-performance-raw.json",
        "V07-wheel-report.json",
    }
    assert evidence.is_dir()


def test_final_mode_rejects_reused_or_user_selected_paths_before_any_tool(
    tmp_path: Path,
) -> None:
    sandbox, script = _sandbox_script(tmp_path)
    env, log, _ = _fake_tools(tmp_path, sandbox)
    evidence = (tmp_path / "attempt").resolve()
    evidence.mkdir()
    (evidence / "old").write_text("preserve\n", encoding="utf-8")

    completed = _run(
        script,
        ["all", "--diagnostics-evidence-root", str(evidence)],
        cwd=tmp_path,
        env=env,
    )

    assert completed.returncode != 0
    assert (evidence / "old").read_text(encoding="utf-8") == "preserve\n"
    assert _calls(log) == []
    assert _diagnostic_calls(env) == []


@pytest.mark.parametrize(
    "missing",
    ["TROUPE_NPM_CACHE", "TROUPE_PLAYWRIGHT_CACHE", "TROUPE_PERFETTO_CACHE"],
)
def test_diagnostics_requires_each_explicit_cache_before_dispatch(
    tmp_path: Path, missing: str
) -> None:
    sandbox, script = _sandbox_script(tmp_path)
    env, log, _ = _fake_tools(tmp_path, sandbox)
    env.pop(missing)

    completed = _run(script, ["diagnostics"], cwd=tmp_path, env=env)

    assert completed.returncode != 0
    assert missing in completed.stderr
    assert _calls(log) == []
    assert _diagnostic_calls(env) == []


def test_help_lists_every_mode_without_dispatch(tmp_path: Path) -> None:
    sandbox, script = _sandbox_script(tmp_path)
    env, log, _ = _fake_tools(tmp_path, sandbox)

    completed = _run(script, ["--help"], cwd=tmp_path, env=env)

    assert completed.returncode == 0
    assert "quality|build|compatibility|diagnostics|all" in completed.stderr
    assert _calls(log) == []
    assert _diagnostic_calls(env) == []


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


def _bootstrap_fake_child(arguments: list[str]) -> int:
    if len(arguments) < 5 or arguments[3] != "--":
        return 2
    label, timeout_seconds, gate_tmp = arguments[:3]
    command = arguments[4:]
    expected_timeout = {
        "quality": "60",
        "build": "60",
        "compatibility": "60",
        "V00": "900",
        "V04": "900",
        "V13": "900",
        "V05": "900",
        "V07": "1800",
        "V08": "1800",
        "V09": "900",
        "V10": "1800",
        "V14": "1800",
        "V15": "900",
    }
    if expected_timeout.get(label) != timeout_seconds or not command:
        return 2
    if label in {"quality", "build", "compatibility"}:
        return 0 if command == [label] and not gate_tmp else 2
    if label in {"V00", "V04", "V13", "V05", "V14"}:
        if command != ["scripts/run_diagnostic_node_gate.sh", label]:
            return 2
    if label == "V05" and not gate_tmp:
        return 2
    if label == "V07":
        if command[:4] != [
            "scripts/test_diagnostics_wheel.sh",
            "--offline",
            "--smoke",
            "active,archive",
        ]:
            return 2
        if command[4:5] != ["--report"] or len(command) != 6:
            return 2
        if not gate_tmp or Path(command[5]).parent != Path(gate_tmp):
            return 2
    return 0


if __name__ == "__main__":
    if len(sys.argv) >= 2 and sys.argv[1] == "--bootstrap-fake-child":
        raise SystemExit(_bootstrap_fake_child(sys.argv[2:]))
    raise SystemExit(2)
