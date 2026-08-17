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
import secrets
import shutil
import stat
import subprocess
import sys
import tempfile
from pathlib import Path


BLOCKED_TOOLS = {
    "node",
    "nodejs",
    "npm",
    "npx",
    "protoc",
    "perfetto",
    "trace_processor_shell",
}
HIDDEN_TOOLS = BLOCKED_TOOLS | {"uv"}
OFFLINE_PROXY = "http://127.0.0.1:9/"
BUILDER_IMAGE = (
    "ghcr.io/pyo3/maturin@"
    "sha256:2665227312dd1eab1c29c70a001dc8aac53155a2d048bede3b2df7f1691c8e38"
)
BUILD_TARGET = "x86_64-unknown-linux-gnu"
CONTAINER_PYTHON = "/opt/python/cp310-cp310/bin/python"


class RunnerError(RuntimeError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RunnerError(message)


def parse(arguments: list[str]) -> tuple[str, Path]:
    if len(arguments) != 5 or arguments[:3] != ["--offline", "--smoke", "active,archive"]:
        raise RunnerError(
            "usage: test_diagnostics_wheel.sh --offline --smoke active,archive --report REPORT"
        )
    require(arguments[3] == "--report", "--report must be the final option")
    return arguments[2], Path(arguments[4])


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


def populate_tool_bin(destination: Path, original_path: str) -> None:
    seen_directories: set[Path] = set()
    for raw in original_path.split(os.pathsep):
        if not raw:
            continue
        try:
            directory = Path(raw).resolve(strict=True)
        except OSError:
            continue
        if directory in seen_directories or not directory.is_dir():
            continue
        seen_directories.add(directory)
        try:
            entries = sorted(directory.iterdir(), key=lambda path: path.name)
        except OSError:
            continue
        for entry in entries:
            if entry.name.casefold() in HIDDEN_TOOLS:
                continue
            target = destination / entry.name
            if target.exists() or target.is_symlink():
                continue
            try:
                resolved = entry.resolve(strict=True)
                if not resolved.is_file() or not os.access(resolved, os.X_OK):
                    continue
                target.symlink_to(resolved)
            except OSError:
                continue


def run(repository: Path, arguments: list[str]) -> int:
    smoke, report = parse(arguments)
    gate_raw = os.environ.get("TROUPE_GATE_TMP")
    require(gate_raw is not None, "TROUPE_GATE_TMP is required")
    gate = exact_directory(Path(gate_raw), "TROUPE_GATE_TMP")
    require(not gate.is_relative_to(repository), "TROUPE_GATE_TMP must be outside the repository")
    require(report.is_absolute() and str(report) == os.path.abspath(report), "report path must be absolute")
    require(report.parent == gate and report.name == "V07-wheel-report.json", "report must be the exact V07 child of TROUPE_GATE_TMP")
    require(not report.exists() and not report.is_symlink(), "report path already exists")

    suite = Path(tempfile.mkdtemp(prefix="troupe-v07.", dir=gate))
    suite_metadata = suite.lstat()
    marker = suite / ".troupe-v07-owner"
    marker_value = f"{os.getpid()}:{secrets.token_hex(16)}\n"
    marker.write_text(marker_value, encoding="ascii")
    try:
        tool_bin = suite / "tools"
        tool_bin.mkdir()
        original_path = os.environ.get("PATH", "")
        populate_tool_bin(tool_bin, original_path)
        sanitized_path = str(tool_bin)
        for name in HIDDEN_TOOLS:
            require(shutil.which(name, path=sanitized_path) is None, f"failed to hide {name}")
        for name in ("docker",):
            require(shutil.which(name, path=sanitized_path) is not None, f"required build tool is unavailable: {name}")

        temporary = suite / "tmp"
        temporary.mkdir()
        container_tools = suite / "container-tools"
        container_tools.mkdir()
        (container_tools / "git").symlink_to("/usr/local/bin/git")
        (container_tools / "patchelf").symlink_to("/usr/local/bin/patchelf")
        (container_tools / "cmake").symlink_to("/usr/local/bin/cmake")
        cargo_root = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
        cargo_registry = exact_directory(cargo_root / "registry", "Cargo registry")
        environment = dict(os.environ)
        for name in list(environment):
            folded = name.casefold()
            if folded.startswith("npm_config_") or name in {
                "TROUPE_NPM_CACHE",
                "TROUPE_PLAYWRIGHT_CACHE",
                "TROUPE_PERFETTO_CACHE",
            }:
                environment.pop(name, None)
        environment.update(
            {
                "PATH": sanitized_path,
                "TMPDIR": str(temporary),
                "CARGO_NET_OFFLINE": "true",
                "PIP_NO_INDEX": "1",
                "UV_OFFLINE": "1",
                "HTTP_PROXY": OFFLINE_PROXY,
                "HTTPS_PROXY": OFFLINE_PROXY,
                "ALL_PROXY": OFFLINE_PROXY,
                "http_proxy": OFFLINE_PROXY,
                "https_proxy": OFFLINE_PROXY,
                "all_proxy": OFFLINE_PROXY,
                "NO_PROXY": "127.0.0.1,localhost",
                "no_proxy": "127.0.0.1,localhost",
                "TROUPE_DIAGNOSTICS_WHEEL_OFFLINE": "1",
                "TROUPE_DIAGNOSTICS_WHEEL_SMOKE": smoke,
                "TROUPE_DIAGNOSTICS_WHEEL_REPORT": str(report),
                "TROUPE_DIAGNOSTICS_WHEEL_EXPECTED": str(
                    repository / "tests/fixtures/release/diagnostics-wheel-expected.json"
                ),
                "TROUPE_DIAGNOSTICS_WHEEL_REPORT_SCHEMA": str(
                    repository / "tests/fixtures/release/diagnostics-wheel-report-schema.json"
                ),
                "TROUPE_DIAGNOSTICS_WHEEL_BUILDER_IMAGE": BUILDER_IMAGE,
                "TROUPE_DIAGNOSTICS_WHEEL_TARGET": BUILD_TARGET,
            }
        )
        verifier = repository / "scripts/verify_wheel.py"
        container_path = os.pathsep.join(
            (
                str(container_tools),
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
            "CARGO_TARGET_DIR": str(temporary / "cargo-target"),
            "GIT_CONFIG_COUNT": "1",
            "GIT_CONFIG_KEY_0": "safe.directory",
            "GIT_CONFIG_VALUE_0": str(repository),
            "HTTP_PROXY": OFFLINE_PROXY,
            "HTTPS_PROXY": OFFLINE_PROXY,
            "NO_PROXY": "127.0.0.1,localhost",
            "PATH": container_path,
            "PIP_NO_INDEX": "1",
            "PYO3_PYTHON": CONTAINER_PYTHON,
            "PYTHONDONTWRITEBYTECODE": "1",
            "TMPDIR": str(temporary),
            "TROUPE_DIAGNOSTICS_WHEEL_BUILDER_IMAGE": BUILDER_IMAGE,
            "TROUPE_DIAGNOSTICS_WHEEL_EXPECTED": environment[
                "TROUPE_DIAGNOSTICS_WHEEL_EXPECTED"
            ],
            "TROUPE_DIAGNOSTICS_WHEEL_OFFLINE": "1",
            "TROUPE_DIAGNOSTICS_WHEEL_REPORT": str(report),
            "TROUPE_DIAGNOSTICS_WHEEL_REPORT_SCHEMA": environment[
                "TROUPE_DIAGNOSTICS_WHEEL_REPORT_SCHEMA"
            ],
            "TROUPE_DIAGNOSTICS_WHEEL_SMOKE": smoke,
            "TROUPE_DIAGNOSTICS_WHEEL_TARGET": BUILD_TARGET,
            "UV_OFFLINE": "1",
            "all_proxy": OFFLINE_PROXY,
            "http_proxy": OFFLINE_PROXY,
            "https_proxy": OFFLINE_PROXY,
            "no_proxy": "127.0.0.1,localhost",
        }
        container_script = r'''
owner_uid=$1
owner_gid=$2
owned_suite=$3
owned_report=$4
container_python=$5
verifier=$6

restore_ownership() {
  original_status=$?
  trap - EXIT
  ownership_status=0
  chown -hR -- "${owner_uid}:${owner_gid}" "$owned_suite" || ownership_status=125
  if [[ -f "$owned_report" && ! -L "$owned_report" ]]; then
    chown --no-dereference -- "${owner_uid}:${owner_gid}" "$owned_report" || ownership_status=125
  fi
  if ((ownership_status != 0)); then
    exit "$ownership_status"
  fi
  exit "$original_status"
}
trap restore_ownership EXIT

required_tools=(
  python maturin cargo rustc cc c++ ar ranlib ld strip cmake make perl
  pkg-config git patchelf
)
for tool in "${required_tools[@]}"; do
  command -v "$tool" >/dev/null
done
for tool in node nodejs npm npx protoc perfetto trace_processor_shell uv; do
  if command -v "$tool" >/dev/null; then
    printf 'forbidden build tool is available: %s\n' "$tool" >&2
    exit 97
  fi
done
printf 'int main(void) { return 0; }\n' | cc -x c - -o "${TMPDIR:?}/cc-smoke"
"${TMPDIR:?}/cc-smoke"
printf 'fn main() {}\n' > "${TMPDIR:?}/rustc-smoke.rs"
rustc --target x86_64-unknown-linux-gnu \
  "${TMPDIR:?}/rustc-smoke.rs" \
  -o "${TMPDIR:?}/rustc-smoke"
"${TMPDIR:?}/rustc-smoke"
cargo metadata \
  --offline \
  --locked \
  --no-deps \
  --format-version 1 \
  --manifest-path "$PWD/rust/Cargo.toml" \
  >/dev/null
git rev-parse --verify HEAD >/dev/null

"$container_python" "$verifier" \
  --build \
  --release \
  --target x86_64-unknown-linux-gnu \
  --manylinux 2_17
'''
        command = [
            "docker",
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
            f"type=bind,src={gate},dst={gate}",
            "--mount",
            f"type=bind,src={cargo_registry},dst=/root/.cargo/registry,readonly",
        ]
        for name, value in sorted(container_environment.items()):
            command.extend(["--env", f"{name}={value}"])
        command.extend(
            [
                BUILDER_IMAGE,
                "-euo",
                "pipefail",
                "-c",
                container_script,
                "troupe-v07-container",
                str(os.getuid()),
                str(os.getgid()),
                str(suite),
                str(report),
                CONTAINER_PYTHON,
                str(verifier),
            ]
        )
        completed = subprocess.run(
            command,
            cwd=repository,
            env=environment,
            stdin=subprocess.DEVNULL,
            check=False,
        )
        require(completed.returncode == 0, f"wheel verifier exited {completed.returncode}")
        try:
            metadata = report.lstat()
            resolved = report.resolve(strict=True)
        except OSError as error:
            raise RunnerError(f"wheel verifier did not publish its report: {error}") from error
        require(stat.S_ISREG(metadata.st_mode) and not stat.S_ISLNK(metadata.st_mode), "wheel report is not a regular file")
        require(resolved == report, "wheel report moved through symlink indirection")
        summary = {
            "schema": "troupe.diagnostics.wheel-runner.v1",
            "result": "passed",
            "report": report.name,
            "report_sha256": sha256_file(report),
        }
        print(json.dumps(summary, sort_keys=True, separators=(",", ":")))
        return 0
    finally:
        try:
            current_suite = suite.lstat()
            current_marker = marker.lstat()
            cleanup_is_exact = (
                suite.parent == gate
                and suite.name.startswith("troupe-v07.")
                and stat.S_ISDIR(current_suite.st_mode)
                and not stat.S_ISLNK(current_suite.st_mode)
                and (current_suite.st_dev, current_suite.st_ino)
                == (suite_metadata.st_dev, suite_metadata.st_ino)
                and suite.resolve(strict=True) == suite
                and stat.S_ISREG(current_marker.st_mode)
                and not stat.S_ISLNK(current_marker.st_mode)
                and marker.read_text(encoding="ascii") == marker_value
            )
        except OSError:
            cleanup_is_exact = False
        if not cleanup_is_exact:
            raise RunnerError("owned temporary directory identity changed before cleanup")
        shutil.rmtree(suite)


def main() -> int:
    repository = Path(sys.argv[1]).resolve(strict=True)
    try:
        return run(repository, sys.argv[2:])
    except RunnerError as error:
        print(f"troupe diagnostics wheel gate failed: {error}", file=sys.stderr)
        return 1


raise SystemExit(main())
PY
