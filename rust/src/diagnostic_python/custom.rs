use std::ffi::CStr;

const SOURCE: &CStr = cr#"
import asyncio as _asyncio
import math as _math
from collections.abc import Mapping as _Mapping
from contextlib import AbstractContextManager as _AbstractContextManager


_MAX_CUSTOM_PAYLOAD_BYTES = 65_536
_custom_admission_hook = None

DiagnosticScalar: _TypeAlias = _NoneType | bool | int | float | _Decimal | str
DiagnosticAttributeValue: _TypeAlias = (
    DiagnosticScalar | list[DiagnosticScalar] | tuple[DiagnosticScalar, ...]
)
DiagnosticDimension: _TypeAlias = bool | int | float | _Decimal | str


@_final
class DiagnosticContextError(RuntimeError):
    __slots__ = ()


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class _CustomInstantCandidate:
    kind: _ClassVar[_Literal["custom_instant_occurred"]] = "custom_instant_occurred"
    name: str
    severity: str
    attributes: DiagnosticAttributes


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class _CustomCounterCandidate:
    kind: _ClassVar[_Literal["custom_counter_sampled"]] = "custom_counter_sampled"
    name: str
    value: int | _Decimal
    unit: str | None
    dimensions: DiagnosticDimensions


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class _CustomSpanStartCandidate:
    kind: _ClassVar[_Literal["custom_span_started"]] = "custom_span_started"
    name: str
    attributes: DiagnosticAttributes


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class _CustomSpanFinishCandidate:
    kind: _ClassVar[_Literal["custom_span_finished"]] = "custom_span_finished"
    outcome: str


def _utf8_length(value, field):
    try:
        return len(value.encode("utf-8"))
    except UnicodeEncodeError as error:
        raise ValueError(f"{field} must be valid UTF-8 text") from error


def _normalize_custom_scalar(value, *, allow_none=True):
    if value is None:
        if allow_none:
            return None
        raise TypeError("dimension values cannot be None")
    if type(value) in (bool, int):
        return value
    if type(value) is float:
        if not _math.isfinite(value):
            raise ValueError("custom float value must be finite")
        return _normalize_decimal(_Decimal(repr(value)))
    if type(value) is _Decimal:
        return _normalize_decimal(value)
    if type(value) is str:
        _utf8_length(value, "custom string")
        return value
    raise TypeError("custom scalar has an unsupported type")


def _normalize_custom_attribute(value):
    if type(value) in (list, tuple):
        if len(value) > _MAX_CUSTOM_LIST_ITEMS:
            raise ValueError("custom scalar list is too long")
        return tuple(_normalize_custom_scalar(item) for item in value)
    return _normalize_custom_scalar(value)


def _normalize_custom_entries(values, *, dimensions=False):
    if values is None:
        return ()
    if not isinstance(values, _Mapping):
        raise TypeError("custom attributes and dimensions must be mappings or None")
    maximum = _MAX_CUSTOM_DIMENSIONS if dimensions else _MAX_CUSTOM_ATTRIBUTES
    if len(values) > maximum:
        raise ValueError("too many custom dimensions" if dimensions else "too many custom attributes")

    entries = []
    seen = set()
    for entry in values.items():
        if type(entry) is not tuple or len(entry) != 2:
            raise TypeError("custom mapping items must be exact pair tuples")
        key, value = entry
        _require_exact(key, str, "custom key")
        key_bytes = _utf8_length(key, "custom key")
        if not key or key_bytes > _MAX_CUSTOM_KEY_BYTES:
            raise ValueError("custom key is out of bounds")
        if key in seen:
            raise ValueError("custom keys must be unique")
        seen.add(key)
        normalized = (
            _normalize_custom_scalar(value, allow_none=False)
            if dimensions
            else _normalize_custom_attribute(value)
        )
        entries.append((key.encode("utf-8"), key, normalized))
        if len(entries) > maximum:
            raise ValueError("too many custom dimensions" if dimensions else "too many custom attributes")
    return tuple((key, value) for _, key, value in sorted(entries, key=lambda entry: entry[0]))


def _normalize_custom_number(value):
    if type(value) is int:
        return value
    if type(value) in (float, _Decimal):
        return _normalize_custom_scalar(value, allow_none=False)
    raise TypeError("custom counter value must be an exact int, float, or Decimal")


def _normalize_custom_unit(value):
    if value is None:
        return None
    _require_exact(value, str, "unit")
    length = _utf8_length(value, "unit")
    if not value or length > _MAX_CUSTOM_UNIT_BYTES:
        raise ValueError("custom counter unit is out of bounds")
    return value


def _custom_integer_text(value):
    if type(value) is not int:
        raise TypeError("custom integer must be an exact int")
    return format(_Decimal(value), "f")


def _encode_custom_input_scalar(value, *, allow_none=True):
    if value is None:
        if not allow_none:
            raise TypeError("dimension value cannot be None")
        return {"type": "null"}
    if type(value) is bool:
        return {"type": "boolean", "value": value}
    if type(value) is int:
        return {"type": "integer", "value": _custom_integer_text(value)}
    if type(value) is _Decimal:
        return {"type": "decimal", "value": _decimal_text(value)}
    if type(value) is str:
        return {"type": "string", "value": value}
    raise TypeError("normalized custom scalar has an unsupported type")


