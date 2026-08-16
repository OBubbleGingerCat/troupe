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


SHA256_PATTERN: Final = re.compile(r"[0-9a-f]{64}\Z")
LAYER_ORDER: Final = ("decode", "sql", "ui")
LAYER_SCRIPTS: Final = {
    "decode": "scripts/test_perfetto_decode_compatibility.sh",
    "sql": "scripts/test_perfetto_sql_compatibility.sh",
    "ui": "scripts/test_perfetto_ui_compatibility.sh",
}
LAYER_TIMEOUT_SECONDS: Final = {"decode": 60, "sql": 180, "ui": 600}
PERFETTO_IDENTITY: Final = ".troupe-perfetto-cache.json"
BROWSER_IDENTITY: Final = ".troupe-playwright-cache.json"


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


def regular_file(path: Path, label: str, *, parent: Path) -> Path:
    try:
        metadata = path.lstat()
        resolved = path.resolve(strict=True)
    except OSError as error:
        fail(f"{label} must be an existing regular file: {error}")
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
        fail(f"{label} must be a regular file without symlink indirection")
    if resolved != path or not is_within(path, parent):
        fail(f"{label} must remain at its exact declared path")
    return path


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
    if resolved != candidate or is_within(candidate, repository):
        fail(f"{label} must be an exact repository-external directory")
    return candidate


def read_bytes(path: Path, label: str) -> bytes:
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


def load_json(path: Path, label: str, *, parent: Path) -> tuple[dict[str, Any], bytes]:
    path = regular_file(path, label, parent=parent)
    payload = read_bytes(path, label)
    try:
        value = json.loads(payload.decode("utf-8"), object_pairs_hook=pairs_object)
    except CompatibilityError:
        raise
    except (UnicodeError, json.JSONDecodeError) as error:
        fail(f"{label} is not valid UTF-8 JSON: {error}")
    if not isinstance(value, dict):
        fail(f"{label} must contain an object")
    return value, payload


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def hash_value(value: Any, label: str) -> str:
    if not isinstance(value, str) or SHA256_PATTERN.fullmatch(value) is None:
        fail(f"{label} must be a lowercase SHA-256")
    return value


def canonical_basename(value: Any, label: str) -> str:
    if not isinstance(value, str):
        fail(f"{label} must be a string")
    path = PurePosixPath(value)
    if not value or path.is_absolute() or len(path.parts) != 1 or path.name != value:
        fail(f"{label} must be a canonical basename")
    return value


def fixture_identity(root: Path) -> dict[str, Any]:
    manifest, payload = load_json(
        root / "tests/fixtures/perfetto/traces/manifest.json",
        "T03 trace manifest",
        parent=root,
    )
    exact_fields(manifest, {"schema", "files"}, "T03 trace manifest")
    if manifest["schema"] != "troupe.perfetto.trace-fixtures.v1":
        fail("T03 trace manifest schema drifted")
    files = manifest["files"]
    if not isinstance(files, list) or len(files) != 9:
        fail("T03 trace manifest must list exactly nine fixtures")
    summary: list[dict[str, str]] = []
    seen: set[str] = set()
    for position, raw in enumerate(files):
        entry = exact_fields(raw, {"path", "bytes", "sha256"}, f"T03 trace {position}")
        path = canonical_basename(entry["path"], f"T03 trace {position}.path")
        if path in seen:
            fail(f"duplicate T03 trace path: {path}")
        seen.add(path)
        if type(entry["bytes"]) is not int or entry["bytes"] < 0:
            fail(f"T03 trace {position}.bytes must be a non-negative integer")
        summary.append(
            {"path": path, "sha256": hash_value(entry["sha256"], f"T03 trace {position}.sha256")}
        )
    return {"manifest_sha256": sha256_bytes(payload), "files": summary}


def perfetto_identity(cache: Path) -> dict[str, Any]:
    identity, _payload = load_json(
        cache / PERFETTO_IDENTITY,
        "Perfetto cache identity",
        parent=cache,
    )
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
    members = identity["members"]
    if not isinstance(members, list) or len(members) != 2:
        fail("Perfetto cache identity must list exactly the tool and UI archives")
    summarized: list[dict[str, str]] = []
    for position, raw in enumerate(members):
        member = exact_fields(raw, {"kind", "cache_filename", "sha256"}, f"Perfetto member {position}")
        kind = member["kind"]
        if kind not in {"tools", "ui"}:
            fail(f"Perfetto member {position}.kind is unsupported")
        summarized.append(
            {
                "kind": kind,
                "sha256": hash_value(member["sha256"], f"Perfetto member {position}.sha256"),
            }
        )
    summarized.sort(key=lambda item: item["kind"])
    if [item["kind"] for item in summarized] != ["tools", "ui"]:
        fail("Perfetto cache identity member kinds are not exact")
    for field in ("release_tag", "release_commit", "platform"):
        if not isinstance(identity[field], str) or not identity[field]:
            fail(f"Perfetto cache identity {field} must be a non-empty string")
    return {
        "release_tag": identity["release_tag"],
        "release_commit": identity["release_commit"],
        "platform": identity["platform"],
        "manifest_sha256": hash_value(identity["manifest_sha256"], "Perfetto manifest hash"),
        "members": summarized,
    }


