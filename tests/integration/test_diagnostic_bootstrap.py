from __future__ import annotations

import json
import os
import subprocess
import sysconfig
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
MATRIX = ROOT / "tests/fixtures/diagnostics/bootstrap/failure-matrix.json"
SOURCE = ROOT / "rust/src/diagnostic_runtime/bootstrap.rs"

PHASES = [
    "run_identity_allocated",
    "archive_prepared",
    "active_lease_acquired",
    "initial_store_ready",
    "writer_supervisor_ready",
    "listener_ready",
    "registry_published",
    "ready_result",
]


def test_failure_matrix_is_closed_and_reverse_cleanup_is_explicit() -> None:
    matrix = json.loads(MATRIX.read_text(encoding="utf-8"))

    assert set(matrix) == {"schema_version", "coordination_order", "cases"}
    assert matrix["schema_version"] == 1
    assert matrix["coordination_order"] == PHASES
    assert [case["fail_after"] for case in matrix["cases"]] == PHASES
    assert all(set(case) == {"fail_after", "cleanup"} for case in matrix["cases"])
    assert matrix["cases"][-1]["cleanup"] == [
        "registry",
        "listener",
        "hub",
        "writer_store",
        "active_lease",
    ]


def test_bootstrap_uses_real_predecessor_apis_without_user_loading() -> None:
    source = SOURCE.read_text(encoding="utf-8")
    startup = source[source.index("fn bootstrap_root<") :]
    ordered = [
        "ArchiveLayout::prepare",
        "ActiveArchiveLease::acquire",
        "DiagnosticStore::create",
        "WriterSupervisor::start",
        "DiagnosticServer::start",
        "publish_registry_entry",
    ]
    positions = [startup.index(marker) for marker in ordered]

    assert positions == sorted(positions)
    assert "load_production" not in source
    assert "construct_production" not in source
    assert "production.path_resolution" not in source


def test_native_bootstrap_contract_and_real_resource_fault_matrix() -> None:
    command = [
        "cargo",
        "test",
        "--locked",
        "--manifest-path",
        "rust/Cargo.toml",
        "--package",
        "troupe",
        "--features",
        "diagnostics-test-support",
        "diagnostic_runtime::bootstrap::tests",
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
