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
import secrets
import shlex
import shutil
import stat
import struct
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
from typing import Any, cast

if sys.version_info >= (3, 11):
    import tomllib
else:  # Python 3.10 maintainer environment.
    import tomli as tomllib

ROOT = Path(__file__).resolve().parents[1]
SOURCE_PACKAGE = ROOT / "src" / "troupe"
SUPPORT = ROOT / "tests" / "support"
sys.path.insert(0, str(SUPPORT))

from artifact_layout import expected_package_members, load_artifact_layout  # noqa: E402


ARTIFACT_LAYOUT = load_artifact_layout(ROOT)
BASE_ARTIFACTS = ARTIFACT_LAYOUT.base
REALIZED_PACKAGE_PYTHON = expected_package_members(ARTIFACT_LAYOUT, ".py")
REALIZED_PACKAGE_STUBS = expected_package_members(ARTIFACT_LAYOUT, ".pyi")
REALIZED_PACKAGE_MEMBERS = (
    *REALIZED_PACKAGE_PYTHON,
    *REALIZED_PACKAGE_STUBS,
    "py.typed",
)
REALIZED_WHEEL_MEMBERS = (
    *(f"troupe/{name}" for name in REALIZED_PACKAGE_MEMBERS),
    "troupe/<native>",
    *(name for name in BASE_ARTIFACTS.wheel_members if not name.startswith("troupe/")),
)
_realized_examples = set(BASE_ARTIFACTS.examples)
for _fragment in ARTIFACT_LAYOUT.fragments.values():
    if _fragment.state != "realized":
        continue
    _realized_examples.update(
        path.removeprefix("examples/")
        for path in _fragment.introduced
        if path.startswith("examples/")
    )
    _realized_examples.difference_update(
        removed.path.removeprefix("examples/")
        for removed in _fragment.removed
        if removed.path.startswith("examples/")
    )
REALIZED_EXAMPLE_FILES = tuple(sorted(_realized_examples))
EXPECTED_WRAPPER = BASE_ARTIFACTS.package_files["__init__.py"].data
EXPECTED_STUB = BASE_ARTIFACTS.package_files["__init__.pyi"].data
EXPECTED_ACT_SCHEMA_STUB_SHA256 = BASE_ARTIFACTS.package_files["act_schema.pyi"].sha256
EXPECTED_PY_TYPED = BASE_ARTIFACTS.package_files["py.typed"].data
PUBLIC_EXPORTS = list(BASE_ARTIFACTS.public_exports)
REALIZED_PUBLIC_EXPORTS = [*PUBLIC_EXPORTS, "diagnostics"]
EXPECTED_EXAMPLE_FILES = tuple(BASE_ARTIFACTS.examples)
SMOKE_TIMEOUT = 10.0
DIAGNOSTICS_SMOKE_TIMEOUT = 60.0
DIAGNOSTICS_BUILDER_IMAGE = (
    "ghcr.io/pyo3/maturin@"
    "sha256:2665227312dd1eab1c29c70a001dc8aac53155a2d048bede3b2df7f1691c8e38"
)
DIAGNOSTICS_TARGET = "x86_64-unknown-linux-gnu"
DIAGNOSTICS_EXPECTED = ROOT / "tests/fixtures/release/diagnostics-wheel-expected.json"
DIAGNOSTICS_REPORT_SCHEMA = (
    ROOT / "tests/fixtures/release/diagnostics-wheel-report-schema.json"
)
DIAGNOSTICS_SMOKE = ROOT / "tests/release/diagnostics_wheel_smoke.py"
DIAGNOSTICS_ENVIRONMENT = {
    "offline": "TROUPE_DIAGNOSTICS_WHEEL_OFFLINE",
    "smoke": "TROUPE_DIAGNOSTICS_WHEEL_SMOKE",
    "report": "TROUPE_DIAGNOSTICS_WHEEL_REPORT",
    "expected": "TROUPE_DIAGNOSTICS_WHEEL_EXPECTED",
    "report_schema": "TROUPE_DIAGNOSTICS_WHEEL_REPORT_SCHEMA",
    "builder_image": "TROUPE_DIAGNOSTICS_WHEEL_BUILDER_IMAGE",
    "target": "TROUPE_DIAGNOSTICS_WHEEL_TARGET",
}


class VerificationError(Exception):
    pass


def _pairs_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise VerificationError(f"duplicate JSON field: {key}")
        result[key] = value
    return result


def _canonical_json(value: object) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode("utf-8")


