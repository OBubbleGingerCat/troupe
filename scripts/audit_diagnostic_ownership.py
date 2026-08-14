#!/usr/bin/env python3
from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import re
import shlex
import subprocess
import sys
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from types import ModuleType
from typing import Any, Final


ROOT = Path(__file__).resolve().parents[1]
SUPPORT = ROOT / "tests" / "support"
sys.path.insert(0, str(SUPPORT))

from artifact_layout import (  # noqa: E402
    ArtifactLayoutError,
    ArtifactFragment,
    GateDescriptor,
    load_artifact_layout,
    load_gate_descriptors,
)


PLAN_PATH: Final = Path("docs/plan/production-diagnostics-implementation-plan.md")
LEDGER_PATH: Final = Path("tests/fixtures/artifact_layout/ownership-ledger.json")
PRODUCT_BASE_RE: Final = re.compile(r"规划基线：`main@(?P<sha>[0-9a-f]{40})`")
NODE_RE: Final = re.compile(r"[A-Z][0-9]{2}")
SHA256_RE: Final = re.compile(r"[0-9a-f]{64}")
ASSET_NAME_RE: Final = re.compile(
    r"diagnostics-(?P<build>[0-9a-f]{64})\.(?P<kind>js|css)\.(?P<encoding>raw|gz|br)"
)
ALLOWED_ROOTS: Final = frozenset(
    {"rust", "src", "tests", "scripts", "frontend", "docs", "examples"}
)
ALLOWED_ROOT_FILES: Final = frozenset({".gitignore", "README.md", "pyproject.toml"})
PLANNING_BUNDLE_PATHS: Final = frozenset(
    {
        "docs/design/actor-agent-session.md",
        "docs/design/production-diagnostics.md",
        "docs/plan/production-diagnostics-implementation-plan.md",
        "docs/plan/verify_production_diagnostics_plan.py",
        "docs/plan/production-diagnostics-plan-review-record.md",
    }
)
FROZEN_HASH_LABELS: Final = {
    "docs/design/actor-agent-session.md": "Actor Design SHA-256",
    "docs/design/production-diagnostics.md": "Diagnostics Design SHA-256",
    "docs/plan/production-diagnostics-implementation-plan.md": "Plan SHA-256",
    "docs/plan/verify_production_diagnostics_plan.py": "Validator SHA-256",
}
LEDGER_FIELDS: Final = frozenset({"paths", "generated_grants"})
PATH_FIELDS: Final = frozenset({"path", "baseline_state", "writers"})
WRITER_FIELDS: Final = frozenset({"node", "role"})
GRANT_FIELDS: Final = frozenset(
    {
        "id",
        "manifest",
        "owner",
        "member_field",
        "exact_parent",
        "filename_template",
        "cardinality",
        "role",
    }
)
WRITER_ROLES: Final = frozenset(
    {"create", "seam", "implement", "assemble", "generate", "remove"}
)
MODIFICATION_ROLES: Final = frozenset({"seam", "implement", "assemble"})
ASSEMBLY_NODES: Final = frozenset({"P04", "T02", "H04", "D07", "W15", "V12", "O04", "V03"})
SEAM_NODES: Final = frozenset({"F04", "F05", "F06"})
REMOVED_PLAN_PATHS: Final = {"rust/src/application/loader.rs": "L00"}


class OwnershipAuditError(RuntimeError):
    pass


@dataclass(frozen=True, slots=True)
class Writer:
    node: str
    role: str


@dataclass(frozen=True, slots=True)
class PathOwnership:
    path: str
    baseline_state: str
    writers: tuple[Writer, ...]


@dataclass(frozen=True, slots=True)
class GeneratedGrant:
    id: str
    manifest: str
    owner: str
    member_field: str
    exact_parent: str
    filename_template: str
    cardinality: int
    role: str


@dataclass(frozen=True, slots=True)
class OwnershipLedger:
    paths: tuple[PathOwnership, ...]
    generated_grants: tuple[GeneratedGrant, ...]

    @property
    def by_path(self) -> dict[str, PathOwnership]:
        return {entry.path: entry for entry in self.paths}


@dataclass(frozen=True, slots=True)
class PlanProjection:
    node_ids: tuple[str, ...]
    dependencies: dict[str, frozenset[str]]
    outgoing: dict[str, frozenset[str]]
    baseline_sha: str
    baseline_paths: frozenset[str]
    path_writers: dict[str, tuple[str, ...]]
    baseline_states: dict[str, str]
    gate_argv: dict[str, tuple[tuple[str, ...], ...]]
    generated_grants: tuple[GeneratedGrant, ...]


_PLAN_VALIDATOR: ModuleType | None = None


def _git(repository_root: Path, *arguments: str, binary: bool = False) -> str | bytes:
    try:
        completed = subprocess.run(
            ["git", *arguments],
            cwd=repository_root,
            check=True,
            capture_output=True,
            text=not binary,
        )
    except subprocess.CalledProcessError as error:
        stderr = error.stderr
        stdout = error.stdout
        if isinstance(stderr, bytes):
            stderr = stderr.decode("utf-8", errors="replace")
        if isinstance(stdout, bytes):
            stdout = stdout.decode("utf-8", errors="replace")
        detail = (stderr or "").strip() or (stdout or "").strip() or str(error)
        raise OwnershipAuditError(detail) from error
    return completed.stdout


def _resolve_commit(repository_root: Path, value: str) -> str:
    resolved = str(
        _git(repository_root, "rev-parse", "--verify", f"{value}^{{commit}}")
    ).strip()
    if not resolved:
        raise OwnershipAuditError(f"could not resolve base commit: {value}")
    result = subprocess.run(
        ["git", "merge-base", "--is-ancestor", resolved, "HEAD"],
        cwd=repository_root,
        check=False,
        capture_output=True,
    )
    if result.returncode != 0:
        raise OwnershipAuditError("ownership audit base is not an ancestor of HEAD")
    return resolved


