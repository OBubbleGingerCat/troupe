use std::ffi::{CStr, CString};

use pyo3::prelude::*;

#[path = "../src/diagnostic_python/custom.rs"]
mod custom;
#[path = "../src/diagnostic_python/events.rs"]
mod events;
#[path = "../src/diagnostic_python/fragment_test_support.rs"]
mod fragment_test_support;

fn combined_source() -> CString {
    let mut source = events::source().to_bytes().to_vec();
    source.push(b'\n');
    source.extend_from_slice(custom::source().to_bytes());
    CString::new(source).expect("diagnostic fragments must not contain embedded NUL bytes")
}

fn with_fresh_custom_module(test: &CStr) {
    pyo3::Python::initialize();
    Python::attach(|py| {
        let source = combined_source();
        let module = fragment_test_support::install_fresh_fragment(
            py,
            &source,
            c"diagnostic-custom.py",
            c"_troupe_diagnostic_custom_test",
        )
        .expect("install diagnostic event and custom fragments");
        let namespace = module.dict();
        py.run(test, Some(&namespace), Some(&namespace))
            .expect("diagnostic Python custom assertion failed");
    });
}

#[test]
fn public_signatures_return_shapes_and_eager_candidates_are_frozen() {
    with_fresh_custom_module(
        cr#"
import inspect as _inspect
from collections import UserDict as _UserDict
from dataclasses import FrozenInstanceError as _FrozenInstanceError, fields as _fields
from decimal import Decimal as _Decimal
from types import NoneType as _NoneType
from typing import get_args as _get_args, get_origin as _get_origin

_event_signature = _inspect.signature(event)
assert tuple(_event_signature.parameters) == ("name", "severity", "attributes")
assert _event_signature.parameters["name"].kind is _inspect.Parameter.POSITIONAL_ONLY
assert _event_signature.parameters["severity"].kind is _inspect.Parameter.KEYWORD_ONLY
assert _event_signature.parameters["severity"].default == "info"
assert _event_signature.parameters["attributes"].kind is _inspect.Parameter.KEYWORD_ONLY
assert _event_signature.parameters["attributes"].default is None
assert _event_signature.parameters["name"].annotation is str
assert _event_signature.parameters["severity"].annotation == _Literal[
    "debug", "info", "warning", "error"
]
assert _event_signature.parameters["attributes"].annotation == (
    _Mapping[str, DiagnosticAttributeValue] | None
)
assert _event_signature.return_annotation is None

_counter_signature = _inspect.signature(counter)
assert tuple(_counter_signature.parameters) == ("name", "value", "unit", "dimensions")
assert all(
    _counter_signature.parameters[name].kind is _inspect.Parameter.POSITIONAL_ONLY
    for name in ("name", "value")
)
assert all(
    _counter_signature.parameters[name].kind is _inspect.Parameter.KEYWORD_ONLY
    for name in ("unit", "dimensions")
)
assert _counter_signature.parameters["unit"].default is None
assert _counter_signature.parameters["dimensions"].default is None
assert _counter_signature.parameters["name"].annotation is str
assert _counter_signature.parameters["value"].annotation == (int | float | _Decimal)
assert _counter_signature.parameters["unit"].annotation == (str | None)
assert _counter_signature.parameters["dimensions"].annotation == (
    _Mapping[str, DiagnosticDimension] | None
)
assert _counter_signature.return_annotation is None

_span_signature = _inspect.signature(span)
assert tuple(_span_signature.parameters) == ("name", "attributes")
assert _span_signature.parameters["name"].kind is _inspect.Parameter.POSITIONAL_ONLY
assert _span_signature.parameters["attributes"].kind is _inspect.Parameter.KEYWORD_ONLY
assert _span_signature.parameters["attributes"].default is None
assert _span_signature.parameters["name"].annotation is str
assert _span_signature.parameters["attributes"].annotation == (
    _Mapping[str, DiagnosticAttributeValue] | None
)
assert _span_signature.return_annotation == _AbstractContextManager[None]
assert not any(_inspect.iscoroutinefunction(function) for function in (event, counter, span))
assert "increment" not in globals()

_scalar_args = _get_args(DiagnosticScalar)
assert _scalar_args == (_NoneType, bool, int, float, _Decimal, str)
_attribute_args = _get_args(DiagnosticAttributeValue)
assert _attribute_args[:6] == _scalar_args
assert _get_origin(_attribute_args[6]) is list
assert _get_origin(_attribute_args[7]) is tuple
assert _get_args(DiagnosticDimension) == (bool, int, float, _Decimal, str)

_admitted = []
def _fake_admission(candidate):
    _admitted.append(candidate)

_set_custom_admission_hook(_fake_admission)
_attributes = _UserDict({
    "z": [None, False, 7, 0.5, _Decimal("1.2500"), "text"],
    "a": "first",
    "password": "not-redacted",
})
assert event("orders.accepted", severity="warning", attributes=_attributes) is None
_attributes["a"] = "mutated"
_attributes["z"].append("later")
_attributes["new"] = 1

_dimensions = {"zone": "east", "attempt": 2}
assert counter("orders.pending", 10, unit="items", dimensions=_dimensions) is None
_dimensions["attempt"] = 99

assert len(_admitted) == 2
_instant, _counter = _admitted
assert type(_instant) is _CustomInstantCandidate
assert _instant.kind == "custom_instant_occurred"
assert _instant.name == "orders.accepted"
assert _instant.severity == "warning"
assert tuple(key for key, _ in _instant.attributes) == ("a", "password", "z")
assert dict(_instant.attributes)["a"] == "first"
assert dict(_instant.attributes)["password"] == "not-redacted"
assert dict(_instant.attributes)["z"] == (
    None, False, 7, _Decimal("0.5"), _Decimal("1.25"), "text",
)

assert type(_counter) is _CustomCounterCandidate
assert _counter.kind == "custom_counter_sampled"
assert _counter.name == "orders.pending"
assert _counter.value == 10 and type(_counter.value) is int
assert _counter.unit == "items"
assert _counter.dimensions == (("attempt", 2), ("zone", "east"))

for _candidate, _expected_fields in (
    (_instant, ("name", "severity", "attributes")),
    (_counter, ("name", "value", "unit", "dimensions")),
):
    _class = type(_candidate)
    assert _class.__dataclass_params__.frozen
    assert "__slots__" in _class.__dict__
    assert not hasattr(_candidate, "__dict__")
    assert tuple(field.name for field in _fields(_class)) == _expected_fields
    assert not {
        "run_id", "sequence", "elapsed_ns", "scope", "parent_span_id",
        "containing_span_id", "caused_by",
    } & set(_expected_fields)
    try:
        type(f"Extended{_class.__name__}", (_class,), {})
    except TypeError:
        pass
    else:
        raise AssertionError(f"{_class.__name__} accepted subclassing")

try:
    _instant.name = "changed.value"
except _FrozenInstanceError:
    pass
else:
    raise AssertionError("custom candidate was mutable")
"#,
    );
}

