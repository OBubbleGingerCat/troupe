from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any

import pytest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT_RELATIVE = Path("scripts/test_diagnostics_rust_quality.sh")
SCRIPT = ROOT / SCRIPT_RELATIVE
FIXTURE_RELATIVE = Path("tests/fixtures/release/rust-quality.json")
ARTIFACT_RELATIVE = Path("tests/fixtures/artifact_layout/nodes/V10.json")
GATE_RELATIVE = Path("tests/fixtures/diagnostic_node_gates/V10.json")


FAKE_CARGO = r"""#!/usr/bin/env python3
import json
import os
import sys
from pathlib import Path

stage = sys.argv[1]
record = {
    "argv": ["cargo", *sys.argv[1:]],
    "cwd": os.getcwd(),
    "offline": os.environ.get("CARGO_NET_OFFLINE"),
    "http_proxy": os.environ.get("http_proxy"),
    "https_proxy": os.environ.get("https_proxy"),
    "no_proxy": os.environ.get("NO_PROXY"),
}
with Path(os.environ["TROUPE_RUST_QUALITY_LOG"]).open("a", encoding="utf-8") as stream:
    stream.write(json.dumps(record) + "\n")

print(f"{stage}-stdout")
print(f"{stage}-stderr", file=sys.stderr)
if os.environ.get("TROUPE_RUST_QUALITY_MUTATE") == stage:
    mutation = Path(
        os.environ.get(
            "TROUPE_RUST_QUALITY_MUTATE_PATH", "unexpected-quality-mutation"
        )
    )
    with mutation.open("a", encoding="utf-8") as stream:
        stream.write("changed\n")
failures = json.loads(os.environ.get("TROUPE_RUST_QUALITY_FAILURES", "{}"))
raise SystemExit(int(failures.get(stage, 0)))
"""


def _expected_stages() -> list[dict[str, object]]:
    return [
        {
            "name": "fmt",
            "argv": [
                "cargo",
                "fmt",
                "--check",
                "--all",
                "--manifest-path",
                "rust/Cargo.toml",
            ],
        },
        {
            "name": "check",
            "argv": [
                "cargo",
                "check",
                "--locked",
                "--offline",
                "--manifest-path",
                "rust/Cargo.toml",
                "--workspace",
                "--all-targets",
                "--all-features",
            ],
        },
        {
            "name": "clippy",
            "argv": [
                "cargo",
                "clippy",
                "--locked",
                "--offline",
                "--manifest-path",
                "rust/Cargo.toml",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--",
                "-D",
                "warnings",
            ],
        },
        {
            "name": "test",
            "argv": [
                "cargo",
                "test",
                "--locked",
                "--offline",
                "--manifest-path",
                "rust/Cargo.toml",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--no-fail-fast",
            ],
        },
    ]