def _changed_paths(repository_root: Path, base: str) -> dict[str, str]:
    result: dict[str, str] = {}
    output = str(
        _git(
            repository_root,
            "diff",
            "--name-status",
            "--no-renames",
            base,
            "HEAD",
            "--",
        )
    )
    for line in output.splitlines():
        try:
            status, path = line.split("\t", 1)
        except ValueError as error:
            raise OwnershipAuditError(f"malformed git diff entry: {line!r}") from error
        if status not in {"A", "M", "D"}:
            raise OwnershipAuditError(f"unsupported git diff status {status!r} for {path}")
        if path in result:
            raise OwnershipAuditError(f"duplicate git diff path: {path}")
        result[path] = status
    return result


def _pairs_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise OwnershipAuditError(f"duplicate JSON field: {key}")
        result[key] = value
    return result


def _read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=_pairs_object)
    except OwnershipAuditError:
        raise
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise OwnershipAuditError(f"could not read {path}: {error}") from error
    if not isinstance(value, dict):
        raise OwnershipAuditError(f"{path} must contain a JSON object")
    return value


def _closed(value: dict[str, Any], expected: frozenset[str], context: str) -> None:
    actual = set(value)
    if actual != expected:
        raise OwnershipAuditError(
            f"{context} fields are not closed: "
            f"missing={sorted(expected - actual)}, extra={sorted(actual - expected)}"
        )


def _canonical_path(value: str, context: str) -> str:
    if (
        not value
        or value.startswith("/")
        or value.startswith("./")
        or "\\" in value
        or "\x00" in value
        or "\n" in value
        or any(part in {"", ".", ".."} for part in value.split("/"))
    ):
        raise OwnershipAuditError(f"{context} is not a canonical repository path: {value!r}")
    if any(character in value for character in "*?[]{}"):
        raise OwnershipAuditError(f"{context} must not contain a glob or subset: {value!r}")
    path = PurePosixPath(value)
    if value.endswith("/"):
        raise OwnershipAuditError(f"{context} must name a file: {value!r}")
    if len(path.parts) == 1:
        if value not in ALLOWED_ROOT_FILES:
            raise OwnershipAuditError(f"{context} has an unknown repository root: {value!r}")
    elif path.parts[0] not in ALLOWED_ROOTS:
        raise OwnershipAuditError(f"{context} has an unknown repository root: {value!r}")
    return value


def _plan_validator() -> ModuleType:
    global _PLAN_VALIDATOR
    if _PLAN_VALIDATOR is not None:
        return _PLAN_VALIDATOR
    path = ROOT / "docs/plan/verify_production_diagnostics_plan.py"
    spec = importlib.util.spec_from_file_location("production_diagnostics_plan_validator", path)
    if spec is None or spec.loader is None:
        raise OwnershipAuditError("could not load the accepted plan validator")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    _PLAN_VALIDATOR = module
    return module


def _read_plan(plan_path: Path) -> str:
    try:
        return plan_path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise OwnershipAuditError(f"could not read the diagnostics plan: {error}") from error


def _baseline_sha(text: str) -> str:
    match = PRODUCT_BASE_RE.search(text)
    if match is None:
        raise OwnershipAuditError("diagnostics plan has no exact product baseline")
    return match.group("sha")


def _tracked_paths_at(repository_root: Path, commit: str) -> frozenset[str]:
    output = str(_git(repository_root, "ls-tree", "-r", "--name-only", commit))
    return frozenset(path for path in output.splitlines() if path)


def _extract_plan_tables(
    text: str,
    validator: ModuleType,
) -> tuple[dict[str, str], dict[str, tuple[str, ...]], dict[str, str]]:
    slot_body = validator.section(
        text,
        "### 4.1 Compile-safe slot 的有限清单",
        "### 4.2 Shared root",
    )
    slots: dict[str, str] = {}
    for creator, raw_paths in validator.parse_closed_table(
        slot_body, ("Creator", "Exact slot paths")
    ):
        value = validator.strip_code(raw_paths, context=f"{creator} slot paths")
        for path in validator.expand_finite_path(value):
            if path in slots:
                raise OwnershipAuditError(f"duplicate compile-safe slot: {path}")
            slots[path] = creator

    shared_body = validator.section(
        text,
        "### 4.2 Shared root 与 assembly slot 的有序 writer",
        "### 4.3 Slot behavior owner 的完整映射",
    )
    shared: dict[str, tuple[str, ...]] = {}
    for raw_paths, raw_writers, _constraint in validator.parse_closed_table(
        shared_body, ("Exact path", "Ordered writers", "约束")
    ):
        value = validator.strip_code(raw_paths, context="shared writer path")
        writers = tuple(raw_writers.split(" -> "))
        for path in validator.expand_finite_path(value):
            if path in shared:
                raise OwnershipAuditError(f"duplicate shared path: {path}")
            shared[path] = writers

    owner_body = validator.section(
        text,
        "### 4.3 Slot behavior owner 的完整映射",
        "## 5. 节点执行合同",
    )
    behavior: dict[str, str] = {}
    for owner, raw_paths in validator.parse_closed_table(
        owner_body, ("Owner", "Exact slot paths")
    ):
        value = validator.strip_code(raw_paths, context=f"{owner} behavior paths")
        for path in validator.expand_finite_path(value):
            if path in behavior:
                raise OwnershipAuditError(f"duplicate slot behavior path: {path}")
            behavior[path] = owner
    return slots, shared, behavior


