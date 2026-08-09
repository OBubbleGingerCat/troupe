from __future__ import annotations

import argparse
import base64
import configparser
import csv
import hashlib
import io
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile
import venv
import zipfile
from collections.abc import Mapping, Sequence
from email.message import Message
from email.parser import BytesParser
from email.policy import default
from pathlib import Path, PurePosixPath
from typing import Any

from wheel.wheelfile import WheelError, WheelFile


ROOT = Path(__file__).resolve().parents[1]
SOURCE_PACKAGE = ROOT / "src" / "troupe"
EXPECTED_WRAPPER = (
    b"from dataclasses import dataclass as _dataclass\n"
    b"from os import PathLike as _PathLike\n"
    b"from typing import Literal as _Literal\n"
    b"\n"
    b"from ._runtime import Actor as Actor\n"
    b"from ._runtime import ActorHandle as ActorHandle\n"
    b"from ._runtime import AgentAuthenticationRequiredError as AgentAuthenticationRequiredError\n"
    b"from ._runtime import AgentError as AgentError\n"
    b"from ._runtime import AgentSessionError as AgentSessionError\n"
    b"from ._runtime import AgentSessionStartError as AgentSessionStartError\n"
    b"from ._runtime import Cue as Cue\n"
    b"from ._runtime import CueContextError as CueContextError\n"
    b"from ._runtime import Effect as Effect\n"
    b"from ._runtime import EffectContextError as EffectContextError\n"
    b"from ._runtime import Production as Production\n"
    b"\n"
    b"\n"
    b"@_dataclass(frozen=True, slots=True, kw_only=True)\n"
    b"class AgentProfile:\n"
    b'    agent: _Literal["codex", "claude", "kimi"]\n'
    b"    workspace: str | _PathLike[str]\n"
    b"    model: str\n"
    b"    effort: str | None\n"
    b"\n"
    b"    def __post_init__(self) -> None:\n"
    b"        if not isinstance(self.agent, str):\n"
    b'            raise TypeError("agent must be a str")\n'
    b'        if self.agent not in {"codex", "claude", "kimi"}:\n'
    b"            raise ValueError(\"agent must be one of: 'codex', 'claude', 'kimi'\")\n"
    b"        if not isinstance(self.model, str):\n"
    b'            raise TypeError("model must be a str")\n'
    b"        if not self.model:\n"
    b'            raise ValueError("model must not be empty")\n'
    b"        if self.effort is not None and not isinstance(self.effort, str):\n"
    b'            raise TypeError("effort must be a str or None")\n'
    b'        if self.effort == "":\n'
    b'            raise ValueError("effort must not be empty")\n'
    b"\n"
    b"\n"
    b"__all__ = [\n"
    b'    "Actor",\n'
    b'    "ActorHandle",\n'
    b'    "AgentAuthenticationRequiredError",\n'
    b'    "AgentError",\n'
    b'    "AgentProfile",\n'
    b'    "AgentSessionError",\n'
    b'    "AgentSessionStartError",\n'
    b'    "Cue",\n'
    b'    "CueContextError",\n'
    b'    "Effect",\n'
    b'    "EffectContextError",\n'
    b'    "Production",\n'
    b"]\n"
)
EXPECTED_STUB = (
    b"from __future__ import annotations\n"
    b"\n"
    b"from collections.abc import Mapping\n"
    b"from dataclasses import dataclass\n"
    b"from os import PathLike\n"
    b"from re import Pattern\n"
    b"from typing import Any, Literal, TypeVar, final, overload\n"
    b"from typing_extensions import disjoint_base\n"
    b"\n"
    b'_EffectT = TypeVar("_EffectT", bound="Effect")\n'
    b"\n"
    b"class AgentError(RuntimeError):\n"
    b"    code: str\n"
    b"\n"
    b"class AgentSessionError(AgentError): ...\n"
    b"\n"
    b"class AgentSessionStartError(AgentSessionError):\n"
    b"    phase: str\n"
    b"\n"
    b"class AgentAuthenticationRequiredError(AgentSessionStartError): ...\n"
    b"\n"
    b"@dataclass(frozen=True, slots=True, kw_only=True)\n"
    b"class AgentProfile:\n"
    b'    agent: Literal["codex", "claude", "kimi"]\n'
    b"    workspace: str | PathLike[str]\n"
    b"    model: str\n"
    b"    effort: str | None\n"
    b"    def __post_init__(self) -> None: ...\n"
    b"\n"
    b"@disjoint_base\n"
    b"class Actor:\n"
    b"    def __init__(self) -> None: ...\n"
    b"    @property\n"
    b"    def name(self) -> str: ...\n"
    b"    @property\n"
    b"    def production(self) -> Production: ...\n"
    b"    def make_effect(\n"
    b"        self,\n"
    b"        effect_type: type[_EffectT],\n"
    b"        *,\n"
    b"        effect_args: tuple[Any, ...],\n"
    b"        effect_kwargs: dict[str, Any],\n"
    b"    ) -> _EffectT: ...\n"
    b"    async def cued(self, cue: Cue) -> tuple[Effect, ...]: ...\n"
    b"\n"
    b"@final\n"
    b"class ActorHandle:\n"
    b"    @property\n"
    b"    def name(self) -> str: ...\n"
    b"    async def cue(self, instruction: dict[Any, Any]) -> tuple[Effect, ...]: ...\n"
    b"\n"
    b"@final\n"
    b"class Cue:\n"
    b"    @property\n"
    b"    def id(self) -> str: ...\n"
    b"    @property\n"
    b"    def instruction(self) -> Mapping[Any, Any]: ...\n"
    b"    @property\n"
    b"    def source(self) -> str: ...\n"
    b"\n"
    b"class CueContextError(RuntimeError): ...\n"
    b"\n"
    b"@disjoint_base\n"
    b"class Effect:\n"
    b"    @property\n"
    b"    def id(self) -> str: ...\n"
    b"    @property\n"
    b"    def owner(self) -> str: ...\n"
    b"\n"
    b"class EffectContextError(RuntimeError): ...\n"
    b"\n"
    b"@disjoint_base\n"
    b"class Production:\n"
    b"    def __new__(cls, args: list[str], /) -> Production: ...\n"
    b"    def cast_actor(\n"
    b"        self,\n"
    b"        actor_type: type[Actor],\n"
    b"        *,\n"
    b"        name: str,\n"
    b"        agent_profile: AgentProfile,\n"
    b"        actor_args: tuple[Any, ...],\n"
    b"        actor_kwargs: dict[str, Any],\n"
    b"    ) -> ActorHandle: ...\n"
    b"    @overload\n"
    b"    def get_actor(self, name: str) -> ActorHandle | None: ...\n"
    b"    @overload\n"
    b"    def get_actor(self, pattern: Pattern[str]) -> list[ActorHandle]: ...\n"
    b"    def get_actors(self) -> list[ActorHandle]: ...\n"
    b"    async def start(self) -> None: ...\n"
    b"    async def scene(self) -> None: ...\n"
    b"    async def stop(self) -> None: ...\n"
    b"\n"
    b"__all__ = [\n"
    b'    "Actor",\n'
    b'    "ActorHandle",\n'
    b'    "AgentAuthenticationRequiredError",\n'
    b'    "AgentError",\n'
    b'    "AgentProfile",\n'
    b'    "AgentSessionError",\n'
    b'    "AgentSessionStartError",\n'
    b'    "Cue",\n'
    b'    "CueContextError",\n'
    b'    "Effect",\n'
    b'    "EffectContextError",\n'
    b'    "Production",\n'
    b"]\n"
)
EXPECTED_PY_TYPED = b""
PUBLIC_EXPORTS = [
    "Actor",
    "ActorHandle",
    "AgentAuthenticationRequiredError",
    "AgentError",
    "AgentProfile",
    "AgentSessionError",
    "AgentSessionStartError",
    "Cue",
    "CueContextError",
    "Effect",
    "EffectContextError",
    "Production",
]
EXPECTED_EXAMPLE_FILES = (
    "README.md",
    "actor_pipeline/__init__.py",
    "actor_pipeline/production.py",
    "cancellation_cleanup/__init__.py",
    "cancellation_cleanup/production.py",
    "cooperative_workers/__init__.py",
    "cooperative_workers/production.py",
    "hello_actor/__init__.py",
    "hello_actor/production.py",
    "repeating_scenes/__init__.py",
    "repeating_scenes/production.py",
)
SMOKE_TIMEOUT = 10.0