def _sandbox(tmp_path: Path) -> tuple[Path, dict[str, str], Path]:
    repository = (tmp_path / "repository").resolve()
    script = repository / SCRIPT_RELATIVE
    script.parent.mkdir(parents=True)
    shutil.copy2(SCRIPT, script)
    script.chmod(0o755)
    (repository / "rust").mkdir()
    (repository / "rust/Cargo.toml").write_text("[workspace]\n", encoding="ascii")
    subprocess.run(["git", "init", "-q", str(repository)], check=True)
    subprocess.run(["git", "-C", str(repository), "add", "."], check=True)
    subprocess.run(
        [
            "git",
            "-C",
            str(repository),
            "-c",
            "user.name=V10 Test",
            "-c",
            "user.email=v10@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
        check=True,
    )

    tools = tmp_path / "tools"
    tools.mkdir()
    cargo = tools / "cargo"
    cargo.write_text(FAKE_CARGO, encoding="utf-8")
    cargo.chmod(0o755)
    log = tmp_path / "cargo.jsonl"
    temporary = tmp_path / "temporary"
    temporary.mkdir()
    sentinel = temporary / "caller-owned"
    sentinel.write_text("preserve\n", encoding="ascii")
    environment = dict(os.environ)
    environment.update(
        {
            "PATH": f"{tools}:{environment['PATH']}",
            "TMPDIR": str(temporary),
            "TROUPE_RUST_QUALITY_LOG": str(log),
        }
    )
    return repository, environment, log


def _run(
    repository: Path,
    environment: dict[str, str],
    arguments: list[str] | None = None,
) -> subprocess.CompletedProcess[str]:
    effective_arguments = (
        ["--all", "--locked", "--deny-warnings"]
        if arguments is None
        else arguments
    )
    return subprocess.run(
        [
            str(repository / SCRIPT_RELATIVE),
            *effective_arguments,
        ],
        cwd=repository,
        env=environment,
        check=False,
        capture_output=True,
        text=True,
        timeout=30,
    )


def _records(log: Path) -> list[dict[str, Any]]:
    if not log.exists():
        return []
    return [json.loads(line) for line in log.read_text(encoding="utf-8").splitlines()]


def _summary(result: subprocess.CompletedProcess[str]) -> dict[str, Any]:
    lines = result.stdout.splitlines()
    assert len(lines) == 1
    value = json.loads(lines[0])
    assert isinstance(value, dict)
    return value


def test_checked_in_contract_and_descriptors_are_exact() -> None:
    assert json.loads((ROOT / FIXTURE_RELATIVE).read_text(encoding="utf-8")) == {
        "schema": "troupe.diagnostics.rust-quality.v1",
        "mode": "all",
        "options": {"locked": True, "offline": True, "deny_warnings": True},
        "stages": _expected_stages(),
    }
    assert json.loads((ROOT / ARTIFACT_RELATIVE).read_text(encoding="utf-8")) == {
        "state": "realized",
        "introduced": [
            "scripts/test_diagnostics_rust_quality.sh",
            "tests/fixtures/release/rust-quality.json",
            "tests/unit/test_diagnostics_rust_quality_runner.py",
        ],
        "modified": [],
        "removed": [],
        "generated": [],
    }
    assert json.loads((ROOT / GATE_RELATIVE).read_text(encoding="utf-8")) == {
        "state": "realized",
        "argv": [
            ["pytest", "-q", "tests/unit/test_diagnostics_rust_quality_runner.py"],
            [
                "scripts/test_diagnostics_rust_quality.sh",
                "--all",
                "--locked",
                "--deny-warnings",
            ],
        ],
        "env": {},
        "maturin_features": [],
        "cache_requirements": [],
        "exclusive_resources": [],
    }


def test_success_runs_exact_offline_stage_order_and_emits_one_summary(
    tmp_path: Path,
) -> None:
    repository, environment, log = _sandbox(tmp_path)
    result = _run(repository, environment)

    assert result.returncode == 0
    records = _records(log)
    assert [record["argv"] for record in records] == [
        stage["argv"] for stage in _expected_stages()
    ]
    assert {record["cwd"] for record in records} == {str(repository)}
    assert {record["offline"] for record in records} == {"true"}
    assert {record["http_proxy"] for record in records} == {
        "http://127.0.0.1:9/"
    }
    assert {record["https_proxy"] for record in records} == {
        "http://127.0.0.1:9/"
    }
    assert {record["no_proxy"] for record in records} == {
        "localhost,127.0.0.1,::1"
    }
    summary = _summary(result)
    assert summary["result"] == "passed"
    assert summary["first_failed_stage"] is None
    assert summary["checkout_unchanged"] is True
    assert [stage["name"] for stage in summary["stages"]] == [
        "fmt",
        "check",
        "clippy",
        "test",
    ]
    assert all(stage["status"] == "passed" for stage in summary["stages"])
    assert all(len(stage["stdout_sha256"]) == 64 for stage in summary["stages"])
    assert "fmt-stdout" in result.stderr
    assert "test-stderr" in result.stderr
    assert {path.name for path in Path(environment["TMPDIR"]).iterdir()} == {
        "caller-owned"
    }


def test_all_stages_run_but_first_failure_controls_exit(tmp_path: Path) -> None:
    repository, environment, log = _sandbox(tmp_path)
    environment["TROUPE_RUST_QUALITY_FAILURES"] = json.dumps(
        {"check": 23, "clippy": 29}
    )

    result = _run(repository, environment)

    assert result.returncode == 23
    assert [record["argv"][1] for record in _records(log)] == [
        "fmt",
        "check",
        "clippy",
        "test",
    ]
    summary = _summary(result)
    assert summary["result"] == "failed"
    assert summary["first_failed_stage"] == "check"
    assert [stage["exit_code"] for stage in summary["stages"]] == [0, 23, 29, 0]


def test_checkout_mutation_is_blocking_after_all_stage_results(tmp_path: Path) -> None:
    repository, environment, log = _sandbox(tmp_path)
    environment["TROUPE_RUST_QUALITY_MUTATE"] = "test"

    result = _run(repository, environment)

    assert result.returncode == 1
    assert len(_records(log)) == 4
    summary = _summary(result)
    assert summary["first_failed_stage"] == "checkout"
    assert summary["checkout_unchanged"] is False
    assert all(stage["status"] == "passed" for stage in summary["stages"])


def test_mutation_of_an_already_dirty_tracked_file_is_detected(tmp_path: Path) -> None:
    repository, environment, log = _sandbox(tmp_path)
    manifest = repository / "rust/Cargo.toml"
    manifest.write_text("[workspace]\n# caller-owned dirty state\n", encoding="ascii")
    environment.update(
        {
            "TROUPE_RUST_QUALITY_MUTATE": "test",
            "TROUPE_RUST_QUALITY_MUTATE_PATH": "rust/Cargo.toml",
        }
    )

    result = _run(repository, environment)

    assert result.returncode == 1
    assert len(_records(log)) == 4
    summary = _summary(result)
    assert summary["first_failed_stage"] == "checkout"
    assert summary["checkout_unchanged"] is False
    assert manifest.read_text(encoding="ascii").endswith("changed\n")


@pytest.mark.parametrize(
    "arguments",
    [
        [],
        ["--all"],
        ["--all", "--deny-warnings", "--locked"],
        ["--check", "--locked", "--deny-warnings"],
        ["--all", "--locked", "--deny-warnings", "extra"],
    ],
)
def test_invalid_cli_is_usage_error_without_running_cargo(
    tmp_path: Path, arguments: list[str]
) -> None:
    repository, environment, log = _sandbox(tmp_path)

    result = _run(repository, environment, arguments)

    assert result.returncode == 2
    assert result.stdout == ""
    assert "usage:" in result.stderr
    assert _records(log) == []


def test_runner_contains_no_package_or_external_tool_boundary() -> None:
    source = SCRIPT.read_text(encoding="utf-8")

    for forbidden in (
        "maturin",
        "pytest",
        "mypy",
        "stubtest",
        "playwright",
        "perfetto",
        "npm ",
        "curl ",
        "wget ",
    ):
        assert forbidden not in source