def _frozen_generated_grant(validator: ModuleType) -> GeneratedGrant:
    grant, manifest, owner, field, parent, template, cardinality = (
        validator.EXPECTED_GENERATED_GRANT
    )
    return GeneratedGrant(
        id=grant,
        manifest=manifest,
        owner=owner,
        member_field=field,
        exact_parent=parent,
        filename_template=template,
        cardinality=cardinality,
        role="generate",
    )


def _project_gate_argv(
    text: str,
    validator: ModuleType,
) -> dict[str, tuple[tuple[str, ...], ...]]:
    result: dict[str, tuple[tuple[str, ...], ...]] = {}
    for node, (_title, body) in validator.parse_contract_blocks(text).items():
        match = re.search(r"^- \*\*Gate\*\*：(\S.+)$", body, re.MULTILINE)
        if match is None:
            raise OwnershipAuditError(f"{node} is missing its Gate field")
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
                raise OwnershipAuditError(
                    f"{node} Gate contains malformed command quoting: {literal!r}"
                ) from error
            if not argv:
                raise OwnershipAuditError(f"{node} Gate contains an empty command")
            commands.append(argv)
        if not commands:
            raise OwnershipAuditError(f"{node} Gate has no projected descriptor command")
        result[node] = tuple(commands)
    return result


def project_plan(
    plan_path: Path,
    *,
    repository_root: Path = ROOT,
) -> PlanProjection:
    text = _read_plan(plan_path)
    validator = _plan_validator()
    try:
        validator.validate_text(text)
        dependencies, _titles, _subprojects = validator.parse_index(text)
        graph = validator.expected_edges(dependencies)
        _order, raw_outgoing = validator.topological_order(dependencies, graph)
        contract_paths = validator.parse_contract_artifact_paths(text)
        slots, shared, behavior = _extract_plan_tables(text, validator)
    except validator.PlanError as error:
        raise OwnershipAuditError(str(error)) from error

    owners: defaultdict[str, set[str]] = defaultdict(set)
    for node, paths in contract_paths.items():
        for path in paths:
            owners[path].add(node)
    for path, creator in slots.items():
        owners[path].add(creator)
        owners[path].add(behavior.get(path, creator))

    path_writers: dict[str, tuple[str, ...]] = {}
    for path, path_owners in owners.items():
        if path in shared:
            writers = shared[path]
            if set(writers) != path_owners:
                raise OwnershipAuditError(
                    f"shared path writer mismatch: {path} "
                    f"table={list(writers)} projected={sorted(path_owners)}"
                )
        elif path in slots:
            creator = slots[path]
            owner = behavior.get(path, creator)
            writers = (creator,) if creator == owner else (creator, owner)
            if set(writers) != path_owners:
                raise OwnershipAuditError(f"slot writer mismatch: {path}")
        else:
            if len(path_owners) != 1:
                raise OwnershipAuditError(
                    "multi-writer contract artifact is missing from shared inventory: "
                    f"{path} {sorted(path_owners)}"
                )
            writers = (next(iter(path_owners)),)
        path_writers[path] = writers

    node_ids = tuple(dependencies)
    for node in node_ids:
        artifact_path = f"tests/fixtures/artifact_layout/nodes/{node}.json"
        gate_path = f"tests/fixtures/diagnostic_node_gates/{node}.json"
        if artifact_path in path_writers or gate_path in path_writers:
            raise OwnershipAuditError(f"lifecycle family collides with an exact artifact: {node}")
        path_writers[artifact_path] = ("F00",) if node == "F00" else ("F00", node)
        path_writers[gate_path] = ("F00",) if node == "F00" else ("F00", node)

    baseline_sha = _baseline_sha(text)
    baseline_paths = _tracked_paths_at(repository_root, baseline_sha)
    overlap = sorted(set(path_writers) & PLANNING_BUNDLE_PATHS)
    if overlap:
        raise OwnershipAuditError(
            f"accepted planning bundle paths have node owners: {overlap}"
        )

    outgoing = {node: frozenset(children) for node, children in raw_outgoing.items()}
    for path, writers in path_writers.items():
        if len(writers) != len(set(writers)):
            raise OwnershipAuditError(f"projected path repeats a writer: {path}")
        for left, right in zip(writers, writers[1:]):
            if not validator.is_reachable(left, right, raw_outgoing):
                raise OwnershipAuditError(
                    f"projected writer order is not reachable: {path} {left}->{right}"
                )

    grant = _frozen_generated_grant(validator)
    nodes_with_artifacts = {writer for writers in path_writers.values() for writer in writers}
    nodes_with_artifacts.add(grant.owner)
    missing_nodes = sorted(set(node_ids) - nodes_with_artifacts)
    if missing_nodes:
        raise OwnershipAuditError(
            f"nodes have no projected exact artifact or generated grant: {missing_nodes}"
        )
    return PlanProjection(
        node_ids=node_ids,
        dependencies={node: frozenset(values) for node, values in dependencies.items()},
        outgoing=outgoing,
        baseline_sha=baseline_sha,
        baseline_paths=baseline_paths,
        path_writers=dict(sorted(path_writers.items())),
        baseline_states={
            path: "existing" if path in baseline_paths else "planned"
            for path in path_writers
        },
        gate_argv=_project_gate_argv(text, validator),
        generated_grants=(grant,),
    )


