use std::ffi::CStr;

const SOURCE: &CStr = cr#"
import json as _json
import re as _re
from dataclasses import dataclass as _dataclass
from decimal import Decimal as _Decimal
from types import NoneType as _NoneType
from typing import ClassVar as _ClassVar, Literal as _Literal, TypeAlias as _TypeAlias
from uuid import UUID as _UUID


_MAX_U64 = 18_446_744_073_709_551_615
_MAX_RUN_LOCAL_ID_BYTES = 128
_MAX_CAUSAL_LINKS = 16
_MAX_CUSTOM_NAME_BYTES = 128
_MAX_CUSTOM_KEY_BYTES = 64
_MAX_CUSTOM_ATTRIBUTES = 32
_MAX_CUSTOM_DIMENSIONS = 8
_MAX_CUSTOM_LIST_ITEMS = 64
_MAX_CUSTOM_UNIT_BYTES = 32
_NONNEGATIVE_INTEGER = _re.compile(r"(?:0|[1-9][0-9]*)\Z")
_CANONICAL_INTEGER = _re.compile(r"(?:0|-?[1-9][0-9]*)\Z")
_CANONICAL_DECIMAL = _re.compile(r"-?(?:0|[1-9][0-9]*)(?:\.[0-9]*[1-9])?\Z")
_CUSTOM_NAME = _re.compile(r"[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)+\Z")

_CAUSAL_RELATIONS = frozenset(("dispatch", "return", "handoff", "retry", "follows_from"))
_SPAN_KINDS = frozenset((
    "run.lifecycle",
    "production.path_resolution",
    "production.load",
    "production.construct",
    "production.start",
    "production.stop",
    "production.shutdown",
    "scene.lifecycle",
    "scene.drain",
    "scene.cleanup",
    "actor.handle_lifetime",
    "cue.mailbox_wait",
    "cue.execution",
    "effect.lifecycle",
    "agent.session.opening",
    "agent.session.lifecycle",
    "agent.session.closing",
    "act.lifecycle",
    "act.caller",
    "agent.turn",
    "agent.thinking",
    "tool.call",
))
_INSTANT_KINDS = frozenset((
    "actor.cast",
    "cue.admitted",
    "cue.enqueued",
    "cue.dispatched",
    "cue.cancel_requested",
    "effect.created",
    "effect.returned",
    "effect.consumed",
    "agent.session.ready",
    "agent.session.broken",
    "act.admitted",
    "act.waiting_ready",
    "act.prompt_submitted",
    "act.cancel_requested",
    "act.supervisor_handoff",
    "agent.turn.activity",
    "agent.turn.terminal",
    "agent.turn.settled",
    "tool.updated",
    "result.submitted",
    "result.rejected",
    "result.repair_requested",
    "result.accepted",
    "result.missing",
    "diagnostic.component_failed",
))
_COUNTER_KINDS = frozenset((
    "actor.mailbox_depth",
    "cue.active",
    "agent.turn.active",
    "result.validation_rejections",
    "diagnostic.dropped_events",
))
_EVENT_KINDS = frozenset((
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
))
_SPAN_OUTCOMES = frozenset(("completed", "cancelled", "failed"))
_PLAN_PRIORITIES = frozenset(("high", "medium", "low"))
_PLAN_STATUSES = frozenset(("pending", "in_progress", "completed"))
_SAMPLE_ORIGINS = frozenset(("provider", "carried_forward"))
_USAGE_AVAILABILITIES = frozenset(("available", "partial", "unavailable"))
_USAGE_SOURCES = frozenset(("acp.prompt_response.usage",))
_USAGE_UNAVAILABLE_REASONS = frozenset((
    "prompt_not_submitted",
    "source_unsupported",
    "usage_not_reported",
    "turn_settlement_unknown",
))
_CUSTOM_SEVERITIES = frozenset(("debug", "info", "warning", "error"))
_TOOL_KINDS = frozenset((
    "read",
    "edit",
    "delete",
    "move",
    "search",
    "execute",
    "think",
    "fetch",
    "switch_mode",
    "other",
))
_TOOL_STATUSES = frozenset(("pending", "in_progress", "completed", "failed"))


def _final(cls):
    def _reject_subclass(subclass, **kwargs):
        raise TypeError(f"{cls.__name__} is final")

    cls.__init_subclass__ = classmethod(_reject_subclass)
    return cls


def _require_exact(value, expected, field):
    if type(value) is not expected:
        raise TypeError(f"{field} must be an exact {expected.__name__}")
    return value


def _require_optional_string(value, field):
    if value is not None:
        _require_exact(value, str, field)
    return value


def _require_bool(value, field):
    return _require_exact(value, bool, field)


def _require_u64(value, field, *, nonzero=False):
    if type(value) is not int:
        raise TypeError(f"{field} must be an exact int")
    if value < int(nonzero) or value > _MAX_U64:
        raise ValueError(f"{field} is outside its u64 domain")
    return value


def _require_optional_u64(value, field, *, nonzero=False):
    if value is not None:
        _require_u64(value, field, nonzero=nonzero)
    return value


def _require_token_integer(value, field):
    if type(value) is not int:
        raise TypeError(f"{field} must be an exact int")
    if value < 0:
        raise ValueError(f"{field} must be nonnegative")
    return value


def _require_optional_token_integer(value, field):
    if value is not None:
        _require_token_integer(value, field)
    return value


def _require_enum(value, allowed, field):
    _require_exact(value, str, field)
    if value not in allowed:
        raise ValueError(f"{field} has an unknown value")
    return value


def _require_run_local_id(value, field):
    _require_exact(value, str, field)
    if not value or not value.isascii() or len(value.encode("ascii")) > _MAX_RUN_LOCAL_ID_BYTES:
        raise ValueError(f"{field} is not a canonical RunLocalId")
    return value


def _require_optional_run_local_id(value, field):
    if value is not None:
        _require_run_local_id(value, field)
    return value


def _require_uuid(value, field):
    if type(value) is not _UUID:
        raise TypeError(f"{field} must be an exact UUID")
    if str(value) != str(value).lower():
        raise ValueError(f"{field} must be canonical")
    return value


def _decimal_text(value):
    if type(value) is not _Decimal:
        raise TypeError("decimal value must be an exact Decimal")
    if not value.is_finite():
        raise ValueError("decimal value must be finite")
    if value.is_zero():
        return "0"
    text = format(value, "f")
    if "." in text:
        text = text.rstrip("0").rstrip(".")
    if len(text.encode("ascii")) > 1024 * 1024:
        raise ValueError("decimal value is too large")
    if _CANONICAL_DECIMAL.fullmatch(text) is None:
        raise ValueError("decimal value is not canonical")
    return text


def _normalize_decimal(value):
    return _Decimal(_decimal_text(value))


def _from_wire_u64(value, field, *, nonzero=False):
    _require_exact(value, str, field)
    if _NONNEGATIVE_INTEGER.fullmatch(value) is None:
        raise ValueError(f"{field} is not a canonical u64")
    parsed = int(value)
    return _require_u64(parsed, field, nonzero=nonzero)


def _from_wire_optional_u64(value, field, *, nonzero=False):
    if value is None:
        return None
    return _from_wire_u64(value, field, nonzero=nonzero)


def _from_wire_token_integer(value, field):
    _require_exact(value, str, field)
    if _NONNEGATIVE_INTEGER.fullmatch(value) is None:
        raise ValueError(f"{field} is not a canonical token integer")
    return int(value)


def _from_wire_optional_token_integer(value, field):
    if value is None:
        return None
    return _from_wire_token_integer(value, field)


def _from_wire_decimal(value, field):
    _require_exact(value, str, field)
    if value == "-0" or _CANONICAL_DECIMAL.fullmatch(value) is None:
        raise ValueError(f"{field} is not a canonical decimal")
    parsed = _Decimal(value)
    if not parsed.is_finite() or _decimal_text(parsed) != value:
        raise ValueError(f"{field} is not a canonical finite decimal")
    return parsed


def _from_wire_optional_decimal(value, field):
    if value is None:
        return None
    return _from_wire_decimal(value, field)


def _closed_mapping(value, fields, context):
    if type(value) is not dict:
        raise TypeError(f"{context} must be an exact object")
    actual = frozenset(value)
    expected = frozenset(fields)
    if actual != expected:
        raise ValueError(f"{context} fields are not closed")
    return value


