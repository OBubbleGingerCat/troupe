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
import http.server
import json
import mimetypes
import os
import platform as host_platform
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import threading
import urllib.parse
import zipfile
from pathlib import Path, PurePosixPath
from typing import Any, Final


RELEASE_TAG: Final = "v57.2"
RELEASE_COMMIT: Final = "da1d152cff27890903d158fe96751de3aab883cc"
UI_SHA256: Final = "3d4043c4451faaddec8f382b1a0efae4c33ca9b168ec037da32e53c1cc308408"
OFFICIAL_MANIFEST_SHA256: Final = (
    "8223d7de5b7afd0b59e50813fbdef2c00271bd8eb0cb4515c2c16e3b3fb68a50"
)
LOCK_SHA256: Final = "d02077d88fce2afe6c62a2f6d5aa75b2a5bcfe6cbbda040f1cffbffe35eba595"
BROWSER_MANIFEST_SHA256: Final = (
    "19a225a7747d22b60fb56afcfe4ea9b25846058295fd57128b021cdba9e9b8c5"
)
CHROMIUM_VERSION: Final = "151.0.7922.34"
CHROMIUM_REVISION: Final = "1234"
CHROMIUM_EXECUTABLE_SHA256: Final = (
    "0b20b130e7edd9dd51873be867761295fe0cfad490c2b9a64f95bd3cfc08fa71"
)
IDENTITY_NAME: Final = ".troupe-perfetto-cache.json"
BROWSER_IDENTITY_NAME: Final = ".troupe-playwright-cache.json"
NPM_IDENTITY_NAME: Final = ".troupe-npm-cache.json"
SHA256_PATTERN: Final = re.compile(r"[0-9a-f]{64}\Z")
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
MAX_UI_MEMBERS: Final = 4096
MAX_UI_MEMBER_BYTES: Final = 128 * 1024 * 1024
MAX_UI_TOTAL_BYTES: Final = 256 * 1024 * 1024


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


def is_within(candidate: Path, parent: Path) -> bool:
    try:
        candidate.relative_to(parent)
    except ValueError:
        return False
    return True


def owned(metadata: os.stat_result, label: str) -> None:
    if hasattr(os, "geteuid") and metadata.st_uid != os.geteuid():
        fail(f"{label} must be owned by the current user")


def existing_directory(raw: str, label: str, *, repository: Path) -> Path:
    candidate = Path(raw)
    if not candidate.is_absolute() or str(candidate) != raw:
        fail(f"{label} must be a canonical absolute path")
    try:
        metadata = candidate.lstat()
        resolved = candidate.resolve(strict=True)
    except OSError as error:
        fail(f"{label} must be an existing directory: {error}")
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
        fail(f"{label} must be a real directory without symlink indirection")
    if resolved != candidate:
        fail(f"{label} must be its exact real path")
    if is_within(candidate, repository):
        fail(f"{label} must remain outside the repository")
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
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
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


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def sha256_file(path: Path, label: str) -> str:
    return sha256_bytes(read_regular_bytes(path, label))


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


def canonical_relative_path(raw: str, label: str) -> PurePosixPath:
    if not isinstance(raw, str):
        fail(f"{label} must be a string")
    pure = PurePosixPath(raw)
    if (
        not raw
        or "\\" in raw
        or "\0" in raw
        or pure.is_absolute()
        or any(part in {"", ".", ".."} for part in pure.parts)
    ):
        fail(f"{label} is not a canonical relative path: {raw!r}")
    return pure


def validate_hash(raw: Any, label: str) -> str:
    if not isinstance(raw, str) or SHA256_PATTERN.fullmatch(raw) is None:
        fail(f"{label} must be a lowercase SHA-256")
    return raw


def validate_counts(raw: Any, label: str) -> dict[str, int]:
    value = exact_fields(raw, {"tracks", "slices", "counters", "flows"}, label)
    if any(type(item) is not int or item < 0 for item in value.values()):
        fail(f"{label} values must be non-negative integers")
    return value


def fixture_file(root: Path, raw: str, expected: str, digest: str) -> tuple[Path, bytes]:
    expected_path = f"tests/fixtures/perfetto/traces/{expected}.pftrace"
    if raw != expected_path:
        fail(f"fixture {expected} path is not exact: {raw!r}")
    pure = canonical_relative_path(raw, f"fixture {expected} path")
    path = regular_file(root.joinpath(*pure.parts), f"fixture {expected}", parent=root)
    payload = read_regular_bytes(path, f"fixture {expected}")
    actual = sha256_bytes(payload)
    if actual != digest:
        fail(f"fixture {expected} hash mismatch: expected {digest}, got {actual}")
    return path, payload


