from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import sys
import zipfile
from pathlib import Path
from typing import Any

import pytest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT_RELATIVE = Path("scripts/test_diagnostics_python_compat.sh")
SCRIPT = ROOT / SCRIPT_RELATIVE
PROBE_RELATIVE = Path("tests/release/diagnostics_python_compat.py")
ARTIFACT_RELATIVE = Path("tests/fixtures/artifact_layout/nodes/V08.json")
GATE_RELATIVE = Path("tests/fixtures/diagnostic_node_gates/V08.json")
VERSIONS = ["3.10", "3.11", "3.12", "3.13", "3.14"]
VERSION_ARGUMENT = ",".join(VERSIONS)
BUILDER_IMAGE = (
    "ghcr.io/pyo3/maturin@"
    "sha256:2665227312dd1eab1c29c70a001dc8aac53155a2d048bede3b2df7f1691c8e38"
)
WHEEL_NAME = "troupe-0.1.0-cp310-abi3-manylinux_2_17_x86_64." "manylinux2014_x86_64.whl"
WHEEL_MEMBERS = [
    "troupe/__init__.py",
    "troupe/__init__.pyi",
    "troupe/act_schema.pyi",
    "troupe/diagnostics.pyi",
    "troupe/py.typed",
    "troupe/_runtime.abi3.so",
    "troupe-0.1.0.dist-info/METADATA",
    "troupe-0.1.0.dist-info/WHEEL",
    "troupe-0.1.0.dist-info/entry_points.txt",
    "troupe-0.1.0.dist-info/RECORD",
]


FAKE_DOCKER = r"""#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import sys
import zipfile
from pathlib import Path


with Path(os.environ["TROUPE_V08_DOCKER_LOG"]).open("a", encoding="utf-8") as stream:
    stream.write(json.dumps(sys.argv[1:]) + "\n")
if os.environ.get("TROUPE_V08_FAIL_PHASE") == "build":
    raise SystemExit(37)
output = Path(sys.argv[-2])
output.mkdir(parents=True, exist_ok=True)
wheel = output / "troupe-0.1.0-cp310-abi3-manylinux_2_17_x86_64.manylinux2014_x86_64.whl"
members = [
    "troupe/__init__.py",
    "troupe/__init__.pyi",
    "troupe/act_schema.pyi",
    "troupe/diagnostics.pyi",
    "troupe/py.typed",
    "troupe/_runtime.abi3.so",
    "troupe-0.1.0.dist-info/METADATA",
    "troupe-0.1.0.dist-info/WHEEL",
    "troupe-0.1.0.dist-info/entry_points.txt",
    "troupe-0.1.0.dist-info/RECORD",
]
with zipfile.ZipFile(wheel, "w") as archive:
    for name in members:
        archive.writestr(name, b"\x7fELFfake" if name.endswith(".so") else name.encode())
"""


