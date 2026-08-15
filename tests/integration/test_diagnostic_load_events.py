from __future__ import annotations

import json
import os
import subprocess
import sysconfig
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "tests/fixtures/diagnostics/producers/load-events.json"
SOURCE = ROOT / "rust/src/diagnostic_runtime/load_producer.rs"
LOADER = ROOT / "rust/src/application/loader/mod.rs"
BOOTSTRAP = ROOT / "rust/src/diagnostic_runtime/bootstrap.rs"


def _method(source: str, name: str, next_name: str) -> str:
    start = source.index(f"pub(crate) fn {name}(")
    end = source.index(f"pub(crate) fn {next_name}(", start)
    return source[start:end]


def test_load_event_fixture_is_closed_and_typed() -> None:
    fixture = json.loads(FIXTURE.read_text(encoding="utf-8"))

    assert set(fixture) == {
        "schema_version",
        "success_events",
        "failure_codes",
        "excluded_payload_fields",
    }
    assert fixture["schema_version"] == 1
    assert fixture["success_events"] == [
        {
            "sequence": "1",
            "kind": "span_started",
            "span_kind": "production.path_resolution",
            "detail_keys": ["production_root", "package"],
            "parent_span_id": None,
        },
        {
            "sequence": "2",
            "kind": "span_finished",
            "span_id": "1",
            "outcome": "completed",
            "error_code": None,
        },
        {
            "sequence": "3",
            "kind": "span_started",
            "span_kind": "production.load",
            "detail_keys": ["package"],
            "parent_span_id": None,
        },
        {
            "sequence": "4",
            "kind": "span_finished",
            "span_id": "3",
            "outcome": "completed",
            "error_code": None,
        },
        {
            "sequence": "5",
            "kind": "span_started",
            "span_kind": "production.construct",
            "detail_keys": ["package", "class_name"],
            "class_name": "Production",
            "parent_span_id": None,
        },
        {
            "sequence": "6",
            "kind": "span_finished",
            "span_id": "5",
            "outcome": "completed",
            "error_code": None,
        },
    ]
    assert fixture["failure_codes"] == {
        "production.path_resolution": [
            "invalid-package-name",
            "missing-init",
            "missing-production",
            "production-path-resolution-failed",
        ],
        "production.load": [
            "package-name-conflict",
            "import-failed",
            "missing-symbol",
            "symbol-not-class",
            "symbol-is-base",
            "symbol-not-subclass",
            "production-load-failed",
        ],
        "production.construct": [
            "construction-failed",
            "system-exit",
            "production-construct-failed",
        ],
    }
    assert fixture["excluded_payload_fields"] == [
        "module_source",
        "script",
        "traceback",
        "raw_exception",
        "exception_message",
    ]


def test_real_operations_are_bracketed_after_ready_without_backfill() -> None:
    source = SOURCE.read_text(encoding="utf-8")
    path_phase = _method(source, "resolve_path", "resolve_class")
    load_phase = _method(source, "resolve_class", "construct")
    construct_start = source.index("pub(crate) fn construct(")
    construct_end = source.index("fn python_failure(", construct_start)
    construct_phase = source[construct_start:construct_end]

    for phase, operation in (
        (path_phase, "resolve_production_package"),
        (load_phase, "resolve_production_class"),
        (construct_phase, "construct_production"),
    ):
        assert phase.index("start_span") < phase.index(operation)
        assert phase.index(operation) < phase.index("finish_span")

    assert "RunClock::from_origin(Instant::now())" in source
    assert "Arc::clone(runtime.hub())" in source
    assert "diagnostic_finish_error: Option<DiagnosticProducerError>" in source
    assert "let cleanup_error = resolved.rollback(py).err();" in source
    for forbidden in (
        "module_source",
        "script",
        "traceback",
        "raw_exception",
        "exception_message",
    ):
        assert forbidden not in source


def test_pre_hub_work_is_only_root_prevalidation() -> None:
    loader = LOADER.read_text(encoding="utf-8")
    bootstrap = BOOTSTRAP.read_text(encoding="utf-8")

    root = loader.index("prevalidate_production_root(py, package_dir)")
    package = loader.index("resolve_production_package(py, root)")
    assert root < package
    assert "production_root: &PrevalidatedProductionRoot" in bootstrap
    assert "ResolvedProductionPath" not in bootstrap


def test_native_load_producer_contract() -> None:
    command = [
        "cargo",
        "test",
        "--locked",
        "--manifest-path",
        "rust/Cargo.toml",
        "--package",
        "troupe",
        "diagnostic_runtime::load_producer::tests",
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
