from __future__ import annotations

import hashlib
import json
import os
import shutil
import stat
import subprocess
import zipfile
from pathlib import Path
from typing import Any

import pytest


ROOT = Path(__file__).resolve().parents[2]
FRONTEND = ROOT / "frontend" / "diagnostics"
SCRIPT = FRONTEND / "scripts" / "provision_browsers.mjs"
LOCK = FRONTEND / "package-lock.json"
CORE = json.loads(LOCK.read_text(encoding="utf-8"))["packages"][
    "node_modules/playwright-core"
]
NODE = shutil.which("node")
ARCHIVES = (
    ("chromium", "1234", "151.0.7922.34"),
    ("chromium-headless-shell", "1234", "151.0.7922.34"),
    ("firefox", "1538", "153.0"),
    ("webkit", "2336", "26.5"),
    ("ffmpeg", "1011", None),
)


def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _tree_hash(members: list[tuple[str, bytes, int]]) -> str:
    digest = hashlib.sha256()
    for name, data, mode in sorted(members):
        member_hash = hashlib.sha256(data).hexdigest()
        published_mode = stat.S_IFMT(mode) | (0o555 if mode & 0o111 else 0o444)
        digest.update(f"{name}\0{published_mode:o}\0{len(data)}\0{member_hash}\n".encode())
    return digest.hexdigest()


def _write_archive(path: Path, members: list[tuple[str, bytes, int]]) -> None:
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for name, data, mode in members:
            info = zipfile.ZipInfo(name)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.create_system = 3
            info.external_attr = mode << 16
            archive.writestr(info, data)


def _fixture(tmp_path: Path) -> tuple[Path, dict[str, Any], dict[str, str]]:
    manifest: dict[str, Any] = {
        "schemaVersion": 1,
        "lockSha256": _sha256(LOCK),
        "playwrightCore": {
            "version": CORE["version"],
            "integrity": CORE["integrity"],
            "browsersSha256": "0" * 64,
        },
        "platforms": {
            "linux-x64": {
                "playwrightPlatform": "ubuntu22.04-x64",
                "archives": [],
            }
        },
    }
    sources: dict[str, str] = {}
    for name, revision, browser_version in ARCHIVES:
        executable = f"{name}/bin/executable"
        members = [
            (executable, f"{name}-{revision}\n".encode(), 0o100755),
            (f"{name}/resources/data.txt", b"fixture\n", 0o100644),
        ]
        archive_path = tmp_path / f"{name}.zip"
        _write_archive(archive_path, members)
        url = f"https://fixtures.invalid/{name}.zip"
        sources[url] = str(archive_path)
        manifest["platforms"]["linux-x64"]["archives"].append(
            {
                "name": name,
                "revision": revision,
                "browserVersion": browser_version,
                "cacheDirectory": f"{name.replace('-', '_')}-{revision}",
                "url": url,
                "archiveSha256": _sha256(archive_path),
                "treeSha256": _tree_hash(members),
                "memberCount": len(members),
                "executable": executable,
                "executableSha256": hashlib.sha256(members[0][1]).hexdigest(),
                "materializedLinks": [],
            }
        )
    manifest_path = tmp_path / "manifest.json"
    manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
    return manifest_path, manifest, sources


def _transport(tmp_path: Path) -> tuple[Path, Path]:
    calls = tmp_path / "transport-calls.jsonl"
    transport = tmp_path / "fake-transport.py"
    transport.write_text(
        """#!/usr/bin/env python3
import json
import os
import shutil
import sys
from pathlib import Path

mapping = json.loads(os.environ["TROUPE_FAKE_BROWSER_ARCHIVES"])
with Path(os.environ["TROUPE_FAKE_BROWSER_CALLS"]).open("a", encoding="utf-8") as stream:
    stream.write(json.dumps(sys.argv[1:]) + "\\n")
source = mapping.get(sys.argv[1])
if source is None:
    raise SystemExit(23)
shutil.copyfile(source, sys.argv[2])
""",
        encoding="utf-8",
    )
    transport.chmod(0o755)
    return transport, calls


