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
import re
import stat
import subprocess
import sys
from pathlib import Path, PurePosixPath
from typing import Any, Final


QUALITY_FIXTURE: Final = "tests/fixtures/release/perfetto-quality.json"
PERFETTO_IDENTITY: Final = ".troupe-perfetto-cache.json"
BROWSER_IDENTITY: Final = ".troupe-playwright-cache.json"
INSTALLATION_MARKER: Final = "INSTALLATION_COMPLETE"
LAYER_ORDER: Final = ("decode", "sql", "ui")
SHA256_RE: Final = re.compile(r"[0-9a-f]{64}\Z")
OFFLINE_PROXY: Final = "http://127.0.0.1:9/"


class ReleaseError(RuntimeError):
    pass


def fail(message: str) -> None:
    raise ReleaseError(message)


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


def hash_value(value: Any, label: str) -> str:
    if not isinstance(value, str) or SHA256_RE.fullmatch(value) is None:
        fail(f"{label} must be a lowercase SHA-256")
    return value


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def is_within(candidate: Path, parent: Path) -> bool:
    try:
        candidate.relative_to(parent)
    except ValueError:
        return False
    return True


def regular_file(
    path: Path,
    label: str,
    *,
    parent: Path,
    readonly: bool = False,
) -> Path:
    try:
        metadata = path.lstat()
        resolved = path.resolve(strict=True)
    except OSError as error:
        fail(f"{label} must be an existing regular file: {error}")
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
        fail(f"{label} must be a regular file without symlink indirection")
    if resolved != path or not is_within(path, parent):
        fail(f"{label} must remain at its exact declared path")
    if readonly and metadata.st_mode & 0o222:
        fail(f"{label} must be read-only")
    return path


def read_bytes(path: Path, label: str) -> bytes:
    before = path.lstat()
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    try:
        opened = os.fstat(descriptor)
        if not stat.S_ISREG(opened.st_mode) or (opened.st_dev, opened.st_ino) != (
            before.st_dev,
            before.st_ino,
        ):
            fail(f"{label} changed while it was opened")
        digest_input: list[bytes] = []
        while True:
            chunk = os.read(descriptor, 1024 * 1024)
            if not chunk:
                return b"".join(digest_input)
            digest_input.append(chunk)
    finally:
        os.close(descriptor)


def sha256_file(path: Path, label: str) -> str:
    before = path.lstat()
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    digest = hashlib.sha256()
    try:
        opened = os.fstat(descriptor)
        if not stat.S_ISREG(opened.st_mode) or (opened.st_dev, opened.st_ino) != (
            before.st_dev,
            before.st_ino,
        ):
            fail(f"{label} changed while it was opened")
        while True:
            chunk = os.read(descriptor, 1024 * 1024)
            if not chunk:
                return digest.hexdigest()
            digest.update(chunk)
    finally:
        os.close(descriptor)


def load_json(path: Path, label: str, *, parent: Path, readonly: bool = False) -> tuple[dict[str, Any], bytes]:
    path = regular_file(path, label, parent=parent, readonly=readonly)
    payload = read_bytes(path, label)
    try:
        value = json.loads(payload.decode("utf-8"), object_pairs_hook=pairs_object)
    except ReleaseError:
        raise
    except (UnicodeError, json.JSONDecodeError) as error:
        fail(f"{label} is not valid UTF-8 JSON: {error}")
    if not isinstance(value, dict):
        fail(f"{label} must contain an object")
    return value, payload


def repository_path(root: Path, value: Any, expected: str, label: str) -> Path:
    if value != expected:
        fail(f"{label} path must be {expected}")
    parsed = PurePosixPath(expected)
    if parsed.is_absolute() or any(part in {"", ".", ".."} for part in parsed.parts):
        fail(f"{label} path is not canonical")
    path = root.joinpath(*parsed.parts)
    try:
        resolved = path.resolve(strict=True)
    except OSError as error:
        fail(f"{label} is unavailable: {error}")
    if resolved != path or not is_within(path, root):
        fail(f"{label} escapes the repository")
    return path