class VerificationError(Exception):
    pass


def _parse_metadata(data: bytes) -> Message:
    return BytesParser(policy=default).parsebytes(data)


def _validate_entry_points(data: bytes) -> None:
    parser = configparser.ConfigParser(
        interpolation=None,
        strict=True,
        delimiters=("=",),
    )
    parser.optionxform = str
    try:
        parser.read_string(data.decode("utf-8"), source="entry_points.txt")
    except (configparser.Error, UnicodeError) as error:
        raise VerificationError("wheel console entry point is malformed") from error

    if parser.defaults() or parser.sections() != ["console_scripts"]:
        raise VerificationError("wheel console entry point is not exact")
    if dict(parser.items("console_scripts", raw=True)) != {
        "troupe": "troupe._runtime:main"
    }:
        raise VerificationError("wheel console entry point is not exact")


def _relative_package_files(names: Sequence[str], prefix: str) -> list[str]:
    return sorted(name.removeprefix(prefix) for name in names if name.startswith(prefix))


def _assert_thin_package(names: Sequence[str], prefix: str) -> None:
    relative = _relative_package_files(names, prefix)
    python_files = [name for name in relative if name.endswith(".py")]
    stub_files = [name for name in relative if name.endswith(".pyi")]
    if python_files != ["__init__.py"]:
        raise VerificationError(f"unexpected Python package files: {python_files}")
    if stub_files != ["__init__.pyi"]:
        raise VerificationError(f"unexpected stub files: {stub_files}")
    if relative.count("py.typed") != 1:
        raise VerificationError("py.typed is missing or ambiguous")


def _validate_source(source_package: Path) -> tuple[bytes, bytes, bytes]:
    try:
        files = [path for path in source_package.rglob("*") if path.is_file()]
        names = [path.relative_to(source_package).as_posix() for path in files]
        _assert_thin_package(names, "")

        allowed = {"__init__.py", "__init__.pyi", "py.typed"}
        for name in names:
            if name in allowed:
                continue
            if name.startswith("__pycache__/") and name.endswith(".pyc"):
                continue
            if re.fullmatch(r"_runtime(?:\.[A-Za-z0-9_]+)*\.so", name):
                continue
            raise VerificationError(f"unexpected source package file: {name}")

        wrapper = (source_package / "__init__.py").read_bytes()
        stub = (source_package / "__init__.pyi").read_bytes()
        py_typed = (source_package / "py.typed").read_bytes()
        if wrapper != EXPECTED_WRAPPER:
            raise VerificationError("source wrapper is not the approved thin wrapper")
        if stub != EXPECTED_STUB:
            raise VerificationError("source stub is not the approved public API")
        if py_typed != EXPECTED_PY_TYPED:
            raise VerificationError("source py.typed marker is not exact")
        return wrapper, stub, py_typed
    except VerificationError:
        raise
    except OSError as error:
        raise VerificationError(f"could not inspect source package: {error}") from error


def _safe_archive_name(name: str) -> bool:
    if not name or "\\" in name or name.startswith("/"):
        return False
    stripped = name.rstrip("/")
    if not stripped:
        return False
    path = PurePosixPath(stripped)
    return not path.is_absolute() and all(part not in ("", ".", "..") for part in path.parts)