def load_ownership_ledger(repository_root: Path = ROOT) -> OwnershipLedger:
    value = _read_json(repository_root / LEDGER_PATH)
    _closed(value, LEDGER_FIELDS, "ownership ledger")
    raw_paths = value["paths"]
    if not isinstance(raw_paths, list) or not raw_paths:
        raise OwnershipAuditError("ownership ledger paths must be a non-empty list")
    node_ids = set(load_artifact_layout(repository_root).node_ids)
    paths: list[PathOwnership] = []
    seen_paths: set[str] = set()
    for position, raw_entry in enumerate(raw_paths):
        if not isinstance(raw_entry, dict):
            raise OwnershipAuditError(f"ownership path {position} must be an object")
        _closed(raw_entry, PATH_FIELDS, f"ownership path {position}")
        path = raw_entry["path"]
        baseline_state = raw_entry["baseline_state"]
        raw_writers = raw_entry["writers"]
        if not isinstance(path, str):
            raise OwnershipAuditError(f"ownership path {position} path must be a string")
        _canonical_path(path, f"ownership path {position}")
        if path in seen_paths:
            raise OwnershipAuditError(f"duplicate ownership path: {path}")
        seen_paths.add(path)
        if baseline_state not in {"existing", "planned"}:
            raise OwnershipAuditError(
                f"ownership path {path} has invalid baseline_state: {baseline_state!r}"
            )
        if not isinstance(raw_writers, list) or not raw_writers:
            raise OwnershipAuditError(f"ownership path {path} writers must be non-empty")
        writers: list[Writer] = []
        seen_writers: set[str] = set()
        for writer_position, raw_writer in enumerate(raw_writers):
            if not isinstance(raw_writer, dict):
                raise OwnershipAuditError(
                    f"ownership path {path} writer {writer_position} must be an object"
                )
            _closed(raw_writer, WRITER_FIELDS, f"ownership path {path} writer {writer_position}")
            node = raw_writer["node"]
            role = raw_writer["role"]
            if not isinstance(node, str) or node not in node_ids:
                raise OwnershipAuditError(f"ownership path {path} has unknown writer: {node!r}")
            if node in seen_writers:
                raise OwnershipAuditError(f"ownership path {path} has a duplicate writer: {node}")
            seen_writers.add(node)
            if not isinstance(role, str) or role not in WRITER_ROLES:
                raise OwnershipAuditError(
                    f"ownership path {path} has invalid writer role: {role!r}"
                )
            writers.append(Writer(node=node, role=role))
        paths.append(
            PathOwnership(
                path=path,
                baseline_state=baseline_state,
                writers=tuple(writers),
            )
        )

    raw_grants = value["generated_grants"]
    if not isinstance(raw_grants, list):
        raise OwnershipAuditError("ownership ledger generated_grants must be a list")
    grants: list[GeneratedGrant] = []
    seen_grants: set[str] = set()
    for position, raw_grant in enumerate(raw_grants):
        if not isinstance(raw_grant, dict):
            raise OwnershipAuditError(f"generated grant {position} must be an object")
        _closed(raw_grant, GRANT_FIELDS, f"generated grant {position}")
        grant_id = raw_grant["id"]
        if not isinstance(grant_id, str) or not grant_id:
            raise OwnershipAuditError(f"generated grant {position} has an invalid id")
        if grant_id in seen_grants:
            raise OwnershipAuditError(f"generated grant is duplicated: {grant_id}")
        seen_grants.add(grant_id)
        strings = {
            field: raw_grant[field]
            for field in (
                "manifest",
                "owner",
                "member_field",
                "exact_parent",
                "filename_template",
                "role",
            )
        }
        if any(not isinstance(item, str) for item in strings.values()):
            raise OwnershipAuditError(f"generated grant {grant_id} fields must be strings")
        if strings["owner"] not in node_ids:
            raise OwnershipAuditError(
                f"generated grant {grant_id} has unknown writer: {strings['owner']!r}"
            )
        cardinality = raw_grant["cardinality"]
        if isinstance(cardinality, bool) or not isinstance(cardinality, int) or cardinality < 1:
            raise OwnershipAuditError(
                f"generated grant {grant_id} has invalid cardinality"
            )
        if strings["role"] != "generate":
            raise OwnershipAuditError(f"generated grant {grant_id} has invalid writer role")
        grants.append(
            GeneratedGrant(
                id=grant_id,
                manifest=strings["manifest"],
                owner=strings["owner"],
                member_field=strings["member_field"],
                exact_parent=strings["exact_parent"],
                filename_template=strings["filename_template"],
                cardinality=cardinality,
                role=strings["role"],
            )
        )
    return OwnershipLedger(paths=tuple(paths), generated_grants=tuple(grants))


def _is_reachable(projection: PlanProjection, source: str, target: str) -> bool:
    pending = [source]
    visited = {source}
    while pending:
        node = pending.pop()
        for child in projection.outgoing[node]:
            if child == target:
                return True
            if child not in visited:
                visited.add(child)
                pending.append(child)
    return False