def cache_directory(raw: str, label: str, *, root: Path) -> Path:
    path = Path(raw)
    if not path.is_absolute() or str(path) != raw:
        fail(f"{label} must be a canonical absolute path")
    try:
        metadata = path.lstat()
        resolved = path.resolve(strict=True)
    except OSError as error:
        fail(f"{label} must be an existing directory: {error}")
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
        fail(f"{label} must be a real directory without symlink indirection")
    if resolved != path or is_within(path, root):
        fail(f"{label} must be an exact repository-external directory")
    if metadata.st_mode & 0o222:
        fail(f"{label} must be read-only")
    return path


def require_readonly_tree(root: Path, label: str) -> None:
    metadata = root.lstat()
    if (
        not stat.S_ISDIR(metadata.st_mode)
        or stat.S_ISLNK(metadata.st_mode)
        or root.resolve(strict=True) != root
        or metadata.st_mode & 0o222
    ):
        fail(f"{label} root must remain an exact read-only directory")
    for current, directories, files in os.walk(root, topdown=True, followlinks=False):
        current_path = Path(current)
        for name in [*directories, *files]:
            path = current_path / name
            metadata = path.lstat()
            if stat.S_ISLNK(metadata.st_mode):
                fail(f"{label} must not contain symlinks: {path.relative_to(root)}")
            if not (stat.S_ISDIR(metadata.st_mode) or stat.S_ISREG(metadata.st_mode)):
                fail(f"{label} contains a special member: {path.relative_to(root)}")
            if metadata.st_mode & 0o222:
                fail(f"{label} contains a writable member: {path.relative_to(root)}")