FAKE_INTERPRETER = r"""#!/usr/bin/env python3
from __future__ import annotations

import hashlib
import json
import os
import sys
from pathlib import Path


version = os.environ.get("TROUPE_V08_CHILD_VERSION")
if version is None:
    version = Path(sys.argv[0]).name.removeprefix("python")
arguments = sys.argv[1:]
if arguments[:2] == ["-I", "-c"]:
    phase = "preflight"
elif arguments[:3] == ["-I", "-m", "venv"]:
    phase = "venv"
elif arguments[:3] == ["-m", "pip", "install"]:
    phase = "install"
elif arguments and arguments[0].endswith("diagnostics_python_compat.py"):
    phase = "probe"
else:
    phase = "unexpected"
record = {
    "version": version,
    "phase": phase,
    "argv": arguments,
    "cwd": os.getcwd(),
    "path": os.environ.get("PATH"),
    "pythonpath": os.environ.get("PYTHONPATH"),
    "pip_no_index": os.environ.get("PIP_NO_INDEX"),
    "http_proxy": os.environ.get("http_proxy"),
    "no_proxy": os.environ.get("no_proxy"),
    "npm_cache": os.environ.get("TROUPE_NPM_CACHE"),
    "implicit_npm": sorted(
        name for name in os.environ if name.casefold().startswith("npm_config_")
    ),
}
with Path(os.environ["TROUPE_V08_INTERPRETER_LOG"]).open("a", encoding="utf-8") as stream:
    stream.write(json.dumps(record) + "\n")
if (
    os.environ.get("TROUPE_V08_FAIL_VERSION") == version
    and os.environ.get("TROUPE_V08_FAIL_PHASE") == phase
):
    raise SystemExit(29)
if phase == "preflight":
    print(json.dumps({
        "implementation": "CPython",
        "version": version,
        "executable": sys.argv[0],
    }, sort_keys=True, separators=(",", ":")))
elif phase == "venv":
    environment = Path(arguments[3])
    binary = environment / "bin"
    binary.mkdir(parents=True)
    child = binary / "python"
    child.write_text(
        f"#!{sys.executable}\n"
        "import os,runpy\n"
        f"os.environ['TROUPE_V08_CHILD_VERSION']={version!r}\n"
        f"runpy.run_path({str(Path(__file__).resolve())!r},run_name='__main__')\n",
        encoding="utf-8",
    )
    child.chmod(0o755)
elif phase == "install":
    console = Path(sys.argv[0]).parent / "troupe"
    console.write_text("#!/bin/sh\nexit 0\n", encoding="ascii")
    console.chmod(0o755)
elif phase == "probe":
    wheel = Path(arguments[arguments.index("--wheel") + 1])
    expected_sha = arguments[arguments.index("--expected-wheel-sha256") + 1]
    assert hashlib.sha256(wheel.read_bytes()).hexdigest() == expected_sha
    print(json.dumps({
        "schema": "troupe.diagnostics.python-compat-probe.v1",
        "python": version,
        "implementation": "CPython",
        "wheel_sha256": expected_sha,
        "native_sha256": "1" * 64,
        "native_bytes": 8,
        "package_members": [
            "__init__.py",
            "__init__.pyi",
            "act_schema.pyi",
            "diagnostics.pyi",
            "py.typed",
        ],
        "extensions": {
            "sink": True,
            "custom": True,
            "view_renderers": ["timeline", "metric", "table", "time_series"],
        },
        "runtime": {
            "active": "passed",
            "archive": "passed",
            "trace_bytes": 1024,
            "production_imports": 1,
        },
    }, separators=(",", ":")))
else:
    raise SystemExit(97)
"""


