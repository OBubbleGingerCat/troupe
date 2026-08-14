from __future__ import annotations

import base64
import hashlib
import importlib.util
import json
import re
import shutil
import subprocess
import sys
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest


ROOT = Path(__file__).resolve().parents[2]
SUPPORT = ROOT / "tests" / "support"
sys.path.insert(0, str(SUPPORT))

from artifact_layout import (  # noqa: E402
    ArtifactLayoutError,
    expected_package_members,
    expected_rust_sources,
    load_artifact_layout,
    load_gate_descriptors,
    validate_index_against_plan,
    validate_repository_artifacts,
)


ARTIFACT_FIELDS = ("state", "introduced", "modified", "removed", "generated")
GATE_FIELDS = (
    "state",
    "argv",
    "env",
    "maturin_features",
    "cache_requirements",
    "exclusive_resources",
)
PLANNING_BUNDLE_PATHS = (
    "docs/design/actor-agent-session.md",
    "docs/design/production-diagnostics.md",
    "docs/plan/production-diagnostics-implementation-plan.md",
    "docs/plan/verify_production_diagnostics_plan.py",
    "docs/plan/production-diagnostics-plan-review-record.md",
)


def _copy_contract(tmp_path: Path) -> Path:
    repository = tmp_path / "repository"
    artifact_target = repository / "tests/fixtures/artifact_layout"
    gate_target = repository / "tests/fixtures/diagnostic_node_gates"
    artifact_target.parent.mkdir(parents=True)
    shutil.copytree(ROOT / "tests/fixtures/artifact_layout", artifact_target)
    shutil.copytree(ROOT / "tests/fixtures/diagnostic_node_gates", gate_target)
    return repository