def _sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def _sha256_path(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as stream:
            while chunk := stream.read(1024 * 1024):
                digest.update(chunk)
    except OSError as error:
        raise VerificationError(f"could not hash {path}: {error}") from error
    return digest.hexdigest()


def _load_json_object(path: Path, label: str) -> tuple[dict[str, Any], bytes]:
    try:
        metadata = path.lstat()
        resolved = path.resolve(strict=True)
        payload = path.read_bytes()
    except OSError as error:
        raise VerificationError(f"could not read {label}: {error}") from error
    if (
        not stat.S_ISREG(metadata.st_mode)
        or stat.S_ISLNK(metadata.st_mode)
        or resolved != path
    ):
        raise VerificationError(f"{label} must be an exact regular file")
    try:
        value = json.loads(payload.decode("utf-8"), object_pairs_hook=_pairs_object)
    except VerificationError:
        raise
    except (UnicodeError, json.JSONDecodeError) as error:
        raise VerificationError(f"{label} is not valid UTF-8 JSON") from error
    if not isinstance(value, dict):
        raise VerificationError(f"{label} must contain an object")
    return value, payload


def _diagnostics_configuration(
    environ: Mapping[str, str],
) -> dict[str, object] | None:
    values = {
        field: environ.get(environment_name)
        for field, environment_name in DIAGNOSTICS_ENVIRONMENT.items()
    }
    present = {field for field, value in values.items() if value is not None}
    if not present:
        return None
    if present != set(DIAGNOSTICS_ENVIRONMENT):
        missing = sorted(set(DIAGNOSTICS_ENVIRONMENT) - present)
        raise VerificationError(
            f"diagnostics wheel environment is incomplete: missing {missing}"
        )
    if values["offline"] != "1":
        raise VerificationError("diagnostics wheel build must be offline")
    if values["smoke"] != "active,archive":
        raise VerificationError(
            "diagnostics wheel smoke must be exactly active,archive"
        )
    if values["builder_image"] != DIAGNOSTICS_BUILDER_IMAGE:
        raise VerificationError("diagnostics wheel builder image is not exact")
    if values["target"] != DIAGNOSTICS_TARGET:
        raise VerificationError("diagnostics wheel target is not exact")

    report = Path(str(values["report"]))
    if not report.is_absolute() or str(report) != os.path.abspath(report):
        raise VerificationError(
            "diagnostics wheel report path must be canonical and absolute"
        )
    try:
        parent_metadata = report.parent.lstat()
        parent_resolved = report.parent.resolve(strict=True)
    except OSError as error:
        raise VerificationError(
            f"diagnostics wheel report parent is unavailable: {error}"
        ) from error
    if (
        not stat.S_ISDIR(parent_metadata.st_mode)
        or stat.S_ISLNK(parent_metadata.st_mode)
        or parent_resolved != report.parent
    ):
        raise VerificationError(
            "diagnostics wheel report parent must be an exact directory"
        )
    if report.exists() or report.is_symlink():
        raise VerificationError("diagnostics wheel report already exists")

    expected = Path(str(values["expected"]))
    report_schema = Path(str(values["report_schema"]))
    try:
        expected_resolved = expected.resolve(strict=True)
        schema_resolved = report_schema.resolve(strict=True)
    except OSError as error:
        raise VerificationError(
            f"diagnostics wheel contract is unavailable: {error}"
        ) from error
    if expected_resolved != DIAGNOSTICS_EXPECTED or expected != expected_resolved:
        raise VerificationError("diagnostics wheel expected contract path is not exact")
    if schema_resolved != DIAGNOSTICS_REPORT_SCHEMA or report_schema != schema_resolved:
        raise VerificationError("diagnostics wheel report schema path is not exact")
    return {
        "offline": True,
        "smoke": ("active", "archive"),
        "report": report,
        "expected": expected,
        "report_schema": report_schema,
        "builder_image": values["builder_image"],
        "target": values["target"],
    }


def _schema_target(schema: Mapping[str, Any], reference: str) -> Mapping[str, Any]:
    if not reference.startswith("#/"):
        raise VerificationError(f"unsupported report schema reference: {reference}")
    value: Any = schema
    for raw in reference[2:].split("/"):
        key = raw.replace("~1", "/").replace("~0", "~")
        if not isinstance(value, dict) or key not in value:
            raise VerificationError(f"unresolved report schema reference: {reference}")
        value = value[key]
    if not isinstance(value, dict):
        raise VerificationError(
            f"report schema reference is not an object: {reference}"
        )
    return value


def _validate_schema_value(
    value: Any,
    rule: Mapping[str, Any],
    schema: Mapping[str, Any],
    location: str,
) -> None:
    reference = rule.get("$ref")
    if reference is not None:
        if not isinstance(reference, str):
            raise VerificationError("report schema $ref must be a string")
        _validate_schema_value(
            value, _schema_target(schema, reference), schema, location
        )
        return
    if "const" in rule and value != rule["const"]:
        raise VerificationError(f"report {location} differs from its constant")
    if "enum" in rule and value not in rule["enum"]:
        raise VerificationError(f"report {location} is outside its enum")

    expected_type = rule.get("type")
    matches = {
        "object": isinstance(value, dict),
        "array": isinstance(value, list),
        "string": isinstance(value, str),
        "integer": isinstance(value, int) and not isinstance(value, bool),
        "number": isinstance(value, (int, float)) and not isinstance(value, bool),
        "boolean": isinstance(value, bool),
        "null": value is None,
    }
    if expected_type is not None:
        types = [expected_type] if isinstance(expected_type, str) else expected_type
        if not isinstance(types, list) or not all(
            isinstance(item, str) for item in types
        ):
            raise VerificationError("report schema type declaration is invalid")
        if not any(matches.get(item, False) for item in types):
            raise VerificationError(f"report {location} has the wrong type")

    if isinstance(value, dict):
        required = rule.get("required", [])
        properties = rule.get("properties", {})
        if not isinstance(required, list) or not isinstance(properties, dict):
            raise VerificationError("report schema object declaration is invalid")
        missing = set(required) - set(value)
        if missing:
            raise VerificationError(f"report {location} is missing {sorted(missing)}")
        if rule.get("additionalProperties") is False:
            extra = set(value) - set(properties)
            if extra:
                raise VerificationError(
                    f"report {location} has extra fields {sorted(extra)}"
                )
        for key, child in value.items():
            child_rule = properties.get(key)
            if child_rule is not None:
                if not isinstance(child_rule, dict):
                    raise VerificationError(
                        "report schema property declaration is invalid"
                    )
                _validate_schema_value(child, child_rule, schema, f"{location}.{key}")

    if isinstance(value, list):
        minimum = rule.get("minItems")
        maximum = rule.get("maxItems")
        if minimum is not None and len(value) < minimum:
            raise VerificationError(f"report {location} has too few items")
        if maximum is not None and len(value) > maximum:
            raise VerificationError(f"report {location} has too many items")
        prefix = rule.get("prefixItems", [])
        if not isinstance(prefix, list):
            raise VerificationError("report schema prefixItems declaration is invalid")
        for index, child_rule in enumerate(prefix):
            if index >= len(value):
                break
            if not isinstance(child_rule, dict):
                raise VerificationError("report schema prefix item is invalid")
            _validate_schema_value(
                value[index], child_rule, schema, f"{location}[{index}]"
            )
        items = rule.get("items")
        if items is False and len(value) > len(prefix):
            raise VerificationError(f"report {location} has disallowed trailing items")
        if isinstance(items, dict):
            start = len(prefix) if prefix else 0
            for index in range(start, len(value)):
                _validate_schema_value(
                    value[index], items, schema, f"{location}[{index}]"
                )

    if isinstance(value, str):
        minimum_length = rule.get("minLength")
        if minimum_length is not None and len(value) < minimum_length:
            raise VerificationError(f"report {location} is too short")
        pattern = rule.get("pattern")
        if pattern is not None and re.search(pattern, value) is None:
            raise VerificationError(f"report {location} does not match its pattern")

    if isinstance(value, (int, float)) and not isinstance(value, bool):
        minimum_value = rule.get("minimum")
        if minimum_value is not None and value < minimum_value:
            raise VerificationError(f"report {location} is below its minimum")


def _validate_report_schema(
    report: Mapping[str, Any], schema: Mapping[str, Any]
) -> None:
    _validate_schema_value(report, schema, schema, "root")


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


def _assert_thin_package(
    names: Sequence[str],
    prefix: str,
    python_members: Sequence[str],
    stub_members: Sequence[str],
) -> None:
    relative = _relative_package_files(names, prefix)
    python_files = [name for name in relative if name.endswith(".py")]
    stub_files = [name for name in relative if name.endswith(".pyi")]
    if python_files != list(python_members):
        raise VerificationError(f"unexpected Python package files: {python_files}")
    if stub_files != list(stub_members):
        raise VerificationError(f"unexpected stub files: {stub_files}")
    if relative.count("py.typed") != 1:
        raise VerificationError("py.typed is missing or ambiguous")


def _validate_source(source_package: Path) -> dict[str, bytes]:
    try:
        repository_source = source_package.resolve(strict=True) == SOURCE_PACKAGE
        python_members = (
            REALIZED_PACKAGE_PYTHON
            if repository_source
            else BASE_ARTIFACTS.package_python_members
        )
        stub_members = (
            REALIZED_PACKAGE_STUBS
            if repository_source
            else BASE_ARTIFACTS.package_stub_members
        )
        files = [path for path in source_package.rglob("*") if path.is_file()]
        names = [path.relative_to(source_package).as_posix() for path in files]
        _assert_thin_package(names, "", python_members, stub_members)

        allowed = {*python_members, *stub_members, "py.typed"}
        for name in names:
            if name in allowed:
                continue
            if name.startswith("__pycache__/") and name.endswith(".pyc"):
                continue
            if re.fullmatch(r"_runtime(?:\.[A-Za-z0-9_]+)*\.so", name):
                continue
            raise VerificationError(f"unexpected source package file: {name}")

        payloads = {name: (source_package / name).read_bytes() for name in allowed}
        for name, snapshot in BASE_ARTIFACTS.package_files.items():
            if repository_source and ARTIFACT_LAYOUT.is_changed_after_base(
                f"src/troupe/{name}"
            ):
                continue
            if payloads[name] != snapshot.data:
                raise VerificationError(
                    f"source package member is not the approved public API: {name}"
                )
        return payloads
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
        repository_source = source_package.resolve(strict=True) == SOURCE_PACKAGE
        expected_names = (
            REALIZED_EXAMPLE_FILES if repository_source else EXPECTED_EXAMPLE_FILES
        )
        files: dict[str, bytes] = {}
        for path in examples.rglob("*"):
            if not path.is_file():
                continue
            name = path.relative_to(examples).as_posix()
            parts = PurePosixPath(name).parts
            if "__pycache__" in parts or path.suffix in (".pyc", ".pyo"):
                continue
            files[name] = path.read_bytes()
        if tuple(sorted(files)) != expected_names:
            raise VerificationError("source examples inventory is not exact")
        return files
    except VerificationError:
        raise
    except OSError as error:
        raise VerificationError(f"could not inspect source examples: {error}") from error


def _source_rust_build_inputs(source_package: Path) -> dict[str, bytes]:
    repository_root = source_package.parent.parent
    rust_root = repository_root / "rust"
    try:
        paths = [rust_root / "Cargo.toml", rust_root / "Cargo.lock"]
        paths.extend(
            path
            for path in rust_root.rglob("*.rs")
            if "target" not in path.relative_to(rust_root).parts
        )
        crates_root = rust_root / "crates"
        if crates_root.is_dir():
            paths.extend(crates_root.rglob("Cargo.toml"))
        files = {
            path.relative_to(repository_root).as_posix(): path.read_bytes()
            for path in paths
            if path.is_file()
        }
        if "rust/Cargo.toml" not in files or "rust/Cargo.lock" not in files:
            raise VerificationError("source Rust build inputs are incomplete")
        return files
    except VerificationError:
        raise
    except OSError as error:
        raise VerificationError(f"could not inspect source Rust inputs: {error}") from error


def _rust_input_matches_sdist(name: str, source: bytes, packaged: bytes) -> bool:
    if source == packaged:
        return True
    if PurePosixPath(name).name != "Cargo.toml":
        return False
    try:
        return tomllib.loads(source.decode("utf-8")) == tomllib.loads(
            packaged.decode("utf-8")
        )
    except (UnicodeError, tomllib.TOMLDecodeError):
        return False


def _validate_sdist(
    source_package: Path,
    sdist: Path,
    *,
    expected: Mapping[str, bytes] | None = None,
) -> None:
    package_payloads = dict(
        expected if expected is not None else _validate_source(source_package)
    )
    python_members = sorted(
        name for name in package_payloads if name.endswith(".py")
    )
    stub_members = sorted(name for name in package_payloads if name.endswith(".pyi"))
    source_examples = _source_examples(source_package)
    source_rust_inputs = _source_rust_build_inputs(source_package)
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
            _assert_thin_package(package_names, prefix, python_members, stub_members)
            expected_package_names = {
                f"{prefix}{name}" for name in package_payloads
            }
            if set(package_names) != expected_package_names:
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

            rust_prefix = f"{distribution_prefix}rust/"
            rust_names = {
                name
                for name in regular_names
                if name.startswith(rust_prefix)
                and (
                    name.endswith(".rs")
                    or name.endswith("/Cargo.toml")
                    or name == f"{rust_prefix}Cargo.lock"
                )
            }
            expected_rust_names = {
                f"{distribution_prefix}{name}" for name in source_rust_inputs
            }
            if rust_names != expected_rust_names:
                raise VerificationError("sdist Rust build input inventory is not exact")

            for name, payload in package_payloads.items():
                member = archive.extractfile(f"{prefix}{name}")
                if member is None or member.read() != payload:
                    raise VerificationError(
                        f"sdist package member differs from source: {name}"
                    )
            for name, data in source_examples.items():
                member = archive.extractfile(f"{examples_prefix}{name}")
                if member is None or member.read() != data:
                    raise VerificationError(
                        f"sdist example differs from source: {name}"
                    )
            for name, data in source_rust_inputs.items():
                member = archive.extractfile(f"{distribution_prefix}{name}")
                if member is None or not _rust_input_matches_sdist(
                    name,
                    data,
                    member.read(),
                ):
                    raise VerificationError(f"sdist Rust input differs from source: {name}")
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


def _validate_record(
    archive: zipfile.ZipFile,
    infos: Sequence[zipfile.ZipInfo],
    record: str,
) -> None:
    names = {info.filename for info in infos}
    try:
        rows = list(csv.reader(io.StringIO(archive.read(record).decode("utf-8"))))
    except (csv.Error, UnicodeError, KeyError) as error:
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
        except KeyError as error:
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
    expected: Mapping[str, bytes] | None = None,
) -> None:
    package_payloads = dict(
        expected if expected is not None else _validate_source(source_package)
    )
    filename_tags, filename_platforms = _parse_wheel_filename(wheel)
    if required_manylinux is not None and not (
        set(filename_platforms) & _required_manylinux_platforms(required_manylinux)
    ):
        raise VerificationError("wheel does not contain the requested manylinux policy tag")

    try:
        with zipfile.ZipFile(wheel) as archive:
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
                native_libraries[0] if name == "troupe/<native>" else name
                for name in (
                    *(f"troupe/{member}" for member in package_payloads),
                    "troupe/<native>",
                    *(
                        member
                        for member in BASE_ARTIFACTS.wheel_members
                        if not member.startswith("troupe/")
                    ),
                )
            }
            if set(names) != expected_names:
                unexpected = sorted(set(names) - expected_names)
                missing = sorted(expected_names - set(names))
                raise VerificationError(
                    f"wheel inventory differs (unexpected={unexpected}, missing={missing})"
                )

            for info in infos:
                archive.read(info)
            for name, payload in package_payloads.items():
                if archive.read(f"troupe/{name}") != payload:
                    raise VerificationError(
                        f"wheel package member differs from source: {name}"
                    )

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
    ) as error:
        raise VerificationError(f"could not validate wheel: {error}") from error