#[test]
fn scalar_normalization_series_identity_and_error_families_are_exact() {
    with_fresh_custom_module(
        cr#"
from collections.abc import Mapping as _Mapping
from decimal import Decimal as _Decimal

_admitted = []
_set_custom_admission_hook(_admitted.append)
_huge_integer = 10 ** 5000
event(
    "numbers.observed",
    attributes={
        "decimal": _Decimal("1.2300"),
        "float": 0.1,
        "integer": _huge_integer,
        "list": [1.0, _Decimal("2.500"), -0.0],
        "negative_zero": -0.0,
    },
)
_values = dict(_admitted[-1].attributes)
assert _values["decimal"] == _Decimal("1.23")
assert type(_values["decimal"]) is _Decimal
assert _values["float"] == _Decimal("0.1")
assert type(_values["float"]) is _Decimal
assert _values["integer"] == _huge_integer
assert type(_values["integer"]) is int
assert _values["negative_zero"] == _Decimal("0")
assert _values["list"] == (_Decimal("1"), _Decimal("2.5"), _Decimal("0"))

counter("numbers.arbitrary_integer", _huge_integer)
assert _admitted[-1].value == _huge_integer
assert type(_admitted[-1].value) is int

counter(
    "numbers.gauge",
    0.1,
    dimensions={"ratio": 0.5, "exact": _Decimal("0.500"), "count": 1},
)
_float_counter = _admitted[-1]
assert _float_counter.value == _Decimal("0.1")
assert type(_float_counter.value) is _Decimal
assert _float_counter.dimensions == (
    ("count", 1),
    ("exact", _Decimal("0.5")),
    ("ratio", _Decimal("0.5")),
)

counter("series.identity", 1, dimensions={"b": 2, "a": 0.5})
counter("series.identity", 1, dimensions={"a": 0.5, "b": 2})
assert _admitted[-2].dimensions == _admitted[-1].dimensions
counter("series.identity", _Decimal("1"), dimensions={"a": 0.5, "b": 2})
assert type(_admitted[-2].value) is int
assert type(_admitted[-1].value) is _Decimal
assert _admitted[-2].value == _admitted[-1].value

class _ListSubclass(list):
    pass

class _StringSubclass(str):
    pass

class _MappingValue(_Mapping):
    def __init__(self, values):
        self._values = values
    def __getitem__(self, key):
        return self._values[key]
    def __iter__(self):
        return iter(self._values)
    def __len__(self):
        return len(self._values)

event("mapping.accepted", attributes=_MappingValue({"value": 1}))
assert _admitted[-1].attributes == (("value", 1),)

def _raises(error, callable, *args, **kwargs):
    before = len(_admitted)
    try:
        callable(*args, **kwargs)
    except error:
        assert len(_admitted) == before
        return
    raise AssertionError(f"{callable!r} accepted invalid input")

for _value in (True, False, None, "1", object()):
    _raises(TypeError, counter, "numbers.invalid", _value)
for _value in (float("nan"), float("inf"), float("-inf"), _Decimal("NaN"), _Decimal("Infinity")):
    _raises(ValueError, counter, "numbers.invalid", _value)
    _raises(ValueError, event, "numbers.invalid", attributes={"value": _value})

_raises(TypeError, event, "shape.invalid", attributes=[])
_raises(TypeError, event, "shape.invalid", attributes=(item for item in ()))
_raises(TypeError, event, "shape.invalid", attributes={"value": {"nested": 1}})
_raises(TypeError, event, "shape.invalid", attributes={"value": [[1]]})
_raises(TypeError, event, "shape.invalid", attributes={"value": b"bytes"})
_raises(TypeError, event, "shape.invalid", attributes={"value": {1, 2}})
_raises(TypeError, event, "shape.invalid", attributes={"value": _ListSubclass([1])})
_raises(TypeError, event, "shape.invalid", attributes={"value": _StringSubclass("text")})
_raises(TypeError, counter, "shape.invalid", 1, dimensions={"value": None})
_raises(TypeError, counter, "shape.invalid", 1, dimensions={"value": [1]})
_raises(TypeError, event, "shape.invalid", severity=1)
_raises(ValueError, event, "shape.invalid", severity="fatal")
_raises(TypeError, counter, "shape.invalid", 1, unit=1)
_raises(ValueError, counter, "shape.invalid", 1, unit="")
_raises(TypeError, event, 1)
_raises(ValueError, event, "single")
_raises(ValueError, event, "Upper.case")
_raises(ValueError, event, "troupe.reserved")
_raises(ValueError, event, "unicode.名称")
_raises(ValueError, event, "shape.invalid", attributes={"bad\ud800": 1})
_raises(ValueError, event, "shape.invalid", attributes={"value": "bad\ud800"})
_raises(ValueError, counter, "shape.invalid", 1, unit="bad\ud800")
"#,
    );
}