def validate_projection(projection: PlanProjection, ledger: OwnershipLedger) -> None:
    ledger_paths = ledger.by_path
    projected_paths = set(projection.path_writers)
    if set(ledger_paths) != projected_paths:
        raise OwnershipAuditError(
            "ledger/projected path mismatch: "
            f"missing={sorted(projected_paths - set(ledger_paths))}, "
            f"extra={sorted(set(ledger_paths) - projected_paths)}"
        )
    for path, projected_writers in projection.path_writers.items():
        entry = ledger_paths[path]
        actual_writers = tuple(writer.node for writer in entry.writers)
        if actual_writers != projected_writers:
            raise OwnershipAuditError(
                f"ownership writer order mismatch for {path}: "
                f"ledger={list(actual_writers)} projected={list(projected_writers)}"
            )
        expected_baseline = projection.baseline_states[path]
        if entry.baseline_state != expected_baseline:
            raise OwnershipAuditError(
                f"ownership baseline_state mismatch for {path}: "
                f"{entry.baseline_state} != {expected_baseline}"
            )
        for left, right in zip(actual_writers, actual_writers[1:]):
            if not _is_reachable(projection, left, right):
                raise OwnershipAuditError(
                    f"ownership writers are incomparable for {path}: {left}->{right}"
                )
        for position, writer in enumerate(entry.writers):
            if writer.role == "generate":
                raise OwnershipAuditError(
                    f"static ownership path uses generated role: {path}"
                )
            if writer.role == "create":
                if entry.baseline_state != "planned" or position != 0:
                    raise OwnershipAuditError(
                        f"create role is incompatible with baseline/writer order: {path}"
                    )
            elif entry.baseline_state == "planned" and position == 0:
                raise OwnershipAuditError(
                    f"planned ownership path first writer must create: {path}"
                )
            if writer.role == "remove" and position != len(entry.writers) - 1:
                raise OwnershipAuditError(f"remove role is not the final writer: {path}")
            if writer.role == "remove" and REMOVED_PLAN_PATHS.get(path) != writer.node:
                raise OwnershipAuditError(f"remove role is not plan-authorized: {path}")

    if ledger.generated_grants != projection.generated_grants:
        raise OwnershipAuditError(
            "generated grant mismatch: "
            f"ledger={ledger.generated_grants!r} projected={projection.generated_grants!r}"
        )


def _ancestors(projection: PlanProjection, node: str) -> frozenset[str]:
    result = {node}
    pending = [node]
    while pending:
        candidate = pending.pop()
        for parent in projection.dependencies[candidate]:
            if parent not in result:
                result.add(parent)
                pending.append(parent)
    return frozenset(result)


def _argv_repository_paths(argv: tuple[str, ...]) -> set[str]:
    paths: set[str] = set()
    frontend_relative = (
        len(argv) >= 2
        and argv[0] == "node"
        and argv[1] == "frontend/diagnostics/scripts/maintain.mjs"
    )
    for token in argv:
        value = token.split("=", 1)[1] if token.startswith("--") and "=" in token else token
        for candidate in value.split(","):
            candidate = candidate.split("::", 1)[0]
            if not candidate or "$" in candidate:
                continue
            if frontend_relative and candidate.startswith("tests/"):
                candidate = "frontend/diagnostics/" + candidate
            if not (
                candidate.split("/", 1)[0] in ALLOWED_ROOTS
                or candidate in ALLOWED_ROOT_FILES
            ):
                continue
            paths.add(_canonical_path(candidate, "gate descriptor path"))
    return paths


def _validate_gate_paths(
    projection: PlanProjection,
    ledger: OwnershipLedger,
    node: str,
    gate: GateDescriptor,
) -> None:
    allowed_nodes = _ancestors(projection, node)
    for argv in gate.argv:
        for path in _argv_repository_paths(argv):
            if path in projection.baseline_paths or path in PLANNING_BUNDLE_PATHS:
                continue
            entry = ledger.by_path.get(path)
            if entry is None:
                raise OwnershipAuditError(
                    f"{node} Gate path has no baseline/artifact owner: {path}"
                )
            owners = {writer.node for writer in entry.writers}
            if not owners & allowed_nodes:
                raise OwnershipAuditError(
                    f"{node} Gate path is owned only by non-ancestors: "
                    f"{path} owners={sorted(owners)}"
                )


def _lifecycle_paths(node_ids: tuple[str, ...]) -> frozenset[str]:
    return frozenset(
        {
            f"tests/fixtures/artifact_layout/nodes/{node}.json"
            for node in node_ids
        }
        | {
            f"tests/fixtures/diagnostic_node_gates/{node}.json"
            for node in node_ids
        }
    )


def _category_paths(fragment: ArtifactFragment) -> dict[str, str]:
    result: dict[str, str] = {}
    for category, paths in (
        ("introduced", fragment.introduced),
        ("modified", fragment.modified),
        ("removed", tuple(item.path for item in fragment.removed)),
    ):
        for path in paths:
            if path in result:
                raise OwnershipAuditError(f"artifact path repeats across categories: {path}")
            result[path] = category
    return result


def _expected_category(role: str) -> str:
    if role == "create":
        return "introduced"
    if role in MODIFICATION_ROLES:
        return "modified"
    if role == "remove":
        return "removed"
    raise OwnershipAuditError(f"static artifact has unsupported role: {role}")