def validate_pixel_oracle(raw: dict[str, Any]) -> None:
    exact_fields(raw, {"schema", "viewport", "timeouts_ms", "fixtures"}, "pixel oracle")
    if raw["schema"] != "troupe.perfetto.ui-pixel-oracle.v1":
        fail("pixel oracle schema is unsupported")
    viewport = exact_fields(
        raw["viewport"],
        {"width", "height", "device_scale_factor"},
        "pixel oracle viewport",
    )
    if viewport != {"width": 1440, "height": 1000, "device_scale_factor": 1}:
        fail("pixel oracle viewport must remain pinned")
    timeouts = exact_fields(raw["timeouts_ms"], {"load", "query", "pixels"}, "pixel timeouts")
    if (
        type(timeouts["load"]) is not int
        or not 1_000 <= timeouts["load"] <= 180_000
        or type(timeouts["query"]) is not int
        or not 1_000 <= timeouts["query"] <= 60_000
        or type(timeouts["pixels"]) is not int
        or not 1_000 <= timeouts["pixels"] <= 60_000
    ):
        fail("pixel oracle timeouts are outside the closed bounds")
    fixtures = exact_fields(raw["fixtures"], {"projection-flow"}, "pixel fixture map")
    probe = exact_fields(
        fixtures["projection-flow"],
        {"minimum_canvas_count", "canvases"},
        "projection-flow pixel oracle",
    )
    if type(probe["minimum_canvas_count"]) is not int or probe["minimum_canvas_count"] < 2:
        fail("projection-flow minimum canvas count is invalid")
    canvases = probe["canvases"]
    if not isinstance(canvases, list) or len(canvases) != 2:
        fail("projection-flow must have two canvas oracles")
    for position, raw_canvas in enumerate(canvases):
        canvas = exact_fields(
            raw_canvas,
            {
                "index",
                "width",
                "height",
                "minimum_opaque_pixels",
                "minimum_distinct_colors",
                "key_pixels",
            },
            f"canvas oracle {position}",
        )
        if canvas["index"] != position:
            fail("canvas oracle indexes must be dense and ordered")
        for field in ("width", "height", "minimum_opaque_pixels", "minimum_distinct_colors"):
            if type(canvas[field]) is not int or canvas[field] <= 0:
                fail(f"canvas oracle {position}.{field} must be a positive integer")
        points = canvas["key_pixels"]
        if not isinstance(points, list) or len(points) != 1:
            fail(f"canvas oracle {position} must have one key pixel")
        point = exact_fields(points[0], {"x", "y", "rgba"}, f"canvas {position} key pixel")
        if type(point["x"]) is not int or type(point["y"]) is not int:
            fail(f"canvas {position} key pixel coordinates must be integers")
        rgba = point["rgba"]
        if (
            not isinstance(rgba, list)
            or len(rgba) != 4
            or any(type(channel) is not int or not 0 <= channel <= 255 for channel in rgba)
        ):
            fail(f"canvas {position} key pixel RGBA is invalid")


