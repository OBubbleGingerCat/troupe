#!/usr/bin/env python3
"""Validate the production diagnostics implementation plan."""

from __future__ import annotations

import argparse
import hashlib
import re
import shlex
import subprocess
import sys
from collections import defaultdict, deque
from functools import lru_cache
from pathlib import Path


NODE_RE = re.compile(r"[A-Z]\d{2}")
INDEX_ROW_RE = re.compile(
    r"\| (?P<node>[A-Z]\d{2}) \| (?P<title>.*?) \| "
    r"(?P<subproject>.*?) \| (?P<dependencies>[^|]+) \|$"
)
EDGE_RE = re.compile(r"(?P<source>[A-Z]\d{2}) --> (?P<target>[A-Z]\d{2})")
CONTRACT_RE = re.compile(r"^#### (?P<node>[A-Z]\d{2}) - (?P<title>.+)$", re.MULTILINE)
DECISION_ROW_RE = re.compile(
    r"^\| D(?P<decision>\d+) \| (?P<owners>[^|]+) \| (?P<acceptance>[^|]+) \|$",
    re.MULTILINE,
)
REQUIRED_CONTRACT_FIELDS = ("产物", "验收", "Gate", "边界")
GATE_COMMAND_RE = re.compile(r"`(?P<command>[^`\n]+)`")
GATE_PREFIXES = (
    "cargo ",
    "git ",
    "node ",
    "python ",
    "scripts/",
    "uv ",
)
ARTIFACT_SHORTHANDS = (
    "core crate `",
    "runtime crate `",
    "perfetto crate `",
    "agent crate `",
    "native crate `",
    "native`",
)
SLOT_ROW_RE = re.compile(
    r"^\| (?P<creator>F0[456]) \| `(?P<path>[^`]+)` \|$", re.MULTILINE
)
SHARED_WRITER_ROW_RE = re.compile(
    r"^\| `(?P<path>[^`]+)` \| (?P<writers>[^|]+) \| (?P<constraint>[^|]+) \|$",
    re.MULTILINE,
)
SLOT_OWNER_ROW_RE = re.compile(
    r"^\| (?P<owner>[A-Z]\d{2}) \| `(?P<path>[^`]+)` \|$", re.MULTILINE
)
SCHEDULE_ROW_RE = re.compile(
    r"^\| (?P<tick>\d{2}) \| (?P<nodes>[A-Z]\d{2}(?:, [A-Z]\d{2})*) \|$",
    re.MULTILINE,
)
ARTIFACT_FRAGMENT_FAMILY = "tests/fixtures/artifact_layout/nodes/<node-id>.json"
GATE_DESCRIPTOR_FAMILY = "tests/fixtures/diagnostic_node_gates/<node-id>.json"
EXPECTED_PARAMETERIZED_FAMILIES = {
    ARTIFACT_FRAGMENT_FAMILY: (
        "artifact-fragment",
        "state,introduced,modified,removed,generated",
        "F00",
        "<node-id>",
        "index-exact",
    ),
    GATE_DESCRIPTOR_FAMILY: (
        "gate-descriptor",
        "state,argv,env,maturin_features,cache_requirements,exclusive_resources",
        "F00",
        "<node-id>",
        "index-exact",
    ),
}
EXPECTED_EXCLUSIVE_RESOURCES = {"V05": "benchmark-host"}
EXPECTED_GENERATED_GRANT = (
    "G01",
    "rust/crates/troupe-diagnostics-runtime/assets/generated/manifest.json",
    "W14",
    "files[].path",
    "rust/crates/troupe-diagnostics-runtime/assets/generated/",
    "diagnostics-<sha256>.{js,css}.{raw,gz,br}",
    6,
)
EXPECTED_EXACT_ARTIFACTS = {
    "W06": {
        "frontend/diagnostics/scripts/build.mjs",
        "frontend/diagnostics/tests/unit/bundle-contract.test.ts",
    },
    "V00": {
        "frontend/diagnostics/tests/e2e/visual/diagnostics.spec.ts",
        "frontend/diagnostics/tests/e2e/visual/viewports.ts",
        "frontend/diagnostics/tests/e2e/visual/pixel-oracle.json",
        "frontend/diagnostics/tests/e2e/visual/screenshot-manifest.json",
        "scripts/test_diagnostics_visual.sh",
        *{
            "frontend/diagnostics/tests/e2e/visual/baselines/"
            f"{engine}-{viewport}-{profile}.png"
            for engine in ("chromium", "firefox", "webkit")
            for viewport in ("desktop", "mobile")
            for profile in ("active", "archive")
        },
    },
}
ALLOWED_ARTIFACT_PREFIXES = (
    "rust/",
    "src/troupe/",
    "tests/",
    "scripts/",
    "frontend/",
    "docs/",
    "examples/",
)
ALLOWED_ARTIFACT_ROOT_FILES = {".gitignore", "README.md", "pyproject.toml"}
EXPECTED_PRODUCT_BASE_SHA = "16c3c9a5a9040916f1f8c7d709dff372204ebd3c"
PLANNING_BUNDLE_PATHS = {
    "docs/design/actor-agent-session.md",
    "docs/design/production-diagnostics.md",
    "docs/plan/production-diagnostics-implementation-plan.md",
    "docs/plan/verify_production_diagnostics_plan.py",
    "docs/plan/production-diagnostics-plan-review-record.md",
}


class PlanError(ValueError):
    pass


def section(text: str, start: str, end: str | None = None) -> str:
    try:
        body = text.split(start, 1)[1]
    except IndexError as error:
        raise PlanError(f"missing section marker: {start}") from error
    if end is None:
        return body
    try:
        return body.split(end, 1)[0]
    except IndexError as error:
        raise PlanError(f"missing section marker: {end}") from error


def parse_index(
    text: str,
) -> tuple[dict[str, set[str]], dict[str, str], dict[str, str]]:
    body = section(text, "### 3.1 节点索引", "### 3.2 依赖图")
    dependencies: dict[str, set[str]] = {}
    titles: dict[str, str] = {}
    subprojects: dict[str, str] = {}
    for node, title, subproject, raw_dependencies in parse_closed_table(
        body, ("ID", "最小步骤", "Subproject", "Depends on")
    ):
        if NODE_RE.fullmatch(node) is None:
            raise PlanError(f"malformed index node: {node!r}")
        if node in dependencies:
            raise PlanError(f"duplicate index node: {node}")
        dependencies[node] = (
            set() if raw_dependencies == "-" else set(raw_dependencies.split(","))
        )
        if any(NODE_RE.fullmatch(value) is None for value in dependencies[node]):
            raise PlanError(f"{node} has malformed dependencies: {raw_dependencies!r}")
        titles[node] = title
        subprojects[node] = subproject
    if not dependencies:
        raise PlanError("node index is empty")
    for node, direct_dependencies in dependencies.items():
        unknown = direct_dependencies - dependencies.keys()
        if unknown:
            raise PlanError(f"{node} has unknown dependencies: {sorted(unknown)}")
        if node in direct_dependencies:
            raise PlanError(f"{node} depends on itself")
    return dependencies, titles, subprojects


def parse_graph(text: str, nodes: set[str]) -> set[tuple[str, str]]:
    body = section(text, "```mermaid", "```")
    if "flowchart LR" not in body:
        raise PlanError("Mermaid graph must be a left-to-right flowchart")
    edges = set(EDGE_RE.findall(body))
    mentioned = {value for edge in edges for value in edge}
    for line in body.splitlines():
        candidate = line.strip()
        if NODE_RE.fullmatch(candidate):
            mentioned.add(candidate)
    unknown = mentioned - nodes
    if unknown:
        raise PlanError(f"Mermaid graph has unknown nodes: {sorted(unknown)}")
    missing = nodes - mentioned
    if missing:
        raise PlanError(f"Mermaid graph omits nodes: {sorted(missing)}")
    return edges


def expected_edges(dependencies: dict[str, set[str]]) -> set[tuple[str, str]]:
    return {
        (dependency, node)
        for node, direct_dependencies in dependencies.items()
        for dependency in direct_dependencies
    }


def topological_order(
    dependencies: dict[str, set[str]], edges: set[tuple[str, str]]
) -> tuple[list[str], dict[str, set[str]]]:
    outgoing = {node: set() for node in dependencies}
    indegree = {node: len(direct) for node, direct in dependencies.items()}
    for source, target in edges:
        outgoing[source].add(target)
    ready = deque(sorted(node for node, degree in indegree.items() if degree == 0))
    order: list[str] = []
    while ready:
        node = ready.popleft()
        order.append(node)
        for target in sorted(outgoing[node]):
            indegree[target] -= 1
            if indegree[target] == 0:
                ready.append(target)
        ready = deque(sorted(ready))
    if len(order) != len(dependencies):
        cyclic = sorted(node for node, degree in indegree.items() if degree > 0)
        raise PlanError(f"DAG contains a cycle involving: {cyclic}")
    return order, outgoing


def is_reachable_without_edge(
    source: str,
    target: str,
    skipped: tuple[str, str],
    outgoing: dict[str, set[str]],
) -> bool:
    pending = [source]
    visited = {source}
    while pending:
        node = pending.pop()
        for child in outgoing[node]:
            if (node, child) == skipped:
                continue
            if child == target:
                return True
            if child not in visited:
                visited.add(child)
                pending.append(child)
    return False


def is_reachable(source: str, target: str, outgoing: dict[str, set[str]]) -> bool:
    pending = [source]
    visited = {source}
    while pending:
        node = pending.pop()
        for child in outgoing[node]:
            if child == target:
                return True
            if child not in visited:
                visited.add(child)
                pending.append(child)
    return False


def validate_graph(dependencies: dict[str, set[str]], graph_edges: set[tuple[str, str]]) -> None:
    wanted = expected_edges(dependencies)
    if graph_edges != wanted:
        missing = sorted(wanted - graph_edges)
        extra = sorted(graph_edges - wanted)
        raise PlanError(f"index/Mermaid edge mismatch: missing={missing}, extra={extra}")
    _, outgoing = topological_order(dependencies, graph_edges)
    redundant = sorted(
        edge
        for edge in graph_edges
        if is_reachable_without_edge(edge[0], edge[1], edge, outgoing)
    )
    if redundant:
        raise PlanError(f"DAG contains transitive edges: {redundant}")
    roots = sorted(node for node, direct in dependencies.items() if not direct)
    sinks = sorted(node for node, children in outgoing.items() if not children)
    if roots != ["F00"]:
        raise PlanError(f"unexpected roots: {roots}")
    if sinks != ["V03"]:
        raise PlanError(f"unexpected sinks: {sinks}")


def validate_verification_dependency_contracts(
    dependencies: dict[str, set[str]],
) -> None:
    expected = {
        "V02": {"X02"},
        "V15": {"T02"},
        "V03": {"O04", "V01", "V16"},
    }
    for node, direct_dependencies in expected.items():
        actual = dependencies.get(node)
        if actual != direct_dependencies:
            raise PlanError(
                "verification dependency contract drift: "
                f"{node} expected={sorted(direct_dependencies)} "
                f"actual={sorted(actual) if actual is not None else None}"
            )
    required_ancestors = {"B16": {"B12", "B17", "B18"}}
    for node, required in required_ancestors.items():
        ancestors: set[str] = set()
        pending = list(dependencies[node])
        while pending:
            dependency = pending.pop()
            if dependency in ancestors:
                continue
            ancestors.add(dependency)
            pending.extend(dependencies[dependency])
        missing = sorted(required - ancestors)
        if missing:
            raise PlanError(
                "implementation dependency closure drift: "
                f"{node} missing_ancestors={missing}"
            )


def validate_subprojects(
    subprojects: dict[str, str], edges: set[tuple[str, str]], nodes: set[str]
) -> int:
    outgoing = {node: set() for node in nodes}
    for source, target in edges:
        outgoing[source].add(target)
    grouped: defaultdict[str, list[str]] = defaultdict(list)
    for node, subproject in subprojects.items():
        if not subproject.strip():
            raise PlanError(f"{node} has an empty subproject")
        grouped[subproject].append(node)
    for subproject, members in grouped.items():
        for index, left in enumerate(members):
            for right in members[index + 1 :]:
                if not is_reachable(left, right, outgoing) and not is_reachable(
                    right, left, outgoing
                ):
                    raise PlanError(
                        "same-subproject nodes can run concurrently: "
                        f"{subproject} contains {left} and {right}"
                    )
    return len(grouped)


def validate_subproject_table(text: str, subprojects: dict[str, str]) -> None:
    body = section(
        text, "## 4. Subproject 与路径所有权", "### 4.1 Compile-safe slot 的有限清单"
    )
    listed: dict[str, str] = {}
    for match in re.finditer(
        r"`(?P<subproject>[a-z0-9-]+)` \("
        r"(?P<nodes>[A-Z]\d{2}(?:,[A-Z]\d{2})*)\)",
        body,
    ):
        for node in match.group("nodes").split(","):
            if node in listed:
                raise PlanError(f"subproject table repeats node: {node}")
            listed[node] = match.group("subproject")
    if listed != subprojects:
        missing = sorted(subprojects.keys() - listed.keys())
        extra = sorted(listed.keys() - subprojects.keys())
        wrong = sorted(
            node
            for node in subprojects.keys() & listed.keys()
            if subprojects[node] != listed[node]
        )
        raise PlanError(
            "subproject table/index mismatch: "
            f"missing={missing}, extra={extra}, wrong={wrong}"
        )


def parse_contract_blocks(text: str) -> dict[str, tuple[str, str]]:
    body = section(text, "## 5. 节点执行合同", "## 6. DAG 调度")
    matches = list(CONTRACT_RE.finditer(body))
    contracts: dict[str, tuple[str, str]] = {}
    for index, match in enumerate(matches):
        node = match.group("node")
        if node in contracts:
            raise PlanError(f"duplicate contract: {node}")
        end = matches[index + 1].start() if index + 1 < len(matches) else len(body)
        contracts[node] = (match.group("title"), body[match.end() : end])
    return contracts