def _sdist_package_prefix(names: Sequence[str]) -> str:
    matches = [
        name.removesuffix("__init__.py")
        for name in names
        if name.endswith("/src/troupe/__init__.py")
    ]
    if len(matches) != 1:
        raise VerificationError("sdist must contain one src/troupe package")
    return matches[0]


def _source_examples(source_package: Path) -> dict[str, bytes]:
    examples = source_package.parent.parent / "examples"
    try:
        files: dict[str, bytes] = {}
        for path in examples.rglob("*"):
            if not path.is_file():
                continue
            name = path.relative_to(examples).as_posix()
            parts = PurePosixPath(name).parts
            if "__pycache__" in parts or path.suffix in (".pyc", ".pyo"):
                continue
            files[name] = path.read_bytes()
        if tuple(sorted(files)) != EXPECTED_EXAMPLE_FILES:
            raise VerificationError("source examples inventory is not exact")
        return files
    except VerificationError:
        raise
    except OSError as error:
        raise VerificationError(f"could not inspect source examples: {error}") from error


def _validate_sdist(
    source_package: Path,
    sdist: Path,
    *,
    expected: tuple[bytes, bytes, bytes] | None = None,
) -> None:
    wrapper, stub, py_typed = (
        expected if expected is not None else _validate_source(source_package)
    )
    source_examples = _source_examples(source_package)
    try:
        with tarfile.open(sdist, "r:*") as archive:
            members = archive.getmembers()
            names = [member.name for member in members]
            if len(names) != len(set(names)):
                raise VerificationError("sdist contains duplicate archive members")
            if any(not _safe_archive_name(name) for name in names):
                raise VerificationError("sdist contains an unsafe archive path")
            if any(not (member.isfile() or member.isdir()) for member in members):
                raise VerificationError("sdist contains a link or special archive member")

            regular_names = [member.name for member in members if member.isfile()]
            prefix = _sdist_package_prefix(regular_names)
            package_names = [name for name in regular_names if name.startswith(prefix)]
            _assert_thin_package(package_names, prefix)
            if set(package_names) != {
                f"{prefix}__init__.py",
                f"{prefix}__init__.pyi",
                f"{prefix}py.typed",
            }:
                raise VerificationError("sdist runtime package inventory is not exact")

            distribution_prefix = prefix.removesuffix("src/troupe/")
            examples_prefix = f"{distribution_prefix}examples/"
            example_names = {
                name for name in regular_names if name.startswith(examples_prefix)
            }
            expected_example_names = {
                f"{examples_prefix}{name}" for name in source_examples
            }
            if example_names != expected_example_names:
                raise VerificationError("sdist examples inventory is not exact")

            wrapper_member = archive.extractfile(f"{prefix}__init__.py")
            stub_member = archive.extractfile(f"{prefix}__init__.pyi")
            py_typed_member = archive.extractfile(f"{prefix}py.typed")
            if wrapper_member is None or wrapper_member.read() != wrapper:
                raise VerificationError("sdist wrapper differs from source")
            if stub_member is None or stub_member.read() != stub:
                raise VerificationError("sdist stub differs from source")
            if py_typed_member is None or py_typed_member.read() != py_typed:
                raise VerificationError("sdist py.typed marker differs from source")
            for name, data in source_examples.items():
                member = archive.extractfile(f"{examples_prefix}{name}")
                if member is None or member.read() != data:
                    raise VerificationError(
                        f"sdist example differs from source: {name}"
                    )
    except VerificationError:
        raise
    except (OSError, tarfile.TarError, KeyError) as error:
        raise VerificationError(f"could not validate sdist: {error}") from error


def _expanded_filename_tags(python: str, abi: str, platform: str) -> list[str]:
    return [
        f"{python_tag}-{abi_tag}-{platform_tag}"
        for python_tag in python.split(".")
        for abi_tag in abi.split(".")
        for platform_tag in platform.split(".")
    ]


def _parse_wheel_filename(wheel: Path) -> tuple[list[str], list[str]]:
    match = re.fullmatch(
        r"troupe-0\.1\.0-(?P<python>[^-]+)-(?P<abi>[^-]+)-(?P<platform>[^-]+)\.whl",
        wheel.name,
    )
    if match is None:
        raise VerificationError("wheel filename is not troupe 0.1.0 with three tags")

    expanded = _expanded_filename_tags(
        match.group("python"),
        match.group("abi"),
        match.group("platform"),
    )
    platforms: list[str] = []
    for tag in expanded:
        python_tag, abi_tag, platform_tag = tag.split("-", maxsplit=2)
        if python_tag != "cp310" or abi_tag != "abi3":
            raise VerificationError("wheel must contain only cp310-abi3 tag tuples")
        if not (
            platform_tag == "manylinux2014_x86_64"
            or re.fullmatch(r"manylinux_[0-9]+_[0-9]+_x86_64", platform_tag)
        ):
            raise VerificationError("wheel must target Linux x86_64 glibc")
        platforms.append(platform_tag)
    return expanded, platforms


def _required_manylinux_platforms(required: str) -> set[str]:
    if re.fullmatch(r"[0-9]+_[0-9]+", required) is None:
        raise VerificationError(f"invalid required manylinux policy: {required}")
    accepted = {f"manylinux_{required}_x86_64"}
    if required == "2_17":
        accepted.add("manylinux2014_x86_64")
    return accepted