def _validate_artifacts(source_package: Path, sdist: Path, wheel: Path) -> None:
    expected = _validate_source(source_package)
    _validate_sdist(source_package, sdist, expected=expected)
    _validate_wheel(source_package, wheel, required_manylinux=None, expected=expected)


def _validate_expected_contract(expected: Mapping[str, Any]) -> None:
    fields = {
        "schema",
        "build_system",
        "builder_image",
        "manylinux",
        "target",
        "smoke_modes",
        "forbidden_tools",
        "wheel_members",
        "allowed_elf_needed",
        "exporter_before",
        "size_is_informational",
    }
    if set(expected) != fields:
        raise VerificationError(
            "diagnostics wheel expected contract fields are not exact"
        )
    if expected["schema"] != "troupe.diagnostics.wheel-expected.v1":
        raise VerificationError("diagnostics wheel expected contract version drifted")
    if expected["build_system"] != {
        "requires": ["maturin==1.14.1"],
        "build_backend": "maturin",
    }:
        raise VerificationError("diagnostics wheel build system contract drifted")
    if expected["builder_image"] != DIAGNOSTICS_BUILDER_IMAGE:
        raise VerificationError("diagnostics wheel builder image contract drifted")
    if expected["manylinux"] != "2_17":
        raise VerificationError("diagnostics wheel manylinux contract drifted")
    if expected["target"] != DIAGNOSTICS_TARGET:
        raise VerificationError("diagnostics wheel target contract drifted")
    if expected["smoke_modes"] != ["active", "archive"]:
        raise VerificationError("diagnostics wheel smoke contract drifted")
    if expected["wheel_members"] != list(REALIZED_WHEEL_MEMBERS):
        raise VerificationError("diagnostics wheel member contract drifted")
    forbidden = expected["forbidden_tools"]
    if forbidden != [
        "node",
        "nodejs",
        "npm",
        "npx",
        "protoc",
        "perfetto",
        "trace_processor_shell",
    ]:
        raise VerificationError("diagnostics wheel forbidden tool contract drifted")
    needed = expected["allowed_elf_needed"]
    if (
        not isinstance(needed, list)
        or not needed
        or not all(isinstance(item, str) and item for item in needed)
        or len(needed) != len(set(needed))
    ):
        raise VerificationError("diagnostics wheel ELF baseline is malformed")
    before = expected["exporter_before"]
    if not isinstance(before, dict) or set(before) != {
        "source_commit",
        "wheel_sha256",
        "wheel_bytes",
        "native_sha256",
        "native_bytes",
    }:
        raise VerificationError("diagnostics exporter size baseline is malformed")
    if not all(
        isinstance(before[name], str) and re.fullmatch(r"[0-9a-f]{64}", before[name])
        for name in ("wheel_sha256", "native_sha256")
    ):
        raise VerificationError("diagnostics exporter baseline hash is malformed")
    if (
        not isinstance(before["source_commit"], str)
        or re.fullmatch(r"[0-9a-f]{40}", before["source_commit"]) is None
    ):
        raise VerificationError("diagnostics exporter baseline commit is malformed")
    if any(
        not isinstance(before[name], int)
        or isinstance(before[name], bool)
        or before[name] <= 0
        for name in ("wheel_bytes", "native_bytes")
    ):
        raise VerificationError("diagnostics exporter baseline size is malformed")
    if expected["size_is_informational"] is not True:
        raise VerificationError("diagnostics exporter size must remain informational")


