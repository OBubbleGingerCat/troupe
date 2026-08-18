from __future__ import annotations

import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
ACTIVATION = ROOT / "rust" / "src" / "diagnostic_runtime" / "activation.rs"
CLI = ROOT / "rust" / "src" / "application" / "cli.rs"
ARTIFACT = ROOT / "tests" / "fixtures" / "artifact_layout" / "nodes" / "X00.json"
GATE = ROOT / "tests" / "fixtures" / "diagnostic_node_gates" / "X00.json"


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def _activation_product_source() -> str:
    return _read(ACTIVATION).split("\n#[cfg(test)]\nmod tests", maxsplit=1)[0]


def test_activation_is_mandatory_and_ready_precedes_every_user_code_phase() -> None:
    source = _activation_product_source()

    for required in (
        "prevalidate_production_root",
        "bootstrap::bootstrap",
        "write_ready(py, runtime.guard())",
        "ProductionLoadProducer::new",
        ".resolve_path(py, root)",
        ".resolve_class(py, path)",
        ".construct(py, class, production_args)",
    ):
        assert required in source
    assert source.index("bootstrap::bootstrap") < source.index(
        "write_ready(py, runtime.guard())"
    )
    assert source.index("write_ready(py, runtime.guard())") < source.index(
        ".resolve_path(py, root)"
    )


def test_all_six_runtime_flags_feed_one_bootstrap_configuration() -> None:
    source = _activation_product_source()

    for field in (
        "bind_host",
        "port",
        "advertise_url",
        "max_run_bytes",
        "writer_stall_timeout",
        "shutdown_timeout",
    ):
        assert f"arguments.{field}" in source
    for required in (
        "BootstrapConfig::new",
        ".with_bind",
        ".with_advertise_url",
        ".with_max_run_bytes",
        ".with_writer_deadlines",
        "WriterDeadlines::new",
    ):
        assert required in source
    for forbidden in ("disable", "best_effort", "fallback_root", "temp_dir"):
        assert forbidden not in source


def test_ready_locator_is_one_versioned_stderr_json_line() -> None:
    source = _activation_product_source()

    assert 'READY_PREFIX: &str = "troupe: diagnostic ready "' in source
    for field in (
        "locator_schema_version",
        "run_id",
        "local_url",
        "advertise_url",
        "archive_directory",
        "security_scope",
    ):
        assert field in source
    assert 'getattr("stderr")' in source
    assert 'getattr("stdout")' not in source
    assert "serde_json::to_string(&locator)" in source
    assert "runtime.server_identity()" in source
    assert ".layout()" in source
    assert ".run_directory()" in source


def test_active_server_assembles_every_read_only_surface_before_registry_ready() -> None:
    source = _activation_product_source()

    for required in (
        "ActiveRouteAssembly::new",
        "QueryEndpoints::active_unobserved",
        "SseEndpoint::active",
        "DumpEndpoints::active",
        "ActiveReplaySource::new",
        "CommitSignal::new",
        "RuntimePerfettoDumpProducer",
    ):
        assert required in source
    assert "load_production" not in source


def test_constructor_context_binds_runtime_custom_and_sink_producers_once() -> None:
    source = _activation_product_source()
    cli = _read(CLI)

    for required in (
        "current_production_construction",
        "PendingProduction",
        "runtime_producer::install",
        "sink_binding::production_capability",
        "custom_binding::bind_run",
    ):
        assert required in source
    assert ".diagnostic_admission()" in source
    assert ".install(capability)" in source
    assert "activation::bind_run" in cli
    assert "runtime_producer::run_started" in cli
    main = cli[cli.index("pub fn main") :]
    assert main.index("activation::activate") < main.index(
        "let guard = SignalGuard::install"
    )
    assert main.index("activation::bind_run") < main.index("run_lifecycle(permit")
    assert "load_production(py" not in cli


def test_x00_descriptors_are_realized_with_the_exact_batched_gate() -> None:
    artifact = json.loads(_read(ARTIFACT))
    gate = json.loads(_read(GATE))

    assert artifact == {
        "state": "realized",
        "introduced": ["tests/integration/test_diagnostic_activation.py"],
        "modified": [
            "rust/src/application/cli.rs",
            "rust/src/diagnostic_runtime/activation.rs",
        ],
        "removed": [],
        "generated": [],
    }
    assert gate == {
        "state": "realized",
        "argv": [
            [
                "pytest",
                "-q",
                "tests/integration/test_diagnostic_activation.py",
                "tests/integration/test_cli.py",
                "tests/integration/test_lifecycle.py",
            ]
        ],
        "env": {},
        "maturin_features": [],
        "cache_requirements": [],
        "exclusive_resources": [],
    }
