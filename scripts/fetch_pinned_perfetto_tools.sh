#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)"

if command -v python3 >/dev/null 2>&1; then
  python_command=python3
else
  python_command=python
fi

exec "$python_command" - "$repository_root" "$@" <<'PY'
from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform as host_platform
import stat
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Final
from urllib.parse import urlsplit


RELEASE_TAG: Final = "v57.2"
RELEASE_COMMIT: Final = "da1d152cff27890903d158fe96751de3aab883cc"
RELEASE_PAGE: Final = "https://github.com/google/perfetto/releases/tag/v57.2"
ASSET_BASE: Final = "https://github.com/google/perfetto/releases/download/v57.2"
CANARY_URL: Final = "https://ui.perfetto.dev/"
IDENTITY_NAME: Final = ".troupe-perfetto-cache.json"
SHA256_PATTERN: Final = __import__("re").compile(r"[0-9a-f]{64}\Z")
OFFICIAL_ASSETS: Final = (
    (
        "tools",
        "linux-amd64",
        "linux-amd64.zip",
        "a5354a4a133cc629bb398da53c95515e5a49d4bd96edfebe1ebc3221c85d936f",
    ),
    (
        "tools",
        "linux-arm",
        "linux-arm.zip",
        "1ba33c50a29fa1b9f9472747ee00b274e9c4f28883ce42de86debf4c48bdb3e4",
    ),
    (
        "tools",
        "linux-arm64",
        "linux-arm64.zip",
        "1a15f63477c03984f8117929484cf599ad8410e0b638f23d2ac1b023679ca10e",
    ),
    (
        "tools",
        "mac-amd64",
        "mac-amd64.zip",
        "8d56edbd061a947ec4a63b2b1b396a9beeccac2bc7b0c33e10240cc1d6bce32f",
    ),
    (
        "tools",
        "mac-arm64",
        "mac-arm64.zip",
        "f0f282ef199a2942ee5286856cd57260b11e93f95fdd80e3ffafe2f56ed936de",
    ),
    (
        "tools",
        "windows-amd64",
        "windows-amd64.zip",
        "0d47a31f9058cae5442baeab1ffce3f3f75e176f4f7cd8fedb1a29a51955975e",
    ),
    (
        "ui",
        "all",
        "perfetto-ui.zip",
        "3d4043c4451faaddec8f382b1a0efae4c33ca9b168ec037da32e53c1cc308408",
    ),
)
PLATFORMS: Final = tuple(
    item[1] for item in OFFICIAL_ASSETS if item[1] != "all"
)


class FetchError(RuntimeError):
    pass


def fail(message: str) -> None:
    raise FetchError(message)


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


def existing_real_directory(raw: str, label: str) -> Path:
    candidate = Path(raw)
    if not candidate.is_absolute():
        fail(f"{label} must be an absolute path")
    if str(candidate) != raw:
        fail(f"{label} must be a canonical absolute path")
    try:
        metadata = candidate.lstat()
        resolved = candidate.resolve(strict=True)
    except OSError as error:
        fail(f"{label} must be an existing directory: {error}")
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
        fail(f"{label} must be a real directory, not a symlink")
    if resolved != candidate:
        fail(f"{label} must be its exact real path without symlink indirection")
    owned(metadata, label)
    return candidate


def regular_path(raw: str, label: str, *, base: Path | None = None) -> Path:
    candidate = Path(raw)
    if not candidate.is_absolute():
        candidate = (base or Path.cwd()) / candidate
    candidate = Path(os.path.abspath(candidate))
    try:
        metadata = candidate.lstat()
        resolved = candidate.resolve(strict=True)
    except OSError as error:
        fail(f"{label} must be an existing regular file: {error}")
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
        fail(f"{label} must be a regular file without symlink indirection")
    if resolved != candidate:
        fail(f"{label} must be its exact real path without symlink indirection")
    owned(metadata, label)
    return candidate


