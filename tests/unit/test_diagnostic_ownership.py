from __future__ import annotations

import base64
import hashlib
import importlib.util
import json
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
    spec.loader.exec_module(module)
    return module


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


def _realize_f01(repository: Path, introduced: list[str]) -> None:
    fragment_path = _artifact(repository, "F01")
    fragment = _json(fragment_path)
    fragment["state"] = "realized"
    fragment["introduced"] = introduced
    _write(fragment_path, fragment)
    gate_path = _gate(repository, "F01")
    gate = _json(gate_path)
    gate["state"] = "realized"
    gate["argv"] = [["pytest", "-q"]]
    _write(gate_path, gate)


def test_index_is_byte_exact_with_the_accepted_plan() -> None:
    validate_index_against_plan(
        ROOT,
        ROOT / "docs/plan/production-diagnostics-implementation-plan.md",
    )


def test_only_f00_lifecycle_files_are_realized() -> None:
    layout = load_artifact_layout(ROOT)
    gates = load_gate_descriptors(ROOT)

    assert layout.fragments["F00"].state == "realized"
    assert gates["F00"].state == "realized"
    assert all(
        fragment.state == "planned" and not fragment.static_paths and not fragment.generated
        for node_id, fragment in layout.fragments.items()
        if node_id != "F00"
    )
    assert all(
        gate.state == "planned"
        and not gate.argv
        and not gate.env
        and not gate.maturin_features
        and not gate.cache_requirements
        and not gate.exclusive_resources
        for node_id, gate in gates.items()
        if node_id != "F00"
    )


def test_realized_fragments_extend_baseline_inventories_without_rewriting_base(
    tmp_path: Path,
) -> None:
    repository = _copy_contract(tmp_path)
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
    path = _artifact(repository, "F01")
    value = _json(path)
    value["introduced"] = ["tests/future.py"]
    _write(path, value)

    with pytest.raises(ArtifactLayoutError, match="planned artifact fragment F01 must be empty"):
        load_artifact_layout(repository)


def test_planned_gate_descriptor_must_be_empty(tmp_path: Path) -> None:
    repository = _copy_contract(tmp_path)
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
    path = _gate(repository, node_id)
    gate = _json(path)
    gate[field] = value
    _write(path, gate)

    with pytest.raises(ArtifactLayoutError, match=f"planned gate descriptor {node_id} must be empty"):
        load_gate_descriptors(repository)


@pytest.mark.parametrize("kind", ["artifact", "gate"])
def test_realized_lifecycle_file_must_be_closed(tmp_path: Path, kind: str) -> None:
    repository = _copy_contract(tmp_path)
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
    path = _artifact(repository, "F01")
    value = _json(path)
    value["state"] = "realized"
    value["introduced"] = [path_value]
    _write(path, value)

    with pytest.raises(ArtifactLayoutError, match=message):
        load_artifact_layout(repository)


def test_fragment_rejects_directory_authorization(tmp_path: Path) -> None:
    repository = _copy_contract(tmp_path)
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
    path = _gate(repository, "F01")
    gate = _json(path)
    gate["state"] = "realized"
    gate["argv"] = ["pytest -q"]
    _write(path, gate)

    with pytest.raises(ArtifactLayoutError, match="list of strings"):
        load_gate_descriptors(repository)


def test_gate_argv_may_repeat_a_literal_argument(tmp_path: Path) -> None:
    repository = _copy_contract(tmp_path)
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