def _run(
    tmp_path: Path,
    target: Path,
    manifest_path: Path,
    sources: dict[str, str],
    *,
    transport: Path | None,
    extra_env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    assert NODE is not None
    command = [
        NODE,
        str(SCRIPT),
        "--browser-cache",
        str(target),
        "--manifest",
        str(manifest_path),
    ]
    calls = tmp_path / "transport-calls.jsonl"
    if transport is not None:
        command.extend(["--transport", str(transport)])
    environment = {
        **os.environ,
        "TROUPE_GATE_TMP": str(tmp_path),
        "TROUPE_PLAYWRIGHT_TEST_TRANSPORT": "1",
        "TROUPE_FAKE_BROWSER_ARCHIVES": json.dumps(sources),
        "TROUPE_FAKE_BROWSER_CALLS": str(calls),
        **(extra_env or {}),
    }
    return subprocess.run(command, cwd=ROOT, env=environment, text=True, capture_output=True)


def _make_writable(path: Path) -> None:
    for member in sorted(path.rglob("*"), reverse=True):
        member.chmod(0o755 if member.is_dir() else 0o644)
    path.chmod(0o755)


@pytest.fixture(autouse=True)
def _restore_fixture_permissions(tmp_path: Path) -> Any:
    yield
    _make_writable(tmp_path)


def test_fake_transport_publish_is_atomic_readonly_and_idempotent(tmp_path: Path) -> None:
    manifest_path, manifest, sources = _fixture(tmp_path)
    transport, calls = _transport(tmp_path)
    target = tmp_path / "published" / manifest["lockSha256"] / "linux-x64"
    target.parent.mkdir(parents=True)

    first = _run(tmp_path, target, manifest_path, sources, transport=transport)
    assert first.returncode == 0, first.stderr
    assert target.is_dir()
    assert target.stat().st_mode & 0o222 == 0
    identity = json.loads((target / ".troupe-playwright-cache.json").read_text())
    assert identity["lockSha256"] == manifest["lockSha256"]
    assert [item["name"] for item in identity["archives"]] == [name for name, _, _ in ARCHIVES]
    before = calls.read_text(encoding="utf-8")

    second = _run(tmp_path, target, manifest_path, sources, transport=transport)
    assert second.returncode == 0, second.stderr
    assert calls.read_text(encoding="utf-8") == before


@pytest.mark.parametrize(
    ("mutation", "error"),
    [
        (lambda manifest: manifest.__setitem__("lockSha256", "1" * 64), "lock"),
        (
            lambda manifest: manifest["platforms"]["linux-x64"]["archives"][0].__setitem__(
                "revision", "9999"
            ),
            "revision",
        ),
        (lambda manifest: manifest.__setitem__("platforms", {"other-x64": {}}), "platform"),
        (
            lambda manifest: manifest["platforms"]["linux-x64"]["archives"][0].__setitem__(
                "archiveSha256", "2" * 64
            ),
            "hash",
        ),
    ],
)
def test_identity_mismatch_fails_without_publication(
    tmp_path: Path,
    mutation: Any,
    error: str,
) -> None:
    manifest_path, manifest, sources = _fixture(tmp_path)
    transport, _ = _transport(tmp_path)
    mutation(manifest)
    manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
    target = tmp_path / "cache" / _sha256(LOCK) / "linux-x64"
    target.parent.mkdir(parents=True)

    completed = _run(tmp_path, target, manifest_path, sources, transport=transport)
    assert completed.returncode == 1
    assert error in completed.stderr.lower()
    assert not target.exists()


@pytest.mark.parametrize("unsafe_name", ["../escape", "/absolute", "safe/../../escape"])
def test_archive_path_traversal_is_rejected(tmp_path: Path, unsafe_name: str) -> None:
    manifest_path, manifest, sources = _fixture(tmp_path)
    transport, _ = _transport(tmp_path)
    archive_path = Path(sources["https://fixtures.invalid/chromium.zip"])
    members = [(unsafe_name, b"escape", 0o100644)]
    _write_archive(archive_path, members)
    chromium = manifest["platforms"]["linux-x64"]["archives"][0]
    chromium.update(
        archiveSha256=_sha256(archive_path),
        treeSha256=_tree_hash(members),
        memberCount=1,
        executableSha256=hashlib.sha256(b"escape").hexdigest(),
    )
    manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
    target = tmp_path / "cache" / _sha256(LOCK) / "linux-x64"
    target.parent.mkdir(parents=True)

    completed = _run(tmp_path, target, manifest_path, sources, transport=transport)
    assert completed.returncode == 1
    assert "path" in completed.stderr.lower()
    assert not target.exists()
    assert not (tmp_path / "escape").exists()


def test_archive_symlink_is_rejected(tmp_path: Path) -> None:
    manifest_path, manifest, sources = _fixture(tmp_path)
    transport, _ = _transport(tmp_path)
    archive_path = Path(sources["https://fixtures.invalid/chromium.zip"])
    members = [("chromium/link", b"target", 0o120777)]
    _write_archive(archive_path, members)
    chromium = manifest["platforms"]["linux-x64"]["archives"][0]
    chromium.update(
        archiveSha256=_sha256(archive_path),
        treeSha256=_tree_hash(members),
        memberCount=1,
        executable="chromium/link",
        executableSha256=hashlib.sha256(b"target").hexdigest(),
    )
    manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
    target = tmp_path / "cache" / _sha256(LOCK) / "linux-x64"
    target.parent.mkdir(parents=True)

    completed = _run(tmp_path, target, manifest_path, sources, transport=transport)
    assert completed.returncode == 1
    assert "symlink" in completed.stderr.lower()
    assert not target.exists()


def test_exact_declared_archive_symlink_is_materialized_as_a_regular_file(tmp_path: Path) -> None:
    manifest_path, manifest, sources = _fixture(tmp_path)
    transport, _ = _transport(tmp_path)
    archive_path = Path(sources["https://fixtures.invalid/chromium.zip"])
    executable_data = b"chromium-1234\n"
    library_data = b"shared-library\n"
    archive_members = [
        ("chromium/bin/executable", executable_data, 0o100755),
        ("chromium/lib/real.so", library_data, 0o100644),
        ("chromium/lib/alias.so", b"real.so", 0o120777),
    ]
    published_members = [
        ("chromium/bin/executable", executable_data, 0o100755),
        ("chromium/lib/real.so", library_data, 0o100644),
        ("chromium/lib/alias.so", library_data, 0o100644),
    ]
    _write_archive(archive_path, archive_members)
    chromium = manifest["platforms"]["linux-x64"]["archives"][0]
    chromium.update(
        archiveSha256=_sha256(archive_path),
        treeSha256=_tree_hash(published_members),
        memberCount=3,
        executableSha256=hashlib.sha256(executable_data).hexdigest(),
        materializedLinks=[{"path": "chromium/lib/alias.so", "target": "real.so"}],
    )
    manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
    target = tmp_path / "cache" / manifest["lockSha256"] / "linux-x64"
    target.parent.mkdir(parents=True)

    completed = _run(tmp_path, target, manifest_path, sources, transport=transport)
    assert completed.returncode == 0, completed.stderr
    materialized = target / "chromium-1234" / "chromium" / "lib" / "alias.so"
    assert materialized.is_file()
    assert not materialized.is_symlink()
    assert materialized.read_bytes() == library_data
    assert not any(member.is_symlink() for member in target.rglob("*"))


def test_partial_or_writable_existing_cache_fails_closed(tmp_path: Path) -> None:
    manifest_path, manifest, sources = _fixture(tmp_path)
    transport, calls = _transport(tmp_path)
    target = tmp_path / "cache" / manifest["lockSha256"] / "linux-x64"
    target.parent.mkdir(parents=True)
    assert _run(tmp_path, target, manifest_path, sources, transport=transport).returncode == 0
    before = calls.read_text(encoding="utf-8")

    _make_writable(target)
    missing = target / "chromium-1234" / "chromium" / "resources" / "data.txt"
    missing.unlink()
    completed = _run(tmp_path, target, manifest_path, sources, transport=transport)
    assert completed.returncode == 1
    assert "cache" in completed.stderr.lower()
    assert calls.read_text(encoding="utf-8") == before
    assert target.exists()


def test_complete_existing_cache_must_remain_readonly(tmp_path: Path) -> None:
    manifest_path, manifest, sources = _fixture(tmp_path)
    transport, calls = _transport(tmp_path)
    target = tmp_path / "cache" / manifest["lockSha256"] / "linux-x64"
    target.parent.mkdir(parents=True)
    assert _run(tmp_path, target, manifest_path, sources, transport=transport).returncode == 0
    before = calls.read_text(encoding="utf-8")

    target.chmod(0o755)
    completed = _run(tmp_path, target, manifest_path, sources, transport=transport)
    assert completed.returncode == 1
    assert "read-only" in completed.stderr.lower()
    assert calls.read_text(encoding="utf-8") == before


def test_target_and_transport_authority_reject_home_relative_and_path_fallback(
    tmp_path: Path,
) -> None:
    manifest_path, manifest, sources = _fixture(tmp_path)
    transport, calls = _transport(tmp_path)
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    fake_playwright = fake_bin / "playwright"
    fake_playwright.write_text("#!/bin/sh\nexit 99\n", encoding="utf-8")
    fake_playwright.chmod(0o755)

    relative = _run(tmp_path, Path("relative-cache"), manifest_path, sources, transport=transport)
    assert relative.returncode == 1
    assert "absolute" in relative.stderr.lower()

    home = tmp_path / "home"
    home.mkdir()
    home_target = home / ".cache" / "ms-playwright" / manifest["lockSha256"] / "linux-x64"
    home_target.parent.mkdir(parents=True)
    home_result = _run(
        tmp_path,
        home_target,
        manifest_path,
        sources,
        transport=transport,
        extra_env={"HOME": str(home), "PATH": f"{fake_bin}:{os.environ.get('PATH', '')}"},
    )
    assert home_result.returncode == 1
    assert "home" in home_result.stderr.lower()
    assert not calls.exists()

    target = tmp_path / "cache" / manifest["lockSha256"] / "linux-x64"
    target.parent.mkdir(parents=True)
    implicit = _run(tmp_path, target, manifest_path, sources, transport=None)
    assert implicit.returncode == 1
    assert "transport" in implicit.stderr.lower()
    assert not target.exists()


def test_transport_failure_preserves_existing_cache_and_source_tree(tmp_path: Path) -> None:
    manifest_path, manifest, sources = _fixture(tmp_path)
    transport, _ = _transport(tmp_path)
    target = tmp_path / "cache" / manifest["lockSha256"] / "linux-x64"
    target.parent.mkdir(parents=True)
    existing = tmp_path / "existing"
    existing.mkdir()
    marker = existing / "marker"
    marker.write_text("unchanged", encoding="utf-8")
    source_hashes = {_path: _sha256(_path) for _path in (SCRIPT, LOCK) if _path.exists()}
    sources.pop("https://fixtures.invalid/firefox.zip")

    completed = _run(tmp_path, target, manifest_path, sources, transport=transport)
    assert completed.returncode == 1
    assert "transport" in completed.stderr.lower()
    assert not target.exists()
    assert marker.read_text(encoding="utf-8") == "unchanged"
    assert {_path: _sha256(_path) for _path in source_hashes} == source_hashes


def test_fixture_declares_regular_executable_modes() -> None:
    assert stat.S_IFMT(0o100755) == stat.S_IFREG
