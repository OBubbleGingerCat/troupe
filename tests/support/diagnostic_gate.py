from __future__ import annotations

import base64
import csv
import hashlib
import io
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import zipfile
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Iterator, Mapping, Sequence


NODE_ID = re.compile(r"[A-Z][0-9]{2}")
NATIVE_MEMBER = re.compile(r"troupe/_runtime(?:\.[A-Za-z0-9_]+)*\.so")
WHEEL_FILENAME = re.compile(r"troupe-0\.1\.0-[^-]+-[^-]+-.+\.whl")
ENV_REFERENCE = re.compile(r"\$\{(?P<name>[A-Z][A-Z0-9_]*):\?\}")
WHEEL_RECORD = "troupe-0.1.0.dist-info/RECORD"
WHEEL_METADATA = "troupe-0.1.0.dist-info/METADATA"
ALLOWED_FEATURES = frozenset({"agent-test-support", "diagnostics-test-support"})
CACHE_ENV = {
    "npm": "TROUPE_NPM_CACHE",
    "perfetto": "TROUPE_PERFETTO_CACHE",
    "playwright": "TROUPE_PLAYWRIGHT_CACHE",
}
PERSISTENT_EVIDENCE_ENV = frozenset(
    {"TROUPE_DIAGNOSTICS_EVIDENCE", "TROUPE_FINAL_ATTEMPT_ID"}
)
PERSISTENT_EVIDENCE_NODES = frozenset({"V03", "V16"})
GATE_ENV_NAMES = frozenset(
    {
        "INTEGRATION_SHA",
        "PLAN_BUNDLE_SHA",
        "PRODUCT_BASE_SHA",
        "TROUPE_DIAGNOSTICS_EVIDENCE",
        "TROUPE_FINAL_ATTEMPT_ID",
        "TROUPE_GATE_TMP",
        "TROUPE_NPM_CACHE",
        "TROUPE_PERFETTO_CACHE",
        "TROUPE_PLAYWRIGHT_CACHE",
    }
)
SHELL_EXECUTABLES = frozenset(
    {"bash", "cmd", "dash", "env", "fish", "powershell", "pwsh", "sh", "zsh"}
)
FRESH_EXECUTABLES = frozenset({"mypy", "pytest", "python", "python3", "stubtest", "troupe"})


class GateError(RuntimeError):
    pass


