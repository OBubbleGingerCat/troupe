#!/usr/bin/env python3
from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import re
import shlex
import stat
import sys
from dataclasses import dataclass
from pathlib import Path
from types import ModuleType
from typing import Any, Final


ROOT: Final = Path(__file__).resolve().parents[1]
DESIGN_RELATIVE: Final = Path("docs/design/production-diagnostics.md")
PLAN_RELATIVE: Final = Path("docs/plan/production-diagnostics-implementation-plan.md")
VALIDATOR_RELATIVE: Final = Path("docs/plan/verify_production_diagnostics_plan.py")
REVIEW_RELATIVE: Final = Path("docs/plan/production-diagnostics-plan-review-record.md")
INDEX_RELATIVE: Final = Path("tests/fixtures/artifact_layout/index.json")
DECISIONS: Final = frozenset(range(1, 55))
NODE_RE: Final = re.compile(r"[A-Z][0-9]{2}")
SHA256_RE: Final = re.compile(r"[0-9a-f]{64}")
SUMMARY_SCHEMA: Final = "troupe.diagnostics.traceability.v1"
HASH_LABELS: Final = {
    "actor_design": (Path("docs/design/actor-agent-session.md"), "Actor Design SHA-256"),
    "diagnostics_design": (DESIGN_RELATIVE, "Diagnostics Design SHA-256"),
    "plan": (PLAN_RELATIVE, "Plan SHA-256"),
    "validator": (VALIDATOR_RELATIVE, "Validator SHA-256"),
}
VALIDATOR_SUMMARY_RE: Final = re.compile(
    r"^- Validator: `(?P<nodes>\d+) nodes; (?P<edges>\d+) direct edges; "
    r"(?P<subprojects>\d+) subprojects; (?P<slots>\d+) slots; "
    r"(?P<shared>\d+) shared paths; (?P<behavior>\d+) behavior owners; "
    r"(?P<families>\d+) parameterized families; (?P<grants>\d+) generated grant; "
    r"self-test passed`$",
    re.MULTILINE,
)


class TraceabilityError(RuntimeError):
    pass


@dataclass(frozen=True, slots=True)
class IndexEntry:
    node_id: str
    artifact: str
    gate: str


@dataclass(frozen=True, slots=True)
class TraceModel:
    node_ids: tuple[str, ...]
    edge_count: int
    decisions: dict[int, tuple[str, ...]]
    gate_argv: dict[str, tuple[tuple[str, ...], ...]]
    artifact_paths: dict[str, frozenset[str]]
    index: tuple[IndexEntry, ...]
    validator_result: tuple[int, int, int, int, int, int, int, int, str]


def _pairs_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise TraceabilityError(f"duplicate JSON field: {key}")
        result[key] = value
    return result


