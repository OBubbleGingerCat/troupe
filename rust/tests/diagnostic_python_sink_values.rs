use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};

use pyo3::prelude::*;

#[path = "../src/diagnostic_python/events.rs"]
mod events;
#[path = "../src/diagnostic_python/fragment_test_support.rs"]
mod fragment_test_support;
#[path = "../src/diagnostic_python/sink.rs"]
mod sink;

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("rust manifest must have a repository parent")
        .to_path_buf()
}

fn combined_source() -> CString {
    let mut source = events::source().to_bytes().to_vec();
    source.push(b'\n');
    source.extend_from_slice(sink::source().to_bytes());
    CString::new(source).expect("diagnostic fragments must not contain embedded NUL bytes")
}

fn with_fresh_sink_module(test: &CStr) {
    pyo3::Python::initialize();
    Python::attach(|py| {
        let source = combined_source();
        let module = fragment_test_support::install_fresh_fragment(
            py,
            &source,
            c"diagnostic-sink.py",
            c"_troupe_diagnostic_sink_test",
        )
        .expect("install diagnostic event and sink fragments");
        module
            .setattr(
                "CAPTURE_MATRIX_PATH",
                repository_root().join("tests/fixtures/diagnostics/sink-capture-matrix.json"),
            )
            .expect("publish capture matrix path");
        let namespace = module.dict();
        py.run(test, Some(&namespace), Some(&namespace))
            .expect("diagnostic Python sink assertion failed");
    });
}

#[test]
fn capture_is_frozen_final_keyword_only_and_strict() {
    with_fresh_sink_module(
        cr#"
import inspect as _inspect
from dataclasses import FrozenInstanceError as _FrozenInstanceError, fields as _fields

_expected_fields = (
    "agent_messages",
    "plans",
    "tool_calls",
    "result_validation",
    "usage",
    "custom_events",
    "tool_inputs",
    "tool_outputs",
)
_capture = DiagnosticCapture()
assert tuple(field.name for field in _fields(DiagnosticCapture)) == _expected_fields
assert tuple(getattr(_capture, field) for field in _expected_fields) == (
    True, True, True, True, True, True, False, False,
)
assert all(
    parameter.kind is _inspect.Parameter.KEYWORD_ONLY
    for parameter in _inspect.signature(DiagnosticCapture).parameters.values()
)
assert DiagnosticCapture.__dataclass_params__.frozen
assert "__slots__" in DiagnosticCapture.__dict__
assert not hasattr(_capture, "__dict__")

try:
    _capture.usage = False
except _FrozenInstanceError:
    pass
else:
    raise AssertionError("DiagnosticCapture was mutable")

try:
    class _ExtendedCapture(DiagnosticCapture):
        pass
except TypeError:
    pass
else:
    raise AssertionError("DiagnosticCapture accepted subclassing")

for _field in _expected_fields:
    for _invalid in (0, 1, None, "true"):
        try:
            DiagnosticCapture(**{_field: _invalid})
        except TypeError:
            pass
        else:
            raise AssertionError(f"{_field} accepted {_invalid!r}")

for _field in ("tool_inputs", "tool_outputs"):
    try:
        DiagnosticCapture(tool_calls=False, **{_field: True})
    except ValueError:
        pass
    else:
        raise AssertionError(f"{_field} did not require tool_calls")

assert DiagnosticCapture(tool_calls=False, tool_inputs=False, tool_outputs=False).tool_calls is False
"#,
    );
}