def load_inputs(root: Path) -> tuple[dict[str, Any], dict[str, Any], list[dict[str, Any]]]:
    ui_root = root / "tests/perfetto/ui"
    manifest = load_json(
        regular_file(ui_root / "fixtures.manifest.json", "UI fixture manifest", parent=root),
        "UI fixture manifest",
    )
    exact_fields(manifest, {"schema", "perfetto", "files", "flow_probe"}, "UI fixture manifest")
    if manifest["schema"] != "troupe.perfetto.ui-fixtures.v1":
        fail("UI fixture manifest schema is unsupported")
    perfetto = exact_fields(
        manifest["perfetto"],
        {"release_tag", "release_commit", "ui_sha256"},
        "UI fixture Perfetto identity",
    )
    if perfetto != {
        "release_tag": RELEASE_TAG,
        "release_commit": RELEASE_COMMIT,
        "ui_sha256": UI_SHA256,
    }:
        fail("UI fixture manifest does not pin official Perfetto v57.2")

    trace_manifest = load_json(
        regular_file(
            root / "tests/fixtures/perfetto/traces/manifest.json",
            "T03 trace manifest",
            parent=root,
        ),
        "T03 trace manifest",
    )
    exact_fields(trace_manifest, {"schema", "files"}, "T03 trace manifest")
    if trace_manifest["schema"] != "troupe.perfetto.trace-fixtures.v1":
        fail("T03 trace manifest schema is unsupported")
    trace_files = trace_manifest["files"]
    files = manifest["files"]
    if (
        not isinstance(files, list)
        or not isinstance(trace_files, list)
        or len(files) != len(FIXTURE_NAMES)
        or len(trace_files) != len(FIXTURE_NAMES)
    ):
        fail("UI fixture manifest must reference the exact nine T03 traces")

    served: list[dict[str, Any]] = []
    for position, expected_name in enumerate(FIXTURE_NAMES):
        entry = exact_fields(
            files[position],
            {"name", "path", "sha256", "counts", "coverage"},
            f"UI fixture {position}",
        )
        source = exact_fields(
            trace_files[position],
            {"path", "bytes", "sha256"},
            f"T03 fixture {position}",
        )
        if entry["name"] != expected_name or entry["coverage"] != expected_name:
            fail(f"UI fixture order/name/coverage mismatch at position {position}")
        digest = validate_hash(entry["sha256"], f"UI fixture {expected_name} hash")
        expected_relative = f"tests/fixtures/perfetto/traces/{source['path']}"
        if entry["path"] != expected_relative or digest != source["sha256"]:
            fail(f"UI fixture {expected_name} does not exactly reference T03")
        if type(source["bytes"]) is not int or source["bytes"] < 0:
            fail(f"T03 fixture {expected_name} byte count is invalid")
        counts = validate_counts(entry["counts"], f"UI fixture {expected_name} counts")
        path, payload = fixture_file(root, entry["path"], expected_name, digest)
        if len(payload) != source["bytes"]:
            fail(f"fixture {expected_name} byte count differs from T03")
        served.append(
            {"name": expected_name, "path": path, "payload": payload, "sha256": digest, "counts": counts}
        )

    if files[3]["sha256"] != files[8]["sha256"]:
        fail("repeated-dump must remain byte-identical to multi-cue")

    projection_manifest = load_json(
        regular_file(
            root / "tests/fixtures/perfetto/projection/manifest.json",
            "projection fixture manifest",
            parent=root,
        ),
        "projection fixture manifest",
    )
    projection_files = projection_manifest.get("files")
    if not isinstance(projection_files, dict) or "expected-trace.pb" not in projection_files:
        fail("projection fixture manifest has no expected trace")
    projection_digest = projection_files["expected-trace.pb"].get("sha256")
    probe = exact_fields(
        manifest["flow_probe"],
        {"name", "path", "sha256", "counts", "required_labels", "pixel_oracle"},
        "UI flow probe",
    )
    if (
        probe["name"] != "projection-flow"
        or probe["path"] != "tests/fixtures/perfetto/projection/expected-trace.pb"
        or probe["pixel_oracle"] != "projection-flow"
        or probe["sha256"] != projection_digest
    ):
        fail("UI flow probe does not exactly reference the projection trace")
    digest = validate_hash(probe["sha256"], "UI flow probe hash")
    counts = validate_counts(probe["counts"], "UI flow probe counts")
    if any(counts[field] <= 0 for field in ("tracks", "slices", "counters", "flows")):
        fail("UI flow probe must exercise every searchable Perfetto table")
    labels = probe["required_labels"]
    if not isinstance(labels, list) or len(labels) != 1 or not isinstance(labels[0], str):
        fail("UI flow probe must pin one visible production label")
    path = regular_file(root / probe["path"], "projection flow trace", parent=root)
    payload = read_regular_bytes(path, "projection flow trace")
    actual = sha256_bytes(payload)
    if actual != digest:
        fail(f"projection flow trace hash mismatch: expected {digest}, got {actual}")
    served.append(
        {"name": probe["name"], "path": path, "payload": payload, "sha256": digest, "counts": counts}
    )

    pixel = load_json(
        regular_file(ui_root / "pixel-oracle.json", "pixel oracle", parent=root),
        "pixel oracle",
    )
    validate_pixel_oracle(pixel)
    return manifest, pixel, served


def platform_names() -> tuple[str, str]:
    system = host_platform.system().lower()
    machine = host_platform.machine().lower()
    if system == "linux" and machine in {"x86_64", "amd64"}:
        return "linux-amd64", "linux-x64"
    fail(f"unsupported Perfetto UI browser platform: {system}-{machine}")


def offline_environment() -> dict[str, str]:
    environment = dict(os.environ)
    environment.update(
        {
            "HTTP_PROXY": "http://127.0.0.1:9/",
            "HTTPS_PROXY": "http://127.0.0.1:9/",
            "ALL_PROXY": "http://127.0.0.1:9/",
            "NO_PROXY": "127.0.0.1,localhost",
        }
    )
    return environment