def read_regular_bytes(path: Path, label: str) -> bytes:
    metadata = path.lstat()
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
        fail(f"{label} must be a regular file without symlink indirection")
    if path.resolve(strict=True) != path:
        fail(f"{label} must be its exact real path")
    owned(metadata, label)
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
                break
            chunks.append(chunk)
        return b"".join(chunks)
    finally:
        os.close(descriptor)


def read_json(path: Path, label: str) -> tuple[bytes, dict[str, Any]]:
    try:
        raw = read_regular_bytes(path, label)
        value = json.loads(raw.decode("utf-8"), object_pairs_hook=pairs_object)
    except FetchError:
        raise
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        fail(f"{label} is not valid UTF-8 JSON: {error}")
    if not isinstance(value, dict):
        fail(f"{label} must contain a JSON object")
    return raw, value


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def sha256_file(path: Path, label: str, *, require_readonly: bool) -> str | None:
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        return None
    except OSError as error:
        fail(f"could not inspect {label}: {error}")
    if stat.S_ISLNK(metadata.st_mode):
        fail(f"{label} must not be a symlink")
    if not stat.S_ISREG(metadata.st_mode):
        fail(f"{label} must be a regular file")
    if path.resolve(strict=True) != path:
        fail(f"{label} escapes the cache through symlink indirection")
    owned(metadata, label)
    if require_readonly and stat.S_IMODE(metadata.st_mode) & 0o222:
        fail(f"{label} must be read-only")
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(path, flags)
    try:
        opened = os.fstat(descriptor)
        if not stat.S_ISREG(opened.st_mode) or (
            opened.st_dev,
            opened.st_ino,
        ) != (metadata.st_dev, metadata.st_ino):
            fail(f"{label} changed while it was opened")
        digest = hashlib.sha256()
        while True:
            chunk = os.read(descriptor, 1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
        return digest.hexdigest()
    finally:
        os.close(descriptor)


def declared_gate_root() -> Path:
    raw = os.environ.get("TROUPE_GATE_TMP")
    if raw is None:
        fail("test mode requires an absolute TROUPE_GATE_TMP")
    return existing_real_directory(raw, "TROUPE_GATE_TMP")


def test_gate_root() -> Path:
    if os.environ.get("TROUPE_PERFETTO_TEST_TRANSPORT") != "1":
        fail("custom manifest or test transport requires explicit test transport authority")
    return declared_gate_root()


def detect_platform() -> str:
    system = host_platform.system().lower()
    machine = host_platform.machine().lower()
    if system == "linux":
        if machine in {"x86_64", "amd64"}:
            return "linux-amd64"
        if machine in {"armv7l", "armv8l", "arm"}:
            return "linux-arm"
        if machine in {"aarch64", "arm64"}:
            return "linux-arm64"
    if system == "darwin":
        if machine in {"x86_64", "amd64"}:
            return "mac-amd64"
        if machine in {"aarch64", "arm64"}:
            return "mac-arm64"
    if system in {"windows", "msys", "cygwin"} and machine in {"x86_64", "amd64"}:
        return "windows-amd64"
    fail(f"unsupported Perfetto tool platform: {system}-{machine}")


def validate_url(value: Any, label: str) -> str:
    if not isinstance(value, str):
        fail(f"{label} URL must be a string")
    parsed = urlsplit(value)
    if (
        parsed.scheme != "https"
        or not parsed.hostname
        or parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
    ):
        fail(f"{label} URL must be an uncredentialed HTTPS URL without query or fragment")
    return value


def validate_manifest(
    path: Path,
    *,
    test_only: bool,
) -> tuple[str, list[dict[str, str]]]:
    raw, manifest = read_json(path, "Perfetto manifest")
    exact_fields(manifest, {"schema_version", "release", "assets"}, "Perfetto manifest")
    if manifest["schema_version"] != 1:
        fail("Perfetto manifest schema version is unsupported")
    release = exact_fields(
        manifest["release"], {"tag", "commit", "release_page"}, "Perfetto release"
    )
    if release != {
        "tag": RELEASE_TAG,
        "commit": RELEASE_COMMIT,
        "release_page": RELEASE_PAGE,
    }:
        fail("Perfetto manifest release identity does not match official v57.2")
    raw_assets = manifest["assets"]
    if not isinstance(raw_assets, list) or len(raw_assets) != len(OFFICIAL_ASSETS):
        fail("Perfetto manifest asset set is not exact")
    assets: list[dict[str, str]] = []
    for position, (raw_asset, expected) in enumerate(zip(raw_assets, OFFICIAL_ASSETS)):
        asset = exact_fields(
            raw_asset,
            {"kind", "platform", "url", "sha256", "cache_filename"},
            f"Perfetto asset {position}",
        )
        kind, platform, filename, digest = expected
        if (
            asset["kind"] != kind
            or asset["platform"] != platform
            or asset["cache_filename"] != filename
        ):
            fail(f"Perfetto manifest filename/platform set is not exact at asset {position}")
        if Path(filename).name != filename or filename in {"", ".", ".."}:
            fail(f"Perfetto manifest cache filename is unsafe at asset {position}")
        url = validate_url(asset["url"], f"Perfetto asset {filename}")
        asset_digest = asset["sha256"]
        if not isinstance(asset_digest, str) or SHA256_PATTERN.fullmatch(asset_digest) is None:
            fail(f"Perfetto manifest SHA-256 is invalid for {filename}")
        if not test_only and (
            url != f"{ASSET_BASE}/{filename}" or asset_digest != digest
        ):
            fail(f"Perfetto manifest does not match the official v57.2 asset {filename}")
        assets.append(
            {
                "kind": kind,
                "platform": platform,
                "url": url,
                "sha256": asset_digest,
                "cache_filename": filename,
            }
        )
    if not test_only:
        validate_sums(path.parent / "SHA256SUMS")
    return sha256_bytes(raw), assets


def validate_sums(path: Path) -> None:
    try:
        text = read_regular_bytes(path, "Perfetto SHA256SUMS").decode("ascii")
    except UnicodeError as error:
        fail(f"Perfetto SHA256SUMS is not ASCII: {error}")
    parsed: list[tuple[str, str]] = []
    for line in text.splitlines():
        parts = line.split("  ")
        if len(parts) != 2:
            fail("Perfetto SHA256SUMS has a malformed line")
        parsed.append((parts[1], parts[0]))
    expected = [(filename, digest) for _, _, filename, digest in OFFICIAL_ASSETS]
    if parsed != expected:
        fail("Perfetto SHA256SUMS does not match the official manifest")


def selected_assets(assets: list[dict[str, str]], platform: str) -> list[dict[str, str]]:
    selected = [asset for asset in assets if asset["platform"] in {platform, "all"}]
    if [asset["kind"] for asset in selected] != ["tools", "ui"]:
        fail(f"Perfetto manifest has no exact tools/UI pair for {platform}")
    return selected


def validate_cache(
    raw: str,
    *,
    repository_root: Path,
    platform: str,
    gate_root: Path | None,
) -> Path:
    cache = existing_real_directory(raw, "--cache")
    if is_within(cache, repository_root):
        fail("--cache must remain outside the repository")
    if cache.name != platform:
        fail(f"--cache must end in the exact selected platform directory {platform!r}")
    if gate_root is not None and not is_within(cache, gate_root):
        fail("test cache must remain inside the owned TROUPE_GATE_TMP")
    return cache


def inspect_cache(cache: Path, selected: list[dict[str, str]]) -> None:
    allowed = {asset["cache_filename"] for asset in selected} | {IDENTITY_NAME}
    try:
        entries = list(cache.iterdir())
    except OSError as error:
        fail(f"could not inspect Perfetto cache: {error}")
    unexpected = sorted(entry.name for entry in entries if entry.name not in allowed)
    if unexpected:
        fail(f"Perfetto cache contains unexpected or stale entries: {unexpected}")
    for entry in entries:
        metadata = entry.lstat()
        if stat.S_ISLNK(metadata.st_mode):
            fail(f"Perfetto cache member must not be a symlink: {entry.name}")
        if not stat.S_ISREG(metadata.st_mode):
            fail(f"Perfetto cache member must be a regular file: {entry.name}")
        if entry.resolve(strict=True) != entry:
            fail(f"Perfetto cache member escapes cache: {entry.name}")
        owned(metadata, f"Perfetto cache member {entry.name}")


def cache_has_no_write_bits(cache: Path) -> bool:
    return (stat.S_IMODE(cache.lstat().st_mode) & 0o222) == 0


def require_frozen_cache(cache: Path) -> None:
    mode = stat.S_IMODE(cache.lstat().st_mode)
    if mode != 0o555:
        fail(f"Perfetto cache directory must be frozen with mode 0555, got {mode:04o}")


def require_readonly_member(path: Path, label: str) -> None:
    metadata = path.lstat()
    if stat.S_IMODE(metadata.st_mode) & 0o222:
        fail(f"{label} must be read-only")


def make_regular_readonly(path: Path, label: str) -> None:
    metadata = path.lstat()
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
        fail(f"{label} must be a regular file without symlink indirection")
    if path.resolve(strict=True) != path:
        fail(f"{label} escapes the cache through symlink indirection")
    owned(metadata, label)
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(path, flags)
    try:
        opened = os.fstat(descriptor)
        if not stat.S_ISREG(opened.st_mode) or (
            opened.st_dev,
            opened.st_ino,
        ) != (metadata.st_dev, metadata.st_ino):
            fail(f"{label} changed while it was opened")
        os.fchmod(descriptor, 0o444)
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def freeze_cache(cache: Path, selected: list[dict[str, str]]) -> None:
    for asset in selected:
        filename = asset["cache_filename"]
        make_regular_readonly(cache / filename, f"Perfetto cache member {filename}")
    make_regular_readonly(cache / IDENTITY_NAME, "Perfetto cache identity")
    cache.chmod(0o555)
    fsync_directory(cache)


def expected_identity(
    manifest_sha256: str,
    platform: str,
    selected: list[dict[str, str]],
    *,
    test_only: bool,
    test_manifest: Path | None,
) -> dict[str, Any]:
    return {
        "schema_version": 1,
        "release_tag": RELEASE_TAG,
        "release_commit": RELEASE_COMMIT,
        "platform": platform,
        "manifest_sha256": manifest_sha256,
        "members": [
            {
                "kind": asset["kind"],
                "cache_filename": asset["cache_filename"],
                "sha256": asset["sha256"],
            }
            for asset in selected
        ],
        "test_only": test_only,
        "test_manifest": str(test_manifest) if test_manifest is not None else None,
    }


def load_identity(path: Path) -> dict[str, Any] | None:
    if not path.exists() and not path.is_symlink():
        return None
    _, identity = read_json(path, "Perfetto cache identity")
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
    return identity


def implicit_gate_manifest(cache: Path, default_manifest: Path) -> tuple[Path, bool]:
    bootstrap_gate = os.environ.get("TROUPE_DIAGNOSTIC_BOOTSTRAP_GATE") == "1"
    test_transport = os.environ.get("TROUPE_PERFETTO_TEST_TRANSPORT") == "1"
    if not bootstrap_gate and not test_transport:
        return default_manifest, False
    identity = load_identity(cache / IDENTITY_NAME)
    if identity is None or identity.get("test_only") is not True:
        return default_manifest, False
    gate_root = declared_gate_root() if bootstrap_gate else test_gate_root()
    raw_manifest = identity.get("test_manifest")
    if not isinstance(raw_manifest, str):
        fail("test cache identity has no test manifest path")
    manifest = regular_path(raw_manifest, "test Perfetto manifest")
    if not is_within(manifest, gate_root):
        fail("test Perfetto manifest escapes TROUPE_GATE_TMP")
    return manifest, True


def fsync_directory(path: Path) -> None:
    flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0)
    descriptor = os.open(path, flags)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def write_all(descriptor: int, data: bytes) -> None:
    remaining = memoryview(data)
    while remaining:
        written = os.write(descriptor, remaining)
        if written <= 0:
            fail("download temporary write made no progress")
        remaining = remaining[written:]