def _validate_build_system(sdist: Path, expected: Mapping[str, Any]) -> None:
    try:
        source_payload = (ROOT / "pyproject.toml").read_bytes()
        source = tomllib.loads(source_payload.decode("utf-8"))
        with tarfile.open(sdist, "r:*") as archive:
            members = [
                member
                for member in archive.getmembers()
                if member.isfile() and member.name.endswith("/pyproject.toml")
            ]
            if len(members) != 1:
                raise VerificationError("sdist must contain exactly one pyproject.toml")
            stream = archive.extractfile(members[0])
            if stream is None:
                raise VerificationError("sdist pyproject.toml is unreadable")
            sdist_payload = stream.read()
    except VerificationError:
        raise
    except (OSError, UnicodeError, tomllib.TOMLDecodeError, tarfile.TarError) as error:
        raise VerificationError(
            f"could not validate wheel build system: {error}"
        ) from error
    if sdist_payload != source_payload:
        raise VerificationError("sdist pyproject.toml differs from source")
    if source.get("build-system") != expected["build_system"]:
        raise VerificationError("pyproject build requirement is not exactly maturin")


def _elf_needed(data: bytes) -> list[str]:
    header_format = "<16sHHIQQQIHHHHHH"
    section_format = "<IIQQQQIIQQ"
    if len(data) < struct.calcsize(header_format):
        raise VerificationError("native module is not a complete ELF file")
    try:
        header = struct.unpack_from(header_format, data)
    except struct.error as error:
        raise VerificationError("native module ELF header is malformed") from error
    identity = header[0]
    if identity[:4] != b"\x7fELF" or identity[4:6] != b"\x02\x01":
        raise VerificationError("native module must be a little-endian ELF64 file")
    if header[2] != 62:
        raise VerificationError("native module must target ELF x86_64")
    section_offset = header[6]
    section_entry_size = header[11]
    section_count = header[12]
    if section_entry_size != struct.calcsize(section_format) or section_count == 0:
        raise VerificationError("native module ELF section table is unsupported")
    table_end = section_offset + section_entry_size * section_count
    if section_offset <= 0 or table_end > len(data):
        raise VerificationError("native module ELF section table is out of bounds")
    sections: list[tuple[int, ...]] = []
    try:
        for index in range(section_count):
            sections.append(
                struct.unpack_from(
                    section_format,
                    data,
                    section_offset + index * section_entry_size,
                )
            )
    except struct.error as error:
        raise VerificationError(
            "native module ELF section table is malformed"
        ) from error
    dynamic_sections = [section for section in sections if section[1] == 6]
    if len(dynamic_sections) != 1:
        raise VerificationError(
            "native module must contain exactly one ELF dynamic section"
        )
    dynamic = dynamic_sections[0]
    dynamic_offset, dynamic_size, string_index, entry_size = (
        dynamic[4],
        dynamic[5],
        dynamic[6],
        dynamic[9],
    )
    if entry_size != 16 or dynamic_offset + dynamic_size > len(data):
        raise VerificationError("native module ELF dynamic section is malformed")
    if string_index >= len(sections):
        raise VerificationError("native module ELF dynamic strings are missing")
    strings = sections[string_index]
    string_offset, string_size = strings[4], strings[5]
    if string_offset + string_size > len(data):
        raise VerificationError("native module ELF dynamic strings are out of bounds")
    string_data = data[string_offset : string_offset + string_size]
    needed: list[str] = []
    try:
        for offset in range(dynamic_offset, dynamic_offset + dynamic_size, entry_size):
            tag, value = struct.unpack_from("<qQ", data, offset)
            if tag == 0:
                break
            if tag != 1:
                continue
            if value >= len(string_data):
                raise VerificationError("native module ELF DT_NEEDED offset is invalid")
            end = string_data.find(b"\0", value)
            if end < 0:
                raise VerificationError("native module ELF DT_NEEDED is unterminated")
            needed.append(string_data[value:end].decode("ascii"))
    except (struct.error, UnicodeError) as error:
        raise VerificationError("native module ELF DT_NEEDED is malformed") from error
    if not needed or len(needed) != len(set(needed)):
        raise VerificationError(
            "native module ELF DT_NEEDED inventory is empty or duplicated"
        )
    return needed


