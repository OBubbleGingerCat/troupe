from __future__ import annotations

import importlib.util
import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Any, Mapping

import pytest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/test_diagnostics_final.sh"
PUBLISHER_SCRIPT = ROOT / "scripts/publish_diagnostics_acceptance.py"
ATTEMPT_ID = "00000000-0000-4000-8000-000000000003"
INTEGRATION_SHA = "1" * 40
CHILD_NAMES = [
    "linux-release",
    "diagnostics-e2e",
    "O00",
    "O01",
    "O02",
    "O03",
    "O04",
    "V11",
    "plan",
    "ownership",
    "generated-diff",
]

spec = importlib.util.spec_from_file_location(
    "diagnostics_acceptance_publisher_for_final_tests", PUBLISHER_SCRIPT
)
assert spec is not None and spec.loader is not None
publisher = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = publisher
spec.loader.exec_module(publisher)


def _schema_target(schema: Mapping[str, Any], reference: str) -> Mapping[str, Any]:
    value: Any = schema
    for key in reference[2:].split("/"):
        value = value[key]
    assert isinstance(value, dict)
    return value


def _minimal(rule: Mapping[str, Any], schema: Mapping[str, Any]) -> Any:
    if "$ref" in rule:
        return _minimal(_schema_target(schema, rule["$ref"]), schema)
    if "const" in rule:
        return rule["const"]
    if "enum" in rule:
        return rule["enum"][0]
    declared = rule.get("type")
    choices = [declared] if isinstance(declared, str) else declared or []
    kind = next((choice for choice in choices if choice != "null"), "null")
    if kind == "object":
        properties = rule.get("properties", {})
        return {
            name: _minimal(properties[name], schema)
            for name in rule.get("required", [])
        }
    if kind == "array":
        values = [_minimal(child, schema) for child in rule.get("prefixItems", [])]
        required = rule.get("minItems", 0)
        item = rule.get("items")
        while len(values) < required:
            assert isinstance(item, dict)
            values.append(_minimal(item, schema))
        return values
    if kind == "string":
        pattern = rule.get("pattern", "")
        if pattern == "^[0-9a-f]{64}$":
            return "a" * 64
        if pattern == "^[0-9a-f]{40}$":
            return INTEGRATION_SHA
        if pattern == "^[0-9]+$":
            return "0"
        if "tar\\.gz" in pattern:
            return "troupe-0.1.0.tar.gz"
        if "\\.whl" in pattern:
            return "troupe-0.1.0-cp310-abi3-manylinux.whl"
        if "_runtime" in pattern:
            return "troupe/_runtime.abi3.so"
        if "4[0-9a-f]" in pattern:
            return ATTEMPT_ID
        return "x" * max(1, rule.get("minLength", 0))
    if kind in {"integer", "number"}:
        return rule.get("minimum", 0)
    if kind == "boolean":
        return False
    return None