def verify_perfetto_cache(root: Path, cache: Path, platform: str) -> Path:
    if cache.name != platform:
        fail(f"--cache must end in the exact platform directory {platform!r}")
    if cache.stat().st_mode & 0o222:
        fail("Perfetto cache must be read-only")
    identity_path = regular_file(cache / IDENTITY_NAME, "Perfetto cache identity", parent=cache)
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
        or identity["manifest_sha256"] != OFFICIAL_MANIFEST_SHA256
        or identity["test_only"] is not False
        or identity["test_manifest"] is not None
    ):
        fail("Perfetto cache identity is not the official v57.2 cache")
    members = identity["members"]
    if not isinstance(members, list) or len(members) != 2:
        fail("Perfetto cache identity must contain the exact tools/UI pair")
    ui_members = [member for member in members if isinstance(member, dict) and member.get("kind") == "ui"]
    if len(ui_members) != 1:
        fail("Perfetto cache identity has no unique UI member")
    ui = exact_fields(ui_members[0], {"kind", "cache_filename", "sha256"}, "Perfetto UI member")
    if ui["cache_filename"] != "perfetto-ui.zip" or ui["sha256"] != UI_SHA256:
        fail("Perfetto cache identity has the wrong UI archive")
    archive = regular_file(cache / "perfetto-ui.zip", "Perfetto UI archive", parent=cache)
    if sha256_file(archive, "Perfetto UI archive") != UI_SHA256:
        fail("Perfetto UI archive hash mismatch")

    verifier = regular_file(
        root / "scripts/fetch_pinned_perfetto_tools.sh",
        "T04 verifier",
        parent=root,
    )
    environment = offline_environment()
    environment["TROUPE_PERFETTO_NETWORK_FORBIDDEN"] = "1"
    for name in ("TROUPE_PERFETTO_TEST_TRANSPORT",):
        environment.pop(name, None)
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
        timeout=60,
    )
    if completed.returncode != 0:
        detail = completed.stderr.decode("utf-8", "replace").strip()
        fail(f"T04 official cache verification failed: {detail}")
    return archive


def verify_browser_cache(root: Path, cache: Path, platform: str) -> tuple[Path, dict[str, str]]:
    if cache.name != platform or cache.parent.name != LOCK_SHA256:
        fail("--browser-cache must end in the exact lock SHA-256 and platform")
    if cache.stat().st_mode & 0o222:
        fail("browser cache must be read-only")
    identity = load_json(
        regular_file(cache / BROWSER_IDENTITY_NAME, "browser cache identity", parent=cache),
        "browser cache identity",
    )
    exact_fields(
        identity,
        {
            "schemaVersion",
            "manifestSha256",
            "lockSha256",
            "platform",
            "playwrightPlatform",
            "playwrightCore",
            "archives",
        },
        "browser cache identity",
    )
    if (
        identity["schemaVersion"] != 1
        or identity["manifestSha256"] != BROWSER_MANIFEST_SHA256
        or identity["lockSha256"] != LOCK_SHA256
        or identity["platform"] != platform
    ):
        fail("browser cache identity does not match the pinned W16 cache")
    archives = identity["archives"]
    if not isinstance(archives, list):
        fail("browser cache archive inventory must be a list")
    chromium = [item for item in archives if isinstance(item, dict) and item.get("name") == "chromium"]
    if len(chromium) != 1:
        fail("browser cache has no unique full Chromium")
    record = chromium[0]
    if (
        record.get("revision") != CHROMIUM_REVISION
        or record.get("browserVersion") != CHROMIUM_VERSION
        or record.get("cacheDirectory") != f"chromium-{CHROMIUM_REVISION}"
        or record.get("executable") != "chrome-linux64/chrome"
        or record.get("executableSha256") != CHROMIUM_EXECUTABLE_SHA256
    ):
        fail("browser cache full Chromium identity drifted")
    executable = regular_file(
        cache / record["cacheDirectory"] / record["executable"],
        "pinned Chromium executable",
        parent=cache,
    )
    if not os.access(executable, os.X_OK):
        fail("pinned Chromium executable is not executable")
    if sha256_file(executable, "pinned Chromium executable") != CHROMIUM_EXECUTABLE_SHA256:
        fail("pinned Chromium executable hash mismatch")

    verifier = regular_file(
        root / "frontend/diagnostics/scripts/provision_browsers.mjs",
        "W16 browser verifier",
        parent=root,
    )
    environment = offline_environment()
    completed = subprocess.run(
        ["node", str(verifier), "--browser-cache", str(cache)],
        cwd=root,
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        timeout=180,
    )
    if completed.returncode != 0:
        detail = completed.stderr.decode("utf-8", "replace").strip()
        fail(f"W16 browser cache verification failed: {detail}")
    return executable, {
        "name": "chromium",
        "version": CHROMIUM_VERSION,
        "executable_sha256": CHROMIUM_EXECUTABLE_SHA256,
    }