def _wheel_observation(
    wheel: Path,
    expected: Mapping[str, Any],
) -> tuple[dict[str, Any], bytes]:
    try:
        with zipfile.ZipFile(wheel) as archive:
            infos = sorted(
                (info for info in archive.infolist() if not info.is_dir()),
                key=lambda info: info.filename,
            )
            members = []
            native_path: str | None = None
            native_data: bytes | None = None
            for info in infos:
                payload = archive.read(info.filename)
                members.append(
                    {
                        "path": info.filename,
                        "sha256": _sha256_bytes(payload),
                        "bytes": len(payload),
                    }
                )
                if re.fullmatch(
                    r"troupe/_runtime(?:\.[A-Za-z0-9_]+)*\.so", info.filename
                ):
                    native_path = info.filename
                    native_data = payload
    except (OSError, KeyError, zipfile.BadZipFile) as error:
        raise VerificationError(f"could not observe wheel artifact: {error}") from error
    if native_path is None or native_data is None:
        raise VerificationError("wheel observation did not find its native module")
    actual_template = [
        "troupe/<native>" if row["path"] == native_path else row["path"]
        for row in members
    ]
    if sorted(actual_template) != sorted(expected["wheel_members"]):
        raise VerificationError(
            "observed wheel manifest differs from its exact contract"
        )
    needed = _elf_needed(native_data)
    unexpected_needed = sorted(set(needed) - set(expected["allowed_elf_needed"]))
    if unexpected_needed:
        raise VerificationError(
            f"native module added ELF DT_NEEDED entries: {unexpected_needed}"
        )
    wheel_bytes = wheel.stat().st_size
    return (
        {
            "filename": wheel.name,
            "sha256": _sha256_path(wheel),
            "bytes": wheel_bytes,
            "members": members,
            "native_path": native_path,
            "native_sha256": _sha256_bytes(native_data),
            "native_bytes": len(native_data),
            "elf_needed": needed,
        },
        native_data,
    )


def _sdist_observation(sdist: Path) -> dict[str, Any]:
    try:
        with tarfile.open(sdist, "r:*") as archive:
            regular_members = sum(member.isfile() for member in archive.getmembers())
        size = sdist.stat().st_size
    except (OSError, tarfile.TarError) as error:
        raise VerificationError(f"could not observe sdist artifact: {error}") from error
    return {
        "filename": sdist.name,
        "sha256": _sha256_path(sdist),
        "bytes": size,
        "regular_members": regular_members,
    }


def _exact_mapping(value: Any, fields: set[str], label: str) -> Mapping[str, Any]:
    if not isinstance(value, dict) or set(value) != fields:
        raise VerificationError(f"{label} fields are not exact")
    return value