#[test]
fn checked_capture_matrix_is_closed_and_matches_d34() {
    with_fresh_sink_module(
        cr#"
import json as _json
from pathlib import Path as _Path

_raw = _Path(CAPTURE_MATRIX_PATH).read_bytes()
assert _raw.endswith(b"\n")
_matrix = _json.loads(_raw)
assert tuple(_matrix) == (
    "schema",
    "capture_fields",
    "dependencies",
    "scope_policy",
    "variant_rules",
    "span_kind_rules",
    "instant_kind_rules",
    "counter_kind_rules",
)
assert _matrix["schema"] == "troupe.diagnostics.sink-capture-matrix"
_capture_fields = (
    "agent_messages",
    "plans",
    "tool_calls",
    "result_validation",
    "usage",
    "custom_events",
    "tool_inputs",
    "tool_outputs",
)
assert tuple(_matrix["capture_fields"]) == _capture_fields
assert _matrix["dependencies"] == [
    {
        "capture": "tool_inputs",
        "requires": "tool_calls",
        "controls_delivery": False,
        "payload_field": "captured_input",
    },
    {
        "capture": "tool_outputs",
        "requires": "tool_calls",
        "controls_delivery": False,
        "payload_field": "captured_output",
    },
]
assert _matrix["scope_policy"] == {
    "delivered_events": "current_act_only",
    "observation_gap": "current_act_impact_only",
    "sink_targeted_component_failure": "always_excluded",
}

def _closed_rules(name, expected_kinds, allowed_capture):
    rules = _matrix[name]
    assert all(type(rule) is dict and tuple(rule) == ("kind", "capture") for rule in rules)
    assert len(rules) == len({rule["kind"] for rule in rules})
    assert {rule["kind"] for rule in rules} == set(expected_kinds)
    assert {rule["capture"] for rule in rules} <= set(allowed_capture)
    return {rule["kind"]: rule["capture"] for rule in rules}

_variants = _closed_rules(
    "variant_rules",
    _EVENT_KINDS,
    (
        "by_span_kind",
        "by_instant_kind",
        "by_counter_kind",
        "always",
        "agent_messages",
        "plans",
        "usage",
        "custom_events",
    ),
)
assert _variants["span_started"] == _variants["span_finished"] == "by_span_kind"
assert _variants["instant_occurred"] == "by_instant_kind"
assert _variants["counter_sampled"] == "by_counter_kind"
assert _variants["observation_gap"] == "always"
assert _variants["agent_message_delta"] == "agent_messages"
assert _variants["agent_message_completed"] == "agent_messages"
assert _variants["agent_plan_snapshot"] == "plans"
assert _variants["context_usage_sampled"] == "usage"
assert _variants["act_token_usage_finalized"] == "usage"

_spans = _closed_rules(
    "span_kind_rules",
    _SPAN_KINDS,
    ("always", "agent_messages", "tool_calls", "excluded"),
)
assert {kind for kind, capture in _spans.items() if capture == "always"} == {
    "act.lifecycle", "act.caller", "agent.turn",
}
assert _spans["agent.thinking"] == "agent_messages"
assert _spans["tool.call"] == "tool_calls"

_instants = _closed_rules(
    "instant_kind_rules",
    _INSTANT_KINDS,
    ("always", "tool_calls", "result_validation", "excluded"),
)
assert {kind for kind, capture in _instants.items() if capture == "always"} == {
    "act.admitted",
    "act.waiting_ready",
    "act.prompt_submitted",
    "act.cancel_requested",
    "act.supervisor_handoff",
    "agent.turn.activity",
    "agent.turn.terminal",
    "agent.turn.settled",
}
assert {kind for kind, capture in _instants.items() if capture == "result_validation"} == {
    "result.submitted",
    "result.rejected",
    "result.repair_requested",
    "result.accepted",
    "result.missing",
}
assert _instants["tool.updated"] == "tool_calls"
assert _instants["diagnostic.component_failed"] == "excluded"

_counters = _closed_rules(
    "counter_kind_rules",
    _COUNTER_KINDS,
    ("always", "result_validation", "excluded"),
)
assert {kind for kind, capture in _counters.items() if capture == "always"} == {
    "agent.turn.active", "diagnostic.dropped_events",
}
assert _counters["result.validation_rejections"] == "result_validation"
assert _counters["actor.mailbox_depth"] == "excluded"
assert _counters["cue.active"] == "excluded"

_matrix_text = _raw.decode("utf-8")
for _forbidden in (
    '"context"',
    '"thinking"',
    "submitted_result",
    "invalid_result",
    "validated_result",
):
    assert _forbidden not in _matrix_text
"#,
    );
}