def cleanup_temp(path: Path, identity: tuple[int, int]) -> None:
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        return
    except OSError:
        return
    if stat.S_ISREG(metadata.st_mode) and (metadata.st_dev, metadata.st_ino) == identity:
        try:
            path.chmod(0o600)
            path.unlink()
        except OSError:
            pass


def download_asset(
    cache: Path,
    asset: dict[str, str],
    *,
    transport: Path | None,
    gate_root: Path | None,
) -> None:
    filename = asset["cache_filename"]
    descriptor, raw_temp = tempfile.mkstemp(prefix=f".{filename}.tmp-", dir=cache)
    temp = Path(raw_temp)
    initial = os.fstat(descriptor)
    temp_identity = (initial.st_dev, initial.st_ino)
    try:
        os.fchmod(descriptor, 0o600)
        if transport is None:
            if os.environ.get("TROUPE_PERFETTO_NETWORK_FORBIDDEN") == "1":
                fail("network access is forbidden in this Perfetto fetch context")
            request = urllib.request.Request(
                asset["url"], headers={"User-Agent": "troupe-perfetto-cache/v57.2"}
            )
            with urllib.request.urlopen(request, timeout=60) as response:
                while True:
                    chunk = response.read(1024 * 1024)
                    if not chunk:
                        break
                    write_all(descriptor, chunk)
        else:
            os.close(descriptor)
            descriptor = -1
            completed = subprocess.run(
                [str(transport), asset["url"], str(temp)],
                cwd=gate_root,
                env=dict(os.environ),
                check=False,
            )
            if completed.returncode != 0:
                fail(
                    f"test transport failed for {filename} with exit code "
                    f"{completed.returncode}"
                )
        if descriptor >= 0:
            os.fsync(descriptor)
            os.close(descriptor)
            descriptor = -1
        metadata = temp.lstat()
        if (
            not stat.S_ISREG(metadata.st_mode)
            or stat.S_ISLNK(metadata.st_mode)
            or (metadata.st_dev, metadata.st_ino) != temp_identity
            or temp.resolve(strict=True).parent != cache
        ):
            fail(f"download temporary identity changed for {filename}")
        actual = sha256_file(temp, f"download temporary for {filename}", require_readonly=False)
        if actual != asset["sha256"]:
            fail(
                f"download hash mismatch for {filename}: "
                f"expected {asset['sha256']}, got {actual}"
            )
        target = cache / filename
        existing = sha256_file(target, f"Perfetto cache member {filename}", require_readonly=False)
        if existing is not None:
            # The target identity was checked above; replacement changes only this cache name.
            pass
        temp.chmod(0o444)
        os.replace(temp, target)
        fsync_directory(cache)
    finally:
        if descriptor >= 0:
            os.close(descriptor)
        cleanup_temp(temp, temp_identity)


