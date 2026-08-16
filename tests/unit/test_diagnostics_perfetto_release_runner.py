from __future__ import annotations

import hashlib
import json
import os
import stat
import subprocess
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT_RELATIVE = Path("scripts/test_diagnostics_perfetto_release.sh")
SCRIPT = ROOT / SCRIPT_RELATIVE
QUALITY_RELATIVE = Path("tests/fixtures/release/perfetto-quality.json")
PERFETTO_MANIFEST_RELATIVE = Path("tests/perfetto/tools/manifest.json")
BROWSER_MANIFEST_RELATIVE = Path(
    "frontend/diagnostics/tests/tooling/playwright-browsers.json"
)
LOCK_RELATIVE = Path("frontend/diagnostics/package-lock.json")
T02_RELATIVE = Path("scripts/test_perfetto_compatibility.sh")


FAKE_T02 = r"""#!/usr/bin/env python3
import hashlib
import json
import os
import sys
from pathlib import Path

arguments = sys.argv[1:]
perfetto = Path(arguments[arguments.index("--cache") + 1])
browser = Path(arguments[arguments.index("--browser-cache") + 1])
with (perfetto / ".troupe-perfetto-cache.json").open(encoding="utf-8") as stream:
    perfetto_identity = json.load(stream)
with (browser / ".troupe-playwright-cache.json").open(encoding="utf-8") as stream:
    browser_identity = json.load(stream)

log = {
    "argv": arguments,
    "HTTP_PROXY": os.environ.get("HTTP_PROXY"),
    "http_proxy": os.environ.get("http_proxy"),
    "network_forbidden": os.environ.get("TROUPE_PERFETTO_NETWORK_FORBIDDEN"),
    "npm_offline": os.environ.get("npm_config_offline"),
    "registry": os.environ.get("npm_config_registry"),
    "HOME": os.environ.get("HOME"),
    "PATH": os.environ.get("PATH"),
}
Path(os.environ["TROUPE_FAKE_LOG"]).write_text(json.dumps(log), encoding="utf-8")

if os.environ.get("TROUPE_FAKE_MALFORMED") == "1":
    print("not-json")
    raise SystemExit(0)

failures = json.loads(os.environ.get("TROUPE_FAKE_FAILURES", "{}"))
layers = []
first_failed = None
first_exit = 0
for name in ("decode", "sql", "ui"):
    exit_code = int(failures.get(name, 0))
    if exit_code and first_failed is None:
        first_failed = name
        first_exit = exit_code
    layers.append(
        {
            "name": name,
            "status": "passed" if exit_code == 0 else "failed",
            "exit_code": exit_code,
            "stdout_sha256": hashlib.sha256((name + "-out").encode()).hexdigest(),
            "stderr_sha256": hashlib.sha256((name + "-err").encode()).hexdigest(),
        }
    )

members = sorted(
    (
        {"kind": member["kind"], "sha256": member["sha256"]}
        for member in perfetto_identity["members"]
    ),
    key=lambda member: member["kind"],
)
chromium = next(
    archive for archive in browser_identity["archives"] if archive["name"] == "chromium"
)
summary = {
    "schema": "troupe.perfetto.compatibility.v1",
    "mode": "all-layers",
    "offline": True,
    "result": "passed" if first_failed is None else "failed",
    "first_failed_layer": first_failed,
    "fixtures": {
        "manifest_sha256": "a" * 64,
        "files": [{"path": "trace.pb", "sha256": "b" * 64}],
    },
    "perfetto": {
        "release_tag": perfetto_identity["release_tag"],
        "release_commit": perfetto_identity["release_commit"],
        "platform": perfetto_identity["platform"],
        "manifest_sha256": perfetto_identity["manifest_sha256"],
        "members": members,
    },
    "browser": {
        "manifest_sha256": browser_identity["manifestSha256"],
        "lock_sha256": browser_identity["lockSha256"],
        "playwright_core": {
            "version": browser_identity["playwrightCore"]["version"],
            "browsers_sha256": browser_identity["playwrightCore"]["browsersSha256"],
        },
        "chromium": {
            "revision": chromium["revision"],
            "version": chromium["browserVersion"],
            "archive_sha256": chromium["archiveSha256"],
            "tree_sha256": chromium["treeSha256"],
            "executable_sha256": chromium["executableSha256"],
        },
    },
    "layers": layers,
}
print(json.dumps(summary, sort_keys=True))

if os.environ.get("TROUPE_FAKE_MUTATE") == "perfetto":
    identity = perfetto / ".troupe-perfetto-cache.json"
    perfetto.chmod(0o755)
    identity.chmod(0o644)
    value = json.loads(identity.read_text(encoding="utf-8"))
    value["release_tag"] = "changed"
    identity.write_text(json.dumps(value), encoding="utf-8")

raise SystemExit(first_exit)
"""