def validate_npm_cache(root: Path, cache: Path) -> None:
    if cache.name != "22" or cache.parent.name != LOCK_SHA256:
        fail("TROUPE_NPM_CACHE must end in the exact lock SHA-256 and Node major")
    if cache.stat().st_mode & 0o222:
        fail("npm cache must be read-only")
    identity = load_json(
        regular_file(cache / NPM_IDENTITY_NAME, "npm cache identity", parent=cache),
        "npm cache identity",
    )
    exact_fields(
        identity,
        {"schemaVersion", "lockSha256", "nodeMajor", "nodeVersion", "npmVersion", "members"},
        "npm cache identity",
    )
    if (
        identity["schemaVersion"] != 1
        or identity["lockSha256"] != LOCK_SHA256
        or identity["nodeMajor"] != 22
        or identity["nodeVersion"] != "22.22.0"
        or identity["npmVersion"] != "10.9.4"
        or not isinstance(identity["members"], list)
    ):
        fail("npm cache identity does not match the pinned frontend lock")
    lock = regular_file(root / "frontend/diagnostics/package-lock.json", "frontend lock", parent=root)
    if sha256_file(lock, "frontend lock") != LOCK_SHA256:
        fail("frontend package lock hash drifted")
    for command, expected in ((["node", "--version"], "v22.22.0\n"), (["npm", "--version"], "10.9.4\n")):
        completed = subprocess.run(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=10,
        )
        if completed.returncode != 0 or completed.stdout.decode("ascii", "replace") != expected:
            fail(f"pinned toolchain check failed for {command[0]}")


def safe_ui_members(archive: zipfile.ZipFile) -> list[zipfile.ZipInfo]:
    members = archive.infolist()
    if not 1 <= len(members) <= MAX_UI_MEMBERS:
        fail("Perfetto UI archive member count is outside the closed bounds")
    names: set[str] = set()
    total = 0
    for member in members:
        raw = member.filename
        stripped = raw[:-1] if raw.endswith("/") else raw
        pure = canonical_relative_path(stripped, "Perfetto UI archive member")
        normalized = pure.as_posix()
        if normalized in names:
            fail(f"Perfetto UI archive has a duplicate member: {normalized}")
        names.add(normalized)
        mode = member.external_attr >> 16
        kind = stat.S_IFMT(mode)
        if member.is_dir():
            if kind not in {0, stat.S_IFDIR}:
                fail(f"Perfetto UI archive directory type is invalid: {raw}")
            continue
        if kind not in {0, stat.S_IFREG}:
            fail(f"Perfetto UI archive member is not a regular file: {raw}")
        if member.flag_bits & 0x1:
            fail(f"Perfetto UI archive member is encrypted: {raw}")
        if member.file_size < 0 or member.file_size > MAX_UI_MEMBER_BYTES:
            fail(f"Perfetto UI archive member is too large: {raw}")
        total += member.file_size
        if total > MAX_UI_TOTAL_BYTES:
            fail("Perfetto UI archive exceeds the total extracted-size limit")
    required = {
        "index.html",
        "v57.2-da1d152cf/frontend_bundle.js",
        "v57.2-da1d152cf/frontend.css",
        "v57.2-da1d152cf/trace_processor_memory64.wasm",
    }
    if not required <= names:
        fail(f"Perfetto UI archive is missing release members: {sorted(required - names)}")
    return members


def extract_ui(archive_path: Path, destination: Path) -> None:
    if sha256_file(archive_path, "Perfetto UI archive") != UI_SHA256:
        fail("Perfetto UI archive changed after cache verification")
    try:
        with zipfile.ZipFile(archive_path) as archive:
            members = safe_ui_members(archive)
            for member in members:
                raw = member.filename
                stripped = raw[:-1] if raw.endswith("/") else raw
                pure = PurePosixPath(stripped)
                target = destination.joinpath(*pure.parts)
                if member.is_dir():
                    target.mkdir(parents=True, exist_ok=False)
                    continue
                target.parent.mkdir(parents=True, exist_ok=True)
                written = 0
                with archive.open(member, "r") as source, target.open("xb") as output:
                    while True:
                        chunk = source.read(1024 * 1024)
                        if not chunk:
                            break
                        written += len(chunk)
                        if written > member.file_size:
                            fail(f"Perfetto UI member exceeded declared size: {raw}")
                        output.write(chunk)
                    output.flush()
                    os.fsync(output.fileno())
                if written != member.file_size:
                    fail(f"Perfetto UI member extraction was truncated: {raw}")
    except CompatibilityError:
        raise
    except (OSError, zipfile.BadZipFile, zipfile.LargeZipFile, RuntimeError) as error:
        fail(f"could not safely extract Perfetto UI: {error}")