def _normalize_diagnostics_smoke(
    raw: Mapping[str, Any],
    expected: Mapping[str, Any],
    wheel: Mapping[str, Any],
) -> dict[str, Any]:
    _exact_mapping(
        raw,
        {
            "modes",
            "run_id",
            "installed",
            "active",
            "archive",
            "forbidden_tools",
            "production_imports",
        },
        "diagnostics wheel smoke",
    )
    if raw["modes"] != expected["smoke_modes"]:
        raise VerificationError("diagnostics wheel smoke modes drifted")
    if raw["forbidden_tools"] != expected["forbidden_tools"]:
        raise VerificationError("diagnostics wheel forbidden tool observation drifted")
    if raw["production_imports"] != 1:
        raise VerificationError("archive smoke imported the Production")
    installed = _exact_mapping(
        raw["installed"],
        {"environment", "troupe_file", "native_file", "native_bytes", "native_sha256"},
        "diagnostics wheel installed origin",
    )
    try:
        environment = Path(str(installed["environment"])).resolve(strict=True)
        for name in ("troupe_file", "native_file"):
            Path(str(installed[name])).resolve(strict=True).relative_to(environment)
    except (OSError, ValueError) as error:
        raise VerificationError(
            "diagnostics wheel smoke imported outside its child venv"
        ) from error
    if (
        installed["native_sha256"] != wheel["native_sha256"]
        or installed["native_bytes"] != wheel["native_bytes"]
    ):
        raise VerificationError("installed native module differs from the wheel member")
    active = _exact_mapping(raw["active"], {"status", "ui"}, "active wheel smoke")
    archive = _exact_mapping(
        raw["archive"],
        {"status", "ui", "trace_bytes", "trace_sha256"},
        "archive wheel smoke",
    )
    if active["status"] != "passed" or archive["status"] != "passed":
        raise VerificationError("diagnostics wheel smoke did not pass")
    return {
        "modes": list(raw["modes"]),
        "run_id": raw["run_id"],
        "installed_native_sha256": installed["native_sha256"],
        "installed_native_bytes": installed["native_bytes"],
        "active": dict(active),
        "archive": dict(archive),
        "forbidden_tools": list(raw["forbidden_tools"]),
        "production_imports": raw["production_imports"],
    }


def _diagnostics_identity() -> dict[str, str]:
    paths = {
        "actor_design_sha256": ROOT / "docs/design/actor-agent-session.md",
        "diagnostics_design_sha256": ROOT / "docs/design/production-diagnostics.md",
        "plan_sha256": ROOT / "docs/plan/production-diagnostics-implementation-plan.md",
        "validator_sha256": ROOT / "docs/plan/verify_production_diagnostics_plan.py",
        "review_record_sha256": ROOT
        / "docs/plan/production-diagnostics-plan-review-record.md",
    }
    identity = {name: _sha256_path(path) for name, path in paths.items()}
    integration = _run(
        ["git", "rev-parse", "HEAD"],
        cwd=ROOT,
        env=_build_environment(os.environ),
    ).strip()
    if re.fullmatch(r"[0-9a-f]{40}", integration) is None:
        raise VerificationError("could not bind wheel report to an integration commit")
    identity["integration_sha"] = integration
    return identity


def _assemble_diagnostics_report(
    configuration: Mapping[str, object],
    expected: Mapping[str, Any],
    expected_payload: bytes,
    schema: Mapping[str, Any],
    schema_payload: bytes,
    sdist: Path,
    wheel_before: Mapping[str, Any],
    wheel_after: Mapping[str, Any],
    smoke_raw: Mapping[str, Any],
) -> dict[str, Any]:
    if (
        wheel_before["sha256"] != wheel_after["sha256"]
        or wheel_before["bytes"] != wheel_after["bytes"]
        or wheel_before["native_sha256"] != wheel_after["native_sha256"]
        or wheel_before["native_bytes"] != wheel_after["native_bytes"]
    ):
        raise VerificationError("wheel or native module changed during packaged smoke")
    smoke = _normalize_diagnostics_smoke(smoke_raw, expected, wheel_after)
    identity = _diagnostics_identity()
    sdist_report = _sdist_observation(sdist)
    wheel_report = {
        "filename": wheel_before["filename"],
        "sha256_before_smoke": wheel_before["sha256"],
        "sha256_after_smoke": wheel_after["sha256"],
        "bytes_before_smoke": wheel_before["bytes"],
        "bytes_after_smoke": wheel_after["bytes"],
        "members": wheel_before["members"],
    }
    native_report = {
        "path": wheel_before["native_path"],
        "sha256_before_smoke": wheel_before["native_sha256"],
        "sha256_after_smoke": wheel_after["native_sha256"],
        "bytes_before_smoke": wheel_before["native_bytes"],
        "bytes_after_smoke": wheel_after["native_bytes"],
        "elf_needed": wheel_before["elf_needed"],
    }
    artifact_material = {
        "sdist": sdist_report,
        "wheel": wheel_report,
        "native": native_report,
    }
    artifacts = {
        "artifact_sha256": _sha256_bytes(_canonical_json(artifact_material)),
        **artifact_material,
    }
    before = dict(expected["exporter_before"])
    after = {
        "source_commit": identity["integration_sha"],
        "wheel_sha256": wheel_after["sha256"],
        "wheel_bytes": wheel_after["bytes"],
        "native_sha256": wheel_after["native_sha256"],
        "native_bytes": wheel_after["native_bytes"],
    }
    cache_requirements: list[str] = []
    report: dict[str, Any] = {
        "schema": "troupe.diagnostics.wheel-report.v1",
        "identity": identity,
        "contract": {
            "expected_sha256": _sha256_bytes(expected_payload),
            "report_schema_sha256": _sha256_bytes(schema_payload),
        },
        "build": {
            "offline": configuration["offline"],
            "build_system": expected["build_system"],
            "builder_image": configuration["builder_image"],
            "manylinux": expected["manylinux"],
            "target": configuration["target"],
            "smoke_modes": list(cast(tuple[str, str], configuration["smoke"])),
            "forbidden_tools": expected["forbidden_tools"],
            "wheel_builds": 1,
        },
        "cache": {
            "requirements": cache_requirements,
            "identity_sha256": _sha256_bytes(_canonical_json(cache_requirements)),
        },
        "artifacts": artifacts,
        "exporter_size": {
            "before": before,
            "after": after,
            "delta": {
                "wheel_bytes": after["wheel_bytes"] - before["wheel_bytes"],
                "native_bytes": after["native_bytes"] - before["native_bytes"],
            },
            "hard_limit": False,
        },
        "smoke": smoke,
    }
    report["result"] = {
        "status": "passed",
        "result_sha256": _sha256_bytes(_canonical_json(report)),
    }
    _validate_report_schema(report, schema)
    return report


