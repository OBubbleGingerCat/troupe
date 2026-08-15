from __future__ import annotations

from abc import ABC as _ABC, abstractmethod as _abstractmethod
from collections.abc import Awaitable as _Awaitable, Mapping as _Mapping
from contextlib import AbstractContextManager as _AbstractContextManager
from dataclasses import dataclass as _dataclass
from decimal import Decimal as _Decimal
from typing import ClassVar as _ClassVar, Literal as _Literal, TypeAlias as _TypeAlias, final as _final
from uuid import UUID as _UUID

_SpanKind: _TypeAlias = _Literal[
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
]
_InstantKind: _TypeAlias = _Literal[
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
]
_CounterKind: _TypeAlias = _Literal[
    "actor.mailbox_depth",
    "cue.active",
    "agent.turn.active",
    "result.validation_rejections",
    "diagnostic.dropped_events",
]
_EventKind: _TypeAlias = _Literal[
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
]
_SpanOutcome: _TypeAlias = _Literal["completed", "cancelled", "failed"]
_CustomSeverity: _TypeAlias = _Literal["debug", "info", "warning", "error"]
_CausalRelation: _TypeAlias = _Literal[
    "dispatch", "return", "handoff", "retry", "follows_from"
]
_PlanPriority: _TypeAlias = _Literal["high", "medium", "low"]
_PlanStatus: _TypeAlias = _Literal["pending", "in_progress", "completed"]
_SampleOrigin: _TypeAlias = _Literal["provider", "carried_forward"]
_UsageAvailability: _TypeAlias = _Literal["available", "partial", "unavailable"]
_UsageSource: _TypeAlias = _Literal["acp.prompt_response.usage"]
_UsageUnavailableReason: _TypeAlias = _Literal[
    "prompt_not_submitted",
    "source_unsupported",
    "usage_not_reported",
    "turn_settlement_unknown",
]
_ToolKind: _TypeAlias = _Literal[
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
]
_ToolStatus: _TypeAlias = _Literal["pending", "in_progress", "completed", "failed"]
_SinkState: _TypeAlias = _Literal["UNBOUND", "BOUND", "SEALED", "CLOSED"]
_SinkStateErrorCode: _TypeAlias = _Literal["uninitialized", "unbound", "already_bound"]
_CallbackFailureKind: _TypeAlias = _Literal["raised", "invalid_return"]
_SinkCloseReason: _TypeAlias = _Literal[
    "act_finished", "callback_failed", "delivery_overflow", "runtime_shutdown"
]
_ViewTimeRange: _TypeAlias = _Literal["viewport", "run"]
_ViewScope: _TypeAlias = _Literal["selection", "run"]
_Reducer: _TypeAlias = _Literal["count", "sum", "min", "max", "mean", "latest"]
_TokenMetric: _TypeAlias = _Literal[
    "provider_total_tokens",
    "input_tokens",
    "output_tokens",
    "thought_tokens",
    "cached_read_tokens",
    "cached_write_tokens",
]
_GroupDimension: _TypeAlias = _Literal[
    "scene",
    "actor",
    "cue",
    "act",
    "event_name",
    "custom_name",
    "attribute",
    "custom_dimension",
]
_TableColumnName: _TypeAlias = _Literal[
    "sequence",
    "elapsed_ns",
    "event_kind",
    "span_kind",
    "instant_kind",
    "counter_kind",
    "scene_id",
    "actor_id",
    "cue_id",
    "act_id",
    "custom_name",
    "outcome",
    "severity",
    "attribute",
    "token",
    "value",
]

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class FrozenJsonArray:
    items: tuple[FrozenJsonValue, ...]
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class FrozenJsonObject:
    entries: tuple[tuple[str, FrozenJsonValue], ...]
    def __post_init__(self) -> None: ...