def write_identity(cache: Path, identity: dict[str, Any]) -> None:
    target = cache / IDENTITY_NAME
    existing = load_identity(target)
    if existing is not None:
        if existing != identity:
            fail("existing Perfetto cache identity does not match this manifest/platform")
        return
    payload = (json.dumps(identity, indent=2, sort_keys=True) + "\n").encode("utf-8")
    descriptor, raw_temp = tempfile.mkstemp(prefix=f".{IDENTITY_NAME}.tmp-", dir=cache)
    temp = Path(raw_temp)
    metadata = os.fstat(descriptor)
    temp_identity = (metadata.st_dev, metadata.st_ino)
    try:
        os.fchmod(descriptor, 0o600)
        write_all(descriptor, payload)
        os.fsync(descriptor)
        os.close(descriptor)
        descriptor = -1
        temp.chmod(0o444)
        if target.exists() or target.is_symlink():
            fail("Perfetto cache identity appeared during publication")
        os.replace(temp, target)
        fsync_directory(cache)
    finally:
        if descriptor >= 0:
            os.close(descriptor)
        cleanup_temp(temp, temp_identity)


def verify_cache(
    cache: Path,
    selected: list[dict[str, str]],
    identity: dict[str, Any],
) -> None:
    require_frozen_cache(cache)
    inspect_cache(cache, selected)
    identity_path = cache / IDENTITY_NAME
    existing_identity = load_identity(identity_path)
    if existing_identity is None:
        fail("Perfetto cache identity is missing")
    if existing_identity != identity:
        fail("Perfetto cache identity/manifest mismatch")
    require_readonly_member(identity_path, "Perfetto cache identity")
    for asset in selected:
        filename = asset["cache_filename"]
        actual = sha256_file(
            cache / filename,
            f"Perfetto cache member {filename}",
            require_readonly=True,
        )
        if actual is None:
            fail(f"Perfetto cache member is missing: {filename}")
        if actual != asset["sha256"]:
            fail(
                f"Perfetto cache member hash mismatch for {filename}: "
                f"expected {asset['sha256']}, got {actual}"
            )


