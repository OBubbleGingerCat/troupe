from __future__ import annotations

import os
import subprocess
import sysconfig
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
QUERY = (
    ROOT
    / "rust/crates/troupe-diagnostics-runtime/src/query/archive_views.rs"
)
RUNTIME = ROOT / "rust/src/diagnostic_runtime/archive_views.rs"


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


def test_archive_view_path_is_stored_data_only() -> None:
    query = QUERY.read_text(encoding="utf-8")
    runtime = RUNTIME.read_text(encoding="utf-8")
    combined = f"{query}\n{runtime}"

    for required in (
        "classify_archived_view_record",
        "source.events()",
        "DiagnosticReader::open_archive",
        "load_stored_view_records",
        "IncompatibilityReason::NewerViewSchema",
        "IncompatibilityReason::CorruptRecord",
        "ReaderProfile::Active",
        "ReaderProfile::Archive",
    ):
        assert required in combined

    for forbidden in (
        "pyo3",
        "py.import",
        "import_module",
        "ObservedProductionClass",
        "construct_production",
        "eval(",
        "exec(",
        "dangerouslySetInnerHTML",
    ):
        assert forbidden not in combined


def test_native_archive_view_compatibility_and_isolation_contracts() -> None:
    _cargo_test(
        "troupe-diagnostics-runtime",
        "query::archive_views::tests",
    )
    _cargo_test(
        "troupe",
        "diagnostic_runtime::archive_views::tests",
    )