def _json(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    assert isinstance(value, dict)
    return value


def _write(path: Path, value: dict[str, Any]) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def _artifact(repository: Path, node_id: str) -> Path:
    return repository / f"tests/fixtures/artifact_layout/nodes/{node_id}.json"


def _gate(repository: Path, node_id: str) -> Path:
    return repository / f"tests/fixtures/diagnostic_node_gates/{node_id}.json"


def _ownership_module() -> Any:
    path = ROOT / "scripts/audit_diagnostic_ownership.py"
    spec = importlib.util.spec_from_file_location("f00_ownership_audit", path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def _ownership_ledger(repository: Path) -> Path:
    return repository / "tests/fixtures/artifact_layout/ownership-ledger.json"


def _accepted_plan() -> Path:
    return ROOT / "docs/plan/production-diagnostics-implementation-plan.md"


def _mutated_plan(
    tmp_path: Path,
    old: str,
    new: str,
    *,
    node_id: str | None = None,
) -> Path:
    text = _accepted_plan().read_text(encoding="utf-8")
    if node_id is None:
        assert old in text
        text = text.replace(old, new, 1)
    else:
        marker = re.compile(
            rf"(^#### {re.escape(node_id)} - .*?)(?=^#### [A-Z][0-9]{{2}} - |\Z)",
            re.MULTILINE | re.DOTALL,
        )
        match = marker.search(text)
        assert match is not None and old in match.group(1)
        replacement = match.group(1).replace(old, new, 1)
        text = text[: match.start()] + replacement + text[match.end() :]
    path = tmp_path / "plan.md"
    path.write_text(text, encoding="utf-8")
    return path


def _copy_planning_bundle(tmp_path: Path) -> Path:
    repository = tmp_path / "planning-repository"
    for relative in PLANNING_BUNDLE_PATHS:
        target = repository / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / relative, target)
    _initialize_git(repository)
    return repository


def _generated_fixture(
    tmp_path: Path,
    audit: Any,
) -> tuple[Path, Any, dict[str, Any]]:
    repository = tmp_path / "generated-repository"
    projection = audit.project_plan(_accepted_plan())
    grant = projection.generated_grants[0]
    build_hash = "1" * 64
    files: list[dict[str, str]] = []
    for kind in ("js", "css"):
        for encoding in ("raw", "gz", "br"):
            relative = (
                grant.exact_parent
                + f"diagnostics-{build_hash}.{kind}.{encoding}"
            )
            path = repository / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            data = f"{kind}:{encoding}\n".encode()
            path.write_bytes(data)
            files.append(
                {
                    "path": relative,
                    "sha256": hashlib.sha256(data).hexdigest(),
                }
            )
    manifest = {"files": files}
    manifest_path = repository / grant.manifest
    manifest_path.parent.mkdir(parents=True, exist_ok=True)
    _write(manifest_path, manifest)
    return repository, grant, manifest


def _git(repository: Path, *arguments: str) -> str:
    return subprocess.run(
        ["git", *arguments],
        cwd=repository,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()


def _initialize_git(repository: Path) -> str:
    _git(repository, "init", "-q")
    _git(repository, "config", "user.name", "F00 Test")
    _git(repository, "config", "user.email", "f00@example.invalid")
    _git(repository, "add", ".")
    _git(repository, "commit", "-qm", "base")
    return _git(repository, "rev-parse", "HEAD")


def _plan_node(repository: Path, node_id: str) -> None:
    _write(
        _artifact(repository, node_id),
        {
            "state": "planned",
            "introduced": [],
            "modified": [],
            "removed": [],
            "generated": [],
        },
    )
    _write(
        _gate(repository, node_id),
        {
            "state": "planned",
            "argv": [],
            "env": {},
            "maturin_features": [],
            "cache_requirements": [],
            "exclusive_resources": [],
        },
    )


def _realize_f01(repository: Path, introduced: list[str]) -> None:
    _write(
        _artifact(repository, "F01"),
        {
            "state": "realized",
            "introduced": introduced,
            "modified": [],
            "removed": [],
            "generated": [],
        },
    )
    _write(
        _gate(repository, "F01"),
        {
            "state": "realized",
            "argv": [["pytest", "-q"]],
            "env": {},
            "maturin_features": [],
            "cache_requirements": [],
            "exclusive_resources": [],
        },
    )


def test_index_is_byte_exact_with_the_accepted_plan() -> None:
    validate_index_against_plan(
        ROOT,
        ROOT / "docs/plan/production-diagnostics-implementation-plan.md",
    )


def test_artifact_and_gate_lifecycle_states_advance_together() -> None:
    layout = load_artifact_layout(ROOT)
    gates = load_gate_descriptors(ROOT)

    realized_fragments = {
        node_id for node_id, fragment in layout.fragments.items() if fragment.state == "realized"
    }
    realized_gates = {node_id for node_id, gate in gates.items() if gate.state == "realized"}

    assert "F00" in realized_fragments
    assert realized_fragments == realized_gates


def test_realized_fragments_extend_baseline_inventories_without_rewriting_base(
    tmp_path: Path,
) -> None:
    repository = _copy_contract(tmp_path)
    _plan_node(repository, "F01")
    path = _artifact(repository, "F01")
    fragment = _json(path)
    fragment["state"] = "realized"
    fragment["introduced"] = [
        "rust/crates/new/src/lib.rs",
        "src/troupe/diagnostics.py",
        "src/troupe/diagnostics.pyi",
    ]
    fragment["modified"] = ["rust/Cargo.toml", "src/troupe/__init__.py"]
    _write(path, fragment)
    layout = load_artifact_layout(repository)

    assert "crates/new/src/lib.rs" in expected_rust_sources(layout)
    assert "diagnostics.py" in expected_package_members(layout, ".py")
    assert "diagnostics.pyi" in expected_package_members(layout, ".pyi")
    assert layout.is_changed_after_base("rust/Cargo.toml")
    assert layout.is_changed_after_base("src/troupe/__init__.py")


def test_artifact_union_never_reads_gate_descriptor_contents(tmp_path: Path) -> None:
    repository = _copy_contract(tmp_path)
    gate = _json(_gate(repository, "F00"))
    gate["introduced"] = ["tests/should-never-enter-the-union.py"]
    _write(_gate(repository, "F00"), gate)

    layout = load_artifact_layout(repository)
    assert "tests/should-never-enter-the-union.py" not in layout.paths
    assert not any(
        path.startswith("tests/fixtures/diagnostic_node_gates/")
        for path in layout.paths
    )
    with pytest.raises(ArtifactLayoutError, match="fields are not closed"):
        load_gate_descriptors(repository)


@pytest.mark.parametrize("kind", ["artifact", "gate"])
@pytest.mark.parametrize("change", ["missing", "extra", "mixed"])
def test_parameterized_schemas_are_closed_field_by_field(
    tmp_path: Path,
    kind: str,
    change: str,
) -> None:
    repository = _copy_contract(tmp_path)
    path = _artifact(repository, "F01") if kind == "artifact" else _gate(repository, "F01")
    value = _json(path)
    own_field = "introduced" if kind == "artifact" else "argv"
    foreign_field = "argv" if kind == "artifact" else "introduced"
    if change == "missing":
        value.pop(own_field)
    elif change == "extra":
        value["unexpected"] = []
    else:
        value[foreign_field] = []
    _write(path, value)

    loader: Callable[[Path], object] = (
        load_artifact_layout if kind == "artifact" else load_gate_descriptors
    )
    with pytest.raises(ArtifactLayoutError, match="fields are not closed"):
        loader(repository)


@pytest.mark.parametrize(
    ("kind", "field"),
    [
        *(("artifact", field) for field in ARTIFACT_FIELDS),
        *(("gate", field) for field in GATE_FIELDS),
    ],
)
def test_every_schema_field_is_required(
    tmp_path: Path,
    kind: str,
    field: str,
) -> None:
    repository = _copy_contract(tmp_path)
    path = _artifact(repository, "F01") if kind == "artifact" else _gate(repository, "F01")
    value = _json(path)
    value.pop(field)
    _write(path, value)

    loader = load_artifact_layout if kind == "artifact" else load_gate_descriptors
    with pytest.raises(ArtifactLayoutError, match="fields are not closed"):
        loader(repository)


@pytest.mark.parametrize("kind", ["artifact", "gate"])
def test_illegal_lifecycle_state_is_rejected(tmp_path: Path, kind: str) -> None:
    repository = _copy_contract(tmp_path)
    path = _artifact(repository, "F01") if kind == "artifact" else _gate(repository, "F01")
    value = _json(path)
    value["state"] = "done"
    _write(path, value)

    loader = load_artifact_layout if kind == "artifact" else load_gate_descriptors
    with pytest.raises(ArtifactLayoutError, match="invalid state"):
        loader(repository)


def test_planned_artifact_fragment_must_be_empty(tmp_path: Path) -> None:
    repository = _copy_contract(tmp_path)
    _plan_node(repository, "F01")
    path = _artifact(repository, "F01")
    value = _json(path)
    value["introduced"] = ["tests/future.py"]
    _write(path, value)

    with pytest.raises(ArtifactLayoutError, match="planned artifact fragment F01 must be empty"):
        load_artifact_layout(repository)


def test_planned_gate_descriptor_must_be_empty(tmp_path: Path) -> None:
    repository = _copy_contract(tmp_path)
    _plan_node(repository, "F01")
    path = _gate(repository, "F01")
    value = _json(path)
    value["argv"] = [["pytest", "-q"]]
    _write(path, value)

    with pytest.raises(ArtifactLayoutError, match="planned gate descriptor F01 must be empty"):
        load_gate_descriptors(repository)


@pytest.mark.parametrize(
    ("field", "value"),
    [
        ("introduced", ["tests/future.py"]),
        ("modified", ["tests/existing.py"]),
        ("removed", [{"path": "tests/old.py", "sha256": "0" * 64}]),
        ("generated", ["G01"]),
    ],
)
def test_every_planned_artifact_category_must_be_empty(
    tmp_path: Path,
    field: str,
    value: object,
) -> None:
    repository = _copy_contract(tmp_path)
    _plan_node(repository, "F01")
    path = _artifact(repository, "F01")
    fragment = _json(path)
    fragment[field] = value
    _write(path, fragment)

    with pytest.raises(ArtifactLayoutError, match="planned artifact fragment F01 must be empty"):
        load_artifact_layout(repository)


@pytest.mark.parametrize(
    ("node_id", "field", "value"),
    [
        ("F01", "argv", [["pytest", "-q"]]),
        ("F01", "env", {"TROUPE_GATE_TMP": "optional"}),
        ("F01", "maturin_features", ["agent-test-support"]),
        ("F01", "cache_requirements", ["npm"]),
        ("V05", "exclusive_resources", ["benchmark-host"]),
    ],
)
def test_every_planned_gate_field_must_be_empty(
    tmp_path: Path,
    node_id: str,
    field: str,
    value: object,
) -> None:
    repository = _copy_contract(tmp_path)
    _plan_node(repository, node_id)
    path = _gate(repository, node_id)
    gate = _json(path)
    gate[field] = value
    _write(path, gate)

    with pytest.raises(ArtifactLayoutError, match=f"planned gate descriptor {node_id} must be empty"):
        load_gate_descriptors(repository)


@pytest.mark.parametrize("kind", ["artifact", "gate"])
def test_realized_lifecycle_file_must_be_closed(tmp_path: Path, kind: str) -> None:
    repository = _copy_contract(tmp_path)
    _plan_node(repository, "F01")
    path = _artifact(repository, "F01") if kind == "artifact" else _gate(repository, "F01")
    value = _json(path)
    value["state"] = "realized"
    _write(path, value)

    loader = load_artifact_layout if kind == "artifact" else load_gate_descriptors
    with pytest.raises(ArtifactLayoutError, match="not closed"):
        loader(repository)


@pytest.mark.parametrize("family", ["artifact", "gate"])
@pytest.mark.parametrize(
    "change",
    ["missing", "extra-json", "extra-non-json", "extra-directory"],
)
def test_parameterized_file_sets_are_exact(
    tmp_path: Path,
    family: str,
    change: str,
) -> None:
    repository = _copy_contract(tmp_path)
    directory = (
        repository / "tests/fixtures/artifact_layout/nodes"
        if family == "artifact"
        else repository / "tests/fixtures/diagnostic_node_gates"
    )
    if change == "missing":
        (directory / "F01.json").unlink()
    elif change == "extra-json":
        (directory / "X99.json").write_text("{}\n", encoding="utf-8")
    elif change == "extra-non-json":
        (directory / "README.txt").write_text("unexpected\n", encoding="utf-8")
    else:
        (directory / "unexpected").mkdir()

    loader = load_artifact_layout if family == "artifact" else load_gate_descriptors
    with pytest.raises(ArtifactLayoutError, match="files are not exact"):
        loader(repository)


@pytest.mark.parametrize("change", ["duplicate", "category"])
def test_fragment_paths_are_unique_within_and_across_categories(
    tmp_path: Path,
    change: str,
) -> None:
    repository = _copy_contract(tmp_path)
    _plan_node(repository, "F01")
    path = _artifact(repository, "F01")
    value = _json(path)
    value["state"] = "realized"
    value["introduced"] = ["tests/future.py", "tests/future.py"] if change == "duplicate" else ["tests/future.py"]
    if change == "category":
        value["modified"] = ["tests/future.py"]
    _write(path, value)

    with pytest.raises(ArtifactLayoutError, match="duplicate|across categories"):
        load_artifact_layout(repository)


@pytest.mark.parametrize(
    ("path_value", "message"),
    [
        ("tests/*.py", "glob or subset"),
        (":(glob)tests/unit/*.py", "glob or subset"),
        ("../tests/unit/test_actor.py", "canonical repository path"),
        ("tests/./unit/test_actor.py", "canonical repository path"),
        ("tests//unit/test_actor.py", "canonical repository path"),
        ("unknown/file.py", "unknown repository root"),
    ],
)
def test_fragment_rejects_globs_subsets_and_noncanonical_paths(
    tmp_path: Path,
    path_value: str,
    message: str,
) -> None:
    repository = _copy_contract(tmp_path)
    _plan_node(repository, "F01")
    path = _artifact(repository, "F01")
    value = _json(path)
    value["state"] = "realized"
    value["introduced"] = [path_value]
    _write(path, value)

    with pytest.raises(ArtifactLayoutError, match=message):
        load_artifact_layout(repository)


def test_fragment_rejects_directory_authorization(tmp_path: Path) -> None:
    repository = _copy_contract(tmp_path)
    _plan_node(repository, "F01")
    (repository / "tests/unit").mkdir()
    path = _artifact(repository, "F01")
    value = _json(path)
    value["state"] = "realized"
    value["introduced"] = ["tests/unit"]
    _write(path, value)

    with pytest.raises(ArtifactLayoutError, match="not a directory"):
        load_artifact_layout(repository)


def test_fragment_rejects_ignore_rules(tmp_path: Path) -> None:
    repository = _copy_contract(tmp_path)
    path = _artifact(repository, "F01")
    value = _json(path)
    value["ignore"] = ["tests/generated.py"]
    _write(path, value)

    with pytest.raises(ArtifactLayoutError, match="fields are not closed"):
        load_artifact_layout(repository)


def test_removed_artifact_requires_its_preimage_hash(tmp_path: Path) -> None:
    repository = _copy_contract(tmp_path)
    _plan_node(repository, "F01")
    path = _artifact(repository, "F01")
    value = _json(path)
    value["state"] = "realized"
    value["removed"] = [{"path": "tests/old.py"}]
    _write(path, value)

    with pytest.raises(ArtifactLayoutError, match="fields are not closed"):
        load_artifact_layout(repository)


@pytest.mark.parametrize(
    ("field", "value", "message"),
    [
        ("env", {"SECRET_TOKEN": "required"}, "unknown env"),
        ("maturin_features", ["all-features"], "unknown maturin feature"),
        ("cache_requirements", ["home-cache"], "unknown cache requirement"),
        ("exclusive_resources", ["network"], "unknown exclusive resource"),
    ],
)
def test_gate_rejects_unknown_environment_feature_cache_and_resource(
    tmp_path: Path,
    field: str,
    value: object,
    message: str,
) -> None:
    repository = _copy_contract(tmp_path)
    _plan_node(repository, "F01")
    path = _gate(repository, "F01")
    gate = _json(path)
    gate["state"] = "realized"
    gate["argv"] = [["pytest", "-q"]]
    gate[field] = value
    _write(path, gate)

    with pytest.raises(ArtifactLayoutError, match=message):
        load_gate_descriptors(repository)


def test_gate_rejects_shell_command_strings(tmp_path: Path) -> None:
    repository = _copy_contract(tmp_path)
    _plan_node(repository, "F01")
    path = _gate(repository, "F01")
    gate = _json(path)
    gate["state"] = "realized"
    gate["argv"] = ["pytest -q"]
    _write(path, gate)

    with pytest.raises(ArtifactLayoutError, match="list of strings"):
        load_gate_descriptors(repository)


def test_gate_argv_may_repeat_a_literal_argument(tmp_path: Path) -> None:
    repository = _copy_contract(tmp_path)
    _plan_node(repository, "F01")
    path = _gate(repository, "F01")
    gate = _json(path)
    gate["state"] = "realized"
    gate["argv"] = [["pytest", "-q", "-q"]]
    _write(path, gate)

    descriptors = load_gate_descriptors(repository)
    assert descriptors["F01"].argv == (("pytest", "-q", "-q"),)


@pytest.mark.parametrize("change", ["delete", "add", "rewrite"])
def test_base_snapshot_rejects_deleted_added_or_rewritten_artifacts(
    tmp_path: Path,
    change: str,
) -> None:
    repository = _copy_contract(tmp_path)
    path = repository / "tests/fixtures/artifact_layout/base.json"
    base = _json(path)
    if change == "delete":
        base["rust_sources"].pop()
    elif change == "add":
        base["rust_sources"].append("src/not-a-real-source.rs")
    else:
        data = b"rewritten\n"
        base["package_files"]["__init__.py"] = {
            "base64": base64.b64encode(data).decode("ascii"),
            "sha256": hashlib.sha256(data).hexdigest(),
        }
    _write(path, base)

    layout = load_artifact_layout(repository)
    with pytest.raises(ArtifactLayoutError, match="inventory|rewritten"):
        validate_repository_artifacts(ROOT, layout)


def test_index_rejects_an_owner_unknown_to_the_plan(tmp_path: Path) -> None:
    repository = _copy_contract(tmp_path)
    index_path = repository / "tests/fixtures/artifact_layout/index.json"
    index = _json(index_path)
    first = index["nodes"][0]
    first["id"] = "X99"
    first["artifact"] = "tests/fixtures/artifact_layout/nodes/X99.json"
    first["gate"] = "tests/fixtures/diagnostic_node_gates/X99.json"
    _write(index_path, index)
    _artifact(repository, "F00").rename(_artifact(repository, "X99"))
    _gate(repository, "F00").rename(_gate(repository, "X99"))

    with pytest.raises(ArtifactLayoutError, match="differ from the accepted plan"):
        validate_index_against_plan(
            repository,
            ROOT / "docs/plan/production-diagnostics-implementation-plan.md",
        )


def test_bootstrap_ownership_audit_matches_lifecycle_and_declared_diff(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    repository = _copy_contract(tmp_path)
    _plan_node(repository, "F01")
    base = _initialize_git(repository)
    _realize_f01(repository, ["tests/new.py"])
    (repository / "tests/new.py").write_text("new\n", encoding="utf-8")
    _git(repository, "add", ".")
    _git(repository, "commit", "-qm", "realize F01")
    audit = _ownership_module()
    monkeypatch.setattr(audit, "ROOT", repository)

    audit.audit_node("F01", base)


def test_bootstrap_ownership_audit_rejects_undeclared_diff(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    repository = _copy_contract(tmp_path)
    _plan_node(repository, "F01")
    base = _initialize_git(repository)
    _realize_f01(repository, ["tests/new.py"])
    (repository / "tests/new.py").write_text("new\n", encoding="utf-8")
    (repository / "tests/undeclared.py").write_text("undeclared\n", encoding="utf-8")
    _git(repository, "add", ".")
    _git(repository, "commit", "-qm", "bad F01")
    audit = _ownership_module()
    monkeypatch.setattr(audit, "ROOT", repository)

    with pytest.raises(audit.OwnershipAuditError, match="diff is not exact"):
        audit.audit_node("F01", base)


def test_bootstrap_ownership_audit_checks_removed_preimage_hash(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    repository = _copy_contract(tmp_path)
    _plan_node(repository, "F01")
    old = repository / "tests/old.py"
    old.write_text("old\n", encoding="utf-8")
    base = _initialize_git(repository)
    _realize_f01(repository, [])
    fragment_path = _artifact(repository, "F01")
    fragment = _json(fragment_path)
    fragment["removed"] = [{"path": "tests/old.py", "sha256": "0" * 64}]
    _write(fragment_path, fragment)
    old.unlink()
    _git(repository, "add", "-A")
    _git(repository, "commit", "-qm", "remove with wrong hash")
    audit = _ownership_module()
    monkeypatch.setattr(audit, "ROOT", repository)

    with pytest.raises(audit.OwnershipAuditError, match="preimage hash differs"):
        audit.audit_node("F01", base)


def test_plan_only_ownership_ledger_is_bidirectionally_closed() -> None:
    audit = _ownership_module()

    audit.audit_plan(_accepted_plan(), repository_root=ROOT)


@pytest.mark.parametrize(
    ("change", "message"),
    [
        ("top-extra", "fields are not closed"),
        ("path-missing-field", "fields are not closed"),
        ("duplicate-path", "duplicate ownership path"),
        ("invalid-baseline", "baseline_state"),
        ("unknown-writer", "unknown writer"),
        ("duplicate-writer", "duplicate writer"),
        ("invalid-role", "invalid writer role"),
        ("missing-path", "ledger/projected path mismatch"),
        ("ghost-path", "ledger/projected path mismatch"),
        ("writer-order", "writer order"),
        ("grant-owner", "generated grant mismatch"),
        ("grant-cardinality", "generated grant mismatch"),
        ("extra-grant", "generated grant is duplicated"),
    ],
)
def test_ownership_ledger_rejects_schema_path_writer_and_grant_drift(
    tmp_path: Path,
    change: str,
    message: str,
) -> None:
    repository = _copy_contract(tmp_path)
    ledger_path = _ownership_ledger(repository)
    ledger = _json(ledger_path)
    paths = ledger["paths"]
    grants = ledger["generated_grants"]
    assert isinstance(paths, list) and isinstance(grants, list)

    if change == "top-extra":
        ledger["unexpected"] = []
    elif change == "path-missing-field":
        paths[0].pop("baseline_state")
    elif change == "duplicate-path":
        paths.append(dict(paths[0]))
    elif change == "invalid-baseline":
        paths[0]["baseline_state"] = "maybe"
    elif change == "unknown-writer":
        paths[0]["writers"][0]["node"] = "Z99"
    elif change == "duplicate-writer":
        paths[0]["writers"].append(dict(paths[0]["writers"][0]))
    elif change == "invalid-role":
        paths[0]["writers"][0]["role"] = "touch"
    elif change == "missing-path":
        paths.pop()
    elif change == "ghost-path":
        paths.append(
            {
                "path": "tests/ghost.py",
                "baseline_state": "planned",
                "writers": [{"node": "F02", "role": "create"}],
            }
        )
    elif change == "writer-order":
        entry = next(
            item
            for item in paths
            if item["path"] == "frontend/diagnostics/src/app.tsx"
        )
        entry["writers"].reverse()
    elif change == "grant-owner":
        grants[0]["owner"] = "W06"
    elif change == "grant-cardinality":
        grants[0]["cardinality"] = 5
    else:
        grants.append(dict(grants[0]))
    _write(ledger_path, ledger)

    audit = _ownership_module()
    if change in {
        "top-extra",
        "path-missing-field",
        "duplicate-path",
        "invalid-baseline",
        "unknown-writer",
        "duplicate-writer",
            "invalid-role",
            "extra-grant",
        }:
        with pytest.raises(audit.OwnershipAuditError, match=message):
            audit.load_ownership_ledger(repository)
    else:
        loaded = audit.load_ownership_ledger(repository)
        projection = audit.project_plan(_accepted_plan())
        with pytest.raises(audit.OwnershipAuditError, match=message):
            audit.validate_projection(projection, loaded)


@pytest.mark.parametrize(
    ("old", "new", "node_id", "message"),
    [
        (
            "`tests/fixtures/artifact_layout/ownership-ledger.json`",
            "`/tests/fixtures/artifact_layout/ownership-ledger.json`",
            "F02",
            "canonical repository-relative",
        ),
        (
            "`tests/fixtures/artifact_layout/ownership-ledger.json`",
            "`tests/fixtures/*/ownership-ledger.json`",
            "F02",
            "glob",
        ),
        (
            "`tests/fixtures/artifact_layout/ownership-ledger.json`",
            "`unknown/ownership-ledger.json`",
            "F02",
            "unknown repository root",
        ),
        (
            "`tests/fixtures/artifact_layout/ownership-ledger.json`",
            "`tests/fixtures/artifact_layout/ownership-ledger.json`; `tests/fixtures/artifact_layout/ownership-ledger.json`",
            "F02",
            "repeats an expanded path",
        ),
        (
            "`frontend/diagnostics/tests/unit/bundle-contract.test.ts`",
            "`frontend/diagnostics/tests/unit/bundle-contract.test.tsx`",
            "W06",
            "W06 exact artifact set drift",
        ),
        (
            "`frontend/diagnostics/tests/e2e/visual/{diagnostics.spec.ts,viewports.ts,pixel-oracle.json,screenshot-manifest.json}`",
            "`frontend/diagnostics/tests/e2e/visual/{diagnostics.spec.ts,viewports.ts,pixel-oracle.txt,screenshot-manifest.json}`",
            "V00",
            "V00 exact artifact set drift",
        ),
    ],
)
def test_plan_projection_rejects_noncanonical_duplicate_and_exact_artifact_drift(
    tmp_path: Path,
    old: str,
    new: str,
    node_id: str,
    message: str,
) -> None:
    audit = _ownership_module()
    path = _mutated_plan(tmp_path, old, new, node_id=node_id)

    with pytest.raises(audit.OwnershipAuditError, match=message):
        audit.project_plan(path)


@pytest.mark.parametrize(
    ("path_value", "message"),
    [
        ("tests/ownerless-f02.py", "no baseline/artifact owner"),
        (
            "frontend/diagnostics/src/timeline/layout.ts",
            "owned only by non-ancestors",
        ),
    ],
)
def test_plan_projection_rejects_ownerless_and_future_only_gate_paths(
    tmp_path: Path,
    path_value: str,
    message: str,
) -> None:
    old = (
        "`python scripts/audit_diagnostic_ownership.py --plan-only --plan "
        "docs/plan/production-diagnostics-implementation-plan.md`"
    )
    new = old[:-1] + f" {path_value}`"
    audit = _ownership_module()
    path = _mutated_plan(tmp_path, old, new, node_id="F02")

    with pytest.raises(audit.OwnershipAuditError, match=message):
        audit.project_plan(path)


@pytest.mark.parametrize(
    ("old", "new", "message"),
    [
        (
            "| `tests/fixtures/diagnostic_node_gates/<node-id>.json` | gate-descriptor |",
            "| `tests/fixtures/diagnostic_node_gates/<node-id>.json` | artifact-fragment |",
            "fragment family contract mismatch",
        ),
        (
            "| G01 | `rust/crates/troupe-diagnostics-runtime/assets/generated/manifest.json` | W14 | `files[].path` | `rust/crates/troupe-diagnostics-runtime/assets/generated/` | `diagnostics-<sha256>.{js,css}.{raw,gz,br}` | 6 |",
            "| G01 | `rust/crates/troupe-diagnostics-runtime/assets/generated/manifest.json` | W14 | `files[].path` | `rust/crates/troupe-diagnostics-runtime/assets/generated/` | `diagnostics-<sha256>.{js,css}.{raw,gz,br}` | 5 |",
            "generated grant contract mismatch",
        ),
        (
            "| `scripts/audit_diagnostic_ownership.py` | F00 -> F02 |",
            "| `scripts/audit_diagnostic_ownership.py` | F00 -> W00 |",
            "shared writer order|contract/shared writer mismatch|bidirectionally grounded",
        ),
    ],
)
def test_plan_projection_rejects_family_grant_and_shared_table_drift(
    tmp_path: Path,
    old: str,
    new: str,
    message: str,
) -> None:
    audit = _ownership_module()
    path = _mutated_plan(tmp_path, old, new)

    with pytest.raises(audit.OwnershipAuditError, match=message):
        audit.project_plan(path)


@pytest.mark.parametrize(
    ("old", "new", "message"),
    [
        (
            "| `tests/fixtures/diagnostic_node_gates/<node-id>.json` | gate-descriptor | `state,argv,env,maturin_features,cache_requirements,exclusive_resources` | F00 | `<node-id>` | index-exact |",
            "| `tests/fixtures/diagnostic_node_gates/<node-id>.json` | gate-descriptor | `state,argv,env,maturin_features,cache_requirements,exclusive_resources` | F00 | `<node-id>` | index-exact |\n"
            "| `tests/fixtures/third/<node-id>.json` | gate-descriptor | `state,argv,env,maturin_features,cache_requirements,exclusive_resources` | F00 | `<node-id>` | index-exact |",
            "fragment family contract mismatch",
        ),
        (
            "| `tests/unit/test_release_script.py` | F00 -> V01 | F00等价迁移，V01只增加diagnostics dispatch cases |\n",
            "",
            "shared writer inventory|shared writer row|contract artifact",
        ),
        (
            "| `tests/unit/test_release_script.py` | F00 -> V01 | F00等价迁移，V01只增加diagnostics dispatch cases |",
            "| `tests/unit/test_release_script.py` | F00 -> V01 | F00等价迁移，V01只增加diagnostics dispatch cases |\n"
            "| `tests/ghost-shared.py` | F00 | ghost |",
            "shared writer row is not bidirectionally grounded",
        ),
        (
            "| `scripts/audit_diagnostic_ownership.py` | F00 -> F02 | F00建立bootstrap检查，F02填充全路径ledger/diff审计 |",
            "| `scripts/audit_diagnostic_ownership.py` | F00 -> Z99 | F00建立bootstrap检查，F02填充全路径ledger/diff审计 |",
            "unknown writers",
        ),
        (
            "| `scripts/audit_diagnostic_ownership.py` | F00 -> F02 | F00建立bootstrap检查，F02填充全路径ledger/diff审计 |",
            "| `scripts/audit_diagnostic_ownership.py` | F02 -> F00 | F00建立bootstrap检查，F02填充全路径ledger/diff审计 |",
            "shared writer order is not reachable",
        ),
        (
            "| G01 | `rust/crates/troupe-diagnostics-runtime/assets/generated/manifest.json` | W14 | `files[].path` |",
            "| G01 | `rust/crates/troupe-diagnostics-runtime/assets/generated/manifest.json` | W14 | `members[].path` |",
            "generated grant contract mismatch",
        ),
        (
            "`diagnostics-<sha256>.{js,css}.{raw,gz,br}` | 6 |",
            "`diagnostics-<sha256>.{js,css}.{raw,gz}` | 6 |",
            "generated grant contract mismatch",
        ),
    ],
)
def test_plan_projection_rejects_closed_machine_table_negative_matrix(
    tmp_path: Path,
    old: str,
    new: str,
    message: str,
) -> None:
    audit = _ownership_module()
    path = _mutated_plan(tmp_path, old, new)

    with pytest.raises(audit.OwnershipAuditError, match=message):
        audit.project_plan(path)


def test_plan_projection_rejects_hidden_multiwriter_join(tmp_path: Path) -> None:
    old = "`tests/unit/test_diagnostic_worktree_gate.py`"
    new = old + "; `tests/fixtures/artifact_layout/ownership-ledger.json`"
    audit = _ownership_module()
    path = _mutated_plan(tmp_path, old, new, node_id="F03")

    with pytest.raises(
        audit.OwnershipAuditError,
        match="multi-writer contract artifact is missing from shared inventory",
    ):
        audit.project_plan(path)


def test_plan_projection_rejects_node_ownership_of_planning_bundle(tmp_path: Path) -> None:
    audit = _ownership_module()
    path = _mutated_plan(
        tmp_path,
        "`tests/fixtures/artifact_layout/ownership-ledger.json`",
        "`docs/design/actor-agent-session.md`",
        node_id="F02",
    )

    with pytest.raises(audit.OwnershipAuditError, match="planning bundle paths have node owners"):
        audit.project_plan(path)


@pytest.mark.parametrize("change", ["untracked", "hash"])
def test_planning_bundle_must_be_tracked_and_match_frozen_hashes(
    tmp_path: Path,
    change: str,
) -> None:
    repository = _copy_planning_bundle(tmp_path)
    if change == "untracked":
        _git(
            repository,
            "rm",
            "--cached",
            "docs/design/actor-agent-session.md",
        )
    else:
        path = repository / "docs/design/actor-agent-session.md"
        path.write_text(path.read_text(encoding="utf-8") + "\n", encoding="utf-8")
    audit = _ownership_module()
    projection = audit.project_plan(_accepted_plan())

    with pytest.raises(
        audit.OwnershipAuditError,
        match="not tracked exactly|bundle hash differs",
    ):
        audit._validate_planning_bundle(repository, projection)


def test_generated_grant_expands_only_the_six_manifest_bound_members(
    tmp_path: Path,
) -> None:
    audit = _ownership_module()
    repository, grant, manifest = _generated_fixture(tmp_path, audit)

    members = audit._generated_members(repository, grant)

    assert members == tuple(sorted(item["path"] for item in manifest["files"]))


@pytest.mark.parametrize(
    ("change", "message"),
    [
        ("cardinality", "cardinality differs"),
        ("field", r"files\[0\]\.path is malformed"),
        ("traversal", "canonical repository path"),
        ("symlink", "contains a symlink"),
        ("parent-symlink", "contains a symlink"),
        ("sha", "member SHA differs"),
        ("mixed-build", "members are not exact"),
    ],
)
def test_generated_grant_rejects_manifest_member_expansion_drift(
    tmp_path: Path,
    change: str,
    message: str,
) -> None:
    audit = _ownership_module()
    repository, grant, manifest = _generated_fixture(tmp_path, audit)
    files = manifest["files"]
    assert isinstance(files, list)
    if change == "cardinality":
        files.pop()
    elif change == "field":
        files[0]["member_path"] = files[0].pop("path")
    elif change == "traversal":
        files[0]["path"] = "../outside.js.raw"
    elif change == "symlink":
        member = repository / files[0]["path"]
        member.unlink()
        target = repository / "target"
        target.write_bytes(b"target\n")
        member.symlink_to(target)
    elif change == "parent-symlink":
        generated = repository / grant.exact_parent.rstrip("/")
        real_generated = generated.with_name("real-generated")
        generated.rename(real_generated)
        generated.symlink_to(real_generated, target_is_directory=True)
    elif change == "sha":
        files[0]["sha256"] = "0" * 64
    else:
        old_path = repository / files[0]["path"]
        files[0]["path"] = files[0]["path"].replace("1" * 64, "2" * 64)
        new_path = repository / files[0]["path"]
        old_path.rename(new_path)
    _write(repository / grant.manifest, manifest)

    with pytest.raises(audit.OwnershipAuditError, match=message):
        audit._generated_members(repository, grant)


@pytest.mark.parametrize(
    ("path_value", "message"),
    [
        ("tests/ownerless-realized.py", "no baseline/artifact owner"),
        (
            "frontend/diagnostics/src/timeline/layout.ts",
            "owned only by non-ancestors",
        ),
    ],
)
def test_realized_gate_descriptor_rejects_ownerless_and_future_paths(
    tmp_path: Path,
    path_value: str,
    message: str,
) -> None:
    repository = _copy_contract(tmp_path)
    gate_path = _gate(repository, "F02")
    gate = _json(gate_path)
    gate["state"] = "realized"
    gate["argv"] = [["pytest", "-q", path_value]]
    _write(gate_path, gate)
    fragment = _json(_artifact(repository, "F02"))
    fragment["state"] = "realized"
    fragment["introduced"] = [
        "tests/fixtures/artifact_layout/ownership-ledger.json"
    ]
    fragment["modified"] = [
        "scripts/audit_diagnostic_ownership.py",
        "tests/unit/test_diagnostic_ownership.py",
    ]
    _write(_artifact(repository, "F02"), fragment)
    audit = _ownership_module()
    projection = audit.project_plan(_accepted_plan())
    ledger = audit.load_ownership_ledger(repository)

    with pytest.raises(audit.OwnershipAuditError, match=message):
        audit.validate_lifecycle(projection, ledger, repository)


def test_realized_gate_descriptor_must_match_projected_structured_argv(
    tmp_path: Path,
) -> None:
    repository = _copy_contract(tmp_path)
    gate = _json(_gate(repository, "W00"))
    gate["argv"] = [["pytest", "-q", "tests/unit/test_artifact_layout.py"]]
    _write(_gate(repository, "W00"), gate)
    audit = _ownership_module()
    projection = audit.project_plan(_accepted_plan())
    ledger = audit.load_ownership_ledger(repository)

    with pytest.raises(audit.OwnershipAuditError, match="descriptor argv differs"):
        audit.validate_lifecycle(projection, ledger, repository)


@pytest.mark.parametrize("change", ["state", "category", "missing"])
def test_realized_fragment_must_match_gate_state_ledger_role_and_writer_union(
    tmp_path: Path,
    change: str,
) -> None:
    repository = _copy_contract(tmp_path)
    if change == "state":
        _plan_node(repository, "W00")
        gate = _json(_gate(repository, "W00"))
        gate["state"] = "realized"
        gate["argv"] = [["pytest", "-q"]]
        _write(_gate(repository, "W00"), gate)
    else:
        fragment = _json(_artifact(repository, "W00"))
        if change == "category":
            moved = fragment["introduced"].pop()
            fragment["modified"].append(moved)
        else:
            fragment["introduced"].pop()
        _write(_artifact(repository, "W00"), fragment)
    audit = _ownership_module()
    projection = audit.project_plan(_accepted_plan())
    ledger = audit.load_ownership_ledger(repository)

    with pytest.raises(
        audit.OwnershipAuditError,
        match="lifecycle states differ|role/category mismatch|artifact writer union",
    ):
        audit.validate_lifecycle(projection, ledger, repository)