def _exact_list(value, context):
    if type(value) is not list:
        raise TypeError(f"{context} must be an exact array")
    return value


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class FrozenJsonArray:
    items: tuple["FrozenJsonValue", ...]

    def __post_init__(self):
        if type(self.items) is not tuple:
            raise TypeError("items must be an exact tuple")
        object.__setattr__(self, "items", tuple(_normalize_frozen_json(item) for item in self.items))


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class FrozenJsonObject:
    entries: tuple[tuple[str, "FrozenJsonValue"], ...]

    def __post_init__(self):
        if type(self.entries) is not tuple:
            raise TypeError("entries must be an exact tuple")
        normalized = []
        keys = []
        for entry in self.entries:
            if type(entry) is not tuple or len(entry) != 2:
                raise TypeError("each FrozenJsonObject entry must be an exact pair tuple")
            key, value = entry
            _require_exact(key, str, "FrozenJsonObject key")
            keys.append(key)
            normalized.append((key, _normalize_frozen_json(value)))
        canonical_keys = sorted(keys, key=lambda key: key.encode("utf-8"))
        if keys != canonical_keys or len(keys) != len(set(keys)):
            raise ValueError("FrozenJsonObject entries must use unique canonical key order")
        object.__setattr__(self, "entries", tuple(normalized))


FrozenJsonValue: _TypeAlias = (
    _NoneType | bool | int | _Decimal | str | FrozenJsonArray | FrozenJsonObject
)


def _normalize_frozen_json(value):
    if value is None or type(value) in (bool, int, str):
        return value
    if type(value) is _Decimal:
        return _normalize_decimal(value)
    if type(value) in (FrozenJsonArray, FrozenJsonObject):
        return value
    raise TypeError("value is not a FrozenJsonValue")


def _freeze_json(value):
    if value is None or type(value) in (bool, int, str):
        return value
    if type(value) is _Decimal:
        return _normalize_decimal(value)
    if type(value) in (FrozenJsonArray, FrozenJsonObject):
        return value
    if type(value) is list:
        return FrozenJsonArray(items=tuple(_freeze_json(item) for item in value))
    if type(value) is dict:
        for key in value:
            _require_exact(key, str, "JSON object key")
        return FrozenJsonObject(entries=tuple(
            (key, _freeze_json(value[key]))
            for key in sorted(value, key=lambda key: key.encode("utf-8"))
        ))
    raise TypeError("tool payload accepts only exact JSON container and scalar types")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class DiagnosticToolLocation:
    path: str
    line: int | None

    def __post_init__(self):
        _require_exact(self.path, str, "path")
        if self.line is not None:
            if type(self.line) is not int:
                raise TypeError("line must be an exact int or None")
            if self.line < 0:
                raise ValueError("line must be nonnegative")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class DiagnosticToolInput:
    raw_input: FrozenJsonValue | None
    truncated: bool

    def __post_init__(self):
        object.__setattr__(self, "raw_input", _normalize_frozen_json(self.raw_input))
        _require_bool(self.truncated, "truncated")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class DiagnosticToolOutput:
    raw_output: FrozenJsonValue | None
    content: tuple[FrozenJsonValue, ...]
    locations: tuple[DiagnosticToolLocation, ...]
    truncated: bool

    def __post_init__(self):
        object.__setattr__(self, "raw_output", _normalize_frozen_json(self.raw_output))
        if type(self.content) is not tuple:
            raise TypeError("content must be an exact tuple")
        object.__setattr__(self, "content", tuple(_normalize_frozen_json(item) for item in self.content))
        if type(self.locations) is not tuple or any(
            type(location) is not DiagnosticToolLocation for location in self.locations
        ):
            raise TypeError("locations must be a tuple of exact DiagnosticToolLocation values")
        _require_bool(self.truncated, "truncated")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class DiagnosticScope:
    scene_id: str | None = None
    actor_id: str | None = None
    cue_id: str | None = None
    effect_id: str | None = None
    act_id: str | None = None
    tool_call_id: str | None = None
    session_generation: int | None = None

    def __post_init__(self):
        for field in ("scene_id", "actor_id", "cue_id", "effect_id", "act_id", "tool_call_id"):
            _require_optional_run_local_id(getattr(self, field), field)
        _require_optional_u64(self.session_generation, "session_generation", nonzero=True)


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class CausalLink:
    source_sequence: int
    relation: str

    def __post_init__(self):
        _require_u64(self.source_sequence, "source_sequence", nonzero=True)
        _require_enum(self.relation, _CAUSAL_RELATIONS, "relation")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class EmptyDetail:
    pass


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class ProductionPathResolutionDetail:
    production_root: str
    package: str

    def __post_init__(self):
        _require_exact(self.production_root, str, "production_root")
        _require_exact(self.package, str, "package")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class ProductionLoadDetail:
    package: str

    def __post_init__(self):
        _require_exact(self.package, str, "package")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class ProductionConstructDetail:
    package: str
    class_name: str

    def __post_init__(self):
        _require_exact(self.package, str, "package")
        _require_exact(self.class_name, str, "class_name")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class ActorDetail:
    display_name: str
    actor_type: str

    def __post_init__(self):
        _require_exact(self.display_name, str, "display_name")
        _require_exact(self.actor_type, str, "actor_type")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class EffectDetail:
    effect_type: str

    def __post_init__(self):
        _require_exact(self.effect_type, str, "effect_type")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class AgentSessionDetail:
    provider: str
    effective_model: str | None
    effective_effort: str | None

    def __post_init__(self):
        _require_exact(self.provider, str, "provider")
        _require_optional_string(self.effective_model, "effective_model")
        _require_optional_string(self.effective_effort, "effective_effort")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class AgentSessionBrokenDetail:
    provider: str
    effective_model: str | None
    effective_effort: str | None
    error_code: str

    def __post_init__(self):
        _require_exact(self.provider, str, "provider")
        _require_optional_string(self.effective_model, "effective_model")
        _require_optional_string(self.effective_effort, "effective_effort")
        _require_exact(self.error_code, str, "error_code")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class AgentTurnTerminalDetail:
    error_code: str | None

    def __post_init__(self):
        _require_optional_string(self.error_code, "error_code")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class ToolCallDetail:
    title: str
    tool_kind: str
    status: str
    error_code: str | None
    captured_input: DiagnosticToolInput | None = None
    captured_output: DiagnosticToolOutput | None = None

    def __post_init__(self):
        _require_exact(self.title, str, "title")
        _require_enum(self.tool_kind, _TOOL_KINDS, "tool_kind")
        _require_enum(self.status, _TOOL_STATUSES, "status")
        _require_optional_string(self.error_code, "error_code")
        if self.captured_input is not None and type(self.captured_input) is not DiagnosticToolInput:
            raise TypeError("captured_input must be an exact DiagnosticToolInput or None")
        if self.captured_output is not None and type(self.captured_output) is not DiagnosticToolOutput:
            raise TypeError("captured_output must be an exact DiagnosticToolOutput or None")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class ResultIssue:
    code: str
    path: str

    def __post_init__(self):
        _require_exact(self.code, str, "code")
        _require_exact(self.path, str, "path")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class ResultTransitionDetail:
    issue: ResultIssue | None
    error_code: str | None

    def __post_init__(self):
        if self.issue is not None and type(self.issue) is not ResultIssue:
            raise TypeError("issue must be an exact ResultIssue or None")
        _require_optional_string(self.error_code, "error_code")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class DiagnosticComponentFailedDetail:
    component: str
    component_id: str
    stage: str
    error_code: str
    related_event_sequence: int | None

    def __post_init__(self):
        if self.component != "sink":
            raise ValueError("component must be sink")
        _require_run_local_id(self.component_id, "component_id")
        valid_pair = (
            self.stage == "enqueue" and self.error_code == "delivery_queue_unavailable"
        ) or (
            self.stage == "callback"
            and self.error_code in ("callback_raised", "callback_invalid_return")
        )
        if not valid_pair:
            raise ValueError("component failure stage and error code do not match")
        _require_optional_u64(self.related_event_sequence, "related_event_sequence", nonzero=True)


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class PlanEntry:
    content: str
    priority: str
    status: str

    def __post_init__(self):
        _require_exact(self.content, str, "content")
        _require_enum(self.priority, _PLAN_PRIORITIES, "priority")
        _require_enum(self.status, _PLAN_STATUSES, "status")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class AffectedElapsedInterval:
    start_ns: int
    end_ns: int

    def __post_init__(self):
        _require_u64(self.start_ns, "start_ns")
        _require_u64(self.end_ns, "end_ns")
        if self.start_ns > self.end_ns:
            raise ValueError("affected elapsed interval is reversed")