def _validate_record(archive: WheelFile, infos: Sequence[zipfile.ZipInfo], record: str) -> None:
    names = {info.filename for info in infos}
    try:
        rows = list(csv.reader(io.StringIO(archive.read(record).decode("utf-8"))))
    except (csv.Error, UnicodeError, KeyError, WheelError) as error:
        raise VerificationError(f"could not read wheel RECORD: {error}") from error

    if any(len(row) != 3 for row in rows):
        raise VerificationError("every RECORD row must contain exactly three columns")
    paths = [row[0] for row in rows]
    if len(paths) != len(set(paths)):
        raise VerificationError("RECORD contains duplicate rows")
    if set(paths) != names:
        raise VerificationError("RECORD members do not exactly match the wheel")

    for path, encoded_hash, encoded_size in rows:
        if path == record:
            if encoded_hash or encoded_size:
                raise VerificationError("the RECORD self row must have empty hash and size")
            continue
        try:
            data = archive.read(path)
        except (KeyError, WheelError) as error:
            raise VerificationError(f"could not read recorded wheel member: {path}") from error
        digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=")
        if encoded_hash != f"sha256={digest.decode('ascii')}":
            raise VerificationError(f"RECORD hash mismatch for {path}")
        if encoded_size != str(len(data)):
            raise VerificationError(f"RECORD size mismatch for {path}")


def _validate_wheel(
    source_package: Path,
    wheel: Path,
    *,
    required_manylinux: str | None,
    expected: tuple[bytes, bytes, bytes] | None = None,
) -> None:
    wrapper, stub, py_typed = (
        expected if expected is not None else _validate_source(source_package)
    )
    filename_tags, filename_platforms = _parse_wheel_filename(wheel)
    if required_manylinux is not None and not (
        set(filename_platforms) & _required_manylinux_platforms(required_manylinux)
    ):
        raise VerificationError("wheel does not contain the requested manylinux policy tag")

    try:
        with WheelFile(wheel) as archive:
            infos = [info for info in archive.infolist() if not info.is_dir()]
            names = [info.filename for info in infos]
            if len(names) != len(set(names)):
                raise VerificationError("wheel contains duplicate archive members")
            if any(not _safe_archive_name(info.filename) for info in archive.infolist()):
                raise VerificationError("wheel contains an unsafe archive path")

            native_libraries = [
                name
                for name in names
                if PurePosixPath(name).parent == PurePosixPath("troupe")
                and name.endswith(".so")
            ]
            if len(native_libraries) != 1 or re.fullmatch(
                r"troupe/_runtime(?:\.[A-Za-z0-9_]+)*\.so", native_libraries[0]
            ) is None:
                raise VerificationError("wheel must contain exactly one native runtime module")

            dist_info = "troupe-0.1.0.dist-info"
            record = f"{dist_info}/RECORD"
            expected_names = {
                "troupe/__init__.py",
                "troupe/__init__.pyi",
                "troupe/py.typed",
                native_libraries[0],
                f"{dist_info}/METADATA",
                f"{dist_info}/WHEEL",
                f"{dist_info}/entry_points.txt",
                record,
            }
            if set(names) != expected_names:
                unexpected = sorted(set(names) - expected_names)
                missing = sorted(expected_names - set(names))
                raise VerificationError(
                    f"wheel inventory differs (unexpected={unexpected}, missing={missing})"
                )

            for info in infos:
                archive.read(info)
            if archive.read("troupe/__init__.py") != wrapper:
                raise VerificationError("wheel wrapper differs from source")
            if archive.read("troupe/__init__.pyi") != stub:
                raise VerificationError("wheel stub differs from source")
            if archive.read("troupe/py.typed") != py_typed:
                raise VerificationError("wheel py.typed marker differs from source")

            metadata = _parse_metadata(archive.read(f"{dist_info}/METADATA"))
            if metadata.get("Name") != "troupe":
                raise VerificationError("wheel has the wrong project name")
            if metadata.get("Version") != "0.1.0":
                raise VerificationError("wheel has the wrong project version")
            if metadata.get("Requires-Python") != ">=3.10":
                raise VerificationError("wheel has the wrong Requires-Python value")
            if metadata.get_all("Requires-Dist"):
                raise VerificationError("wheel must not declare runtime dependencies")

            wheel_metadata = _parse_metadata(archive.read(f"{dist_info}/WHEEL"))
            if wheel_metadata.get("Wheel-Version") != "1.0":
                raise VerificationError("wheel must use Wheel-Version 1.0")
            if wheel_metadata.get("Root-Is-Purelib") != "false":
                raise VerificationError("native wheel must not be purelib")
            wheel_tags = wheel_metadata.get_all("Tag") or []
            if len(wheel_tags) != len(set(wheel_tags)):
                raise VerificationError("WHEEL contains duplicate Tag fields")
            if set(wheel_tags) != set(filename_tags):
                raise VerificationError("wheel filename and WHEEL Tag set differ")

            _validate_entry_points(archive.read(f"{dist_info}/entry_points.txt"))
            _validate_record(archive, infos, record)
    except VerificationError:
        raise
    except (
        OSError,
        ValueError,
        zipfile.BadZipFile,
        KeyError,
        UnicodeError,
        WheelError,
    ) as error:
        raise VerificationError(f"could not validate wheel: {error}") from error


def _validate_artifacts(source_package: Path, sdist: Path, wheel: Path) -> None:
    expected = _validate_source(source_package)
    _validate_sdist(source_package, sdist, expected=expected)
    _validate_wheel(source_package, wheel, required_manylinux=None, expected=expected)


def _maturin_command(
    output: Path,
    release: bool,
    target: str | None,
    manylinux: str | None,
) -> list[str]:
    command = [
        "maturin",
        "build",
        "--sdist",
        "--locked",
        "--manifest-path",
        "rust/Cargo.toml",
        "--out",
        str(output),
    ]
    if release:
        command.append("--release")
    if target is not None:
        command.extend(["--target", target])
    if manylinux is not None:
        command.extend(["--manylinux", manylinux])
    return command


def _build_environment(environ: Mapping[str, str]) -> dict[str, str]:
    result = dict(environ)
    result.pop("CONDA_PREFIX", None)
    return result