def _sha256(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def _write(path: Path, value: bytes, mode: int = 0o644) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(value)
    path.chmod(mode)


def _write_json(path: Path, value: object, mode: int = 0o644) -> None:
    _write(path, (json.dumps(value, sort_keys=True) + "\n").encode(), mode)


def _freeze(root: Path) -> None:
    paths = sorted(root.rglob("*"), key=lambda path: len(path.parts), reverse=True)
    for path in paths:
        metadata = path.lstat()
        if stat.S_ISDIR(metadata.st_mode):
            path.chmod(0o555)
        elif metadata.st_mode & 0o111:
            path.chmod(0o555)
        else:
            path.chmod(0o444)
    root.chmod(0o555)


def _thaw(root: Path) -> None:
    if not root.exists():
        return
    root.chmod(0o755)
    for path in root.rglob("*"):
        if path.is_symlink():
            continue
        if path.is_dir():
            path.chmod(0o755)


def _copy_harness(tmp_path: Path) -> tuple[Path, Path, Path]:
    root = (tmp_path / "repository").resolve()
    _write(root / SCRIPT_RELATIVE, SCRIPT.read_bytes(), 0o755)
    _write(root / T02_RELATIVE, FAKE_T02.encode(), 0o755)

    tools_payload = b"fake-perfetto-tools\n"
    ui_payload = b"fake-perfetto-ui\n"
    tools_hash = _sha256(tools_payload)
    ui_hash = _sha256(ui_payload)
    perfetto_manifest = {
        "schema_version": 1,
        "release": {
            "tag": "v-test",
            "commit": "test-commit",
            "release_page": "https://invalid.test/perfetto",
        },
        "assets": [
            {
                "kind": "tools",
                "platform": "linux-test",
                "url": "https://invalid.test/tools.zip",
                "sha256": tools_hash,
                "cache_filename": "tools.zip",
            },
            {
                "kind": "ui",
                "platform": "all",
                "url": "https://invalid.test/ui.zip",
                "sha256": ui_hash,
                "cache_filename": "ui.zip",
            },
        ],
    }
    _write_json(root / PERFETTO_MANIFEST_RELATIVE, perfetto_manifest)
    perfetto_manifest_hash = _sha256((root / PERFETTO_MANIFEST_RELATIVE).read_bytes())

    lock_payload = b'{"name":"fake-lock"}\n'
    _write(root / LOCK_RELATIVE, lock_payload)
    lock_hash = _sha256(lock_payload)
    executable_payload = b"fake chromium executable\n"
    executable_hash = _sha256(executable_payload)
    browser_archive = {
        "name": "chromium",
        "revision": "test-revision",
        "browserVersion": "test-version",
        "cacheDirectory": "chromium-test",
        "url": "https://invalid.test/chromium.zip",
        "archiveSha256": "1" * 64,
        "treeSha256": "2" * 64,
        "memberCount": 2,
        "executable": "chrome/chrome",
        "executableSha256": executable_hash,
        "materializedLinks": [],
    }
    browser_manifest = {
        "schemaVersion": 1,
        "lockSha256": lock_hash,
        "playwrightCore": {
            "version": "test-core",
            "integrity": "sha512-test",
            "browsersSha256": "3" * 64,
        },
        "platforms": {
            "linux-test": {
                "playwrightPlatform": "test-platform",
                "archives": [browser_archive],
            }
        },
    }
    _write_json(root / BROWSER_MANIFEST_RELATIVE, browser_manifest)
    browser_manifest_hash = _sha256((root / BROWSER_MANIFEST_RELATIVE).read_bytes())

    quality = {
        "schema": "troupe.diagnostics.perfetto-quality.v1",
        "compatibility": {
            "path": T02_RELATIVE.as_posix(),
            "sha256": _sha256((root / T02_RELATIVE).read_bytes()),
        },
        "perfetto": {
            "manifest": PERFETTO_MANIFEST_RELATIVE.as_posix(),
            "manifest_sha256": perfetto_manifest_hash,
            "platform": "linux-test",
        },
        "playwright": {
            "lock": LOCK_RELATIVE.as_posix(),
            "lock_sha256": lock_hash,
            "manifest": BROWSER_MANIFEST_RELATIVE.as_posix(),
            "manifest_sha256": browser_manifest_hash,
            "platform": "linux-test",
        },
    }
    _write_json(root / QUALITY_RELATIVE, quality)

    perfetto = (tmp_path / "perfetto-cache").resolve()
    perfetto.mkdir()
    _write(perfetto / "tools.zip", tools_payload)
    _write(perfetto / "ui.zip", ui_payload)
    _write_json(
        perfetto / ".troupe-perfetto-cache.json",
        {
            "schema_version": 1,
            "release_tag": "v-test",
            "release_commit": "test-commit",
            "platform": "linux-test",
            "manifest_sha256": perfetto_manifest_hash,
            "members": [
                {"kind": "tools", "cache_filename": "tools.zip", "sha256": tools_hash},
                {"kind": "ui", "cache_filename": "ui.zip", "sha256": ui_hash},
            ],
            "test_only": False,
            "test_manifest": None,
        },
    )
    _freeze(perfetto)

    browser = (tmp_path / "browser-cache").resolve()
    archive_root = browser / "chromium-test"
    _write(archive_root / "chrome/chrome", executable_payload, 0o755)
    _write(archive_root / "INSTALLATION_COMPLETE", b"complete\n")
    cache_archive = {key: value for key, value in browser_archive.items() if key != "url"}
    _write_json(
        browser / ".troupe-playwright-cache.json",
        {
            "schemaVersion": 1,
            "manifestSha256": browser_manifest_hash,
            "lockSha256": lock_hash,
            "platform": "linux-test",
            "playwrightPlatform": "test-platform",
            "playwrightCore": browser_manifest["playwrightCore"],
            "archives": [cache_archive],
        },
    )
    _freeze(browser)
    return root, perfetto, browser


def _arguments(perfetto: Path, browser: Path) -> list[str]:
    return [
        "--offline",
        "--all-layers",
        "--perfetto-cache",
        str(perfetto),
        "--browser-cache",
        str(browser),
    ]


def _run(
    root: Path,
    arguments: list[str],
    *,
    extra_environment: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    environment = dict(os.environ)
    environment["TROUPE_FAKE_LOG"] = str(root.parent / "fake-child.json")
    if extra_environment:
        environment.update(extra_environment)
    try:
        return subprocess.run(
            [str(root / SCRIPT_RELATIVE), *arguments],
            cwd=root,
            env=environment,
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
        )
    finally:
        _thaw(root.parent / "perfetto-cache")
        _thaw(root.parent / "browser-cache")


def test_checked_in_fixture_binds_exact_ancestor_inputs() -> None:
    quality = json.loads((ROOT / QUALITY_RELATIVE).read_text(encoding="utf-8"))

    assert quality == {
        "schema": "troupe.diagnostics.perfetto-quality.v1",
        "compatibility": {
            "path": T02_RELATIVE.as_posix(),
            "sha256": _sha256((ROOT / T02_RELATIVE).read_bytes()),
        },
        "perfetto": {
            "manifest": PERFETTO_MANIFEST_RELATIVE.as_posix(),
            "manifest_sha256": _sha256((ROOT / PERFETTO_MANIFEST_RELATIVE).read_bytes()),
            "platform": "linux-amd64",
        },
        "playwright": {
            "lock": LOCK_RELATIVE.as_posix(),
            "lock_sha256": _sha256((ROOT / LOCK_RELATIVE).read_bytes()),
            "manifest": BROWSER_MANIFEST_RELATIVE.as_posix(),
            "manifest_sha256": _sha256((ROOT / BROWSER_MANIFEST_RELATIVE).read_bytes()),
            "platform": "linux-x64",
        },
    }
    browser = json.loads((ROOT / BROWSER_MANIFEST_RELATIVE).read_text(encoding="utf-8"))
    assert [archive["name"] for archive in browser["platforms"]["linux-x64"]["archives"]] == [
        "chromium",
        "chromium-headless-shell",
        "firefox",
        "webkit",
        "ffmpeg",
    ]
    assert stat.S_IMODE(SCRIPT.stat().st_mode) == 0o755


def test_release_wrapper_uses_exact_child_and_poisoned_offline_environment(tmp_path: Path) -> None:
    root, perfetto, browser = _copy_harness(tmp_path)
    fallback_home = tmp_path / "fallback-home"
    fallback_home.mkdir()
    (fallback_home / ".cache").mkdir()
    decoy = tmp_path / "decoy"
    decoy.mkdir()
    sentinel = tmp_path / "path-fallback-ran"
    _write(
        decoy / T02_RELATIVE.name,
        f"#!/bin/sh\ntouch {sentinel}\nexit 91\n".encode(),
        0o755,
    )

    completed = _run(
        root,
        _arguments(perfetto, browser),
        extra_environment={
            "HOME": str(fallback_home),
            "PATH": f"{decoy}{os.pathsep}{os.environ['PATH']}",
        },
    )

    assert completed.returncode == 0, completed.stderr
    assert not sentinel.exists()
    child = json.loads((root.parent / "fake-child.json").read_text(encoding="utf-8"))
    assert child["argv"] == [
        "--offline",
        "--all-layers",
        "--cache",
        str(perfetto),
        "--browser-cache",
        str(browser),
    ]
    assert child["HOME"] == str(fallback_home)
    assert child["HTTP_PROXY"] == child["http_proxy"] == "http://127.0.0.1:9/"
    assert child["network_forbidden"] == "1"
    assert child["npm_offline"] == "true"
    assert child["registry"] == "http://127.0.0.1:9/"
    summary = json.loads(completed.stdout)
    assert summary["schema"] == "troupe.diagnostics.perfetto-release.v1"
    assert summary["result"] == "passed"
    assert [layer["status"] for layer in summary["compatibility"]["layers"]] == [
        "passed",
        "passed",
        "passed",
    ]
    assert summary["cache"]["perfetto"]["realpath"] == str(perfetto)
    assert summary["cache"]["playwright"]["realpath"] == str(browser)


def test_missing_repository_child_does_not_fall_back_to_path(tmp_path: Path) -> None:
    root, perfetto, browser = _copy_harness(tmp_path)
    (root / T02_RELATIVE).unlink()
    decoy = tmp_path / "decoy"
    decoy.mkdir()
    sentinel = tmp_path / "path-fallback-ran"
    _write(
        decoy / T02_RELATIVE.name,
        f"#!/bin/sh\ntouch {sentinel}\nexit 0\n".encode(),
        0o755,
    )

    completed = _run(
        root,
        _arguments(perfetto, browser),
        extra_environment={"PATH": f"{decoy}{os.pathsep}{os.environ['PATH']}"},
    )

    assert completed.returncode == 1
    assert "compatibility script is unavailable" in completed.stderr
    assert not sentinel.exists()
    assert not (root.parent / "fake-child.json").exists()


def test_home_cache_candidates_do_not_replace_explicit_arguments(tmp_path: Path) -> None:
    root, _perfetto, _browser = _copy_harness(tmp_path)
    fallback_home = tmp_path / "fallback-home"
    (fallback_home / "perfetto-cache").mkdir(parents=True)
    (fallback_home / "browser-cache").mkdir()

    completed = _run(
        root,
        ["--offline", "--all-layers"],
        extra_environment={"HOME": str(fallback_home)},
    )

    assert completed.returncode == 2
    assert "--perfetto-cache is required" in completed.stderr
    assert not (root.parent / "fake-child.json").exists()


@pytest.mark.parametrize(
    "arguments",
    [
        [],
        ["--all-layers"],
        ["--offline"],
        ["--offline", "--all-layers"],
        ["--offline", "--all-layers", "--perfetto-cache", "/tmp/cache"],
        ["--offline", "--all-layers", "--browser-cache", "/tmp/cache"],
        ["--offline", "--all-layers", "--decode"],
    ],
)
def test_release_arguments_are_closed(tmp_path: Path, arguments: list[str]) -> None:
    root, _perfetto, _browser = _copy_harness(tmp_path)
    completed = _run(root, arguments)

    assert completed.returncode == 2
    assert not (root.parent / "fake-child.json").exists()


@pytest.mark.parametrize("change", ["missing", "writable", "identity", "browser-revision"])
def test_invalid_cache_fails_before_child(tmp_path: Path, change: str) -> None:
    root, perfetto, browser = _copy_harness(tmp_path)
    if change == "missing":
        perfetto = tmp_path / "missing-cache"
    elif change == "writable":
        perfetto.chmod(0o755)
    elif change == "identity":
        identity = perfetto / ".troupe-perfetto-cache.json"
        perfetto.chmod(0o755)
        identity.chmod(0o644)
        value = json.loads(identity.read_text(encoding="utf-8"))
        value["release_commit"] = "wrong"
        _write_json(identity, value)
        _freeze(perfetto)
    else:
        identity = browser / ".troupe-playwright-cache.json"
        browser.chmod(0o755)
        identity.chmod(0o644)
        value = json.loads(identity.read_text(encoding="utf-8"))
        value["archives"][0]["revision"] = "wrong"
        _write_json(identity, value)
        _freeze(browser)

    completed = _run(root, _arguments(perfetto, browser))

    assert completed.returncode == 1
    assert not (root.parent / "fake-child.json").exists()


def test_child_failure_preserves_all_layer_results_and_first_exit(tmp_path: Path) -> None:
    root, perfetto, browser = _copy_harness(tmp_path)

    completed = _run(
        root,
        _arguments(perfetto, browser),
        extra_environment={"TROUPE_FAKE_FAILURES": json.dumps({"sql": 7, "ui": 9})},
    )

    assert completed.returncode == 7
    summary = json.loads(completed.stdout)
    assert summary["result"] == "failed"
    assert summary["compatibility"]["first_failed_layer"] == "sql"
    assert [layer["exit_code"] for layer in summary["compatibility"]["layers"]] == [0, 7, 9]


def test_cache_mutation_by_child_is_blocking(tmp_path: Path) -> None:
    root, perfetto, browser = _copy_harness(tmp_path)

    completed = _run(
        root,
        _arguments(perfetto, browser),
        extra_environment={"TROUPE_FAKE_MUTATE": "perfetto"},
    )

    assert completed.returncode == 1
    assert "root must remain an exact read-only directory" in completed.stderr


def test_malformed_child_summary_is_blocking(tmp_path: Path) -> None:
    root, perfetto, browser = _copy_harness(tmp_path)

    completed = _run(
        root,
        _arguments(perfetto, browser),
        extra_environment={"TROUPE_FAKE_MALFORMED": "1"},
    )

    assert completed.returncode == 1
    assert "T02 compatibility summary is invalid" in completed.stderr


def test_ancestor_script_hash_drift_is_blocking(tmp_path: Path) -> None:
    root, perfetto, browser = _copy_harness(tmp_path)
    with (root / T02_RELATIVE).open("ab") as stream:
        stream.write(b"# drift\n")

    completed = _run(root, _arguments(perfetto, browser))

    assert completed.returncode == 1
    assert "compatibility script hash differs" in completed.stderr
    assert not (root.parent / "fake-child.json").exists()


def test_cache_symlink_is_rejected_before_child(tmp_path: Path) -> None:
    root, perfetto, browser = _copy_harness(tmp_path)
    perfetto.chmod(0o755)
    member = perfetto / "tools.zip"
    member.chmod(0o644)
    payload = member.read_bytes()
    member.unlink()
    target = tmp_path / "outside-tools.zip"
    target.write_bytes(payload)
    member.symlink_to(target)
    perfetto.chmod(0o555)

    completed = _run(root, _arguments(perfetto, browser))

    assert completed.returncode == 1
    assert "must not contain symlinks" in completed.stderr
    assert not (root.parent / "fake-child.json").exists()
