from __future__ import annotations

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
SCRIPT_RELATIVE = Path("scripts/test_diagnostics_frontend_release.sh")
SCRIPT = ROOT / SCRIPT_RELATIVE
CONTRACT_RELATIVE = Path("tests/fixtures/release/frontend-asset-contract.json")
ARTIFACT_RELATIVE = Path("tests/fixtures/artifact_layout/nodes/V09.json")
GATE_RELATIVE = Path("tests/fixtures/diagnostic_node_gates/V09.json")

UNIT_TESTS = [
    "tests/unit/browser-provisioning.test.ts",
    "tests/unit/bundle-contract.test.ts",
    "tests/unit/generated-assets.test.ts",
    "tests/unit/protocol-controls.test.ts",
    "tests/unit/protocol-events.test.ts",
    "tests/unit/protocol-views.test.ts",
    "tests/unit/state-property.test.ts",
    "tests/unit/state-reducer.test.ts",
    "tests/unit/state-windows.test.ts",
    "tests/unit/timeline-hit-test.test.ts",
    "tests/unit/timeline-layout.test.ts",
    "tests/unit/toolchain.test.ts",
]

FAKE_TOOL = f"""#!{sys.executable}
from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

name = Path(sys.argv[0]).name
record = {{
    "tool": name,
    "argv": [name, *sys.argv[1:]],
    "cwd": os.getcwd(),
    "cargo_offline": os.environ.get("CARGO_NET_OFFLINE"),
    "path": os.environ.get("PATH"),
}}
if name == "cargo":
    blocked = subprocess.run(["node", "--version"], check=False, capture_output=True)
    record["blocked_node_exit"] = blocked.returncode
with Path(os.environ["TROUPE_FRONTEND_RELEASE_LOG"]).open("a", encoding="utf-8") as stream:
    stream.write(json.dumps(record) + "\\n")
mutation = os.environ.get("TROUPE_FRONTEND_RELEASE_MUTATE")
if mutation == name:
    with Path("tracked.txt").open("a", encoding="ascii") as stream:
        stream.write("mutated\\n")
failures = json.loads(os.environ.get("TROUPE_FRONTEND_RELEASE_FAILURES", "{{}}"))
print(f"{{name}}-stdout")
print(f"{{name}}-stderr", file=sys.stderr)
raise SystemExit(int(failures.get(name, 0)))
"""


