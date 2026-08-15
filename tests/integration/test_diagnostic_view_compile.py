from __future__ import annotations

import os
import subprocess
import sysconfig
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
COMPILER = ROOT / "rust/src/diagnostic_runtime/view_compile.rs"
LOADER = ROOT / "rust/src/application/loader/class.rs"
RECORDS = (
    ROOT
    / "rust/crates/troupe-diagnostics-runtime/src/store/view_records.rs"
)


def _cargo_test(package: str, test_filter: str) -> None:
    command = [
        "cargo",
        "test",
        "--locked",
        "--manifest-path",
        "rust/Cargo.toml",
        "--package",
        package,
        test_filter,
        "--",
        "--nocapture",
    ]
    environment = os.environ.copy()
    libdir = sysconfig.get_config_var("LIBDIR")
    if libdir:
        current = environment.get("LD_LIBRARY_PATH")
        environment["LD_LIBRARY_PATH"] = (
            f"{libdir}{os.pathsep}{current}" if current else str(libdir)
        )
    subprocess.run(command, cwd=ROOT, env=environment, check=True)


def test_view_declarations_are_static_exact_and_pre_constructor() -> None:
    compiler = COMPILER.read_text(encoding="utf-8")
    loader = LOADER.read_text(encoding="utf-8")

    assert 'inspect_static_attribute(py, DIAGNOSTIC_VIEWS_ATTRIBUTE)' in compiler
    assert 'py.import("inspect")' in loader
    assert 'getattr("getattr_static")' in loader
    assert "is_exact_instance_of::<PyTuple>()" in compiler
    assert "item_type.as_any().is(&class)" in compiler
    assert "is_exact_instance_of::<PyBytes>()" in compiler
    assert "construct_production" not in compiler

    prepare = compiler[compiler.index("fn prepare_views<") :]
    assert prepare.index("compile_static_views") < prepare.index("persist_view_set")
    assert prepare.index("persist_view_set") < prepare.index("Ok(PreparedViewClass")

    compile_views = compiler[
        compiler.index("fn compile_static_views<") : compiler.index(
            "struct EncodedViewRecords"
        )
    ]
    assert compile_views.index("tuple.len() > MAX_VIEW_RECORDS") < compile_views.index(
        "PythonViewEncoder::load"
    )
    assert compile_views.index("encoder.encode") < compile_views.index("records.try_push")
    accumulator = compiler[
        compiler.index("impl EncodedViewRecords") : compiler.index(
            "struct PythonViewEncoder"
        )
    ]
    assert accumulator.index("bytes.len() > MAX_VIEW_RECORD_BYTES") < accumulator.index(
        "self.records.push"
    )
    assert accumulator.index("next_total > MAX_TOTAL_VIEW_RECORD_BYTES") < (
        accumulator.index("self.records.push")
    )


def test_compiled_records_are_bounded_versioned_and_atomically_persisted() -> None:
    records = RECORDS.read_text(encoding="utf-8")

    for declaration in (
        "pub const VIEW_MANIFEST_SCHEMA_VERSION: u8 = 1;",
        "pub const MAX_VIEW_RECORDS: usize = 64;",
        "pub const MAX_VIEW_RECORD_BYTES: usize = 256 * 1024;",
        "pub const MAX_TOTAL_VIEW_RECORD_BYTES: usize = 4 * 1024 * 1024;",
        "pub const MAX_VIEW_MANIFEST_BYTES: usize = 64 * 1024;",
    ):
        assert declaration in records
    assert "let record: ViewRecord = serde_json::from_slice(bytes)" in records
    assert "if canonical != bytes" in records
    assert "if !ids.insert(record.id().to_owned())" in records
    assert "transaction_with_behavior(TransactionBehavior::Immediate)" in records
    assert records.index("INSERT INTO diagnostic_view_records") < records.index(
        "INSERT INTO diagnostic_view_manifest"
    )
    assert records.index("hook.before_commit") < records.index("transaction.commit")


def test_failure_classification_requires_clean_user_finalization_or_core_abort() -> None:
    compiler = COMPILER.read_text(encoding="utf-8")

    assert "UserConfiguration" in compiler
    assert "CoreFailure" in compiler
    assert "finalize_user_failure" in compiler
    assert "abort_core_failure" in compiler
    assert "clean_shutdown=true" in compiler
    assert "without marking the archive clean" in compiler
    assert "class.rollback(py)" in compiler
    assert "diagnostic_views.user_failure_finalization_failed" in compiler
    assert "diagnostic_views.import_rollback_failed" in compiler


def test_native_view_compile_and_persistence_contracts() -> None:
    _cargo_test(
        "troupe-diagnostics-runtime",
        "store::view_records::tests",
    )
    _cargo_test(
        "troupe",
        "diagnostic_runtime::view_compile::tests",
    )