def source_inputs(root: Path) -> tuple[dict[str, Any], dict[str, Any], Path]:
    quality, _quality_payload = load_json(
        root / QUALITY_FIXTURE,
        "Perfetto release quality fixture",
        parent=root,
    )
    exact_fields(quality, {"schema", "compatibility", "perfetto", "playwright"}, "quality fixture")
    if quality["schema"] != "troupe.diagnostics.perfetto-quality.v1":
        fail("Perfetto release quality fixture schema is unsupported")

    compatibility = exact_fields(
        quality["compatibility"], {"path", "sha256"}, "compatibility fixture"
    )
    compatibility_path = repository_path(
        root,
        compatibility["path"],
        "scripts/test_perfetto_compatibility.sh",
        "compatibility script",
    )
    regular_file(compatibility_path, "compatibility script", parent=root)
    if not os.access(compatibility_path, os.X_OK):
        fail("compatibility script must be executable")
    if sha256_file(compatibility_path, "compatibility script") != hash_value(
        compatibility["sha256"], "compatibility script hash"
    ):
        fail("compatibility script hash differs from the release fixture")

    perfetto_quality = exact_fields(
        quality["perfetto"], {"manifest", "manifest_sha256", "platform"}, "Perfetto fixture"
    )
    perfetto_manifest_path = repository_path(
        root,
        perfetto_quality["manifest"],
        "tests/perfetto/tools/manifest.json",
        "Perfetto source manifest",
    )
    perfetto_manifest, perfetto_payload = load_json(
        perfetto_manifest_path, "Perfetto source manifest", parent=root
    )
    if sha256_bytes(perfetto_payload) != hash_value(
        perfetto_quality["manifest_sha256"], "Perfetto source manifest hash"
    ):
        fail("Perfetto source manifest hash differs from the release fixture")
    exact_fields(perfetto_manifest, {"schema_version", "release", "assets"}, "Perfetto source manifest")
    if perfetto_manifest["schema_version"] != 1:
        fail("Perfetto source manifest schema is unsupported")
    release = exact_fields(
        perfetto_manifest["release"], {"tag", "commit", "release_page"}, "Perfetto release"
    )
    if not all(isinstance(release[field], str) and release[field] for field in release):
        fail("Perfetto release identity must contain non-empty strings")
    platform = perfetto_quality["platform"]
    if not isinstance(platform, str) or not platform:
        fail("Perfetto fixture platform must be a non-empty string")
    selected: list[dict[str, Any]] = []
    assets = perfetto_manifest["assets"]
    if not isinstance(assets, list):
        fail("Perfetto source manifest assets must be a list")
    for position, raw in enumerate(assets):
        asset = exact_fields(
            raw,
            {"kind", "platform", "url", "sha256", "cache_filename"},
            f"Perfetto asset {position}",
        )
        if (asset["kind"], asset["platform"]) in {("tools", platform), ("ui", "all")}:
            if (
                not isinstance(asset["cache_filename"], str)
                or PurePosixPath(asset["cache_filename"]).name != asset["cache_filename"]
            ):
                fail(f"Perfetto asset {position} cache filename is unsafe")
            hash_value(asset["sha256"], f"Perfetto asset {position} hash")
            selected.append(asset)
    if [(asset["kind"], asset["platform"]) for asset in selected] != [
        ("tools", platform),
        ("ui", "all"),
    ]:
        fail("Perfetto source manifest has no exact tools/UI pair")
    expected_perfetto = {
        "schema_version": 1,
        "release_tag": release["tag"],
        "release_commit": release["commit"],
        "platform": platform,
        "manifest_sha256": perfetto_quality["manifest_sha256"],
        "members": [
            {
                "kind": asset["kind"],
                "cache_filename": asset["cache_filename"],
                "sha256": asset["sha256"],
            }
            for asset in selected
        ],
        "test_only": False,
        "test_manifest": None,
    }

    playwright_quality = exact_fields(
        quality["playwright"],
        {"manifest", "manifest_sha256", "lock", "lock_sha256", "platform"},
        "Playwright fixture",
    )
    browser_manifest_path = repository_path(
        root,
        playwright_quality["manifest"],
        "frontend/diagnostics/tests/tooling/playwright-browsers.json",
        "Playwright source manifest",
    )
    browser_manifest, browser_payload = load_json(
        browser_manifest_path, "Playwright source manifest", parent=root
    )
    if sha256_bytes(browser_payload) != hash_value(
        playwright_quality["manifest_sha256"], "Playwright source manifest hash"
    ):
        fail("Playwright source manifest hash differs from the release fixture")
    lock_path = repository_path(
        root,
        playwright_quality["lock"],
        "frontend/diagnostics/package-lock.json",
        "frontend package lock",
    )
    regular_file(lock_path, "frontend package lock", parent=root)
    lock_hash = sha256_bytes(read_bytes(lock_path, "frontend package lock"))
    if lock_hash != hash_value(playwright_quality["lock_sha256"], "frontend lock hash"):
        fail("frontend package lock hash differs from the release fixture")
    exact_fields(
        browser_manifest,
        {"schemaVersion", "lockSha256", "playwrightCore", "platforms"},
        "Playwright source manifest",
    )
    if browser_manifest["schemaVersion"] != 1 or browser_manifest["lockSha256"] != lock_hash:
        fail("Playwright source manifest schema/lock identity is unsupported")
    core = exact_fields(
        browser_manifest["playwrightCore"],
        {"version", "integrity", "browsersSha256"},
        "Playwright core identity",
    )
    hash_value(core["browsersSha256"], "Playwright browsers hash")
    browser_platform = playwright_quality["platform"]
    platforms = browser_manifest["platforms"]
    if not isinstance(platforms, dict) or browser_platform not in platforms:
        fail("Playwright source manifest has no selected platform")
    selected_platform = exact_fields(
        platforms[browser_platform], {"playwrightPlatform", "archives"}, "Playwright platform"
    )
    raw_archives = selected_platform["archives"]
    if not isinstance(raw_archives, list) or not raw_archives:
        fail("Playwright platform archives must be a non-empty list")
    archives: list[dict[str, Any]] = []
    seen_names: set[str] = set()
    for position, raw in enumerate(raw_archives):
        archive = exact_fields(
            raw,
            {
                "name",
                "revision",
                "browserVersion",
                "cacheDirectory",
                "url",
                "archiveSha256",
                "treeSha256",
                "memberCount",
                "executable",
                "executableSha256",
                "materializedLinks",
            },
            f"Playwright archive {position}",
        )
        name = archive["name"]
        if not isinstance(name, str) or not name or name in seen_names:
            fail(f"Playwright archive {position} name is invalid or duplicated")
        seen_names.add(name)
        for field in ("archiveSha256", "treeSha256", "executableSha256"):
            hash_value(archive[field], f"Playwright archive {position} {field}")
        for field in ("cacheDirectory", "executable"):
            value = archive[field]
            parsed = PurePosixPath(value) if isinstance(value, str) else PurePosixPath("/")
            if not isinstance(value, str) or parsed.is_absolute() or any(
                part in {"", ".", ".."} for part in parsed.parts
            ):
                fail(f"Playwright archive {position} {field} is unsafe")
        links = archive["materializedLinks"]
        if not isinstance(links, list):
            fail(f"Playwright archive {position} materializedLinks must be a list")
        for link_position, link in enumerate(links):
            exact_fields(link, {"path", "target"}, f"Playwright link {position}/{link_position}")
        archives.append({key: value for key, value in archive.items() if key != "url"})
    expected_browser = {
        "schemaVersion": 1,
        "manifestSha256": playwright_quality["manifest_sha256"],
        "lockSha256": lock_hash,
        "platform": browser_platform,
        "playwrightPlatform": selected_platform["playwrightPlatform"],
        "playwrightCore": core,
        "archives": archives,
    }
    return expected_perfetto, expected_browser, compatibility_path