def load_json(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    assert isinstance(value, dict)
    return value


def sandbox(
    tmp_path: Path,
    *,
    missing: set[str] | frozenset[str] = frozenset(),
) -> tuple[Path, dict[str, str], Path, Path, Path]:
    repository = (tmp_path / "repository").resolve()
    script = repository / SCRIPT_RELATIVE
    script.parent.mkdir(parents=True)
    shutil.copy2(SCRIPT, script)
    script.chmod(0o755)
    probe = repository / PROBE_RELATIVE
    probe.parent.mkdir(parents=True)
    probe.write_text("# fake compatibility probe\n", encoding="ascii")
    expected = repository / "tests/fixtures/release/diagnostics-wheel-expected.json"
    expected.parent.mkdir(parents=True)
    expected.write_text(
        json.dumps(
            {
                "wheel_members": [
                    "troupe/<native>" if name.endswith(".so") else name
                    for name in WHEEL_MEMBERS
                ]
            }
        ),
        encoding="utf-8",
    )
    subprocess.run(["git", "init", "-q", str(repository)], check=True)
    subprocess.run(["git", "-C", str(repository), "add", "."], check=True)
    subprocess.run(
        [
            "git",
            "-C",
            str(repository),
            "-c",
            "user.name=V08 Test",
            "-c",
            "user.email=v08@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
        check=True,
    )

    tools = (tmp_path / "tools").resolve()
    tools.mkdir()
    for name in ("bash", "dirname", "git", "python3"):
        executable = shutil.which(name)
        assert executable is not None
        (tools / name).symlink_to(Path(executable).resolve())
    docker = tools / "docker"
    docker.write_text(FAKE_DOCKER, encoding="utf-8")
    docker.chmod(0o755)
    for version in VERSIONS:
        if version in missing:
            continue
        interpreter = tools / f"python{version}"
        interpreter.write_text(FAKE_INTERPRETER, encoding="utf-8")
        interpreter.chmod(0o755)

    cargo_home = (tmp_path / "cargo-home").resolve()
    (cargo_home / "registry").mkdir(parents=True)
    temporary = (tmp_path / "temporary").resolve()
    temporary.mkdir()
    sentinel = temporary / "caller-owned"
    sentinel.write_text("preserve\n", encoding="ascii")
    docker_log = (tmp_path / "docker.jsonl").resolve()
    interpreter_log = (tmp_path / "interpreters.jsonl").resolve()
    environment = dict(os.environ)
    environment.update(
        {
            "CARGO_HOME": str(cargo_home),
            "PATH": str(tools),
            "TROUPE_GATE_TMP": str(temporary),
            "TROUPE_NPM_CACHE": str(tmp_path / "npm-cache"),
            "NpM_CoNfIg_CaChE": str(tmp_path / "implicit-npm-cache"),
            "TROUPE_V08_DOCKER_LOG": str(docker_log),
            "TROUPE_V08_INTERPRETER_LOG": str(interpreter_log),
        }
    )
    environment.pop("PYTHONPATH", None)
    return repository, environment, docker_log, interpreter_log, sentinel


def run(
    repository: Path,
    environment: dict[str, str],
    *,
    arguments: list[str] | None = None,
) -> subprocess.CompletedProcess[str]:
    effective = (
        ["--versions", VERSION_ARGUMENT, "--build-current-wheel-once"]
        if arguments is None
        else arguments
    )
    return subprocess.run(
        [str(repository / SCRIPT_RELATIVE), *effective],
        cwd=repository,
        env=environment,
        check=False,
        capture_output=True,
        text=True,
        timeout=30,
    )


def records(path: Path) -> list[Any]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines()]


def test_checked_contract_and_descriptors_are_exact() -> None:
    assert os.access(ROOT / SCRIPT_RELATIVE, os.X_OK)
    assert load_json(ROOT / ARTIFACT_RELATIVE) == {
        "state": "realized",
        "introduced": [
            "scripts/test_diagnostics_python_compat.sh",
            "tests/release/diagnostics_python_compat.py",
            "tests/unit/test_diagnostics_python_compat_runner.py",
        ],
        "modified": [],
        "removed": [],
        "generated": [],
    }
    assert load_json(ROOT / GATE_RELATIVE) == {
        "state": "realized",
        "argv": [
            ["pytest", "-q", "tests/unit/test_diagnostics_python_compat_runner.py"]
        ],
        "env": {"TROUPE_GATE_TMP": "optional"},
        "maturin_features": [],
        "cache_requirements": [],
        "exclusive_resources": [],
    }


def test_success_builds_once_and_uses_five_isolated_venvs(tmp_path: Path) -> None:
    repository, environment, docker_log, interpreter_log, sentinel = sandbox(tmp_path)

    completed = run(repository, environment)

    assert completed.returncode == 0, completed.stderr
    assert completed.stderr == ""
    lines = completed.stdout.splitlines()
    assert len(lines) == 1
    summary = json.loads(lines[0])
    assert summary["schema"] == "troupe.diagnostics.python-compat-runner.v1"
    assert summary["result"] == "passed"
    assert summary["wheel"]["filename"] == WHEEL_NAME
    assert summary["wheel"]["builds"] == 1
    assert summary["wheel"]["builder_image"] == BUILDER_IMAGE
    assert [item["python"] for item in summary["versions"]] == VERSIONS
    assert {item["wheel_sha256"] for item in summary["versions"]} == {
        summary["wheel"]["sha256"]
    }
    assert {
        (item["native_sha256"], item["native_bytes"]) for item in summary["versions"]
    } == {("1" * 64, 8)}

    docker_calls = records(docker_log)
    assert len(docker_calls) == 1
    docker = docker_calls[0]
    assert docker[:2] == ["run", "--rm"]
    assert docker[docker.index("--network") + 1] == "none"
    assert docker[docker.index("--pull") + 1] == "never"
    assert docker[docker.index("--entrypoint") + 1] == "/bin/bash"
    assert docker.count(BUILDER_IMAGE) == 1
    container_script = docker[docker.index("-c") + 1]
    assert container_script.count("maturin build") == 1
    assert "--sdist" not in container_script
    mounts = [
        docker[index + 1] for index, value in enumerate(docker) if value == "--mount"
    ]
    assert f"type=bind,src={repository},dst={repository},readonly" in mounts
    assert any(mount.endswith("dst=/root/.cargo/registry,readonly") for mount in mounts)

    calls = records(interpreter_log)
    assert [(call["version"], call["phase"]) for call in calls[:5]] == [
        (version, "preflight") for version in VERSIONS
    ]
    matrix = calls[5:]
    assert [(call["version"], call["phase"]) for call in matrix] == [
        pair
        for version in VERSIONS
        for pair in ((version, "venv"), (version, "install"), (version, "probe"))
    ]
    venvs = [Path(call["argv"][3]) for call in matrix if call["phase"] == "venv"]
    assert len(venvs) == len(set(venvs)) == 5
    assert len({path.parent for path in venvs}) == 1
    for call in matrix:
        if call["phase"] not in {"install", "probe"}:
            continue
        workspace = Path(call["cwd"])
        expected_bin = workspace.parents[1] / "venvs" / call["version"] / "bin"
        assert call["path"] == str(expected_bin)
        assert call["pythonpath"] is None
        assert call["pip_no_index"] == "1"
        assert call["http_proxy"] == "http://127.0.0.1:9/"
        assert call["no_proxy"] == "127.0.0.1,localhost,::1"
        assert call["npm_cache"] is None
        assert call["implicit_npm"] == []
    assert sentinel.read_text(encoding="ascii") == "preserve\n"
    assert not list(Path(environment["TROUPE_GATE_TMP"]).glob("troupe-python-compat.*"))
    assert (
        subprocess.run(
            ["git", "status", "--porcelain"],
            cwd=repository,
            check=True,
            capture_output=True,
            text=True,
        ).stdout
        == ""
    )


def test_missing_interpreter_fails_before_build(tmp_path: Path) -> None:
    repository, environment, docker_log, _, sentinel = sandbox(
        tmp_path,
        missing={"3.12", "3.14"},
    )

    completed = run(repository, environment)

    assert completed.returncode == 1
    assert "missing CPython interpreters: 3.12,3.14" in completed.stderr
    assert not docker_log.exists()
    assert sentinel.read_text(encoding="ascii") == "preserve\n"
    assert not list(Path(environment["TROUPE_GATE_TMP"]).glob("troupe-python-compat.*"))


@pytest.mark.parametrize("phase", ["build", "probe"])
def test_failure_cleans_owned_state_and_stops_the_matrix(
    tmp_path: Path,
    phase: str,
) -> None:
    repository, environment, docker_log, interpreter_log, sentinel = sandbox(tmp_path)
    environment["TROUPE_V08_FAIL_PHASE"] = phase
    if phase == "probe":
        environment["TROUPE_V08_FAIL_VERSION"] = "3.12"

    completed = run(repository, environment)

    assert completed.returncode == 1
    assert len(records(docker_log)) == 1
    calls = records(interpreter_log)
    if phase == "build":
        assert [call["phase"] for call in calls] == ["preflight"] * 5
    else:
        probes = [call["version"] for call in calls if call["phase"] == "probe"]
        assert probes == ["3.10", "3.11", "3.12"]
    assert sentinel.read_text(encoding="ascii") == "preserve\n"
    assert not list(Path(environment["TROUPE_GATE_TMP"]).glob("troupe-python-compat.*"))


@pytest.mark.parametrize(
    "arguments",
    [
        [],
        ["--versions", VERSION_ARGUMENT],
        ["--build-current-wheel-once", "--versions", VERSION_ARGUMENT],
        ["--versions", "3.10,3.14", "--build-current-wheel-once"],
    ],
)
def test_argument_shape_is_closed(tmp_path: Path, arguments: list[str]) -> None:
    repository, environment, docker_log, interpreter_log, sentinel = sandbox(tmp_path)

    completed = run(repository, environment, arguments=arguments)

    assert completed.returncode == 1
    assert "usage:" in completed.stderr
    assert not docker_log.exists()
    assert not interpreter_log.exists()
    assert sentinel.read_text(encoding="ascii") == "preserve\n"


def test_fake_wheel_hash_is_stable() -> None:
    payloads = {
        name: b"\x7fELFfake" if name.endswith(".so") else name.encode()
        for name in WHEEL_MEMBERS
    }
    digest = hashlib.sha256()
    for name in sorted(payloads):
        digest.update(name.encode())
        digest.update(payloads[name])
    assert len(digest.hexdigest()) == 64