def _json(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    assert isinstance(value, dict)
    return value


def _sandbox(tmp_path: Path) -> tuple[Path, dict[str, str], Path, Path, Path]:
    repository = (tmp_path / "repository").resolve()
    script = repository / SCRIPT_RELATIVE
    script.parent.mkdir(parents=True)
    shutil.copy2(SCRIPT, script)
    script.chmod(0o755)
    maintainer = repository / "frontend/diagnostics/scripts/maintain.mjs"
    maintainer.parent.mkdir(parents=True)
    maintainer.write_text("// fake maintainer\n", encoding="ascii")
    (repository / "rust").mkdir()
    (repository / "rust/Cargo.toml").write_text("[workspace]\n", encoding="ascii")
    (repository / "tracked.txt").write_text("stable\n", encoding="ascii")
    subprocess.run(["git", "init", "-q", str(repository)], check=True)
    subprocess.run(["git", "-C", str(repository), "add", "."], check=True)
    subprocess.run(
        [
            "git",
            "-C",
            str(repository),
            "-c",
            "user.name=V09 Test",
            "-c",
            "user.email=v09@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
        check=True,
    )

    tools = tmp_path / "tools"
    tools.mkdir()
    for name in ("node", "cargo"):
        target = tools / name
        target.write_text(FAKE_TOOL, encoding="utf-8")
        target.chmod(target.stat().st_mode | stat.S_IXUSR)
    cache = (tmp_path / "npm-cache").resolve()
    cache.mkdir()
    temporary = (tmp_path / "temporary").resolve()
    temporary.mkdir()
    sentinel = temporary / "caller-owned"
    sentinel.write_text("preserve\n", encoding="ascii")
    log = tmp_path / "release.jsonl"
    environment = dict(os.environ)
    environment.update(
        {
            "PATH": f"{tools}:{environment['PATH']}",
            "TROUPE_FRONTEND_RELEASE_LOG": str(log),
            "TROUPE_GATE_TMP": str(temporary),
        }
    )
    return repository, environment, cache, log, sentinel


def _run(
    repository: Path,
    environment: dict[str, str],
    cache: Path,
    arguments: list[str] | None = None,
) -> subprocess.CompletedProcess[str]:
    effective = [
        "--clean",
        "--check-generated",
        "--forbid-update",
        "--npm-cache",
        str(cache),
    ] if arguments is None else arguments
    return subprocess.run(
        [str(repository / SCRIPT_RELATIVE), *effective],
        cwd=repository,
        env=environment,
        check=False,
        capture_output=True,
        text=True,
        timeout=30,
    )


def _records(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def _summary(result: subprocess.CompletedProcess[str]) -> dict[str, Any]:
    lines = result.stdout.splitlines()
    assert len(lines) == 1
    value = json.loads(lines[0])
    assert isinstance(value, dict)
    return value


def test_checked_contract_artifacts_and_bootstrap_descriptor_are_exact() -> None:
    assert _json(ROOT / CONTRACT_RELATIVE) == {
        "schema": "troupe.diagnostics.frontend-assets.v1",
        "toolchain": {
            "node_major": 22,
            "npm_version": "10.9.4",
            "offline_clean_install": True,
        },
        "checks": {
            "strict_typescript": True,
            "unit_tests": UNIT_TESTS,
            "raw_build_repetitions": 2,
            "generated_asset_repetitions": 2,
            "checked_in_assets_exact": True,
            "rust_build_without_frontend_tools": True,
        },
        "shape": {
            "html": 1,
            "javascript": 1,
            "stylesheet": 1,
            "source_maps": 0,
            "relative_full_hash_urls": True,
            "external_requests": 0,
        },
        "budgets": {
            "logical_uncompressed_bytes": 512 * 1024,
            "first_load_brotli_bytes": 160 * 1024,
            "all_embedded_bytes": 768 * 1024,
        },
        "rust_command": [
            "cargo", "build", "--locked", "--offline", "--manifest-path",
            "rust/Cargo.toml", "--package", "troupe-diagnostics-runtime", "--lib",
        ],
    }
    assert _json(ROOT / ARTIFACT_RELATIVE) == {
        "state": "realized",
        "introduced": [
            "scripts/test_diagnostics_frontend_release.sh",
            "tests/fixtures/release/frontend-asset-contract.json",
            "tests/unit/test_diagnostics_frontend_release_runner.py",
        ],
        "modified": [],
        "removed": [],
        "generated": [],
    }
    assert _json(ROOT / GATE_RELATIVE) == {
        "state": "realized",
        "argv": [["pytest", "-q", "tests/unit/test_diagnostics_frontend_release_runner.py"]],
        "env": {},
        "maturin_features": [],
        "cache_requirements": [],
        "exclusive_resources": [],
    }


def test_success_dispatches_one_full_frontend_stage_then_node_free_rust(
    tmp_path: Path,
) -> None:
    repository, environment, cache, log, sentinel = _sandbox(tmp_path)
    result = _run(repository, environment, cache)

    assert result.returncode == 0, result.stderr
    records = _records(log)
    assert [record["tool"] for record in records] == ["node", "cargo"]
    assert records[0]["argv"] == [
        "node",
        "frontend/diagnostics/scripts/maintain.mjs",
        "--npm-cache",
        str(cache),
        "--check-toolchain",
        "--typecheck",
        "--unit",
        ",".join(UNIT_TESTS),
        "--build-raw",
        "--verify-reproducible",
        "--generate-assets",
        "--check",
        "--repeat",
        "2",
    ]
    assert records[1]["argv"] == _json(ROOT / CONTRACT_RELATIVE)["rust_command"]
    assert records[1]["cargo_offline"] == "true"
    assert records[1]["blocked_node_exit"] == 97
    summary = _summary(result)
    assert summary["result"] == "passed"
    assert summary["first_failed_stage"] is None
    assert summary["checkout_unchanged"] is True
    assert [stage["name"] for stage in summary["stages"]] == ["frontend", "rust-embedded"]
    assert sentinel.read_text(encoding="ascii") == "preserve\n"
    assert not list(sentinel.parent.glob("troupe-frontend-release.*"))


def test_first_failure_is_preserved_while_both_stages_are_reported(tmp_path: Path) -> None:
    repository, environment, cache, log, _ = _sandbox(tmp_path)
    environment["TROUPE_FRONTEND_RELEASE_FAILURES"] = json.dumps({"node": 7, "cargo": 8})
    result = _run(repository, environment, cache)

    assert result.returncode == 7
    assert [record["tool"] for record in _records(log)] == ["node", "cargo"]
    summary = _summary(result)
    assert summary["result"] == "failed"
    assert summary["first_failed_stage"] == "frontend"
    assert [stage["exit_code"] for stage in summary["stages"]] == [7, 8]


@pytest.mark.parametrize("tool", ["node", "cargo"])
def test_tracked_mutation_is_detected_even_when_the_tool_returns_success(
    tmp_path: Path,
    tool: str,
) -> None:
    repository, environment, cache, _, _ = _sandbox(tmp_path)
    environment["TROUPE_FRONTEND_RELEASE_MUTATE"] = tool
    result = _run(repository, environment, cache)

    assert result.returncode == 1
    summary = _summary(result)
    assert summary["first_failed_stage"] == "checkout"
    assert summary["checkout_unchanged"] is False


def test_usage_cache_and_clean_checkout_fail_before_any_tool(tmp_path: Path) -> None:
    repository, environment, cache, log, _ = _sandbox(tmp_path)
    bad_usage = _run(repository, environment, cache, ["--clean"])
    assert bad_usage.returncode == 2

    relative = _run(
        repository,
        environment,
        cache,
        ["--clean", "--check-generated", "--forbid-update", "--npm-cache", "relative"],
    )
    assert relative.returncode == 1

    (repository / "frontend/diagnostics/node_modules").mkdir()
    unclean = _run(repository, environment, cache)
    assert unclean.returncode == 1
    assert "clean source tree" in unclean.stderr
    assert _records(log) == []


def test_dirty_checkout_and_repository_local_temp_are_rejected(tmp_path: Path) -> None:
    repository, environment, cache, log, _ = _sandbox(tmp_path)
    (repository / "tracked.txt").write_text("dirty\n", encoding="ascii")
    dirty = _run(repository, environment, cache)
    assert dirty.returncode == 1
    assert "clean tracked checkout" in dirty.stderr

    subprocess.run(["git", "-C", str(repository), "restore", "tracked.txt"], check=True)
    local = repository / "local-tmp"
    local.mkdir()
    subprocess.run(["git", "-C", str(repository), "add", "local-tmp"], check=True)
    environment["TROUPE_GATE_TMP"] = str(local)
    rejected = _run(repository, environment, cache)
    assert rejected.returncode == 1
    assert "outside the repository" in rejected.stderr
    assert _records(log) == []


def test_runner_has_no_browser_wheel_or_update_boundary() -> None:
    source = SCRIPT.read_text(encoding="utf-8")
    for forbidden in (
        "playwright",
        "maturin",
        "pip install",
        "--allow-registry",
        "--provision-package-cache",
    ):
        assert forbidden not in source
    assert "--generate-assets" in source
    assert "--check" in source
    assert "--forbid-update" in source
