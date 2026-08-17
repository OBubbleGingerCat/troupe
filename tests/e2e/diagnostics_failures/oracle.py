"""Closed contracts and result validation for the diagnostics failure matrix."""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any


class OracleError(AssertionError):
    pass


def require(condition: object, detail: str) -> None:
    if not condition:
        raise OracleError(detail)


EXPECTED_CHECKS = {
    "limits.batch": "deterministic_boundary",
    "limits.deadlines": "deterministic_boundary",
    "limits.default_ingress": "deterministic_boundary",
    "limits.deterministic_clock": "deterministic_boundary",
    "local.archive_query": "operation_local",
    "local.archive_reader": "operation_local",
    "local.archive_store": "operation_local",
    "local.exporter": "operation_local",
    "local.invalid_request": "operation_local",
    "local.single_client": "operation_local",
    "local.single_request": "operation_local",
    "local.sink_callback": "sink_local",
    "local.sink_overflow": "sink_local",
    "local.sse_overflow": "operation_local",
    "local.sse_slow_client": "operation_local",
    "runtime.active_reader_corruption": "runtime_fatal",
    "runtime.active_reader_dense_prefix": "runtime_fatal",
    "runtime.active_reader_identity": "runtime_fatal",
    "runtime.admission_byte_boundary": "runtime_fatal",
    "runtime.admission_event_boundary": "runtime_fatal",
    "runtime.first_cause": "runtime_fatal",
    "runtime.hub": "runtime_fatal",
    "runtime.listener": "runtime_fatal",
    "runtime.query_execution_context": "runtime_fatal",
    "runtime.run_quota": "runtime_fatal",
    "runtime.server_execution_context": "runtime_fatal",
    "runtime.sse_reader": "runtime_fatal",
    "runtime.stop_production": "runtime_fatal",
    "runtime.storage_disk": "runtime_fatal",
    "runtime.storage_permission": "runtime_fatal",
    "runtime.view_worker": "runtime_fatal",
    "runtime.writer_commit": "runtime_fatal",
    "runtime.writer_queue": "runtime_fatal",
    "runtime.writer_stall": "runtime_fatal",
    "runtime.writer_transaction": "runtime_fatal",
    "shutdown.archive_readable": "archive_readable",
    "shutdown.clean_shutdown": "clean_terminal",
    "shutdown.dense_prefix": "crash_incomplete",
    "shutdown.drain_timeout": "shutdown_fatal",
    "shutdown.hard_crash": "crash_incomplete",
    "shutdown.incomplete_on_failure": "shutdown_fatal",
    "shutdown.no_residue": "resource_released",
    "shutdown.registry_unpublish": "shutdown_fatal",
    "shutdown.resource_close": "resource_released",
    "shutdown.stream_peer_disconnect": "operation_local",
    "shutdown.terminal_commit": "clean_terminal",
    "sink.callback_component_once": "sink_local",
    "sink.counter_failure_nonrecursive": "sink_local",
    "sink.drop_counter": "sink_local",
    "sink.drop_summary": "sink_local",
    "sink.failure_not_redelivered": "sink_local",
    "sink.failure_visible_store_http_cli": "sink_local",
    "sink.production_continues": "sink_local",
    "sink.unexpected_enqueue_once": "sink_local",
    "startup.active_lease": "startup_fatal",
    "startup.bind": "startup_fatal",
    "startup.initial_commit": "startup_fatal",
    "startup.pre_import": "startup_fatal",
    "startup.registry_publish": "startup_fatal",
    "startup.rollback_resources": "resource_released",
    "startup.schema": "startup_fatal",
    "startup.state_path": "startup_fatal",
    "startup.view_commit_incomplete": "startup_fatal",
    "startup.view_constructor_not_run": "user_startup_failure",
    "startup.view_duplicate": "user_startup_failure",
    "startup.view_finalization_incomplete": "startup_fatal",
    "startup.view_incompatible": "user_startup_failure",
    "startup.view_invalid": "user_startup_failure",
    "startup.view_user_failure_clean": "clean_terminal",
    "startup.write_probe": "startup_fatal",
    "usage.authoritative_race": "usage_terminal",
    "usage.before_act_finish": "usage_terminal",
    "usage.exactly_once": "usage_terminal",
    "usage.mandatory_ack": "usage_terminal",
    "usage.prompt_not_submitted": "usage_terminal",
    "usage.session_terminal_unknown": "usage_terminal",
}


@dataclass(frozen=True, slots=True)
class MatrixCheck:
    identifier: str
    outcome: str


@dataclass(frozen=True, slots=True)
class MatrixCase:
    identifier: str
    phase: str
    adapter: str
    checks: tuple[MatrixCheck, ...]