#[test]
fn every_custom_resource_limit_accepts_below_and_equal_then_rejects_above() {
    with_fresh_custom_module(
        cr#"
import json as _json

_admitted = []
_set_custom_admission_hook(_admitted.append)

def _raises_value_error(callable, *args, **kwargs):
    before = len(_admitted)
    try:
        callable(*args, **kwargs)
    except ValueError:
        assert len(_admitted) == before
        return
    raise AssertionError("resource limit violation was accepted")

for _length in (127, 128):
    _name = "a." + "b" * (_length - 2)
    assert len(_name.encode("ascii")) == _length
    event(_name)
_raises_value_error(event, "a." + "b" * 127)

for _length in (63, 64):
    _key = "k" * _length
    event("limits.key", attributes={_key: 1})
_raises_value_error(event, "limits.key", attributes={"k" * 65: 1})

for _length in (31, 32):
    _unit = "u" * _length
    counter("limits.unit", 1, unit=_unit)
_raises_value_error(counter, "limits.unit", 1, unit="u" * 33)

for _count in (31, 32):
    event("limits.attributes", attributes={f"k{index:02}": index for index in range(_count)})
_raises_value_error(
    event,
    "limits.attributes",
    attributes={f"k{index:02}": index for index in range(33)},
)

for _count in (7, 8):
    counter("limits.dimensions", 1, dimensions={f"k{index}": index for index in range(_count)})
_raises_value_error(
    counter,
    "limits.dimensions",
    1,
    dimensions={f"k{index}": index for index in range(9)},
)

for _count in (63, 64):
    event("limits.list", attributes={"items": list(range(_count))})
_raises_value_error(event, "limits.list", attributes={"items": list(range(65))})

def _encoded_size(payload):
    return len(_json.dumps(
        payload,
        ensure_ascii=False,
        allow_nan=False,
        separators=(",", ":"),
    ).encode("utf-8"))

def _tagged_string(value):
    return {"type": "string", "value": value}

def _payload(kind, text):
    if kind == "event":
        return {
            "name": "limits.payload_event",
            "severity": "info",
            "attributes": {"payload": _tagged_string(text)},
        }
    if kind == "counter":
        return {
            "name": "limits.payload_counter",
            "value": {"type": "integer", "value": "1"},
            "unit": None,
            "dimensions": {"payload": _tagged_string(text)},
        }
    assert kind == "span"
    return {
        "name": "limits.payload_span",
        "attributes": {"payload": _tagged_string(text)},
    }

def _publish(kind, text):
    if kind == "event":
        return event("limits.payload_event", attributes={"payload": text})
    if kind == "counter":
        return counter("limits.payload_counter", 1, dimensions={"payload": text})
    return span("limits.payload_span", attributes={"payload": text})

for _kind in ("event", "counter", "span"):
    _base_size = _encoded_size(_payload(_kind, ""))
    _equal_text = "x" * (65_536 - _base_size)
    assert _encoded_size(_payload(_kind, _equal_text)) == 65_536
    _publish(_kind, _equal_text[:-1])
    _publish(_kind, _equal_text)
    _raises_value_error(_publish, _kind, _equal_text + "x")
"#,
    );
}

