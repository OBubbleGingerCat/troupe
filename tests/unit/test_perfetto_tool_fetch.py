from __future__ import annotations

import hashlib
import json
import os
import shutil
import stat
import subprocess
import sys
from pathlib import Path
from typing import Any

import pytest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "fetch_pinned_perfetto_tools.sh"
MANIFEST = ROOT / "tests" / "perfetto" / "tools" / "manifest.json"
SUMS = ROOT / "tests" / "perfetto" / "tools" / "SHA256SUMS"
RELEASE_TAG = "v57.2"
RELEASE_COMMIT = "da1d152cff27890903d158fe96751de3aab883cc"
RELEASE_PAGE = "https://github.com/google/perfetto/releases/tag/v57.2"
ASSET_BASE = "https://github.com/google/perfetto/releases/download/v57.2"
OFFICIAL_ASSETS = (
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


def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _is_within(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
    except ValueError:
        return False
    return True


def _official_manifest() -> dict[str, Any]:
    return {
        "schema_version": 1,
        "release": {
            "tag": RELEASE_TAG,
            "commit": RELEASE_COMMIT,
            "release_page": RELEASE_PAGE,
        },
        "assets": [
            {
                "kind": kind,
                "platform": platform,
                "url": f"{ASSET_BASE}/{filename}",
                "sha256": digest,
                "cache_filename": filename,
            }
            for kind, platform, filename, digest in OFFICIAL_ASSETS
        ],
    }


def _write_json(path: Path, value: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def _fixture(root: Path) -> tuple[Path, dict[str, Any], dict[str, str]]:
    fixture_root = root / "fixture"
    fixture_root.mkdir(parents=True, exist_ok=True)
    manifest = _official_manifest()
    sources: dict[str, str] = {}
    for asset in manifest["assets"]:
        filename = asset["cache_filename"]
        payload = f"owned fake Perfetto asset: {filename}\n".encode()
        source = fixture_root / filename
        source.write_bytes(payload)
        url = f"https://fixtures.invalid/perfetto/{filename}"
        asset["url"] = url
        asset["sha256"] = hashlib.sha256(payload).hexdigest()
        sources[url] = str(source)
    manifest_path = fixture_root / "manifest.json"
    _write_json(manifest_path, manifest)
    return manifest_path, manifest, sources


def _transport(root: Path, *, exit_code: int = 0) -> tuple[Path, Path]:
    transport = root / "fixture" / "fake-transport.py"
    calls = root / "fixture" / "transport-calls.jsonl"
    transport.parent.mkdir(parents=True, exist_ok=True)
    transport.write_text(
        f"""#!{sys.executable}
import json
import os
import shutil
import sys
from pathlib import Path

with Path(os.environ["TROUPE_FAKE_PERFETTO_CALLS"]).open("a", encoding="utf-8") as stream:
    stream.write(json.dumps(sys.argv[1:]) + "\\n")
if {exit_code}:
    raise SystemExit({exit_code})
mapping = json.loads(os.environ["TROUPE_FAKE_PERFETTO_ASSETS"])
source = mapping.get(sys.argv[1])
if source is None:
    raise SystemExit(23)
shutil.copyfile(source, sys.argv[2])
""",
        encoding="utf-8",
    )
    transport.chmod(0o755)
    return transport, calls


def _cache(root: Path, platform: str = "linux-amd64") -> Path:
    cache = root / "cache" / platform
    cache.mkdir(parents=True, exist_ok=True)
    return cache


def _make_tree_writable(root: Path) -> None:
    for current, directories, filenames in os.walk(root, topdown=True, followlinks=False):
        current_path = Path(current)
        current_path.chmod(0o700)
        for name in directories:
            path = current_path / name
            if not path.is_symlink():
                path.chmod(0o700)
        for name in filenames:
            path = current_path / name
            if not path.is_symlink():
                path.chmod(0o600)


@pytest.fixture(autouse=True)
def _restore_tmp_permissions(tmp_path: Path) -> Any:
    yield
    _make_tree_writable(tmp_path)


def _run(
    root: Path,
    *arguments: str,
    sources: dict[str, str] | None = None,
    calls: Path | None = None,
    authority: bool = True,
    gate_root: Path | None = None,
    bootstrap_gate: bool | None = None,
) -> subprocess.CompletedProcess[str]:
    environment = dict(os.environ)
    environment.update(
        {
            "TROUPE_GATE_TMP": str(gate_root or root),
            "TROUPE_FAKE_PERFETTO_ASSETS": json.dumps(sources or {}),
            "TROUPE_FAKE_PERFETTO_CALLS": str(calls or root / "calls.jsonl"),
            "TROUPE_PERFETTO_NETWORK_FORBIDDEN": "1",
        }
    )
    if authority:
        environment["TROUPE_PERFETTO_TEST_TRANSPORT"] = "1"
    else:
        environment.pop("TROUPE_PERFETTO_TEST_TRANSPORT", None)
    if bootstrap_gate is True:
        environment["TROUPE_DIAGNOSTIC_BOOTSTRAP_GATE"] = "1"
    elif bootstrap_gate is False:
        environment.pop("TROUPE_DIAGNOSTIC_BOOTSTRAP_GATE", None)
    return subprocess.run(
        ["bash", str(SCRIPT), *arguments],
        cwd=ROOT,
        env=environment,
        text=True,
        capture_output=True,
        check=False,
    )


def _provision(
    root: Path,
    cache: Path,
    manifest: Path,
    sources: dict[str, str],
    transport: Path,
    calls: Path,
    *,
    gate_root: Path | None = None,
) -> subprocess.CompletedProcess[str]:
    return _run(
        root,
        "--manifest",
        str(manifest),
        "--cache",
        str(cache),
        "--platform",
        "linux-amd64",
        "--provision",
        "--transport",
        str(transport),
        sources=sources,
        calls=calls,
        gate_root=gate_root,
    )


@pytest.fixture(scope="session", autouse=True)
def _seed_bootstrap_gate_fake_cache(tmp_path_factory: pytest.TempPathFactory) -> None:
    if os.environ.get("TROUPE_DIAGNOSTIC_BOOTSTRAP_GATE") != "1":
        return
    gate_root = Path(os.environ["TROUPE_GATE_TMP"]).resolve(strict=True)
    cache = Path(os.environ["TROUPE_PERFETTO_CACHE"]).resolve(strict=True)
    fixture_root = tmp_path_factory.mktemp("perfetto-gate-fixture")
    if not _is_within(fixture_root, gate_root):
        raise AssertionError("bootstrap fixture root escaped TROUPE_GATE_TMP")
    if not _is_within(cache, gate_root):
        raise AssertionError("bootstrap Perfetto cache escaped TROUPE_GATE_TMP")
    manifest, _, sources = _fixture(fixture_root)
    transport, calls = _transport(fixture_root)
    completed = _provision(
        fixture_root,
        cache,
        manifest,
        sources,
        transport,
        calls,
        gate_root=gate_root,
    )
    assert completed.returncode == 0, completed.stderr


def test_manifest_and_sums_are_the_exact_official_v57_2_release() -> None:
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    assert manifest == _official_manifest()

    sums: dict[str, str] = {}
    for line in SUMS.read_text(encoding="utf-8").splitlines():
        digest, filename = line.split("  ", 1)
        assert filename not in sums
        sums[filename] = digest
    assert sums == {filename: digest for _, _, filename, digest in OFFICIAL_ASSETS}


def test_fake_transport_downloads_temp_hashes_and_atomically_publishes(
    tmp_path: Path,
) -> None:
    manifest_path, manifest, sources = _fixture(tmp_path)
    transport, calls = _transport(tmp_path)
    cache = _cache(tmp_path)

    completed = _provision(tmp_path, cache, manifest_path, sources, transport, calls)
    assert completed.returncode == 0, completed.stderr

    selected = [
        asset
        for asset in manifest["assets"]
        if asset["platform"] in {"linux-amd64", "all"}
    ]
    assert [entry.name for entry in cache.iterdir()] == [
        "linux-amd64.zip",
        "perfetto-ui.zip",
        ".troupe-perfetto-cache.json",
    ]
    for asset in selected:
        published = cache / asset["cache_filename"]
        assert _sha256(published) == asset["sha256"]
        assert stat.S_IMODE(published.stat().st_mode) == 0o444
    assert stat.S_IMODE(cache.stat().st_mode) == 0o555
    transport_calls = [json.loads(line) for line in calls.read_text().splitlines()]
    assert [call[0] for call in transport_calls] == [asset["url"] for asset in selected]
    assert all(Path(call[1]).parent == cache for call in transport_calls)
    assert all(Path(call[1]).name.startswith(".") for call in transport_calls)
    assert not list(cache.glob(".*.tmp-*"))

    identity = json.loads((cache / ".troupe-perfetto-cache.json").read_text())
    assert stat.S_IMODE((cache / ".troupe-perfetto-cache.json").stat().st_mode) == 0o444
    assert identity["release_tag"] == RELEASE_TAG
    assert identity["release_commit"] == RELEASE_COMMIT
    assert identity["platform"] == "linux-amd64"
    assert identity["manifest_sha256"] == _sha256(manifest_path)
    assert identity["test_only"] is True


def test_readonly_valid_cache_is_reused_without_transport(tmp_path: Path) -> None:
    manifest, _, sources = _fixture(tmp_path)
    transport, calls = _transport(tmp_path)
    cache = _cache(tmp_path)
    assert _provision(tmp_path, cache, manifest, sources, transport, calls).returncode == 0
    before = calls.read_text(encoding="utf-8")

    repeated = _provision(tmp_path, cache, manifest, sources, transport, calls)
    assert repeated.returncode == 0, repeated.stderr
    assert calls.read_text(encoding="utf-8") == before
    assert stat.S_IMODE(cache.stat().st_mode) == 0o555


def test_complete_writable_cache_is_frozen_without_transport(tmp_path: Path) -> None:
    manifest, _, sources = _fixture(tmp_path)
    transport, calls = _transport(tmp_path)
    cache = _cache(tmp_path)
    assert _provision(tmp_path, cache, manifest, sources, transport, calls).returncode == 0
    before = calls.read_text(encoding="utf-8")
    cache.chmod(0o755)
    (cache / "linux-amd64.zip").chmod(0o644)
    (cache / ".troupe-perfetto-cache.json").chmod(0o644)

    repeated = _provision(tmp_path, cache, manifest, sources, transport, calls)
    assert repeated.returncode == 0, repeated.stderr
    assert calls.read_text(encoding="utf-8") == before
    assert stat.S_IMODE(cache.stat().st_mode) == 0o555
    assert stat.S_IMODE((cache / "linux-amd64.zip").stat().st_mode) == 0o444
    assert stat.S_IMODE((cache / ".troupe-perfetto-cache.json").stat().st_mode) == 0o444


def test_offline_verify_never_invokes_transport(tmp_path: Path) -> None:
    manifest, _, sources = _fixture(tmp_path)
    transport, calls = _transport(tmp_path)
    cache = _cache(tmp_path)
    assert _provision(tmp_path, cache, manifest, sources, transport, calls).returncode == 0
    before = calls.read_text(encoding="utf-8")

    completed = _run(
        tmp_path,
        "--manifest",
        str(manifest),
        "--cache",
        str(cache),
        "--platform",
        "linux-amd64",
        "--offline",
        "--verify-only",
        sources=sources,
        calls=calls,
    )
    assert completed.returncode == 0, completed.stderr
    assert calls.read_text(encoding="utf-8") == before


@pytest.mark.parametrize("writable", ["directory", "identity", "member"])
def test_offline_rejects_any_writable_cache_component(
    tmp_path: Path,
    writable: str,
) -> None:
    manifest, _, sources = _fixture(tmp_path)
    transport, calls = _transport(tmp_path)
    cache = _cache(tmp_path)
    assert _provision(tmp_path, cache, manifest, sources, transport, calls).returncode == 0
    before = calls.read_text(encoding="utf-8")
    if writable == "directory":
        cache.chmod(0o755)
    elif writable == "identity":
        (cache / ".troupe-perfetto-cache.json").chmod(0o644)
    else:
        (cache / "linux-amd64.zip").chmod(0o644)

    completed = _run(
        tmp_path,
        "--manifest",
        str(manifest),
        "--cache",
        str(cache),
        "--platform",
        "linux-amd64",
        "--offline",
        "--verify-only",
        sources=sources,
        calls=calls,
    )
    assert completed.returncode == 1
    assert "read-only" in completed.stderr.lower() or "frozen" in completed.stderr.lower()
    assert calls.read_text(encoding="utf-8") == before


@pytest.mark.parametrize("state", ["missing", "mismatch"])
def test_offline_missing_or_mismatched_member_fails_without_transport(
    tmp_path: Path,
    state: str,
) -> None:
    manifest, _, sources = _fixture(tmp_path)
    transport, calls = _transport(tmp_path)
    cache = _cache(tmp_path)
    assert _provision(tmp_path, cache, manifest, sources, transport, calls).returncode == 0
    member = cache / "linux-amd64.zip"
    if state == "missing":
        cache.chmod(0o755)
        member.unlink()
        cache.chmod(0o555)
    else:
        member.chmod(0o644)
        member.write_bytes(b"mismatch\n")
        member.chmod(0o444)
    before = calls.read_text(encoding="utf-8")

    completed = _run(
        tmp_path,
        "--manifest",
        str(manifest),
        "--cache",
        str(cache),
        "--platform",
        "linux-amd64",
        "--offline",
        "--verify-only",
        sources=sources,
        calls=calls,
    )
    assert completed.returncode == 1
    assert state in completed.stderr.lower() or "hash" in completed.stderr.lower()
    assert calls.read_text(encoding="utf-8") == before


def test_failed_download_preserves_existing_member_and_removes_temp(tmp_path: Path) -> None:
    manifest, _, sources = _fixture(tmp_path)
    transport, calls = _transport(tmp_path)
    cache = _cache(tmp_path)
    assert _provision(tmp_path, cache, manifest, sources, transport, calls).returncode == 0
    cache.chmod(0o755)
    member = cache / "linux-amd64.zip"
    member.chmod(0o644)
    member.write_bytes(b"old mismatched cache member\n")
    member.chmod(0o444)
    corrupt = tmp_path / "fixture" / "corrupt.zip"
    corrupt.write_bytes(b"bad download\n")
    sources["https://fixtures.invalid/perfetto/linux-amd64.zip"] = str(corrupt)

    completed = _provision(tmp_path, cache, manifest, sources, transport, calls)
    assert completed.returncode == 1
    assert member.read_bytes() == b"old mismatched cache member\n"
    assert not list(cache.glob(".*.tmp-*"))


@pytest.mark.parametrize("state", ["missing", "mismatch"])
def test_frozen_invalid_cache_is_not_thawed_or_repaired(
    tmp_path: Path,
    state: str,
) -> None:
    manifest, _, sources = _fixture(tmp_path)
    transport, calls = _transport(tmp_path)
    cache = _cache(tmp_path)
    assert _provision(tmp_path, cache, manifest, sources, transport, calls).returncode == 0
    member = cache / "linux-amd64.zip"
    if state == "missing":
        cache.chmod(0o755)
        member.unlink()
        cache.chmod(0o555)
    else:
        member.chmod(0o644)
        member.write_bytes(b"frozen mismatch\n")
        member.chmod(0o444)
    before = calls.read_text(encoding="utf-8")

    completed = _provision(tmp_path, cache, manifest, sources, transport, calls)
    assert completed.returncode == 1
    assert state in completed.stderr.lower() or "hash" in completed.stderr.lower()
    assert calls.read_text(encoding="utf-8") == before
    assert stat.S_IMODE(cache.stat().st_mode) == 0o555


@pytest.mark.parametrize("cache_value", ["relative/cache", "."])
def test_cache_must_be_explicit_absolute_external_directory(
    tmp_path: Path,
    cache_value: str,
) -> None:
    manifest, _, sources = _fixture(tmp_path)
    transport, calls = _transport(tmp_path)
    value = cache_value if cache_value != "." else str(ROOT)
    completed = _run(
        tmp_path,
        "--manifest",
        str(manifest),
        "--cache",
        value,
        "--platform",
        "linux-amd64",
        "--provision",
        "--transport",
        str(transport),
        sources=sources,
        calls=calls,
    )
    assert completed.returncode == 1
    assert "absolute" in completed.stderr.lower() or "outside" in completed.stderr.lower()


def test_cache_directory_symlink_is_rejected(tmp_path: Path) -> None:
    manifest, _, sources = _fixture(tmp_path)
    transport, calls = _transport(tmp_path)
    real_cache = tmp_path / "real" / "linux-amd64"
    real_cache.mkdir(parents=True)
    link = tmp_path / "cache" / "linux-amd64"
    link.parent.mkdir()
    link.symlink_to(real_cache, target_is_directory=True)

    completed = _provision(tmp_path, link, manifest, sources, transport, calls)
    assert completed.returncode == 1
    assert "symlink" in completed.stderr.lower() or "real path" in completed.stderr.lower()


def test_cache_member_symlink_and_escape_are_rejected(tmp_path: Path) -> None:
    manifest, _, sources = _fixture(tmp_path)
    cache = _cache(tmp_path)
    outside = tmp_path / "outside.zip"
    outside.write_bytes(b"outside\n")
    (cache / "linux-amd64.zip").symlink_to(outside)
    cache.chmod(0o555)

    completed = _run(
        tmp_path,
        "--manifest",
        str(manifest),
        "--cache",
        str(cache),
        "--platform",
        "linux-amd64",
        "--offline",
        "--verify-only",
        sources=sources,
    )
    assert completed.returncode == 1
    assert "symlink" in completed.stderr.lower()
    assert outside.read_bytes() == b"outside\n"


def test_manifest_traversal_filename_is_rejected_before_transport(tmp_path: Path) -> None:
    manifest_path, manifest, sources = _fixture(tmp_path)
    manifest["assets"][0]["cache_filename"] = "../escape.zip"
    _write_json(manifest_path, manifest)
    transport, calls = _transport(tmp_path)
    cache = _cache(tmp_path)

    completed = _provision(tmp_path, cache, manifest_path, sources, transport, calls)
    assert completed.returncode == 1
    assert "filename" in completed.stderr.lower() or "manifest" in completed.stderr.lower()
    assert not calls.exists()
    assert not (cache.parent / "escape.zip").exists()


def test_custom_manifest_and_transport_require_owned_test_authority(tmp_path: Path) -> None:
    manifest, _, sources = _fixture(tmp_path)
    transport, calls = _transport(tmp_path)
    cache = _cache(tmp_path)

    missing_authority = _run(
        tmp_path,
        "--manifest",
        str(manifest),
        "--cache",
        str(cache),
        "--platform",
        "linux-amd64",
        "--provision",
        "--transport",
        str(transport),
        sources=sources,
        calls=calls,
        authority=False,
    )
    assert missing_authority.returncode == 1
    assert "test transport" in missing_authority.stderr.lower()

    gate_root = tmp_path / "smaller-gate-root"
    gate_root.mkdir()
    escaped = _run(
        tmp_path,
        "--manifest",
        str(manifest),
        "--cache",
        str(cache),
        "--platform",
        "linux-amd64",
        "--provision",
        "--transport",
        str(transport),
        sources=sources,
        calls=calls,
        gate_root=gate_root,
    )
    assert escaped.returncode == 1
    assert "gate" in escaped.stderr.lower() or "owned" in escaped.stderr.lower()


def test_bootstrap_offline_command_can_verify_seeded_fake_cache_without_ambient_authority(
    tmp_path: Path,
) -> None:
    manifest, _, sources = _fixture(tmp_path)
    transport, calls = _transport(tmp_path)
    cache = _cache(tmp_path)
    assert _provision(tmp_path, cache, manifest, sources, transport, calls).returncode == 0

    authorized = _run(
        tmp_path,
        "--cache",
        str(cache),
        "--platform",
        "linux-amd64",
        "--offline",
        "--verify-only",
        sources=sources,
        calls=calls,
        authority=False,
        bootstrap_gate=True,
    )
    assert authorized.returncode == 0, authorized.stderr

    unauthenticated = _run(
        tmp_path,
        "--cache",
        str(cache),
        "--platform",
        "linux-amd64",
        "--offline",
        "--verify-only",
        sources=sources,
        calls=calls,
        authority=False,
        bootstrap_gate=False,
    )
    assert unauthenticated.returncode == 1


def test_current_public_ui_canary_is_separate_and_non_blocking(tmp_path: Path) -> None:
    transport, calls = _transport(tmp_path, exit_code=37)
    completed = _run(
        tmp_path,
        "--current-public-ui-canary",
        "--transport",
        str(transport),
        calls=calls,
    )
    assert completed.returncode == 0
    assert "non-blocking" in completed.stderr.lower()
    arguments = json.loads(calls.read_text(encoding="utf-8").splitlines()[0])
    assert arguments[0] == "https://ui.perfetto.dev/"

    mixed = _run(
        tmp_path,
        "--current-public-ui-canary",
        "--cache",
        str(_cache(tmp_path)),
    )
    assert mixed.returncode != 0


def test_downloaded_archives_remain_external_to_source_and_wheel_inputs() -> None:
    for _, _, filename, _ in OFFICIAL_ASSETS:
        assert not (ROOT / filename).exists()
        assert not (ROOT / "tests" / "perfetto" / "tools" / filename).exists()
    assert "tests/perfetto/tools" not in (ROOT / "pyproject.toml").read_text(encoding="utf-8")