SpanStartDetail: _TypeAlias = (
    EmptyDetail
    | ProductionPathResolutionDetail
    | ProductionLoadDetail
    | ProductionConstructDetail
    | ActorDetail
    | EffectDetail
    | AgentSessionDetail
    | ToolCallDetail
)
InstantDetail: _TypeAlias = (
    EmptyDetail
    | ActorDetail
    | EffectDetail
    | AgentSessionDetail
    | AgentSessionBrokenDetail
    | AgentTurnTerminalDetail
    | ToolCallDetail
    | ResultTransitionDetail
    | DiagnosticComponentFailedDetail
)
DiagnosticScalar: _TypeAlias = _NoneType | bool | int | _Decimal | str
DiagnosticAttributeValue: _TypeAlias = DiagnosticScalar | tuple[DiagnosticScalar, ...]
DiagnosticDimension: _TypeAlias = bool | int | _Decimal | str
DiagnosticAttributes: _TypeAlias = tuple[tuple[str, DiagnosticAttributeValue], ...]
DiagnosticDimensions: _TypeAlias = tuple[tuple[str, DiagnosticDimension], ...]


def _normalize_diagnostic_scalar(value, *, allow_none=True):
    if value is None:
        if allow_none:
            return None
        raise TypeError("dimension values cannot be None")
    if type(value) in (bool, int, str):
        return value
    if type(value) is _Decimal:
        return _normalize_decimal(value)
    raise TypeError("diagnostic scalar has an unsupported type")


def _normalize_attribute_value(value):
    if type(value) is tuple:
        if len(value) > _MAX_CUSTOM_LIST_ITEMS:
            raise ValueError("diagnostic scalar tuple is too long")
        return tuple(_normalize_diagnostic_scalar(item) for item in value)
    return _normalize_diagnostic_scalar(value)


def _normalize_ordered_pairs(values, *, dimensions=False):
    if type(values) is not tuple:
        raise TypeError("diagnostic attributes and dimensions must be exact tuples")
    maximum = _MAX_CUSTOM_DIMENSIONS if dimensions else _MAX_CUSTOM_ATTRIBUTES
    if len(values) > maximum:
        raise ValueError("too many diagnostic entries")
    normalized = []
    keys = []
    for entry in values:
        if type(entry) is not tuple or len(entry) != 2:
            raise TypeError("diagnostic entries must be exact pair tuples")
        key, value = entry
        _require_exact(key, str, "diagnostic key")
        if not key or len(key.encode("utf-8")) > _MAX_CUSTOM_KEY_BYTES:
            raise ValueError("diagnostic key is out of bounds")
        keys.append(key)
        normalized_value = (
            _normalize_diagnostic_scalar(value, allow_none=False)
            if dimensions
            else _normalize_attribute_value(value)
        )
        normalized.append((key, normalized_value))
    canonical_keys = sorted(keys, key=lambda key: key.encode("utf-8"))
    if keys != canonical_keys or len(keys) != len(set(keys)):
        raise ValueError("diagnostic entries must use unique canonical key order")
    return tuple(normalized)


def _require_custom_name(value):
    _require_exact(value, str, "name")
    if (
        not value.isascii()
        or not 1 <= len(value.encode("ascii")) <= _MAX_CUSTOM_NAME_BYTES
        or _CUSTOM_NAME.fullmatch(value) is None
        or value.split(".", 1)[0] == "troupe"
    ):
        raise ValueError("custom diagnostic name is invalid or reserved")
    return value