def _load_module(path: Path, name: str) -> ModuleType:
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise TraceabilityError(f"could not load module spec: {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    try:
        spec.loader.exec_module(module)
    except BaseException:
        sys.modules.pop(name, None)
        raise
    return module


def _section(text: str, start: str, end: str) -> str:
    try:
        return text.split(start, 1)[1].split(end, 1)[0]
    except IndexError as error:
        raise TraceabilityError(f"missing section boundary: {start!r} / {end!r}") from error


def _closed_table(body: str, headers: tuple[str, ...]) -> list[tuple[str, ...]]:
    header = "| " + " | ".join(headers) + " |"
    separator = "|" + "|".join("---" for _ in headers) + "|"
    lines = body.splitlines()
    positions = [position for position, line in enumerate(lines) if line == header]
    if len(positions) != 1:
        raise TraceabilityError(
            f"table header occurrence mismatch: {headers!r} count={len(positions)}"
        )
    start = positions[0]
    if start + 1 >= len(lines) or lines[start + 1] != separator:
        raise TraceabilityError(f"table separator mismatch: {headers!r}")
    rows: list[tuple[str, ...]] = []
    for line in lines[start + 2 :]:
        if not line:
            break
        if not line.startswith("|") or not line.endswith("|"):
            raise TraceabilityError(f"malformed table row: {line!r}")
        cells = _split_table_row(line)
        if len(cells) != len(headers) or any(not cell for cell in cells):
            raise TraceabilityError(f"malformed table row: {line!r}")
        rows.append(cells)
    if not rows:
        raise TraceabilityError(f"table has no rows: {headers!r}")
    return rows


def _split_table_row(line: str) -> tuple[str, ...]:
    cells: list[str] = []
    current: list[str] = []
    in_code = False
    for character in line[1:-1]:
        if character == "`":
            in_code = not in_code
            current.append(character)
        elif character == "|" and not in_code:
            cells.append("".join(current).strip())
            current = []
        else:
            current.append(character)
    if in_code:
        raise TraceabilityError(f"unclosed code span in table row: {line!r}")
    cells.append("".join(current).strip())
    return tuple(cells)


def parse_design_decisions(text: str) -> dict[int, str]:
    body = _section(text, "## 2. 已确认的设计决策", "## 3. 总体架构")
    decisions: dict[int, str] = {}
    for raw_id, statement in _closed_table(body, ("ID", "决策")):
        match = re.fullmatch(r"D([1-9][0-9]*)", raw_id)
        if match is None:
            raise TraceabilityError(f"malformed design decision ID: {raw_id!r}")
        decision = int(match.group(1))
        if decision in decisions:
            raise TraceabilityError(f"duplicate design decision: D{decision}")
        decisions[decision] = statement
    if set(decisions) != DECISIONS:
        missing = sorted(DECISIONS - decisions.keys())
        extra = sorted(decisions.keys() - DECISIONS)
        raise TraceabilityError(
            f"design decision coverage mismatch: missing={missing}, extra={extra}"
        )
    return decisions


def parse_plan_decisions(
    text: str,
    validator: ModuleType,
    nodes: set[str],
) -> dict[int, tuple[str, ...]]:
    body = validator.section(text, "## 9. D1-D54 追踪矩阵", "## 10. Plan Freeze")
    decisions: dict[int, tuple[str, ...]] = {}
    for raw_id, raw_owners, acceptance in validator.parse_closed_table(
        body,
        ("Decision", "Owner nodes", "Blocking acceptance"),
    ):
        match = re.fullmatch(r"D([1-9][0-9]*)", raw_id)
        if match is None:
            raise TraceabilityError(f"malformed plan decision ID: {raw_id!r}")
        decision = int(match.group(1))
        if decision in decisions:
            raise TraceabilityError(f"duplicate plan decision: D{decision}")
        owners = tuple(raw_owners.split(", "))
        if not owners or any(NODE_RE.fullmatch(owner) is None for owner in owners):
            raise TraceabilityError(f"D{decision} has a missing or malformed owner")
        if len(owners) != len(set(owners)):
            raise TraceabilityError(f"D{decision} has a duplicate owner")
        unknown = set(owners) - nodes
        if unknown:
            raise TraceabilityError(f"D{decision} has unknown owners: {sorted(unknown)}")
        if not acceptance.strip():
            raise TraceabilityError(f"D{decision} has no blocking acceptance")
        decisions[decision] = owners
    if set(decisions) != DECISIONS:
        missing = sorted(DECISIONS - decisions.keys())
        extra = sorted(decisions.keys() - DECISIONS)
        raise TraceabilityError(
            f"plan decision coverage mismatch: missing={missing}, extra={extra}"
        )
    return decisions


def _project_gate_argv(
    node: str,
    body: str,
    validator: ModuleType,
) -> tuple[tuple[str, ...], ...]:
    match = re.search(r"^- \*\*Gate\*\*：(\S.+)$", body, re.MULTILINE)
    if match is None:
        raise TraceabilityError(f"{node} has no Gate contract")
    gate_text = match.group(1)
    command_text = (
        gate_text.split("descriptor执行", 1)[1]
        if "descriptor执行" in gate_text
        else gate_text
    )
    commands: list[tuple[str, ...]] = []
    for literal in validator.GATE_COMMAND_RE.findall(command_text):
        if not literal.startswith((*validator.GATE_PREFIXES, "pytest ")):
            continue
        try:
            argv = tuple(shlex.split(literal))
        except ValueError as error:
            raise TraceabilityError(f"{node} has malformed Gate argv: {literal!r}") from error
        if not argv:
            raise TraceabilityError(f"{node} has an empty Gate command")
        commands.append(argv)
    if not commands:
        raise TraceabilityError(f"{node} has no automated Gate command")
    if len(commands) != len(set(commands)):
        raise TraceabilityError(f"{node} has a duplicate automated Gate command")
    return tuple(commands)


def parse_index(value: Any) -> tuple[IndexEntry, ...]:
    if not isinstance(value, dict) or set(value) != {"nodes"}:
        raise TraceabilityError("fragment index fields are not exact")
    raw_nodes = value["nodes"]
    if not isinstance(raw_nodes, list) or not raw_nodes:
        raise TraceabilityError("fragment index nodes must be a non-empty list")
    entries: list[IndexEntry] = []
    for position, raw in enumerate(raw_nodes):
        if not isinstance(raw, dict) or set(raw) != {"id", "artifact", "gate"}:
            raise TraceabilityError(f"fragment index node {position} fields are not exact")
        node_id, artifact, gate = raw["id"], raw["artifact"], raw["gate"]
        if not all(isinstance(item, str) for item in (node_id, artifact, gate)):
            raise TraceabilityError(f"fragment index node {position} values must be strings")
        if NODE_RE.fullmatch(node_id) is None:
            raise TraceabilityError(f"fragment index node is malformed: {node_id!r}")
        entries.append(IndexEntry(node_id, artifact, gate))
    ids = [entry.node_id for entry in entries]
    if len(ids) != len(set(ids)):
        raise TraceabilityError("fragment index contains a duplicate node")
    artifact_paths = [entry.artifact for entry in entries]
    gate_paths = [entry.gate for entry in entries]
    if len(artifact_paths) != len(set(artifact_paths)):
        raise TraceabilityError("fragment index contains a duplicate artifact path")
    if len(gate_paths) != len(set(gate_paths)):
        raise TraceabilityError("fragment index contains a duplicate Gate path")
    for entry in entries:
        expected_artifact = (
            f"tests/fixtures/artifact_layout/nodes/{entry.node_id}.json"
        )
        expected_gate = (
            f"tests/fixtures/diagnostic_node_gates/{entry.node_id}.json"
        )
        if entry.artifact != expected_artifact:
            raise TraceabilityError(
                f"fragment index artifact path is not exact for {entry.node_id}"
            )
        if entry.gate != expected_gate:
            raise TraceabilityError(
                f"fragment index Gate path is not exact for {entry.node_id}"
            )
    return tuple(entries)


def build_trace_model(
    design_text: str,
    plan_text: str,
    index_value: Any,
    validator: ModuleType,
) -> TraceModel:
    try:
        dependencies, _titles, _subprojects = validator.parse_index(plan_text)
        graph = validator.parse_graph(plan_text, set(dependencies))
        contracts = validator.parse_contract_blocks(plan_text)
        raw_artifacts = validator.parse_contract_artifact_paths(plan_text)
    except Exception as error:
        raise TraceabilityError(f"plan validation failed: {error}") from error
    node_ids = tuple(dependencies)
    nodes = set(node_ids)
    if set(contracts) != nodes:
        raise TraceabilityError("plan contract nodes differ from the DAG index")
    parse_design_decisions(design_text)
    decisions = parse_plan_decisions(plan_text, validator, nodes)
    gate_argv = {
        node: _project_gate_argv(node, contracts[node][1], validator)
        for node in node_ids
    }
    artifact_paths = {
        node: frozenset(raw_artifacts[node])
        for node in node_ids
    }
    index = parse_index(index_value)
    indexed_ids = tuple(entry.node_id for entry in index)
    if indexed_ids != node_ids:
        missing = sorted(nodes - set(indexed_ids))
        extra = sorted(set(indexed_ids) - nodes)
        raise TraceabilityError(
            f"fragment index differs from DAG nodes: missing={missing}, extra={extra}"
        )
    for decision, owners in decisions.items():
        for owner in owners:
            if not gate_argv[owner]:
                raise TraceabilityError(f"D{decision} owner {owner} has no automated Gate")
            if not artifact_paths[owner]:
                raise TraceabilityError(f"D{decision} owner {owner} has no owned artifact path")
    try:
        raw_result = validator.validate_text(plan_text)
    except Exception as error:
        raise TraceabilityError(f"plan validation failed: {error}") from error
    validator_result = tuple(raw_result)
    if len(validator_result) != 9:
        raise TraceabilityError("plan validator returned a malformed result")
    return TraceModel(
        node_ids=node_ids,
        edge_count=len(graph),
        decisions=decisions,
        gate_argv=gate_argv,
        artifact_paths=artifact_paths,
        index=index,
        validator_result=validator_result,  # type: ignore[arg-type]
    )


def _read_text(path: Path, label: str) -> str:
    try:
        metadata = path.lstat()
        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
            raise TraceabilityError(f"{label} must be a regular file")
        return path.read_text(encoding="utf-8")
    except TraceabilityError:
        raise
    except (OSError, UnicodeError) as error:
        raise TraceabilityError(f"could not read {label}: {error}") from error


def _exact_input(raw: Path, expected: Path, label: str) -> Path:
    try:
        resolved = raw.resolve(strict=True)
    except OSError as error:
        raise TraceabilityError(f"could not resolve {label}: {error}") from error
    if resolved != expected.resolve(strict=True):
        raise TraceabilityError(f"{label} must be the tracked accepted file: {expected}")
    _read_text(resolved, label)
    return resolved


def _read_json(path: Path, label: str) -> Any:
    text = _read_text(path, label)
    try:
        return json.loads(text, object_pairs_hook=_pairs_object)
    except TraceabilityError:
        raise
    except json.JSONDecodeError as error:
        raise TraceabilityError(f"{label} is not valid JSON: {error}") from error


def _review_identity(review: str, root: Path) -> tuple[dict[str, str], tuple[int, ...]]:
    hashes: dict[str, str] = {}
    for key, (relative, label) in HASH_LABELS.items():
        matches = re.findall(
            rf"^- {re.escape(label)}: `([0-9a-f]{{64}})`$",
            review,
            re.MULTILINE,
        )
        if len(matches) != 1:
            raise TraceabilityError(f"review record has no unique {label}")
        digest = hashlib.sha256((root / relative).read_bytes()).hexdigest()
        if digest != matches[0]:
            raise TraceabilityError(f"review record hash differs for {relative}")
        hashes[key] = digest
    summaries = list(VALIDATOR_SUMMARY_RE.finditer(review))
    if len(summaries) != 1:
        raise TraceabilityError("review record has no unique validator count summary")
    summary = summaries[0]
    counts = tuple(
        int(summary.group(name))
        for name in (
            "nodes",
            "edges",
            "subprojects",
            "slots",
            "shared",
            "behavior",
            "families",
            "grants",
        )
    )
    return hashes, counts


def verify_repository(
    design_path: Path,
    plan_path: Path,
    *,
    repository_root: Path = ROOT,
) -> dict[str, Any]:
    root = repository_root.resolve(strict=True)
    design = _exact_input(design_path, root / DESIGN_RELATIVE, "diagnostics design")
    plan = _exact_input(plan_path, root / PLAN_RELATIVE, "implementation plan")
    design_text = _read_text(design, "diagnostics design")
    plan_text = _read_text(plan, "implementation plan")
    index_value = _read_json(root / INDEX_RELATIVE, "implementation fragment index")
    validator = _load_module(root / VALIDATOR_RELATIVE, "_troupe_trace_validator")
    model = build_trace_model(design_text, plan_text, index_value, validator)

    support = root / "tests/support"
    sys.path.insert(0, str(support))
    try:
        from artifact_layout import load_artifact_layout, load_gate_descriptors

        layout = load_artifact_layout(root)
        gates = load_gate_descriptors(root)
    except Exception as error:
        raise TraceabilityError(f"implementation catalog validation failed: {error}") from error
    finally:
        try:
            sys.path.remove(str(support))
        except ValueError:
            pass
    if layout.node_ids != model.node_ids or tuple(gates) != model.node_ids:
        raise TraceabilityError("implementation catalogs differ from DAG node order")
    for node in model.node_ids:
        fragment = layout.fragments[node]
        gate = gates[node]
        if fragment.state != gate.state:
            raise TraceabilityError(f"{node} fragment/Gate lifecycle states differ")
        if gate.state == "realized" and gate.argv != model.gate_argv[node]:
            raise TraceabilityError(f"{node} realized Gate differs from its plan contract")

    ownership = _load_module(
        root / "scripts/audit_diagnostic_ownership.py",
        "_troupe_trace_ownership",
    )
    try:
        projection, ledger = ownership.audit_plan(plan, repository_root=root)
    except Exception as error:
        raise TraceabilityError(f"ownership trace validation failed: {error}") from error
    if projection.node_ids != model.node_ids:
        raise TraceabilityError("ownership projection differs from DAG nodes")
    for node, paths in model.artifact_paths.items():
        for path in paths:
            if node not in projection.path_writers.get(path, ()):
                raise TraceabilityError(
                    f"{node} artifact path has no matching ledger owner: {path}"
                )

    review = _read_text(root / REVIEW_RELATIVE, "plan review record")
    hashes, recorded_counts = _review_identity(review, root)
    validator_counts = tuple(int(value) for value in model.validator_result[:8])
    if validator_counts != recorded_counts:
        raise TraceabilityError(
            f"validator counts differ from review record: {validator_counts} != {recorded_counts}"
        )
    if model.validator_result[8] != hashes["plan"]:
        raise TraceabilityError("validator plan hash differs from the frozen review record")
    try:
        validator.run_self_test(plan_text)
    except Exception as error:
        raise TraceabilityError(f"plan validator mutation self-test failed: {error}") from error

    owner_links = sum(len(owners) for owners in model.decisions.values())
    gate_commands = sum(len(commands) for commands in model.gate_argv.values())
    realized = sum(layout.fragments[node].state == "realized" for node in model.node_ids)
    return {
        "artifact_paths": len(ledger.paths),
        "automated_gate_commands": gate_commands,
        "decision_owner_links": owner_links,
        "decisions": len(model.decisions),
        "edges": model.edge_count,
        "fragments": len(layout.fragments),
        "gate_descriptors": len(gates),
        "hashes": hashes,
        "nodes": len(model.node_ids),
        "realized_nodes": realized,
        "schema": SUMMARY_SCHEMA,
        "status": "passed",
    }


def parse_arguments(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Verify production diagnostics design-to-implementation traceability"
    )
    parser.add_argument("--design", required=True, type=Path)
    parser.add_argument("--plan", required=True, type=Path)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    options = parse_arguments(argv)
    try:
        summary = verify_repository(options.design, options.plan)
    except TraceabilityError as error:
        print(f"diagnostics traceability: {error}", file=sys.stderr)
        return 1
    print(json.dumps(summary, ensure_ascii=True, separators=(",", ":"), sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