def _smoke_environment(environ: Mapping[str, str]) -> dict[str, str]:
    result = dict(environ)
    for name in ("CONDA_PREFIX", "PYTHONPATH", "PYTHONHOME", "VIRTUAL_ENV"):
        result.pop(name, None)
    result["PYTHONDONTWRITEBYTECODE"] = "1"
    return result


def _validate_installed_paths(
    child_venv: Path,
    payload: Mapping[str, object],
) -> None:
    root = child_venv.resolve()
    for name in ("troupe_file", "runtime_file", "dependency_file"):
        try:
            value = payload[name]
            if not isinstance(value, str):
                raise TypeError(f"{name} is not a string")
            Path(value).resolve(strict=True).relative_to(root)
        except (KeyError, OSError, TypeError, ValueError) as error:
            raise VerificationError(f"{name} was imported outside the child venv") from error


def _only_artifact(output: Path, pattern: str) -> Path:
    matches = list(output.glob(pattern))
    if len(matches) != 1:
        raise VerificationError(f"expected one {pattern} artifact, found {len(matches)}")
    return matches[0]


def _run(
    command: list[str],
    *,
    cwd: Path,
    env: Mapping[str, str],
    forbidden_stderr: str | None = None,
    timeout: float | None = None,
) -> str:
    options: dict[str, Any] = {
        "cwd": cwd,
        "env": dict(env),
        "check": False,
        "capture_output": True,
        "text": True,
    }
    if timeout is not None:
        options["timeout"] = timeout
    try:
        completed = subprocess.run(command, **options)
    except subprocess.TimeoutExpired as error:
        raise VerificationError(
            f"command timed out after {timeout} seconds: {command[0]}"
        ) from error
    except OSError as error:
        raise VerificationError(f"could not execute {command[0]}: {error}") from error
    if completed.returncode != 0:
        output = completed.stdout + completed.stderr
        raise VerificationError(f"command failed ({completed.returncode}): {output.strip()}")
    if forbidden_stderr is not None and forbidden_stderr in completed.stderr:
        raise VerificationError(f"command emitted forbidden stderr: {completed.stderr.strip()}")
    return completed.stdout


SMOKE = r'''
import asyncio
import dataclasses
import importlib.metadata
import inspect
import json
import sysconfig

import troupe
import troupe._runtime as runtime
import troupe_smoke_dependency

public_exports = [
    "Actor",
    "ActorHandle",
    "AgentAuthenticationRequiredError",
    "AgentError",
    "AgentProfile",
    "AgentSessionError",
    "AgentSessionStartError",
    "Cue",
    "CueContextError",
    "Effect",
    "EffectContextError",
    "Production",
]
native_exports = [name for name in public_exports if name != "AgentProfile"]
public_identities = all(
    getattr(troupe, name) is getattr(runtime, name) for name in native_exports
)
public_modules = all(getattr(troupe, name).__module__ == "troupe" for name in public_exports)
assert troupe.Production is runtime.Production
assert troupe.Production.__module__ == "troupe"
assert troupe.__all__ == public_exports
assert public_identities
assert public_modules
assert not hasattr(runtime, "AgentProfile")
assert dataclasses.is_dataclass(troupe.AgentProfile)
assert tuple(field.name for field in dataclasses.fields(troupe.AgentProfile)) == (
    "agent",
    "workspace",
    "model",
    "effort",
)
assert troupe.AgentProfile.__match_args__ == ()
profile = troupe.AgentProfile(
    agent="codex", workspace=".", model="gpt-5.6-sol", effort="max"
)
assert profile == troupe.AgentProfile(
    agent="codex", workspace=".", model="gpt-5.6-sol", effort="max"
)
assert hash(profile) == hash(
    troupe.AgentProfile(
        agent="codex", workspace=".", model="gpt-5.6-sol", effort="max"
    )
)
try:
    profile.model = "other"
except dataclasses.FrozenInstanceError:
    pass
else:
    raise AssertionError("AgentProfile is mutable")
agent_test_support_absent = not any(
    hasattr(runtime, name)
    for name in (
        "_agent_launch_specs_for_test",
        "_agent_test_set_launch",
        "_agent_test_reset_launch",
        "_agent_test_hold_opening",
        "_agent_test_release_opening",
        "_agent_test_hold_configuration_ready",
        "_agent_test_release_configuration_ready",
        "_agent_test_hold_mcp_ready",
        "_agent_test_release_mcp_ready",
        "_agent_test_readiness_gate_states",
        "_agent_test_result_generation_isolation",
    )
) and not any(
    hasattr(runtime.ActorHandle, name)
    for name in (
        "_agent_state_for_test",
        "_agent_ready_for_test",
    )
) and not any(
    hasattr(runtime.Production, name)
    for name in (
        "_agent_shutdown_for_test",
        "_agent_is_shutting_down_for_test",
    )
)
assert agent_test_support_absent
assert sysconfig.get_config_var("Py_GIL_DISABLED") != 1

native_construction_gates = True
for native_type in (troupe.Actor, troupe.ActorHandle, troupe.Cue, troupe.Effect):
    try:
        native_type()
    except TypeError:
        pass
    else:
        native_construction_gates = False
assert native_construction_gates

production_type = troupe.Production
for args in ([], ["--value", "1"], ["\udcff"]):
    assert isinstance(production_type(args), production_type)
for call in (
    lambda: production_type(),
    lambda: production_type([], []),
    lambda: production_type(args=[]),
    lambda: production_type(()),
    lambda: production_type("value"),
    lambda: production_type([1]),
):
    try:
        call()
    except TypeError:
        pass
    else:
        raise AssertionError("invalid constructor arguments were accepted")

base = production_type([])
assert not hasattr(base, "args")

class CustomProduction(production_type):
    def __init__(self, args):
        self.received = args

    async def scene(self):
        return None

async def exercise():
    start = base.start()
    scene = base.scene()
    stop = base.stop()
    assert inspect.isawaitable(start)
    assert inspect.isawaitable(scene)
    assert inspect.isawaitable(stop)
    assert await start is None
    try:
        await scene
    except NotImplementedError as error:
        assert str(error) == "Production.scene() is not implemented"
    else:
        raise AssertionError("base scene did not fail")
    assert await stop is None

    args = ["--custom"]
    custom = CustomProduction(args)
    assert type(custom) is CustomProduction
    assert custom.received is args
    assert await custom.start() is None
    assert await custom.scene() is None
    assert await custom.stop() is None

asyncio.run(exercise())
entries = [
    [entry.name, entry.value]
    for entry in importlib.metadata.entry_points(group="console_scripts")
    if entry.name == "troupe"
]
assert entries == [["troupe", "troupe._runtime:main"]]
assert troupe_smoke_dependency.VALUE == "dependency-ok"
print(json.dumps({
    "troupe_file": troupe.__file__,
    "runtime_file": runtime.__file__,
    "dependency_file": troupe_smoke_dependency.__file__,
    "production_identity": troupe.Production is runtime.Production,
    "production_module": troupe.Production.__module__,
    "exports": troupe.__all__,
    "public_identities": public_identities,
    "public_modules": public_modules,
    "agent_test_support_absent": agent_test_support_absent,
    "native_construction_gates": native_construction_gates,
    "gil_disabled": sysconfig.get_config_var("Py_GIL_DISABLED") == 1,
    "surrogate_constructor": True,
    "default_hooks": True,
    "subclass_override": True,
    "entry_points": entries,
}))
'''


