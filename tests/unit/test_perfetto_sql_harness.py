from __future__ import annotations

import json
import os
import shutil
import stat
import subprocess
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT_RELATIVE = Path("scripts/test_perfetto_sql_compatibility.sh")
SCRIPT = REPO_ROOT / SCRIPT_RELATIVE
SQL_ROOT_RELATIVE = Path("tests/perfetto/sql")
TOOLS_ROOT_RELATIVE = Path("tests/perfetto/tools")
TRACES_RELATIVE = Path("tests/fixtures/perfetto/traces")


def _cache() -> Path:
    raw = os.environ.get("TROUPE_PERFETTO_CACHE")
    assert raw, "TROUPE_PERFETTO_CACHE is required by the T06 Gate"
    cache = Path(raw)
    assert cache.is_absolute()
    return cache.resolve(strict=True)


def _run(
    root: Path = REPO_ROOT,
    *,
    cache: Path | None = None,
    environment: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [
            str(root / SCRIPT_RELATIVE),
            "--offline",
            "--cache",
            str(_cache() if cache is None else cache),
        ],
        cwd=root,
        env=environment,
        check=False,
        capture_output=True,
        text=True,
        timeout=30,
    )


def _copy_harness(tmp_path: Path) -> Path:
    root = tmp_path / "repository"
    for relative in (SCRIPT_RELATIVE, Path("scripts/fetch_pinned_perfetto_tools.sh")):
        target = root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(REPO_ROOT / relative, target)
    for relative in (SQL_ROOT_RELATIVE, TOOLS_ROOT_RELATIVE, TRACES_RELATIVE):
        shutil.copytree(REPO_ROOT / relative, root / relative)
    return root.resolve(strict=True)


def test_official_tool_produces_exact_canonical_report() -> None:
    completed = _run()
    assert completed.returncode == 0, completed.stderr
    assert completed.stderr == ""

    report = json.loads(completed.stdout)
    assert completed.stdout == (
        json.dumps(report, ensure_ascii=True, separators=(",", ":"), sort_keys=True) + "\n"
    )
    assert report["tool"] == {
        "release_commit": "da1d152cff27890903d158fe96751de3aab883cc",
        "release_tag": "v57.2",
        "rpc_api_version": 14,
    }
    fixtures = report["fixtures"]
    assert fixtures["open"]["facts"]["open_slices"] == 1
    assert fixtures["overlap"]["facts"]["overlapping_cue_pairs"] == 1
    assert fixtures["numeric-boundary"]["facts"] == {
        "fallback_counter_tracks": 0,
        "i64_max_counters": 1,
        "non_exact_fallbacks": 1,
        "open_slices": 0,
        "overlapping_cue_pairs": 0,
    }
    assert fixtures["numeric-boundary"]["counters"] == [
        {
            "name": "numeric.i64_max",
            "track_type": "global_counter_track_event",
            "ts": "9223372036854775807",
            "value": "9.223372036854776e+18",
        }
    ]
    assert fixtures["multi-cue"] == fixtures["repeated-dump"]


def test_path_trace_processor_shell_is_never_used(tmp_path: Path) -> None:
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    marker = tmp_path / "path-tool-was-used"
    fake = fake_bin / "trace_processor_shell"
    fake.write_text(
        f"#!/bin/sh\nprintf used > {marker}\nexit 99\n",
        encoding="utf-8",
    )
    fake.chmod(0o700)
    environment = dict(os.environ)
    environment["PATH"] = f"{fake_bin}{os.pathsep}{environment['PATH']}"

    completed = _run(environment=environment)

    assert completed.returncode == 0, completed.stderr
    assert not marker.exists()


def test_fixture_hash_mismatch_is_blocking(tmp_path: Path) -> None:
    root = _copy_harness(tmp_path)
    trace = root / TRACES_RELATIVE / "open.pftrace"
    payload = bytearray(trace.read_bytes())
    payload[-1] ^= 1
    trace.write_bytes(payload)

    completed = _run(root)

    assert completed.returncode == 1
    assert "fixture open hash mismatch" in completed.stderr


def test_missing_official_tool_archive_is_blocking(tmp_path: Path) -> None:
    source = _cache()
    cache = tmp_path / "cache" / source.name
    cache.mkdir(parents=True)
    identity = cache / ".troupe-perfetto-cache.json"
    shutil.copy2(source / identity.name, identity)
    identity.chmod(0o444)
    cache.chmod(0o555)

    try:
        completed = _run(cache=cache.resolve(strict=True))
    finally:
        cache.chmod(0o755)
        identity.chmod(0o644)

    assert completed.returncode == 1
    assert "cache verification failed" in completed.stderr.lower()
    assert "missing" in completed.stderr.lower()


def test_official_tool_archive_hash_mismatch_is_blocking(tmp_path: Path) -> None:
    source = _cache()
    cache = tmp_path / "cache" / source.name
    cache.mkdir(parents=True)
    for name in (".troupe-perfetto-cache.json", f"{source.name}.zip"):
        shutil.copy2(source / name, cache / name)
        (cache / name).chmod(0o444)
    archive = cache / f"{source.name}.zip"
    archive.chmod(0o644)
    with archive.open("r+b") as stream:
        first = stream.read(1)
        stream.seek(0)
        stream.write(bytes([first[0] ^ 1]))
    archive.chmod(0o444)
    cache.chmod(0o555)

    try:
        completed = _run(cache=cache.resolve(strict=True))
    finally:
        cache.chmod(0o755)
        for member in cache.iterdir():
            member.chmod(0o644)

    assert completed.returncode == 1
    assert "cache verification failed" in completed.stderr.lower()
    assert "hash mismatch" in completed.stderr.lower()


def test_query_schema_drift_is_a_tool_failure(tmp_path: Path) -> None:
    root = _copy_harness(tmp_path)
    assertions = root / SQL_ROOT_RELATIVE / "assertions.sql"
    source = assertions.read_text(encoding="utf-8")
    assertions.write_text(
        source.replace("FROM track", "FROM removed_track_table", 1),
        encoding="utf-8",
    )

    completed = _run(root)

    assert completed.returncode == 1
    assert "trace_processor_shell failed for fixture active-watermark" in completed.stderr
    assert "no such table" in completed.stderr.lower()


def test_golden_drift_is_blocking(tmp_path: Path) -> None:
    root = _copy_harness(tmp_path)
    expected_path = root / SQL_ROOT_RELATIVE / "expected.json"
    expected = json.loads(expected_path.read_text(encoding="utf-8"))
    expected["fixtures"]["open"]["facts"]["open_slices"] = 0
    expected_path.write_text(json.dumps(expected, indent=2) + "\n", encoding="utf-8")

    completed = _run(root)

    assert completed.returncode == 1
    assert "fixture open SQL expectation mismatch" in completed.stderr
    assert "$.facts.open_slices" in completed.stderr


def test_script_and_cache_permissions_are_closed() -> None:
    assert stat.S_IMODE(SCRIPT.stat().st_mode) == 0o755
    cache = _cache()
    assert stat.S_IMODE(cache.stat().st_mode) == 0o555
    assert stat.S_IMODE((cache / f"{cache.name}.zip").stat().st_mode) == 0o444
