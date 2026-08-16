from __future__ import annotations

import ast
import json
import re
import runpy
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[2]
DOCUMENT = ROOT / "docs/diagnostics/python.md"
STUB = ROOT / "src/troupe/diagnostics.pyi"
EXAMPLES = (
    ROOT / "examples/diagnostics/sink.py",
    ROOT / "examples/diagnostics/custom.py",
    ROOT / "examples/diagnostics/views.py",
)
ARTIFACT = ROOT / "tests/fixtures/artifact_layout/nodes/O01.json"
GATE = ROOT / "tests/fixtures/diagnostic_node_gates/O01.json"

CAPTURE_FIELDS = (
    "agent_messages",
    "plans",
    "tool_calls",
    "result_validation",
    "usage",
    "custom_events",
    "tool_inputs",
    "tool_outputs",
)
EVENT_CLASSES = (
    "SpanStarted",
    "SpanFinished",
    "InstantOccurred",
    "CounterSampled",
    "AgentMessageDelta",
    "AgentMessageCompleted",
    "AgentPlanSnapshot",
    "ContextUsageSampled",
    "ActTokenUsageFinalized",
    "ObservationGap",
    "CustomSpanStarted",
    "CustomSpanFinished",
    "CustomInstantOccurred",
    "CustomCounterSampled",
)


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def _json(path: Path) -> Any:
    return json.loads(_read(path))


def _between(source: str, start: str, end: str) -> str:
    start_index = source.index(start)
    return source[start_index : source.index(end, start_index)]


def test_document_matches_the_closed_public_event_and_capture_surfaces() -> None:
    document = _read(DOCUMENT)
    stub = _read(STUB)
    capture = _between(stub, "class DiagnosticCapture:", "class DiagnosticSinkStateError")
    event_union = _between(stub, "DiagnosticEvent: _TypeAlias = (", "class DiagnosticCapture:")

    declared_capture = tuple(
        re.findall(r"^    ([a-z_]+): bool = ", capture, flags=re.MULTILINE)
    )
    declared_events = tuple(
        name for name in EVENT_CLASSES if re.search(rf"\b{name}\b", event_union)
    )
    assert declared_capture == CAPTURE_FIELDS
    assert declared_events == EVENT_CLASSES
    for name in (*CAPTURE_FIELDS, *EVENT_CLASSES):
        assert f"`{name}`" in document

    for field in (
        "schema_version",
        "run_id",
        "sequence",
        "elapsed_ns",
        "DiagnosticScope",
        "caused_by",
    ):
        assert f"`{field}`" in document
    assert "User-visible append-only text, never thought content" in document
    assert "Provider raw payloads are not part of this hierarchy" in document


def test_sink_lifecycle_usage_and_observation_boundaries_are_explicit() -> None:
    document = _read(DOCUMENT)
    normalized = " ".join(document.split())

    for text in (
        "UNBOUND -> BOUND -> SEALED -> CLOSED",
        "exactly one admitted Act",
        "final token usage first",
        "act.lifecycle",
        "expires the Act's Python task authority",
        "Multiple calls and multiple concurrent waiters receive the same immutable",
        "does not mean that the Act succeeded",
        "Actor.act()` still returns its validated `dict`",
        "cannot control or extend the observed Act",
        "The Web interface never executes sink callback Python",
    ):
        assert text in normalized

    context = _between(
        document,
        "## Context occupancy and Act token use",
        "## Custom instrumentation",
    )
    normalized_context = " ".join(context.split())
    for text in (
        "not cumulative session usage",
        "exactly one `ActTokenUsageFinalized`",
        "available",
        "partial",
        "unavailable",
        "zero is observed usage",
        "does not impose a public `u64` token maximum",
        "does not duplicate these values",
    ):
        assert text in normalized_context


