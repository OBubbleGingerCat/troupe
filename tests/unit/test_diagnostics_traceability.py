from __future__ import annotations

import copy
import importlib.util
import json
import os
import re
import stat
import subprocess
import sys
from pathlib import Path
from types import ModuleType

import pytest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/verify_diagnostics_traceability.py"
DESIGN = ROOT / "docs/design/production-diagnostics.md"
PLAN = ROOT / "docs/plan/production-diagnostics-implementation-plan.md"
INDEX = ROOT / "tests/fixtures/artifact_layout/index.json"
CHECKLIST = ROOT / "docs/diagnostics/RELEASE_CHECKLIST.md"


def _module(path: Path, name: str) -> ModuleType:
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


TRACE = _module(SCRIPT, "_test_diagnostics_traceability")
VALIDATOR = _module(
    ROOT / "docs/plan/verify_production_diagnostics_plan.py",
    "_test_diagnostics_traceability_validator",
)


def _inputs() -> tuple[str, str, dict[str, object]]:
    return (
        DESIGN.read_text(encoding="utf-8"),
        PLAN.read_text(encoding="utf-8"),
        json.loads(INDEX.read_text(encoding="utf-8")),
    )


def _model(
    design: str,
    plan: str,
    index: dict[str, object],
) -> object:
    return TRACE.build_trace_model(design, plan, index, VALIDATOR)


def _decision_row(text: str, decision: int) -> str:
    match = re.search(rf"^\| D{decision} \| .+$", text, re.MULTILINE)
    assert match is not None
    return match.group(0)


def _gate_line(text: str, node: str) -> str:
    contracts = VALIDATOR.parse_contract_blocks(text)
    match = re.search(r"^- \*\*Gate\*\*：\S.+$", contracts[node][1], re.MULTILINE)
    assert match is not None
    return match.group(0)


def test_repository_traceability_is_closed() -> None:
    summary = TRACE.verify_repository(DESIGN, PLAN)

    assert summary["schema"] == "troupe.diagnostics.traceability.v1"
    assert summary["status"] == "passed"
    assert summary["decisions"] == 54
    assert summary["nodes"] == 145
    assert summary["edges"] == 254
    assert summary["fragments"] == 145
    assert summary["gate_descriptors"] == 145
    assert summary["realized_nodes"] >= 130
    assert set(summary["hashes"]) == {
        "actor_design",
        "diagnostics_design",
        "plan",
        "validator",
    }


def test_cli_emits_one_canonical_summary() -> None:
    completed = subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            "--design",
            str(DESIGN.relative_to(ROOT)),
            "--plan",
            str(PLAN.relative_to(ROOT)),
        ],
        cwd=ROOT,
        env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"},
        check=False,
        capture_output=True,
        text=True,
        timeout=20,
    )

    assert completed.returncode == 0, completed.stderr
    assert completed.stderr == ""
    lines = completed.stdout.splitlines()
    assert len(lines) == 1
    assert json.loads(lines[0])["status"] == "passed"


def test_missing_and_duplicate_design_decisions_are_rejected() -> None:
    design, plan, index = _inputs()
    row = _decision_row(design, 54)
    with pytest.raises(TRACE.TraceabilityError, match="design decision coverage mismatch"):
        _model(design.replace(row + "\n", "", 1), plan, index)
    with pytest.raises(TRACE.TraceabilityError, match="duplicate design decision"):
        _model(design.replace(row, row + "\n" + row, 1), plan, index)


def test_missing_and_duplicate_fragment_nodes_are_rejected() -> None:
    design, plan, index = _inputs()
    missing = copy.deepcopy(index)
    assert isinstance(missing["nodes"], list)
    missing["nodes"].pop()
    with pytest.raises(TRACE.TraceabilityError, match="fragment index differs"):
        _model(design, plan, missing)

    duplicate = copy.deepcopy(index)
    assert isinstance(duplicate["nodes"], list)
    duplicate["nodes"].append(copy.deepcopy(duplicate["nodes"][0]))
    with pytest.raises(TRACE.TraceabilityError, match="duplicate node"):
        _model(design, plan, duplicate)


def test_missing_and_duplicate_decision_owners_are_rejected() -> None:
    design, plan, index = _inputs()
    row = _decision_row(plan, 1)
    cells = row[1:-1].split("|")
    owners = cells[1].strip()
    missing = row.replace(owners, "-", 1)
    duplicate = row.replace(owners, "C02, " + owners, 1)
    with pytest.raises(TRACE.TraceabilityError, match="missing or malformed owner"):
        _model(design, plan.replace(row, missing, 1), index)
    with pytest.raises(TRACE.TraceabilityError, match="duplicate owner"):
        _model(design, plan.replace(row, duplicate, 1), index)


def test_missing_and_duplicate_automated_gates_are_rejected() -> None:
    design, plan, index = _inputs()
    line = _gate_line(plan, "V11")
    missing = "- **Gate**：仅人工确认。"
    command = "`pytest -q tests/unit/test_diagnostics_traceability.py`"
    assert command in line
    duplicate = line.replace(command, f"{command}和{command}", 1)
    with pytest.raises(TRACE.TraceabilityError, match="no automated Gate command"):
        _model(design, plan.replace(line, missing, 1), index)
    with pytest.raises(TRACE.TraceabilityError, match="duplicate automated Gate"):
        _model(design, plan.replace(line, duplicate, 1), index)


def test_missing_and_duplicate_index_paths_are_rejected() -> None:
    design, plan, index = _inputs()
    missing = copy.deepcopy(index)
    assert isinstance(missing["nodes"], list)
    assert isinstance(missing["nodes"][0], dict)
    del missing["nodes"][0]["artifact"]
    with pytest.raises(TRACE.TraceabilityError, match="fields are not exact"):
        _model(design, plan, missing)

    duplicate = copy.deepcopy(index)
    assert isinstance(duplicate["nodes"], list)
    first = duplicate["nodes"][0]
    second = duplicate["nodes"][1]
    assert isinstance(first, dict) and isinstance(second, dict)
    second["artifact"] = first["artifact"]
    with pytest.raises(TRACE.TraceabilityError, match="duplicate artifact path"):
        _model(design, plan, duplicate)


def test_cli_rejects_noncanonical_planning_inputs(tmp_path: Path) -> None:
    copied = tmp_path / "design.md"
    copied.write_bytes(DESIGN.read_bytes())
    completed = subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            "--design",
            str(copied),
            "--plan",
            str(PLAN),
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
        timeout=10,
    )

    assert completed.returncode == 1
    assert completed.stdout == ""
    assert "must be the tracked accepted file" in completed.stderr


def test_release_checklist_is_executable_and_evidence_only() -> None:
    text = CHECKLIST.read_text(encoding="utf-8")
    assert "scripts/test_diagnostics_final.sh --all" in text
    assert "scripts/run_diagnostic_bootstrap_gate.sh V11" in text
    for evidence in (
        "V05-performance-raw.json",
        "V07-wheel-report.json",
        "V03-final-evidence.json",
        "accepted.json",
    ):
        assert evidence in text
    forbidden = (
        "looks correct",
        "visually inspect",
        "manual approval",
        "人工确认",
        "看起来正确",
        "- [ ]",
    )
    assert not any(value in text for value in forbidden)
    assert stat.S_IMODE(SCRIPT.stat().st_mode) == 0o755