class LoopbackServer(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, ui_root: Path, traces: dict[str, bytes]) -> None:
        self.ui_root = ui_root
        self.traces = traces
        self.requests: list[str] = []
        self.request_lock = threading.Lock()
        super().__init__(("127.0.0.1", 0), LoopbackHandler)


class LoopbackHandler(http.server.BaseHTTPRequestHandler):
    server: LoopbackServer
    protocol_version = "HTTP/1.1"

    def log_message(self, _format: str, *_arguments: object) -> None:
        return

    def do_GET(self) -> None:
        self._serve(head=False)

    def do_HEAD(self) -> None:
        self._serve(head=True)

    def do_POST(self) -> None:
        self.send_error(405)

    def do_PUT(self) -> None:
        self.send_error(405)

    def _payload(self, path: str) -> tuple[bytes, str] | None:
        if path.startswith("/traces/"):
            name = path.removeprefix("/traces/")
            if not re.fullmatch(r"[a-z0-9-]+\.pftrace", name):
                return None
            payload = self.server.traces.get(name.removesuffix(".pftrace"))
            return None if payload is None else (payload, "application/octet-stream")
        relative = "index.html" if path == "/" else path.removeprefix("/")
        try:
            pure = canonical_relative_path(relative, "HTTP UI path")
            target = self.server.ui_root.joinpath(*pure.parts)
            target = regular_file(target, "served Perfetto UI member", parent=self.server.ui_root)
            payload = read_regular_bytes(target, "served Perfetto UI member")
        except CompatibilityError:
            return None
        content_type = mimetypes.guess_type(target.name)[0] or "application/octet-stream"
        if target.suffix == ".js":
            content_type = "text/javascript"
        elif target.suffix == ".wasm":
            content_type = "application/wasm"
        return payload, content_type

    def _serve(self, *, head: bool) -> None:
        try:
            parsed = urllib.parse.urlsplit(self.path)
            path = urllib.parse.unquote(parsed.path, errors="strict")
        except (UnicodeError, ValueError):
            self.send_error(400)
            return
        if parsed.scheme or parsed.netloc or "\\" in path or "\0" in path or "//" in path:
            self.send_error(400)
            return
        with self.server.request_lock:
            self.server.requests.append(f"{self.command} {path}")
        selected = self._payload(path)
        if selected is None:
            self.send_error(404)
            return
        payload, content_type = selected
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("Cross-Origin-Opener-Policy", "same-origin")
        self.send_header("Cross-Origin-Embedder-Policy", "require-corp")
        self.send_header("Cross-Origin-Resource-Policy", "same-origin")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.end_headers()
        if not head:
            try:
                self.wfile.write(payload)
            except (BrokenPipeError, ConnectionResetError):
                return


def npm_environment(temporary: Path, cache: Path, browser_cache: Path) -> dict[str, str]:
    environment = offline_environment()
    for name in tuple(environment):
        if name.lower() in {"npm_config_cache", "npm_config_registry"}:
            environment.pop(name)
    logs = temporary / "npm-logs"
    logs.mkdir()
    user_config = temporary / "npmrc"
    global_config = temporary / "global-npmrc"
    user_config.write_text("", encoding="utf-8")
    global_config.write_text("", encoding="utf-8")
    environment.update(
        {
            "npm_config_cache": str(cache),
            "npm_config_userconfig": str(user_config),
            "npm_config_globalconfig": str(global_config),
            "npm_config_logs_dir": str(logs),
            "npm_config_audit": "false",
            "npm_config_fund": "false",
            "npm_config_update_notifier": "false",
            "npm_config_logs_max": "0",
            "npm_config_offline": "true",
            "npm_config_registry": "http://127.0.0.1:9/",
            "PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD": "1",
            "PLAYWRIGHT_BROWSERS_PATH": str(browser_cache),
        }
    )
    return environment