@_dataclass(frozen=True, slots=True, kw_only=True)
class _EventBase:
    schema_version: int
    run_id: _UUID
    sequence: int
    elapsed_ns: int
    scope: DiagnosticScope
    caused_by: tuple[CausalLink, ...]

    def __post_init__(self):
        if type(self.schema_version) is not int:
            raise TypeError("schema_version must be an exact int")
        if self.schema_version != 1:
            raise ValueError("unsupported diagnostic event schema version")
        _require_uuid(self.run_id, "run_id")
        _require_u64(self.sequence, "sequence", nonzero=True)
        _require_u64(self.elapsed_ns, "elapsed_ns")
        if type(self.scope) is not DiagnosticScope:
            raise TypeError("scope must be an exact DiagnosticScope")
        if type(self.caused_by) is not tuple or any(
            type(link) is not CausalLink for link in self.caused_by
        ):
            raise TypeError("caused_by must be a tuple of exact CausalLink values")
        if len(self.caused_by) > _MAX_CAUSAL_LINKS:
            raise ValueError("caused_by has too many links")
        if any(link.source_sequence >= self.sequence for link in self.caused_by):
            raise ValueError("causal links must point backward")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class SpanStarted(_EventBase):
    kind: _ClassVar[_Literal["span_started"]] = "span_started"
    span_kind: str
    detail: SpanStartDetail
    parent_span_id: int | None

    def __post_init__(self):
        _EventBase.__post_init__(self)
        _require_enum(self.span_kind, _SPAN_KINDS, "span_kind")
        expected = _SPAN_DETAIL_TYPES[self.span_kind]
        if type(self.detail) is not expected:
            raise TypeError("span detail does not match span_kind")
        _require_optional_u64(self.parent_span_id, "parent_span_id", nonzero=True)


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class SpanFinished(_EventBase):
    kind: _ClassVar[_Literal["span_finished"]] = "span_finished"
    span_id: int
    outcome: str
    error_code: str | None

    def __post_init__(self):
        _EventBase.__post_init__(self)
        _require_u64(self.span_id, "span_id", nonzero=True)
        _require_enum(self.outcome, _SPAN_OUTCOMES, "outcome")
        _require_optional_string(self.error_code, "error_code")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class InstantOccurred(_EventBase):
    kind: _ClassVar[_Literal["instant_occurred"]] = "instant_occurred"
    instant_kind: str
    detail: InstantDetail
    containing_span_id: int | None

    def __post_init__(self):
        _EventBase.__post_init__(self)
        _require_enum(self.instant_kind, _INSTANT_KINDS, "instant_kind")
        expected = _INSTANT_DETAIL_TYPES[self.instant_kind]
        if type(self.detail) is not expected:
            raise TypeError("instant detail does not match instant_kind")
        _require_optional_u64(self.containing_span_id, "containing_span_id", nonzero=True)


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class CounterSampled(_EventBase):
    kind: _ClassVar[_Literal["counter_sampled"]] = "counter_sampled"
    counter_kind: str
    value: int

    def __post_init__(self):
        _EventBase.__post_init__(self)
        _require_enum(self.counter_kind, _COUNTER_KINDS, "counter_kind")
        _require_u64(self.value, "value")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class AgentMessageDelta(_EventBase):
    kind: _ClassVar[_Literal["agent_message_delta"]] = "agent_message_delta"
    message_id: str
    source_message_id: str | None
    text_delta: str

    def __post_init__(self):
        _EventBase.__post_init__(self)
        _require_run_local_id(self.message_id, "message_id")
        _require_optional_string(self.source_message_id, "source_message_id")
        _require_exact(self.text_delta, str, "text_delta")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class AgentMessageCompleted(_EventBase):
    kind: _ClassVar[_Literal["agent_message_completed"]] = "agent_message_completed"
    message_id: str
    utf8_bytes: int
    unicode_scalar_count: int
    truncated: bool

    def __post_init__(self):
        _EventBase.__post_init__(self)
        _require_run_local_id(self.message_id, "message_id")
        _require_u64(self.utf8_bytes, "utf8_bytes")
        _require_u64(self.unicode_scalar_count, "unicode_scalar_count")
        _require_bool(self.truncated, "truncated")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class AgentPlanSnapshot(_EventBase):
    kind: _ClassVar[_Literal["agent_plan_snapshot"]] = "agent_plan_snapshot"
    entries: tuple[PlanEntry, ...]
    truncated: bool

    def __post_init__(self):
        _EventBase.__post_init__(self)
        if type(self.entries) is not tuple or any(type(entry) is not PlanEntry for entry in self.entries):
            raise TypeError("entries must be a tuple of exact PlanEntry values")
        _require_bool(self.truncated, "truncated")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class ContextUsageSampled(_EventBase):
    kind: _ClassVar[_Literal["context_usage_sampled"]] = "context_usage_sampled"
    context_used_tokens: int | None
    context_window_tokens: int | None
    cumulative_cost_amount: _Decimal | None
    cumulative_cost_currency: str | None
    sample_origin: str
    observed_elapsed_ns: int | None

    def __post_init__(self):
        _EventBase.__post_init__(self)
        _require_optional_u64(self.context_used_tokens, "context_used_tokens")
        _require_optional_u64(self.context_window_tokens, "context_window_tokens")
        if (
            self.context_used_tokens is not None
            and self.context_window_tokens is not None
            and self.context_used_tokens > self.context_window_tokens
        ):
            raise ValueError("context used tokens exceed the context window")
        if self.cumulative_cost_amount is not None:
            amount = _normalize_decimal(self.cumulative_cost_amount)
            if amount < 0:
                raise ValueError("cumulative cost amount must be nonnegative")
            object.__setattr__(self, "cumulative_cost_amount", amount)
        _require_optional_string(self.cumulative_cost_currency, "cumulative_cost_currency")
        if (self.cumulative_cost_amount is None) != (self.cumulative_cost_currency is None):
            raise ValueError("cumulative cost amount and currency must appear together")
        if self.cumulative_cost_currency is not None and (
            len(self.cumulative_cost_currency) != 3
            or not self.cumulative_cost_currency.isascii()
            or not self.cumulative_cost_currency.isalpha()
            or not self.cumulative_cost_currency.isupper()
        ):
            raise ValueError("cumulative cost currency must be ISO-4217 shaped")
        _require_enum(self.sample_origin, _SAMPLE_ORIGINS, "sample_origin")
        _require_optional_u64(self.observed_elapsed_ns, "observed_elapsed_ns")
        if self.sample_origin == "carried_forward" and self.observed_elapsed_ns is None:
            raise ValueError("carried-forward usage requires its original observation time")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class ActTokenUsageFinalized(_EventBase):
    kind: _ClassVar[_Literal["act_token_usage_finalized"]] = "act_token_usage_finalized"
    availability: str
    source: str | None
    unavailable_reason: str | None
    provider_total_tokens: int | None
    input_tokens: int | None
    output_tokens: int | None
    thought_tokens: int | None
    cached_read_tokens: int | None
    cached_write_tokens: int | None

    def __post_init__(self):
        _EventBase.__post_init__(self)
        _require_enum(self.availability, _USAGE_AVAILABILITIES, "availability")
        if self.source is not None:
            _require_enum(self.source, _USAGE_SOURCES, "source")
        if self.unavailable_reason is not None:
            _require_enum(self.unavailable_reason, _USAGE_UNAVAILABLE_REASONS, "unavailable_reason")
        values = (
            self.provider_total_tokens,
            self.input_tokens,
            self.output_tokens,
            self.thought_tokens,
            self.cached_read_tokens,
            self.cached_write_tokens,
        )
        for index, value in enumerate(values):
            _require_optional_token_integer(value, f"token field {index}")
        primary_complete = all(value is not None for value in values[:3])
        any_value = any(value is not None for value in values)
        valid = (
            self.availability == "available"
            and primary_complete
            and self.source == "acp.prompt_response.usage"
            and self.unavailable_reason is None
        ) or (
            self.availability == "partial"
            and any_value
            and not primary_complete
            and self.source == "acp.prompt_response.usage"
            and self.unavailable_reason is None
        ) or (
            self.availability == "unavailable"
            and not any_value
            and self.source is None
            and self.unavailable_reason is not None
        )
        if not valid:
            raise ValueError("terminal usage availability fields are inconsistent")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class ObservationGap(_EventBase):
    kind: _ClassVar[_Literal["observation_gap"]] = "observation_gap"
    producer: str
    component: str | None
    reason: str
    dropped_count: int | None
    affected_elapsed: AffectedElapsedInterval | None
    affected_kind: str | None
    affected_scope: DiagnosticScope | None

    def __post_init__(self):
        _EventBase.__post_init__(self)
        _require_exact(self.producer, str, "producer")
        _require_optional_string(self.component, "component")
        _require_exact(self.reason, str, "reason")
        _require_optional_u64(self.dropped_count, "dropped_count")
        if self.affected_elapsed is not None and type(self.affected_elapsed) is not AffectedElapsedInterval:
            raise TypeError("affected_elapsed must be an exact AffectedElapsedInterval or None")
        if self.affected_kind is not None:
            _require_enum(self.affected_kind, _EVENT_KINDS, "affected_kind")
        if self.affected_scope is not None and type(self.affected_scope) is not DiagnosticScope:
            raise TypeError("affected_scope must be an exact DiagnosticScope or None")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class CustomSpanStarted(_EventBase):
    kind: _ClassVar[_Literal["custom_span_started"]] = "custom_span_started"
    name: str
    parent_span_id: int | None
    attributes: DiagnosticAttributes

    def __post_init__(self):
        _EventBase.__post_init__(self)
        _require_custom_name(self.name)
        _require_optional_u64(self.parent_span_id, "parent_span_id", nonzero=True)
        object.__setattr__(self, "attributes", _normalize_ordered_pairs(self.attributes))


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class CustomSpanFinished(_EventBase):
    kind: _ClassVar[_Literal["custom_span_finished"]] = "custom_span_finished"
    span_id: int
    outcome: str

    def __post_init__(self):
        _EventBase.__post_init__(self)
        _require_u64(self.span_id, "span_id", nonzero=True)
        _require_enum(self.outcome, _SPAN_OUTCOMES, "outcome")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class CustomInstantOccurred(_EventBase):
    kind: _ClassVar[_Literal["custom_instant_occurred"]] = "custom_instant_occurred"
    name: str
    containing_span_id: int | None
    severity: str | None
    attributes: DiagnosticAttributes

    def __post_init__(self):
        _EventBase.__post_init__(self)
        _require_custom_name(self.name)
        _require_optional_u64(self.containing_span_id, "containing_span_id", nonzero=True)
        if self.severity is not None:
            _require_enum(self.severity, _CUSTOM_SEVERITIES, "severity")
        object.__setattr__(self, "attributes", _normalize_ordered_pairs(self.attributes))


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class CustomCounterSampled(_EventBase):
    kind: _ClassVar[_Literal["custom_counter_sampled"]] = "custom_counter_sampled"
    name: str
    value: int | _Decimal
    unit: str | None
    dimensions: DiagnosticDimensions

    def __post_init__(self):
        _EventBase.__post_init__(self)
        _require_custom_name(self.name)
        if type(self.value) is int:
            pass
        elif type(self.value) is _Decimal:
            object.__setattr__(self, "value", _normalize_decimal(self.value))
        else:
            raise TypeError("custom counter value must be an exact int or Decimal")
        _require_optional_string(self.unit, "unit")
        if self.unit is not None and (
            not self.unit or len(self.unit.encode("utf-8")) > _MAX_CUSTOM_UNIT_BYTES
        ):
            raise ValueError("custom counter unit is out of bounds")
        object.__setattr__(self, "dimensions", _normalize_ordered_pairs(self.dimensions, dimensions=True))


DiagnosticEvent: _TypeAlias = (
    SpanStarted
    | SpanFinished
    | InstantOccurred
    | CounterSampled
    | AgentMessageDelta
    | AgentMessageCompleted
    | AgentPlanSnapshot
    | ContextUsageSampled
    | ActTokenUsageFinalized
    | ObservationGap
    | CustomSpanStarted
    | CustomSpanFinished
    | CustomInstantOccurred
    | CustomCounterSampled
)