FrozenJsonValue: _TypeAlias = (
    None | bool | int | _Decimal | str | FrozenJsonArray | FrozenJsonObject
)

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class DiagnosticToolLocation:
    path: str
    line: int | None
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class DiagnosticToolInput:
    raw_input: FrozenJsonValue | None
    truncated: bool
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class DiagnosticToolOutput:
    raw_output: FrozenJsonValue | None
    content: tuple[FrozenJsonValue, ...]
    locations: tuple[DiagnosticToolLocation, ...]
    truncated: bool
    def __post_init__(self) -> None: ...

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
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class CausalLink:
    source_sequence: int
    relation: _CausalRelation
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class EmptyDetail: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class ProductionPathResolutionDetail:
    production_root: str
    package: str
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class ProductionLoadDetail:
    package: str
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class ProductionConstructDetail:
    package: str
    class_name: str
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class ActorDetail:
    display_name: str
    actor_type: str
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class EffectDetail:
    effect_type: str
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class AgentSessionDetail:
    provider: str
    effective_model: str | None
    effective_effort: str | None
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class AgentSessionBrokenDetail:
    provider: str
    effective_model: str | None
    effective_effort: str | None
    error_code: str
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class AgentTurnTerminalDetail:
    error_code: str | None
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class ToolCallDetail:
    title: str
    tool_kind: _ToolKind
    status: _ToolStatus
    error_code: str | None
    captured_input: DiagnosticToolInput | None = None
    captured_output: DiagnosticToolOutput | None = None
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class ResultIssue:
    code: str
    path: str
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class ResultTransitionDetail:
    issue: ResultIssue | None
    error_code: str | None
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class DiagnosticComponentFailedDetail:
    component: _Literal["sink"]
    component_id: str
    stage: _Literal["enqueue", "callback"]
    error_code: _Literal[
        "delivery_queue_unavailable", "callback_raised", "callback_invalid_return"
    ]
    related_event_sequence: int | None
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class PlanEntry:
    content: str
    priority: _PlanPriority
    status: _PlanStatus
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class AffectedElapsedInterval:
    start_ns: int
    end_ns: int
    def __post_init__(self) -> None: ...

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
DiagnosticScalar: _TypeAlias = None | bool | int | float | _Decimal | str
DiagnosticAttributeValue: _TypeAlias = (
    DiagnosticScalar | list[DiagnosticScalar] | tuple[DiagnosticScalar, ...]
)
DiagnosticDimension: _TypeAlias = bool | int | float | _Decimal | str
_ProjectedDiagnosticScalar: _TypeAlias = None | bool | int | _Decimal | str
_ProjectedDiagnosticAttributeValue: _TypeAlias = (
    _ProjectedDiagnosticScalar | tuple[_ProjectedDiagnosticScalar, ...]
)
DiagnosticAttributes: _TypeAlias = tuple[tuple[str, _ProjectedDiagnosticAttributeValue], ...]
DiagnosticDimensions: _TypeAlias = tuple[tuple[str, bool | int | _Decimal | str], ...]