def _build_dependency_wheel(workspace: Path) -> Path:
    wheel = workspace / "troupe_smoke_dependency-1.0.0-py3-none-any.whl"
    dist_info = "troupe_smoke_dependency-1.0.0.dist-info"
    module = (ROOT / "tests" / "fixtures" / "wheel_smoke_dependency.py").read_bytes()
    metadata = (
        b"Metadata-Version: 2.1\n"
        b"Name: troupe-smoke-dependency\n"
        b"Version: 1.0.0\n"
    )
    wheel_metadata = (
        b"Wheel-Version: 1.0\n"
        b"Generator: troupe-wheel-verifier\n"
        b"Root-Is-Purelib: true\n"
        b"Tag: py3-none-any\n"
    )
    try:
        with WheelFile(wheel, "w") as archive:
            archive.writestr("troupe_smoke_dependency.py", module)
            archive.writestr(f"{dist_info}/METADATA", metadata)
            archive.writestr(f"{dist_info}/WHEEL", wheel_metadata)
    except (OSError, WheelError, zipfile.BadZipFile) as error:
        raise VerificationError(f"could not build smoke dependency wheel: {error}") from error
    return wheel


def _validate_smoke_payload(child_venv: Path, payload: Mapping[str, object]) -> None:
    expected_values: dict[str, object] = {
        "production_identity": True,
        "production_module": "troupe",
        "exports": PUBLIC_EXPORTS,
        "public_identities": True,
        "public_modules": True,
        "agent_test_support_absent": True,
        "native_construction_gates": True,
        "gil_disabled": False,
        "surrogate_constructor": True,
        "default_hooks": True,
        "subclass_override": True,
        "entry_points": [["troupe", "troupe._runtime:main"]],
    }
    expected_keys = {
        "troupe_file",
        "runtime_file",
        "dependency_file",
        *expected_values,
    }
    if set(payload) != expected_keys:
        raise VerificationError("wheel smoke payload fields are not exact")
    for name, value in expected_values.items():
        if payload.get(name) != value:
            raise VerificationError(f"wheel smoke reported an invalid {name}")
    _validate_installed_paths(child_venv, payload)


def _validate_smoke_tools(child_venv: Path, env: Mapping[str, str]) -> None:
    expected_path = f"{child_venv}/bin:/usr/bin:/bin"
    if env.get("PATH") != expected_path:
        raise VerificationError("wheel smoke PATH is not isolated")
    if shutil.which("uv", path=expected_path) is not None:
        raise VerificationError("uv is visible inside the wheel smoke environment")
    if shutil.which("troupe", path=expected_path) != str(child_venv / "bin" / "troupe"):
        raise VerificationError("wheel smoke did not resolve the child venv troupe command")


def _install_mock_agent_launcher(child_venv: Path, workspace: Path) -> None:
    mock_agent = ROOT / "tests" / "support" / "mock_acp_agent.py"
    child_python = child_venv / "bin" / "python"
    launcher = child_venv / "bin" / "npx"
    events = workspace / "agent-events.jsonl"
    if not mock_agent.is_file() or not child_python.is_file():
        raise VerificationError("wheel smoke mock agent inputs are unavailable")
    command = " ".join(
        shlex.quote(str(value))
        for value in (
            child_python,
            mock_agent,
            "--events",
            events,
            "--scenario",
            "ready",
        )
    )
    try:
        launcher.write_text(f"#!/bin/sh\nexec {command}\n", encoding="utf-8")
        launcher.chmod(0o755)
    except OSError as error:
        raise VerificationError("could not install wheel smoke mock launcher") from error