def provision_cache(
    cache: Path,
    selected: list[dict[str, str]],
    identity: dict[str, Any],
    *,
    transport: Path | None,
    gate_root: Path | None,
) -> None:
    inspect_cache(cache, selected)
    if cache_has_no_write_bits(cache):
        verify_cache(cache, selected, identity)
        return
    existing_identity = load_identity(cache / IDENTITY_NAME)
    if existing_identity is not None and existing_identity != identity:
        fail("existing Perfetto cache identity does not match this manifest/platform")
    for asset in selected:
        filename = asset["cache_filename"]
        actual = sha256_file(
            cache / filename,
            f"Perfetto cache member {filename}",
            require_readonly=False,
        )
        if actual == asset["sha256"]:
            continue
        download_asset(cache, asset, transport=transport, gate_root=gate_root)
    write_identity(cache, identity)
    freeze_cache(cache, selected)
    verify_cache(cache, selected, identity)


def validate_test_transport(raw: str, gate_root: Path) -> Path:
    transport = regular_path(raw, "test transport")
    if not is_within(transport, gate_root):
        fail("test transport must remain inside the owned TROUPE_GATE_TMP")
    if not os.access(transport, os.X_OK):
        fail("test transport must be executable")
    return transport


def run_canary(raw_transport: str | None) -> int:
    temp: Path | None = None
    try:
        if raw_transport is not None:
            gate_root = test_gate_root()
            transport = validate_test_transport(raw_transport, gate_root)
            descriptor, raw_temp = tempfile.mkstemp(prefix="perfetto-ui-canary-", dir=gate_root)
            os.close(descriptor)
            temp = Path(raw_temp)
            completed = subprocess.run(
                [str(transport), CANARY_URL, str(temp)],
                cwd=gate_root,
                env=dict(os.environ),
                check=False,
            )
            if completed.returncode != 0:
                raise FetchError(f"test transport exited {completed.returncode}")
        else:
            if os.environ.get("TROUPE_PERFETTO_NETWORK_FORBIDDEN") == "1":
                raise FetchError("network access is forbidden")
            request = urllib.request.Request(
                CANARY_URL,
                headers={"User-Agent": "troupe-perfetto-public-ui-canary/v1"},
            )
            with urllib.request.urlopen(request, timeout=15) as response:
                response.read(1)
        print("Perfetto current-public-UI canary passed (non-blocking)", file=sys.stderr)
    except Exception as error:  # The scheduled canary never controls release correctness.
        print(
            f"Perfetto current-public-UI canary warning (non-blocking): {error}",
            file=sys.stderr,
        )
    finally:
        if temp is not None:
            try:
                temp.unlink()
            except FileNotFoundError:
                pass
    return 0