@_dataclass(frozen=True, kw_only=True)
class _EventBase:
    __slots__ = (
        "schema_version",
        "run_id",
        "sequence",
        "elapsed_ns",
        "scope",
        "caused_by",
    )
    schema_version: _Literal[1]
    run_id: _UUID
    sequence: int
    elapsed_ns: int
    scope: DiagnosticScope
    caused_by: tuple[CausalLink, ...]
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, kw_only=True)
class SpanStarted(_EventBase):
    __slots__ = ("span_kind", "detail", "parent_span_id")
    kind: _ClassVar[_Literal["span_started"]]
    span_kind: _SpanKind
    detail: SpanStartDetail
    parent_span_id: int | None
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, kw_only=True)
class SpanFinished(_EventBase):
    __slots__ = ("span_id", "outcome", "error_code")
    kind: _ClassVar[_Literal["span_finished"]]
    span_id: int
    outcome: _SpanOutcome
    error_code: str | None
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, kw_only=True)
class InstantOccurred(_EventBase):
    __slots__ = ("instant_kind", "detail", "containing_span_id")
    kind: _ClassVar[_Literal["instant_occurred"]]
    instant_kind: _InstantKind
    detail: InstantDetail
    containing_span_id: int | None
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, kw_only=True)
class CounterSampled(_EventBase):
    __slots__ = ("counter_kind", "value")
    kind: _ClassVar[_Literal["counter_sampled"]]
    counter_kind: _CounterKind
    value: int
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, kw_only=True)
class AgentMessageDelta(_EventBase):
    __slots__ = ("message_id", "source_message_id", "text_delta")
    kind: _ClassVar[_Literal["agent_message_delta"]]
    message_id: str
    source_message_id: str | None
    text_delta: str
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, kw_only=True)
class AgentMessageCompleted(_EventBase):
    __slots__ = ("message_id", "utf8_bytes", "unicode_scalar_count", "truncated")
    kind: _ClassVar[_Literal["agent_message_completed"]]
    message_id: str
    utf8_bytes: int
    unicode_scalar_count: int
    truncated: bool
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, kw_only=True)
class AgentPlanSnapshot(_EventBase):
    __slots__ = ("entries", "truncated")
    kind: _ClassVar[_Literal["agent_plan_snapshot"]]
    entries: tuple[PlanEntry, ...]
    truncated: bool
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, kw_only=True)
class ContextUsageSampled(_EventBase):
    __slots__ = (
        "context_used_tokens",
        "context_window_tokens",
        "cumulative_cost_amount",
        "cumulative_cost_currency",
        "sample_origin",
        "observed_elapsed_ns",
    )
    kind: _ClassVar[_Literal["context_usage_sampled"]]
    context_used_tokens: int | None
    context_window_tokens: int | None
    cumulative_cost_amount: _Decimal | None
    cumulative_cost_currency: str | None
    sample_origin: _SampleOrigin
    observed_elapsed_ns: int | None
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, kw_only=True)
class ActTokenUsageFinalized(_EventBase):
    __slots__ = (
        "availability",
        "source",
        "unavailable_reason",
        "provider_total_tokens",
        "input_tokens",
        "output_tokens",
        "thought_tokens",
        "cached_read_tokens",
        "cached_write_tokens",
    )
    kind: _ClassVar[_Literal["act_token_usage_finalized"]]
    availability: _UsageAvailability
    source: _UsageSource | None
    unavailable_reason: _UsageUnavailableReason | None
    provider_total_tokens: int | None
    input_tokens: int | None
    output_tokens: int | None
    thought_tokens: int | None
    cached_read_tokens: int | None
    cached_write_tokens: int | None
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, kw_only=True)
class ObservationGap(_EventBase):
    __slots__ = (
        "producer",
        "component",
        "reason",
        "dropped_count",
        "affected_elapsed",
        "affected_kind",
        "affected_scope",
    )
    kind: _ClassVar[_Literal["observation_gap"]]
    producer: str
    component: str | None
    reason: str
    dropped_count: int | None
    affected_elapsed: AffectedElapsedInterval | None
    affected_kind: _EventKind | None
    affected_scope: DiagnosticScope | None
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, kw_only=True)
class CustomSpanStarted(_EventBase):
    __slots__ = ("name", "parent_span_id", "attributes")
    kind: _ClassVar[_Literal["custom_span_started"]]
    name: str
    parent_span_id: int | None
    attributes: DiagnosticAttributes
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, kw_only=True)
class CustomSpanFinished(_EventBase):
    __slots__ = ("span_id", "outcome")
    kind: _ClassVar[_Literal["custom_span_finished"]]
    span_id: int
    outcome: _SpanOutcome
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, kw_only=True)
class CustomInstantOccurred(_EventBase):
    __slots__ = ("name", "containing_span_id", "severity", "attributes")
    kind: _ClassVar[_Literal["custom_instant_occurred"]]
    name: str
    containing_span_id: int | None
    severity: _CustomSeverity | None
    attributes: DiagnosticAttributes
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, kw_only=True)
class CustomCounterSampled(_EventBase):
    __slots__ = ("name", "value", "unit", "dimensions")
    kind: _ClassVar[_Literal["custom_counter_sampled"]]
    name: str
    value: int | _Decimal
    unit: str | None
    dimensions: DiagnosticDimensions
    def __post_init__(self) -> None: ...

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

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class DiagnosticCapture:
    agent_messages: bool = True
    plans: bool = True
    tool_calls: bool = True
    result_validation: bool = True
    usage: bool = True
    custom_events: bool = True
    tool_inputs: bool = False
    tool_outputs: bool = False
    def __post_init__(self) -> None: ...

