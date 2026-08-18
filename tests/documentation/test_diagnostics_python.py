from __future__ import annotations

import ast
import asyncio
import importlib
import json
import re
import runpy
from contextlib import nullcontext
from pathlib import Path
from types import SimpleNamespace
from typing import Any

import pytest


ROOT = Path(__file__).resolve().parents[2]
DOCUMENT = ROOT / "docs/diagnostics/python.md"
STUB = ROOT / "src/troupe/diagnostics.pyi"
EXAMPLES = (
    ROOT / "examples/diagnostics/sink.py",
    ROOT / "examples/diagnostics/custom.py",
)
SHOWCASE = ROOT / "examples/diagnostics/production.py"
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


def _showcase_module(monkeypatch: pytest.MonkeyPatch) -> Any:
    monkeypatch.syspath_prepend(str(ROOT))
    return importlib.import_module("examples.diagnostics.production")


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


def test_python_cannot_register_visualization_panels() -> None:
    document = _read(DOCUMENT)
    stub = _read(STUB)
    showcase = _read(SHOWCASE)
    assert "Built-in Timeline" in document
    assert "ViewSpec" not in stub
    assert "diagnostic_views" not in showcase


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


def test_end_to_end_showcase_composes_real_finite_scenes_without_running_provider() -> None:
    document = _read(DOCUMENT)
    source = _read(SHOWCASE)
    tree = ast.parse(source, filename=str(SHOWCASE))

    assert (SHOWCASE.parent / "__init__.py").read_bytes() == b""
    assert "examples/diagnostics/production.py" in document
    examples_document = _read(ROOT / "examples/README.md")
    assert "continuously consumes provider tokens" in examples_document
    assert "agent-test-support" in examples_document
    assert "maturin develop --uv --locked" in examples_document
    for command in (
        "troupe diagnostic status --production examples/diagnostics",
        "troupe diagnostic events --production examples/diagnostics",
        "troupe diagnostic dump --production examples/diagnostics",
        "troupe diagnostic serve --production examples/diagnostics",
    ):
        assert command in examples_document
    for marker in (
        "diagnostic_sink=sink",
        "await sink.wait_closed()",
        "record_batch(queue_depth=planned_depth, region=operation)",
        '"example.scene_cycle"',
        '"example.act_observed"',
        "await asyncio.gather(",
        "await asyncio.sleep(self.interval_seconds)",
    ):
        assert marker in source

    production = next(
        node
        for node in tree.body
        if isinstance(node, ast.ClassDef) and node.name == "Production"
    )
    scene = next(
        node
        for node in production.body
        if isinstance(node, ast.AsyncFunctionDef) and node.name == "scene"
    )
    assert not any(
        isinstance(node, (ast.For, ast.AsyncFor, ast.While))
        for node in ast.walk(scene)
    )
    assert sum(
        isinstance(node, ast.Call)
        and isinstance(node.func, ast.Attribute)
        and node.func.attr == "create_task"
        for node in ast.walk(scene)
    ) == 2
    custom_span_blocks = [
        node
        for node in ast.walk(tree)
        if isinstance(node, ast.With)
        and any(
            isinstance(item.context_expr, ast.Call)
            and isinstance(item.context_expr.func, ast.Attribute)
            and isinstance(item.context_expr.func.value, ast.Name)
            and item.context_expr.func.value.id == "diagnostics"
            and item.context_expr.func.attr == "span"
            for item in node.items
        )
    ]
    assert custom_span_blocks
    assert all(
        not any(
            isinstance(descendant, ast.Await)
            for statement in block.body
            for descendant in ast.walk(statement)
        )
        for block in custom_span_blocks
    )