def parse_arguments(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Fetch or verify pinned Perfetto v57.2 tools")
    parser.add_argument("--manifest")
    parser.add_argument("--cache")
    parser.add_argument("--platform", choices=PLATFORMS)
    parser.add_argument("--provision", action="store_true")
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--verify-only", action="store_true")
    parser.add_argument("--transport")
    parser.add_argument("--current-public-ui-canary", action="store_true")
    options = parser.parse_args(argv)
    if options.current_public_ui_canary:
        if any(
            (
                options.manifest,
                options.cache,
                options.platform,
                options.provision,
                options.offline,
                options.verify_only,
            )
        ):
            parser.error("--current-public-ui-canary is a separate non-blocking mode")
        return options
    if options.cache is None:
        parser.error("--cache is required")
    if options.provision == options.offline:
        parser.error("exactly one of --provision or --offline is required")
    if options.verify_only and not options.offline:
        parser.error("--verify-only requires --offline")
    if options.transport is not None and not options.provision:
        parser.error("--transport is only valid with --provision or the canary")
    return options


def main(argv: list[str]) -> int:
    repository_root = Path(argv[0]).resolve(strict=True)
    options = parse_arguments(argv[1:])
    if options.current_public_ui_canary:
        return run_canary(options.transport)

    selected_platform = options.platform or detect_platform()
    possible_gate_root: Path | None = None
    if os.environ.get("TROUPE_DIAGNOSTIC_BOOTSTRAP_GATE") == "1":
        possible_gate_root = declared_gate_root()
    elif os.environ.get("TROUPE_PERFETTO_TEST_TRANSPORT") == "1":
        possible_gate_root = test_gate_root()
    cache = validate_cache(
        options.cache,
        repository_root=repository_root,
        platform=selected_platform,
        gate_root=possible_gate_root,
    )

    default_manifest = repository_root / "tests" / "perfetto" / "tools" / "manifest.json"
    if options.manifest is None:
        manifest_path = default_manifest
    else:
        manifest_path = regular_path(options.manifest, "Perfetto manifest", base=Path.cwd())
    test_only = manifest_path != default_manifest
    if options.offline and not test_only and options.manifest is None:
        manifest_path, test_only = implicit_gate_manifest(cache, default_manifest)
    gate_root: Path | None = None
    if test_only:
        if os.environ.get("TROUPE_DIAGNOSTIC_BOOTSTRAP_GATE") == "1":
            gate_root = declared_gate_root()
        else:
            gate_root = test_gate_root()
        if not is_within(manifest_path, gate_root):
            fail("test Perfetto manifest must remain inside the owned TROUPE_GATE_TMP")
    elif options.transport is not None:
        fail("test transport cannot override the official Perfetto manifest")

    transport: Path | None = None
    if options.transport is not None:
        gate_root = test_gate_root()
        transport = validate_test_transport(options.transport, gate_root)
        if not test_only:
            fail("test transport requires a custom owned manifest")
    elif options.provision and test_only:
        fail("custom test manifest provisioning requires an owned fake transport")

    manifest_sha256, assets = validate_manifest(manifest_path, test_only=test_only)
    selected = selected_assets(assets, selected_platform)
    identity = expected_identity(
        manifest_sha256,
        selected_platform,
        selected,
        test_only=test_only,
        test_manifest=manifest_path if test_only else None,
    )
    if options.offline:
        verify_cache(cache, selected, identity)
        print(
            f"verified Perfetto {RELEASE_TAG} cache for {selected_platform}: {cache}"
        )
    else:
        provision_cache(
            cache,
            selected,
            identity,
            transport=transport,
            gate_root=gate_root,
        )
        print(
            f"provisioned Perfetto {RELEASE_TAG} cache for {selected_platform}: {cache}"
        )
    return 0


try:
    raise SystemExit(main(sys.argv[1:]))
except FetchError as error:
    print(f"fetch_pinned_perfetto_tools: {error}", file=sys.stderr)
    raise SystemExit(1)
except (OSError, urllib.error.URLError) as error:
    print(f"fetch_pinned_perfetto_tools: I/O failure: {error}", file=sys.stderr)
    raise SystemExit(1)
PY