def perfetto_snapshot(cache: Path, expected: dict[str, Any]) -> dict[str, Any]:
    require_readonly_tree(cache, "Perfetto cache")
    identity, payload = load_json(
        cache / PERFETTO_IDENTITY,
        "Perfetto cache identity",
        parent=cache,
        readonly=True,
    )
    if identity != expected:
        fail("Perfetto cache identity does not match the frozen release manifest")
    expected_names = {PERFETTO_IDENTITY, *(member["cache_filename"] for member in expected["members"])}
    if {entry.name for entry in cache.iterdir()} != expected_names:
        fail("Perfetto cache member inventory is not exact")
    members: list[dict[str, str]] = []
    for member in expected["members"]:
        path = regular_file(
            cache / member["cache_filename"],
            f"Perfetto {member['kind']} archive",
            parent=cache,
            readonly=True,
        )
        digest = sha256_file(path, f"Perfetto {member['kind']} archive")
        if digest != member["sha256"]:
            fail(f"Perfetto {member['kind']} archive hash differs from the release manifest")
        members.append({"kind": member["kind"], "sha256": digest})
    metadata = cache.lstat()
    return {
        "realpath": str(cache),
        "device": metadata.st_dev,
        "inode": metadata.st_ino,
        "identity_sha256": sha256_bytes(payload),
        "release_tag": expected["release_tag"],
        "release_commit": expected["release_commit"],
        "platform": expected["platform"],
        "manifest_sha256": expected["manifest_sha256"],
        "members": members,
    }


def browser_snapshot(cache: Path, expected: dict[str, Any]) -> dict[str, Any]:
    require_readonly_tree(cache, "Playwright cache")
    identity, payload = load_json(
        cache / BROWSER_IDENTITY,
        "Playwright cache identity",
        parent=cache,
        readonly=True,
    )
    if identity != expected:
        fail("Playwright cache identity does not match the frozen release manifest")
    expected_names = {BROWSER_IDENTITY, *(archive["cacheDirectory"] for archive in expected["archives"])}
    if {entry.name for entry in cache.iterdir()} != expected_names:
        fail("Playwright cache member inventory is not exact")
    archives: list[dict[str, Any]] = []
    for archive in expected["archives"]:
        directory = cache / archive["cacheDirectory"]
        try:
            metadata = directory.lstat()
            resolved = directory.resolve(strict=True)
        except OSError as error:
            fail(f"Playwright {archive['name']} cache directory is missing: {error}")
        if not stat.S_ISDIR(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode) or resolved != directory:
            fail(f"Playwright {archive['name']} cache directory is not exact")
        regular_file(
            directory / INSTALLATION_MARKER,
            f"Playwright {archive['name']} installation marker",
            parent=directory,
            readonly=True,
        )
        executable = regular_file(
            directory.joinpath(*PurePosixPath(archive["executable"]).parts),
            f"Playwright {archive['name']} executable",
            parent=directory,
            readonly=True,
        )
        digest = sha256_file(executable, f"Playwright {archive['name']} executable")
        if digest != archive["executableSha256"]:
            fail(f"Playwright {archive['name']} executable hash differs from the release manifest")
        archives.append(
            {
                "name": archive["name"],
                "revision": archive["revision"],
                "browser_version": archive["browserVersion"],
                "archive_sha256": archive["archiveSha256"],
                "tree_sha256": archive["treeSha256"],
                "executable_sha256": digest,
            }
        )
    metadata = cache.lstat()
    return {
        "realpath": str(cache),
        "device": metadata.st_dev,
        "inode": metadata.st_ino,
        "identity_sha256": sha256_bytes(payload),
        "manifest_sha256": expected["manifestSha256"],
        "lock_sha256": expected["lockSha256"],
        "platform": expected["platform"],
        "playwright_core": expected["playwrightCore"]["version"],
        "archives": archives,
    }