def _publish_diagnostics_report(path: Path, report: Mapping[str, Any]) -> None:
    payload = _canonical_json(report) + b"\n"
    directory_flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0)
    no_follow = getattr(os, "O_NOFOLLOW", 0)
    try:
        directory = os.open(path.parent, directory_flags | no_follow)
    except OSError as error:
        raise VerificationError(
            f"could not open wheel report directory: {error}"
        ) from error
    staging: str | None = None
    published = False
    try:
        if path.exists() or path.is_symlink():
            raise VerificationError("diagnostics wheel report already exists")
        for _ in range(16):
            candidate = f".{path.name}.{os.getpid()}.{secrets.token_hex(8)}.tmp"
            try:
                descriptor = os.open(
                    candidate,
                    os.O_WRONLY | os.O_CREAT | os.O_EXCL | no_follow,
                    0o600,
                    dir_fd=directory,
                )
            except FileExistsError:
                continue
            staging = candidate
            break
        else:
            raise VerificationError("could not allocate wheel report staging name")
        try:
            position = 0
            while position < len(payload):
                written = os.write(descriptor, payload[position:])
                if written <= 0:
                    raise OSError("short write while publishing wheel report")
                position += written
            os.fsync(descriptor)
        finally:
            os.close(descriptor)
        os.link(
            staging,
            path.name,
            src_dir_fd=directory,
            dst_dir_fd=directory,
            follow_symlinks=False,
        )
        published = True
        os.unlink(staging, dir_fd=directory)
        staging = None
        os.fsync(directory)
    except VerificationError:
        raise
    except FileExistsError as error:
        raise VerificationError("diagnostics wheel report already exists") from error
    except OSError as error:
        raise VerificationError(
            f"could not publish diagnostics wheel report atomically: {error}"
        ) from error
    finally:
        if staging is not None and not published:
            try:
                os.unlink(staging, dir_fd=directory)
            except OSError:
                pass
        os.close(directory)


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
import sys
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
    "AgentResultError",
    "AgentResultIssue",
    "AgentResultMissingError",
    "AgentSessionBrokenError",
    "AgentSessionBusyError",
    "AgentSessionError",
    "AgentSessionStartError",
    "AgentTurnError",
    "Cue",
    "CueContextError",
    "Effect",
    "EffectContextError",
    "Production",
    "act_schema",
]
module_exports = ["act_schema"]
if hasattr(troupe, "diagnostics"):
    public_exports.append("diagnostics")
    module_exports.append("diagnostics")
