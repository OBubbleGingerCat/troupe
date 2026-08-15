from __future__ import annotations

import json
import shutil
import subprocess
import sys
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
AUDITOR = REPO_ROOT / "scripts/audit_perfetto_schema.py"
SCHEMA_RELATIVE = Path("rust/crates/troupe-diagnostics-perfetto/schema")
SOURCE_RELATIVE = Path("rust/crates/troupe-diagnostics-perfetto/src/schema.rs")
CARGO_RELATIVE = Path("rust/crates/troupe-diagnostics-perfetto/Cargo.toml")


def _copy_audit_root(tmp_path: Path) -> Path:
    root = tmp_path / "repo"
    shutil.copytree(REPO_ROOT / SCHEMA_RELATIVE, root / SCHEMA_RELATIVE)
    (root / SOURCE_RELATIVE.parent).mkdir(parents=True)
    shutil.copy2(REPO_ROOT / SOURCE_RELATIVE, root / SOURCE_RELATIVE)
    shutil.copy2(REPO_ROOT / CARGO_RELATIVE, root / CARGO_RELATIVE)
    return root


def _run(root: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(AUDITOR), "--offline", "--root", str(root)],
        check=False,
        capture_output=True,
        text=True,
    )


def _manifest(root: Path) -> tuple[Path, dict[str, object]]:
    path = root / SCHEMA_RELATIVE / "used-fields.json"
    return path, json.loads(path.read_text(encoding="utf-8"))


def _write_manifest(path: Path, manifest: dict[str, object]) -> None:
    path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")


def test_offline_audit_accepts_closed_subset_with_missing_unselected_imports(tmp_path: Path) -> None:
    root = _copy_audit_root(tmp_path)
    track_event = (
        root
        / SCHEMA_RELATIVE
        / "upstream/protos/perfetto/trace/track_event/track_event.proto"
    ).read_text(encoding="utf-8")
    assert 'import "protos/perfetto/trace/track_event/log_message.proto";' in track_event
    assert not (
        root
        / SCHEMA_RELATIVE
        / "upstream/protos/perfetto/trace/track_event/log_message.proto"
    ).exists()

    result = _run(root)

    assert result.returncode == 0, result.stderr
    assert "da1d152cff27890903d158fe96751de3aab883cc" in result.stdout


def test_snapshot_content_drift_fails(tmp_path: Path) -> None:
    root = _copy_audit_root(tmp_path)
    trace = root / SCHEMA_RELATIVE / "upstream/protos/perfetto/trace/trace.proto"
    trace.write_text(trace.read_text(encoding="utf-8") + "\n", encoding="utf-8")

    result = _run(root)

    assert result.returncode == 1
    assert "snapshot hash drift" in result.stderr


def test_hash_manifest_drift_fails_even_when_snapshot_is_unchanged(tmp_path: Path) -> None:
    root = _copy_audit_root(tmp_path)
    sums = root / SCHEMA_RELATIVE / "SHA256SUMS"
    sums.write_text(sums.read_text(encoding="ascii").replace("9a682a56", "0a682a56", 1), encoding="ascii")

    result = _run(root)

    assert result.returncode == 1
    assert "differs from the pinned" in result.stderr


def test_used_field_number_drift_fails(tmp_path: Path) -> None:
    root = _copy_audit_root(tmp_path)
    path, manifest = _manifest(root)
    definitions = manifest["definitions"]
    assert isinstance(definitions, list)
    trace = next(item for item in definitions if item["full_name"] == "perfetto.protos.Trace")
    trace["fields"][0]["number"] = 2
    _write_manifest(path, manifest)

    result = _run(root)

    assert result.returncode == 1
    assert "field number drift" in result.stderr


def test_used_type_dependency_drift_fails(tmp_path: Path) -> None:
    root = _copy_audit_root(tmp_path)
    path, manifest = _manifest(root)
    definitions = manifest["definitions"]
    assert isinstance(definitions, list)
    event = next(item for item in definitions if item["full_name"] == "perfetto.protos.TrackEvent")
    debug = next(field for field in event["fields"] if field["name"] == "debug_annotations")
    debug["type"] = "perfetto.protos.MissingAnnotation"
    _write_manifest(path, manifest)

    result = _run(root)

    assert result.returncode == 1
    assert "field type drift" in result.stderr


def test_enum_value_drift_fails(tmp_path: Path) -> None:
    root = _copy_audit_root(tmp_path)
    path, manifest = _manifest(root)
    definitions = manifest["definitions"]
    assert isinstance(definitions, list)
    clock = next(item for item in definitions if item["full_name"] == "perfetto.protos.BuiltinClock")
    trace_file = next(value for value in clock["values"] if value["name"] == "BUILTIN_CLOCK_TRACE_FILE")
    trace_file["number"] = 12
    _write_manifest(path, manifest)

    result = _run(root)

    assert result.returncode == 1
    assert "enum value drift" in result.stderr


def test_removed_used_definition_breaks_recursive_closure(tmp_path: Path) -> None:
    root = _copy_audit_root(tmp_path)
    path, manifest = _manifest(root)
    definitions = manifest["definitions"]
    assert isinstance(definitions, list)
    manifest["definitions"] = [
        item for item in definitions if item["full_name"] != "perfetto.protos.DebugAnnotation"
    ]
    _write_manifest(path, manifest)

    result = _run(root)

    assert result.returncode == 1
    assert "field type drift" in result.stderr or "unclosed used type dependency" in result.stderr


def test_orphan_snapshot_file_fails(tmp_path: Path) -> None:
    root = _copy_audit_root(tmp_path)
    orphan = root / SCHEMA_RELATIVE / "upstream/orphan.proto"
    orphan.write_text('syntax = "proto2";\n', encoding="utf-8")

    result = _run(root)

    assert result.returncode == 1
    assert "orphan file" in result.stderr


def test_rust_wire_tag_drift_fails(tmp_path: Path) -> None:
    root = _copy_audit_root(tmp_path)
    source = root / SOURCE_RELATIVE
    text = source.read_text(encoding="utf-8")
    text = text.replace('#[prost(uint64, optional, tag = "8")]', '#[prost(uint64, optional, tag = "7")]', 1)
    source.write_text(text, encoding="utf-8")

    result = _run(root)

    assert result.returncode == 1
    assert "Rust tag drift" in result.stderr


def test_provenance_commit_drift_fails(tmp_path: Path) -> None:
    root = _copy_audit_root(tmp_path)
    provenance = root / SCHEMA_RELATIVE / "PROVENANCE.md"
    provenance.write_text(
        provenance.read_text(encoding="utf-8").replace(
            "da1d152cff27890903d158fe96751de3aab883cc",
            "0a1d152cff27890903d158fe96751de3aab883cc",
        ),
        encoding="utf-8",
    )

    result = _run(root)

    assert result.returncode == 1
    assert "pinned commit" in result.stderr


def test_perfetto_schema_path_has_no_build_tool_dependency(tmp_path: Path) -> None:
    root = _copy_audit_root(tmp_path)
    cargo = root / CARGO_RELATIVE
    cargo.write_text(cargo.read_text(encoding="utf-8") + '\nprost-build = "0.14"\n', encoding="utf-8")

    result = _run(root)

    assert result.returncode == 1
    assert "forbidden" in result.stderr or "directly pin" in result.stderr