def validate_lifecycle(
    projection: PlanProjection,
    ledger: OwnershipLedger,
    repository_root: Path,
    *,
    all_realized: bool = False,
    check_checkout: bool = False,
) -> None:
    try:
        layout = load_artifact_layout(repository_root)
        gates = load_gate_descriptors(repository_root)
    except ArtifactLayoutError as error:
        raise OwnershipAuditError(str(error)) from error
    if layout.node_ids != projection.node_ids or tuple(gates) != projection.node_ids:
        raise OwnershipAuditError("lifecycle index differs from the plan projection")
    realized_artifacts = {
        node for node, fragment in layout.fragments.items() if fragment.state == "realized"
    }
    realized_gates = {node for node, gate in gates.items() if gate.state == "realized"}
    if realized_artifacts != realized_gates:
        raise OwnershipAuditError(
            "artifact/gate lifecycle states differ: "
            f"artifacts={sorted(realized_artifacts)} gates={sorted(realized_gates)}"
        )
    mandatory = {"F00", "F01", "W00", "F02"}
    if not mandatory <= realized_artifacts:
        raise OwnershipAuditError(
            f"mandatory ownership lifecycle nodes are not realized: "
            f"{sorted(mandatory - realized_artifacts)}"
        )
    if all_realized and realized_artifacts != set(projection.node_ids):
        raise OwnershipAuditError(
            f"not all lifecycle nodes are realized: "
            f"{sorted(set(projection.node_ids) - realized_artifacts)}"
        )
    for node in realized_artifacts:
        missing_dependencies = projection.dependencies[node] - realized_artifacts
        if missing_dependencies:
            raise OwnershipAuditError(
                f"realized node {node} has planned dependencies: {sorted(missing_dependencies)}"
            )

    lifecycle_paths = _lifecycle_paths(projection.node_ids)
    ledger_by_path = ledger.by_path
    grants_by_owner: defaultdict[str, set[str]] = defaultdict(set)
    for grant in ledger.generated_grants:
        grants_by_owner[grant.owner].add(grant.id)
    for node in realized_artifacts:
        fragment = layout.fragments[node]
        actual_categories = _category_paths(fragment)
        expected_paths: dict[str, str] = {}
        for entry in ledger.paths:
            if entry.path in lifecycle_paths:
                continue
            writer = next((writer for writer in entry.writers if writer.node == node), None)
            if writer is not None:
                expected_paths[entry.path] = _expected_category(writer.role)
        if set(actual_categories) != set(expected_paths):
            raise OwnershipAuditError(
                f"artifact writer union differs for {node}: "
                f"missing={sorted(set(expected_paths) - set(actual_categories))}, "
                f"extra={sorted(set(actual_categories) - set(expected_paths))}"
            )
        wrong_categories = sorted(
            path
            for path in expected_paths
            if actual_categories[path] != expected_paths[path]
        )
        if wrong_categories:
            raise OwnershipAuditError(
                f"artifact role/category mismatch for {node}: {wrong_categories}"
            )
        expected_grants = grants_by_owner.get(node, set())
        if set(fragment.generated) != expected_grants:
            raise OwnershipAuditError(
                f"artifact generated grant union differs for {node}: "
                f"{sorted(fragment.generated)} != {sorted(expected_grants)}"
            )
        _validate_gate_paths(projection, ledger, node, gates[node])
        if gates[node].argv != projection.gate_argv[node]:
            raise OwnershipAuditError(
                f"realized gate descriptor argv differs from the plan for {node}: "
                f"descriptor={gates[node].argv!r} plan={projection.gate_argv[node]!r}"
            )

    if check_checkout:
        for path, entry in ledger_by_path.items():
            realized_writers = [
                writer for writer in entry.writers if writer.node in realized_artifacts
            ]
            if realized_writers != list(entry.writers[: len(realized_writers)]):
                raise OwnershipAuditError(f"realized writer prefix is not closed for {path}")
            exists = (repository_root / path).is_file()
            expected_exists = entry.baseline_state == "existing"
            if realized_writers:
                expected_exists = realized_writers[-1].role != "remove"
            if exists != expected_exists:
                raise OwnershipAuditError(
                    f"checkout state differs from ownership lifecycle for {path}: "
                    f"exists={exists} expected={expected_exists}"
                )


def _validate_planning_bundle(
    repository_root: Path,
    projection: PlanProjection,
) -> None:
    overlap = sorted(set(ledger_path for ledger_path in projection.path_writers) & PLANNING_BUNDLE_PATHS)
    if overlap:
        raise OwnershipAuditError(f"accepted planning bundle has node owners: {overlap}")
    output = str(_git(repository_root, "ls-files", "--", *sorted(PLANNING_BUNDLE_PATHS)))
    tracked = set(output.splitlines())
    if tracked != set(PLANNING_BUNDLE_PATHS):
        raise OwnershipAuditError(
            "accepted planning bundle files are not tracked exactly: "
            f"missing={sorted(PLANNING_BUNDLE_PATHS - tracked)}"
        )
    review_path = repository_root / "docs/plan/production-diagnostics-plan-review-record.md"
    try:
        review = review_path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise OwnershipAuditError(f"could not read the plan review record: {error}") from error
    for path, label in FROZEN_HASH_LABELS.items():
        matches = re.findall(rf"^- {re.escape(label)}: `([0-9a-f]{{64}})`$", review, re.MULTILINE)
        if len(matches) != 1:
            raise OwnershipAuditError(f"review record has no unique frozen hash for {path}")
        try:
            digest = hashlib.sha256((repository_root / path).read_bytes()).hexdigest()
        except OSError as error:
            raise OwnershipAuditError(f"could not hash accepted planning input {path}") from error
        if digest != matches[0]:
            raise OwnershipAuditError(
                f"accepted planning bundle hash differs for {path}: {digest} != {matches[0]}"
            )


def audit_plan(
    plan_path: Path,
    *,
    repository_root: Path = ROOT,
    all_realized: bool = False,
) -> tuple[PlanProjection, OwnershipLedger]:
    expected_plan = (repository_root / PLAN_PATH).resolve()
    try:
        actual_plan = plan_path.resolve(strict=True)
    except OSError as error:
        raise OwnershipAuditError(f"could not resolve the accepted diagnostics plan: {error}") from error
    if actual_plan != expected_plan:
        raise OwnershipAuditError(
            f"plan-only audit requires the tracked accepted plan: {expected_plan}"
        )
    projection = project_plan(plan_path, repository_root=repository_root)
    ledger = load_ownership_ledger(repository_root)
    validate_projection(projection, ledger)
    validate_lifecycle(
        projection,
        ledger,
        repository_root,
        all_realized=all_realized,
        check_checkout=True,
    )
    _validate_planning_bundle(repository_root, projection)
    return projection, ledger