def test_end_to_end_showcase_runs_repeated_scene_cycles_without_provider(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    showcase = _showcase_module(monkeypatch)
    recorded_events: list[tuple[str, dict[str, object]]] = []
    recorded_counters: list[tuple[str, int]] = []

    def record_event(name: str, *, attributes: dict[str, object]) -> None:
        recorded_events.append((name, attributes))

    def record_counter(
        name: str,
        value: int,
        *,
        unit: str,
        dimensions: dict[str, object] | None = None,
    ) -> None:
        _ = (unit, dimensions)
        recorded_counters.append((name, value))

    fake_diagnostics = SimpleNamespace(
        span=lambda *_args, **_kwargs: nullcontext(),
        event=record_event,
        counter=record_counter,
    )
    monkeypatch.setattr(showcase, "diagnostics", fake_diagnostics)

    class FakeWorker:
        def __init__(self) -> None:
            self.calls: list[dict[str, object]] = []
            self.barriers: dict[int, asyncio.Event] = {}
            self.counts: dict[int, int] = {}

        async def cue(self, instruction: dict[str, object]) -> tuple[object, ...]:
            scene = int(instruction["scene_number"])
            barrier = self.barriers.setdefault(scene, asyncio.Event())
            self.calls.append(dict(instruction))
            self.counts[scene] = self.counts.get(scene, 0) + 1
            if self.counts[scene] == 2:
                barrier.set()
            await barrier.wait()
            operation = str(instruction["operation"])
            return (
                SimpleNamespace(
                    operation=operation,
                    result={"operation": operation},
                    observation={"sink_complete": True},
                ),
            )

    async def exercise() -> tuple[SimpleNamespace, FakeWorker]:
        worker = FakeWorker()
        state = SimpleNamespace(
            interval_seconds=0.0,
            scene_number=0,
            completed_scenes=0,
            worker=worker,
        )
        await showcase.Production.scene(state)
        await showcase.Production.scene(state)
        return state, worker

    state, worker = asyncio.run(exercise())
    assert state.scene_number == 2
    assert state.completed_scenes == 2
    assert [call["operation"] for call in worker.calls] == [
        "probe",
        "recall",
        "probe",
        "recall",
    ]
    assert recorded_counters == [
        ("example.completed_scenes", 1),
        ("example.completed_scenes", 2),
    ]
    assert [name for name, _ in recorded_events] == [
        "example.scene_started",
        "example.scene_completed",
        "example.scene_started",
        "example.scene_completed",
    ]


def test_end_to_end_showcase_does_not_complete_a_scene_cancelled_during_interval(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    showcase = _showcase_module(monkeypatch)
    original_sleep = asyncio.sleep
    recorded_events: list[str] = []

    async def cancel_interval(delay: float) -> None:
        if delay == 0:
            await original_sleep(0)
            return
        raise asyncio.CancelledError

    monkeypatch.setattr(showcase.asyncio, "sleep", cancel_interval)
    monkeypatch.setattr(
        showcase,
        "diagnostics",
        SimpleNamespace(
            span=lambda *_args, **_kwargs: nullcontext(),
            event=lambda name, **_kwargs: recorded_events.append(name),
            counter=lambda *_args, **_kwargs: None,
        ),
    )

    class FakeWorker:
        async def cue(self, instruction: dict[str, object]) -> tuple[object, ...]:
            operation = str(instruction["operation"])
            return (
                SimpleNamespace(
                    operation=operation,
                    result={"operation": operation},
                    observation={"sink_complete": True},
                ),
            )

    state = SimpleNamespace(
        interval_seconds=1.0,
        scene_number=0,
        completed_scenes=0,
        worker=FakeWorker(),
    )
    with pytest.raises(asyncio.CancelledError):
        asyncio.run(showcase.Production.scene(state))
    assert state.scene_number == 1
    assert state.completed_scenes == 0
    assert "example.scene_completed" not in recorded_events


@pytest.mark.parametrize("value", ["nan", "inf", "-1", "1 2"])
def test_end_to_end_showcase_rejects_invalid_intervals(
    monkeypatch: pytest.MonkeyPatch,
    value: str,
) -> None:
    showcase = _showcase_module(monkeypatch)
    with pytest.raises(ValueError):
        showcase.parse_interval_seconds([value])


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
        "maturin_features": ["diagnostics-test-support"],
        "cache_requirements": [],
        "exclusive_resources": [],
    }
