use std::path::{Path, PathBuf};

use pyo3::prelude::*;

#[path = "../src/diagnostic_python/events.rs"]
mod events;
#[path = "../src/diagnostic_python/fragment_test_support.rs"]
mod fragment_test_support;

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("rust manifest must have a repository parent")
        .to_path_buf()
}

fn with_fresh_events_module(test: &std::ffi::CStr) {
    pyo3::Python::initialize();
    Python::attach(|py| {
        let module = fragment_test_support::install_fresh_fragment(
            py,
            events::source(),
            c"diagnostic-events.py",
            c"_troupe_diagnostic_events_test",
        )
        .expect("install diagnostic event fragment");
        module
            .setattr(
                "FIXTURES_ROOT",
                repository_root().join("tests/fixtures/diagnostics/events"),
            )
            .expect("publish fixture path");
        let namespace = module.dict();
        py.run(test, Some(&namespace), Some(&namespace))
            .expect("diagnostic Python event assertion failed");
    });
}

#[test]
fn canonical_c03_fixtures_project_losslessly_into_closed_immutable_events() {
    with_fresh_events_module(
        cr#"
import hashlib as _hashlib
import json as _json
from dataclasses import fields as _fields, is_dataclass as _is_dataclass
from decimal import Decimal as _Decimal
from pathlib import Path as _Path
from types import NoneType as _NoneType
from typing import get_args as _get_args
from uuid import UUID as _UUID

_expected_variants = (
    SpanStarted,
    SpanFinished,
    InstantOccurred,
    CounterSampled,
    AgentMessageDelta,
    AgentMessageCompleted,
    AgentPlanSnapshot,
    ContextUsageSampled,
    ActTokenUsageFinalized,
    ObservationGap,
    CustomSpanStarted,
    CustomSpanFinished,
    CustomInstantOccurred,
    CustomCounterSampled,
)
assert _get_args(DiagnosticEvent) == _expected_variants
assert tuple(value.kind for value in _expected_variants) == (
    "span_started",
    "span_finished",
    "instant_occurred",
    "counter_sampled",
    "agent_message_delta",
    "agent_message_completed",
    "agent_plan_snapshot",
    "context_usage_sampled",
    "act_token_usage_finalized",
    "observation_gap",
    "custom_span_started",
    "custom_span_finished",
    "custom_instant_occurred",
    "custom_counter_sampled",
)
assert _get_args(FrozenJsonValue) == (
    _NoneType,
    bool,
    int,
    _Decimal,
    str,
    FrozenJsonArray,
    FrozenJsonObject,
)

def _assert_no_mutable_container(value):
    assert not isinstance(value, (dict, list, set, bytearray))
    if _is_dataclass(value):
        for field in _fields(value):
            _assert_no_mutable_container(getattr(value, field.name))
    elif type(value) is tuple:
        for item in value:
            _assert_no_mutable_container(item)

_root = _Path(FIXTURES_ROOT)
_manifest_bytes = (_root / "manifest.json").read_bytes()
assert _manifest_bytes.endswith(b"\n")
_manifest = _json.loads(_manifest_bytes)
_seen_kinds = set()
_saw_uuid = False
_saw_decimal = False
_saw_arbitrary_token_integer = False
_saw_scope_and_causality = False
_tool_details = []

for _entry in _manifest["fixtures"]:
    _fixture_bytes = (_root / _entry["file"]).read_bytes()
    assert _hashlib.sha256(_fixture_bytes).hexdigest() == _entry["sha256"]
    _payload = _json.loads(_fixture_bytes)
    if _entry["format"] == "malformed_cases":
        for _case in _payload["cases"]:
            try:
                _event_from_mapping(_case["event"])
            except (TypeError, ValueError):
                pass
            else:
                raise AssertionError(f"malformed fixture was accepted: {_case['name']}")
        continue

    assert _entry["format"] == "event_array"
    assert type(_payload) is list
    for _raw in _payload:
        _event = _event_from_mapping(_raw)
        assert isinstance(_event, DiagnosticEvent)
        assert type(_event) in _expected_variants
        assert _event.kind == _raw["kind"]
        _seen_kinds.add(_event.kind)
        _assert_no_mutable_container(_event)

        _expected_event_bytes = _json.dumps(
            _raw,
            ensure_ascii=False,
            allow_nan=False,
            separators=(",", ":"),
        ).encode("utf-8")
        assert _event_to_json_bytes(_event) == _expected_event_bytes
        assert _event_from_json_bytes(_expected_event_bytes) == _event

        _saw_uuid |= type(_event.run_id) is _UUID
        _saw_scope_and_causality |= bool(_event.caused_by) and any(
            value is not None
            for value in (
                _event.scope.scene_id,
                _event.scope.actor_id,
                _event.scope.cue_id,
                _event.scope.effect_id,
                _event.scope.act_id,
                _event.scope.tool_call_id,
                _event.scope.session_generation,
            )
        )
        if isinstance(_event, ContextUsageSampled):
            _saw_decimal |= type(_event.cumulative_cost_amount) is _Decimal
        if isinstance(_event, ActTokenUsageFinalized):
            if _event.provider_total_tokens is not None:
                _saw_arbitrary_token_integer |= _event.provider_total_tokens > 2**64
        if isinstance(_event, SpanStarted) and _event.span_kind == "tool.call":
            _tool_details.append(_event.detail)
        if isinstance(_event, InstantOccurred) and _event.instant_kind == "tool.updated":
            _tool_details.append(_event.detail)

assert _seen_kinds == {value.kind for value in _expected_variants}
assert _saw_uuid
assert _saw_decimal
assert _saw_arbitrary_token_integer
assert _saw_scope_and_causality
assert _tool_details
for _detail in _tool_details:
    assert type(_detail) is ToolCallDetail
    assert _detail.captured_input is None
    assert _detail.captured_output is None
"#,
    );
}