def t02_perfetto_identity(expected: dict[str, Any]) -> dict[str, Any]:
    return {
        "release_tag": expected["release_tag"],
        "release_commit": expected["release_commit"],
        "platform": expected["platform"],
        "manifest_sha256": expected["manifest_sha256"],
        "members": sorted(
            ({"kind": member["kind"], "sha256": member["sha256"]} for member in expected["members"]),
            key=lambda member: member["kind"],
        ),
    }


def t02_browser_identity(expected: dict[str, Any]) -> dict[str, Any]:
    chromium = [archive for archive in expected["archives"] if archive["name"] == "chromium"]
    if len(chromium) != 1:
        fail("Playwright release manifest must contain exactly one Chromium archive")
    archive = chromium[0]
    return {
        "manifest_sha256": expected["manifestSha256"],
        "lock_sha256": expected["lockSha256"],
        "playwright_core": {
            "version": expected["playwrightCore"]["version"],
            "browsers_sha256": expected["playwrightCore"]["browsersSha256"],
        },
        "chromium": {
            "revision": archive["revision"],
            "version": archive["browserVersion"],
            "archive_sha256": archive["archiveSha256"],
            "tree_sha256": archive["treeSha256"],
            "executable_sha256": archive["executableSha256"],
        },
    }


def offline_environment() -> dict[str, str]:
    environment = dict(os.environ)
    environment.update(
        {
            "HTTP_PROXY": OFFLINE_PROXY,
            "HTTPS_PROXY": OFFLINE_PROXY,
            "ALL_PROXY": OFFLINE_PROXY,
            "NO_PROXY": "127.0.0.1,localhost,::1",
            "http_proxy": OFFLINE_PROXY,
            "https_proxy": OFFLINE_PROXY,
            "all_proxy": OFFLINE_PROXY,
            "no_proxy": "127.0.0.1,localhost,::1",
            "TROUPE_PERFETTO_NETWORK_FORBIDDEN": "1",
            "PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD": "1",
            "npm_config_offline": "true",
            "npm_config_registry": OFFLINE_PROXY,
        }
    )
    environment.pop("TROUPE_PERFETTO_TEST_TRANSPORT", None)
    environment.pop("TROUPE_PLAYWRIGHT_TEST_TRANSPORT", None)
    return environment


def mapped_exit(returncode: int) -> int:
    if 0 <= returncode <= 255:
        return returncode
    if -127 <= returncode < 0:
        return 128 - returncode
    return 1