public_type_exports = [name for name in public_exports if name not in module_exports]
native_exports = [name for name in public_type_exports if name != "AgentProfile"]
schema_exports = [
    "BoolValue",
    "Field",
    "Float64Value",
    "Int64Value",
    "ListValue",
    "NullableValue",
    "ObjectValue",
    "SchemaCallbackError",
    "SchemaValue",
    "StrValue",
    "ValueRejected",
]
schema_contract = (
    troupe.act_schema is runtime.act_schema
    and sys.modules.get("troupe.act_schema") is troupe.act_schema
    and troupe.act_schema.__name__ == "troupe.act_schema"
    and troupe.act_schema.__all__ == schema_exports
    and inspect.isabstract(troupe.act_schema.SchemaValue)
    and all(
        getattr(troupe.act_schema, name).__module__ == "troupe.act_schema"
        for name in schema_exports
    )
)
public_identities = all(
    getattr(troupe, name) is getattr(runtime, name)
    for name in [*native_exports, *module_exports]
)
public_modules = all(
    getattr(troupe, name).__module__ == "troupe" for name in public_type_exports
) and all(
    getattr(troupe, name).__name__ == f"troupe.{name}" for name in module_exports
)
assert troupe.Production is runtime.Production
assert troupe.Production.__module__ == "troupe"
assert troupe.__all__ == public_exports
assert public_identities
assert public_modules
assert schema_contract
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
        "_agent_test_hold_opening_backoff",
        "_agent_test_release_opening_backoff",
        "_agent_test_opening_backoff_state",
        "_agent_test_hold_configuration_ready",
        "_agent_test_release_configuration_ready",
        "_agent_test_hold_mcp_ready",
        "_agent_test_release_mcp_ready",
        "_agent_test_readiness_gate_states",
        "_agent_test_hold_turn_registration",
        "_agent_test_release_turn_registration",
        "_agent_test_hold_turn_intake",
        "_agent_test_release_turn_intake",
        "_agent_test_hold_turn_submission",
        "_agent_test_release_turn_submission",
        "_agent_test_hold_turn_response_flush",
        "_agent_test_release_turn_response_flush",
        "_agent_test_hold_turn_settlement",
        "_agent_test_release_turn_settlement",
        "_agent_test_hold_turn_terminal_delivery",
        "_agent_test_release_turn_terminal_delivery",
        "_agent_test_hold_turn_outcome",
        "_agent_test_release_turn_outcome",
        "_agent_test_turn_gate_states",
        "_agent_test_result_generation_isolation",
    )
) and not any(
    hasattr(runtime.ActorHandle, name)
    for name in (
        "_agent_state_for_test",
        "_agent_has_queued_turn_for_test",
        "_agent_fail_transport_for_test",
        "_agent_ready_for_test",
    )
) and not any(
    hasattr(runtime.Production, name)
    for name in (
        "_agent_shutdown_for_test",
        "_agent_is_shutting_down_for_test",
        "_agent_fail_result_listener_for_test",
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
    "schema_contract": schema_contract,
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
    members = {
        "troupe_smoke_dependency.py": module,
        f"{dist_info}/METADATA": metadata,
        f"{dist_info}/WHEEL": wheel_metadata,
    }
    record_lines = []
    for name, payload in members.items():
        digest = base64.urlsafe_b64encode(hashlib.sha256(payload).digest()).rstrip(b"=")
        record_lines.append(f"{name},sha256={digest.decode('ascii')},{len(payload)}\n")
    record_name = f"{dist_info}/RECORD"
    record = "".join([*record_lines, f"{record_name},,\n"]).encode("utf-8")
    try:
        with zipfile.ZipFile(wheel, "w", compression=zipfile.ZIP_DEFLATED) as archive:
            for name, payload in members.items():
                archive.writestr(name, payload)
            archive.writestr(record_name, record)
    except (OSError, ValueError, zipfile.BadZipFile) as error:
        raise VerificationError(f"could not build smoke dependency wheel: {error}") from error
    return wheel


def _validate_smoke_payload(
    child_venv: Path,
    payload: Mapping[str, object],
    *,
    diagnostics: bool = False,
) -> None:
    expected_values: dict[str, object] = {
        "production_identity": True,
        "production_module": "troupe",
        "exports": REALIZED_PUBLIC_EXPORTS if diagnostics else PUBLIC_EXPORTS,
        "public_identities": True,
        "public_modules": True,
        "schema_contract": True,
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


def _validate_smoke_tools(
    child_venv: Path,
    env: Mapping[str, str],
    *,
    diagnostics: bool = False,
) -> None:
    expected_path = (
        f"{child_venv}/bin:{os.environ['PATH']}"
        if diagnostics
        else f"{child_venv}/bin:/usr/bin:/bin"
    )
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


def _smoke_wheel(wheel: Path, workspace: Path) -> dict[str, Any] | None:
    diagnostics = _diagnostics_configuration(os.environ)
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
    env["PATH"] = (
        f"{child_venv}/bin:{os.environ['PATH']}"
        if diagnostics is not None
        else f"{child_venv}/bin:/usr/bin:/bin"
    )
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
    if diagnostics is None:
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
    _validate_smoke_payload(
        child_venv,
        payload,
        diagnostics=diagnostics is not None,
    )
    _validate_smoke_tools(child_venv, env, diagnostics=diagnostics is not None)

    _run(["troupe", "--help"], cwd=outside, env=env, forbidden_stderr="troupe:")
    diagnostics_result: dict[str, Any] | None = None
    if diagnostics is not None:
        diagnostics_workspace = workspace / "diagnostics-smoke"
        diagnostics_workspace.mkdir()
        diagnostics_output = _run(
            [
                child_python,
                str(DIAGNOSTICS_SMOKE),
                "--workspace",
                str(diagnostics_workspace),
                "--smoke",
                str(os.environ[DIAGNOSTICS_ENVIRONMENT["smoke"]]),
            ],
            cwd=outside,
            env=env,
            timeout=DIAGNOSTICS_SMOKE_TIMEOUT,
        )
        try:
            decoded = json.loads(diagnostics_output)
        except (TypeError, json.JSONDecodeError) as error:
            raise VerificationError(
                "diagnostics wheel smoke did not return valid JSON"
            ) from error
        if not isinstance(decoded, dict):
            raise VerificationError("diagnostics wheel smoke result must be an object")
        diagnostics_result = decoded
        _install_mock_agent_launcher(child_venv, workspace)

    events = workspace / "events.json"
    source_fixture = (
        ROOT / "tests" / "fixtures" / "productions" / "wheel_smoke_production"
    )
    if diagnostics is None:
        fixture = source_fixture
    else:
        fixture = workspace / "wheel-smoke-production"
        shutil.copytree(
            source_fixture,
            fixture,
            ignore=shutil.ignore_patterns(".troupe", "__pycache__", "*.pyc", "*.pyo"),
        )
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
    return diagnostics_result


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
    diagnostics = _diagnostics_configuration(os.environ)
    diagnostics_expected: dict[str, Any] | None = None
    diagnostics_expected_payload: bytes | None = None
    diagnostics_schema: dict[str, Any] | None = None
    diagnostics_schema_payload: bytes | None = None
    if diagnostics is not None:
        if (
            not arguments.release
            or arguments.manylinux != "2_17"
            or arguments.target != DIAGNOSTICS_TARGET
            or arguments.output_dir is not None
        ):
            raise VerificationError(
                "diagnostics wheel mode requires one unpublished release manylinux 2_17 x86_64 build"
            )
        diagnostics_expected, diagnostics_expected_payload = _load_json_object(
            Path(str(diagnostics["expected"])),
            "diagnostics wheel expected contract",
        )
        diagnostics_schema, diagnostics_schema_payload = _load_json_object(
            Path(str(diagnostics["report_schema"])),
            "diagnostics wheel report schema",
        )
        _validate_expected_contract(diagnostics_expected)
        if (
            os.environ.get("CARGO_NET_OFFLINE") != "true"
            or os.environ.get("PIP_NO_INDEX") != "1"
        ):
            raise VerificationError("diagnostics wheel build is not offline")
        for tool in diagnostics_expected["forbidden_tools"]:
            if shutil.which(tool) is not None:
                raise VerificationError(
                    f"forbidden tool is available during diagnostics wheel build: {tool}"
                )

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
            diagnostics_before: dict[str, Any] | None = None
            if diagnostics_expected is not None:
                _validate_build_system(sdist, diagnostics_expected)
                diagnostics_before, _ = _wheel_observation(
                    wheel,
                    diagnostics_expected,
                )
            smoke_result = _smoke_wheel(wheel, workspace)
            if diagnostics is not None:
                if (
                    diagnostics_expected is None
                    or diagnostics_expected_payload is None
                    or diagnostics_schema is None
                    or diagnostics_schema_payload is None
                    or diagnostics_before is None
                    or smoke_result is None
                ):
                    raise VerificationError("diagnostics wheel evidence is incomplete")
                diagnostics_after, _ = _wheel_observation(
                    wheel,
                    diagnostics_expected,
                )
                report = _assemble_diagnostics_report(
                    diagnostics,
                    diagnostics_expected,
                    diagnostics_expected_payload,
                    diagnostics_schema,
                    diagnostics_schema_payload,
                    sdist,
                    diagnostics_before,
                    diagnostics_after,
                    smoke_result,
                )
                _publish_diagnostics_report(
                    Path(str(diagnostics["report"])),
                    report,
                )
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
    if _diagnostics_configuration(os.environ) is not None:
        raise VerificationError("diagnostics report mode requires --build")
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