_SPAN_DETAIL_TYPES = {
    "run.lifecycle": EmptyDetail,
    "production.path_resolution": ProductionPathResolutionDetail,
    "production.load": ProductionLoadDetail,
    "production.construct": ProductionConstructDetail,
    "production.start": EmptyDetail,
    "production.stop": EmptyDetail,
    "production.shutdown": EmptyDetail,
    "scene.lifecycle": EmptyDetail,
    "scene.drain": EmptyDetail,
    "scene.cleanup": EmptyDetail,
    "actor.handle_lifetime": ActorDetail,
    "cue.mailbox_wait": EmptyDetail,
    "cue.execution": EmptyDetail,
    "effect.lifecycle": EffectDetail,
    "agent.session.opening": AgentSessionDetail,
    "agent.session.lifecycle": AgentSessionDetail,
    "agent.session.closing": AgentSessionDetail,
    "act.lifecycle": AgentSessionDetail,
    "act.caller": EmptyDetail,
    "agent.turn": AgentSessionDetail,
    "agent.thinking": EmptyDetail,
    "tool.call": ToolCallDetail,
}
_INSTANT_DETAIL_TYPES = {
    "actor.cast": ActorDetail,
    "cue.admitted": EmptyDetail,
    "cue.enqueued": EmptyDetail,
    "cue.dispatched": EmptyDetail,
    "cue.cancel_requested": EmptyDetail,
    "effect.created": EffectDetail,
    "effect.returned": EffectDetail,
    "effect.consumed": EffectDetail,
    "agent.session.ready": AgentSessionDetail,
    "agent.session.broken": AgentSessionBrokenDetail,
    "act.admitted": EmptyDetail,
    "act.waiting_ready": EmptyDetail,
    "act.prompt_submitted": EmptyDetail,
    "act.cancel_requested": EmptyDetail,
    "act.supervisor_handoff": EmptyDetail,
    "agent.turn.activity": EmptyDetail,
    "agent.turn.terminal": AgentTurnTerminalDetail,
    "agent.turn.settled": AgentTurnTerminalDetail,
    "tool.updated": ToolCallDetail,
    "result.submitted": ResultTransitionDetail,
    "result.rejected": ResultTransitionDetail,
    "result.repair_requested": ResultTransitionDetail,
    "result.accepted": ResultTransitionDetail,
    "result.missing": ResultTransitionDetail,
    "diagnostic.component_failed": DiagnosticComponentFailedDetail,
}