@_final
class DiagnosticSinkStateError(RuntimeError):
    def __init__(self, *, code: _SinkStateErrorCode) -> None: ...
    @property
    def code(self) -> _SinkStateErrorCode: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class DiagnosticCallbackFailure:
    kind: _CallbackFailureKind
    event_sequence: int
    exception_type: str | None
    message: str | None
    message_truncated: bool
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class DiagnosticDropCount:
    event_kind: _EventKind
    events: int
    encoded_bytes: int
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class DiagnosticSinkSummary:
    run_id: _UUID
    act_id: str
    act_outcome: _SpanOutcome | None
    close_reason: _SinkCloseReason
    complete: bool
    delivered_events: int
    first_delivered_sequence: int | None
    last_delivered_sequence: int | None
    dropped_events: int
    dropped_bytes: int
    dropped_by_kind: tuple[DiagnosticDropCount, ...]
    source_gaps: int
    truncated_payloads: int
    callback_failure: DiagnosticCallbackFailure | None
    callback_abandoned: bool
    def __post_init__(self) -> None: ...

class DiagnosticSink(_ABC):
    """Observer base accepted by Actor.act(diagnostic_sink=...)."""
    def __init__(self, *, capture: DiagnosticCapture | None = None) -> None: ...
    @property
    def capture(self) -> DiagnosticCapture: ...
    @property
    def state(self) -> _SinkState: ...
    @_abstractmethod
    def on_event(self, event: DiagnosticEvent, /) -> None | _Awaitable[None]: ...
    async def wait_closed(self) -> DiagnosticSinkSummary: ...

@_final
class DiagnosticContextError(RuntimeError): ...

def event(
    name: str,
    /,
    *,
    severity: _CustomSeverity = "info",
    attributes: _Mapping[str, DiagnosticAttributeValue] | None = None,
) -> None: ...

def counter(
    name: str,
    value: int | float | _Decimal,
    /,
    *,
    unit: str | None = None,
    dimensions: _Mapping[str, DiagnosticDimension] | None = None,
) -> None: ...

def span(
    name: str,
    /,
    *,
    attributes: _Mapping[str, DiagnosticAttributeValue] | None = None,
) -> _AbstractContextManager[None]: ...

ViewScalar: _TypeAlias = None | bool | int | float | _Decimal | str

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class SpanSource:
    kind: _SpanKind | None = None
    name: str | None = None
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class InstantSource:
    kind: _InstantKind | None = None
    name: str | None = None
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class CounterSource:
    kind: _CounterKind | None = None
    name: str | None = None
    def __post_init__(self) -> None: ...

TimelineSource: _TypeAlias = SpanSource | InstantSource

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class SeverityFilter:
    filter: _ClassVar[_Literal["severity"]]
    value: _CustomSeverity
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class OutcomeFilter:
    filter: _ClassVar[_Literal["outcome"]]
    value: _SpanOutcome
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class AttributeEqualsFilter:
    filter: _ClassVar[_Literal["attribute_equals"]]
    key: str
    value: ViewScalar
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class AttributeExistsFilter:
    filter: _ClassVar[_Literal["attribute_exists"]]
    key: str
    def __post_init__(self) -> None: ...

ViewFilter: _TypeAlias = (
    SeverityFilter | OutcomeFilter | AttributeEqualsFilter | AttributeExistsFilter
)

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class GroupBy:
    dimension: _GroupDimension
    key: str | None = None
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class CounterValue:
    source: _ClassVar[_Literal["counter_value"]]
    selector: CounterSource
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class InstantCount:
    source: _ClassVar[_Literal["instant_count"]]
    selector: InstantSource
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class CompletedSpanDuration:
    source: _ClassVar[_Literal["completed_span_duration"]]
    selector: SpanSource
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class ActTokenMetric:
    source: _ClassVar[_Literal["act_token"]]
    metric: _TokenMetric
    def __post_init__(self) -> None: ...

MetricSource: _TypeAlias = CounterValue | InstantCount | CompletedSpanDuration | ActTokenMetric

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class EventRows:
    source: _ClassVar[_Literal["event"]]
    kind: _EventKind
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class SpanRows:
    source: _ClassVar[_Literal["span"]]
    selector: SpanSource
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class InstantRows:
    source: _ClassVar[_Literal["instant"]]
    selector: InstantSource
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class CounterRows:
    source: _ClassVar[_Literal["counter"]]
    selector: CounterSource
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class ActTokenUsageRows:
    source: _ClassVar[_Literal["act_token_usage"]]