def compatibility_summary(
    stdout: bytes,
    returncode: int,
    expected_perfetto: dict[str, Any],
    expected_browser: dict[str, Any],
) -> dict[str, Any]:
    lines = [line for line in stdout.splitlines() if line.strip()]
    if len(lines) != 1:
        fail("T02 compatibility output must contain exactly one JSON summary")
    try:
        value = json.loads(lines[0].decode("utf-8"), object_pairs_hook=pairs_object)
    except ReleaseError:
        raise
    except (UnicodeError, json.JSONDecodeError) as error:
        fail(f"T02 compatibility summary is invalid: {error}")
    summary = exact_fields(
        value,
        {
            "schema",
            "mode",
            "offline",
            "result",
            "first_failed_layer",
            "fixtures",
            "perfetto",
            "browser",
            "layers",
        },
        "T02 compatibility summary",
    )
    if (
        summary["schema"] != "troupe.perfetto.compatibility.v1"
        or summary["mode"] != "all-layers"
        or summary["offline"] is not True
    ):
        fail("T02 compatibility summary is not the frozen all-layer offline mode")
    if summary["perfetto"] != t02_perfetto_identity(expected_perfetto):
        fail("T02 Perfetto identity differs from the release cache")
    if summary["browser"] != t02_browser_identity(expected_browser):
        fail("T02 browser identity differs from the release cache")
    fixtures = exact_fields(summary["fixtures"], {"manifest_sha256", "files"}, "T02 fixtures")
    hash_value(fixtures["manifest_sha256"], "T02 fixture manifest hash")
    if not isinstance(fixtures["files"], list) or not fixtures["files"]:
        fail("T02 fixture summary must contain files")
    for position, entry in enumerate(fixtures["files"]):
        exact_fields(entry, {"path", "sha256"}, f"T02 fixture {position}")
        hash_value(entry["sha256"], f"T02 fixture {position} hash")
    layers = summary["layers"]
    if not isinstance(layers, list) or len(layers) != len(LAYER_ORDER):
        fail("T02 compatibility summary must contain all three layers")
    failed: list[str] = []
    for position, (name, raw) in enumerate(zip(LAYER_ORDER, layers)):
        layer = exact_fields(
            raw,
            {"name", "status", "exit_code", "stdout_sha256", "stderr_sha256"},
            f"T02 layer {position}",
        )
        if layer["name"] != name or layer["status"] not in {"passed", "failed"}:
            fail(f"T02 layer {position} identity/status is invalid")
        if type(layer["exit_code"]) is not int:
            fail(f"T02 layer {position} exit code must be an integer")
        hash_value(layer["stdout_sha256"], f"T02 layer {position} stdout hash")
        hash_value(layer["stderr_sha256"], f"T02 layer {position} stderr hash")
        if (layer["exit_code"] == 0) != (layer["status"] == "passed"):
            fail(f"T02 layer {position} status does not match its exit code")
        if layer["status"] == "failed":
            failed.append(name)
    expected_result = "passed" if not failed else "failed"
    expected_first = None if not failed else failed[0]
    if summary["result"] != expected_result or summary["first_failed_layer"] != expected_first:
        fail("T02 aggregate result does not match its layer results")
    if (returncode == 0) != (expected_result == "passed"):
        fail("T02 process exit does not match its compatibility result")
    return summary


def parse_arguments(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run the frozen Perfetto release compatibility gate")
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--all-layers", action="store_true")
    parser.add_argument("--perfetto-cache")
    parser.add_argument("--browser-cache")
    options = parser.parse_args(argv)
    if not options.offline:
        parser.error("--offline is required")
    if not options.all_layers:
        parser.error("--all-layers is required")
    if options.perfetto_cache is None:
        parser.error("--perfetto-cache is required")
    if options.browser_cache is None:
        parser.error("--browser-cache is required")
    return options


def main(argv: list[str]) -> int:
    root = Path(argv[0]).resolve(strict=True)
    options = parse_arguments(argv[1:])
    expected_perfetto, expected_browser, compatibility = source_inputs(root)
    perfetto_cache = cache_directory(options.perfetto_cache, "--perfetto-cache", root=root)
    browser_cache = cache_directory(options.browser_cache, "--browser-cache", root=root)
    before = {
        "perfetto": perfetto_snapshot(perfetto_cache, expected_perfetto),
        "playwright": browser_snapshot(browser_cache, expected_browser),
    }
    command = [
        str(compatibility),
        "--offline",
        "--all-layers",
        "--cache",
        str(perfetto_cache),
        "--browser-cache",
        str(browser_cache),
    ]
    try:
        completed = subprocess.run(
            command,
            cwd=root,
            env=offline_environment(),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=1200,
        )
    except subprocess.TimeoutExpired as error:
        detail = ((error.stdout or b"") + (error.stderr or b"")).decode("utf-8", "replace")
        fail(f"T02 compatibility wrapper timed out: {detail[-4000:]}")
    summary = compatibility_summary(
        completed.stdout,
        completed.returncode,
        expected_perfetto,
        expected_browser,
    )
    after = {
        "perfetto": perfetto_snapshot(perfetto_cache, expected_perfetto),
        "playwright": browser_snapshot(browser_cache, expected_browser),
    }
    if after != before:
        fail("Perfetto or Playwright cache identity changed during the release gate")
    result = {
        "schema": "troupe.diagnostics.perfetto-release.v1",
        "offline": True,
        "result": summary["result"],
        "cache": after,
        "compatibility": summary,
    }
    print(json.dumps(result, ensure_ascii=True, separators=(",", ":"), sort_keys=True))
    if completed.stderr:
        sys.stderr.buffer.write(completed.stderr)
    return mapped_exit(completed.returncode)


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except (ReleaseError, OSError, subprocess.SubprocessError) as error:
        print(f"Perfetto release compatibility: {error}", file=sys.stderr)
        raise SystemExit(1)
PY