def _validate_smoke_events(path: Path, raw_args: list[str]) -> None:
    try:
        actual = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise VerificationError("wheel smoke did not write a valid event log") from error

    try:
        if not isinstance(actual, list) or len(actual) != 6:
            raise TypeError("event inventory is not exact")
        actor = actual[3][1]
        cancellation = actual[4][1]
        if not isinstance(actor, dict) or not isinstance(cancellation, dict):
            raise TypeError("event payloads must be objects")
        root_cue = actor["root_cue"]
        if not isinstance(root_cue, dict):
            raise TypeError("root cue must be an object")
        scene = root_cue["source"]
        if not isinstance(scene, str) or re.fullmatch(
            r"scene-[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-"
            r"[89ab][0-9a-f]{3}-[0-9a-f]{12}",
            scene,
        ) is None:
            raise TypeError("root cue source is not a scene UUID")
        threads = actor["threads"]
        if (
            not isinstance(threads, list)
            or len(threads) != 8
            or any(type(thread_id) is not int for thread_id in threads)
            or len(set(threads)) != 1
        ):
            raise TypeError("thread observations are not exact")
        downstream_id = f"{scene}-cue1"
        effect_id = f"{downstream_id}-effect0"
        expected = [
            ["args", raw_args],
            ["start"],
            ["scene", "dependency-ok", "module-ok", "resource-ok"],
            [
                "actor-round-trip",
                {
                    "constructors": [
                        ["router", "router", True],
                        ["worker", "worker", True],
                    ],
                    "queries": {
                        "exact": "router",
                        "pattern": ["router", "worker"],
                    },
                    "root_cue": {"id": f"{scene}-cue0", "source": scene},
                    "downstream_cue": {
                        "id": downstream_id,
                        "source": "router",
                    },
                    "effect": {
                        "id": effect_id,
                        "owner": "worker",
                        "value": "mutated",
                    },
                    "result": {
                        "type": "tuple",
                        "items": [[effect_id, "worker", "mutated"]],
                    },
                    "threads": [threads[0]] * 8,
                },
            ],
            [
                "cancellation",
                {
                    "admitted_snapshot": "before-release",
                    "pre_release": {
                        "caller_done": False,
                        "successor_done": False,
                        "successor_entered": False,
                    },
                    "other_actor_result": [],
                    "completion_saw_release": {
                        "caller": True,
                        "successor": True,
                    },
                    "caller_outcome": "CancelledError",
                    "successor_result": [],
                },
            ],
            ["stop"],
        ]
    except (IndexError, KeyError, TypeError) as error:
        raise VerificationError(
            "wheel smoke event log differs from the actor contract"
        ) from error
    if actual != expected:
        raise VerificationError("wheel smoke event log differs from the actor contract")


def _validate_mock_agent_cleanup(path: Path) -> None:
    if not path.exists():
        raise VerificationError("wheel smoke did not start the mock agent")
    try:
        rows = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]
        pids = {
            row["pid"]
            for row in rows
            if isinstance(row, dict) and row.get("event") == "process_started"
        }
        if any(type(pid) is not int or pid <= 0 for pid in pids):
            raise TypeError("invalid mock agent process id")
        if len(pids) != 2:
            raise TypeError("wheel smoke did not start exactly one process per Actor")
        for pid in pids:
            process_rows = [row for row in rows if row.get("pid") == pid]
            events = [row.get("event") for row in process_rows]
            if events.count("process_started") != 1:
                raise TypeError("mock agent process inventory is invalid")
            if events.count("session_new_received") != 1:
                raise TypeError("mock agent did not receive exactly one session/new")
            if events.count("mcp_tools_list") != 1:
                raise TypeError("mock agent did not discover the result tool")
            configured = [
                row.get("config_id")
                for row in process_rows
                if row.get("event") == "config_applied"
            ]
            if configured != ["mode", "model"]:
                raise TypeError("mock agent configuration sequence is invalid")
    except (KeyError, OSError, TypeError, UnicodeError, json.JSONDecodeError) as error:
        raise VerificationError("wheel smoke mock agent log is malformed") from error
    for pid in pids:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            continue
        except OSError as error:
            raise VerificationError("could not inspect wheel smoke mock agent") from error
        raise VerificationError("wheel smoke left a mock agent process running")


def _smoke_wheel(wheel: Path, workspace: Path) -> None:
    child_venv = workspace / "child-venv"
    outside = workspace / "outside-repository"
    outside.mkdir()
    builder = venv.EnvBuilder(with_pip=True)
    original_base_executable = sys._base_executable
    try:
        resolved_base_executable = str(
            Path(original_base_executable).resolve(strict=True)
        )
        try:
            sys._base_executable = resolved_base_executable
            builder.create(child_venv)
        finally:
            sys._base_executable = original_base_executable
    except OSError as error:
        raise VerificationError(f"could not create child venv: {error}") from error

    dependency = _build_dependency_wheel(workspace)
    child_python = str(child_venv / "bin" / "python")
    env = _smoke_environment(os.environ)
    env["PATH"] = f"{child_venv}/bin:/usr/bin:/bin"
    _run(
        [
            child_python,
            "-m",
            "pip",
            "install",
            "--no-index",
            "--no-deps",
            str(wheel.resolve()),
            str(dependency.resolve()),
        ],
        cwd=outside,
        env=env,
    )
    _run([child_python, "-m", "pip", "check"], cwd=outside, env=env)
    _install_mock_agent_launcher(child_venv, workspace)
    output = _run(
        [child_python, "-c", SMOKE],
        cwd=outside,
        env=env,
        timeout=SMOKE_TIMEOUT,
    )
    try:
        payload = json.loads(output)
    except (TypeError, json.JSONDecodeError) as error:
        raise VerificationError("wheel smoke did not return valid JSON metadata") from error
    if not isinstance(payload, dict):
        raise VerificationError("wheel smoke metadata must be an object")
    _validate_smoke_payload(child_venv, payload)
    _validate_smoke_tools(child_venv, env)

    _run(["troupe", "--help"], cwd=outside, env=env, forbidden_stderr="troupe:")
    events = workspace / "events.json"
    fixture = ROOT / "tests" / "fixtures" / "productions" / "wheel_smoke_production"
    raw_args = ["--events", str(events), "--value", "7", "input.txt"]
    _run(
        ["troupe", "--production", str(fixture), "--", *raw_args],
        cwd=outside,
        env=env,
        forbidden_stderr="troupe:",
        timeout=SMOKE_TIMEOUT,
    )
    _validate_smoke_events(events, raw_args)
    _validate_mock_agent_cleanup(workspace / "agent-events.jsonl")