def parse_closed_table(
    body: str,
    headers: tuple[str, ...],
    *,
    separators: tuple[str, ...] | None = None,
) -> list[tuple[str, ...]]:
    """Parse one exact Markdown machine table and reject malformed rows."""
    header = "| " + " | ".join(headers) + " |"
    separator_values = separators or tuple("---" for _ in headers)
    if len(separator_values) != len(headers):
        raise PlanError(f"machine table separator arity mismatch: {headers!r}")
    separator = "|" + "|".join(separator_values) + "|"
    lines = body.splitlines()
    positions = [index for index, line in enumerate(lines) if line == header]
    if len(positions) != 1:
        raise PlanError(
            f"machine table header occurrence mismatch: {headers!r} count={len(positions)}"
        )
    start = positions[0]
    if start + 1 >= len(lines) or lines[start + 1] != separator:
        raise PlanError(f"machine table separator mismatch: {headers!r}")
    rows: list[tuple[str, ...]] = []
    for line in lines[start + 2 :]:
        if not line:
            break
        if not line.startswith("|") or not line.endswith("|"):
            raise PlanError(f"malformed machine table row: {line!r}")
        cells = tuple(cell.strip() for cell in line[1:-1].split("|"))
        if len(cells) != len(headers) or any(not cell for cell in cells):
            raise PlanError(f"malformed machine table row: {line!r}")
        rows.append(cells)
    if not rows:
        raise PlanError(f"machine table has no rows: {headers!r}")
    return rows


def strip_code(value: str, *, context: str) -> str:
    if len(value) < 2 or not value.startswith("`") or not value.endswith("`"):
        raise PlanError(f"{context} must be one exact code literal: {value!r}")
    inner = value[1:-1]
    if not inner or "`" in inner:
        raise PlanError(f"{context} has malformed code literal: {value!r}")
    return inner


def validate_artifact_path_literal(node: str, raw_path: str) -> list[str]:
    if raw_path in EXPECTED_PARAMETERIZED_FAMILIES:
        if node != "F00":
            raise PlanError(f"{node} artifact uses a lifecycle family owned by F00: {raw_path}")
        return []
    if raw_path == "files[].path":
        if node != "W14":
            raise PlanError(f"{node} artifact uses the W14-only manifest field")
        return []
    if raw_path.startswith("tests/fixtures/diagnostic_node_gates/"):
        raise PlanError(
            f"{node} artifact leaks a concrete gate descriptor into ownership: {raw_path}"
        )
    if raw_path.startswith("/") or raw_path.startswith("./"):
        raise PlanError(f"{node} artifact path is not canonical repository-relative: {raw_path}")
    if "\\" in raw_path or "//" in raw_path:
        raise PlanError(f"{node} artifact path is not canonical repository-relative: {raw_path}")
    if any(character in raw_path for character in "*?[]"):
        raise PlanError(f"{node} artifact contains a glob: {raw_path}")
    if re.search(r"<[^>]+>", raw_path):
        raise PlanError(f"{node} artifact has an unregistered placeholder: {raw_path}")
    segments = raw_path.split("/")
    if any(segment in {"", ".", ".."} for segment in segments):
        raise PlanError(f"{node} artifact path is not canonical repository-relative: {raw_path}")
    if not (
        raw_path.startswith(ALLOWED_ARTIFACT_PREFIXES)
        or raw_path in ALLOWED_ARTIFACT_ROOT_FILES
    ):
        raise PlanError(f"{node} artifact has an unknown repository root: {raw_path}")
    paths = expand_finite_path(raw_path)
    for path in paths:
        if path.endswith("/"):
            raise PlanError(f"{node} artifact contains a directory key: {path}")
        if any(segment in {"", ".", ".."} for segment in path.split("/")):
            raise PlanError(f"{node} artifact expansion is not canonical: {path}")
        if path.startswith("tests/") and path.endswith(".rs"):
            raise PlanError(f"{node} artifact has an ambiguous crate-local test path: {path}")
    return paths


def parse_contract_artifact_paths(text: str) -> dict[str, set[str]]:
    artifacts: dict[str, set[str]] = {}
    for node, (_, body) in parse_contract_blocks(text).items():
        match = re.search(r"^- \*\*产物\*\*：(\S.+)$", body, re.MULTILINE)
        if match is None:
            raise PlanError(f"{node} is missing its artifact field")
        exact_paths: set[str] = set()
        literals = GATE_COMMAND_RE.findall(match.group(1))
        if not literals:
            raise PlanError(f"{node} artifact has no exact repository path")
        for raw_path in literals:
            for path in validate_artifact_path_literal(node, raw_path):
                if path in exact_paths:
                    raise PlanError(f"{node} artifact repeats an expanded path: {path}")
                exact_paths.add(path)
        if not exact_paths and node not in {"F00", "W14"}:
            raise PlanError(f"{node} artifact has no exact repository path")
        artifacts[node] = exact_paths
    return artifacts


def contains_direct_npm(value: str) -> bool:
    if re.search(
        r"(?<![A-Za-z0-9_.-])(?:[A-Za-z0-9_.-]+/)*(?:npm|npx)(?![A-Za-z0-9_.-])",
        value,
    ):
        return True
    try:
        tokens = shlex.split(value)
    except ValueError:
        tokens = value.split()
    return any(
        token.rstrip(";,|&()").rsplit("/", 1)[-1] in {"npm", "npx"}
        for token in tokens
    )


def validate_contracts(text: str, titles: dict[str, str]) -> None:
    contracts = parse_contract_blocks(text)
    nodes = set(titles)
    missing = nodes - contracts.keys()
    extra = contracts.keys() - nodes
    if missing or extra:
        raise PlanError(
            f"contract/index mismatch: missing={sorted(missing)}, extra={sorted(extra)}"
        )
    for node, (contract_title, body) in contracts.items():
        if contract_title != titles[node]:
            raise PlanError(
                f"{node} index/contract title mismatch: "
                f"{titles[node]!r} != {contract_title!r}"
            )
        for field in REQUIRED_CONTRACT_FIELDS:
            pattern = rf"^- \*\*{re.escape(field)}\*\*：\S.+$"
            matches = re.findall(pattern, body, re.MULTILINE)
            if len(matches) != 1:
                raise PlanError(
                    f"{node} must contain exactly one non-empty {field} field; found {len(matches)}"
                )
        gate_match = re.search(r"^- \*\*Gate\*\*：(\S.+)$", body, re.MULTILINE)
        if gate_match is None:
            raise PlanError(f"{node} is missing its Gate field")
        gate_text = gate_match.group(1)
        literal_gate_values = GATE_COMMAND_RE.findall(gate_text)
        if contains_direct_npm(gate_text):
            raise PlanError(f"{node} Gate bypasses maintain.mjs with npm/npx")
        commands = [
            match.group("command")
            for match in GATE_COMMAND_RE.finditer(gate_text)
            if match.group("command").startswith(GATE_PREFIXES)
        ]
        if not commands:
            raise PlanError(f"{node} Gate has no literal executable command")
        if "uv run --no-sync" in gate_text:
            raise PlanError(f"{node} Gate bypasses the isolated gate runner")
        persistent_markers = (
            "TROUPE_DIAGNOSTICS_EVIDENCE",
            "TROUPE_FINAL_ATTEMPT_ID",
            "--persistent-copy",
            "--publish-evidence",
            "accepted.json",
            ".troupe/diagnostics/evidence",
        )
        ordinary_gate = node not in {"V03", "V16"}
        if ordinary_gate and any(marker in gate_text for marker in persistent_markers):
            raise PlanError(f"{node} ordinary Gate writes persistent evidence")
        bad_evidence_literals = [
            value
            for value in literal_gate_values
            if ("/evidence/" in value or value.startswith("evidence/"))
            and "TROUPE_GATE_TMP" not in value
        ]
        if ordinary_gate and bad_evidence_literals:
            raise PlanError(
                f"{node} ordinary Gate writes evidence outside TROUPE_GATE_TMP: "
                f"{bad_evidence_literals}"
            )
        maintain_commands = [
            command
            for command in commands
            if command.startswith("node frontend/diagnostics/scripts/maintain.mjs")
        ]
        if any("--npm-cache" not in command for command in maintain_commands):
            raise PlanError(f"{node} frontend Gate omits the pinned npm cache")
        if node == "W00":
            if not maintain_commands or any(
                "--allow-registry" not in command for command in maintain_commands
            ):
                raise PlanError("W00 Gate must explicitly allow its one registry access")
        elif "--allow-registry" in gate_text:
            raise PlanError(f"{node} frontend Gate illegally allows registry access")
        artifact_match = re.search(r"^- \*\*产物\*\*：(\S.+)$", body, re.MULTILINE)
        if artifact_match is None:
            raise PlanError(f"{node} is missing its artifact field")
        artifact = artifact_match.group(1)
        shorthand = next(
            (value for value in ARTIFACT_SHORTHANDS if value in artifact), None
        )
        if shorthand is not None:
            raise PlanError(f"{node} artifact uses crate path shorthand: {shorthand}")
    parsed_artifacts = parse_contract_artifact_paths(text)
    for node, expected_exact in EXPECTED_EXACT_ARTIFACTS.items():
        exact_paths = parsed_artifacts[node]
        if exact_paths != expected_exact:
            missing = sorted(expected_exact - exact_paths)
            extra = sorted(exact_paths - expected_exact)
            raise PlanError(
                f"{node} exact artifact set drift: missing={missing}, extra={extra}"
            )