TableSource: _TypeAlias = EventRows | SpanRows | InstantRows | CounterRows | ActTokenUsageRows

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class TableColumn:
    column: _TableColumnName
    key: str | None = None
    metric: _TokenMetric | None = None
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class TimelineQuery:
    source: TimelineSource
    filters: tuple[ViewFilter, ...] = ()
    group_by: GroupBy | None = None
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class MetricQuery:
    source: MetricSource
    reducer: _Reducer
    filters: tuple[ViewFilter, ...] = ()
    group_by: GroupBy | None = None
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class TableQuery:
    source: TableSource
    columns: tuple[TableColumn, ...]
    page_size: int
    filters: tuple[ViewFilter, ...] = ()
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class TimeSeriesQuery:
    source: MetricSource
    reducer: _Reducer
    filters: tuple[ViewFilter, ...] = ()
    group_by: GroupBy | None = None
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class TimelineView:
    renderer: _ClassVar[_Literal["timeline"]]
    id: str
    title: str
    query: TimelineQuery
    time_range: _ViewTimeRange
    scope: _ViewScope
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class MetricView:
    renderer: _ClassVar[_Literal["metric"]]
    id: str
    title: str
    query: MetricQuery
    time_range: _ViewTimeRange
    scope: _ViewScope
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class TableView:
    renderer: _ClassVar[_Literal["table"]]
    id: str
    title: str
    query: TableQuery
    time_range: _ViewTimeRange
    scope: _ViewScope
    def __post_init__(self) -> None: ...

@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class TimeSeriesView:
    renderer: _ClassVar[_Literal["time_series"]]
    id: str
    title: str
    query: TimeSeriesQuery
    time_range: _ViewTimeRange
    scope: _ViewScope
    def __post_init__(self) -> None: ...

ViewSpec: _TypeAlias = TimelineView | MetricView | TableView | TimeSeriesView

__all__ = [
    "ActTokenMetric",
    "ActTokenUsageFinalized",
    "ActTokenUsageRows",
    "ActorDetail",
    "AffectedElapsedInterval",
    "AgentMessageCompleted",
    "AgentMessageDelta",
    "AgentPlanSnapshot",
    "AgentSessionBrokenDetail",
    "AgentSessionDetail",
    "AgentTurnTerminalDetail",
    "AttributeEqualsFilter",
    "AttributeExistsFilter",
    "CausalLink",
    "CompletedSpanDuration",
    "ContextUsageSampled",
    "CounterRows",
    "CounterSampled",
    "CounterSource",
    "CounterValue",
    "CustomCounterSampled",
    "CustomInstantOccurred",
    "CustomSpanFinished",
    "CustomSpanStarted",
    "DiagnosticAttributeValue",
    "DiagnosticAttributes",
    "DiagnosticCallbackFailure",
    "DiagnosticCapture",
    "DiagnosticComponentFailedDetail",
    "DiagnosticContextError",
    "DiagnosticDimension",
    "DiagnosticDimensions",
    "DiagnosticDropCount",
    "DiagnosticEvent",
    "DiagnosticScalar",
    "DiagnosticScope",
    "DiagnosticSink",
    "DiagnosticSinkStateError",
    "DiagnosticSinkSummary",
    "DiagnosticToolInput",
    "DiagnosticToolLocation",
    "DiagnosticToolOutput",
    "EffectDetail",
    "EmptyDetail",
    "EventRows",
    "FrozenJsonArray",
    "FrozenJsonObject",
    "FrozenJsonValue",
    "GroupBy",
    "InstantCount",
    "InstantDetail",
    "InstantOccurred",
    "InstantRows",
    "InstantSource",
    "MetricQuery",
    "MetricSource",
    "MetricView",
    "ObservationGap",
    "OutcomeFilter",
    "PlanEntry",
    "ProductionConstructDetail",
    "ProductionLoadDetail",
    "ProductionPathResolutionDetail",
    "ResultIssue",
    "ResultTransitionDetail",
    "SeverityFilter",
    "SpanFinished",
    "SpanRows",
    "SpanSource",
    "SpanStartDetail",
    "SpanStarted",
    "TableColumn",
    "TableQuery",
    "TableSource",
    "TableView",
    "TimeSeriesQuery",
    "TimeSeriesView",
    "TimelineQuery",
    "TimelineSource",
    "TimelineView",
    "ToolCallDetail",
    "ViewFilter",
    "ViewScalar",
    "ViewSpec",
    "counter",
    "event",
    "span",
]