#[test]
fn sink_is_an_initialized_abc_with_one_event_callback_and_repeatable_wait() {
    with_fresh_sink_module(
        cr#"
import asyncio as _asyncio
import inspect as _inspect
import threading as _threading

assert _inspect.isabstract(DiagnosticSink)
assert tuple(_inspect.signature(DiagnosticSink).parameters) == ("capture",)
assert _inspect.signature(DiagnosticSink).parameters["capture"].kind is _inspect.Parameter.KEYWORD_ONLY
assert _inspect.signature(DiagnosticSink).parameters["capture"].default is None
_callback_parameters = tuple(_inspect.signature(DiagnosticSink.on_event).parameters.values())
assert tuple(parameter.name for parameter in _callback_parameters) == ("self", "event")
assert all(parameter.kind is _inspect.Parameter.POSITIONAL_ONLY for parameter in _callback_parameters)
assert tuple(_inspect.signature(DiagnosticSink.wait_closed).parameters) == ("self",)
assert _inspect.iscoroutinefunction(DiagnosticSink.wait_closed)
assert not hasattr(DiagnosticSink, "close")
assert not hasattr(DiagnosticSink, "force_close")

class _Sink(DiagnosticSink):
    __slots__ = ("events",)

    def __init__(self, *, capture=None):
        super().__init__(capture=capture)
        self.events = []

    def on_event(self, event, /):
        self.events.append(event)

class _MissingSuper(DiagnosticSink):
    def __init__(self):
        pass

    def on_event(self, event, /):
        pass

def _assert_state_error(awaitable, code):
    try:
        _asyncio.run(awaitable)
    except DiagnosticSinkStateError as error:
        assert error.code == code
        assert error.args == (code,)
    else:
        raise AssertionError(f"expected DiagnosticSinkStateError({code!r})")

_missing = _MissingSuper()
try:
    _ = _missing.state
except DiagnosticSinkStateError as error:
    assert error.code == "uninitialized"
else:
    raise AssertionError("missing super initializer was accepted")
_assert_state_error(_missing.wait_closed(), "uninitialized")

_capture = DiagnosticCapture(plans=False)
_sink = _Sink(capture=_capture)
assert _sink.capture is _capture
assert _sink.state == "UNBOUND"
_assert_state_error(_sink.wait_closed(), "unbound")

for _invalid_capture in (False, object()):
    try:
        _Sink(capture=_invalid_capture)
    except TypeError:
        pass
    else:
        raise AssertionError("DiagnosticSink accepted invalid capture")

try:
    DiagnosticSink()
except TypeError:
    pass
else:
    raise AssertionError("DiagnosticSink ABC was directly instantiated")

_summary = DiagnosticSinkSummary(
    run_id=_UUID("12345678-1234-4234-9234-123456789abc"),
    act_id="act-1",
    act_outcome="completed",
    close_reason="act_finished",
    complete=True,
    delivered_events=0,
    first_delivered_sequence=None,
    last_delivered_sequence=None,
    dropped_events=0,
    dropped_bytes=0,
    dropped_by_kind=(),
    source_gaps=0,
    truncated_payloads=0,
    callback_failure=None,
    callback_abandoned=False,
)

_sink._diagnostic_bind()
assert _sink.state == "BOUND"
try:
    _sink._diagnostic_bind()
except DiagnosticSinkStateError as error:
    assert error.code == "already_bound"
else:
    raise AssertionError("sink accepted a second binding")

async def _cancel_one_waiter():
    _cancelled_waiter = _asyncio.create_task(_sink.wait_closed())
    await _asyncio.sleep(0)
    _cancelled_waiter.cancel()
    try:
        await _cancelled_waiter
    except _asyncio.CancelledError:
        pass
    else:
        raise AssertionError("waiter cancellation did not propagate")
    assert _sink.state == "BOUND"

_asyncio.run(_cancel_one_waiter())
_sink._diagnostic_seal()
assert _sink.state == "SEALED"
_sink._diagnostic_close(_summary)
assert _sink.state == "CLOSED"

async def _repeat_wait():
    assert await _sink.wait_closed() is _summary
    assert await _sink.wait_closed() is _summary

_asyncio.run(_repeat_wait())

_cross_thread_sink = _Sink()
_cross_thread_sink._diagnostic_bind()
_cross_thread_sink._diagnostic_seal()
_cross_thread_result = []

def _wait_from_another_thread():
    _cross_thread_result.append(_asyncio.run(_cross_thread_sink.wait_closed()))

_thread = _threading.Thread(target=_wait_from_another_thread)
_thread.start()
_cross_thread_sink._diagnostic_close(_summary)
_thread.join(timeout=5)
assert not _thread.is_alive()
assert _cross_thread_result == [_summary]
"#,
    );
}