@lru_cache(maxsize=None)
def git_tracked_paths_at(commit: str) -> frozenset[str]:
    repository_root = Path(__file__).resolve().parents[2]
    result = subprocess.run(
        ["git", "ls-tree", "-r", "--name-only", commit],
        cwd=repository_root,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise PlanError(f"cannot resolve planning baseline: {result.stderr.strip()}")
    return frozenset(path for path in result.stdout.splitlines() if path)


def baseline_tracked_paths(text: str) -> frozenset[str]:
    match = re.search(r"规划基线：`main@(?P<sha>[0-9a-f]{40})`", text)
    if match is None:
        raise PlanError("missing exact main baseline SHA")
    if match.group("sha") != EXPECTED_PRODUCT_BASE_SHA:
        raise PlanError("planning baseline SHA drift")
    return git_tracked_paths_at(EXPECTED_PRODUCT_BASE_SHA)


def gate_repository_paths(gate_text: str) -> set[str]:
    paths: set[str] = set()
    for command in GATE_COMMAND_RE.findall(gate_text):
        try:
            tokens = shlex.split(command)
        except ValueError as error:
            raise PlanError(f"Gate contains malformed command quoting: {command!r}") from error
        frontend_relative = command.startswith(
            "node frontend/diagnostics/scripts/maintain.mjs"
        )
        for token in tokens:
            value = token.split("=", 1)[1] if token.startswith("--") and "=" in token else token
            for candidate in value.split(","):
                candidate = candidate.split("::", 1)[0]
                if not candidate or "$" in candidate:
                    continue
                if frontend_relative and candidate.startswith("tests/"):
                    candidate = "frontend/diagnostics/" + candidate
                if not (
                    candidate.startswith(ALLOWED_ARTIFACT_PREFIXES)
                    or candidate in ALLOWED_ARTIFACT_ROOT_FILES
                ):
                    continue
                if (
                    candidate.startswith("/")
                    or candidate.startswith("./")
                    or "\\" in candidate
                    or "//" in candidate
                    or any(character in candidate for character in "*?[]")
                    or any(segment in {"", ".", ".."} for segment in candidate.split("/"))
                ):
                    raise PlanError(f"Gate path is not canonical repository-relative: {candidate}")
                paths.add(candidate)
    return paths


def validate_gate_path_ownership(
    text: str, dependencies: dict[str, set[str]]
) -> None:
    artifacts = parse_contract_artifact_paths(text)
    owners: defaultdict[str, set[str]] = defaultdict(set)
    for owner, paths in artifacts.items():
        for path in paths:
            owners[path].add(owner)

    ancestors: dict[str, set[str]] = {}

    def visit(node: str) -> set[str]:
        if node not in ancestors:
            value = {node}
            for dependency in dependencies[node]:
                value.update(visit(dependency))
            ancestors[node] = value
        return ancestors[node]

    baseline = baseline_tracked_paths(text)
    contracts = parse_contract_blocks(text)
    for node, (_, body) in contracts.items():
        gate = re.search(r"^- \*\*Gate\*\*：(\S.+)$", body, re.MULTILINE)
        if gate is None:
            raise PlanError(f"{node} is missing its Gate field")
        for path in gate_repository_paths(gate.group(1)):
            if path in baseline or path in PLANNING_BUNDLE_PATHS:
                continue
            path_owners = owners.get(path, set())
            if not path_owners:
                raise PlanError(f"{node} Gate path has no baseline/artifact owner: {path}")
            allowed = path_owners & visit(node)
            if not allowed:
                raise PlanError(
                    f"{node} Gate path is owned only by non-ancestors: "
                    f"{path} owners={sorted(path_owners)}"
                )


def expand_finite_path(path: str) -> list[str]:
    if any(character in path for character in "*?[]"):
        raise PlanError(f"slot path contains a glob: {path}")
    opening = path.find("{")
    if opening < 0:
        if "}" in path or not path:
            raise PlanError(f"malformed slot path: {path}")
        return [path]
    closing = path.find("}", opening + 1)
    if closing < 0 or "{" in path[opening + 1 : closing]:
        raise PlanError(f"malformed finite brace path: {path}")
    members = path[opening + 1 : closing].split(",")
    if any(not member or member.strip() != member for member in members):
        raise PlanError(f"malformed finite brace members: {path}")
    prefix = path[:opening]
    suffix = path[closing + 1 :]
    expanded: list[str] = []
    for member in members:
        expanded.extend(expand_finite_path(prefix + member + suffix))
    return expanded


def validate_ownership_machine_tables(text: str, nodes: set[str]) -> tuple[int, int]:
    body = section(
        text,
        "### 4.2 Shared root 与 assembly slot 的有序 writer",
        "### 4.3 Slot behavior owner 的完整映射",
    )
    family_rows = parse_closed_table(
        body,
        (
            "Exact parameterized family",
            "Kind",
            "Closed fields",
            "Bootstrap writer",
            "Expanded writer",
            "Expansion",
        ),
    )
    families: dict[str, tuple[str, str, str, str, str]] = {}
    for raw_path, kind, raw_fields, bootstrap, raw_expanded, expansion in family_rows:
        path = strip_code(raw_path, context="parameterized family")
        if path in families:
            raise PlanError(f"duplicate fragment family: {path}")
        families[path] = (
            kind,
            strip_code(raw_fields, context=f"{path} closed fields"),
            bootstrap,
            strip_code(raw_expanded, context=f"{path} expanded writer"),
            expansion,
        )
    if families != EXPECTED_PARAMETERIZED_FAMILIES:
        raise PlanError(f"fragment family contract mismatch: families={families!r}")

    expanded_fragment_paths: set[str] = set()
    for family in families:
        for node in nodes:
            path = family.replace("<node-id>", node)
            if path in expanded_fragment_paths:
                raise PlanError(f"fragment family expansion collides: {path}")
            expanded_fragment_paths.add(path)
    if len(expanded_fragment_paths) != len(nodes) * len(families):
        raise PlanError("fragment family expansion is incomplete")

    f00_artifact = re.search(
        r"^- \*\*产物\*\*：(\S.+)$",
        parse_contract_blocks(text)["F00"][1],
        re.MULTILINE,
    )
    if f00_artifact is None:
        raise PlanError("F00 artifact field is missing")
    listed_families = {
        path
        for path in GATE_COMMAND_RE.findall(f00_artifact.group(1))
        if "<node-id>" in path
    }
    if listed_families != set(EXPECTED_PARAMETERIZED_FAMILIES):
        raise PlanError(
            "F00 artifact/fragment family mismatch: "
            f"{sorted(listed_families)}"
        )

    resource_rows = parse_closed_table(
        body, ("Node", "Exact exclusive resource")
    )
    resources: dict[str, str] = {}
    for node, raw_resource in resource_rows:
        if node in resources:
            raise PlanError(f"duplicate exclusive resource row: {node}")
        resources[node] = strip_code(
            raw_resource, context=f"{node} exclusive resource"
        )
    if resources != EXPECTED_EXCLUSIVE_RESOURCES:
        raise PlanError(f"exclusive resource contract mismatch: {resources!r}")
    if set(resources) - nodes:
        raise PlanError(f"exclusive resource has unknown node: {sorted(set(resources) - nodes)}")

    grant_rows = parse_closed_table(
        body,
        (
            "Grant",
            "Manifest",
            "Owner",
            "Member field",
            "Exact parent",
            "Filename template",
            "Cardinality",
        ),
    )
    grants: list[tuple[str, str, str, str, str, str, int]] = []
    for grant, raw_manifest, owner, raw_field, raw_parent, raw_template, raw_count in grant_rows:
        if not re.fullmatch(r"[1-9]\d*", raw_count):
            raise PlanError(f"generated grant has invalid cardinality: {raw_count!r}")
        grants.append(
            (
                grant,
                strip_code(raw_manifest, context=f"{grant} manifest"),
                owner,
                strip_code(raw_field, context=f"{grant} field"),
                strip_code(raw_parent, context=f"{grant} parent"),
                strip_code(raw_template, context=f"{grant} template"),
                int(raw_count),
            )
        )
    if grants != [EXPECTED_GENERATED_GRANT]:
        raise PlanError(f"generated grant contract mismatch: grants={grants!r}")
    grant, manifest, owner, field, parent, template, cardinality = grants[0]
    if owner not in nodes:
        raise PlanError(f"generated grant has unknown owner: {owner}")
    if template.count("<sha256>") != 1 or "/" in template:
        raise PlanError("generated grant filename template is not a basename")
    sample_members = expand_finite_path(template.replace("<sha256>", "0" * 64))
    if len(sample_members) != cardinality or len(set(sample_members)) != cardinality:
        raise PlanError("generated grant cardinality does not match its template")
    if any(".." in member or not member for member in sample_members):
        raise PlanError("generated grant contains an unsafe member")

    owner_contract = parse_contract_blocks(text)[owner][1]
    owner_artifact = re.search(
        r"^- \*\*产物\*\*：(\S.+)$", owner_contract, re.MULTILINE
    )
    if owner_artifact is None:
        raise PlanError(f"{owner} artifact field is missing")
    owner_artifact_text = owner_artifact.group(1)
    for required in (grant, field):
        if required not in owner_artifact_text:
            raise PlanError(
                f"{owner} artifact omits generated grant fact: {required}"
            )
    owner_static_paths: set[str] = set()
    for raw_path in GATE_COMMAND_RE.findall(owner_artifact_text):
        if raw_path.startswith(("rust/", "frontend/", "tests/", "scripts/")):
            owner_static_paths.update(expand_finite_path(raw_path))
    if manifest not in owner_static_paths:
        raise PlanError(f"{owner} artifact omits generated manifest: {manifest}")
    return len(families), len(grants)


def contract_artifact_paths(text: str) -> dict[str, set[str]]:
    owners: defaultdict[str, set[str]] = defaultdict(set)
    for node, paths in parse_contract_artifact_paths(text).items():
        for path in paths:
            owners[path].add(node)
    return dict(owners)


def validate_slot_inventory(
    text: str, nodes: set[str], outgoing: dict[str, set[str]]
) -> tuple[int, int, int]:
    slot_body = section(text, "### 4.1 Compile-safe slot 的有限清单", "### 4.2 Shared root")
    inventory: dict[str, str] = {}
    for creator, raw_paths in parse_closed_table(
        slot_body, ("Creator", "Exact slot paths")
    ):
        if creator not in {"F04", "F05", "F06"}:
            raise PlanError(f"slot table has invalid creator: {creator}")
        slot_paths = strip_code(raw_paths, context=f"{creator} slot paths")
        for path in expand_finite_path(slot_paths):
            if not path.endswith(".rs"):
                raise PlanError(f"compile-safe slot is not an .rs file: {path}")
            if path in inventory:
                raise PlanError(f"duplicate compile-safe slot: {path}")
            inventory[path] = creator
    if not inventory or set(inventory.values()) != {"F04", "F05", "F06"}:
        raise PlanError("slot inventory must contain F04, F05, and F06 paths")

    shared_body = section(
        text,
        "### 4.2 Shared root 与 assembly slot 的有序 writer",
        "### 4.3 Slot behavior owner 的完整映射",
    )
    shared: dict[str, tuple[str, ...]] = {}
    for raw_paths, raw_writers, _constraint in parse_closed_table(
        shared_body, ("Exact path", "Ordered writers", "约束")
    ):
        writers = tuple(raw_writers.split(" -> "))
        if not writers:
            raise PlanError(f"shared writer row has no writer: {raw_paths}")
        if any(NODE_RE.fullmatch(writer) is None for writer in writers):
            raise PlanError(f"shared writer row is malformed: {raw_writers!r}")
        if len(set(writers)) != len(writers):
            raise PlanError(f"shared writer row repeats a writer: {raw_paths}")
        unknown = set(writers) - nodes
        if unknown:
            raise PlanError(
                f"shared writer row has unknown writers: {raw_paths} {sorted(unknown)}"
            )
        for left, right in zip(writers, writers[1:]):
            if not is_reachable(left, right, outgoing):
                raise PlanError(
                    f"shared writer order is not reachable: {raw_paths} {left}->{right}"
                )
        paths = strip_code(raw_paths, context="shared writer path")
        for path in expand_finite_path(paths):
            if path in shared:
                raise PlanError(f"duplicate shared writer path: {path}")
            if path in inventory and inventory[path] != writers[0]:
                raise PlanError(
                    f"slot/shared creator mismatch: {path} {inventory[path]} != {writers[0]}"
                )
            shared[path] = writers
    if not shared:
        raise PlanError("shared writer inventory is empty")

    explicit_artifacts = contract_artifact_paths(text)
    for path, owners in explicit_artifacts.items():
        if len(owners) < 2:
            continue
        if path not in shared:
            raise PlanError(
                f"multi-writer contract artifact is missing from shared inventory: "
                f"{path} {sorted(owners)}"
            )
        if path not in inventory and set(shared[path]) != owners:
            raise PlanError(
                f"contract/shared writer mismatch: {path} "
                f"{sorted(owners)} != {list(shared[path])}"
            )

    owner_body = section(
        text,
        "### 4.3 Slot behavior owner 的完整映射",
        "## 5. 节点执行合同",
    )
    behavior_owners: dict[str, str] = {}
    for owner, raw_paths in parse_closed_table(
        owner_body, ("Owner", "Exact slot paths")
    ):
        if owner not in nodes:
            raise PlanError(f"slot behavior row has unknown owner: {owner}")
        paths = strip_code(raw_paths, context=f"{owner} behavior slot paths")
        for path in expand_finite_path(paths):
            if path not in inventory:
                raise PlanError(f"slot behavior row has unknown path: {path}")
            if path in behavior_owners:
                raise PlanError(f"duplicate slot behavior owner: {path}")
            creator = inventory[path]
            if creator != owner and not is_reachable(creator, owner, outgoing):
                raise PlanError(
                    f"slot behavior owner is not reachable from creator: {path} {creator}->{owner}"
                )
            if path in shared and owner not in shared[path]:
                raise PlanError(
                    f"slot behavior owner is absent from shared writers: {path} "
                    f"{owner} not in {shared[path]}"
                )
            if path in shared:
                expected_owner = shared[path][1] if len(shared[path]) > 1 else creator
                if owner != expected_owner:
                    raise PlanError(
                        f"slot primary behavior owner mismatch: {path} "
                        f"{owner} != {expected_owner}"
                    )
            behavior_owners[path] = owner
    required_behavior_paths = {
        path for path in inventory if not path.endswith("/mod.rs")
    }
    required_behavior_paths.update(
        path
        for path, writers in shared.items()
        if path in inventory and writers[-1] != inventory[path]
    )
    if behavior_owners.keys() != required_behavior_paths:
        missing = sorted(required_behavior_paths - behavior_owners.keys())
        extra = sorted(behavior_owners.keys() - required_behavior_paths)
        raise PlanError(
            f"slot behavior coverage mismatch: missing={missing}, extra={extra}"
        )
    for path, creator in inventory.items():
        behavior_owner = behavior_owners.get(path, creator)
        contract_writers = explicit_artifacts.get(path, set())
        allowed_writers = set(shared.get(path, (creator, behavior_owner)))
        unexpected = contract_writers - allowed_writers
        if unexpected:
            raise PlanError(
                f"slot contract has an unexpected writer: {path} {sorted(unexpected)}"
            )
        if behavior_owner != creator and behavior_owner not in contract_writers:
            raise PlanError(
                "slot behavior owner is missing from its contract artifact: "
                f"{path} owner={behavior_owner}"
            )
    for path, writers in shared.items():
        derived_writers = set(explicit_artifacts.get(path, set()))
        if path in inventory:
            derived_writers.add(inventory[path])
            if path in behavior_owners:
                derived_writers.add(behavior_owners[path])
        if set(writers) != derived_writers:
            raise PlanError(
                "shared writer row is not bidirectionally grounded: "
                f"{path} table={list(writers)} derived={sorted(derived_writers)}"
            )
    return len(inventory), len(shared), len(behavior_owners)


def derived_schedule(
    dependencies: dict[str, set[str]],
    subprojects: dict[str, str],
    outgoing: dict[str, set[str]],
) -> tuple[list[tuple[str, ...]], list[str]]:
    order, _ = topological_order(dependencies, expected_edges(dependencies))
    remaining: dict[str, int] = {}
    next_node: dict[str, str | None] = {}
    for node in reversed(order):
        children = outgoing[node]
        if not children:
            remaining[node] = 1
            next_node[node] = None
            continue
        child = min(children, key=lambda item: (-remaining[item], item))
        remaining[node] = 1 + remaining[child]
        next_node[node] = child

    merged: set[str] = set()
    ticks: list[tuple[str, ...]] = []
    while len(merged) < len(dependencies):
        candidates = [
            node
            for node, direct in dependencies.items()
            if node not in merged and direct <= merged
        ]
        candidates.sort(key=lambda item: (-remaining[item], item))
        selected: list[str] = []
        selected_subprojects: set[str] = set()
        for node in candidates:
            if node in EXPECTED_EXCLUSIVE_RESOURCES:
                if selected:
                    continue
                selected.append(node)
                selected_subprojects.add(subprojects[node])
                break
            if selected and selected[0] in EXPECTED_EXCLUSIVE_RESOURCES:
                break
            if subprojects[node] in selected_subprojects:
                continue
            selected.append(node)
            selected_subprojects.add(subprojects[node])
            if len(selected) == 3:
                break
        if not selected:
            raise PlanError("scheduler made no progress")
        ticks.append(tuple(selected))
        merged.update(selected)

    path = ["F00"]
    while next_node[path[-1]] is not None:
        path.append(next_node[path[-1]])  # type: ignore[arg-type]
    return ticks, path


def validate_derived_schedule(
    text: str,
    dependencies: dict[str, set[str]],
    subprojects: dict[str, str],
    outgoing: dict[str, set[str]],
    edge_count: int,
) -> None:
    body = section(text, "### 6.1 直接依赖与 critical path", "### 6.4 Merge ownership")
    stats = re.search(
        r"- (?P<nodes>\d+)个节点、(?P<edges>\d+)条直接边、root=`F00`、唯一sink=`V03`",
        body,
    )
    if stats is None or (int(stats.group("nodes")), int(stats.group("edges"))) != (
        len(dependencies),
        edge_count,
    ):
        raise PlanError("derived node/edge statistics are stale")
    ticks, critical_path = derived_schedule(dependencies, subprojects, outgoing)
    path_match = re.search(
        r"- longest remaining path长度为(?P<length>\d+)：\n  `(?P<path>[^`]+)`",
        body,
    )
    expected_path = " -> ".join(critical_path)
    if (
        path_match is None
        or int(path_match.group("length")) != len(critical_path)
        or path_match.group("path") != expected_path
    ):
        raise PlanError("derived critical path is stale")
    raw_schedule_rows = parse_closed_table(
        body,
        ("Tick", "Ready nodes dispatched"),
        separators=("---:", "---"),
    )
    schedule_rows: list[tuple[int, tuple[str, ...]]] = []
    for raw_tick, raw_nodes in raw_schedule_rows:
        if re.fullmatch(r"\d{2}", raw_tick) is None:
            raise PlanError(f"malformed schedule tick: {raw_tick!r}")
        scheduled_nodes = tuple(raw_nodes.split(", "))
        if any(NODE_RE.fullmatch(node) is None for node in scheduled_nodes):
            raise PlanError(f"malformed schedule node row: {raw_nodes!r}")
        if len(scheduled_nodes) > 3:
            raise PlanError(f"schedule exceeds three slots: {raw_nodes!r}")
        if any(node in EXPECTED_EXCLUSIVE_RESOURCES for node in scheduled_nodes) and len(
            scheduled_nodes
        ) != 1:
            raise PlanError(f"exclusive resource node overlaps a schedule tick: {raw_nodes!r}")
        schedule_rows.append((int(raw_tick), scheduled_nodes))
    expected_rows = list(enumerate(ticks, start=1))
    if schedule_rows != expected_rows:
        raise PlanError("derived three-slot schedule is stale")
    utilization = 100.0 * len(dependencies) / (3 * len(ticks))
    schedule_summary = re.search(
        r"slot下为(?P<ticks>\d+)个时隙，静态slot利用率(?P<util>\d+\.\d)%",
        body,
    )
    if (
        schedule_summary is None
        or int(schedule_summary.group("ticks")) != len(ticks)
        or float(schedule_summary.group("util")) != round(utilization, 1)
    ):
        raise PlanError("derived schedule summary is stale")
    if "参考排程不构成最优性或验收要求" not in body:
        raise PlanError("schedule acceptance scope is stale")


def validate_fragment_contract(text: str, nodes: set[str]) -> None:
    architecture = section(text, "### 1.4 Artifact contract 的并行化", "## 2. Worktree")
    contracts = parse_contract_blocks(text)
    required_architecture = (
        "tests/fixtures/artifact_layout/nodes/<node-id>.json",
        "tests/fixtures/diagnostic_node_gates/<node-id>.json",
        "index.json`显式枚举第3.1节的每个node ID",
        "唯一ownership fragment",
        "structured gate descriptor",
        "绝不参与artifact path union",
        "ownership-ledger.json",
        "state=planned",
        "state=realized",
        "--plan-only",
        "projected writer集合，并与ledger双向相等",
        f"全部{len(nodes)}个artifact fragment与{len(nodes)}个gate descriptor realized",
        "introduced ∪ modified ∪ removed ∪ generated",
        "完整path集合",
        "create|seam|implement|assemble|generate|remove",
        "manifest grant",
        "files[].path",
    )
    missing = [
        phrase for phrase in required_architecture if phrase not in architecture
    ]
    if missing:
        raise PlanError(f"artifact/gate fragment contract drift: missing={missing}")
    required_f00 = (
        "artifact family的F00 fragment为`state=realized`",
        "其余artifact fragment为`state=planned`",
        "gate family的F00 descriptor为`state=realized`",
        "其余descriptor为`state=planned`",
        "恰好一个artifact fragment family和一个gate descriptor family",
        "schema字段混用",
        "非法state",
        "两类planned非空",
        "两类realized未闭合",
        "resource全空",
    )
    missing_f00 = [phrase for phrase in required_f00 if phrase not in contracts["F00"][1]]
    if missing_f00:
        raise PlanError(f"F00 fragment lifecycle drift: missing={missing_f00}")
    required_f02 = (
        "--plan-only",
        "projected writer集合",
        "与ledger/grant双向相等",
        "F00/F01/W00/F02的两类lifecycle file必须realized",
        "其余两类file分别保持planned/empty",
        "introduced ∪ modified ∪ removed ∪ generated",
        "第三个parameterized family",
        "artifact/gate schema混用",
        "每个slot的creator/behavior owner",
        "gate descriptor concrete path绝不进入artifact union",
        "Gate literal与realized descriptor argv中的每个repository path",
        "ownerless、sibling或future-only path在dispatch前失败",
        "所有4.2机器表逐row/逐column closed解析",
        "五文件accepted planning bundle",
    )
    missing_f02 = [phrase for phrase in required_f02 if phrase not in contracts["F02"][1]]
    if missing_f02:
        raise PlanError(f"F02 fragment lifecycle drift: missing={missing_f02}")
    if len(nodes) < 1:
        raise PlanError("artifact/gate fragment contract has no nodes")


def validate_sink_and_publication_contracts(text: str) -> None:
    contracts = parse_contract_blocks(text)
    required_facts = {
        "F01": (
            "rust/crates/troupe-agent-runtime/Cargo.toml",
            "troupe-agent-runtime -> diagnostics-core",
            "unstable_end_turn_token_usage",
        ),
        "F06": (
            "rust/crates/troupe-agent-runtime/src/result/mod.rs",
            "真实state transition线性化后",
            "cumulative rejection count",
            "prompt submission前把`TurnDiagnosticContext`恰好一次安装到existing `AgentTurnControl`",
            "已有session observer时必须复用且不能被per-turn destination覆盖",
            "A09 sink-only input/output policy",
            "不编辑`diagnostics/mod.rs`、session roots或`result/mod.rs`",
        ),
        "F05": (
            "RunBinding seam可携带一个type-erased optional diagnostic admission capability",
            "X00安装Production mandatory durable capability",
            "B18仅在合法sink-only API path且不存在Production capability时安装volatile capability",
            "prompt submission前通过同一seam把existing `AgentTurnControl`和bind-time frozen internal capture config交给capability",
        ),
        "C02": (
            "Production profile只允许mandatory durable reserver",
            "B18的sink-only profile只允许bounded in-memory reserver",
            "复用同一identity/sequence/validation/fan-out algorithm",
            "profile不能在Run中切换或让Production降级到volatile",
        ),
        "C01": (
            "diagnostic.component_failed",
            "enqueue|callback stage",
            "三个stable error code",
            "禁止raw exception/payload",
        ),
        "C03": (
            "diagnostic-component-failed.json",
            "sink enqueue/callback `diagnostic.component_failed` typed detail",
        ),
        "C05": (
            "Run-origin",
            "左闭右开",
            "`max_points=1024`",
            "`width=max(1,ceil(duration/1023))`",
            "partial/empty bucket",
            "整体stale binding",
        ),
        "P00": (
            "FrozenJsonValue",
            "FrozenJsonArray",
            "FrozenJsonObject",
            "DiagnosticToolLocation",
            "DiagnosticToolInput",
            "DiagnosticToolOutput",
            "captured_input/captured_output",
        ),
        "P01": (
            "sink-capture-matrix.json",
            "八个strict-bool fields",
            "不存在隐式context/thinking flag",
            "usage=False",
            "agent.turn.active",
            "diagnostic.dropped_events",
            "mailbox/Cue/Run级counter明确排除",
            "所有sink-targeted `diagnostic.component_failed`明确排除",
            "`result_validation`同时控制五种transition metadata和`result.validation_rejections` counter",
            "不含submitted/invalid/validated result value",
        ),
        "P04": ("D38 public Frozen JSON/tool payload types", "八字段`DiagnosticCapture`"),
        "B06": (
            "rust/src/orchestration/actor.rs",
            "真实PyO3 method signature",
            "context/schema/prompt处理",
        ),
        "B15": (
            "逐项执行P01 checked-in D34 matrix",
            "usage=False",
            "captured_input/captured_output",
            "agent.turn.active",
            "diagnostic.dropped_events",
            "排除mailbox/Cue/Run counter",
            "所有sink-targeted `diagnostic.component_failed`",
            "result_validation同时选择五种transition metadata和`result.validation_rejections` counter",
            "永不附result value",
            "纯capture/projection",
            "不admit/bind/subscribe/enqueue/call callback",
        ),
        "A00": (
            "typed `TurnDiagnosticContext`",
            "effective observer destination与只供A09消费的input/output capture policy分开",
            "Production sidecar不能替换Run observer",
        ),
        "A09": (
            "`TurnDiagnosticContext`的bind-frozen input/output sidecar policy",
            "Production复用Run observer时policy仍生效",
            "payload只随该Act的internal candidate流向B15/B18",
            "关闭或无context时raw字段在source boundary立即释放",
        ),
        "B18": (
            "成功Act admission与one-shot sink `UNBOUND -> BOUND`是同一不可分割transition",
            "在第二次prompt submission前",
            "调用B15 pure projection后按sequence送K00/K02 enqueue",
            "subscriber enqueue failure不回滚Act或mandatory hub",
            "B18是唯一delivery-fact bridge",
            "K00普通DropDelta",
            "不发component failure",
            "K01首次CallbackFailure或unexpected enqueue channel failure",
            "恰好一次admit C01 typed `diagnostic.component_failed`",
            "该instant不进入任何per-Act sink",
            "不重新解释capture matrix或payload",
            "存在X00 Production capability时必须复用mandatory durable hub",
            "binding-owned C02 sink-only bounded in-memory hub",
            "安装bind-frozen `TurnDiagnosticContext`",
            "注册A09 input/output sidecar policy",
            "Production destination不可覆盖",
            "不越权要求尚非ancestor的B12/B17真实canonical terminal链路",
            "不启动server/registry/SQLite、不访问`.troupe`",
            "sink为None时不创建context/fallback",
            "fallback不能用作Production diagnostics disable/degrade路径",
        ),
        "B02": (
            "active状态只由open/finished `scene.lifecycle`推导",
            "不生成closed taxonomy之外的`scene.active` counter",
        ),
        "B00": (
            "consume CLI-prevalidated production root + allocate run ID",
            "不把尚未建立hub前的CLI path parsing伪装成canonical `production.path_resolution` span",
        ),
        "B09": (
            "包裹真正的Production package/class resolution、import/load和construct",
            "初始CLI root语法/存在性验证不属于该span",
            "不能事后伪造elapsed",
        ),
        "B12": (
            "session opening/lifecycle/closing/closed",
            "ready/broken",
            "exactly-once canonical session span",
            "cumulative `result.validation_rejections` counter各恰好admit一次",
            "每个A00-A03/A05-A08输入candidate恰好一次admission或stable rejection",
        ),
        "A08": (
            "真实result MCP state machine",
            "cumulative `result.validation_rejections` counter candidate",
            "值严格为1..N",
            "真实Result MCP service/state machine及F06 seam",
            "不能只直接构造normalizer输入",
        ),
        "B05": (
            "`agent.turn.active=1`",
            "`agent.turn.active=0`",
            "未started turn不伪造sample",
            "normal/cancel/handoff/failure均断言顺序与cardinality",
        ),
        "K00": (
            "typed cumulative DropDelta",
            "不自行写canonical gap/counter",
        ),
        "K01": (
            "跨sink只在callback yield时可交错",
            "阻塞型sync callback会占用同一个diagnostic loop",
            "不阻塞mandatory hub或Production",
            "typed CallbackFailure outcome",
            "本节点不admit canonical event",
            "由B18消费首次failure",
        ),
        "K02": (
            "summary的delivered/dropped/failure/close reason/`complete`只来自K00/K01事实",
            "B18消费K00/K01 typed outcome",
        ),
        "S00": ("owner-only `0700`", "`umask 000`", "chmod/fstat"),
        "S01": ("owner-only `0600`", "父目录保持`0700`", "chmod/fchmod/fstat"),
        "R01": ("instances目录精确`0700`", "temp精确`0600`", "`umask 000`"),
        "S05": (
            "Runtime active持有process-owned exclusive lease",
            "borrowed guard capability",
            "不得重新取得shared lease或释放active guard",
            "inactive local/archive status/snapshot/events/dump/serve reader使用shared lease",
        ),
        "Q00": (
            "active reader必须接收S05 borrowed active guard",
            "绝不加锁/释放guard",
            "archive reader先持有并最终释放request-owned shared lease",
            "active SQLite corruption、identity或dense-prefix invariant failure发typed core-fatal signal",
            "archive schema/corruption/identity/lease failure只返回该archive operation error",
        ),
        "Q01": (
            "Run-origin/left-closed-right-open/1024-point width规则",
            "counter bucket latest",
            "partial/empty/coverage byte-exact且不截断",
            "watermark/viewport/width变化使旧result整体stale",
            "engine worker/system context loss产生typed core-fatal signal",
        ),
        "H00": (
            "Forwarded",
            "X-Forwarded-Host",
            "X-Forwarded-Proto",
            "X-Forwarded-Prefix",
            "byte-equal",
        ),
        "T00": (
            "protos/perfetto/common/builtin_clock.proto",
            "protos/perfetto/trace/track_event/debug_annotation.proto",
            "逐文件hash与closed used-definition manifest闭合",
            "递归包含被选字段实际引用的message/enum definition",
            "未选中的upstream import/oneof arm不属于closure",
            "audit不调用protoc或尝试编译完整raw schema",
            "缺失未选import target不应失败",
            "`BUILTIN_CLOCK_TRACE_FILE=11`",
        ),
        "T03": (
            "CapturedEventSource -> AsyncWrite/packet stream",
            "最多1,000,000个structural entries和64 MiB owned structural payload",
            "StructuralIndexLimitExceeded { dimension, limit, required }",
            "在首次poll writer以前",
            "sequence/span records、unique tracks、spans、lane assignments、causal flows及start/end attachments、Act usage、dense identities和descriptor order",
            "不spill filesystem/temp index",
            "第二遍才重新读取同一captured prefix",
            "可留下partial stream",
        ),
        "T08": (
            "接收T03 stream producer",
            "exclusive temp",
            "backup hard link+directory-fsync",
            "结果closed为published/not_published/publication_indeterminate",
            "identity-check+rollback rename/unlink+directory-fsync",
            "不谎称旧目标不变",
            "wrapper不读取event、选择W或解释trace metadata",
        ),
        "H05": (
            "GET /api/v1/dump",
            "active profile复用S05 Runtime-held borrowed guard",
            "零lock acquire/release",
            "archive profile才由server端取得并释放request-owned shared lease",
            "绝不释放active guard",
            "remote request不能提供server output path",
            "T03 bounded encoder",
            "第一遍structural preflight全部成功后才commit successful response",
        ),
        "D06": (
            "URL/active target只调用H05 endpoint",
            "在调用方本机发布",
            "成功必须是`published`",
            "必须`not_published`",
            "报告`publication_indeterminate`",
            "不能声称旧target未变",
            "不让remote选择server filesystem path",
        ),
        "H03": (
            "GET /api/v1/views",
            "最多64项、保持manifest order且允许empty",
            "archive incompatible entry只含status",
            "任何其他无view_id参数fail closed",
            "exact frozen range/width/aligned empty+partial buckets",
            "active profile的Q00 corruption或query execution context系统性退出转发core-fatal",
            "archive profile同类store/query open failure只终止对应request/serve command",
        ),
        "B08": (
            "阻止constructor/start",
            "`outcome=failed,clean_shutdown=true`",
            "durable registry unpublish",
            "active lease release",
            "diagnostic finalization本身失败才保持incomplete/`clean_shutdown=false`",
        ),
        "B17": (
            "pre-submission Act terminal",
            "prompt已提交但无authoritative settlement",
            "session terminal",
            "authoritative settlement消费至多一个A04 candidate",
            "三类trigger race",
            "usage-before-finish",
        ),
        "B16": (
            "B17完成唯一canonical usage admission",
            "B05完成`act.lifecycle` SpanFinished admission",
            "B18对两项应用B15 projection",
            "B14使act/generation authority过期",
            "settlement hook确认authority expiry后",
            "不得留下半过期authority或半seal queue",
            "B12/B17/B18汇合后的standalone full-chain owner",
            "已有session不重建、message/plan/tool/result/context/terminal usage canonical events",
            "standalone opt-in tool payload只到sink且没有store/file side effect",
        ),
        "O00": ("`README.md`",),
        "O04": ("docs/diagnostics/index.md", "不拥有或调用final runner/publisher"),
        "O03": (
            "`published/not_published/publication_indeterminate`三态",
            "identity-checked durable rollback",
            "indeterminate时保留现场并人工检查",
            "active borrowed guard/archive shared lease",
        ),
        "X01": (
            "active Q00 reader的SQLite corruption/identity/dense-prefix invariant failure",
            "active Q01/H03 query worker/execution-context系统性退出",
            "archive reader/query/store failure",
        ),
        "V02": (
            "OS-assigned port 0",
            "不占用fixed release port",
            "`agent.turn.active`的1/0 pair",
            "`result.validation_rejections`的1..N samples",
            "真实Production session observer + per-turn sidecar进入opt-in sink",
            "canonical store/events/Web/Perfetto detail保持`captured_input/captured_output=None`",
            "关闭capture时source payload立即释放且sink也为None",
            "Run-origin/left-closed-right-open/1024-point width",
            "viewport/range导致的derived-width变化",
            "不同binding的bucket不混合",
            "active Perfetto dump零lease reacquire",
            "archive queries/serve/dump各自shared lease acquire/release",
        ),
        "W10": (
            "在owned `client.ts`复用W01 decoder",
            "每个Run/Views surface恰fetch一次",
            "incompatible entry零query",
            "整个surface进入local error且不保留partial catalog",
            "query generation key冻结run/view/selection/scope/W/viewport range",
            "pan/zoom或width变化时abort-or-ignore旧inflight response",
            "不得merge/rebucket不同width",
        ),
        "H01": (
            "`after=A&through=W`返回exact dense `(A,W]`",
            "`{api_schema_version,run_id,captured_watermark,events,next_after:null}`",
            "不新增limit/API版本",
            "through alone、after+tail、tail+through拒绝",
        ),
        "W08": (
            "提供W05唯一调用的atomic hydrate",
            "snapshot是materialized facts authority",
            "不经ordinary `event_received`重放",
            "设置tool/result refresh flags和`dropped_through=A`",
        ),
        "W05": (
            "`A=max(0,W-4096)`",
            "`GET /api/v1/events?after=A&through=W`",
            "exact dense `(A,W]`",
            "调用W08 atomic hydrate",
            "native EventSource after W",
            "不得发送limit或引入新API版本",
        ),
        "W15": (
            "catalog严格保持manifest order",
            "incompatible entry固定走W20",
            "empty catalog显示empty surface且不发query",
        ),
        "B13": (
            "必须先检查manifest声明的schema version",
            "newer version保持opaque",
            "绝不按current JSON schema解析",
            "只有current-version record才执行decode/identity/canonical验证",
        ),
        "B14": (
            "`RunBinding + act_id/generation + role/state` authority",
            "caller、caller descendant、supervisor",
            "只有registered child传播",
            "绝不fallback到Cue scope、sink registry reverse lookup",
            "staged/transactional commit",
            "B16 settlement hook提交authority expiry",
        ),
        "V00": (
            "slow response制造pan/zoom viewport/derived-width race",
            "browser不rebucket",
        ),
        "V06": (
            "OS-assigned port 0",
            "不占用fixed release port",
            "与V02 node Gate并发都不共享listener namespace",
            "invalid/duplicate/incompatible ViewSpec",
            "`outcome=failed,clean_shutdown=true`",
            "active Q00 SQLite corruption/identity/dense-prefix invariant",
            "active Q01/H03 worker/execution-context loss均core-fatal",
            "archive reader/query/store fault",
            "pre-submission Act terminal",
            "submitted-without-settlement session terminal",
            "usage-before-Act-finish",
            "store、HTTP/Web events与CLI恰好一次可见",
            "普通DropDelta只形成累计`diagnostic.dropped_events`和summary、零component failure",
            "counter投递失败不递归",
        ),
        "V12": (
            "V02/V06各自只用OS-assigned isolated ports",
            "串行只用于确定性聚合而不是声明node Gate间有共享端口资源",
        ),
        "V05": (
            'exclusive_resources=["benchmark-host"]',
            "禁止并发本机Gate",
            "environment fingerprint",
            "actor-design/diagnostics-design/plan/validator/integration/browser/cache/result SHA",
        ),
        "V07": (
            "actor-design/diagnostics-design/plan/validator/integration/artifact/result SHA",
            "包体成为hard gate",
        ),
        "V16": (
            "publish_diagnostics_acceptance.py",
            "test_diagnostics_acceptance_publisher.py",
            "diagnostics-final-evidence-schema.json",
            "diagnostics-accepted-evidence-schema.json",
            "O_EXCL",
            "no-overwrite hard-link publish",
            "staging-name unlink",
            "rollback fsync",
            "preexisting regular/symlink/special path",
            "publication_indeterminate",
            "actor-design/diagnostics-design/plan/validator/commit/cache/result SHA",
        ),
        "V03": (
            "V16-owned `scripts/publish_diagnostics_acceptance.py`",
            "任一前置失败不得调用publisher",
            "恰好一次",
            "O04 docs-index test",
            "actor-design/diagnostics-design/plan/validator/commit/cache/child-result/report hashes",
        ),
    }
    for node, facts in required_facts.items():
        body = contracts[node][1]
        missing = [fact for fact in facts if fact not in body]
        if missing:
            raise PlanError(f"{node} executable contract drift: missing={missing}")
    artifact_owners = contract_artifact_paths(text)
    required_owned_artifacts = {
        "rust/src/orchestration/actor.rs": {"F05", "B06"},
        "rust/crates/troupe-diagnostics-perfetto/src/collect.rs": {"T01", "T03"},
        "rust/crates/troupe-diagnostics-perfetto/src/tracks.rs": {"T01", "T03"},
        "rust/src/diagnostic_runtime/sink_binding.rs": {"B18", "B16", "B14"},
        "rust/src/diagnostic_sink/mod.rs": {"B16"},
        "rust/src/application/cli.rs": {"D07", "X00", "X01"},
        "rust/src/diagnostic_runtime/activation.rs": {"X00", "X01"},
        "rust/src/diagnostic_runtime/bootstrap.rs": {"B00", "X01"},
        "rust/src/orchestration/python_task.rs": {"F05", "B14", "X01"},
        "rust/src/orchestration/runtime.rs": {"F05", "X01"},
        "rust/src/orchestration/scene_context.rs": {"F05", "B14"},
        "tests/integration/test_actor_act_diagnostic_sink_binding.py": {"B18", "B14"},
        "README.md": {"O00"},
        "docs/diagnostics/index.md": {"O04"},
        "scripts/test_diagnostics_final.sh": {"V03"},
        "tests/unit/test_diagnostics_final_runner.py": {"V03"},
        "scripts/publish_diagnostics_acceptance.py": {"V16"},
        "tests/unit/test_diagnostics_acceptance_publisher.py": {"V16"},
        "tests/fixtures/release/diagnostics-final-evidence-schema.json": {"V16"},
        "tests/fixtures/release/diagnostics-accepted-evidence-schema.json": {"V16"},
    }
    for path, expected_owners in required_owned_artifacts.items():
        actual_owners = artifact_owners.get(path, set())
        if actual_owners != expected_owners:
            raise PlanError(
                f"required artifact ownership drift: {path} "
                f"expected={sorted(expected_owners)} actual={sorted(actual_owners)}"
            )


def validate_planning_bundle(text: str) -> None:
    protocol = section(text, "### 2.1 Integration branch 与基线冻结", "### 2.2 一个节点")
    freeze = section(text, "### 10.1 Freeze protocol", "### 10.2 Required")
    required_protocol = (
        "git add -f",
        "accepted-production-diagnostics-plan",
        "PLAN_BUNDLE_SHA",
        "docs/design/actor-agent-session.md",
        "docs/design/production-diagnostics.md",
        "docs/plan/production-diagnostics-implementation-plan.md",
        "docs/plan/verify_production_diagnostics_plan.py",
        "docs/plan/production-diagnostics-plan-review-record.md",
        "任何node修改这五个",
    )
    missing_protocol = [value for value in required_protocol if value not in protocol]
    if missing_protocol:
        raise PlanError(
            f"accepted planning bundle protocol drift: missing={missing_protocol}"
        )
    required_freeze = (
        "Actor Design SHA-256",
        "Diagnostics Design SHA-256",
        "Validator SHA-256",
        "Plan SHA-256",
    )
    missing_freeze = [value for value in required_freeze if value not in freeze]
    if missing_freeze:
        raise PlanError(f"planning freeze identity drift: missing={missing_freeze}")


def validate_cumulative_gate_contracts(text: str) -> None:
    cumulative = section(
        text,
        "### 8.1 每次 merge 的 mandatory gate",
        "### 8.2 Join-point gates",
    )
    required = (
        "从repository root执行",
        "cargo fmt --manifest-path rust/Cargo.toml --all -- --check",
        "cargo check --locked --manifest-path rust/Cargo.toml --workspace --all-targets --all-features",
    )
    missing = [fact for fact in required if fact not in cumulative]
    forbidden = [
        command
        for command in (
            "`cargo fmt --check --all`",
            "`cargo check --locked --workspace --all-targets --all-features`",
        )
        if command in cumulative
    ]
    if missing or forbidden:
        raise PlanError(
            f"cumulative Rust Gate drift: missing={missing} forbidden={forbidden}"
        )


def validate_cache_and_evidence_protocol(text: str) -> None:
    worktree_protocol = section(text, "### 2.1 Integration branch", "### 2.3 并行规则")
    required_worktree_facts = (
        "每次第8.3节真实final attempt前",
        "node/worktree/merge Gate不得写这里",
        "TROUPE_FINAL_ATTEMPT_ID",
        "attempts/<TROUPE_FINAL_ATTEMPT_ID>/",
        "V07-wheel-report.json",
        "V05-performance-raw.json",
        "V03-final-evidence.json",
        "任一预存同名文件都fail closed",
        "失败attempt原样保留且重试必须使用新ID",
        "accepted.json",
        "scripts/publish_diagnostics_acceptance.py",
        "O_EXCL",
        "no-overwrite hard-link publish",
        "staging-name unlink",
        "directory fsync",
        "rollback-fsync",
        "绝不覆盖旧acceptance",
        "publication_indeterminate",
        "root没有手工JSON/rename步骤",
        "TROUPE_GATE_TMP=$(mktemp -d)",
        "不同attempt绝不复用path",
        "--npm-cache \"${TROUPE_NPM_CACHE:?}\"",
        "--provision-package-cache --allow-registry",
        "npm ci --offline --ignore-scripts",
        "唯一允许访问npm registry的node",
    )
    missing = [fact for fact in required_worktree_facts if fact not in worktree_protocol]
    if missing:
        raise PlanError(f"cache/evidence protocol drift: missing={missing}")

    contracts = parse_contract_blocks(text)
    required_contract_facts = {
        "W00": ("--allow-registry", "--verify-offline-cache-replay"),
        "V00": ("--npm-cache \"${TROUPE_NPM_CACHE:?}\"",),
        "V04": ("--npm-cache \"${TROUPE_NPM_CACHE:?}\"",),
        "V13": ("--npm-cache \"${TROUPE_NPM_CACHE:?}\"",),
        "V05": (
            "${TROUPE_GATE_TMP:?}/V05-performance-raw.json",
            "performance-baseline.raw.json",
            "--npm-cache \"${TROUPE_NPM_CACHE:?}\"",
            "actor-design/diagnostics-design/plan/validator/integration/browser/cache/result SHA",
        ),
        "V07": (
            "${TROUPE_GATE_TMP:?}/V07-wheel-report.json",
            "actor-design/diagnostics-design/plan/validator/integration/artifact/result SHA",
        ),
        "V09": ("--npm-cache \"${TROUPE_NPM_CACHE:?}\"",),
        "V16": (
            "accepted.json",
            "publish_diagnostics_acceptance.py",
            "no-overwrite hard-link publish",
            "staging-name unlink",
            "rollback-fsync",
            "publication_indeterminate",
            "actor-design/diagnostics-design/plan/validator/commit/cache/result SHA",
        ),
        "V03": (
            "--evidence-root \"${TROUPE_GATE_TMP:?}/evidence/attempts/00000000-0000-4000-8000-000000000003\"",
            "--all-realized",
            "accepted.json",
            "publish_diagnostics_acceptance.py",
            "--acceptance-path",
            "--integration-sha",
            "actor-design/diagnostics-design/plan/validator/commit/cache/child-result/report hashes",
        ),
    }
    for node, facts in required_contract_facts.items():
        body = contracts[node][1]
        absent = [fact for fact in facts if fact not in body]
        if absent:
            raise PlanError(f"{node} cache/evidence contract drift: missing={absent}")
    v07_gate = re.search(
        r"^- \*\*Gate\*\*：(\S.+)$", contracts["V07"][1], re.MULTILINE
    )
    if v07_gate is None or "TROUPE_DIAGNOSTICS_EVIDENCE" in v07_gate.group(1):
        raise PlanError("V07 ordinary Gate writes persistent evidence")

    final_gate = section(text, "### 8.3 Final release gate", "## 9. D1-D54")
    final_facts = (
        "TROUPE_FINAL_ATTEMPT_ID",
        "--evidence-root \"${TROUPE_DIAGNOSTICS_EVIDENCE:?}/attempts/${TROUPE_FINAL_ATTEMPT_ID:?}\"",
        "--diagnostics-evidence-root \"${TROUPE_DIAGNOSTICS_EVIDENCE:?}/attempts/${TROUPE_FINAL_ATTEMPT_ID:?}\"",
        "audit_diagnostic_ownership.py --all-realized",
        "scripts/run_diagnostic_bootstrap_gate.sh O04",
        "要求145个artifact fragment和145个gate descriptor全部realized",
        "accepted.json",
        "python scripts/publish_diagnostics_acceptance.py",
        "--integration-sha \"${INTEGRATION_SHA:?}\"",
        "root不手工构造或移动JSON",
    )
    missing_final = [fact for fact in final_facts if fact not in final_gate]
    if missing_final:
        raise PlanError(f"final evidence protocol drift: missing={missing_final}")


def validate_decisions(text: str, nodes: set[str]) -> None:
    body = section(text, "## 9. D1-D54 追踪矩阵", "## 10. Plan Freeze")
    decisions: dict[int, set[str]] = {}
    node_coverage: defaultdict[str, set[int]] = defaultdict(set)
    for raw_decision, raw_owners, acceptance in parse_closed_table(
        body, ("Decision", "Owner nodes", "Blocking acceptance")
    ):
        decision_match = re.fullmatch(r"D(?P<decision>[1-9]\d*)", raw_decision)
        if decision_match is None:
            raise PlanError(f"malformed decision ID: {raw_decision!r}")
        decision = int(decision_match.group("decision"))
        if decision in decisions:
            raise PlanError(f"duplicate decision row: D{decision}")
        owner_values = raw_owners.split(", ")
        if any(NODE_RE.fullmatch(owner) is None for owner in owner_values):
            raise PlanError(f"D{decision} has malformed owners: {raw_owners!r}")
        owners = set(owner_values)
        if not owners:
            raise PlanError(f"D{decision} has no owner node")
        unknown = owners - nodes
        if unknown:
            raise PlanError(f"D{decision} has unknown owner nodes: {sorted(unknown)}")
        if not acceptance.strip():
            raise PlanError(f"D{decision} has empty blocking acceptance")
        decisions[decision] = owners
        for node in owners:
            node_coverage[node].add(decision)
    wanted = set(range(1, 55))
    if decisions.keys() != wanted:
        missing = sorted(wanted - decisions.keys())
        extra = sorted(decisions.keys() - wanted)
        raise PlanError(f"decision coverage mismatch: missing={missing}, extra={extra}")
    # Foundation/loader establish substrate; verification/docs own final gates and prose.
    exempt_prefixes = ("F", "L", "V", "O")
    uncovered = sorted(
        node
        for node in nodes
        if not node.startswith(exempt_prefixes) and node not in node_coverage
    )
    if uncovered:
        raise PlanError(f"implementation nodes missing from D1-D54 matrix: {uncovered}")


def validate_text(text: str) -> tuple[int, int, int, int, int, int, int, int, str]:
    dependencies, titles, subprojects = parse_index(text)
    graph_edges = parse_graph(text, set(dependencies))
    validate_verification_dependency_contracts(dependencies)
    validate_graph(dependencies, graph_edges)
    _, outgoing = topological_order(dependencies, graph_edges)
    subproject_count = validate_subprojects(
        subprojects, graph_edges, set(dependencies)
    )
    validate_subproject_table(text, subprojects)
    validate_contracts(text, titles)
    validate_gate_path_ownership(text, dependencies)
    validate_fragment_contract(text, set(dependencies))
    validate_sink_and_publication_contracts(text)
    validate_planning_bundle(text)
    validate_cumulative_gate_contracts(text)
    validate_cache_and_evidence_protocol(text)
    parameterized_family_count, generated_grant_count = validate_ownership_machine_tables(
        text, set(dependencies)
    )
    slot_count, shared_path_count, behavior_owner_count = validate_slot_inventory(
        text, set(dependencies), outgoing
    )
    validate_derived_schedule(
        text, dependencies, subprojects, outgoing, len(graph_edges)
    )
    validate_decisions(text, set(dependencies))
    digest = hashlib.sha256(text.encode("utf-8")).hexdigest()
    return (
        len(dependencies),
        len(graph_edges),
        subproject_count,
        slot_count,
        shared_path_count,
        behavior_owner_count,
        parameterized_family_count,
        generated_grant_count,
        digest,
    )


def mutate(text: str, old: str, new: str, label: str) -> str:
    if old not in text:
        raise PlanError(f"self-test mutation target missing: {label}")
    mutated = text.replace(old, new, 1)
    if mutated == text:
        raise PlanError(f"self-test mutation made no change: {label}")
    return mutated


def expect_failure(text: str, label: str, expected_message: str) -> None:
    try:
        validate_text(text)
    except PlanError as error:
        if expected_message not in str(error):
            raise PlanError(
                f"self-test {label} failed for the wrong reason: {error}"
            ) from error
        return
    raise PlanError(f"self-test mutation unexpectedly passed: {label}")


def run_self_test(text: str) -> None:
    baseline_drift = mutate(
        text,
        f"规划基线：`main@{EXPECTED_PRODUCT_BASE_SHA}`",
        "规划基线：`main@0000000000000000000000000000000000000000`",
        "planning baseline drift",
    )
    expect_failure(
        baseline_drift,
        "planning baseline drift",
        "planning baseline SHA drift",
    )
    missing_gate_path = mutate(
        text,
        "tests/integration/test_actor_act_diagnostic_sink_binding.py`。\n- **边界**：不改变public custom/sink values",
        "tests/integration/test_actor_act_diagnostic_sink_projection.py`。\n- **边界**：不改变public custom/sink values",
        "Gate path without owner",
    )
    expect_failure(
        missing_gate_path,
        "Gate path without owner",
        "B14 Gate path has no baseline/artifact owner",
    )
    nonancestor_gate_path = mutate(
        text,
        "tests/integration/test_actor_act_diagnostic_sink_binding.py`。\n- **边界**：不改变public custom/sink values",
        "tests/integration/test_diagnostic_cli_dump.py`。\n- **边界**：不改变public custom/sink values",
        "Gate path owned by non-ancestor",
    )
    expect_failure(
        nonancestor_gate_path,
        "Gate path owned by non-ancestor",
        "B14 Gate path is owned only by non-ancestors",
    )
    graph_mismatch = mutate(
        text, "F00 --> F01", "F00 --> C01", "graph mismatch"
    )
    expect_failure(graph_mismatch, "graph mismatch", "edge mismatch")
    v02_release_serialization = mutate(
        text,
        "| V02 | Full-system happy-path E2E matrix | verify-system | X02 |",
        "| V02 | Full-system happy-path E2E matrix | verify-system | V01 |",
        "V02 release serialization",
    )
    v02_release_serialization = mutate(
        v02_release_serialization,
        "  X02 --> V02",
        "  V01 --> V02",
        "V02 release serialization graph",
    )
    expect_failure(
        v02_release_serialization,
        "V02 release serialization",
        "verification dependency contract drift",
    )
    v15_runtime_dependency = mutate(
        text,
        "| V15 | Perfetto compatibility release mode | verify-perfetto-quality | T02 |",
        "| V15 | Perfetto compatibility release mode | verify-perfetto-quality | T02,X02 |",
        "V15 runtime dependency",
    )
    v15_runtime_dependency = mutate(
        v15_runtime_dependency,
        "  T02 --> V15",
        "  T02 --> V15\n  X02 --> V15",
        "V15 runtime dependency graph",
    )
    expect_failure(
        v15_runtime_dependency,
        "V15 runtime dependency",
        "verification dependency contract drift",
    )
    missing_v01_final_join = mutate(
        text,
        "| V03 | Final release runner closure | verify-final | O04,V01,V16 |",
        "| V03 | Final release runner closure | verify-final | O04,V16 |",
        "missing V01 final join",
    )
    missing_v01_final_join = mutate(
        missing_v01_final_join,
        "  V01 --> V03\n",
        "",
        "missing V01 final join graph",
    )
    expect_failure(
        missing_v01_final_join,
        "missing V01 final join",
        "verification dependency contract drift",
    )
    missing_standalone_join = mutate(
        text,
        "| B16 | Act sink seal、`wait_closed()` 与 summary settlement | producer-act-sink | B17,B18 |",
        "| B16 | Act sink seal、`wait_closed()` 与 summary settlement | producer-act-sink | B18 |",
        "missing standalone producer join",
    )
    missing_standalone_join = mutate(
        missing_standalone_join,
        "  B17 --> B16\n",
        "",
        "missing standalone producer join graph",
    )
    expect_failure(
        missing_standalone_join,
        "missing standalone producer join",
        "implementation dependency closure drift",
    )
    cyclic = mutate(
        text,
        "| F01 | Diagnostics workspace crates 与 dependency graph | foundation | F00 |",
        "| F01 | Diagnostics workspace crates 与 dependency graph | foundation | V03 |",
        "cycle index",
    )
    cyclic = mutate(cyclic, "F00 --> F01", "V03 --> F01", "cycle graph")
    expect_failure(cyclic, "cycle", "contains a cycle")
    missing_dependency = mutate(
        text,
        "| C00 | Canonical scalar、ID、时间与 JSON wire primitives | core-model | F04 |",
        "| C00 | Canonical scalar、ID、时间与 JSON wire primitives | core-model | Z99 |",
        "missing dependency",
    )
    expect_failure(missing_dependency, "missing dependency", "unknown dependencies")
    subproject_drift = mutate(
        text,
        "`core-hub` (C02)",
        "`core-hub-drift` (C02)",
        "subproject allocation drift",
    )
    expect_failure(
        subproject_drift,
        "subproject allocation drift",
        "subproject table/index mismatch",
    )
    missing_acceptance = mutate(
        text, "- **验收**：", "- **missing-acceptance**：", "missing acceptance"
    )
    expect_failure(missing_acceptance, "missing acceptance", "验收 field")
    prose_gate = re.sub(
        r"^- \*\*Gate\*\*：.*$",
        "- **Gate**：manual inspection only。",
        text,
        count=1,
        flags=re.MULTILINE,
    )
    if prose_gate == text:
        raise PlanError("self-test mutation made no change: prose gate")
    expect_failure(prose_gate, "prose gate", "no literal executable command")
    bare_uv_gate = re.sub(
        r"^- \*\*Gate\*\*：.*$",
        "- **Gate**：`uv run --no-sync pytest -q tests/unit/test_artifact_layout.py`。",
        text,
        count=1,
        flags=re.MULTILINE,
    )
    if bare_uv_gate == text:
        raise PlanError("self-test mutation made no change: bare uv gate")
    expect_failure(bare_uv_gate, "bare uv gate", "bypasses the isolated gate runner")
    glob_artifact = mutate(
        text,
        "`tests/fixtures/artifact_layout/{index,base}.json`",
        "`tests/fixtures/artifact_layout/*.json`",
        "artifact glob",
    )
    expect_failure(glob_artifact, "artifact glob", "artifact contains a glob")
    fragment_drift = mutate(
        text,
        "index.json`显式枚举第3.1节的每个node ID",
        "index.json`未枚举node ID",
        "fragment contract",
    )
    expect_failure(fragment_drift, "fragment contract", "fragment contract drift")
    role_drift = mutate(
        text,
        "introduced ∪ modified ∪ removed ∪ generated",
        "introduced or modified or removed or generated",
        "role-aware fragment equality",
    )
    expect_failure(role_drift, "role-aware fragment equality", "fragment contract drift")
    state_drift = mutate(
        text,
        "artifact family的F00 fragment为`state=realized`，其余artifact fragment为`state=planned`",
        "artifact family的F00 fragment为`state=realized`，其余artifact fragment为`state=pending`",
        "fragment lifecycle state",
    )
    expect_failure(state_drift, "fragment lifecycle state", "F00 fragment lifecycle drift")
    gate_schema_drift = mutate(
        text,
        "`diagnostic_node_gates/<node-id>.json`是structured gate descriptor",
        "`diagnostic_node_gates/<node-id>.json`是第二种ownership fragment",
        "gate descriptor schema separation",
    )
    expect_failure(
        gate_schema_drift,
        "gate descriptor schema separation",
        "fragment contract drift",
    )
    projected_equality_drift = mutate(
        text,
        "计算projected writer集合，与ledger/grant双向相等",
        "计算projected writer集合",
        "projected ledger equality",
    )
    expect_failure(
        projected_equality_drift,
        "projected ledger equality",
        "F02 fragment lifecycle drift",
    )
    family_writer_drift = mutate(
        text,
        "| `tests/fixtures/artifact_layout/nodes/<node-id>.json` | artifact-fragment | `state,introduced,modified,removed,generated` | F00 | `<node-id>` | index-exact |",
        "| `tests/fixtures/artifact_layout/nodes/<node-id>.json` | artifact-fragment | `state,introduced,modified,removed,generated` | F01 | `<node-id>` | index-exact |",
        "fragment family writer",
    )
    expect_failure(
        family_writer_drift,
        "fragment family writer",
        "fragment family contract mismatch",
    )
    family_row = (
        "| `tests/fixtures/diagnostic_node_gates/<node-id>.json` | "
        "gate-descriptor | `state,argv,env,maturin_features,cache_requirements,exclusive_resources` | "
        "F00 | `<node-id>` | index-exact |"
    )
    third_family = mutate(
        text,
        family_row,
        family_row
        + "\n| `tests/fixtures/extra/<node-id>.json` | "
        + "artifact-fragment | `state,introduced,modified,removed,generated` | "
        + "F00 | `<node-id>` | index-exact |",
        "third fragment family",
    )
    expect_failure(
        third_family,
        "third fragment family",
        "fragment family contract mismatch",
    )
    gate_closed_field_drift = mutate(
        text,
        "| `tests/fixtures/diagnostic_node_gates/<node-id>.json` | gate-descriptor | `state,argv,env,maturin_features,cache_requirements,exclusive_resources` | F00 | `<node-id>` | index-exact |",
        "| `tests/fixtures/diagnostic_node_gates/<node-id>.json` | gate-descriptor | `state,argv,env,maturin_features,cache_requirements,exclusive_resources,introduced` | F00 | `<node-id>` | index-exact |",
        "gate descriptor closed fields",
    )
    expect_failure(
        gate_closed_field_drift,
        "gate descriptor closed fields",
        "fragment family contract mismatch",
    )
    malformed_resource_row = mutate(
        text,
        "| V05 | `benchmark-host` |",
        "| V05 | `benchmark-host` | rogue |",
        "machine table extra column",
    )
    expect_failure(
        malformed_resource_row,
        "machine table extra column",
        "malformed machine table row",
    )
    rogue_resource_row = mutate(
        text,
        "| V05 | `benchmark-host` |",
        "| V05 | `benchmark-host` |\n| V07 | `benchmark-host` |",
        "rogue exclusive resource",
    )
    expect_failure(
        rogue_resource_row,
        "rogue exclusive resource",
        "exclusive resource contract mismatch",
    )
    generated_field_drift = mutate(
        text,
        "| W14 | `files[].path` | `rust/crates/troupe-diagnostics-runtime/assets/generated/` |",
        "| W14 | `files[].bogus` | `rust/crates/troupe-diagnostics-runtime/assets/generated/` |",
        "generated grant field",
    )
    expect_failure(
        generated_field_drift,
        "generated grant field",
        "generated grant contract mismatch",
    )
    generated_template_drift = mutate(
        text,
        "`diagnostics-<sha256>.{js,css}.{raw,gz,br}` | 6 |",
        "`diagnostics-<sha256>.{js,css}.{raw,gz,zst}` | 6 |",
        "generated grant template",
    )
    expect_failure(
        generated_template_drift,
        "generated grant template",
        "generated grant contract mismatch",
    )
    generated_cardinality_drift = mutate(
        text,
        "`diagnostics-<sha256>.{js,css}.{raw,gz,br}` | 6 |",
        "`diagnostics-<sha256>.{js,css}.{raw,gz,br}` | 5 |",
        "generated grant cardinality",
    )
    expect_failure(
        generated_cardinality_drift,
        "generated grant cardinality",
        "generated grant contract mismatch",
    )
    unregistered_placeholder = mutate(
        text,
        "`frontend/diagnostics/scripts/build.mjs`",
        "`frontend/diagnostics/scripts/build-<sha256>.mjs`",
        "unregistered artifact placeholder",
    )
    expect_failure(
        unregistered_placeholder,
        "unregistered artifact placeholder",
        "unregistered placeholder",
    )
    w00_registry_drift = mutate(
        text,
        "--allow-registry --check-toolchain --unit",
        "--check-toolchain --unit",
        "W00 registry authority",
    )
    expect_failure(
        w00_registry_drift,
        "W00 registry authority",
        "W00 Gate must explicitly allow",
    )
    frontend_cache_drift = mutate(
        text,
        "--npm-cache \"${TROUPE_NPM_CACHE:?}\" --typecheck --unit tests/unit/protocol-events.test.ts",
        "--typecheck --unit tests/unit/protocol-events.test.ts",
        "frontend npm cache",
    )
    expect_failure(
        frontend_cache_drift,
        "frontend npm cache",
        "frontend Gate omits the pinned npm cache",
    )
    direct_npm_gate = mutate(
        text,
        "--unit tests/unit/bundle-contract.test.ts`。",
        "--unit tests/unit/bundle-contract.test.ts`；`npm install left-pad`。",
        "direct npm bypass",
    )
    expect_failure(
        direct_npm_gate,
        "direct npm bypass",
        "bypasses maintain.mjs with npm/npx",
    )
    absolute_npm_gate = mutate(
        text,
        "--unit tests/unit/bundle-contract.test.ts`。",
        "--unit tests/unit/bundle-contract.test.ts`；`/usr/bin/npm install left-pad`。",
        "absolute npm bypass",
    )
    expect_failure(
        absolute_npm_gate,
        "absolute npm bypass",
        "bypasses maintain.mjs with npm/npx",
    )
    absolute_npx_gate = mutate(
        text,
        "--unit tests/unit/bundle-contract.test.ts`。",
        "--unit tests/unit/bundle-contract.test.ts`；`/usr/bin/npx vite build`。",
        "absolute npx bypass",
    )
    expect_failure(
        absolute_npx_gate,
        "absolute npx bypass",
        "bypasses maintain.mjs with npm/npx",
    )
    nested_acceptance_write = mutate(
        text,
        "--unit tests/unit/bundle-contract.test.ts`。",
        "--unit tests/unit/bundle-contract.test.ts`；"
        "`python -c 'open(\"accepted.json\",\"w\")'`。",
        "nested accepted evidence write",
    )
    expect_failure(
        nested_acceptance_write,
        "nested accepted evidence write",
        "ordinary Gate writes persistent evidence",
    )
    nested_archive_evidence_write = mutate(
        text,
        "--unit tests/unit/bundle-contract.test.ts`。",
        "--unit tests/unit/bundle-contract.test.ts`；"
        "`python -c 'open(\".troupe/diagnostics/evidence/rogue.json\",\"w\")'`。",
        "nested archive evidence write",
    )
    expect_failure(
        nested_archive_evidence_write,
        "nested archive evidence write",
        "ordinary Gate writes persistent evidence",
    )
    w06_temp_artifact = mutate(
        text,
        "`frontend/diagnostics/tests/unit/bundle-contract.test.ts`。",
        "`frontend/diagnostics/tests/unit/bundle-contract.test.ts`；"
        "`frontend/diagnostics/.tmp-dist-manifest.json`。",
        "W06 temporary artifact",
    )
    expect_failure(
        w06_temp_artifact,
        "W06 temporary artifact",
        "W06 exact artifact set drift",
    )
    unknown_root_artifact = mutate(
        text,
        "`frontend/diagnostics/scripts/build.mjs`",
        "`dist/rogue.bin`",
        "unknown artifact root",
    )
    expect_failure(
        unknown_root_artifact,
        "unknown artifact root",
        "artifact has an unknown repository root",
    )
    question_glob_artifact = mutate(
        text,
        "`frontend/diagnostics/scripts/build.mjs`",
        "`frontend/diagnostics/scripts/build?.mjs`",
        "question-mark artifact glob",
    )
    expect_failure(
        question_glob_artifact,
        "question-mark artifact glob",
        "artifact contains a glob",
    )
    dot_prefixed_artifact = mutate(
        text,
        "`frontend/diagnostics/scripts/build.mjs`",
        "`./frontend/diagnostics/scripts/build.mjs`",
        "dot-prefixed artifact",
    )
    expect_failure(
        dot_prefixed_artifact,
        "dot-prefixed artifact",
        "artifact path is not canonical repository-relative",
    )
    parent_segment_artifact = mutate(
        text,
        "`scripts/test_perfetto_compatibility.sh`",
        "`scripts/../scripts/test_perfetto_compatibility.sh`",
        "parent-segment artifact",
    )
    expect_failure(
        parent_segment_artifact,
        "parent-segment artifact",
        "artifact path is not canonical repository-relative",
    )
    concrete_gate_artifact = mutate(
        text,
        "`tests/unit/test_diagnostic_worktree_gate.py`。",
        "`tests/unit/test_diagnostic_worktree_gate.py`；"
        "`tests/fixtures/diagnostic_node_gates/F03.json`。",
        "concrete gate descriptor artifact",
    )
    expect_failure(
        concrete_gate_artifact,
        "concrete gate descriptor artifact",
        "leaks a concrete gate descriptor",
    )
    v00_directory_artifact = mutate(
        text,
        "`frontend/diagnostics/tests/e2e/visual/baselines/{chromium,firefox,webkit}-{desktop,mobile}-{active,archive}.png`",
        "`frontend/diagnostics/tests/e2e/visual/baselines/`",
        "V00 directory artifact",
    )
    expect_failure(
        v00_directory_artifact,
        "V00 directory artifact",
        "V00 artifact path is not canonical repository-relative",
    )
    v05_persistent_gate = mutate(
        text,
        "--raw-report \"${TROUPE_GATE_TMP:?}/V05-performance-raw.json\"",
        "--raw-report \"${TROUPE_GATE_TMP:?}/V05-performance-raw.json\" "
        "--persistent-copy \"${TROUPE_DIAGNOSTICS_EVIDENCE:?}/V05.json\"",
        "V05 persistent ordinary Gate",
    )
    expect_failure(
        v05_persistent_gate,
        "V05 persistent ordinary Gate",
        "V05 ordinary Gate writes persistent evidence",
    )
    v07_persistent_gate = mutate(
        text,
        "${TROUPE_GATE_TMP:?}/V07-wheel-report.json",
        "${TROUPE_DIAGNOSTICS_EVIDENCE:?}/V07-wheel-report.json",
        "V07 persistent ordinary Gate",
    )
    expect_failure(
        v07_persistent_gate,
        "V07 persistent ordinary Gate",
        "V07 ordinary Gate writes persistent evidence",
    )
    unrooted_artifact = mutate(
        text,
        "`rust/crates/troupe-diagnostics-core/tests/scalar_wire.rs`",
        "`tests/scalar_wire.rs`",
        "unrooted crate test artifact",
    )
    expect_failure(
        unrooted_artifact,
        "unrooted crate test artifact",
        "ambiguous crate-local test path",
    )
    ignored_bundle = mutate(
        text, "git add -f", "git add", "ignored planning bundle"
    )
    expect_failure(
        ignored_bundle,
        "ignored planning bundle",
        "accepted planning bundle protocol drift",
    )
    missing_actor_design_bundle = mutate(
        text,
        "   `git add -f`纳入`docs/design/actor-agent-session.md`、\n",
        "   `git add -f`纳入",
        "planning bundle Actor design",
    )
    expect_failure(
        missing_actor_design_bundle,
        "planning bundle Actor design",
        "accepted planning bundle protocol drift",
    )
    root_cargo_fmt_drift = mutate(
        text,
        "`cargo fmt --manifest-path rust/Cargo.toml --all -- --check`",
        "`cargo fmt --check --all`",
        "root cumulative cargo fmt",
    )
    expect_failure(
        root_cargo_fmt_drift,
        "root cumulative cargo fmt",
        "cumulative Rust Gate drift",
    )
    root_cargo_check_drift = mutate(
        text,
        "；`cargo check --locked --manifest-path rust/Cargo.toml --workspace --all-targets --all-features` |",
        "；`cargo check --locked --workspace --all-targets --all-features` |",
        "root cumulative cargo check",
    )
    expect_failure(
        root_cargo_check_drift,
        "root cumulative cargo check",
        "cumulative Rust Gate drift",
    )
    acceptance_overwrite = mutate(
        text,
        "fail closed且绝不覆盖旧acceptance",
        "失败时允许覆盖旧acceptance",
        "acceptance no-overwrite",
    )
    expect_failure(
        acceptance_overwrite,
        "acceptance no-overwrite",
        "cache/evidence protocol drift",
    )
    perfetto_definition_file_drift = mutate(
        text,
        "protos/perfetto/trace/track_event/debug_annotation.proto",
        "protos/perfetto/trace/track_event/debug_annotation_missing.proto",
        "Perfetto debug annotation definition file",
    )
    expect_failure(
        perfetto_definition_file_drift,
        "Perfetto debug annotation definition file",
        "T00 executable contract drift",
    )
    perfetto_definition_closure_drift = mutate(
        text,
        "未选中的upstream import/oneof arm不属于closure",
        "所有upstream import均隐式属于closure",
        "Perfetto used-definition closure",
    )
    expect_failure(
        perfetto_definition_closure_drift,
        "Perfetto used-definition closure",
        "T00 executable contract drift",
    )
    publication_state_drift = mutate(
        text,
        "结果closed为published/not_published/publication_indeterminate",
        "结果只区分success/failure",
        "Perfetto publication three-state",
    )
    expect_failure(
        publication_state_drift,
        "Perfetto publication three-state",
        "T08 executable contract drift",
    )
    active_lease_drift = mutate(
        text,
        "active profile复用S05 Runtime-held borrowed guard且断言零lock acquire/release",
        "active profile重新取得shared lease",
        "active dump borrowed guard",
    )
    expect_failure(
        active_lease_drift,
        "active dump borrowed guard",
        "H05 executable contract drift",
    )
    timeseries_bucket_drift = mutate(
        text,
        "`max_points=1024`、`width=max(1,ceil(duration/1023))`",
        "`max_points=2048`、implementation-defined width",
        "TimeSeries bucket contract",
    )
    expect_failure(
        timeseries_bucket_drift,
        "TimeSeries bucket contract",
        "C05 executable contract drift",
    )
    timeseries_client_stale_drift = mutate(
        text,
        "pan/zoom或width变化时abort-or-ignore旧inflight response",
        "pan/zoom时允许旧response完成",
        "TimeSeries client stale binding",
    )
    expect_failure(
        timeseries_client_stale_drift,
        "TimeSeries client stale binding",
        "W10 executable contract drift",
    )
    clean_failed_view_drift = mutate(
        text,
        "固定完成`outcome=failed,clean_shutdown=true` terminal transaction",
        "保持incomplete且继续构造Production",
        "invalid ViewSpec clean failure",
    )
    expect_failure(
        clean_failed_view_drift,
        "invalid ViewSpec clean failure",
        "B08 executable contract drift",
    )
    active_query_fatal_drift = mutate(
        text,
        "active Q00 reader的SQLite corruption/identity/dense-prefix invariant failure以及active Q01/H03 query worker/execution-context系统性退出",
        "active query failures只返回单个request error",
        "active query core fatal",
    )
    expect_failure(
        active_query_fatal_drift,
        "active query core fatal",
        "X01 executable contract drift",
    )
    usage_terminal_boundary_drift = mutate(
        text,
        "pre-submission Act terminal直接以`prompt_not_submitted` finalise",
        "只在authoritative settlement finalise",
        "usage terminal boundaries",
    )
    expect_failure(
        usage_terminal_boundary_drift,
        "usage terminal boundaries",
        "B17 executable contract drift",
    )
    sink_component_taxonomy_drift = mutate(
        text,
        "diagnostic-component-failed.json",
        "diagnostic-sink-failed.json",
        "sink component failure taxonomy",
    )
    expect_failure(
        sink_component_taxonomy_drift,
        "sink component failure taxonomy",
        "C03 executable contract drift",
    )
    sink_drop_bridge_drift = mutate(
        text,
        "K00普通DropDelta按Act/sink scope合并为cumulative `diagnostic.dropped_events` counter且不发component failure",
        "K00普通DropDelta直接发component failure",
        "sink drop delivery bridge",
    )
    expect_failure(
        sink_drop_bridge_drift,
        "sink drop delivery bridge",
        "B18 executable contract drift",
    )
    sink_visibility_e2e_drift = mutate(
        text,
        "store、HTTP/Web events与CLI恰好一次可见",
        "只在内部队列可见",
        "sink delivery system visibility",
    )
    expect_failure(
        sink_visibility_e2e_drift,
        "sink delivery system visibility",
        "V06 executable contract drift",
    )
    agent_member_manifest_drift = mutate(
        text,
        "`rust/crates/troupe-agent-runtime/Cargo.toml`的member dependency/ACP feature edges",
        "agent-runtime member dependency/ACP feature edges",
        "agent member manifest ownership",
    )
    expect_failure(
        agent_member_manifest_drift,
        "agent member manifest ownership",
        "F01 executable contract drift",
    )
    result_seam_ownership_drift = mutate(
        text,
        "`rust/crates/troupe-agent-runtime/src/result/mod.rs`的no-op result-transition observation seam",
        "result-transition observation seam",
        "result state-machine seam ownership",
    )
    expect_failure(
        result_seam_ownership_drift,
        "result state-machine seam ownership",
        "F06 executable contract drift",
    )
    result_counter_producer_drift = mutate(
        text,
        "cumulative `result.validation_rejections` counter candidate，值严格为1..N",
        "optional validation counter candidate",
        "result rejection counter producer",
    )
    expect_failure(
        result_counter_producer_drift,
        "result rejection counter producer",
        "A08 executable contract drift",
    )
    active_turn_counter_producer_drift = mutate(
        text,
        "`agent.turn.active=1`，在matching SpanFinished admission后恰好发`agent.turn.active=0`",
        "implementation-defined active-turn samples",
        "active turn counter producer",
    )
    expect_failure(
        active_turn_counter_producer_drift,
        "active turn counter producer",
        "B05 executable contract drift",
    )
    result_counter_admission_drift = mutate(
        text,
        "cumulative `result.validation_rejections` counter各恰好admit一次",
        "result counter可选admission",
        "result rejection counter admission",
    )
    expect_failure(
        result_counter_admission_drift,
        "result rejection counter admission",
        "B12 executable contract drift",
    )
    e2e_port_isolation_drift = mutate(
        text,
        "V02/V06各自只用OS-assigned isolated ports",
        "V02/V06争用fixed release ports",
        "E2E port isolation",
    )
    expect_failure(
        e2e_port_isolation_drift,
        "E2E port isolation",
        "V12 executable contract drift",
    )
    scene_counter_taxonomy_drift = mutate(
        text,
        "不生成closed taxonomy之外的`scene.active` counter",
        "Scene active counter准确",
        "scene counter taxonomy",
    )
    expect_failure(
        scene_counter_taxonomy_drift,
        "scene counter taxonomy",
        "B02 executable contract drift",
    )
    result_capture_counter_drift = mutate(
        text,
        "`result_validation`同时控制五种transition metadata和`result.validation_rejections` counter",
        "`result_validation`只控制五种transition metadata",
        "result counter capture matrix",
    )
    expect_failure(
        result_capture_counter_drift,
        "result counter capture matrix",
        "P01 executable contract drift",
    )
    sink_only_hub_drift = mutate(
        text,
        "binding-owned C02 sink-only bounded in-memory hub",
        "sink-only hub后续再实现",
        "standalone sink-only hub",
    )
    expect_failure(
        sink_only_hub_drift,
        "standalone sink-only hub",
        "B18 executable contract drift",
    )
    standalone_observer_attach_drift = mutate(
        text,
        "安装bind-frozen `TurnDiagnosticContext`",
        "standalone observer以后再连接",
        "standalone per-turn observer attach",
    )
    expect_failure(
        standalone_observer_attach_drift,
        "standalone per-turn observer attach",
        "B18 executable contract drift",
    )
    production_payload_sidecar_drift = mutate(
        text,
        "注册A09 input/output sidecar policy",
        "Production不支持per-turn payload sidecar",
        "production payload sidecar",
    )
    expect_failure(
        production_payload_sidecar_drift,
        "production payload sidecar",
        "B18 executable contract drift",
    )
    production_payload_e2e_drift = mutate(
        text,
        "真实Production session observer + per-turn sidecar进入opt-in sink",
        "Production payload只做unit测试",
        "production payload E2E",
    )
    expect_failure(
        production_payload_e2e_drift,
        "production payload E2E",
        "V02 executable contract drift",
    )
    standalone_full_chain_owner_drift = mutate(
        text,
        "B12/B17/B18汇合后的standalone full-chain owner",
        "B18单独负责standalone full-chain",
        "standalone full-chain owner",
    )
    expect_failure(
        standalone_full_chain_owner_drift,
        "standalone full-chain owner",
        "B16 executable contract drift",
    )
    production_volatile_fallback_drift = mutate(
        text,
        "profile不能在Run中切换或让Production降级到volatile",
        "Production失败时可降级到volatile",
        "production volatile fallback",
    )
    expect_failure(
        production_volatile_fallback_drift,
        "production volatile fallback",
        "C02 executable contract drift",
    )
    prehub_path_span_drift = mutate(
        text,
        "初始CLI root语法/存在性验证不属于该span，不能事后伪造elapsed",
        "初始CLI root解析在hub ready后补写历史span",
        "pre-hub path span",
    )
    expect_failure(
        prehub_path_span_drift,
        "pre-hub path span",
        "B09 executable contract drift",
    )
    capture_usage_drift = mutate(
        text,
        "mailbox/Cue/Run级counter明确排除",
        "所有counter均允许进入Act sink",
        "capture matrix usage",
    )
    expect_failure(
        capture_usage_drift,
        "capture matrix usage",
        "P01 executable contract drift",
    )
    session_bridge_drift = mutate(
        text,
        "A00的session opening/lifecycle/closing/closed观察映射为exactly-once canonical session span start/finish",
        "A00 session observation暂不映射",
        "canonical session bridge",
    )
    expect_failure(
        session_bridge_drift,
        "canonical session bridge",
        "B12 executable contract drift",
    )
    sync_callback_drift = mutate(
        text,
        "阻塞型sync callback会占用同一个diagnostic loop并延迟其他sink",
        "阻塞型sync callback不影响任何其他sink",
        "sync callback loop semantics",
    )
    expect_failure(
        sync_callback_drift,
        "sync callback loop semantics",
        "K01 executable contract drift",
    )
    forwarded_header_drift = mutate(
        text,
        "`X-Forwarded-Prefix`",
        "`Trusted-Proxy-Prefix`",
        "forwarded header ignore",
    )
    expect_failure(
        forwarded_header_drift,
        "forwarded header ignore",
        "H00 executable contract drift",
    )
    owner_mode_drift = mutate(
        text,
        "owner-only `0700`",
        "implementation-defined mode",
        "archive owner-only mode",
    )
    expect_failure(
        owner_mode_drift,
        "archive owner-only mode",
        "S00 executable contract drift",
    )
    remote_dump_endpoint_drift = mutate(
        text,
        "identity-checked read-only `GET /api/v1/dump`",
        "identity-checked read-only dump operation",
        "remote Perfetto dump endpoint",
    )
    expect_failure(
        remote_dump_endpoint_drift,
        "remote Perfetto dump endpoint",
        "H05 executable contract drift",
    )
    remote_dump_cli_drift = mutate(
        text,
        "URL/active target只调用H05 endpoint",
        "URL target尚未接入",
        "remote Perfetto dump CLI",
    )
    expect_failure(
        remote_dump_cli_drift,
        "remote Perfetto dump CLI",
        "D06 executable contract drift",
    )
    sink_terminal_order_drift = mutate(
        text,
        "B17完成唯一canonical usage admission -> B05完成`act.lifecycle` SpanFinished admission",
        "B17和B05最终都完成",
        "sink terminal order",
    )
    expect_failure(
        sink_terminal_order_drift,
        "sink terminal order",
        "B16 executable contract drift",
    )
    public_payload_drift = mutate(
        text,
        "另外定义D38的immutable public `FrozenJsonArray`、`FrozenJsonObject`、closed `FrozenJsonValue` alias、`DiagnosticToolLocation`、`DiagnosticToolInput`和`DiagnosticToolOutput`",
        "另外内部处理D38 payload",
        "public tool payload ownership",
    )
    expect_failure(
        public_payload_drift,
        "public tool payload ownership",
        "P00 executable contract drift",
    )
    b06_actor_artifact_drift = mutate(
        text,
        "`rust/src/orchestration/actor.rs`的真实PyO3 method signature/pre-context validation调用；",
        "",
        "B06 Actor.act artifact",
    )
    expect_failure(
        b06_actor_artifact_drift,
        "B06 Actor.act artifact",
        "B06 executable contract drift",
    )
    readme_artifact_drift = mutate(
        text,
        "`README.md`的diagnostics入口",
        "README的diagnostics入口",
        "O00 README artifact",
    )
    expect_failure(
        readme_artifact_drift,
        "O00 README artifact",
        "O00 executable contract drift",
    )
    publisher_artifact_drift = mutate(
        text,
        "`scripts/publish_diagnostics_acceptance.py`；",
        "",
        "acceptance publisher artifact",
    )
    expect_failure(
        publisher_artifact_drift,
        "acceptance publisher artifact",
        "V16 executable contract drift",
    )
    publisher_schema_drift = mutate(
        text,
        "`tests/fixtures/release/{diagnostics-final-evidence-schema.json,diagnostics-accepted-evidence-schema.json}`",
        "`tests/fixtures/release/diagnostics-accepted-evidence-schema.json`",
        "acceptance publisher schema artifact",
    )
    expect_failure(
        publisher_schema_drift,
        "acceptance publisher schema artifact",
        "V16 executable contract drift",
    )
    actor_evidence_binding_drift = mutate(
        text,
        "actor-design/diagnostics-design/plan/validator/integration/artifact/result SHA",
        "diagnostics-design/plan/validator/integration/artifact/result SHA",
        "Actor design evidence binding",
    )
    expect_failure(
        actor_evidence_binding_drift,
        "Actor design evidence binding",
        "V07 executable contract drift",
    )
    publisher_extra_owner = mutate(
        text,
        "- **产物**：`scripts/test_diagnostics_final.sh`；`tests/unit/test_diagnostics_final_runner.py`。",
        "- **产物**：`scripts/test_diagnostics_final.sh`；"
        "`tests/unit/test_diagnostics_final_runner.py`；"
        "`scripts/publish_diagnostics_acceptance.py`。",
        "publisher additional owner",
    )
    expect_failure(
        publisher_extra_owner,
        "publisher additional owner",
        "required artifact ownership drift",
    )
    publisher_durability_drift = mutate(
        text,
        "通过accepted所在目录内`O_EXCL` staging、完整write、file fsync、保留staging fd、no-overwrite hard-link publish、staging-name unlink和directory fsync创建一次`accepted.json`",
        "通过普通临时文件创建`accepted.json`",
        "acceptance publisher durability",
    )
    expect_failure(
        publisher_durability_drift,
        "acceptance publisher durability",
        "V16 executable contract drift",
    )
    final_invocation_drift = mutate(
        text,
        "任一前置失败不得调用publisher",
        "前置失败也可以调用publisher",
        "final publisher invocation",
    )
    expect_failure(
        final_invocation_drift,
        "final publisher invocation",
        "V03 executable contract drift",
    )
    writer_order = mutate(
        text,
        "| `rust/crates/troupe-diagnostics-core/src/lib.rs` | F01 -> F04 |",
        "| `rust/crates/troupe-diagnostics-core/src/lib.rs` | F04 -> F01 |",
        "writer order",
    )
    expect_failure(writer_order, "writer order", "shared writer order is not reachable")
    ghost_shared_row = mutate(
        text,
        "| `rust/Cargo.toml` | F01 -> F05 |",
        "| `tests/fixtures/ghost-static.json` | F00 | ghost |\n"
        "| `rust/Cargo.toml` | F01 -> F05 |",
        "ghost shared writer row",
    )
    expect_failure(
        ghost_shared_row,
        "ghost shared writer row",
        "shared writer row is not bidirectionally grounded",
    )
    actor_writer_drift = mutate(
        text,
        "| `rust/src/orchestration/actor.rs` | F05 -> B06 |",
        "| `rust/src/orchestration/actor.rs` | F05 |",
        "Actor.act shared writer",
    )
    expect_failure(
        actor_writer_drift,
        "Actor.act shared writer",
        "contract/shared writer mismatch",
    )
    missing_shared_artifact = mutate(
        text,
        "| `scripts/verify_wheel.py` | F00 -> V07 | F00等价迁移，V07只增加diagnostics assertions |\n",
        "",
        "missing shared contract artifact",
    )
    expect_failure(
        missing_shared_artifact,
        "missing shared contract artifact",
        "multi-writer contract artifact is missing",
    )
    missing_slot_owner = mutate(
        text,
        "| C04 | `rust/crates/troupe-diagnostics-core/src/validate.rs` |\n",
        "",
        "missing slot behavior owner",
    )
    expect_failure(
        missing_slot_owner,
        "missing slot behavior owner",
        "slot behavior coverage mismatch",
    )
    missing_slot_contract_owner = mutate(
        text,
        "`rust/crates/troupe-diagnostics-core/src/{scalar,id,time,wire}.rs`；"
        "`rust/crates/troupe-diagnostics-core/tests/scalar_wire.rs`。",
        "`rust/crates/troupe-diagnostics-core/src/{scalar,time,wire}.rs`；"
        "`rust/crates/troupe-diagnostics-core/tests/scalar_wire.rs`。",
        "slot contract behavior owner deletion",
    )
    expect_failure(
        missing_slot_contract_owner,
        "slot contract behavior owner deletion",
        "slot behavior owner is missing from its contract artifact",
    )
    moved_slot_behavior_owner = mutate(
        text,
        "| C00 | `rust/crates/troupe-diagnostics-core/src/{scalar,id,time,wire}.rs` |",
        "| C01 | `rust/crates/troupe-diagnostics-core/src/{scalar,id,time,wire}.rs` |",
        "slot behavior owner reassignment",
    )
    expect_failure(
        moved_slot_behavior_owner,
        "slot behavior owner reassignment",
        "slot contract has an unexpected writer",
    )
    successor_as_primary_owner = mutate(
        text,
        "| T01 | `rust/crates/troupe-diagnostics-perfetto/src/{collect,identity,tracks,project}.rs` |\n"
        "| T03 | `rust/crates/troupe-diagnostics-perfetto/src/dump.rs` |",
        "| T01 | `rust/crates/troupe-diagnostics-perfetto/src/{identity,project}.rs` |\n"
        "| T03 | `rust/crates/troupe-diagnostics-perfetto/src/{collect,tracks,dump}.rs` |",
        "slot successor promoted to primary owner",
    )
    expect_failure(
        successor_as_primary_owner,
        "slot successor promoted to primary owner",
        "slot primary behavior owner mismatch",
    )
    exclusive_schedule_overlap = mutate(
        text,
        "| 47 | V05 |",
        "| 47 | V05, V08 |",
        "exclusive schedule overlap",
    )
    expect_failure(
        exclusive_schedule_overlap,
        "exclusive schedule overlap",
        "exclusive resource node overlaps a schedule tick",
    )
    stale_schedule = re.sub(
        r"- \d+个节点、\d+条直接边、root=`F00`、唯一sink=`V03`",
        "- 1个节点、1条直接边、root=`F00`、唯一sink=`V03`",
        text,
        count=1,
    )
    if stale_schedule == text:
        raise PlanError("self-test mutation made no change: stale schedule")
    expect_failure(stale_schedule, "stale schedule", "statistics are stale")
    decision_row = re.search(r"^\| D1 \|.*$", text, re.MULTILINE)
    if decision_row is None:
        raise PlanError("self-test could not locate D1")
    duplicate = text[: decision_row.end()] + "\n" + decision_row.group(0) + text[decision_row.end() :]
    expect_failure(duplicate, "duplicate decision", "duplicate decision row")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("plan", type=Path)
    parser.add_argument("--self-test", action="store_true")
    arguments = parser.parse_args()
    try:
        text = arguments.plan.read_text(encoding="utf-8")
        (
            nodes,
            edges,
            subprojects,
            slots,
            shared_paths,
            behavior_owners,
            parameterized_families,
            generated_grants,
            digest,
        ) = validate_text(text)
        if arguments.self_test:
            run_self_test(text)
    except (OSError, PlanError) as error:
        print(f"plan validation failed: {error}", file=sys.stderr)
        return 1
    suffix = " self-test=passed" if arguments.self_test else ""
    print(
        "plan validation passed: "
        f"nodes={nodes} edges={edges} subprojects={subprojects} "
        f"slots={slots} shared_paths={shared_paths} "
        f"behavior_owners={behavior_owners} "
        f"parameterized_families={parameterized_families} "
        "artifact_fragment_families=1 gate_descriptor_families=1 "
        f"generated_grants={generated_grants} "
        f"sha256={digest}{suffix}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