def browser_identity(cache: Path) -> dict[str, Any]:
    identity, _payload = load_json(
        cache / BROWSER_IDENTITY,
        "browser cache identity",
        parent=cache,
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
    core = exact_fields(
        identity["playwrightCore"],
        {"version", "integrity", "browsersSha256"},
        "Playwright core identity",
    )
    archives = identity["archives"]
    if not isinstance(archives, list):
        fail("browser cache archives must be a list")
    chromium = [entry for entry in archives if isinstance(entry, dict) and entry.get("name") == "chromium"]
    if len(chromium) != 1:
        fail("browser cache must list one full Chromium archive")
    record = chromium[0]
    required = {
        "name",
        "revision",
        "browserVersion",
        "cacheDirectory",
        "archiveSha256",
        "treeSha256",
        "memberCount",
        "executable",
        "executableSha256",
        "materializedLinks",
    }
    exact_fields(record, required, "Chromium archive identity")
    for field in ("revision", "browserVersion"):
        if not isinstance(record[field], str) or not record[field]:
            fail(f"Chromium {field} must be a non-empty string")
    if not isinstance(core["version"], str) or not core["version"]:
        fail("Playwright core version must be a non-empty string")
    return {
        "manifest_sha256": hash_value(identity["manifestSha256"], "browser manifest hash"),
        "lock_sha256": hash_value(identity["lockSha256"], "browser lock hash"),
        "playwright_core": {
            "version": core["version"],
            "browsers_sha256": hash_value(core["browsersSha256"], "Playwright browsers hash"),
        },
        "chromium": {
            "revision": record["revision"],
            "version": record["browserVersion"],
            "archive_sha256": hash_value(record["archiveSha256"], "Chromium archive hash"),
            "tree_sha256": hash_value(record["treeSha256"], "Chromium tree hash"),
            "executable_sha256": hash_value(
                record["executableSha256"],
                "Chromium executable hash",
            ),
        },
    }


def offline_environment() -> dict[str, str]:
    environment = dict(os.environ)
    environment.update(
        {
            "HTTP_PROXY": "http://127.0.0.1:9/",
            "HTTPS_PROXY": "http://127.0.0.1:9/",
            "ALL_PROXY": "http://127.0.0.1:9/",
            "NO_PROXY": "127.0.0.1,localhost,::1",
            "http_proxy": "http://127.0.0.1:9/",
            "https_proxy": "http://127.0.0.1:9/",
            "all_proxy": "http://127.0.0.1:9/",
            "no_proxy": "127.0.0.1,localhost,::1",
            "TROUPE_PERFETTO_NETWORK_FORBIDDEN": "1",
            "PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD": "1",
            "npm_config_offline": "true",
            "npm_config_registry": "http://127.0.0.1:9/",
        }
    )
    environment.pop("TROUPE_PERFETTO_TEST_TRANSPORT", None)
    return environment


def layer_command(
    root: Path,
    layer: str,
    perfetto_cache: Path | None,
    browser_cache: Path | None,
) -> list[str]:
    script = regular_file(root / LAYER_SCRIPTS[layer], f"{layer} compatibility layer", parent=root)
    if not os.access(script, os.X_OK):
        fail(f"{layer} compatibility layer is not executable")
    command = [str(script), "--offline"]
    if layer in {"sql", "ui"}:
        if perfetto_cache is None:
            fail(f"{layer} compatibility layer requires --cache")
        command.extend(("--cache", str(perfetto_cache)))
    if layer == "ui":
        if browser_cache is None:
            fail("UI compatibility layer requires --browser-cache")
        command.extend(("--browser-cache", str(browser_cache)))
    return command


def empty_layer_result(name: str, status: str) -> dict[str, Any]:
    return {
        "name": name,
        "status": status,
        "exit_code": None,
        "stdout_sha256": None,
        "stderr_sha256": None,
    }


def process_exit_code(returncode: int) -> int:
    if 0 < returncode <= 255:
        return returncode
    if -127 <= returncode < 0:
        return 128 - returncode
    return 1


def execute_layers(
    root: Path,
    selected: tuple[str, ...],
    perfetto_cache: Path | None,
    browser_cache: Path | None,
) -> tuple[list[dict[str, Any]], int, str | None, list[tuple[str, bytes, bytes]]]:
    commands = {
        layer: layer_command(root, layer, perfetto_cache, browser_cache) for layer in selected
    }
    results = {
        layer: empty_layer_result(layer, "pending" if layer in selected else "not_selected")
        for layer in LAYER_ORDER
    }
    first_exit = 0
    first_failed: str | None = None
    failed_output: list[tuple[str, bytes, bytes]] = []
    environment = offline_environment()
    for layer in selected:
        try:
            completed = subprocess.run(
                commands[layer],
                cwd=root,
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
                timeout=LAYER_TIMEOUT_SECONDS[layer],
            )
            exit_code = completed.returncode
            stdout = completed.stdout
            stderr = completed.stderr
        except subprocess.TimeoutExpired as error:
            exit_code = 124
            stdout = error.stdout or b""
            stderr = error.stderr or b""
        status = "passed" if exit_code == 0 else "failed"
        results[layer] = {
            "name": layer,
            "status": status,
            "exit_code": exit_code,
            "stdout_sha256": sha256_bytes(stdout),
            "stderr_sha256": sha256_bytes(stderr),
        }
        if exit_code != 0:
            failed_output.append((layer, stdout, stderr))
            if first_exit == 0:
                first_exit = process_exit_code(exit_code)
                first_failed = layer
    return [results[layer] for layer in LAYER_ORDER], first_exit, first_failed, failed_output


def parse_arguments(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Assemble the three pinned Perfetto compatibility layers")
    selector = parser.add_mutually_exclusive_group(required=True)
    selector.add_argument("--all-layers", action="store_true")
    selector.add_argument("--decode", action="store_true")
    selector.add_argument("--sql", action="store_true")
    selector.add_argument("--ui", action="store_true")
    selector.add_argument("--current-public-ui-canary", action="store_true")
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--cache")
    parser.add_argument("--browser-cache")
    options = parser.parse_args(argv)
    if options.current_public_ui_canary:
        if options.offline or options.cache is not None or options.browser_cache is not None:
            parser.error("--current-public-ui-canary is a separate non-release mode")
        return options
    if not options.offline:
        parser.error("--offline is required")
    if options.decode and (options.cache is not None or options.browser_cache is not None):
        parser.error("--decode does not accept cache arguments")
    if options.sql and options.cache is None:
        parser.error("--sql requires --cache")
    if options.sql and options.browser_cache is not None:
        parser.error("--sql does not accept --browser-cache")
    if (options.ui or options.all_layers) and options.cache is None:
        parser.error("selected layers require --cache")
    if (options.ui or options.all_layers) and options.browser_cache is None:
        parser.error("selected layers require --browser-cache")
    return options


def run_canary(root: Path) -> int:
    fetcher = regular_file(
        root / "scripts/fetch_pinned_perfetto_tools.sh",
        "current public UI canary",
        parent=root,
    )
    if not os.access(fetcher, os.X_OK):
        fail("current public UI canary is not executable")
    return subprocess.run(
        [str(fetcher), "--current-public-ui-canary"],
        cwd=root,
        env=dict(os.environ),
        check=False,
    ).returncode


def main(argv: list[str]) -> int:
    root = Path(argv[0]).resolve(strict=True)
    options = parse_arguments(argv[1:])
    if options.current_public_ui_canary:
        return run_canary(root)

    if options.all_layers:
        mode = "all-layers"
        selected = LAYER_ORDER
    elif options.decode:
        mode = "decode"
        selected = ("decode",)
    elif options.sql:
        mode = "sql"
        selected = ("sql",)
    else:
        mode = "ui"
        selected = ("ui",)

    perfetto_cache = (
        None
        if options.cache is None
        else existing_directory(options.cache, "--cache", repository=root)
    )
    browser_cache = (
        None
        if options.browser_cache is None
        else existing_directory(options.browser_cache, "--browser-cache", repository=root)
    )
    fixtures = fixture_identity(root)
    perfetto = None if perfetto_cache is None else perfetto_identity(perfetto_cache)
    browser = None if browser_cache is None else browser_identity(browser_cache)
    layers, exit_code, first_failed, failed_output = execute_layers(
        root,
        selected,
        perfetto_cache,
        browser_cache,
    )
    if exit_code == 0:
        if perfetto_cache is not None and perfetto_identity(perfetto_cache) != perfetto:
            fail("Perfetto cache identity changed while compatibility layers ran")
        if browser_cache is not None and browser_identity(browser_cache) != browser:
            fail("browser cache identity changed while compatibility layers ran")
    summary = {
        "schema": "troupe.perfetto.compatibility.v1",
        "mode": mode,
        "offline": True,
        "result": "passed" if exit_code == 0 else "failed",
        "first_failed_layer": first_failed,
        "fixtures": fixtures,
        "perfetto": perfetto,
        "browser": browser,
        "layers": layers,
    }
    print(json.dumps(summary, ensure_ascii=True, separators=(",", ":"), sort_keys=True))
    for layer, stdout, stderr in failed_output:
        detail = (stdout + stderr).decode("utf-8", "replace").strip()
        if detail:
            print(f"Perfetto {layer} layer failed: {detail[-4000:]}", file=sys.stderr)
    return exit_code


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except (CompatibilityError, OSError, subprocess.SubprocessError) as error:
        print(f"Perfetto compatibility: {error}", file=sys.stderr)
        raise SystemExit(1)
PY