#[test]
fn delivery_values_are_closed_immutable_and_consistent() {
    with_fresh_sink_module(
        cr#"
import inspect as _inspect
from dataclasses import FrozenInstanceError as _FrozenInstanceError, fields as _fields

_failure = DiagnosticCallbackFailure(
    kind="raised",
    event_sequence=9,
    exception_type="RuntimeError",
    message="callback failed",
    message_truncated=False,
)
_drop = DiagnosticDropCount(
    event_kind="agent_message_delta",
    events=2,
    encoded_bytes=64,
)
_summary = DiagnosticSinkSummary(
    run_id=_UUID("12345678-1234-4234-9234-123456789abc"),
    act_id="act-1",
    act_outcome="failed",
    close_reason="callback_failed",
    complete=False,
    delivered_events=3,
    first_delivered_sequence=1,
    last_delivered_sequence=9,
    dropped_events=2,
    dropped_bytes=64,
    dropped_by_kind=(_drop,),
    source_gaps=1,
    truncated_payloads=1,
    callback_failure=_failure,
    callback_abandoned=False,
)
assert tuple(field.name for field in _fields(DiagnosticCallbackFailure)) == (
    "kind", "event_sequence", "exception_type", "message", "message_truncated",
)
assert tuple(field.name for field in _fields(DiagnosticDropCount)) == (
    "event_kind", "events", "encoded_bytes",
)
_summary_fields = tuple(field.name for field in _fields(DiagnosticSinkSummary))
assert _summary_fields == (
    "run_id",
    "act_id",
    "act_outcome",
    "close_reason",
    "complete",
    "delivered_events",
    "first_delivered_sequence",
    "last_delivered_sequence",
    "dropped_events",
    "dropped_bytes",
    "dropped_by_kind",
    "source_gaps",
    "truncated_payloads",
    "callback_failure",
    "callback_abandoned",
)
assert not any("token" in field or "usage" in field or "pointer" in field for field in _summary_fields)
assert _summary.callback_failure is _failure
assert _summary.dropped_by_kind == (_drop,)

for _class in (DiagnosticCallbackFailure, DiagnosticDropCount, DiagnosticSinkSummary):
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

for _value, _field, _replacement in (
    (_failure, "message", None),
    (_drop, "events", 0),
    (_summary, "complete", True),
):
    assert not hasattr(_value, "__dict__")
    try:
        setattr(_value, _field, _replacement)
    except _FrozenInstanceError:
        pass
    else:
        raise AssertionError(f"{type(_value).__name__} was mutable")

def _raises(error, callable, **kwargs):
    try:
        callable(**kwargs)
    except error:
        return
    raise AssertionError(f"{callable.__name__} accepted {kwargs!r}")

_raises(
    ValueError,
    DiagnosticCallbackFailure,
    kind="unknown",
    event_sequence=1,
    exception_type=None,
    message=None,
    message_truncated=False,
)
_raises(
    TypeError,
    DiagnosticCallbackFailure,
    kind="raised",
    event_sequence=1,
    exception_type=None,
    message=None,
    message_truncated=0,
)
_raises(
    ValueError,
    DiagnosticDropCount,
    event_kind="unknown_event",
    events=0,
    encoded_bytes=0,
)
_raises(
    TypeError,
    DiagnosticDropCount,
    event_kind="agent_message_delta",
    events=False,
    encoded_bytes=0,
)

_valid = dict(
    run_id=_UUID("12345678-1234-4234-9234-123456789abc"),
    act_id="act-1",
    act_outcome="completed",
    close_reason="act_finished",
    complete=True,
    delivered_events=0,
    first_delivered_sequence=None,
    last_delivered_sequence=None,
    dropped_events=0,
    dropped_bytes=0,
    dropped_by_kind=(),
    source_gaps=0,
    truncated_payloads=0,
    callback_failure=None,
    callback_abandoned=False,
)
for _changes, _error in (
    ({"act_outcome": "unknown"}, ValueError),
    ({"close_reason": "unknown"}, ValueError),
    ({"complete": 1}, TypeError),
    ({"delivered_events": -1}, ValueError),
    ({"delivered_events": 1}, ValueError),
    ({"dropped_events": 1}, ValueError),
    ({"dropped_by_kind": []}, TypeError),
    ({"source_gaps": 1}, ValueError),
    ({"truncated_payloads": 1}, ValueError),
    ({"callback_abandoned": True}, ValueError),
):
    _arguments = _valid | _changes
    _raises(_error, DiagnosticSinkSummary, **_arguments)

_raises(
    ValueError,
    DiagnosticSinkSummary,
    **(_valid | {
        "close_reason": "callback_failed",
        "complete": False,
        "callback_failure": None,
    }),
)
"#,
    );
}