def _regular_file_without_symlink_parent(
    repository_root: Path,
    relative: str,
    context: str,
) -> Path:
    root = repository_root.resolve(strict=True)
    candidate = root
    for segment in PurePosixPath(relative).parts:
        candidate = candidate / segment
        if candidate.is_symlink():
            raise OwnershipAuditError(f"{context} contains a symlink: {relative}")
    try:
        resolved = candidate.resolve(strict=True)
        resolved.relative_to(root)
    except (OSError, ValueError) as error:
        raise OwnershipAuditError(f"{context} escapes the repository: {relative}") from error
    if not resolved.is_file():
        raise OwnershipAuditError(f"{context} is not a regular file: {relative}")
    return resolved


def _generated_members(repository_root: Path, grant: GeneratedGrant) -> tuple[str, ...]:
    manifest_path = _regular_file_without_symlink_parent(
        repository_root,
        grant.manifest,
        f"generated grant {grant.id} manifest",
    )
    manifest = _read_json(manifest_path)
    raw_files = manifest.get("files")
    if not isinstance(raw_files, list) or len(raw_files) != grant.cardinality:
        raise OwnershipAuditError(
            f"generated grant {grant.id} manifest cardinality differs: "
            f"{len(raw_files) if isinstance(raw_files, list) else 'invalid'}"
        )
    members: list[str] = []
    build_hashes: set[str] = set()
    combinations: set[tuple[str, str]] = set()
    for position, raw_file in enumerate(raw_files):
        if not isinstance(raw_file, dict) or not isinstance(raw_file.get("path"), str):
            raise OwnershipAuditError(
                f"generated grant {grant.id} files[{position}].path is malformed"
            )
        path = _canonical_path(raw_file["path"], f"generated grant {grant.id} member")
        if not path.startswith(grant.exact_parent):
            raise OwnershipAuditError(f"generated grant {grant.id} member has wrong parent: {path}")
        basename = path.removeprefix(grant.exact_parent)
        match = ASSET_NAME_RE.fullmatch(basename)
        if match is None:
            raise OwnershipAuditError(f"generated grant {grant.id} member name is invalid: {path}")
        build_hashes.add(match.group("build"))
        combinations.add((match.group("kind"), match.group("encoding")))
        member_path = _regular_file_without_symlink_parent(
            repository_root,
            path,
            f"generated grant {grant.id} member",
        )
        declared_sha = raw_file.get("sha256")
        if not isinstance(declared_sha, str) or SHA256_RE.fullmatch(declared_sha) is None:
            raise OwnershipAuditError(
                f"generated grant {grant.id} member SHA is malformed: {path}"
            )
        if hashlib.sha256(member_path.read_bytes()).hexdigest() != declared_sha:
            raise OwnershipAuditError(
                f"generated grant {grant.id} member SHA differs: {path}"
            )
        members.append(path)
    expected_combinations = {
        (kind, encoding)
        for kind in ("js", "css")
        for encoding in ("raw", "gz", "br")
    }
    if len(set(members)) != grant.cardinality or len(build_hashes) != 1 or combinations != expected_combinations:
        raise OwnershipAuditError(f"generated grant {grant.id} members are not exact")
    return tuple(sorted(members))


def _legacy_audit_node(node_id: str, base: str, repository_root: Path) -> None:
    try:
        layout = load_artifact_layout(repository_root)
        gates = load_gate_descriptors(repository_root)
    except ArtifactLayoutError as error:
        raise OwnershipAuditError(str(error)) from error
    if node_id not in layout.fragments:
        raise OwnershipAuditError(f"unknown diagnostic node: {node_id}")
    fragment = layout.fragments[node_id]
    if fragment.state != "realized" or gates[node_id].state != "realized":
        raise OwnershipAuditError(f"artifact and gate lifecycle must both be realized for {node_id}")
    if fragment.generated:
        raise OwnershipAuditError("generated grants require the F02 ownership ledger audit")
    resolved_base = _resolve_commit(repository_root, base)
    expected = _expected_fragment_diff(fragment)
    actual = _changed_paths(repository_root, resolved_base)
    lifecycle_paths = (
        {
            f"tests/fixtures/artifact_layout/nodes/{candidate}.json"
            for candidate in layout.node_ids
        }
        | {
            f"tests/fixtures/diagnostic_node_gates/{candidate}.json"
            for candidate in layout.node_ids
        }
        if node_id == "F00"
        else {
            f"tests/fixtures/artifact_layout/nodes/{node_id}.json",
            f"tests/fixtures/diagnostic_node_gates/{node_id}.json",
        }
    )
    lifecycle_status = "A" if node_id == "F00" else "M"
    for path in lifecycle_paths:
        if actual.get(path) != lifecycle_status:
            raise OwnershipAuditError(f"node {node_id} lifecycle files are not exact")
        del actual[path]
    _compare_diff(node_id, expected, actual)
    _validate_removed_preimages(repository_root, resolved_base, fragment)


def _expected_fragment_diff(fragment: ArtifactFragment) -> dict[str, str]:
    expected: dict[str, str] = {}
    for status, paths in (
        ("A", fragment.introduced),
        ("M", fragment.modified),
        ("D", tuple(item.path for item in fragment.removed)),
    ):
        for path in paths:
            if path in expected:
                raise OwnershipAuditError(f"artifact path appears in multiple categories: {path}")
            expected[path] = status
    return expected


def _compare_diff(node_id: str, expected: dict[str, str], actual: dict[str, str]) -> None:
    if actual == expected:
        return
    missing = sorted(set(expected) - set(actual))
    extra = sorted(set(actual) - set(expected))
    wrong = sorted(
        path for path in set(actual) & set(expected) if actual[path] != expected[path]
    )
    raise OwnershipAuditError(
        f"node {node_id} diff is not exact: "
        f"missing={missing}, extra={extra}, wrong_status={wrong}"
    )


