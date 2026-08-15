from __future__ import annotations

import json
import os
import subprocess
import sysconfig
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "rust/src/diagnostic_runtime/custom_binding.rs"
CUSTOM_VALUES = ROOT / "rust/src/diagnostic_python/custom.rs"
MATRIX = ROOT / "tests/fixtures/diagnostics/custom-runtime-matrix.json"


def _load_json(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def _canonical_json(value: Any) -> bytes:
    return json.dumps(
        value,
        ensure_ascii=False,
        allow_nan=False,
        separators=(",", ":"),
    ).encode("utf-8")


def test_context_matrix_closes_authority_parent_and_failure_contracts() -> None:
    matrix = _load_json(MATRIX)

    assert set(matrix) == {
        "schema_version",
        "authorized_contexts",
        "rejected_contexts",
        "caller_control",
        "temporal_parent",
        "span_exit_outcomes",
        "publication",
        "canonical_fixtures",
    }
    assert matrix["schema_version"] == 1

    authorized = {case["context"]: case for case in matrix["authorized_contexts"]}
    assert set(authorized) == {
        "production_start",
        "scene",
        "cue",
        "production_stop",
        "registered_child",
    }
    assert authorized["production_start"]["scope_fields"] == []
    assert authorized["scene"]["scope_fields"] == ["scene_id"]
    assert authorized["cue"]["scope_fields"] == [
        "scene_id",
        "actor_id",
        "cue_id",
    ]
    assert authorized["production_stop"]["scope_fields"] == []
    assert authorized["registered_child"] == {
        "context": "registered_child",
        "scope_fields": "inherited_domain",
        "containing_span": "inherited_builtin_domain",
        "registered_child": True,
    }

    rejected = {case["context"]: case for case in matrix["rejected_contexts"]}
    assert set(rejected) == {
        "module_import",
        "production_constructor",
        "plain_thread",
        "unregistered_task",
        "expired_scope",
        "run_ended",
    }
    assert all(case["error"] == "DiagnosticContextError" for case in rejected.values())
    assert all(case["allocates_sequence"] is False for case in rejected.values())

    caller = matrix["caller_control"]
    assert caller["runtime_owned_fields"] == [
        "run_id",
        "sequence",
        "elapsed_ns",
        "scope",
        "parent_span_id",
        "containing_span_id",
        "caused_by",
    ]
    assert caller["returns_canonical_id"] is False

    parent = matrix["temporal_parent"]
    assert parent["same_task_nested_custom_span"] == "innermost_custom_span"
    assert parent["same_task_without_custom_span"] == "innermost_builtin_span"
    assert parent["registered_child"] == "innermost_builtin_span"
    assert parent["propagates_custom_span_to_registered_child"] is False

    assert matrix["span_exit_outcomes"] == [
        {"exit": "normal", "outcome": "completed", "suppresses_exception": False},
        {
            "exit": "cancelled",
            "outcome": "cancelled",
            "suppresses_exception": False,
        },
        {"exit": "failed", "outcome": "failed", "suppresses_exception": False},
    ]
    publication = matrix["publication"]
    for operation in ("event", "counter", "span_enter", "span_exit"):
        assert publication[operation] == {
            "canonical_facts": 1,
            "path": "mandatory_s04",
            "return": None,
        }
    assert publication["core_failure"] == "fatal_latched"


def test_native_binding_uses_lineage_and_mandatory_admission_seams_only() -> None:
    source = SOURCE.read_text(encoding="utf-8")
    implementation = source[: source.rindex("#[cfg(test)]\nmod tests")]

    for required in (
        "binding.current_lineage(py)?",
        ".filter(TaskLineage::is_active)",
        "lineage.runtime()",
        "lineage.cued()",
        "runtime_producer::lineage_snapshot(binding, phase)",
        "cue_producer::lineage_snapshot(&cued)",
        "scene_producer::lineage_snapshot(lineage)",
        'py.import("asyncio")?.getattr("current_task")',
        "PyWeakrefReference::new(task)?",
        'diagnostics.getattr("DiagnosticContextError")',
        "start_custom_span",
        "finish_custom_span",
        "emit_custom_instant",
        "emit_custom_counter",
        "runtime.latch_diagnostic_failure(error)",
    ):
        assert required in implementation

    assert implementation.count(".start_custom_span(") == 1
    assert implementation.count(".finish_custom_span(") == 1
    assert implementation.count(".emit_custom_instant(") == 1
    assert implementation.count(".emit_custom_counter(") == 1
    assert "ProductionDiagnosticHub" not in implementation
    assert "diagnostic_sink" not in implementation
    assert "custom_act_binding" not in implementation
    assert "callback" not in implementation


def test_native_hook_accepts_only_p02_candidates_and_owns_canonical_fields() -> None:
    source = SOURCE.read_text(encoding="utf-8")
    parser = source[
        source.index("fn parse_candidate(") : source.index("fn core_failure(")
    ]

    for candidate in (
        "_CustomInstantCandidate",
        "_CustomCounterCandidate",
        "_CustomSpanStartCandidate",
        "_CustomSpanFinishCandidate",
    ):
        assert candidate in parser
    assert "candidate.get_type().is(&expected)" in source
    assert 'getattr("_custom_candidate_payload")' in source
    for runtime_owned in (
        "run_id",
        "sequence",
        "elapsed_ns",
        "scope",
        "parent_span_id",
        "containing_span_id",
        "caused_by",
    ):
        assert runtime_owned not in parser

    assert (
        "pub(crate) fn install(diagnostics: &Bound<'_, PyModule>) -> PyResult<()>"
        in source
    )
    assert 'getattr("_set_custom_admission_hook")' in source
    assert "#[pyclass(frozen, module = \"troupe.diagnostics\")]" in source
    assert "pub(crate) fn bind_run(_py: Python<'_>, binding: &Arc<RunBinding>)" in source
    assert "binding: Weak<RunBinding>" in source
    assert "binding: Arc::downgrade(binding)" in source


def test_span_exit_contract_remains_the_frozen_p02_protocol() -> None:
    source = CUSTOM_VALUES.read_text(encoding="utf-8")
    span_context = source[
        source.index("class _CustomSpanContext:") : source.index("def event(")
    ]

    assert "_admit_custom_candidate(self._candidate)" in span_context
    assert 'outcome = "completed"' in span_context
    assert "isinstance(exception, _asyncio.CancelledError)" in span_context
    assert 'outcome = "cancelled"' in span_context
    assert 'outcome = "failed"' in span_context
    assert "_CustomSpanFinishCandidate(outcome=outcome)" in span_context
    assert "return False" in span_context


def test_c03_custom_fixtures_round_trip_without_identity_reinterpretation() -> None:
    matrix = _load_json(MATRIX)
    expected = {
        "custom_span_started",
        "custom_instant_occurred",
        "custom_counter_sampled",
        "custom_span_finished",
    }
    observed: set[str] = set()

    for fixture in matrix["canonical_fixtures"]:
        path = ROOT / fixture["path"]
        events = _load_json(path)
        assert json.loads(_canonical_json(events)) == events
        assert any(event["kind"] == fixture["kind"] for event in events)
        for event in events:
            assert event["schema_version"] == 1
            assert isinstance(event["sequence"], str) and event["sequence"].isdigit()
            assert isinstance(event["elapsed_ns"], str) and event["elapsed_ns"].isdigit()
            assert set(event["scope"]) == {
                "scene_id",
                "actor_id",
                "cue_id",
                "effect_id",
                "act_id",
                "tool_call_id",
                "session_generation",
            }
        observed.add(fixture["kind"])

    assert observed == expected


def test_native_custom_runtime_contract_and_non_test_library_build() -> None:
    environment = os.environ.copy()
    libdir = sysconfig.get_config_var("LIBDIR")
    if libdir:
        current = environment.get("LD_LIBRARY_PATH")
        environment["LD_LIBRARY_PATH"] = (
            f"{libdir}{os.pathsep}{current}" if current else str(libdir)
        )

    base = [
        "cargo",
        "--locked",
        "--manifest-path",
        "rust/Cargo.toml",
        "--package",
        "troupe",
        "--features",
        "agent-test-support,diagnostics-test-support",
    ]
    subprocess.run(
        [
            base[0],
            "test",
            *base[1:],
            "diagnostic_runtime::custom_binding::tests",
            "--lib",
            "--no-fail-fast",
            "--",
            "--nocapture",
        ],
        cwd=ROOT,
        env=environment,
        check=True,
    )
    subprocess.run(
        [base[0], "check", *base[1:], "--lib"],
        cwd=ROOT,
        env=environment,
        check=True,
    )