def _write_sha256(wheel: Path, checksum: Path) -> None:
    digest = hashlib.sha256(wheel.read_bytes()).hexdigest()
    checksum.write_bytes(f"{digest}  {wheel.name}\n".encode("ascii"))


def _validate_sha256(wheel: Path, checksum: Path) -> None:
    try:
        expected = f"{hashlib.sha256(wheel.read_bytes()).hexdigest()}  {wheel.name}\n".encode(
            "ascii"
        )
        actual = checksum.read_bytes()
    except (OSError, UnicodeError) as error:
        raise VerificationError(f"could not read wheel checksum: {error}") from error
    if actual != expected:
        raise VerificationError("SHA256SUMS is not exact or does not match the wheel")


def _discard_staging(staging: Path | None) -> None:
    if staging is not None and staging.exists():
        shutil.rmtree(staging, ignore_errors=True)


def _stage_publication(wheel: Path, output: Path) -> Path:
    if output.exists():
        raise VerificationError(f"output directory already exists: {output}")
    try:
        output.parent.mkdir(parents=True, exist_ok=True)
        staging = Path(tempfile.mkdtemp(prefix=f".{output.name}-", dir=output.parent))
    except OSError as error:
        raise VerificationError(f"could not create publication staging: {error}") from error
    try:
        staged_wheel = staging / wheel.name
        shutil.copy2(wheel, staged_wheel)
        checksum = staging / "SHA256SUMS"
        _write_sha256(staged_wheel, checksum)
        _validate_sha256(staged_wheel, checksum)
        return staging
    except BaseException as error:
        _discard_staging(staging)
        if not isinstance(error, Exception):
            raise
        if isinstance(error, VerificationError):
            raise
        raise VerificationError(f"could not stage wheel publication: {error}") from error


def _commit_publication(staging: Path, output: Path) -> None:
    try:
        if output.exists():
            raise VerificationError(f"output directory already exists: {output}")
        (staged_wheel,) = staging.glob("*.whl")
        checksum = staging / "SHA256SUMS"
        _validate_sha256(staged_wheel, checksum)

        parent = output.parent.stat()
        staged_wheel.chmod(0o644)
        checksum.chmod(0o644)
        staging.chmod(0o755)
        os.chown(staged_wheel, parent.st_uid, parent.st_gid)
        os.chown(checksum, parent.st_uid, parent.st_gid)
        os.chown(staging, parent.st_uid, parent.st_gid)
        os.rename(staging, output)
    except BaseException as error:
        _discard_staging(staging)
        if not isinstance(error, Exception):
            raise
        if isinstance(error, VerificationError):
            raise
        raise VerificationError(f"could not publish wheel atomically: {error}") from error


class _ModeParser(argparse.ArgumentParser):
    def parse_args(
        self,
        args: Sequence[str] | None = None,
        namespace: argparse.Namespace | None = None,
    ) -> argparse.Namespace:
        parsed = super().parse_args(args, namespace)
        if parsed.build:
            if parsed.sha256_file is not None:
                self.error("--sha256-file is valid only with --wheel")
        elif parsed.sha256_file is None:
            self.error("--wheel requires --sha256-file")
        elif parsed.release or parsed.target or parsed.manylinux or parsed.output_dir:
            self.error("build options are valid only with --build")
        return parsed


def _parser() -> argparse.ArgumentParser:
    parser = _ModeParser()
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--build", action="store_true")
    mode.add_argument("--wheel", type=Path)
    parser.add_argument("--sha256-file", type=Path)
    parser.add_argument("--release", action="store_true")
    parser.add_argument("--target")
    parser.add_argument("--manylinux")
    parser.add_argument("--output-dir", type=Path)
    return parser


def _build_mode(arguments: argparse.Namespace) -> None:
    output: Path | None = arguments.output_dir
    if output is not None and output.exists():
        raise VerificationError(f"output directory already exists: {output}")

    staging: Path | None = None
    try:
        with tempfile.TemporaryDirectory(prefix="troupe-wheel-") as temporary:
            workspace = Path(temporary)
            artifacts = workspace / "artifacts"
            _run(
                _maturin_command(
                    artifacts,
                    arguments.release,
                    arguments.target,
                    arguments.manylinux,
                ),
                cwd=ROOT,
                env=_build_environment(os.environ),
            )
            sdist = _only_artifact(artifacts, "*.tar.gz")
            wheel = _only_artifact(artifacts, "*.whl")
            expected = _validate_source(SOURCE_PACKAGE)
            _validate_sdist(SOURCE_PACKAGE, sdist, expected=expected)
            _validate_wheel(
                SOURCE_PACKAGE,
                wheel,
                required_manylinux=arguments.manylinux,
                expected=expected,
            )
            _smoke_wheel(wheel, workspace)
            if output is not None:
                staging = _stage_publication(wheel, output)
    except BaseException:
        _discard_staging(staging)
        raise

    if output is not None:
        if staging is None:
            raise VerificationError("wheel publication was not staged")
        _commit_publication(staging, output)


def _wheel_mode(arguments: argparse.Namespace) -> None:
    wheel: Path = arguments.wheel
    checksum: Path = arguments.sha256_file
    _validate_sha256(wheel, checksum)
    with tempfile.TemporaryDirectory(prefix="troupe-wheel-") as temporary:
        expected = _validate_source(SOURCE_PACKAGE)
        _validate_wheel(
            SOURCE_PACKAGE,
            wheel,
            required_manylinux=None,
            expected=expected,
        )
        _smoke_wheel(wheel, Path(temporary))


def main(argv: Sequence[str] | None = None) -> int:
    arguments = _parser().parse_args(argv)
    try:
        if arguments.build:
            _build_mode(arguments)
        else:
            _wheel_mode(arguments)
    except (VerificationError, OSError) as error:
        print(f"troupe artifact verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