def test_tool_payload_and_custom_limits_do_not_promise_content_processing() -> None:
    document = _read(DOCUMENT)
    normalized = " ".join(document.split())
    custom = _read(ROOT / "rust/src/diagnostic_python/custom.rs")
    detail = _read(ROOT / "rust/crates/troupe-diagnostics-core/src/detail.rs")

    for declaration in (
        "MAX_CUSTOM_NAME_BYTES: usize = 128",
        "MAX_CUSTOM_KEY_BYTES: usize = 64",
        "MAX_CUSTOM_UNIT_BYTES: usize = 32",
        "MAX_CUSTOM_ATTRIBUTES: usize = 32",
        "MAX_CUSTOM_DIMENSIONS: usize = 8",
        "MAX_CUSTOM_LIST_ITEMS: usize = 64",
    ):
        assert declaration in detail
    assert "_MAX_CUSTOM_PAYLOAD_BYTES = 65_536" in custom
    for bound in (
        "128 byte",
        "64 UTF-8 bytes",
        "32 bytes",
        "32 attributes",
        "8 dimensions",
        "64 items",
        "64 KiB",
    ):
        assert bound in normalized

    assert "does not inspect keys, identify credentials, redact, or rewrite" in normalized
    assert "performs no content scan, credential-key redaction, or rewriting" in normalized
    for name in (
        "FrozenJsonArray",
        "FrozenJsonObject",
        "FrozenJsonValue",
        "DiagnosticToolInput",
        "DiagnosticToolOutput",
        "DiagnosticToolLocation",
    ):
        assert f"`{name}`" in document


def test_four_static_view_types_and_pre_constructor_contract_are_documented() -> None:
    document = _read(DOCUMENT)
    views = _read(EXAMPLES[2])

    for name in ("TimelineView", "MetricView", "TableView", "TimeSeriesView"):
        assert f"`{name}`" in document
        assert f"diagnostics.{name}(" in views
    for forbidden in ("SQL", "regex", "joins", "Python callables", "custom renderers"):
        assert forbidden in document
    assert "before calling the Production constructor" in document
    assert "do not import or execute Production Python" in document
    assert "diagnostic_views = DIAGNOSTIC_VIEWS" in views


def test_examples_are_parseable_bounded_and_callbacks_are_observational() -> None:
    trees = {path.name: ast.parse(_read(path), filename=str(path)) for path in EXAMPLES}
    sink_tree = trees["sink.py"]

    for path in EXAMPLES:
        runpy.run_path(str(path), run_name="__main__")

    callback = next(
        node
        for node in ast.walk(sink_tree)
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))
        and node.name == "on_event"
    )
    callback_attributes = {
        node.attr for node in ast.walk(callback) if isinstance(node, ast.Attribute)
    }
    assert "act" not in callback_attributes
    assert "make_effect" not in callback_attributes

    sink_source = _read(EXAMPLES[0])
    assert "diagnostic_sink=sink" in sink_source
    assert "await sink.wait_closed()" in sink_source
    assert "tuple[dict[str, JsonValue], diagnostics.DiagnosticSinkSummary]" in sink_source

    for tree in trees.values():
        called_names = {
            node.func.id
            for node in ast.walk(tree)
            if isinstance(node, ast.Call) and isinstance(node.func, ast.Name)
        }
        assert not ({"eval", "exec"} & called_names)


def test_node_descriptors_project_the_exact_documentation_gate() -> None:
    introduced = {
        "docs/diagnostics/python.md",
        "examples/diagnostics/custom.py",
        "examples/diagnostics/sink.py",
        "examples/diagnostics/views.py",
        "tests/documentation/test_diagnostics_python.py",
        "tests/typing/diagnostics_examples.py",
    }
    assert _json(ARTIFACT) == {
        "state": "realized",
        "introduced": sorted(introduced),
        "modified": [],
        "removed": [],
        "generated": [],
    }
    assert _json(GATE) == {
        "state": "realized",
        "argv": [
            ["pytest", "-q", "tests/documentation/test_diagnostics_python.py"],
            ["python", "-m", "mypy", "--strict", "tests/typing/diagnostics_examples.py"],
        ],
        "env": {},
        "maturin_features": [],
        "cache_requirements": [],
        "exclusive_resources": [],
    }