#[test]
fn span_context_and_context_errors_admit_exact_pairs_without_suppressing() {
    with_fresh_custom_module(
        cr#"
import asyncio as _asyncio
from dataclasses import FrozenInstanceError as _FrozenInstanceError, fields as _fields

_set_custom_admission_hook(None)
try:
    event("context.missing")
except DiagnosticContextError:
    pass
else:
    raise AssertionError("event without publication context was accepted")

try:
    event("invalid")
except ValueError:
    pass
else:
    raise AssertionError("argument validation did not precede context validation")

_unbound_span = span("context.deferred", attributes={"copied": [1, 2]})
try:
    with _unbound_span:
        pass
except DiagnosticContextError:
    pass
else:
    raise AssertionError("span enter without publication context was accepted")

_admitted = []
def _fake_admission(candidate):
    _admitted.append(candidate)
_set_custom_admission_hook(_fake_admission)

_never_entered = span("span.never_entered", attributes={"value": 1})
assert _admitted == []

_span_attributes = {"copied": [1, 2]}
_completed = span("span.completed", attributes=_span_attributes)
_span_attributes["copied"].append(3)
_span_attributes["later"] = True
with _completed as _entered:
    assert _entered is None

_failure = ValueError("body failed")
try:
    with span("span.failed"):
        raise _failure
except ValueError as _caught:
    assert _caught is _failure
else:
    raise AssertionError("span suppressed a body exception")

_cancellation = _asyncio.CancelledError()
try:
    with span("span.cancelled"):
        raise _cancellation
except _asyncio.CancelledError as _caught:
    assert _caught is _cancellation
else:
    raise AssertionError("span suppressed cancellation")

assert tuple(candidate.kind for candidate in _admitted) == (
    "custom_span_started",
    "custom_span_finished",
    "custom_span_started",
    "custom_span_finished",
    "custom_span_started",
    "custom_span_finished",
)
assert tuple(candidate.outcome for candidate in _admitted if type(candidate) is _CustomSpanFinishCandidate) == (
    "completed", "failed", "cancelled",
)
for _index in (0, 2, 4):
    _candidate = _admitted[_index]
    assert type(_candidate) is _CustomSpanStartCandidate
    assert tuple(field.name for field in _fields(type(_candidate))) == ("name", "attributes")
    assert not hasattr(_candidate, "__dict__")
assert _admitted[0].attributes == (("copied", (1, 2)),)
for _index in (1, 3, 5):
    _candidate = _admitted[_index]
    assert type(_candidate) is _CustomSpanFinishCandidate
    assert tuple(field.name for field in _fields(type(_candidate))) == ("outcome",)

try:
    _admitted[1].outcome = "failed"
except _FrozenInstanceError:
    pass
else:
    raise AssertionError("span finish candidate was mutable")

_before_reuse = len(_admitted)
try:
    with _completed:
        pass
except RuntimeError:
    pass
else:
    raise AssertionError("a completed context manager was reused")
assert len(_admitted) == _before_reuse

def _invalid_bridge(candidate):
    return object()
_set_custom_admission_hook(_invalid_bridge)
try:
    event("bridge.invalid_return")
except RuntimeError:
    pass
else:
    raise AssertionError("private admission bridge returned an identity")

assert issubclass(DiagnosticContextError, RuntimeError)
try:
    class _ExtendedContextError(DiagnosticContextError):
        pass
except TypeError:
    pass
else:
    raise AssertionError("DiagnosticContextError accepted subclassing")
"#,
    );
}
