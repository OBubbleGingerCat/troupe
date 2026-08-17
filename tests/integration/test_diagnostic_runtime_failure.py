from __future__ import annotations

import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SUPERVISOR = ROOT / "rust/src/diagnostic_runtime/supervisor.rs"
ACTIVATION = ROOT / "rust/src/diagnostic_runtime/activation.rs"
BOOTSTRAP = ROOT / "rust/src/diagnostic_runtime/bootstrap.rs"
CLI = ROOT / "rust/src/application/cli.rs"
RUNTIME = ROOT / "rust/src/orchestration/runtime.rs"
PYTHON_TASK = ROOT / "rust/src/orchestration/python_task.rs"
MATRIX = ROOT / "tests/fixtures/diagnostics/runtime-failure-matrix.json"
ARTIFACT = ROOT / "tests/fixtures/artifact_layout/nodes/X01.json"
GATE = ROOT / "tests/fixtures/diagnostic_node_gates/X01.json"


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def _json(path: Path) -> dict[str, object]:
    value = json.loads(_read(path))
    assert isinstance(value, dict)
    return value


def test_failure_matrix_closes_fatal_and_local_boundaries() -> None:
    matrix = _json(MATRIX)

    assert matrix["schema_version"] == 1
    assert matrix["first_cause_policy"] == "latch_once"
    assert {entry["id"] for entry in matrix["fatal_sources"]} == {
        "server_execution_context",
        "listener",
        "hub_canonical_path",
        "writer_task",
        "writer_commit",
        "persistent_store",
        "mandatory_ingress_budget",
        "writer_stall",
        "run_quota",
        "active_reader_sqlite_corruption",
        "active_reader_identity",
        "active_reader_dense_prefix",
        "active_query_execution_context",
        "active_view_query_worker",
        "active_sse_reader",
    }
    assert {entry["id"] for entry in matrix["local_sources"]} == {
        "single_http_client",
        "single_http_request",
        "invalid_http_request",
        "sse_slow_client",
        "sse_subscriber_overflow",
        "archive_reader",
        "archive_query",
        "archive_store",
        "python_sink_callback",
        "python_sink_overflow",
        "on_demand_exporter",
    }
    assert matrix["fatal_convergence"] == {
        "seal_new_work": True,
        "cancel_runtime": True,
        "wait_for_current_operation": True,
        "exit_nonzero": True,
        "archive_clean_shutdown_requires_x02_terminal_transaction": True,
        "restart_component": False,
    }
    assert matrix["failure_precedence"] == [
        "diagnostic_infrastructure",
        "production_lifecycle",
        "diagnostic_shutdown",
    ]


def test_active_failure_reporters_feed_one_first_cause_latch() -> None:
    activation = _read(ACTIVATION)
    supervisor = _read(SUPERVISOR)

    assert "|_failure| {}" not in activation
    for reporter in ("report_query", "report_view", "report_sse"):
        assert reporter in activation
    for source in ("report_guard", "report_producer", "report_query", "report_view"):
        assert f"fn {source}" in supervisor
    assert "if current.is_some()" in supervisor
    assert "*current = Some(failure)" in supervisor
    assert "SseCoreFailureCode::Replay(_) => None" in supervisor


def test_fatal_convergence_seals_before_cancelling_and_preserves_surface() -> None:
    supervisor = _read(SUPERVISOR)
    cli = _read(CLI)
    bootstrap = _read(BOOTSTRAP)
    drive = supervisor[supervisor.index("pub(crate) async fn supervise") :]

    assert drive.index("probe.seal_new_work()") < drive.index("core.request_shutdown()")
    assert "operation.as_mut().await" in drive
    assert "seal_for_external_core_failure" in bootstrap
    assert "failure_probe()" in cli
    assert ".bind_producer(producer)" in cli
    assert "supervisor::supervise(probe, core, operation)" in cli
    assert '"troupe: diagnostic runtime failed: "' in supervisor
    assert "if let Some(failure) = infrastructure_failure" in cli
    assert cli.index("if let Some(failure) = infrastructure_failure") < cli.index(
        "match (run_result, restore_result, diagnostic_shutdown)"
    )


def test_in_flight_start_hook_uses_the_runtime_cancellation_token() -> None:
    runtime = _read(RUNTIME)
    python_task = _read(PYTHON_TASK)
    lifecycle = runtime[runtime.index("pub(crate) async fn run_lifecycle") :]

    start_call, stop_call = lifecycle.split("let stop_result = await_hook", maxsplit=1)
    assert "Some(permit.core.shutdown_token())" in start_call
    assert "None," in stop_call
    assert "cancellation.cancelled()" in python_task
    assert "cancel_python_task(locals, &task).await" in python_task
    assert "resolve_cancelled_hook" in python_task


def test_x01_descriptors_are_realized_with_one_source_gate() -> None:
    assert _json(ARTIFACT) == {
        "state": "realized",
        "introduced": [
            "tests/fixtures/diagnostics/runtime-failure-matrix.json",
            "tests/integration/test_diagnostic_runtime_failure.py",
        ],
        "modified": [
            "rust/src/application/cli.rs",
            "rust/src/diagnostic_runtime/activation.rs",
            "rust/src/diagnostic_runtime/bootstrap.rs",
            "rust/src/diagnostic_runtime/supervisor.rs",
            "rust/src/orchestration/python_task.rs",
            "rust/src/orchestration/runtime.rs",
        ],
        "removed": [],
        "generated": [],
    }
    assert _json(GATE) == {
        "state": "realized",
        "argv": [["pytest", "-q", "tests/integration/test_diagnostic_runtime_failure.py"]],
        "env": {},
        "maturin_features": [],
        "cache_requirements": [],
        "exclusive_resources": [],
    }