def _load_schema(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    assert isinstance(value, dict)
    return value


def _write_json(path: Path, value: Mapping[str, Any]) -> bytes:
    payload = publisher._canonical_json(value) + b"\n"
    path.write_bytes(payload)
    return payload


def _write_source_reports(
    evidence: Path,
    integration_sha: str,
    *,
    mutation: str | None = None,
) -> None:
    evidence.mkdir(parents=True, exist_ok=True)
    identity = publisher._identity(integration_sha)

    performance_schema = _load_schema(publisher.PERFORMANCE_SCHEMA)
    performance = _minimal(performance_schema, performance_schema)
    performance["kind"] = "gate"
    performance["identity"] = identity
    performance["environment"]["cache"]["npm_cache_manifest_sha256"] = "6" * 64
    performance["environment"]["cache"]["browser_cache_manifest_sha256"] = "7" * 64
    performance["result"]["status"] = "passed"
    performance["result"]["violations"] = []
    performance["result"]["result_sha256"] = publisher._performance_result_sha(
        performance
    )
    if mutation == "identity":
        performance["identity"]["integration_sha"] = "2" * 40
    if mutation == "partial":
        del performance["summary"]
    _write_json(evidence / publisher.REPORT_NAMES["performance"], performance)

    wheel_schema = _load_schema(publisher.WHEEL_SCHEMA)
    wheel = _minimal(wheel_schema, wheel_schema)
    wheel["identity"] = identity
    wheel["result"]["status"] = "passed"
    wheel["result"]["result_sha256"] = publisher._result_sha(wheel)
    _write_json(evidence / publisher.REPORT_NAMES["wheel"], wheel)


def _append_json(path: Path, value: Mapping[str, Any]) -> None:
    with path.open("a", encoding="utf-8") as stream:
        stream.write(json.dumps(value, sort_keys=True) + "\n")


def _fake_child(arguments: list[str]) -> int:
    if len(arguments) < 6 or arguments[4] != "--":
        return 2
    index = int(arguments[0])
    name = arguments[1]
    timeout_seconds = int(arguments[2])
    evidence = Path(arguments[3])
    command = arguments[5:]
    if index < 1 or index > len(CHILD_NAMES) or CHILD_NAMES[index - 1] != name:
        return 2
    log_name = os.environ.get("TROUPE_FINAL_FAKE_CHILD_LOG")
    if log_name:
        _append_json(
            Path(log_name),
            {
                "argv": command,
                "evidence": str(evidence),
                "index": index,
                "name": name,
                "timeout_seconds": timeout_seconds,
            },
        )
    behavior = json.loads(os.environ.get("TROUPE_FINAL_FAKE_BEHAVIOR", "{}"))
    selected = behavior.get(str(index), behavior.get(name, 0))
    if not isinstance(selected, int) or not 0 <= selected <= 255:
        return 2
    if index == 1 and selected == 0:
        _write_source_reports(
            evidence,
            os.environ["INTEGRATION_SHA"],
            mutation=os.environ.get("TROUPE_FINAL_FAKE_REPORT_MUTATION"),
        )
    return selected


def _fake_publisher(arguments: list[str]) -> int:
    if len(arguments) < 2 or arguments[0] != "--":
        return 2
    command = arguments[1:]
    log_name = os.environ.get("TROUPE_FINAL_FAKE_PUBLISHER_LOG")
    if log_name:
        _append_json(Path(log_name), {"argv": command})
    code = int(os.environ.get("TROUPE_FINAL_FAKE_PUBLISHER_CODE", "0"))
    if code != 0:
        return code
    try:
        output = Path(command[command.index("--output") + 1])
        descriptor = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        try:
            os.write(descriptor, b'{"fake":true}\n')
            os.fsync(descriptor)
        finally:
            os.close(descriptor)
    except (OSError, ValueError, IndexError):
        return 1
    return 0


def _environment(tmp_path: Path) -> tuple[dict[str, str], Path, Path, Path]:
    temporary = tmp_path / "tmp"
    temporary.mkdir()
    child_log = tmp_path / "children.jsonl"
    publisher_log = tmp_path / "publisher.jsonl"
    environment = dict(os.environ)
    for name in list(environment):
        if name.startswith("TROUPE_FINAL_FAKE_"):
            environment.pop(name)
    environment.update(
        {
            "PYTHONDONTWRITEBYTECODE": "1",
            "TMPDIR": str(temporary),
            "TROUPE_FINAL_FAKE_CHILD_LOG": str(child_log),
            "TROUPE_FINAL_FAKE_PUBLISHER_LOG": str(publisher_log),
        }
    )
    return environment, child_log, publisher_log, temporary


def _arguments(tmp_path: Path) -> tuple[list[str], Path, Path]:
    base = (tmp_path / "evidence").resolve()
    evidence = base / "attempts" / ATTEMPT_ID
    acceptance = base / "accepted.json"
    return (
        [
            "--verify-dispatch",
            "--evidence-root",
            str(evidence),
            "--acceptance-path",
            str(acceptance),
            "--attempt-id",
            ATTEMPT_ID,
            "--integration-sha",
            INTEGRATION_SHA,
        ],
        evidence,
        acceptance,
    )


def _run(
    arguments: list[str], environment: Mapping[str, str]
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [str(SCRIPT), *arguments],
        cwd=ROOT,
        env=dict(environment),
        check=False,
        capture_output=True,
        text=True,
        timeout=30,
    )


def _records(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def _assert_no_runner_temp(temporary: Path) -> None:
    assert not list(temporary.glob("troupe-diagnostics-final.*"))


def test_verify_dispatch_runs_exact_eleven_then_publishes_once(tmp_path: Path) -> None:
    environment, child_log, publisher_log, temporary = _environment(tmp_path)
    arguments, evidence, acceptance = _arguments(tmp_path)

    completed = _run(arguments, environment)

    assert completed.returncode == 0, completed.stderr
    children = _records(child_log)
    assert [child["index"] for child in children] == list(range(1, 12))
    assert [child["name"] for child in children] == CHILD_NAMES
    assert children[0]["argv"] == [
        "scripts/test_linux_release.sh",
        "all",
        "--diagnostics-evidence-root",
        str(evidence),
    ]
    assert children[1]["argv"] == ["scripts/test_diagnostics_e2e.sh", "--all"]
    assert [child["argv"] for child in children[2:8]] == [
        ["scripts/run_diagnostic_node_gate.sh", "O00"],
        ["scripts/run_diagnostic_node_gate.sh", "O01"],
        ["scripts/run_diagnostic_bootstrap_gate.sh", "O02"],
        ["scripts/run_diagnostic_node_gate.sh", "O03"],
        ["scripts/run_diagnostic_bootstrap_gate.sh", "O04"],
        ["scripts/run_diagnostic_bootstrap_gate.sh", "V11"],
    ]
    assert children[8]["argv"] == [
        "python",
        "docs/plan/verify_production_diagnostics_plan.py",
        "--self-test",
        "docs/plan/production-diagnostics-implementation-plan.md",
    ]
    assert children[9]["argv"] == [
        "python",
        "scripts/audit_diagnostic_ownership.py",
        "--all-realized",
        "--base",
        "0" * 40,
    ]
    assert children[10]["argv"] == [
        "git",
        "diff",
        "--exit-code",
        "--",
        "rust/crates/troupe-diagnostics-runtime/assets/generated",
        "frontend/diagnostics/package-lock.json",
    ]
    assert [child["timeout_seconds"] for child in children] == [
        21600,
        7200,
        3600,
        3600,
        1800,
        1800,
        1800,
        1800,
        600,
        600,
        600,
    ]
    publishers = _records(publisher_log)
    assert len(publishers) == 1
    assert publishers[0]["argv"] == [
        "python",
        "scripts/publish_diagnostics_acceptance.py",
        "--evidence-base",
        str(evidence.parent.parent),
        "--attempt-id",
        ATTEMPT_ID,
        "--integration-sha",
        INTEGRATION_SHA,
        "--output",
        str(acceptance),
    ]
    assert acceptance.read_bytes() == b'{"fake":true}\n'
    assert {path.name for path in evidence.iterdir()} == {
        "V05-performance-raw.json",
        "V07-wheel-report.json",
        "V03-final-evidence.json",
    }
    report = json.loads((evidence / "V03-final-evidence.json").read_text())
    schema = _load_schema(publisher.FINAL_SCHEMA)
    publisher.validate_schema(report, schema, "final report")
    assert report["result"]["result_sha256"] == publisher._result_sha(report)
    assert [child["name"] for child in report["children"]] == CHILD_NAMES
    assert all(child["exit_code"] == 0 for child in report["children"])
    _assert_no_runner_temp(temporary)


@pytest.mark.parametrize(
    ("behavior", "expected"),
    [({"3": 23, "5": 29}, 23), ({"1": 41}, 41), ({"11": 130}, 130)],
)
def test_first_child_failure_is_retained_all_children_run_and_publisher_is_forbidden(
    tmp_path: Path, behavior: dict[str, int], expected: int
) -> None:
    environment, child_log, publisher_log, temporary = _environment(tmp_path)
    environment["TROUPE_FINAL_FAKE_BEHAVIOR"] = json.dumps(behavior)
    arguments, evidence, acceptance = _arguments(tmp_path)

    completed = _run(arguments, environment)

    assert completed.returncode == expected
    assert [child["index"] for child in _records(child_log)] == list(range(1, 12))
    assert _records(publisher_log) == []
    assert not acceptance.exists()
    assert not (evidence / "V03-final-evidence.json").exists()
    assert evidence.is_dir()
    _assert_no_runner_temp(temporary)


@pytest.mark.parametrize("mutation", ["identity", "partial"])
def test_invalid_source_report_blocks_final_evidence_and_publisher(
    tmp_path: Path, mutation: str
) -> None:
    environment, child_log, publisher_log, temporary = _environment(tmp_path)
    environment["TROUPE_FINAL_FAKE_REPORT_MUTATION"] = mutation
    arguments, evidence, acceptance = _arguments(tmp_path)

    completed = _run(arguments, environment)

    assert completed.returncode == 1
    assert len(_records(child_log)) == 11
    assert _records(publisher_log) == []
    assert not acceptance.exists()
    assert not (evidence / "V03-final-evidence.json").exists()
    assert (evidence / "V05-performance-raw.json").exists()
    assert (evidence / "V07-wheel-report.json").exists()
    _assert_no_runner_temp(temporary)


def test_publisher_failure_is_propagated_without_retry_or_report_cleanup(
    tmp_path: Path,
) -> None:
    environment, child_log, publisher_log, temporary = _environment(tmp_path)
    environment["TROUPE_FINAL_FAKE_PUBLISHER_CODE"] = "37"
    arguments, evidence, acceptance = _arguments(tmp_path)

    completed = _run(arguments, environment)

    assert completed.returncode == 37
    assert len(_records(child_log)) == 11
    assert len(_records(publisher_log)) == 1
    assert not acceptance.exists()
    assert {path.name for path in evidence.iterdir()} == {
        "V05-performance-raw.json",
        "V07-wheel-report.json",
        "V03-final-evidence.json",
    }
    _assert_no_runner_temp(temporary)


def test_zero_audit_rejects_relative_diagnostic_process_with_repository_cwd(
    tmp_path: Path,
) -> None:
    environment, child_log, publisher_log, temporary = _environment(tmp_path)
    environment["TROUPE_FINAL_FAKE_ZERO_AUDIT"] = "1"
    arguments, evidence, acceptance = _arguments(tmp_path)
    process = subprocess.Popen(
        [
            sys.executable,
            "-c",
            "import time; time.sleep(30)",
            "troupe-diagnostic-relative",
        ],
        cwd=ROOT,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        completed = _run(arguments, environment)
    finally:
        process.terminate()
        process.wait(timeout=5)

    assert completed.returncode == 1
    assert "diagnostic child process remains" in completed.stderr
    assert len(_records(child_log)) == 11
    assert _records(publisher_log) == []
    assert not acceptance.exists()
    assert not (evidence / "V03-final-evidence.json").exists()
    _assert_no_runner_temp(temporary)


@pytest.mark.parametrize("preexisting", ["report", "acceptance"])
def test_fresh_attempt_and_no_overwrite_are_checked_before_dispatch(
    tmp_path: Path, preexisting: str
) -> None:
    environment, child_log, publisher_log, temporary = _environment(tmp_path)
    arguments, evidence, acceptance = _arguments(tmp_path)
    evidence.mkdir(parents=True)
    target = (
        evidence / "V05-performance-raw.json" if preexisting == "report" else acceptance
    )
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text("preserve\n", encoding="utf-8")

    completed = _run(arguments, environment)

    assert completed.returncode == 1
    assert target.read_text(encoding="utf-8") == "preserve\n"
    assert _records(child_log) == []
    assert _records(publisher_log) == []
    _assert_no_runner_temp(temporary)


@pytest.mark.parametrize(
    "arguments",
    [
        [],
        ["--all"],
        ["--verify-dispatch", "--all"],
        ["--verify-dispatch", "--npm-cache", "/tmp"],
        ["--unknown"],
    ],
)
def test_argument_surface_is_closed(tmp_path: Path, arguments: list[str]) -> None:
    environment, child_log, publisher_log, temporary = _environment(tmp_path)

    completed = _run(arguments, environment)

    assert completed.returncode == 2
    assert _records(child_log) == []
    assert _records(publisher_log) == []
    _assert_no_runner_temp(temporary)


if __name__ == "__main__":
    if len(sys.argv) >= 2 and sys.argv[1] == "--fake-child":
        raise SystemExit(_fake_child(sys.argv[2:]))
    if len(sys.argv) >= 2 and sys.argv[1] == "--fake-publisher":
        raise SystemExit(_fake_publisher(sys.argv[2:]))
    raise SystemExit(2)