_EVENT_CLASSES = (
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
_COMMON_WIRE_FIELDS = (
    "kind",
    "schema_version",
    "run_id",
    "sequence",
    "elapsed_ns",
    "scope",
    "caused_by",
)
_EVENT_WIRE_FIELDS = {
    "span_started": ("span_kind", "detail", "parent_span_id"),
    "span_finished": ("span_id", "outcome", "error_code"),
    "instant_occurred": ("instant_kind", "detail", "containing_span_id"),
    "counter_sampled": ("counter_kind", "value"),
    "agent_message_delta": ("message_id", "source_message_id", "text_delta"),
    "agent_message_completed": ("message_id", "utf8_bytes", "unicode_scalar_count", "truncated"),
    "agent_plan_snapshot": ("entries", "truncated"),
    "context_usage_sampled": (
        "context_used_tokens",
        "context_window_tokens",
        "cumulative_cost_amount",
        "cumulative_cost_currency",
        "sample_origin",
        "observed_elapsed_ns",
    ),
    "act_token_usage_finalized": (
        "availability",
        "source",
        "unavailable_reason",
        "provider_total_tokens",
        "input_tokens",
        "output_tokens",
        "thought_tokens",
        "cached_read_tokens",
        "cached_write_tokens",
    ),
    "observation_gap": (
        "producer",
        "component",
        "reason",
        "dropped_count",
        "affected_elapsed",
        "affected_kind",
        "affected_scope",
    ),
    "custom_span_started": ("name", "parent_span_id", "attributes"),
    "custom_span_finished": ("span_id", "outcome"),
    "custom_instant_occurred": ("name", "containing_span_id", "severity", "attributes"),
    "custom_counter_sampled": ("name", "value", "unit", "dimensions"),
}


def _decode_uuid(value):
    _require_exact(value, str, "run_id")
    try:
        parsed = _UUID(value)
    except (ValueError, AttributeError) as error:
        raise ValueError("run_id is not a UUID") from error
    if str(parsed) != value:
        raise ValueError("run_id is not a canonical lowercase UUID")
    return parsed


def _decode_optional_string(value, field):
    if value is None:
        return None
    return _require_exact(value, str, field)


def _decode_scope(value):
    _closed_mapping(
        value,
        (
            "scene_id",
            "actor_id",
            "cue_id",
            "effect_id",
            "act_id",
            "tool_call_id",
            "session_generation",
        ),
        "scope",
    )
    identifiers = {}
    for field in ("scene_id", "actor_id", "cue_id", "effect_id", "act_id", "tool_call_id"):
        raw = value[field]
        if raw is not None:
            _require_run_local_id(raw, field)
        identifiers[field] = raw
    return DiagnosticScope(
        **identifiers,
        session_generation=_from_wire_optional_u64(
            value["session_generation"], "session_generation", nonzero=True
        ),
    )


def _encode_scope(value):
    if type(value) is not DiagnosticScope:
        raise TypeError("scope must be an exact DiagnosticScope")
    return {
        "scene_id": value.scene_id,
        "actor_id": value.actor_id,
        "cue_id": value.cue_id,
        "effect_id": value.effect_id,
        "act_id": value.act_id,
        "tool_call_id": value.tool_call_id,
        "session_generation": None
        if value.session_generation is None
        else str(value.session_generation),
    }


def _decode_causal_links(value):
    links = _exact_list(value, "caused_by")
    if len(links) > _MAX_CAUSAL_LINKS:
        raise ValueError("caused_by has too many links")
    projected = []
    for raw in links:
        _closed_mapping(raw, ("source_sequence", "relation"), "causal link")
        projected.append(CausalLink(
            source_sequence=_from_wire_u64(raw["source_sequence"], "source_sequence", nonzero=True),
            relation=_require_enum(raw["relation"], _CAUSAL_RELATIONS, "relation"),
        ))
    return tuple(projected)


def _encode_causal_links(value):
    return [
        {"source_sequence": str(link.source_sequence), "relation": link.relation}
        for link in value
    ]


def _decode_empty_detail(value):
    _closed_mapping(value, (), "empty detail")
    return EmptyDetail()


def _decode_agent_session_detail(value):
    _closed_mapping(value, ("provider", "effective_model", "effective_effort"), "agent session detail")
    return AgentSessionDetail(
        provider=_require_exact(value["provider"], str, "provider"),
        effective_model=_decode_optional_string(value["effective_model"], "effective_model"),
        effective_effort=_decode_optional_string(value["effective_effort"], "effective_effort"),
    )


def _decode_tool_call_detail(value):
    _closed_mapping(value, ("title", "tool_kind", "status", "error_code"), "tool call detail")
    return ToolCallDetail(
        title=_require_exact(value["title"], str, "title"),
        tool_kind=_require_enum(value["tool_kind"], _TOOL_KINDS, "tool_kind"),
        status=_require_enum(value["status"], _TOOL_STATUSES, "status"),
        error_code=_decode_optional_string(value["error_code"], "error_code"),
        captured_input=None,
        captured_output=None,
    )


def _decode_result_transition_detail(value):
    _closed_mapping(value, ("issue", "error_code"), "result transition detail")
    issue = value["issue"]
    if issue is not None:
        _closed_mapping(issue, ("code", "path"), "result issue")
        issue = ResultIssue(
            code=_require_exact(issue["code"], str, "issue code"),
            path=_require_exact(issue["path"], str, "issue path"),
        )
    return ResultTransitionDetail(
        issue=issue,
        error_code=_decode_optional_string(value["error_code"], "error_code"),
    )


def _decode_span_detail(kind, value):
    expected = _SPAN_DETAIL_TYPES[kind]
    if expected is EmptyDetail:
        return _decode_empty_detail(value)
    if expected is ProductionPathResolutionDetail:
        _closed_mapping(value, ("production_root", "package"), "production path detail")
        return ProductionPathResolutionDetail(
            production_root=_require_exact(value["production_root"], str, "production_root"),
            package=_require_exact(value["package"], str, "package"),
        )
    if expected is ProductionLoadDetail:
        _closed_mapping(value, ("package",), "production load detail")
        return ProductionLoadDetail(package=_require_exact(value["package"], str, "package"))
    if expected is ProductionConstructDetail:
        _closed_mapping(value, ("package", "class_name"), "production construct detail")
        return ProductionConstructDetail(
            package=_require_exact(value["package"], str, "package"),
            class_name=_require_exact(value["class_name"], str, "class_name"),
        )
    if expected is ActorDetail:
        _closed_mapping(value, ("display_name", "actor_type"), "actor detail")
        return ActorDetail(
            display_name=_require_exact(value["display_name"], str, "display_name"),
            actor_type=_require_exact(value["actor_type"], str, "actor_type"),
        )
    if expected is EffectDetail:
        _closed_mapping(value, ("effect_type",), "effect detail")
        return EffectDetail(effect_type=_require_exact(value["effect_type"], str, "effect_type"))
    if expected is AgentSessionDetail:
        return _decode_agent_session_detail(value)
    if expected is ToolCallDetail:
        return _decode_tool_call_detail(value)
    raise AssertionError("unhandled span detail class")


def _decode_instant_detail(kind, value):
    expected = _INSTANT_DETAIL_TYPES[kind]
    if expected is EmptyDetail:
        return _decode_empty_detail(value)
    if expected is ActorDetail:
        _closed_mapping(value, ("display_name", "actor_type"), "actor detail")
        return ActorDetail(
            display_name=_require_exact(value["display_name"], str, "display_name"),
            actor_type=_require_exact(value["actor_type"], str, "actor_type"),
        )
    if expected is EffectDetail:
        _closed_mapping(value, ("effect_type",), "effect detail")
        return EffectDetail(effect_type=_require_exact(value["effect_type"], str, "effect_type"))
    if expected is AgentSessionDetail:
        return _decode_agent_session_detail(value)
    if expected is AgentSessionBrokenDetail:
        _closed_mapping(
            value,
            ("provider", "effective_model", "effective_effort", "error_code"),
            "broken agent session detail",
        )
        return AgentSessionBrokenDetail(
            provider=_require_exact(value["provider"], str, "provider"),
            effective_model=_decode_optional_string(value["effective_model"], "effective_model"),
            effective_effort=_decode_optional_string(value["effective_effort"], "effective_effort"),
            error_code=_require_exact(value["error_code"], str, "error_code"),
        )
    if expected is AgentTurnTerminalDetail:
        _closed_mapping(value, ("error_code",), "agent turn terminal detail")
        return AgentTurnTerminalDetail(
            error_code=_decode_optional_string(value["error_code"], "error_code")
        )
    if expected is ToolCallDetail:
        return _decode_tool_call_detail(value)
    if expected is ResultTransitionDetail:
        return _decode_result_transition_detail(value)
    if expected is DiagnosticComponentFailedDetail:
        _closed_mapping(
            value,
            ("component", "component_id", "stage", "error_code", "related_event_sequence"),
            "diagnostic component failure detail",
        )
        return DiagnosticComponentFailedDetail(
            component=_require_exact(value["component"], str, "component"),
            component_id=_require_run_local_id(value["component_id"], "component_id"),
            stage=_require_exact(value["stage"], str, "stage"),
            error_code=_require_exact(value["error_code"], str, "error_code"),
            related_event_sequence=_from_wire_optional_u64(
                value["related_event_sequence"], "related_event_sequence", nonzero=True
            ),
        )
    raise AssertionError("unhandled instant detail class")


def _decode_diagnostic_scalar(value, *, allow_none=True):
    if type(value) is not dict:
        raise TypeError("diagnostic scalar must be an exact tagged object")
    scalar_type = value.get("type")
    _require_exact(scalar_type, str, "diagnostic scalar type")
    if scalar_type == "null":
        _closed_mapping(value, ("type",), "null diagnostic scalar")
        if not allow_none:
            raise ValueError("dimension value cannot be null")
        return None
    _closed_mapping(value, ("type", "value"), "diagnostic scalar")
    raw = value["value"]
    if scalar_type == "boolean":
        return _require_bool(raw, "boolean diagnostic scalar")
    if scalar_type == "integer":
        _require_exact(raw, str, "integer diagnostic scalar")
        if _CANONICAL_INTEGER.fullmatch(raw) is None:
            raise ValueError("integer diagnostic scalar is not canonical")
        return int(raw)
    if scalar_type == "decimal":
        return _from_wire_decimal(raw, "decimal diagnostic scalar")
    if scalar_type == "string":
        return _require_exact(raw, str, "string diagnostic scalar")
    raise ValueError("diagnostic scalar type is unknown")


def _decode_attribute_value(value):
    if type(value) is not dict:
        raise TypeError("diagnostic attribute must be an exact tagged object")
    if value.get("type") != "list":
        return _decode_diagnostic_scalar(value)
    _closed_mapping(value, ("type", "value"), "diagnostic scalar list")
    items = _exact_list(value["value"], "diagnostic scalar list")
    if len(items) > _MAX_CUSTOM_LIST_ITEMS:
        raise ValueError("diagnostic scalar list is too long")
    return tuple(_decode_diagnostic_scalar(item) for item in items)


def _decode_ordered_entries(value, *, dimensions=False):
    if type(value) is not dict:
        raise TypeError("diagnostic entries must be an exact object")
    if len(value) > (_MAX_CUSTOM_DIMENSIONS if dimensions else _MAX_CUSTOM_ATTRIBUTES):
        raise ValueError("too many diagnostic entries")
    keys = list(value)
    if keys != sorted(keys, key=lambda key: key.encode("utf-8")):
        raise ValueError("diagnostic object keys are not in canonical order")
    result = []
    for key in keys:
        _require_exact(key, str, "diagnostic key")
        if not key or len(key.encode("utf-8")) > _MAX_CUSTOM_KEY_BYTES:
            raise ValueError("diagnostic key is out of bounds")
        result.append((
            key,
            _decode_diagnostic_scalar(value[key], allow_none=False)
            if dimensions
            else _decode_attribute_value(value[key]),
        ))
    return tuple(result)


def _decode_custom_number(value):
    if type(value) is not dict:
        raise TypeError("custom number must be an exact tagged object")
    _closed_mapping(value, ("type", "value"), "custom number")
    number_type = _require_exact(value["type"], str, "custom number type")
    raw = value["value"]
    if number_type == "integer":
        _require_exact(raw, str, "custom integer")
        if _CANONICAL_INTEGER.fullmatch(raw) is None:
            raise ValueError("custom integer is not canonical")
        return int(raw)
    if number_type == "decimal":
        return _from_wire_decimal(raw, "custom decimal")
    raise ValueError("custom number type is unknown")


def _decode_plan_entries(value):
    return tuple(
        PlanEntry(
            content=_require_exact(
                _closed_mapping(raw, ("content", "priority", "status"), "plan entry")["content"],
                str,
                "plan content",
            ),
            priority=_require_enum(raw["priority"], _PLAN_PRIORITIES, "plan priority"),
            status=_require_enum(raw["status"], _PLAN_STATUSES, "plan status"),
        )
        for raw in _exact_list(value, "plan entries")
    )


def _decode_common(value):
    return {
        "schema_version": _require_exact(value["schema_version"], int, "schema_version"),
        "run_id": _decode_uuid(value["run_id"]),
        "sequence": _from_wire_u64(value["sequence"], "sequence", nonzero=True),
        "elapsed_ns": _from_wire_u64(value["elapsed_ns"], "elapsed_ns"),
        "scope": _decode_scope(value["scope"]),
        "caused_by": _decode_causal_links(value["caused_by"]),
    }


def _event_from_mapping(value):
    if type(value) is not dict:
        raise TypeError("diagnostic event must be an exact object")
    kind = value.get("kind")
    _require_exact(kind, str, "kind")
    if kind not in _EVENT_WIRE_FIELDS:
        raise ValueError("diagnostic event kind is unknown")
    _closed_mapping(value, _COMMON_WIRE_FIELDS + _EVENT_WIRE_FIELDS[kind], "diagnostic event")
    common = _decode_common(value)

    if kind == "span_started":
        span_kind = _require_enum(value["span_kind"], _SPAN_KINDS, "span_kind")
        return SpanStarted(
            **common,
            span_kind=span_kind,
            detail=_decode_span_detail(span_kind, value["detail"]),
            parent_span_id=_from_wire_optional_u64(
                value["parent_span_id"], "parent_span_id", nonzero=True
            ),
        )
    if kind == "span_finished":
        return SpanFinished(
            **common,
            span_id=_from_wire_u64(value["span_id"], "span_id", nonzero=True),
            outcome=_require_enum(value["outcome"], _SPAN_OUTCOMES, "outcome"),
            error_code=_decode_optional_string(value["error_code"], "error_code"),
        )
    if kind == "instant_occurred":
        instant_kind = _require_enum(value["instant_kind"], _INSTANT_KINDS, "instant_kind")
        return InstantOccurred(
            **common,
            instant_kind=instant_kind,
            detail=_decode_instant_detail(instant_kind, value["detail"]),
            containing_span_id=_from_wire_optional_u64(
                value["containing_span_id"], "containing_span_id", nonzero=True
            ),
        )
    if kind == "counter_sampled":
        return CounterSampled(
            **common,
            counter_kind=_require_enum(value["counter_kind"], _COUNTER_KINDS, "counter_kind"),
            value=_from_wire_u64(value["value"], "value"),
        )
    if kind == "agent_message_delta":
        return AgentMessageDelta(
            **common,
            message_id=_require_run_local_id(value["message_id"], "message_id"),
            source_message_id=_decode_optional_string(value["source_message_id"], "source_message_id"),
            text_delta=_require_exact(value["text_delta"], str, "text_delta"),
        )
    if kind == "agent_message_completed":
        return AgentMessageCompleted(
            **common,
            message_id=_require_run_local_id(value["message_id"], "message_id"),
            utf8_bytes=_from_wire_u64(value["utf8_bytes"], "utf8_bytes"),
            unicode_scalar_count=_from_wire_u64(
                value["unicode_scalar_count"], "unicode_scalar_count"
            ),
            truncated=_require_bool(value["truncated"], "truncated"),
        )
    if kind == "agent_plan_snapshot":
        return AgentPlanSnapshot(
            **common,
            entries=_decode_plan_entries(value["entries"]),
            truncated=_require_bool(value["truncated"], "truncated"),
        )
    if kind == "context_usage_sampled":
        return ContextUsageSampled(
            **common,
            context_used_tokens=_from_wire_optional_u64(
                value["context_used_tokens"], "context_used_tokens"
            ),
            context_window_tokens=_from_wire_optional_u64(
                value["context_window_tokens"], "context_window_tokens"
            ),
            cumulative_cost_amount=_from_wire_optional_decimal(
                value["cumulative_cost_amount"], "cumulative_cost_amount"
            ),
            cumulative_cost_currency=_decode_optional_string(
                value["cumulative_cost_currency"], "cumulative_cost_currency"
            ),
            sample_origin=_require_enum(value["sample_origin"], _SAMPLE_ORIGINS, "sample_origin"),
            observed_elapsed_ns=_from_wire_optional_u64(
                value["observed_elapsed_ns"], "observed_elapsed_ns"
            ),
        )
    if kind == "act_token_usage_finalized":
        return ActTokenUsageFinalized(
            **common,
            availability=_require_enum(value["availability"], _USAGE_AVAILABILITIES, "availability"),
            source=None
            if value["source"] is None
            else _require_enum(value["source"], _USAGE_SOURCES, "source"),
            unavailable_reason=None
            if value["unavailable_reason"] is None
            else _require_enum(
                value["unavailable_reason"], _USAGE_UNAVAILABLE_REASONS, "unavailable_reason"
            ),
            provider_total_tokens=_from_wire_optional_token_integer(
                value["provider_total_tokens"], "provider_total_tokens"
            ),
            input_tokens=_from_wire_optional_token_integer(value["input_tokens"], "input_tokens"),
            output_tokens=_from_wire_optional_token_integer(value["output_tokens"], "output_tokens"),
            thought_tokens=_from_wire_optional_token_integer(value["thought_tokens"], "thought_tokens"),
            cached_read_tokens=_from_wire_optional_token_integer(
                value["cached_read_tokens"], "cached_read_tokens"
            ),
            cached_write_tokens=_from_wire_optional_token_integer(
                value["cached_write_tokens"], "cached_write_tokens"
            ),
        )
    if kind == "observation_gap":
        affected_elapsed = value["affected_elapsed"]
        if affected_elapsed is not None:
            _closed_mapping(affected_elapsed, ("start_ns", "end_ns"), "affected elapsed interval")
            affected_elapsed = AffectedElapsedInterval(
                start_ns=_from_wire_u64(affected_elapsed["start_ns"], "start_ns"),
                end_ns=_from_wire_u64(affected_elapsed["end_ns"], "end_ns"),
            )
        affected_kind = value["affected_kind"]
        if affected_kind is not None:
            affected_kind = _require_enum(affected_kind, _EVENT_KINDS, "affected_kind")
        return ObservationGap(
            **common,
            producer=_require_exact(value["producer"], str, "producer"),
            component=_decode_optional_string(value["component"], "component"),
            reason=_require_exact(value["reason"], str, "reason"),
            dropped_count=_from_wire_optional_u64(value["dropped_count"], "dropped_count"),
            affected_elapsed=affected_elapsed,
            affected_kind=affected_kind,
            affected_scope=None
            if value["affected_scope"] is None
            else _decode_scope(value["affected_scope"]),
        )
    if kind == "custom_span_started":
        return CustomSpanStarted(
            **common,
            name=_require_custom_name(value["name"]),
            parent_span_id=_from_wire_optional_u64(
                value["parent_span_id"], "parent_span_id", nonzero=True
            ),
            attributes=_decode_ordered_entries(value["attributes"]),
        )
    if kind == "custom_span_finished":
        return CustomSpanFinished(
            **common,
            span_id=_from_wire_u64(value["span_id"], "span_id", nonzero=True),
            outcome=_require_enum(value["outcome"], _SPAN_OUTCOMES, "outcome"),
        )
    if kind == "custom_instant_occurred":
        severity = value["severity"]
        if severity is not None:
            severity = _require_enum(severity, _CUSTOM_SEVERITIES, "severity")
        return CustomInstantOccurred(
            **common,
            name=_require_custom_name(value["name"]),
            containing_span_id=_from_wire_optional_u64(
                value["containing_span_id"], "containing_span_id", nonzero=True
            ),
            severity=severity,
            attributes=_decode_ordered_entries(value["attributes"]),
        )
    if kind == "custom_counter_sampled":
        return CustomCounterSampled(
            **common,
            name=_require_custom_name(value["name"]),
            value=_decode_custom_number(value["value"]),
            unit=_decode_optional_string(value["unit"], "unit"),
            dimensions=_decode_ordered_entries(value["dimensions"], dimensions=True),
        )
    raise AssertionError("unhandled diagnostic event kind")


def _encode_agent_session_detail(value):
    return {
        "provider": value.provider,
        "effective_model": value.effective_model,
        "effective_effort": value.effective_effort,
    }


def _encode_tool_call_detail(value):
    return {
        "title": value.title,
        "tool_kind": value.tool_kind,
        "status": value.status,
        "error_code": value.error_code,
    }


def _encode_span_detail(kind, value):
    expected = _SPAN_DETAIL_TYPES[kind]
    if type(value) is not expected:
        raise TypeError("span detail does not match span_kind")
    if expected is EmptyDetail:
        return {}
    if expected is ProductionPathResolutionDetail:
        return {"production_root": value.production_root, "package": value.package}
    if expected is ProductionLoadDetail:
        return {"package": value.package}
    if expected is ProductionConstructDetail:
        return {"package": value.package, "class_name": value.class_name}
    if expected is ActorDetail:
        return {"display_name": value.display_name, "actor_type": value.actor_type}
    if expected is EffectDetail:
        return {"effect_type": value.effect_type}
    if expected is AgentSessionDetail:
        return _encode_agent_session_detail(value)
    if expected is ToolCallDetail:
        return _encode_tool_call_detail(value)
    raise AssertionError("unhandled span detail class")


def _encode_instant_detail(kind, value):
    expected = _INSTANT_DETAIL_TYPES[kind]
    if type(value) is not expected:
        raise TypeError("instant detail does not match instant_kind")
    if expected is EmptyDetail:
        return {}
    if expected is ActorDetail:
        return {"display_name": value.display_name, "actor_type": value.actor_type}
    if expected is EffectDetail:
        return {"effect_type": value.effect_type}
    if expected is AgentSessionDetail:
        return _encode_agent_session_detail(value)
    if expected is AgentSessionBrokenDetail:
        return {
            "provider": value.provider,
            "effective_model": value.effective_model,
            "effective_effort": value.effective_effort,
            "error_code": value.error_code,
        }
    if expected is AgentTurnTerminalDetail:
        return {"error_code": value.error_code}
    if expected is ToolCallDetail:
        return _encode_tool_call_detail(value)
    if expected is ResultTransitionDetail:
        return {
            "issue": None
            if value.issue is None
            else {"code": value.issue.code, "path": value.issue.path},
            "error_code": value.error_code,
        }
    if expected is DiagnosticComponentFailedDetail:
        return {
            "component": value.component,
            "component_id": value.component_id,
            "stage": value.stage,
            "error_code": value.error_code,
            "related_event_sequence": None
            if value.related_event_sequence is None
            else str(value.related_event_sequence),
        }
    raise AssertionError("unhandled instant detail class")


def _encode_diagnostic_scalar(value, *, allow_none=True):
    if value is None:
        if not allow_none:
            raise TypeError("dimension value cannot be None")
        return {"type": "null"}
    if type(value) is bool:
        return {"type": "boolean", "value": value}
    if type(value) is int:
        return {"type": "integer", "value": str(value)}
    if type(value) is _Decimal:
        return {"type": "decimal", "value": _decimal_text(value)}
    if type(value) is str:
        return {"type": "string", "value": value}
    raise TypeError("diagnostic scalar has an unsupported type")


def _encode_attribute_value(value):
    if type(value) is tuple:
        return {"type": "list", "value": [_encode_diagnostic_scalar(item) for item in value]}
    return _encode_diagnostic_scalar(value)


def _encode_entries(value, *, dimensions=False):
    return {
        key: _encode_diagnostic_scalar(item, allow_none=False)
        if dimensions
        else _encode_attribute_value(item)
        for key, item in value
    }


def _encode_custom_number(value):
    if type(value) is int:
        return {"type": "integer", "value": str(value)}
    if type(value) is _Decimal:
        return {"type": "decimal", "value": _decimal_text(value)}
    raise TypeError("custom counter value must be an exact int or Decimal")


def _event_to_mapping(event):
    if type(event) not in _EVENT_CLASSES:
        raise TypeError("value is not an exact DiagnosticEvent variant")
    value = {
        "kind": event.kind,
        "schema_version": event.schema_version,
        "run_id": str(event.run_id),
        "sequence": str(event.sequence),
        "elapsed_ns": str(event.elapsed_ns),
        "scope": _encode_scope(event.scope),
        "caused_by": _encode_causal_links(event.caused_by),
    }
    if type(event) is SpanStarted:
        value.update({
            "span_kind": event.span_kind,
            "detail": _encode_span_detail(event.span_kind, event.detail),
            "parent_span_id": None if event.parent_span_id is None else str(event.parent_span_id),
        })
    elif type(event) is SpanFinished:
        value.update({
            "span_id": str(event.span_id),
            "outcome": event.outcome,
            "error_code": event.error_code,
        })
    elif type(event) is InstantOccurred:
        value.update({
            "instant_kind": event.instant_kind,
            "detail": _encode_instant_detail(event.instant_kind, event.detail),
            "containing_span_id": None
            if event.containing_span_id is None
            else str(event.containing_span_id),
        })
    elif type(event) is CounterSampled:
        value.update({"counter_kind": event.counter_kind, "value": str(event.value)})
    elif type(event) is AgentMessageDelta:
        value.update({
            "message_id": event.message_id,
            "source_message_id": event.source_message_id,
            "text_delta": event.text_delta,
        })
    elif type(event) is AgentMessageCompleted:
        value.update({
            "message_id": event.message_id,
            "utf8_bytes": str(event.utf8_bytes),
            "unicode_scalar_count": str(event.unicode_scalar_count),
            "truncated": event.truncated,
        })
    elif type(event) is AgentPlanSnapshot:
        value.update({
            "entries": [
                {"content": entry.content, "priority": entry.priority, "status": entry.status}
                for entry in event.entries
            ],
            "truncated": event.truncated,
        })
    elif type(event) is ContextUsageSampled:
        value.update({
            "context_used_tokens": None
            if event.context_used_tokens is None
            else str(event.context_used_tokens),
            "context_window_tokens": None
            if event.context_window_tokens is None
            else str(event.context_window_tokens),
            "cumulative_cost_amount": None
            if event.cumulative_cost_amount is None
            else _decimal_text(event.cumulative_cost_amount),
            "cumulative_cost_currency": event.cumulative_cost_currency,
            "sample_origin": event.sample_origin,
            "observed_elapsed_ns": None
            if event.observed_elapsed_ns is None
            else str(event.observed_elapsed_ns),
        })
    elif type(event) is ActTokenUsageFinalized:
        value.update({
            "availability": event.availability,
            "source": event.source,
            "unavailable_reason": event.unavailable_reason,
            "provider_total_tokens": None
            if event.provider_total_tokens is None
            else str(event.provider_total_tokens),
            "input_tokens": None if event.input_tokens is None else str(event.input_tokens),
            "output_tokens": None if event.output_tokens is None else str(event.output_tokens),
            "thought_tokens": None if event.thought_tokens is None else str(event.thought_tokens),
            "cached_read_tokens": None
            if event.cached_read_tokens is None
            else str(event.cached_read_tokens),
            "cached_write_tokens": None
            if event.cached_write_tokens is None
            else str(event.cached_write_tokens),
        })
    elif type(event) is ObservationGap:
        value.update({
            "producer": event.producer,
            "component": event.component,
            "reason": event.reason,
            "dropped_count": None if event.dropped_count is None else str(event.dropped_count),
            "affected_elapsed": None
            if event.affected_elapsed is None
            else {
                "start_ns": str(event.affected_elapsed.start_ns),
                "end_ns": str(event.affected_elapsed.end_ns),
            },
            "affected_kind": event.affected_kind,
            "affected_scope": None
            if event.affected_scope is None
            else _encode_scope(event.affected_scope),
        })
    elif type(event) is CustomSpanStarted:
        value.update({
            "name": event.name,
            "parent_span_id": None if event.parent_span_id is None else str(event.parent_span_id),
            "attributes": _encode_entries(event.attributes),
        })
    elif type(event) is CustomSpanFinished:
        value.update({"span_id": str(event.span_id), "outcome": event.outcome})
    elif type(event) is CustomInstantOccurred:
        value.update({
            "name": event.name,
            "containing_span_id": None
            if event.containing_span_id is None
            else str(event.containing_span_id),
            "severity": event.severity,
            "attributes": _encode_entries(event.attributes),
        })
    elif type(event) is CustomCounterSampled:
        value.update({
            "name": event.name,
            "value": _encode_custom_number(event.value),
            "unit": event.unit,
            "dimensions": _encode_entries(event.dimensions, dimensions=True),
        })
    else:
        raise AssertionError("unhandled DiagnosticEvent class")
    return value


def _event_to_json_bytes(event):
    return _json.dumps(
        _event_to_mapping(event),
        ensure_ascii=False,
        allow_nan=False,
        separators=(",", ":"),
    ).encode("utf-8")


def _event_from_json_bytes(value):
    if type(value) is str:
        encoded = value.encode("utf-8")
    elif type(value) is bytes:
        encoded = value
    else:
        raise TypeError("canonical event input must be an exact str or bytes")
    try:
        mapping = _json.loads(encoded)
    except (UnicodeDecodeError, _json.JSONDecodeError) as error:
        raise ValueError("canonical event JSON is invalid") from error
    event = _event_from_mapping(mapping)
    if _event_to_json_bytes(event) != encoded:
        raise ValueError("diagnostic event JSON is not canonical")
    return event
"#;

pub(crate) const fn source() -> &'static CStr {
    SOURCE
}