def prepare_node_project(root: Path, temporary: Path, npm_cache: Path, browser_cache: Path) -> tuple[Path, dict[str, str]]:
    project = temporary / "node-project"
    harness = project / "harness"
    harness.mkdir(parents=True)
    shutil.copyfile(root / "frontend/diagnostics/package.json", project / "package.json")
    shutil.copyfile(root / "frontend/diagnostics/package-lock.json", project / "package-lock.json")
    shutil.copyfile(root / "tests/perfetto/ui/playwright.config.ts", harness / "playwright.config.ts")
    shutil.copyfile(root / "tests/perfetto/ui/trace.spec.ts", harness / "trace.spec.ts")
    environment = npm_environment(temporary, npm_cache, browser_cache)
    completed = subprocess.run(
        [
            "npm",
            "ci",
            "--ignore-scripts",
            "--cache",
            str(npm_cache),
            "--prefix",
            str(project),
            "--no-audit",
            "--no-fund",
            "--offline",
        ],
        cwd=root,
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        timeout=240,
    )
    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout).decode("utf-8", "replace").strip()
        fail(f"offline npm ci failed: {detail[-4000:]}")
    compiler = regular_file(
        project / "node_modules/typescript/bin/tsc",
        "TypeScript compiler",
        parent=project,
    )
    completed = subprocess.run(
        [
            "node",
            str(compiler),
            "--noEmit",
            "--strict",
            "--target",
            "ES2022",
            "--module",
            "NodeNext",
            "--moduleResolution",
            "NodeNext",
            "--types",
            "node,@playwright/test",
            "--skipLibCheck",
            str(harness / "playwright.config.ts"),
            str(harness / "trace.spec.ts"),
        ],
        cwd=project,
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        timeout=60,
    )
    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout).decode("utf-8", "replace").strip()
        fail(f"strict TypeScript check failed: {detail[-4000:]}")
    return harness, environment


def validate_report(
    report: dict[str, Any],
    manifest: dict[str, Any],
    fixtures: list[dict[str, Any]],
    browser: dict[str, str],
) -> None:
    exact_fields(
        report,
        {"schema", "perfetto", "browser", "fixtures", "network", "pixels", "failure_detectors"},
        "UI compatibility report",
    )
    if report["schema"] != "troupe.perfetto.ui-report.v1" or report["perfetto"] != manifest["perfetto"]:
        fail("UI compatibility report identity drifted")
    if report["browser"] != browser:
        fail("UI compatibility report browser identity drifted")
    results = report["fixtures"]
    if not isinstance(results, list) or len(results) != len(fixtures):
        fail("UI compatibility report fixture cardinality drifted")
    for position, (result_raw, expected) in enumerate(zip(results, fixtures)):
        result = exact_fields(
            result_raw,
            {"name", "sha256", "counts", "required_labels", "canvases"},
            f"UI report fixture {position}",
        )
        if (
            result["name"] != expected["name"]
            or result["sha256"] != expected["sha256"]
            or result["counts"] != expected["counts"]
        ):
            fail(f"UI report fixture {expected['name']} result drifted")
        if not isinstance(result["required_labels"], list) or not isinstance(result["canvases"], list):
            fail(f"UI report fixture {expected['name']} evidence is malformed")
        if expected["name"] == "projection-flow" and len(result["canvases"]) < 2:
            fail("projection-flow report has no pixel evidence")
    network = exact_fields(
        report["network"],
        {"continued_transport", "blocked_public_origins", "synthetic_loopback_requests", "public_uploads"},
        "UI report network evidence",
    )
    if (
        network["continued_transport"] != "loopback-only"
        or network["public_uploads"] != 0
        or not isinstance(network["synthetic_loopback_requests"], int)
        or not isinstance(network["blocked_public_origins"], list)
        or not network["blocked_public_origins"]
    ):
        fail("UI report network isolation evidence is incomplete")
    for raw_origin in network["blocked_public_origins"]:
        if not isinstance(raw_origin, str):
            fail("UI report blocked origin must be a string")
        parsed = urllib.parse.urlsplit(raw_origin)
        if parsed.hostname in {"127.0.0.1", "::1", "localhost"}:
            fail("UI report classified loopback as public")
    pixels = exact_fields(report["pixels"], {"screenshot_sha256", "oracle"}, "UI pixel evidence")
    validate_hash(pixels["screenshot_sha256"], "UI screenshot hash")
    if pixels["oracle"] != "projection-flow":
        fail("UI report used the wrong pixel oracle")
    if report["failure_detectors"] != ["blank_canvas", "console_error", "load_timeout"]:
        fail("UI report failure detector set drifted")


