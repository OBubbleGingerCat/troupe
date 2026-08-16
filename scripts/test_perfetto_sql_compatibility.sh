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

import argparse
import hashlib
import json
import os
import platform as host_platform
import re
import stat
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path, PurePosixPath
from typing import Any, Final


RELEASE_TAG: Final = "v57.2"
RELEASE_COMMIT: Final = "da1d152cff27890903d158fe96751de3aab883cc"
RPC_API_VERSION: Final = 14
VERSION_STDOUT: Final = (
    b"Perfetto v57.2-da1d152cf (da1d152cff27890903d158fe96751de3aab883cc)\n"
    b"Trace Processor RPC API version: 14\n"
)
IDENTITY_NAME: Final = ".troupe-perfetto-cache.json"
OFFICIAL_MANIFEST_SHA256: Final = (
    "8223d7de5b7afd0b59e50813fbdef2c00271bd8eb0cb4515c2c16e3b3fb68a50"
)
TOOL_ARCHIVE_SHA256: Final = {
    "linux-amd64": "a5354a4a133cc629bb398da53c95515e5a49d4bd96edfebe1ebc3221c85d936f",
    "linux-arm": "1ba33c50a29fa1b9f9472747ee00b274e9c4f28883ce42de86debf4c48bdb3e4",
    "linux-arm64": "1a15f63477c03984f8117929484cf599ad8410e0b638f23d2ac1b023679ca10e",
    "mac-amd64": "8d56edbd061a947ec4a63b2b1b396a9beeccac2bc7b0c33e10240cc1d6bce32f",
    "mac-arm64": "f0f282ef199a2942ee5286856cd57260b11e93f95fdd80e3ffafe2f56ed936de",
    "windows-amd64": "0d47a31f9058cae5442baeab1ffce3f3f75e176f4f7cd8fedb1a29a51955975e",
}
FIXTURE_NAMES: Final = (
    "active-watermark",
    "archive-watermark",
    "empty",
    "multi-cue",
    "nested",
    "numeric-boundary",
    "open",
    "overlap",
    "repeated-dump",
)
SHA256_PATTERN: Final = re.compile(r"[0-9a-f]{64}\Z")
HEX_PATTERN: Final = re.compile(rb"[0-9A-F]+\Z")
MAX_TOOL_BYTES: Final = 64 * 1024 * 1024
RESULT_FIELDS: Final = {
    "schema",
    "counts",
    "tracks",
    "slices",
    "counters",
    "flows",
    "args",
    "metadata",
    "stats",
    "facts",
}


class CompatibilityError(RuntimeError):
    pass


def fail(message: str) -> None:
    raise CompatibilityError(message)


def pairs_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            fail(f"duplicate JSON field: {key}")
        result[key] = value
    return result