#[test]
fn event_and_nested_value_classes_are_frozen_slotted_keyword_only_and_final() {
    with_fresh_events_module(
        cr#"
import inspect as _inspect
from dataclasses import FrozenInstanceError as _FrozenInstanceError, is_dataclass as _is_dataclass
from decimal import Decimal as _Decimal
from uuid import UUID as _UUID

_public_dataclasses = tuple(
    value
    for name, value in tuple(globals().items())
    if not name.startswith("_")
    and isinstance(value, type)
    and _is_dataclass(value)
)
assert _public_dataclasses
for _class in _public_dataclasses:
    assert _class.__dataclass_params__.frozen
    assert "__slots__" in _class.__dict__
    assert all(
        parameter.kind is _inspect.Parameter.KEYWORD_ONLY
        for parameter in _inspect.signature(_class).parameters.values()
    )
    try:
        type(f"Extended{_class.__name__}", (_class,), {})
    except TypeError:
        pass
    else:
        raise AssertionError(f"{_class.__name__} accepted subclassing")

_scope = DiagnosticScope(
    scene_id="scene-1",
    actor_id="actor-1",
    cue_id="cue-1",
    effect_id=None,
    act_id="act-1",
    tool_call_id=None,
    session_generation=1,
)
_event = CounterSampled(
    schema_version=1,
    run_id=_UUID("12345678-1234-4234-9234-123456789abc"),
    sequence=2,
    elapsed_ns=3,
    scope=_scope,
    caused_by=(CausalLink(source_sequence=1, relation="dispatch"),),
    counter_kind="cue.active",
    value=0,
)
assert not hasattr(_event, "__dict__")
try:
    _event.value = 4
except _FrozenInstanceError:
    pass
else:
    raise AssertionError("event was mutable")
try:
    CounterSampled(
        kind="agent_message_delta",
        schema_version=1,
        run_id=_event.run_id,
        sequence=2,
        elapsed_ns=3,
        scope=_scope,
        caused_by=(),
        counter_kind="cue.active",
        value=0,
    )
except TypeError:
    pass
else:
    raise AssertionError("event kind discriminator was forgeable")

for _forbidden in (
    "ActDiagnosticEvent",
    "StreamReady",
    "Heartbeat",
    "DeliveryGap",
    "ResyncRequired",
    "StreamClosed",
):
    assert _forbidden not in globals()
assert not any(name.endswith("V1") for name in globals())
"#,
    );
}

#[test]
fn frozen_tool_payloads_are_recursive_canonical_and_do_not_call_object_hooks() {
    with_fresh_events_module(
        cr#"
from dataclasses import FrozenInstanceError as _FrozenInstanceError
from decimal import Decimal as _Decimal

_payload = FrozenJsonObject(
    entries=(
        ("a", FrozenJsonArray(items=(None, False, 3, _Decimal("1.25"), "text"))),
        ("b", FrozenJsonObject(entries=(("nested", True),))),
    ),
)
_captured_input = DiagnosticToolInput(raw_input=_payload, truncated=False)
_captured_output = DiagnosticToolOutput(
    raw_output=None,
    content=(FrozenJsonArray(items=("chunk",)),),
    locations=(DiagnosticToolLocation(path="src/main.py", line=7),),
    truncated=True,
)
assert _captured_input.raw_input is _payload
assert _captured_output.truncated is True
assert _captured_output.content[0].items == ("chunk",)
assert _captured_output.locations[0].line == 7

for _value, _field, _replacement in (
    (_payload, "entries", ()),
    (_captured_input, "truncated", True),
    (_captured_output.locations[0], "line", 8),
):
    try:
        setattr(_value, _field, _replacement)
    except _FrozenInstanceError:
        pass
    else:
        raise AssertionError(f"{type(_value).__name__} was mutable")

def _raises(error, callable, *args, **kwargs):
    try:
        callable(*args, **kwargs)
    except error:
        return
    raise AssertionError(f"{callable!r} did not raise {error.__name__}")

_raises(TypeError, FrozenJsonArray, items=[])
_raises(TypeError, FrozenJsonObject, entries={"a": 1})
_raises(ValueError, FrozenJsonObject, entries=(("b", 1), ("a", 2)))
_raises(ValueError, FrozenJsonObject, entries=(("a", 1), ("a", 2)))
_raises(ValueError, FrozenJsonArray, items=(_Decimal("NaN"),))
_raises(TypeError, DiagnosticToolOutput, raw_output=None, content=[], locations=(), truncated=False)
_raises(TypeError, DiagnosticToolLocation, path="x", line=True)

class _Hostile(dict):
    calls = 0
    def items(self):
        type(self).calls += 1
        raise AssertionError("object hook was called")
    def __iter__(self):
        type(self).calls += 1
        raise AssertionError("object hook was called")

_raises(TypeError, _freeze_json, _Hostile(a=1))
assert _Hostile.calls == 0
_frozen = _freeze_json({"z": [1, {"b": 2, "a": _Decimal("3.5")}], "a": None})
assert type(_frozen) is FrozenJsonObject
assert tuple(key for key, _ in _frozen.entries) == ("a", "z")
assert tuple(key for key, _ in _frozen.entries[1][1].items[1].entries) == ("a", "b")

# None means capture was not requested/available. A present truncated wrapper is distinct.
assert None is not DiagnosticToolInput(raw_input=None, truncated=True)
assert None is not DiagnosticToolOutput(
    raw_output=None,
    content=(),
    locations=(),
    truncated=True,
)
"#,
    );
}