def _validate_removed_preimages(
    repository_root: Path,
    base: str,
    fragment: ArtifactFragment,
) -> None:
    for removed in fragment.removed:
        try:
            previous = _git(repository_root, "show", f"{base}:{removed.path}", binary=True)
        except OwnershipAuditError as error:
            raise OwnershipAuditError(
                f"removed path did not exist at base: {removed.path}"
            ) from error
        assert isinstance(previous, bytes)
        if hashlib.sha256(previous).hexdigest() != removed.sha256:
            raise OwnershipAuditError(f"removed path preimage hash differs: {removed.path}")


def audit_node(node_id: str, base: str) -> None:
    repository_root = ROOT
    plan_path = repository_root / PLAN_PATH
    if not plan_path.is_file() or not (repository_root / LEDGER_PATH).is_file():
        _legacy_audit_node(node_id, base, repository_root)
        return
    projection, ledger = audit_plan(plan_path, repository_root=repository_root)
    if node_id not in projection.dependencies:
        raise OwnershipAuditError(f"unknown diagnostic node: {node_id}")
    resolved_base = _resolve_commit(repository_root, base)
    layout = load_artifact_layout(repository_root)
    gates = load_gate_descriptors(repository_root)
    fragment = layout.fragments[node_id]
    gate = gates[node_id]
    if fragment.state == "planned" and gate.state == "planned":
        head = str(_git(repository_root, "rev-parse", "HEAD")).strip()
        if resolved_base != head or _changed_paths(repository_root, resolved_base):
            raise OwnershipAuditError(
                f"planned dispatch audit for {node_id} requires base=HEAD and an empty diff"
            )
        realized = {
            candidate
            for candidate, value in layout.fragments.items()
            if value.state == "realized"
        }
        missing = projection.dependencies[node_id] - realized
        if missing:
            raise OwnershipAuditError(
                f"node {node_id} is not ready; planned dependencies={sorted(missing)}"
            )
        return
    if fragment.state != "realized" or gate.state != "realized":
        raise OwnershipAuditError(f"artifact and gate lifecycle must advance together for {node_id}")

    expected = _expected_fragment_diff(fragment)
    for grant in ledger.generated_grants:
        if grant.owner == node_id:
            for path in _generated_members(repository_root, grant):
                if path in expected:
                    raise OwnershipAuditError(f"generated member overlaps a static artifact: {path}")
                expected[path] = "A"
    lifecycle = (
        {
            f"tests/fixtures/artifact_layout/nodes/{candidate}.json"
            for candidate in projection.node_ids
        }
        | {
            f"tests/fixtures/diagnostic_node_gates/{candidate}.json"
            for candidate in projection.node_ids
        }
        if node_id == "F00"
        else {
            f"tests/fixtures/artifact_layout/nodes/{node_id}.json",
            f"tests/fixtures/diagnostic_node_gates/{node_id}.json",
        }
    )
    for path in lifecycle:
        if path in expected:
            raise OwnershipAuditError(f"lifecycle path leaks into artifact fragment: {path}")
        expected[path] = "A" if node_id == "F00" else "M"
    actual = _changed_paths(repository_root, resolved_base)
    _compare_diff(node_id, expected, actual)
    _validate_removed_preimages(repository_root, resolved_base, fragment)


def audit_all_realized(base: str, plan_path: Path) -> None:
    projection, ledger = audit_plan(
        plan_path,
        repository_root=ROOT,
        all_realized=True,
    )
    resolved_base = _resolve_commit(ROOT, base)
    if resolved_base != projection.baseline_sha:
        raise OwnershipAuditError(
            f"all-realized audit base differs from PRODUCT_BASE_SHA: "
            f"{resolved_base} != {projection.baseline_sha}"
        )
    layout = load_artifact_layout(ROOT)
    expected: dict[str, str] = {path: "A" for path in PLANNING_BUNDLE_PATHS}
    for entry in ledger.paths:
        status = "M" if entry.baseline_state == "existing" else "A"
        if entry.writers[-1].role == "remove":
            status = "D"
        expected[entry.path] = status
    for grant in ledger.generated_grants:
        for path in _generated_members(ROOT, grant):
            if path in expected:
                raise OwnershipAuditError(f"generated member overlaps a static artifact: {path}")
            expected[path] = "A"
    actual = _changed_paths(ROOT, resolved_base)
    _compare_diff("all-realized", expected, actual)
    for fragment in layout.fragments.values():
        _validate_removed_preimages(ROOT, resolved_base, fragment)


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Audit diagnostics artifact ownership")
    modes = parser.add_mutually_exclusive_group(required=True)
    modes.add_argument("--node")
    modes.add_argument("--plan-only", action="store_true")
    modes.add_argument("--all-realized", action="store_true")
    parser.add_argument("--base")
    parser.add_argument("--plan", default=str(PLAN_PATH))
    return parser


def main(argv: list[str] | None = None) -> int:
    arguments = _parser().parse_args(argv)
    plan_path = Path(arguments.plan)
    if not plan_path.is_absolute():
        plan_path = ROOT / plan_path
    try:
        if arguments.plan_only:
            if arguments.base is not None:
                raise OwnershipAuditError("--plan-only does not accept --base")
            audit_plan(plan_path, repository_root=ROOT)
        elif arguments.all_realized:
            if arguments.base is None:
                raise OwnershipAuditError("--all-realized requires --base")
            audit_all_realized(arguments.base, plan_path)
        else:
            if arguments.base is None:
                raise OwnershipAuditError("--node requires --base")
            audit_node(arguments.node, arguments.base)
    except (ArtifactLayoutError, OwnershipAuditError) as error:
        print(f"diagnostic ownership audit: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
