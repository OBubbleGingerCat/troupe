#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)"

if command -v python3 >/dev/null 2>&1; then
  python_command=python3
else
  python_command=python
fi

export PYTHONDONTWRITEBYTECODE=1
exec "$python_command" - "$repository_root" "$@" <<'PY'
from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path
from typing import Any, Mapping, Sequence


VERSIONS = ("3.10", "3.11", "3.12", "3.13", "3.14")
VERSION_ARGUMENT = ",".join(VERSIONS)
BUILDER_IMAGE = (
    "ghcr.io/pyo3/maturin@"
    "sha256:2665227312dd1eab1c29c70a001dc8aac53155a2d048bede3b2df7f1691c8e38"
)
BUILD_TARGET = "x86_64-unknown-linux-gnu"
CONTAINER_PYTHON = "/opt/python/cp310-cp310/bin/python"
WHEEL_NAME = (
    "troupe-0.1.0-cp310-abi3-manylinux_2_17_x86_64."
    "manylinux2014_x86_64.whl"
)
OFFLINE_PROXY = "http://127.0.0.1:9/"
PROBE_SCHEMA = "troupe.diagnostics.python-compat-probe.v1"
RUNNER_SCHEMA = "troupe.diagnostics.python-compat-runner.v1"


class RunnerError(RuntimeError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RunnerError(message)


def canonical_json(value: object) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def exact_directory(path: Path, label: str) -> Path:
    require(path.is_absolute() and str(path) == os.path.abspath(path), f"{label} must be absolute")
    try:
        metadata = path.lstat()
        resolved = path.resolve(strict=True)
    except OSError as error:
        raise RunnerError(f"{label} is unavailable: {error}") from error
    require(stat.S_ISDIR(metadata.st_mode) and not stat.S_ISLNK(metadata.st_mode), f"{label} must be a real directory")
    require(resolved == path, f"{label} must not use symlink indirection")
    return path


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def run_command(
    command: Sequence[str],
    *,
    cwd: Path,
    env: Mapping[str, str],
    label: str,
    timeout: float,
) -> subprocess.CompletedProcess[str]:
    try:
        completed = subprocess.run(
            list(command),
            cwd=cwd,
            env=dict(env),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
            timeout=timeout,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise RunnerError(f"{label} could not run: {error}") from error
    if completed.returncode != 0:
        detail = (completed.stdout + completed.stderr).strip()
        raise RunnerError(f"{label} exited {completed.returncode}: {detail[-4000:]}")
    return completed


def clean_environment(environ: Mapping[str, str]) -> dict[str, str]:
    result = dict(environ)
    for name in (
        "CONDA_PREFIX",
        "PYTHONHOME",
        "PYTHONPATH",
        "VIRTUAL_ENV",
        "TROUPE_NPM_CACHE",
        "TROUPE_PERFETTO_CACHE",
        "TROUPE_PLAYWRIGHT_CACHE",
    ):
        result.pop(name, None)
    for name in tuple(result):
        if name.casefold().startswith("npm_config_"):
            result.pop(name, None)
    result.update(
        {
            "ALL_PROXY": OFFLINE_PROXY,
            "HTTP_PROXY": OFFLINE_PROXY,
            "HTTPS_PROXY": OFFLINE_PROXY,
            "PIP_DISABLE_PIP_VERSION_CHECK": "1",
            "PIP_NO_INDEX": "1",
            "PYTHONDONTWRITEBYTECODE": "1",
            "UV_OFFLINE": "1",
            "all_proxy": OFFLINE_PROXY,
            "http_proxy": OFFLINE_PROXY,
            "https_proxy": OFFLINE_PROXY,
            "NO_PROXY": "127.0.0.1,localhost,::1",
            "no_proxy": "127.0.0.1,localhost,::1",
        }
    )
    return result


def interpreter_probe(path: Path, version: str, environ: Mapping[str, str]) -> dict[str, str]:
    source = (
        "import ensurepip,json,platform,sys,venv;"
        "print(json.dumps({'implementation':platform.python_implementation(),"
        "'version':f'{sys.version_info.major}.{sys.version_info.minor}',"
        "'executable':sys.executable},sort_keys=True,separators=(',',':')))"
    )
    completed = run_command(
        [str(path), "-I", "-c", source],
        cwd=Path.cwd(),
        env=environ,
        label=f"CPython {version} preflight",
        timeout=10.0,
    )
    try:
        value = json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise RunnerError(f"CPython {version} preflight returned invalid JSON") from error
    require(
        isinstance(value, dict)
        and set(value) == {"implementation", "version", "executable"}
        and value["implementation"] == "CPython"
        and value["version"] == version,
        f"python{version} is not CPython {version}",
    )
    require(
        isinstance(value["executable"], str) and Path(value["executable"]).is_absolute(),
        f"python{version} did not report an absolute executable",
    )
    return {name: str(item) for name, item in value.items()}


def discover_interpreters(environ: Mapping[str, str]) -> tuple[dict[str, Path], list[dict[str, str]]]:
    missing: list[str] = []
    paths: dict[str, Path] = {}
    observations: list[dict[str, str]] = []
    search_path = environ.get("PATH")
    for version in VERSIONS:
        value = shutil.which(f"python{version}", path=search_path)
        if value is None:
            missing.append(version)
            continue
        path = Path(value).absolute()
        observations.append(interpreter_probe(path, version, environ))
        paths[version] = path
    require(not missing, f"missing CPython interpreters: {','.join(missing)}")
    resolved = [path.resolve(strict=True) for path in paths.values()]
    require(len(resolved) == len(set(resolved)), "CPython versions do not resolve to distinct interpreters")
    return paths, observations


def checkout_state(repository: Path, environ: Mapping[str, str]) -> bytes:
    try:
        completed = subprocess.run(
            ["git", "status", "--porcelain=v1", "-z", "--untracked-files=all"],
            cwd=repository,
            env=dict(environ),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=10,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise RunnerError(f"could not inspect checkout state: {error}") from error
    require(completed.returncode == 0, "could not inspect checkout state")
    return completed.stdout


CONTAINER_SCRIPT = r'''
owner_uid=$1
owner_gid=$2
owned_suite=$3
repository=$4
output=$5
container_python=$6

restore_ownership() {
  original_status=$?
  trap - EXIT
  chown -hR -- "${owner_uid}:${owner_gid}" "$owned_suite" || exit 125
  exit "$original_status"
}
trap restore_ownership EXIT

required_tools=(python maturin cargo rustc cc c++ ar ranlib ld strip cmake make perl patchelf git)
for tool in "${required_tools[@]}"; do
  command -v "$tool" >/dev/null
done
for tool in node nodejs npm npx protoc perfetto trace_processor_shell uv; do
  if command -v "$tool" >/dev/null 2>&1; then
    printf 'forbidden compatibility build tool is available: %s\n' "$tool" >&2
    exit 1
  fi
done
test "$PYO3_PYTHON" = "$container_python"
test ! -e "$output"/*.whl
cargo metadata --locked --offline --no-deps --format-version 1 \
  --manifest-path "$repository/rust/Cargo.toml" >/dev/null
maturin build --locked --release \
  --manifest-path "$repository/rust/Cargo.toml" \
  --out "$output" \
  --target x86_64-unknown-linux-gnu \
  --manylinux 2_17
'''


def build_wheel(repository: Path, suite: Path, environ: Mapping[str, str]) -> Path:
    docker = shutil.which("docker", path=environ.get("PATH"))
    require(docker is not None, "Docker is required for the compatibility wheel build")
    cargo_home = Path(environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    cargo_registry = exact_directory(cargo_home / "registry", "Cargo registry")
    output = suite / "wheel"
    output.mkdir()
    tools = suite / "container-tools"
    tools.mkdir()
    (tools / "git").symlink_to("/usr/local/bin/git")
    (tools / "patchelf").symlink_to("/usr/local/bin/patchelf")
    (tools / "cmake").symlink_to("/usr/local/bin/cmake")
    container_path = os.pathsep.join(
        (
            str(tools),
            "/opt/python/cp310-cp310/bin",
            "/root/.cargo/bin",
            "/opt/clang/bin",
            "/opt/rh/devtoolset-10/root/usr/bin",
            "/usr/local/sbin",
            "/usr/bin",
            "/usr/sbin",
            "/bin",
            "/sbin",
        )
    )
    container_environment = {
        "ALL_PROXY": OFFLINE_PROXY,
        "CARGO_HOME": "/root/.cargo",
        "CARGO_NET_OFFLINE": "true",
        "CARGO_TARGET_DIR": str(suite / "cargo-target"),
        "GIT_CONFIG_COUNT": "1",
        "GIT_CONFIG_KEY_0": "safe.directory",
        "GIT_CONFIG_VALUE_0": str(repository),
        "HTTP_PROXY": OFFLINE_PROXY,
        "HTTPS_PROXY": OFFLINE_PROXY,
        "NO_PROXY": "127.0.0.1,localhost",
        "PATH": container_path,
        "PYO3_PYTHON": CONTAINER_PYTHON,
        "PYTHONDONTWRITEBYTECODE": "1",
        "all_proxy": OFFLINE_PROXY,
        "http_proxy": OFFLINE_PROXY,
        "https_proxy": OFFLINE_PROXY,
        "no_proxy": "127.0.0.1,localhost",
    }
    command = [
        docker,
        "run",
        "--rm",
        "--network",
        "none",
        "--pull",
        "never",
        "--entrypoint",
        "/bin/bash",
        "--workdir",
        str(repository),
        "--mount",
        f"type=bind,src={repository},dst={repository},readonly",
        "--mount",
        f"type=bind,src={suite},dst={suite}",
        "--mount",
        f"type=bind,src={cargo_registry},dst=/root/.cargo/registry,readonly",
    ]
    for name, value in sorted(container_environment.items()):
        command.extend(("--env", f"{name}={value}"))
    command.extend(
        (
            BUILDER_IMAGE,
            "-euo",
            "pipefail",
            "-c",
            CONTAINER_SCRIPT,
            "bash",
            str(os.getuid()),
            str(os.getgid()),
            str(suite),
            str(repository),
            str(output),
            CONTAINER_PYTHON,
        )
    )
    run_command(
        command,
        cwd=repository,
        env=environ,
        label="one-wheel compatibility build",
        timeout=600.0,
    )
    wheels = list(output.glob("*.whl"))
    require(len(wheels) == 1, f"compatibility build produced {len(wheels)} wheels")
    wheel = wheels[0]
    require(wheel.name == WHEEL_NAME, f"compatibility wheel filename drifted: {wheel.name}")
    return wheel


def validate_wheel(repository: Path, wheel: Path) -> tuple[str, int]:
    try:
        expected = json.loads(
            (repository / "tests/fixtures/release/diagnostics-wheel-expected.json").read_text(
                encoding="utf-8"
            )
        )
        expected_members = expected["wheel_members"]
        with zipfile.ZipFile(wheel) as archive:
            infos = [info for info in archive.infolist() if not info.is_dir()]
            names = [info.filename for info in infos]
            require(len(names) == len(set(names)), "compatibility wheel has duplicate members")
            native = [
                name
                for name in names
                if re.fullmatch(r"troupe/_runtime(?:\.[A-Za-z0-9_]+)*\.so", name)
            ]
            require(len(native) == 1, "compatibility wheel does not have one native module")
            template = ["troupe/<native>" if name == native[0] else name for name in names]
            require(sorted(template) == sorted(expected_members), "compatibility wheel members drifted")
            for info in infos:
                archive.read(info)
    except (OSError, KeyError, TypeError, json.JSONDecodeError, zipfile.BadZipFile) as error:
        raise RunnerError(f"could not validate compatibility wheel: {error}") from error
    return sha256_file(wheel), wheel.stat().st_size


def parse_probe(stdout: str, version: str, wheel_sha256: str) -> dict[str, Any]:
    try:
        require(stdout.endswith("\n") and stdout.count("\n") == 1, "probe output is not one line")
        value = json.loads(stdout)
    except json.JSONDecodeError as error:
        raise RunnerError(f"CPython {version} probe returned invalid JSON") from error
    fields = {
        "schema",
        "python",
        "implementation",
        "wheel_sha256",
        "native_sha256",
        "native_bytes",
        "package_members",
        "extensions",
        "runtime",
    }
    require(isinstance(value, dict) and set(value) == fields, f"CPython {version} probe fields drifted")
    require(
        value["schema"] == PROBE_SCHEMA
        and value["python"] == version
        and value["implementation"] == "CPython"
        and value["wheel_sha256"] == wheel_sha256,
        f"CPython {version} probe identity drifted",
    )
    require(
        value["package_members"]
        == ["__init__.py", "__init__.pyi", "act_schema.pyi", "diagnostics.pyi", "py.typed"],
        f"CPython {version} installed package members drifted",
    )
    extensions = value["extensions"]
    runtime = value["runtime"]
    require(
        isinstance(extensions, dict)
        and extensions
        == {
            "sink": True,
            "custom": True,
            "view_renderers": ["timeline", "metric", "table", "time_series"],
        },
        f"CPython {version} extension surface drifted",
    )
    require(
        isinstance(runtime, dict)
        and set(runtime) == {"active", "archive", "trace_bytes", "production_imports"}
        and runtime["active"] == "passed"
        and runtime["archive"] == "passed"
        and runtime["production_imports"] == 1
        and isinstance(runtime["trace_bytes"], int)
        and runtime["trace_bytes"] > 0,
        f"CPython {version} runtime smoke drifted",
    )
    require(
        isinstance(value["native_sha256"], str)
        and re.fullmatch(r"[0-9a-f]{64}", value["native_sha256"]) is not None
        and isinstance(value["native_bytes"], int)
        and value["native_bytes"] > 0,
        f"CPython {version} native observation drifted",
    )
    return value


def run_version(
    repository: Path,
    suite: Path,
    version: str,
    interpreter: Path,
    wheel: Path,
    wheel_sha256: str,
    environ: Mapping[str, str],
) -> dict[str, Any]:
    environment_root = suite / "venvs" / version
    environment_root.parent.mkdir(exist_ok=True)
    run_command(
        [str(interpreter), "-I", "-m", "venv", str(environment_root)],
        cwd=suite,
        env=environ,
        label=f"CPython {version} venv creation",
        timeout=60.0,
    )
    child_python = environment_root / "bin" / "python"
    require(child_python.exists(), f"CPython {version} venv has no Python")
    workspace = suite / "workspaces" / version
    workspace.mkdir(parents=True)
    child_environment = clean_environment(environ)
    child_environment["PATH"] = str(environment_root / "bin")
    child_environment["TMPDIR"] = str(workspace)
    child_environment["TMP"] = str(workspace)
    run_command(
        [
            str(child_python),
            "-m",
            "pip",
            "install",
            "--no-index",
            "--no-deps",
            str(wheel),
        ],
        cwd=workspace,
        env=child_environment,
        label=f"CPython {version} wheel install",
        timeout=60.0,
    )
    probe = run_command(
        [
            str(child_python),
            str(repository / "tests/release/diagnostics_python_compat.py"),
            "--workspace",
            str(workspace),
            "--wheel",
            str(wheel),
            "--expected-python",
            version,
            "--expected-wheel-sha256",
            wheel_sha256,
        ],
        cwd=workspace,
        env=child_environment,
        label=f"CPython {version} installed-wheel probe",
        timeout=90.0,
    )
    require(probe.stderr == "", f"CPython {version} probe emitted stderr")
    return parse_probe(probe.stdout, version, wheel_sha256)


def owned_cleanup(suite: Path, marker: Path, identity: tuple[int, int], marker_value: str) -> None:
    try:
        metadata = suite.lstat()
        current_marker = marker.read_text(encoding="ascii")
    except OSError as error:
        raise RunnerError(f"could not validate compatibility cleanup ownership: {error}") from error
    require(
        stat.S_ISDIR(metadata.st_mode)
        and not stat.S_ISLNK(metadata.st_mode)
        and (metadata.st_dev, metadata.st_ino) == identity
        and current_marker == marker_value,
        "refusing to clean unowned compatibility temporary state",
    )
    try:
        shutil.rmtree(suite)
    except OSError as error:
        raise RunnerError(f"could not clean compatibility temporary state: {error}") from error


def execute(repository: Path, arguments: list[str]) -> dict[str, Any]:
    require(
        arguments == ["--versions", VERSION_ARGUMENT, "--build-current-wheel-once"],
        "usage: test_diagnostics_python_compat.sh --versions 3.10,3.11,3.12,3.13,3.14 --build-current-wheel-once",
    )
    repository = exact_directory(repository, "repository root")
    base_environment = clean_environment(os.environ)
    initial_checkout = checkout_state(repository, base_environment)
    require(initial_checkout == b"", "compatibility runner requires a clean checkout")
    interpreters, interpreter_observations = discover_interpreters(base_environment)
    temporary_value = Path(os.environ.get("TROUPE_GATE_TMP", tempfile.gettempdir()))
    temporary_base = exact_directory(temporary_value, "compatibility temporary base")
    try:
        temporary_base.relative_to(repository)
    except ValueError:
        pass
    else:
        raise RunnerError("compatibility temporary base must remain outside the repository")
    suite = Path(tempfile.mkdtemp(prefix="troupe-python-compat.", dir=temporary_base))
    metadata = suite.lstat()
    identity = (metadata.st_dev, metadata.st_ino)
    marker = suite / ".troupe-python-compat-owned"
    marker_value = f"{os.getpid()}:{os.urandom(16).hex()}\n"
    marker.write_text(marker_value, encoding="ascii")
    result: dict[str, Any] | None = None
    failure: BaseException | None = None
    try:
        wheel = build_wheel(repository, suite, base_environment)
        wheel_sha256, wheel_bytes = validate_wheel(repository, wheel)
        observations = [
            run_version(
                repository,
                suite,
                version,
                interpreters[version],
                wheel,
                wheel_sha256,
                base_environment,
            )
            for version in VERSIONS
        ]
        native_identities = {
            (item["native_sha256"], item["native_bytes"]) for item in observations
        }
        require(len(native_identities) == 1, "CPython versions loaded different native modules")
        require(
            checkout_state(repository, base_environment) == initial_checkout,
            "compatibility runner changed the checkout",
        )
        result = {
            "schema": RUNNER_SCHEMA,
            "result": "passed",
            "wheel": {
                "filename": wheel.name,
                "sha256": wheel_sha256,
                "bytes": wheel_bytes,
                "builds": 1,
                "builder_image": BUILDER_IMAGE,
                "target": BUILD_TARGET,
            },
            "interpreters": interpreter_observations,
            "versions": observations,
        }
    except BaseException as error:
        failure = error
    try:
        owned_cleanup(suite, marker, identity, marker_value)
    except BaseException as error:
        if failure is None:
            failure = error
    if failure is not None:
        raise failure
    if result is None:
        raise RunnerError("compatibility result is unavailable")
    return result


def main() -> int:
    repository = Path(sys.argv[1])
    try:
        result = execute(repository, sys.argv[2:])
    except RunnerError as error:
        print(f"troupe Python compatibility gate failed: {error}", file=sys.stderr)
        return 1
    print(canonical_json(result))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
PY