def _exact_keys(value: dict[str, Any], expected: set[str], context: str) -> None:
    require(set(value) == expected, f"{context} keys differ: {sorted(value)}")


def load_matrix(path: Path) -> tuple[dict[str, Any], tuple[MatrixCase, ...]]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise OracleError(f"could not read failure matrix: {error}") from error
    require(isinstance(value, dict), "failure matrix must be an object")
    _exact_keys(value, {"schema_version", "execution", "cases"}, "matrix")
    require(value["schema_version"] == 1, "unsupported failure matrix schema")

    execution = value["execution"]
    require(isinstance(execution, dict), "execution contract must be an object")
    _exact_keys(
        execution,
        {
            "port",
            "isolated_temp_per_child",
            "child_process_boundary",
            "deterministic_faults",
            "real_disk_fill",
        },
        "execution contract",
    )
    require(execution["port"] == 0, "failure children must request port 0")
    require(execution["isolated_temp_per_child"] is True, "child temp must be isolated")
    require(execution["child_process_boundary"] is True, "faults must run in children")
    require(execution["deterministic_faults"] is True, "faults must be deterministic")
    require(execution["real_disk_fill"] is False, "matrix must not fill the real disk")

    rows = value["cases"]
    require(isinstance(rows, list) and rows, "failure cases must be non-empty")
    cases: list[MatrixCase] = []
    observed: dict[str, str] = {}
    case_ids: set[str] = set()
    for index, row in enumerate(rows):
        require(isinstance(row, dict), f"case {index} is not an object")
        _exact_keys(row, {"id", "phase", "adapter", "checks"}, f"case {index}")
        identifier = row["id"]
        phase = row["phase"]
        adapter = row["adapter"]
        require(isinstance(identifier, str) and identifier, f"case {index} has bad id")
        require(identifier not in case_ids, f"duplicate case id: {identifier}")
        require(
            phase in {"startup", "runtime", "local", "usage", "sink", "shutdown"},
            f"bad phase: {phase}",
        )
        require(isinstance(adapter, str) and adapter, f"case {identifier} has bad adapter")
        raw_checks = row["checks"]
        require(isinstance(raw_checks, list) and raw_checks, f"case {identifier} has no checks")
        checks: list[MatrixCheck] = []
        for raw_check in raw_checks:
            require(isinstance(raw_check, dict), f"case {identifier} has non-object check")
            _exact_keys(raw_check, {"id", "outcome"}, f"case {identifier} check")
            check_id = raw_check["id"]
            outcome = raw_check["outcome"]
            require(isinstance(check_id, str), f"case {identifier} has bad check id")
            require(isinstance(outcome, str), f"case {identifier} has bad outcome")
            require(check_id not in observed, f"duplicate matrix check: {check_id}")
            observed[check_id] = outcome
            checks.append(MatrixCheck(check_id, outcome))
        case_ids.add(identifier)
        cases.append(MatrixCase(identifier, phase, adapter, tuple(checks)))

    require(observed == EXPECTED_CHECKS, "failure matrix check inventory or outcomes drifted")
    return execution, tuple(cases)


def validate_child_result(value: object, case: MatrixCase) -> dict[str, Any]:
    require(isinstance(value, dict), f"child {case.identifier} did not return an object")
    _exact_keys(
        value,
        {
            "result_schema_version",
            "case_id",
            "adapter",
            "pid",
            "requested_port",
            "commands",
            "assertions",
            "duration_ms",
            "status",
        },
        f"child {case.identifier} result",
    )
    require(value["result_schema_version"] == 1, "bad child result schema")
    require(value["case_id"] == case.identifier, "child case identity drifted")
    require(value["adapter"] == case.adapter, "child adapter identity drifted")
    require(type(value["pid"]) is int and value["pid"] > 0, "bad child pid")
    require(value["requested_port"] == 0, "child did not request port 0")
    require(value["status"] == "passed", f"child {case.identifier} did not pass")
    require(type(value["duration_ms"]) is int and value["duration_ms"] >= 0, "bad duration")
    require(isinstance(value["commands"], list), "child commands must be a list")
    require(
        type(value["assertions"]) is int and value["assertions"] > 0,
        "child made no assertions",
    )
    return value


def adapter_checks(cases: tuple[MatrixCase, ...]) -> dict[str, frozenset[str]]:
    result: dict[str, frozenset[str]] = {}
    for case in cases:
        require(case.adapter not in result, f"duplicate matrix adapter: {case.adapter}")
        result[case.adapter] = frozenset(check.identifier for check in case.checks)
    return result
