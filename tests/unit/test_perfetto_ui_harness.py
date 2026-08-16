from __future__ import annotations

import json
import os
import shutil
import stat
import subprocess
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT_RELATIVE = Path("scripts/test_perfetto_ui_compatibility.sh")
SCRIPT = REPO_ROOT / SCRIPT_RELATIVE
UI_ROOT_RELATIVE = Path("tests/perfetto/ui")
TRACES_RELATIVE = Path("tests/fixtures/perfetto/traces")
PROJECTION_RELATIVE = Path("tests/fixtures/perfetto/projection")
FIXTURE_NAMES = (
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


def _cache(name: str) -> Path:
    raw = os.environ.get(name)
    assert raw, f"{name} is required by the T07 Gate"
    cache = Path(raw)
    assert cache.is_absolute()
    return cache.resolve(strict=True)


def _environment() -> dict[str, str]:
    environment = dict(os.environ)
    environment["TROUPE_NPM_CACHE"] = str(_cache("TROUPE_NPM_CACHE"))
    return environment


def _run(
    root: Path = REPO_ROOT,
    *,
    perfetto_cache: Path | None = None,
    browser_cache: Path | None = None,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [
            str(root / SCRIPT_RELATIVE),
            "--offline",
            "--cache",
            str(_cache("TROUPE_PERFETTO_CACHE") if perfetto_cache is None else perfetto_cache),
            "--browser-cache",
            str(_cache("TROUPE_PLAYWRIGHT_CACHE") if browser_cache is None else browser_cache),
        ],
        cwd=root,
        env=_environment(),
        check=False,
        capture_output=True,
        text=True,
        timeout=30,
    )


def _copy_preflight_harness(tmp_path: Path) -> Path:
    root = tmp_path / "repository"
    script = root / SCRIPT_RELATIVE
    script.parent.mkdir(parents=True)
    shutil.copy2(SCRIPT, script)
    shutil.copytree(REPO_ROOT / UI_ROOT_RELATIVE, root / UI_ROOT_RELATIVE)
    shutil.copytree(REPO_ROOT / TRACES_RELATIVE, root / TRACES_RELATIVE)
    projection = root / PROJECTION_RELATIVE
    projection.mkdir(parents=True)
    for name in ("manifest.json", "expected-trace.pb"):
        shutil.copy2(REPO_ROOT / PROJECTION_RELATIVE / name, projection / name)
    return root.resolve(strict=True)


def test_manifest_exactly_references_t03_and_projection_fixtures() -> None:
    ui = json.loads((REPO_ROOT / UI_ROOT_RELATIVE / "fixtures.manifest.json").read_text())
    t03 = json.loads((REPO_ROOT / TRACES_RELATIVE / "manifest.json").read_text())
    projection = json.loads((REPO_ROOT / PROJECTION_RELATIVE / "manifest.json").read_text())

    assert ui["schema"] == "troupe.perfetto.ui-fixtures.v1"
    assert [entry["name"] for entry in ui["files"]] == list(FIXTURE_NAMES)
    assert [
        (entry["path"], entry["sha256"])
        for entry in ui["files"]
    ] == [
        (f"tests/fixtures/perfetto/traces/{entry['path']}", entry["sha256"])
        for entry in t03["files"]
    ]
    assert {entry["coverage"] for entry in ui["files"]} == set(FIXTURE_NAMES)
    assert ui["files"][3]["sha256"] == ui["files"][8]["sha256"]
    assert ui["flow_probe"]["path"] == "tests/fixtures/perfetto/projection/expected-trace.pb"
    assert ui["flow_probe"]["sha256"] == projection["files"]["expected-trace.pb"]["sha256"]
    assert all(value > 0 for value in ui["flow_probe"]["counts"].values())


def test_pixel_oracle_and_failure_detectors_are_closed() -> None:
    oracle = json.loads((REPO_ROOT / UI_ROOT_RELATIVE / "pixel-oracle.json").read_text())
    assert oracle["viewport"] == {"width": 1440, "height": 1000, "device_scale_factor": 1}
    assert set(oracle["fixtures"]) == {"projection-flow"}
    probe = oracle["fixtures"]["projection-flow"]
    assert probe["minimum_canvas_count"] >= 4
    assert [entry["index"] for entry in probe["canvases"]] == [0, 1]
    assert all(entry["minimum_opaque_pixels"] > 0 for entry in probe["canvases"])
    assert all(len(entry["key_pixels"]) == 1 for entry in probe["canvases"])

    source = (REPO_ROOT / UI_ROOT_RELATIVE / "trace.spec.ts").read_text(encoding="utf-8")
    assert 'test("failure detectors reject blank canvas, console error, and load timeout"' in source
    assert "assertCanvasMetrics(\"synthetic\"" in source
    assert "assertNoPageErrors(\"synthetic\"" in source
    assert "waitForTraceLoaded(timeoutPage, \"never-loaded\", 75)" in source


def test_fixture_hash_mismatch_is_blocking_before_browser_start(tmp_path: Path) -> None:
    root = _copy_preflight_harness(tmp_path)
    trace = root / TRACES_RELATIVE / "open.pftrace"
    payload = bytearray(trace.read_bytes())
    payload[-1] ^= 1
    trace.write_bytes(payload)

    completed = _run(root)

    assert completed.returncode == 1
    assert "fixture open hash mismatch" in completed.stderr


def test_missing_fixture_is_blocking_before_browser_start(tmp_path: Path) -> None:
    root = _copy_preflight_harness(tmp_path)
    (root / TRACES_RELATIVE / "empty.pftrace").unlink()

    completed = _run(root)

    assert completed.returncode == 1
    assert "fixture empty must be an existing regular file" in completed.stderr


def _copy_perfetto_identity(tmp_path: Path) -> tuple[Path, Path]:
    source = _cache("TROUPE_PERFETTO_CACHE")
    cache = tmp_path / "cache" / source.name
    cache.mkdir(parents=True)
    identity = cache / ".troupe-perfetto-cache.json"
    shutil.copy2(source / identity.name, identity)
    return source, cache.resolve(strict=True)


def test_missing_official_ui_archive_is_blocking(tmp_path: Path) -> None:
    _source, cache = _copy_perfetto_identity(tmp_path)
    (cache / ".troupe-perfetto-cache.json").chmod(0o444)
    cache.chmod(0o555)
    try:
        completed = _run(perfetto_cache=cache)
    finally:
        cache.chmod(0o755)
        (cache / ".troupe-perfetto-cache.json").chmod(0o644)

    assert completed.returncode == 1
    assert "Perfetto UI archive must be an existing regular file" in completed.stderr


def test_official_ui_archive_hash_mismatch_is_blocking(tmp_path: Path) -> None:
    source, cache = _copy_perfetto_identity(tmp_path)
    archive = cache / "perfetto-ui.zip"
    shutil.copy2(source / archive.name, archive)
    archive.chmod(0o644)
    with archive.open("r+b") as stream:
        first = stream.read(1)
        stream.seek(0)
        stream.write(bytes([first[0] ^ 1]))
    for member in cache.iterdir():
        member.chmod(0o444)
    cache.chmod(0o555)
    try:
        completed = _run(perfetto_cache=cache)
    finally:
        cache.chmod(0o755)
        for member in cache.iterdir():
            member.chmod(0o644)

    assert completed.returncode == 1
    assert "Perfetto UI archive hash mismatch" in completed.stderr


def test_script_arguments_and_permissions_are_closed() -> None:
    assert stat.S_IMODE(SCRIPT.stat().st_mode) == 0o755
    completed = subprocess.run(
        [str(SCRIPT), "--offline"],
        cwd=REPO_ROOT,
        env=_environment(),
        check=False,
        capture_output=True,
        text=True,
        timeout=10,
    )
    assert completed.returncode == 2
    assert "--cache is required" in completed.stderr