def exact_fields(value: Any, fields: set[str], label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{label} must be an object")
    actual = set(value)
    if actual != fields:
        fail(
            f"{label} fields are not exact: "
            f"missing={sorted(fields - actual)}, extra={sorted(actual - fields)}"
        )
    return value


def owned(metadata: os.stat_result, label: str) -> None:
    if hasattr(os, "geteuid") and metadata.st_uid != os.geteuid():
        fail(f"{label} must be owned by the current user")


def is_within(candidate: Path, parent: Path) -> bool:
    try:
        candidate.relative_to(parent)
    except ValueError:
        return False
    return True


def existing_directory(raw: str, label: str) -> Path:
    candidate = Path(raw)
    if not candidate.is_absolute() or str(candidate) != raw:
        fail(f"{label} must be a canonical absolute path")
    try:
        metadata = candidate.lstat()
        resolved = candidate.resolve(strict=True)
    except OSError as error:
        fail(f"{label} must be an existing directory: {error}")
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
        fail(f"{label} must be a real directory")
    if resolved != candidate:
        fail(f"{label} must not use symlink indirection")
    owned(metadata, label)
    return candidate


def regular_file(path: Path, label: str, *, parent: Path | None = None) -> Path:
    try:
        metadata = path.lstat()
        resolved = path.resolve(strict=True)
    except OSError as error:
        fail(f"{label} must be an existing regular file: {error}")
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
        fail(f"{label} must be a regular file without symlink indirection")
    if resolved != path:
        fail(f"{label} must be its exact real path")
    if parent is not None and not is_within(path, parent):
        fail(f"{label} escapes its declared root")
    owned(metadata, label)
    return path


def read_regular_bytes(path: Path, label: str) -> bytes:
    metadata = path.lstat()
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(path, flags)
    try:
        opened = os.fstat(descriptor)
        if not stat.S_ISREG(opened.st_mode) or (
            opened.st_dev,
            opened.st_ino,
        ) != (metadata.st_dev, metadata.st_ino):
            fail(f"{label} changed while it was opened")
        chunks: list[bytes] = []
        while True:
            chunk = os.read(descriptor, 1024 * 1024)
            if not chunk:
                return b"".join(chunks)
            chunks.append(chunk)
    finally:
        os.close(descriptor)


def load_json(path: Path, label: str) -> dict[str, Any]:
    try:
        value = json.loads(
            read_regular_bytes(path, label).decode("utf-8"),
            object_pairs_hook=pairs_object,
        )
    except CompatibilityError:
        raise
    except (UnicodeError, json.JSONDecodeError) as error:
        fail(f"{label} is not valid UTF-8 JSON: {error}")
    if not isinstance(value, dict):
        fail(f"{label} must contain an object")
    return value


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def platform_name() -> tuple[str, str]:
    system = host_platform.system().lower()
    machine = host_platform.machine().lower()
    if system == "linux":
        if machine in {"x86_64", "amd64"}:
            return "linux-amd64", "trace_processor_shell"
        if machine in {"armv7l", "armv8l", "arm"}:
            return "linux-arm", "trace_processor_shell"
        if machine in {"aarch64", "arm64"}:
            return "linux-arm64", "trace_processor_shell"
    if system == "darwin":
        if machine in {"x86_64", "amd64"}:
            return "mac-amd64", "trace_processor_shell"
        if machine in {"aarch64", "arm64"}:
            return "mac-arm64", "trace_processor_shell"
    if system in {"windows", "msys", "cygwin"} and machine in {"x86_64", "amd64"}:
        return "windows-amd64", "trace_processor_shell.exe"
    fail(f"unsupported Perfetto tool platform: {system}-{machine}")


def verify_official_cache(root: Path, cache: Path, platform: str) -> dict[str, Any]:
    verifier = regular_file(
        root / "scripts/fetch_pinned_perfetto_tools.sh",
        "T04 verifier",
        parent=root,
    )
    environment = dict(os.environ)
    for name in (
        "TROUPE_DIAGNOSTIC_BOOTSTRAP_GATE",
        "TROUPE_PERFETTO_TEST_TRANSPORT",
        "TROUPE_GATE_TMP",
    ):
        environment.pop(name, None)
    environment["TROUPE_PERFETTO_NETWORK_FORBIDDEN"] = "1"
    completed = subprocess.run(
        [
            str(verifier),
            "--offline",
            "--verify-only",
            "--cache",
            str(cache),
            "--platform",
            platform,
        ],
        cwd=root,
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        timeout=30,
    )
    if completed.returncode != 0:
        detail = completed.stderr.decode("utf-8", "replace").strip()
        fail(f"T04 official cache verification failed: {detail}")

    identity_path = regular_file(
        cache / IDENTITY_NAME,
        "Perfetto cache identity",
        parent=cache,
    )
    identity = load_json(identity_path, "Perfetto cache identity")
    exact_fields(
        identity,
        {
            "schema_version",
            "release_tag",
            "release_commit",
            "platform",
            "manifest_sha256",
            "members",
            "test_only",
            "test_manifest",
        },
        "Perfetto cache identity",
    )
    if (
        identity["schema_version"] != 1
        or identity["release_tag"] != RELEASE_TAG
        or identity["release_commit"] != RELEASE_COMMIT
        or identity["platform"] != platform
        or identity["test_only"] is not False
        or identity["test_manifest"] is not None
    ):
        fail("Perfetto cache identity is not the official v57.2 cache")
    if identity["manifest_sha256"] != OFFICIAL_MANIFEST_SHA256:
        fail("Perfetto cache identity has the wrong official manifest hash")
    return identity


def tool_archive(identity: dict[str, Any], cache: Path, platform: str) -> tuple[Path, str]:
    members = identity["members"]
    if not isinstance(members, list) or len(members) != 2:
        fail("Perfetto cache identity must contain the exact tools/UI pair")
    tools: list[dict[str, Any]] = []
    for position, raw_member in enumerate(members):
        member = exact_fields(
            raw_member,
            {"kind", "cache_filename", "sha256"},
            f"Perfetto cache member {position}",
        )
        if not isinstance(member["sha256"], str) or not SHA256_PATTERN.fullmatch(
            member["sha256"]
        ):
            fail(f"Perfetto cache member {position} has an invalid hash")
        if member["kind"] == "tools":
            tools.append(member)
    if (
        len(tools) != 1
        or tools[0]["cache_filename"] != f"{platform}.zip"
        or tools[0]["sha256"] != TOOL_ARCHIVE_SHA256[platform]
    ):
        fail("Perfetto cache identity has no exact platform tool archive")
    archive = regular_file(
        cache / tools[0]["cache_filename"],
        "Perfetto tool archive",
        parent=cache,
    )
    return archive, tools[0]["sha256"]


def safe_zip_info(information: zipfile.ZipInfo) -> None:
    name = information.filename
    pure = PurePosixPath(name)
    if (
        not name
        or "\\" in name
        or pure.is_absolute()
        or any(part in {"", ".", ".."} for part in pure.parts)
    ):
        fail(f"Perfetto tool archive has an unsafe member: {name!r}")
    mode = information.external_attr >> 16
    if information.is_dir():
        if mode and not stat.S_ISDIR(mode):
            fail(f"Perfetto tool archive directory has invalid type: {name}")
    elif mode and not stat.S_ISREG(mode):
        fail(f"Perfetto tool archive member is not a regular file: {name}")


def extract_tool(
    archive_path: Path,
    expected_hash: str,
    platform: str,
    executable_name: str,
    target: Path,
) -> None:
    metadata = archive_path.lstat()
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(archive_path, flags)
    try:
        opened = os.fstat(descriptor)
        if not stat.S_ISREG(opened.st_mode) or (
            opened.st_dev,
            opened.st_ino,
        ) != (metadata.st_dev, metadata.st_ino):
            fail("Perfetto tool archive changed while it was opened")
        digest = hashlib.sha256()
        while True:
            chunk = os.read(descriptor, 1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
        if digest.hexdigest() != expected_hash:
            fail("Perfetto tool archive hash changed after T04 verification")
        os.lseek(descriptor, 0, os.SEEK_SET)
        with os.fdopen(descriptor, "rb", closefd=False) as source:
            with zipfile.ZipFile(source) as archive:
                information = archive.infolist()
                names = [item.filename for item in information]
                if len(names) != len(set(names)):
                    fail("Perfetto tool archive contains duplicate member names")
                for item in information:
                    safe_zip_info(item)
                expected_name = f"{platform}/{executable_name}"
                matches = [item for item in information if item.filename == expected_name]
                if len(matches) != 1:
                    fail("Perfetto tool archive has no unique trace_processor_shell")
                selected = matches[0]
                if selected.file_size <= 0 or selected.file_size > MAX_TOOL_BYTES:
                    fail("trace_processor_shell has an invalid extracted size")
                output_flags = (
                    os.O_WRONLY
                    | os.O_CREAT
                    | os.O_EXCL
                    | getattr(os, "O_NOFOLLOW", 0)
                )
                output = os.open(target, output_flags, 0o700)
                written = 0
                try:
                    with archive.open(selected, "r") as member:
                        while True:
                            chunk = member.read(1024 * 1024)
                            if not chunk:
                                break
                            written += len(chunk)
                            if written > selected.file_size:
                                fail("trace_processor_shell exceeded its declared size")
                            remaining = memoryview(chunk)
                            while remaining:
                                count = os.write(output, remaining)
                                if count <= 0:
                                    fail("trace_processor_shell extraction made no progress")
                                remaining = remaining[count:]
                    if written != selected.file_size:
                        fail("trace_processor_shell extraction was truncated")
                    os.fchmod(output, 0o700)
                    os.fsync(output)
                finally:
                    os.close(output)
    except (OSError, zipfile.BadZipFile, zipfile.LargeZipFile) as error:
        fail(f"could not extract trace_processor_shell: {error}")
    finally:
        os.close(descriptor)


def fixture_path(root: Path, raw: str, name: str) -> Path:
    expected = f"tests/fixtures/perfetto/traces/{name}.pftrace"
    if raw != expected:
        fail(f"fixture {name} path is not exact: {raw!r}")
    pure = PurePosixPath(raw)
    if pure.is_absolute() or any(part in {"", ".", ".."} for part in pure.parts):
        fail(f"fixture {name} path is unsafe")
    return regular_file(root.joinpath(*pure.parts), f"fixture {name}", parent=root)


def load_inputs(root: Path) -> tuple[Path, dict[str, Any], list[tuple[str, Path]]]:
    sql_root = root / "tests/perfetto/sql"
    assertions = regular_file(sql_root / "assertions.sql", "SQL assertions", parent=root)
    manifest = load_json(
        regular_file(sql_root / "fixtures.manifest.json", "SQL fixture manifest", parent=root),
        "SQL fixture manifest",
    )
    exact_fields(manifest, {"schema", "files"}, "SQL fixture manifest")
    if manifest["schema"] != "troupe.perfetto.sql-fixtures.v1":
        fail("SQL fixture manifest schema is unsupported")
    files = manifest["files"]
    if not isinstance(files, list) or len(files) != len(FIXTURE_NAMES):
        fail("SQL fixture manifest file set is not exact")
    fixtures: list[tuple[str, Path]] = []
    for position, (raw_file, expected_name) in enumerate(zip(files, FIXTURE_NAMES)):
        entry = exact_fields(
            raw_file,
            {"name", "path", "sha256"},
            f"SQL fixture {position}",
        )
        if entry["name"] != expected_name:
            fail(f"SQL fixture order/name mismatch at position {position}")
        if not isinstance(entry["path"], str) or not isinstance(entry["sha256"], str):
            fail(f"SQL fixture {expected_name} path/hash must be strings")
        if SHA256_PATTERN.fullmatch(entry["sha256"]) is None:
            fail(f"SQL fixture {expected_name} has an invalid SHA-256")
        path = fixture_path(root, entry["path"], expected_name)
        actual = sha256_bytes(read_regular_bytes(path, f"fixture {expected_name}"))
        if actual != entry["sha256"]:
            fail(
                f"fixture {expected_name} hash mismatch: "
                f"expected {entry['sha256']}, got {actual}"
            )
        fixtures.append((expected_name, path))

    expected = load_json(
        regular_file(sql_root / "expected.json", "SQL expectations", parent=root),
        "SQL expectations",
    )
    exact_fields(expected, {"schema", "tool", "fixtures"}, "SQL expectations")
    if expected["schema"] != "troupe.perfetto.sql-expectations.v1":
        fail("SQL expectation schema is unsupported")
    tool = exact_fields(
        expected["tool"],
        {"release_tag", "release_commit", "rpc_api_version"},
        "SQL expected tool",
    )
    if tool != {
        "release_tag": RELEASE_TAG,
        "release_commit": RELEASE_COMMIT,
        "rpc_api_version": RPC_API_VERSION,
    }:
        fail("SQL expectations are not bound to official Perfetto v57.2")
    expected_fixtures = expected["fixtures"]
    if not isinstance(expected_fixtures, dict) or tuple(expected_fixtures) != FIXTURE_NAMES:
        fail("SQL expectation fixture set/order is not exact")
    for name in FIXTURE_NAMES:
        validate_result(expected_fixtures[name], f"expected fixture {name}")
    return assertions, expected, fixtures


def validate_result(value: Any, label: str) -> dict[str, Any]:
    result = exact_fields(value, RESULT_FIELDS, label)
    if result["schema"] != "troupe.perfetto.sql-result.v1":
        fail(f"{label} has an unsupported result schema")
    nested = {
        "counts": {
            "tracks",
            "slices",
            "counters",
            "flows",
            "args",
            "metadata",
            "track_event_stats",
        },
        "args": {"troupe_count", "keys"},
        "metadata": {"trace_type", "trace_size_bytes", "production_roots"},
        "stats": {"missing_sequence_id", "invalid_counter_track_uuid", "nonzero_errors"},
        "facts": {
            "open_slices",
            "overlapping_cue_pairs",
            "non_exact_fallbacks",
            "fallback_counter_tracks",
            "i64_max_counters",
        },
    }
    for name, fields in nested.items():
        exact_fields(result[name], fields, f"{label}.{name}")
    row_fields = {
        "tracks": {"name", "type", "parent"},
        "slices": {"name", "ts", "dur", "track", "depth"},
        "counters": {"name", "ts", "value", "track_type"},
        "flows": {"outgoing", "incoming"},
    }
    for name, fields in row_fields.items():
        rows = result[name]
        if not isinstance(rows, list):
            fail(f"{label}.{name} must be an array")
        for position, row in enumerate(rows):
            exact_fields(row, fields, f"{label}.{name}[{position}]")
    keys = result["args"]["keys"]
    if (
        not isinstance(keys, list)
        or any(not isinstance(key, str) for key in keys)
        or keys != sorted(set(keys))
    ):
        fail(f"{label}.args.keys must be sorted unique strings")
    for section in ("counts", "stats", "facts"):
        if any(type(item) is not int for item in result[section].values()):
            fail(f"{label}.{section} values must be integers")
    if type(result["args"]["troupe_count"]) is not int:
        fail(f"{label}.args.troupe_count must be an integer")
    if result["metadata"]["trace_type"] != "proto":
        fail(f"{label}.metadata.trace_type must be proto")
    size = result["metadata"]["trace_size_bytes"]
    if not isinstance(size, str) or not size.isdecimal():
        fail(f"{label}.metadata.trace_size_bytes must be a decimal string")
    return result


def parse_query_stdout(stdout: bytes, label: str) -> dict[str, Any]:
    lines = stdout.splitlines()
    if len(lines) != 2 or lines[0] != b'"result_hex"':
        fail(f"{label} query output schema drifted")
    encoded = lines[1]
    if len(encoded) < 3 or encoded[:1] != b'"' or encoded[-1:] != b'"':
        fail(f"{label} query result framing drifted")
    payload_hex = encoded[1:-1]
    if len(payload_hex) % 2 or HEX_PATTERN.fullmatch(payload_hex) is None:
        fail(f"{label} query result is not canonical uppercase hex")
    try:
        payload = bytes.fromhex(payload_hex.decode("ascii"))
        value = json.loads(payload.decode("utf-8"), object_pairs_hook=pairs_object)
    except CompatibilityError:
        raise
    except (UnicodeError, ValueError, json.JSONDecodeError) as error:
        fail(f"{label} query result is not valid UTF-8 JSON: {error}")
    return validate_result(value, label)


def difference(expected: Any, actual: Any, path: str = "$") -> str:
    if type(expected) is not type(actual):
        return f"{path}: expected {type(expected).__name__}, got {type(actual).__name__}"
    if isinstance(expected, dict):
        if set(expected) != set(actual):
            return f"{path}: field set differs"
        for key in expected:
            if expected[key] != actual[key]:
                return difference(expected[key], actual[key], f"{path}.{key}")
    elif isinstance(expected, list):
        if len(expected) != len(actual):
            return f"{path}: expected {len(expected)} rows, got {len(actual)}"
        for position, item in enumerate(expected):
            if item != actual[position]:
                return difference(item, actual[position], f"{path}[{position}]")
    return f"{path}: expected {expected!r}, got {actual!r}"


def run_tool(tool: Path, assertions: Path, fixtures: list[tuple[str, Path]]) -> dict[str, Any]:
    version = subprocess.run(
        [str(tool), "--version"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        timeout=10,
    )
    if version.returncode != 0 or version.stdout != VERSION_STDOUT or version.stderr:
        fail("trace_processor_shell version identity does not match official v57.2")

    results: dict[str, Any] = {}
    environment = dict(os.environ)
    environment["TROUPE_PERFETTO_NETWORK_FORBIDDEN"] = "1"
    for name, trace in fixtures:
        try:
            completed = subprocess.run(
                [str(tool), "query", "-f", str(assertions), str(trace)],
                cwd=assertions.parents[3],
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
                timeout=30,
            )
        except subprocess.TimeoutExpired:
            fail(f"trace_processor_shell timed out for fixture {name}")
        if completed.returncode != 0:
            detail = completed.stderr.decode("utf-8", "replace").strip()
            fail(f"trace_processor_shell failed for fixture {name}: {detail}")
        results[name] = parse_query_stdout(completed.stdout, f"fixture {name}")
    return results


def parse_arguments(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Verify Troupe traces with Perfetto v57.2 SQL")
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--cache")
    options = parser.parse_args(argv)
    if not options.offline:
        parser.error("--offline is required")
    if options.cache is None:
        parser.error("--cache is required")
    return options


def main(argv: list[str]) -> int:
    root = Path(argv[0]).resolve(strict=True)
    options = parse_arguments(argv[1:])
    cache = existing_directory(options.cache, "--cache")
    if is_within(cache, root):
        fail("--cache must remain outside the repository")
    platform, executable_name = platform_name()
    assertions, expected, fixtures = load_inputs(root)
    identity = verify_official_cache(root, cache, platform)
    archive, archive_hash = tool_archive(identity, cache, platform)

    with tempfile.TemporaryDirectory(prefix="troupe-perfetto-sql-") as raw_temp:
        temporary = Path(raw_temp).resolve(strict=True)
        if is_within(temporary, root):
            fail("Perfetto SQL temporary directory must remain outside the repository")
        tool = temporary / executable_name
        extract_tool(archive, archive_hash, platform, executable_name, tool)
        actual = run_tool(tool, assertions, fixtures)

    expected_fixtures = expected["fixtures"]
    for name in FIXTURE_NAMES:
        if actual[name] != expected_fixtures[name]:
            fail(f"fixture {name} SQL expectation mismatch: {difference(expected_fixtures[name], actual[name])}")

    report = {
        "schema": "troupe.perfetto.sql-report.v1",
        "tool": expected["tool"],
        "fixtures": actual,
    }
    print(json.dumps(report, ensure_ascii=True, separators=(",", ":"), sort_keys=True))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except (CompatibilityError, OSError, subprocess.SubprocessError) as error:
        print(f"Perfetto SQL compatibility: {error}", file=sys.stderr)
        raise SystemExit(1)
PY