def run_browser(
    root: Path,
    harness: Path,
    environment: dict[str, str],
    server: LoopbackServer,
    executable: Path,
    browser: dict[str, str],
    manifest_path: Path,
    oracle_path: Path,
    result_path: Path,
    output_path: Path,
) -> None:
    origin = f"http://127.0.0.1:{server.server_address[1]}"
    environment = dict(environment)
    environment.update(
        {
            "TROUPE_PERFETTO_UI_ORIGIN": origin,
            "TROUPE_PERFETTO_UI_MANIFEST": str(manifest_path),
            "TROUPE_PERFETTO_UI_PIXEL_ORACLE": str(oracle_path),
            "TROUPE_PERFETTO_UI_RESULT": str(result_path),
            "TROUPE_PERFETTO_UI_OUTPUT": str(output_path),
            "TROUPE_PERFETTO_UI_CHROMIUM": str(executable),
            "TROUPE_PERFETTO_UI_BROWSER_VERSION": browser["version"],
            "TROUPE_PERFETTO_UI_BROWSER_SHA256": browser["executable_sha256"],
        }
    )
    runner = harness.parent / "node_modules/@playwright/test/cli.js"
    regular_file(runner, "Playwright test runner", parent=harness.parent)
    completed = subprocess.run(
        ["node", str(runner), "test", "--config", str(harness / "playwright.config.ts")],
        cwd=harness,
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        timeout=600,
    )
    if completed.returncode != 0:
        output = (completed.stdout + completed.stderr).decode("utf-8", "replace").strip()
        fail(f"pinned Chromium Perfetto UI test failed: {output[-8000:]}")


def parse_arguments(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Verify Troupe traces with the pinned Perfetto UI")
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--cache")
    parser.add_argument("--browser-cache")
    options = parser.parse_args(argv)
    if not options.offline:
        parser.error("--offline is required")
    if options.cache is None:
        parser.error("--cache is required")
    if options.browser_cache is None:
        parser.error("--browser-cache is required")
    return options


def main(argv: list[str]) -> int:
    root = Path(argv[0]).resolve(strict=True)
    options = parse_arguments(argv[1:])
    manifest, _pixel, fixtures = load_inputs(root)

    perfetto_platform, browser_platform = platform_names()
    perfetto_cache = existing_directory(options.cache, "--cache", repository=root)
    browser_cache = existing_directory(options.browser_cache, "--browser-cache", repository=root)
    raw_npm_cache = os.environ.get("TROUPE_NPM_CACHE")
    if raw_npm_cache is None:
        fail("TROUPE_NPM_CACHE is required")
    npm_cache = existing_directory(raw_npm_cache, "TROUPE_NPM_CACHE", repository=root)

    ui_archive = verify_perfetto_cache(root, perfetto_cache, perfetto_platform)
    executable, browser = verify_browser_cache(root, browser_cache, browser_platform)
    validate_npm_cache(root, npm_cache)

    with tempfile.TemporaryDirectory(prefix="troupe-perfetto-ui-") as raw_temporary:
        temporary = Path(raw_temporary).resolve(strict=True)
        if is_within(temporary, root):
            fail("Perfetto UI temporary directory must remain outside the repository")
        ui_root = temporary / "official-ui"
        ui_root.mkdir()
        extract_ui(ui_archive, ui_root)
        harness, environment = prepare_node_project(root, temporary, npm_cache, browser_cache)
        result_path = temporary / "ui-report.json"
        output_path = temporary / "playwright-output"
        traces = {fixture["name"]: fixture["payload"] for fixture in fixtures}
        server = LoopbackServer(ui_root, traces)
        thread = threading.Thread(target=server.serve_forever, name="perfetto-ui-loopback", daemon=True)
        thread.start()
        try:
            if not thread.is_alive():
                fail("Perfetto UI loopback server failed to start")
            run_browser(
                root,
                harness,
                environment,
                server,
                executable,
                browser,
                root / "tests/perfetto/ui/fixtures.manifest.json",
                root / "tests/perfetto/ui/pixel-oracle.json",
                result_path,
                output_path,
            )
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)
        if thread.is_alive():
            fail("Perfetto UI loopback server did not stop")
        if not server.requests or any(not request.startswith(("GET ", "HEAD ")) for request in server.requests):
            fail("Perfetto UI loopback server request audit failed")
        report = load_json(
            regular_file(result_path, "Perfetto UI browser report", parent=temporary),
            "Perfetto UI browser report",
        )
        validate_report(report, manifest, fixtures, browser)

    print(json.dumps(report, ensure_ascii=True, separators=(",", ":"), sort_keys=True))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except (CompatibilityError, OSError, subprocess.SubprocessError) as error:
        print(f"Perfetto UI compatibility: {error}", file=sys.stderr)
        raise SystemExit(1)
PY