def _encode_custom_input_attribute(value):
    if type(value) is tuple:
        return {"type": "list", "value": [_encode_custom_input_scalar(item) for item in value]}
    return _encode_custom_input_scalar(value)


def _encode_custom_input_entries(value, *, dimensions=False):
    return {
        key: _encode_custom_input_scalar(item, allow_none=False)
        if dimensions
        else _encode_custom_input_attribute(item)
        for key, item in value
    }


def _encode_custom_input_number(value):
    if type(value) is int:
        return {"type": "integer", "value": _custom_integer_text(value)}
    if type(value) is _Decimal:
        return {"type": "decimal", "value": _decimal_text(value)}
    raise TypeError("normalized custom counter must be an exact int or Decimal")


def _custom_candidate_payload(candidate):
    if type(candidate) is _CustomInstantCandidate:
        return {
            "name": candidate.name,
            "severity": candidate.severity,
            "attributes": _encode_custom_input_entries(candidate.attributes),
        }
    if type(candidate) is _CustomCounterCandidate:
        return {
            "name": candidate.name,
            "value": _encode_custom_input_number(candidate.value),
            "unit": candidate.unit,
            "dimensions": _encode_custom_input_entries(candidate.dimensions, dimensions=True),
        }
    if type(candidate) is _CustomSpanStartCandidate:
        return {
            "name": candidate.name,
            "attributes": _encode_custom_input_entries(candidate.attributes),
        }
    raise TypeError("candidate has no caller-supplied custom payload")


def _custom_candidate_payload_bytes(candidate):
    return _json.dumps(
        _custom_candidate_payload(candidate),
        ensure_ascii=False,
        allow_nan=False,
        separators=(",", ":"),
    ).encode("utf-8")


def _require_custom_payload_size(candidate):
    if len(_custom_candidate_payload_bytes(candidate)) > _MAX_CUSTOM_PAYLOAD_BYTES:
        raise ValueError("custom caller-supplied payload exceeds 64 KiB")
    return candidate


def _set_custom_admission_hook(hook):
    global _custom_admission_hook
    if hook is not None and not callable(hook):
        raise TypeError("custom admission hook must be callable or None")
    _custom_admission_hook = hook


def _admit_custom_candidate(candidate):
    hook = _custom_admission_hook
    if hook is None:
        raise DiagnosticContextError("diagnostic publication requires an active Runtime context")
    result = hook(candidate)
    if result is not None:
        raise RuntimeError("custom admission hook must return None")


@_final
class _CustomSpanContext:
    __slots__ = ("_candidate", "_phase")

    def __init__(self, candidate):
        if type(candidate) is not _CustomSpanStartCandidate:
            raise TypeError("span context requires an exact start candidate")
        self._candidate = candidate
        self._phase = 0

    def __enter__(self):
        if self._phase != 0:
            raise RuntimeError("custom span context cannot be entered more than once")
        _admit_custom_candidate(self._candidate)
        self._phase = 1
        return None

    def __exit__(self, exception_type, exception, traceback):
        if self._phase != 1:
            raise RuntimeError("custom span context is not active")
        self._phase = 2
        if exception is None:
            outcome = "completed"
        elif isinstance(exception, _asyncio.CancelledError):
            outcome = "cancelled"
        else:
            outcome = "failed"
        _admit_custom_candidate(_CustomSpanFinishCandidate(outcome=outcome))
        return False


def event(
    name: str,
    /,
    *,
    severity: _Literal["debug", "info", "warning", "error"] = "info",
    attributes: _Mapping[str, DiagnosticAttributeValue] | None = None,
) -> None:
    candidate = _require_custom_payload_size(_CustomInstantCandidate(
        name=_require_custom_name(name),
        severity=_require_enum(severity, _CUSTOM_SEVERITIES, "severity"),
        attributes=_normalize_custom_entries(attributes),
    ))
    _admit_custom_candidate(candidate)


def counter(
    name: str,
    value: int | float | _Decimal,
    /,
    *,
    unit: str | None = None,
    dimensions: _Mapping[str, DiagnosticDimension] | None = None,
) -> None:
    candidate = _require_custom_payload_size(_CustomCounterCandidate(
        name=_require_custom_name(name),
        value=_normalize_custom_number(value),
        unit=_normalize_custom_unit(unit),
        dimensions=_normalize_custom_entries(dimensions, dimensions=True),
    ))
    _admit_custom_candidate(candidate)


def span(
    name: str,
    /,
    *,
    attributes: _Mapping[str, DiagnosticAttributeValue] | None = None,
) -> _AbstractContextManager[None]:
    candidate = _require_custom_payload_size(_CustomSpanStartCandidate(
        name=_require_custom_name(name),
        attributes=_normalize_custom_entries(attributes),
    ))
    return _CustomSpanContext(candidate)
"#;

pub(crate) const fn source() -> &'static CStr {
    SOURCE
}