def _is_within(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
    except ValueError:
        return False
    return True


def _identity(path: Path) -> tuple[int, int]:
    value = path.lstat()
    return value.st_dev, value.st_ino


def _regular_directory(path: Path, *, context: str) -> tuple[int, int]:
    try:
        value = path.lstat()
    except OSError as error:
        raise GateError(f"could not inspect {context}: {error}") from error
    if stat.S_ISLNK(value.st_mode) or not stat.S_ISDIR(value.st_mode):
        raise GateError(f"{context} must be a regular directory")
    return value.st_dev, value.st_ino


@dataclass(frozen=True, slots=True)
class OwnedWorkspace:
    repository: Path
    parent: Path
    root: Path
    parent_device: int
    parent_inode: int
    root_device: int
    root_inode: int

    @property
    def home(self) -> Path:
        return self.root / "home"

    @property
    def tmp(self) -> Path:
        return self.root / "tmp"

    @property
    def uv_cache(self) -> Path:
        return self.root / "uv-cache"

    @property
    def cargo_home(self) -> Path:
        return self.root / "cargo-home"

    @property
    def venv(self) -> Path:
        return self.root / "venv"

    @property
    def target(self) -> Path:
        return self.root / "target"

    @property
    def wheels(self) -> Path:
        return self.root / "wheels"

    @property
    def npm_cache(self) -> Path:
        return self.root / "npm-cache"


@dataclass(frozen=True, slots=True)
class NativeWheel:
    member: str
    data: bytes
    sha256: str


def writable_paths(workspace: OwnedWorkspace) -> tuple[Path, ...]:
    return (
        workspace.home,
        workspace.tmp,
        workspace.uv_cache,
        workspace.cargo_home,
        workspace.venv,
        workspace.target,
        workspace.wheels,
        workspace.npm_cache,
    )


def create_owned_workspace(repository_root: Path, node_id: str) -> OwnedWorkspace:
    if NODE_ID.fullmatch(node_id) is None:
        raise GateError(f"invalid diagnostic node ID: {node_id!r}")
    try:
        repository = repository_root.resolve(strict=True)
    except OSError as error:
        raise GateError(f"could not resolve repository root: {error}") from error
    if not repository.is_dir():
        raise GateError("repository root must be a directory")

    parent = repository / ".troupe-test"
    try:
        parent.mkdir(mode=0o700, exist_ok=True)
    except OSError as error:
        raise GateError(f"could not create owned gate parent: {error}") from error
    parent_device, parent_inode = _regular_directory(parent, context="owned gate parent")
    if parent.resolve(strict=True) != parent or not _is_within(parent, repository):
        raise GateError("owned gate parent escapes repository")

    try:
        root = Path(tempfile.mkdtemp(prefix=f"{node_id.lower()}.", dir=parent))
        root_device, root_inode = _regular_directory(root, context="owned gate workspace")
        if root.resolve(strict=True) != root or not _is_within(root, parent):
            raise GateError("owned gate workspace escapes repository")
    except BaseException:
        if "root" in locals() and root.exists() and not root.is_symlink():
            shutil.rmtree(root)
        raise
    return OwnedWorkspace(
        repository=repository,
        parent=parent,
        root=root,
        parent_device=parent_device,
        parent_inode=parent_inode,
        root_device=root_device,
        root_inode=root_inode,
    )


def _existing_workspace(
    repository_root: Path,
    workspace_root: Path,
    identities: Sequence[int],
) -> OwnedWorkspace:
    if len(identities) != 4:
        raise GateError("owned workspace identity is malformed")
    repository = repository_root.resolve(strict=True)
    parent = repository / ".troupe-test"
    root = workspace_root.absolute()
    workspace = OwnedWorkspace(repository, parent, root, *identities)
    if _identity(parent) != (workspace.parent_device, workspace.parent_inode):
        raise GateError("owned gate parent identity changed")
    if _identity(root) != (workspace.root_device, workspace.root_inode):
        raise GateError("owned gate workspace identity changed")
    if parent.resolve(strict=True) != parent or root.resolve(strict=True) != root:
        raise GateError("owned gate workspace escapes repository")
    if not _is_within(root, parent):
        raise GateError("owned gate workspace escapes repository")
    return workspace


def cleanup_owned_workspace(workspace: OwnedWorkspace) -> None:
    try:
        if _identity(workspace.parent) != (workspace.parent_device, workspace.parent_inode):
            raise GateError("owned gate parent identity changed during cleanup")
        if _identity(workspace.root) != (workspace.root_device, workspace.root_inode):
            raise GateError("owned gate workspace identity changed during cleanup")
        if workspace.parent.resolve(strict=True) != workspace.parent:
            raise GateError("owned gate parent identity changed during cleanup")
        if workspace.root.resolve(strict=True) != workspace.root:
            raise GateError("owned gate workspace identity changed during cleanup")
        if not _is_within(workspace.root, workspace.parent):
            raise GateError("owned gate workspace escaped during cleanup")
        shutil.rmtree(workspace.root)
    except GateError:
        raise
    except OSError as error:
        raise GateError(f"could not clean owned gate workspace: {error}") from error


@contextmanager
def owned_workspace(repository_root: Path, node_id: str) -> Iterator[OwnedWorkspace]:
    workspace = create_owned_workspace(repository_root, node_id)
    try:
        yield workspace
    finally:
        cleanup_owned_workspace(workspace)


def gate_environment(
    workspace: OwnedWorkspace,
    caller: Mapping[str, str],
) -> dict[str, str]:
    environment = dict(caller)
    caller_home = caller.get("HOME")
    rustup_home = caller.get("RUSTUP_HOME")
    if rustup_home is None and caller_home:
        candidate = Path(caller_home) / ".rustup"
        if candidate.is_dir():
            rustup_home = str(candidate.resolve())

    for name in (
        "CARGO_HOME",
        "CARGO_TARGET_DIR",
        "CONDA_PREFIX",
        "HOME",
        "NPM_CONFIG_CACHE",
        "PIP_CACHE_DIR",
        "PYO3_PYTHON",
        "PYTHONHOME",
        "PYTHONPATH",
        "RUSTC_WRAPPER",
        "TMP",
        "TMPDIR",
        "UV_CACHE_DIR",
        "UV_PROJECT_ENVIRONMENT",
        "VIRTUAL_ENV",
        "XDG_CACHE_HOME",
    ):
        environment.pop(name, None)
    for path in writable_paths(workspace):
        if path != workspace.venv:
            path.mkdir(parents=True, exist_ok=False)
    environment.update(
        {
            "CARGO_HOME": str(workspace.cargo_home),
            "CARGO_TARGET_DIR": str(workspace.target),
            "HOME": str(workspace.home),
            "NPM_CONFIG_CACHE": str(workspace.npm_cache),
            "PIP_CACHE_DIR": str(workspace.uv_cache / "pip"),
            "PYO3_PYTHON": str(workspace.venv / "bin/python"),
            "PYTHONDONTWRITEBYTECODE": "1",
            "PYTEST_ADDOPTS": "-p no:cacheprovider",
            "TMP": str(workspace.tmp),
            "TMPDIR": str(workspace.tmp),
            "UV_CACHE_DIR": str(workspace.uv_cache),
            "UV_PROJECT_ENVIRONMENT": str(workspace.venv),
            "XDG_CACHE_HOME": str(workspace.home / ".cache"),
        }
    )
    if rustup_home is not None:
        environment["RUSTUP_HOME"] = rustup_home
    return environment


def validated_features(manifest: Path, features: Sequence[str]) -> str:
    if not features:
        raise GateError("native gate descriptor must select at least one maturin feature")
    if len(features) != len(set(features)):
        raise GateError("native gate descriptor contains a duplicate feature")
    unknown_contract = set(features) - ALLOWED_FEATURES
    if unknown_contract:
        raise GateError(f"native gate descriptor has unknown features: {sorted(unknown_contract)}")
    try:
        try:
            import tomllib
        except ModuleNotFoundError:
            import tomli as tomllib  # type: ignore[no-redef]
        parsed = tomllib.loads(manifest.read_text(encoding="utf-8"))
        available = parsed["features"]
    except (OSError, UnicodeError, KeyError, TypeError, ValueError) as error:
        raise GateError(f"could not inspect current Cargo features: {error}") from error
    missing = [feature for feature in features if feature not in available]
    if missing:
        raise GateError(f"native gate descriptor selects unknown current manifest features: {missing}")
    return ",".join(features)


def _resolved_artifact(path: str, target: Path) -> Path:
    candidate = Path(path)
    if not candidate.is_absolute():
        raise GateError(f"Cargo artifact path is not absolute: {path!r}")
    try:
        resolved = candidate.resolve(strict=True)
    except OSError as error:
        raise GateError(f"Cargo artifact path is unavailable: {path!r}") from error
    if not _is_within(resolved, target.resolve(strict=True)):
        raise GateError(f"Cargo artifact path is outside owned target: {path!r}")
    return resolved


def validate_cargo_artifacts(output: str, target: Path) -> None:
    artifacts = 0
    try:
        for line in output.splitlines():
            if not line:
                continue
            message = json.loads(line)
            if not isinstance(message, dict):
                raise TypeError("Cargo message is not an object")
            reason = message.get("reason")
            if reason == "compiler-artifact":
                filenames = message.get("filenames")
                if not isinstance(filenames, list) or any(not isinstance(item, str) for item in filenames):
                    raise TypeError("compiler artifact filenames are malformed")
                for filename in filenames:
                    _resolved_artifact(filename, target)
                    artifacts += 1
                executable = message.get("executable")
                if executable is not None:
                    if not isinstance(executable, str):
                        raise TypeError("compiler artifact executable is malformed")
                    _resolved_artifact(executable, target)
                    artifacts += 1
            elif reason == "build-script-executed":
                out_dir = message.get("out_dir")
                if not isinstance(out_dir, str):
                    raise TypeError("build script output directory is malformed")
                _resolved_artifact(out_dir, target)
    except (json.JSONDecodeError, TypeError) as error:
        raise GateError(f"Cargo JSON artifact stream is malformed: {error}") from error
    if artifacts == 0:
        raise GateError("Cargo JSON artifact stream contains no compiler artifacts")


def select_built_wheel(wheel_directory: Path, build_started_ns: int) -> Path:
    try:
        entries = list(wheel_directory.iterdir())
    except OSError as error:
        raise GateError(f"could not inspect wheel output: {error}") from error
    wheels = [entry for entry in entries if entry.suffix == ".whl"]
    if len(wheels) != 1 or len(entries) != 1:
        raise GateError("maturin must produce exactly one wheel and no foreign output")
    wheel = wheels[0]
    if WHEEL_FILENAME.fullmatch(wheel.name) is None:
        raise GateError("maturin produced a foreign wheel filename")
    value = wheel.lstat()
    if stat.S_ISLNK(value.st_mode) or not stat.S_ISREG(value.st_mode):
        raise GateError("built wheel must be a regular file")
    if wheel.resolve(strict=True).parent != wheel_directory.resolve(strict=True):
        raise GateError("built wheel escapes owned output")
    if value.st_mtime_ns < build_started_ns:
        raise GateError("built wheel is stale")
    return wheel


def _safe_archive_name(name: str) -> bool:
    if not name or name.startswith("/") or "\\" in name or name.endswith("/"):
        return False
    parts = name.split("/")
    path = PurePosixPath(name)
    return not path.is_absolute() and all(part not in {"", ".", ".."} for part in parts)


def _regular_zip_member(info: zipfile.ZipInfo) -> bool:
    mode = info.external_attr >> 16
    file_type = stat.S_IFMT(mode)
    return file_type in {0, stat.S_IFREG}


def inspect_wheel(wheel: Path) -> NativeWheel:
    try:
        value = wheel.lstat()
        if stat.S_ISLNK(value.st_mode) or not stat.S_ISREG(value.st_mode):
            raise GateError("wheel must be a regular file")
        with zipfile.ZipFile(wheel) as archive:
            infos = archive.infolist()
            names = [info.filename for info in infos]
            if len(names) != len(set(names)):
                raise GateError("wheel contains duplicate members")
            if any(not _safe_archive_name(name) for name in names):
                raise GateError("wheel contains an unsafe member path")
            if any(not _regular_zip_member(info) for info in infos):
                raise GateError("wheel members must be regular files")
            native_members = [name for name in names if NATIVE_MEMBER.fullmatch(name)]
            if len(native_members) != 1:
                raise GateError("wheel must contain exactly one native runtime member")
            records = [name for name in names if name.endswith(".dist-info/RECORD")]
            if records != [WHEEL_RECORD] or WHEEL_METADATA not in names:
                raise GateError("wheel distribution identity is foreign")
            record = WHEEL_RECORD
            metadata_lines = archive.read(WHEEL_METADATA).decode("utf-8").splitlines()
            if "Name: troupe" not in metadata_lines or "Version: 0.1.0" not in metadata_lines:
                raise GateError("wheel distribution identity is foreign")
            rows = list(csv.reader(io.StringIO(archive.read(record).decode("utf-8"))))
            if any(len(row) != 3 for row in rows):
                raise GateError("wheel RECORD rows are malformed")
            paths = [row[0] for row in rows]
            if len(paths) != len(set(paths)) or set(paths) != set(names):
                raise GateError("wheel RECORD member inventory is not exact")
            for path, encoded_hash, encoded_size in rows:
                if path == record:
                    if encoded_hash or encoded_size:
                        raise GateError("wheel RECORD self row is malformed")
                    continue
                data = archive.read(path)
                expected_hash = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=")
                if encoded_hash != f"sha256={expected_hash.decode('ascii')}":
                    raise GateError(f"wheel RECORD hash does not match member: {path}")
                if encoded_size != str(len(data)):
                    raise GateError(f"wheel RECORD size does not match member: {path}")
            native = native_members[0]
            data = archive.read(native)
            return NativeWheel(native, data, hashlib.sha256(data).hexdigest())
    except GateError:
        raise
    except (OSError, UnicodeError, csv.Error, KeyError, zipfile.BadZipFile) as error:
        raise GateError(f"could not inspect built wheel: {error}") from error


def _payload_path(payload: Mapping[str, Any], name: str) -> Path:
    value = payload.get(name)
    if not isinstance(value, str) or not Path(value).is_absolute():
        raise GateError(f"installed origin probe has invalid {name}")
    return Path(value)


def validate_installed_origin(
    payload: Mapping[str, Any],
    workspace: OwnedWorkspace,
    wheel: NativeWheel,
    build_started_ns: int,
) -> None:
    expected = {"sys_executable", "console_script", "troupe_file", "runtime_file"}
    if set(payload) != expected:
        raise GateError("installed origin probe fields are not exact")
    venv = workspace.venv.absolute()
    executable = _payload_path(payload, "sys_executable")
    console = _payload_path(payload, "console_script")
    troupe_file = _payload_path(payload, "troupe_file")
    runtime_file = _payload_path(payload, "runtime_file")
    for name, path in (
        ("sys.executable", executable),
        ("console script", console),
        ("troupe module", troupe_file),
        ("native runtime", runtime_file),
    ):
        if not _is_within(path, venv):
            raise GateError(f"{name} is outside fresh venv")
        try:
            value = path.lstat()
        except OSError as error:
            raise GateError(f"{name} is unavailable: {error}") from error
        if name != "sys.executable" and (stat.S_ISLNK(value.st_mode) or not stat.S_ISREG(value.st_mode)):
            raise GateError(f"{name} is not a regular installed file")
        if name != "sys.executable" and not _is_within(path.resolve(strict=True), venv.resolve(strict=True)):
            raise GateError(f"{name} is outside fresh venv")
    if executable != workspace.venv / "bin/python":
        raise GateError("sys.executable is not the fresh venv Python")
    if console != workspace.venv / "bin/troupe":
        raise GateError("console script is not the fresh venv troupe command")
    if tuple(runtime_file.parts[-len(PurePosixPath(wheel.member).parts) :]) != PurePosixPath(
        wheel.member
    ).parts:
        raise GateError("installed native runtime path differs from wheel member")
    try:
        installed = runtime_file.read_bytes()
        runtime_mtime = runtime_file.stat().st_mtime_ns
    except OSError as error:
        raise GateError(f"could not inspect installed native runtime: {error}") from error
    if hashlib.sha256(installed).hexdigest() != wheel.sha256 or installed != wheel.data:
        raise GateError("installed native runtime does not match wheel RECORD member")
    if runtime_mtime < build_started_ns:
        raise GateError("installed native runtime is stale")


def resolve_command(
    command: Sequence[str],
    workspace: OwnedWorkspace,
    repository_root: Path,
    *,
    path: str | None = None,
) -> list[str]:
    if not command or any(not item or "\x00" in item or "\n" in item for item in command):
        raise GateError("gate command is malformed")
    executable = command[0]
    basename = Path(executable).name
    if basename in SHELL_EXECUTABLES:
        raise GateError("gate descriptor must not execute a shell")
    if basename in {"python", "python3"} and "-c" in command[1:]:
        raise GateError("gate descriptor must not execute inline Python")
    if basename in FRESH_EXECUTABLES and "/" not in executable:
        candidate = workspace.venv / "bin" / ("python" if basename == "python3" else basename)
    elif "/" in executable:
        candidate = Path(executable)
        if not candidate.is_absolute():
            if any(part == ".." for part in PurePosixPath(executable).parts):
                raise GateError(f"gate executable escapes repository: {executable}")
            candidate = repository_root / candidate
        try:
            resolved = candidate.resolve(strict=True)
        except OSError as error:
            raise GateError(f"gate executable is unavailable: {executable}") from error
        if not _is_within(resolved, repository_root.resolve(strict=True)):
            raise GateError(f"gate executable escapes repository: {executable}")
        candidate = resolved
    else:
        found = shutil.which(executable, path=path)
        if found is None:
            raise GateError(f"gate executable is unavailable: {executable}")
        candidate = Path(found).absolute()
    if not candidate.exists():
        raise GateError(f"gate executable is unavailable: {executable}")
    return [str(candidate), *command[1:]]


def _run(
    command: Sequence[str],
    *,
    cwd: Path,
    env: Mapping[str, str],
    capture: bool = False,
) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(
            list(command),
            cwd=cwd,
            env=dict(env),
            check=True,
            capture_output=capture,
            text=capture,
        )
    except subprocess.CalledProcessError as error:
        detail = ""
        if capture:
            output = "\n".join(part for part in (error.stdout, error.stderr) if part)
            detail = f": {output[-4000:]}" if output else ""
        raise GateError(f"gate command failed with exit code {error.returncode}{detail}") from error


def _current_checkout(repository: Path, environment: Mapping[str, str]) -> None:
    result = _run(
        ["git", "rev-parse", "--show-toplevel"],
        cwd=Path.cwd(),
        env=environment,
        capture=True,
    )
    try:
        current = Path(result.stdout.strip()).resolve(strict=True)
    except OSError as error:
        raise GateError("could not resolve current Git checkout") from error
    if current != repository:
        raise GateError("native gate runner is not executing in its current checkout")


def _workspace_metadata(
    cargo: str,
    manifest: Path,
    workspace: OwnedWorkspace,
    environment: Mapping[str, str],
) -> None:
    result = _run(
        [
            cargo,
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--no-deps",
            "--manifest-path",
            str(manifest),
        ],
        cwd=workspace.repository,
        env=environment,
        capture=True,
    )
    try:
        metadata = json.loads(result.stdout)
        cargo_root = Path(metadata["workspace_root"]).resolve(strict=True)
        target = Path(metadata["target_directory"]).resolve()
    except (OSError, KeyError, TypeError, json.JSONDecodeError) as error:
        raise GateError(f"Cargo workspace metadata is malformed: {error}") from error
    if cargo_root != (workspace.repository / "rust").resolve(strict=True):
        raise GateError("Cargo workspace root differs from current checkout")
    if target != workspace.target.resolve(strict=True):
        raise GateError("Cargo metadata selected a shared or foreign target")


def _descriptor_environment(
    node_id: str,
    environment: Mapping[str, str],
    policies: Mapping[str, str],
    cache_requirements: Sequence[str],
) -> dict[str, str]:
    if node_id not in PERSISTENT_EVIDENCE_NODES and PERSISTENT_EVIDENCE_ENV & policies.keys():
        raise GateError("ordinary diagnostic node must not request persistent evidence environment")
    result = dict(environment)
    caller_values = {name: result.get(name) for name in GATE_ENV_NAMES}
    for name in GATE_ENV_NAMES:
        result.pop(name, None)
    for name, policy in policies.items():
        if policy == "required":
            value = caller_values.get(name)
            if not value:
                raise GateError(f"required gate environment is missing: {name}")
            result[name] = value
        elif policy == "optional":
            value = caller_values.get(name)
            if value is not None:
                result[name] = value
        else:
            result[name] = policy
    for cache in cache_requirements:
        name = CACHE_ENV[cache]
        value = caller_values.get(name)
        if not value:
            raise GateError(f"required gate cache is missing: {name}")
        result[name] = value
    return result


def _expand_argument(argument: str, environment: Mapping[str, str]) -> str:
    def replace(match: re.Match[str]) -> str:
        name = match.group("name")
        value = environment.get(name)
        if not value:
            raise GateError(f"gate argument requires missing environment: {name}")
        return value

    expanded = ENV_REFERENCE.sub(replace, argument)
    if "${" in expanded:
        raise GateError(f"gate argument contains an unsupported environment reference: {argument}")
    return expanded


def _origin_probe(workspace: OwnedWorkspace, environment: Mapping[str, str]) -> dict[str, Any]:
    support = Path(__file__).resolve(strict=True)
    result = _run(
        [str(workspace.venv / "bin/python"), "-B", str(support), "origin-probe"],
        cwd=workspace.tmp,
        env=environment,
        capture=True,
    )
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise GateError("installed origin probe did not return JSON") from error
    if not isinstance(payload, dict):
        raise GateError("installed origin probe did not return an object")
    return payload


def _execute_gate(repository: Path, node_id: str, workspace: OwnedWorkspace) -> None:
    support = repository / "tests/support"
    sys.path.insert(0, str(support))
    try:
        from artifact_layout import ArtifactLayoutError, load_gate_descriptors
    except ImportError as error:
        raise GateError("fresh gate environment cannot load artifact contract") from error
    try:
        descriptors = load_gate_descriptors(repository)
    except ArtifactLayoutError as error:
        raise GateError(str(error)) from error
    descriptor = descriptors.get(node_id)
    if descriptor is None:
        raise GateError(f"unknown diagnostic node: {node_id}")
    if descriptor.state != "realized":
        raise GateError(f"diagnostic node gate is not realized: {node_id}")

    environment = dict(os.environ)
    command_environment = _descriptor_environment(
        node_id,
        environment,
        descriptor.env,
        descriptor.cache_requirements,
    )
    _current_checkout(repository, environment)
    manifest = (repository / "rust/Cargo.toml").resolve(strict=True)
    features = validated_features(manifest, descriptor.maturin_features)
    cargo = shutil.which("cargo", path=environment.get("PATH"))
    uv = shutil.which("uv", path=environment.get("PATH"))
    if cargo is None or uv is None:
        raise GateError("native gate requires cargo and uv")
    _workspace_metadata(cargo, manifest, workspace, environment)

    build_started_ns = time.time_ns()
    build = _run(
        [
            cargo,
            "build",
            "--locked",
            "--manifest-path",
            str(manifest),
            "--features",
            features,
            "--message-format=json-render-diagnostics",
        ],
        cwd=repository,
        env=environment,
        capture=True,
    )
    validate_cargo_artifacts(build.stdout, workspace.target)

    maturin = workspace.venv / "bin/maturin"
    _run(
        [
            str(maturin),
            "build",
            "--locked",
            "--features",
            features,
            "--manifest-path",
            str(manifest),
            "--target-dir",
            str(workspace.target),
            "--out",
            str(workspace.wheels),
        ],
        cwd=repository,
        env=environment,
    )
    wheel_path = select_built_wheel(workspace.wheels, build_started_ns)
    native = inspect_wheel(wheel_path)
    _run(
        [
            uv,
            "pip",
            "install",
            "--python",
            str(workspace.venv / "bin/python"),
            "--reinstall",
            "--no-deps",
            str(wheel_path),
        ],
        cwd=repository,
        env=environment,
    )
    payload = _origin_probe(workspace, environment)
    validate_installed_origin(payload, workspace, native, build_started_ns)
    _run(
        [str(workspace.venv / "bin/troupe"), "--help"],
        cwd=workspace.tmp,
        env=environment,
    )

    command_environment["PATH"] = (
        f"{workspace.venv / 'bin'}{os.pathsep}{command_environment.get('PATH', '')}"
    )
    for structured in descriptor.argv:
        expanded = tuple(_expand_argument(argument, command_environment) for argument in structured)
        if expanded == ("scripts/run_diagnostic_node_gate.sh", node_id):
            continue
        command = resolve_command(
            expanded,
            workspace,
            repository,
            path=command_environment["PATH"],
        )
        _run(command, cwd=repository, env=command_environment)


def _bootstrap_run(repository: Path, node_id: str, caller: Mapping[str, str]) -> None:
    _current_checkout(repository, caller)
    uv = shutil.which("uv", path=caller.get("PATH"))
    if uv is None:
        raise GateError("native gate requires uv")
    with owned_workspace(repository, node_id) as workspace:
        environment = gate_environment(workspace, caller)
        _run(
            [uv, "sync", "--frozen", "--all-groups", "--no-install-project"],
            cwd=repository,
            env=environment,
        )
        environment["PATH"] = f"{workspace.venv / 'bin'}{os.pathsep}{environment.get('PATH', '')}"
        support = Path(__file__).resolve(strict=True)
        identities = (
            workspace.parent_device,
            workspace.parent_inode,
            workspace.root_device,
            workspace.root_inode,
        )
        _run(
            [
                str(workspace.venv / "bin/python"),
                "-B",
                str(support),
                "execute",
                node_id,
                str(workspace.root),
                *(str(value) for value in identities),
            ],
            cwd=repository,
            env=environment,
        )


def _print_origin_probe() -> None:
    import importlib

    troupe = importlib.import_module("troupe")
    runtime = importlib.import_module("troupe._runtime")
    payload = {
        "sys_executable": sys.executable,
        "console_script": shutil.which("troupe"),
        "troupe_file": troupe.__file__,
        "runtime_file": runtime.__file__,
    }
    print(json.dumps(payload, sort_keys=True, separators=(",", ":")))


def main(argv: Sequence[str] | None = None) -> int:
    arguments = list(sys.argv[1:] if argv is None else argv)
    repository = Path(__file__).resolve().parents[2]
    try:
        if len(arguments) == 2 and arguments[0] == "run":
            _bootstrap_run(repository, arguments[1], dict(os.environ))
        elif len(arguments) == 7 and arguments[0] == "execute":
            workspace = _existing_workspace(
                repository,
                Path(arguments[2]),
                tuple(int(value) for value in arguments[3:]),
            )
            _execute_gate(repository, arguments[1], workspace)
        elif arguments == ["origin-probe"]:
            _print_origin_probe()
        else:
            print("usage: diagnostic_gate.py run <node-id>", file=sys.stderr)
            return 2
    except (GateError, OSError, ValueError) as error:
        print(f"diagnostic node gate: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
