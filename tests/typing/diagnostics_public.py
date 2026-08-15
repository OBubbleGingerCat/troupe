from __future__ import annotations

from collections.abc import Awaitable
from decimal import Decimal
from typing import Literal
from uuid import UUID

import troupe.diagnostics as imported_diagnostics
from troupe import diagnostics
from typing_extensions import assert_type


assert imported_diagnostics is diagnostics

PUBLIC_CLASSES: tuple[type[object], ...] = (
    diagnostics.ActTokenMetric,
    diagnostics.ActTokenUsageFinalized,
    diagnostics.ActTokenUsageRows,
    diagnostics.ActorDetail,
    diagnostics.AffectedElapsedInterval,
    diagnostics.AgentMessageCompleted,
    diagnostics.AgentMessageDelta,
    diagnostics.AgentPlanSnapshot,
    diagnostics.AgentSessionBrokenDetail,
    diagnostics.AgentSessionDetail,
    diagnostics.AgentTurnTerminalDetail,
    diagnostics.AttributeEqualsFilter,
    diagnostics.AttributeExistsFilter,
    diagnostics.CausalLink,
    diagnostics.CompletedSpanDuration,
    diagnostics.ContextUsageSampled,
    diagnostics.CounterRows,
    diagnostics.CounterSampled,
    diagnostics.CounterSource,
    diagnostics.CounterValue,
    diagnostics.CustomCounterSampled,
    diagnostics.CustomInstantOccurred,
    diagnostics.CustomSpanFinished,
    diagnostics.CustomSpanStarted,
    diagnostics.DiagnosticCallbackFailure,
    diagnostics.DiagnosticCapture,
    diagnostics.DiagnosticComponentFailedDetail,
    diagnostics.DiagnosticContextError,
    diagnostics.DiagnosticDropCount,
    diagnostics.DiagnosticScope,
    diagnostics.DiagnosticSink,
    diagnostics.DiagnosticSinkStateError,
    diagnostics.DiagnosticSinkSummary,
    diagnostics.DiagnosticToolInput,
    diagnostics.DiagnosticToolLocation,
    diagnostics.DiagnosticToolOutput,
    diagnostics.EffectDetail,
    diagnostics.EmptyDetail,
    diagnostics.EventRows,
    diagnostics.FrozenJsonArray,
    diagnostics.FrozenJsonObject,
    diagnostics.GroupBy,
    diagnostics.InstantCount,
    diagnostics.InstantOccurred,
    diagnostics.InstantRows,
    diagnostics.InstantSource,
    diagnostics.MetricQuery,
    diagnostics.MetricView,
    diagnostics.ObservationGap,
    diagnostics.OutcomeFilter,
    diagnostics.PlanEntry,
    diagnostics.ProductionConstructDetail,
    diagnostics.ProductionLoadDetail,
    diagnostics.ProductionPathResolutionDetail,
    diagnostics.ResultIssue,
    diagnostics.ResultTransitionDetail,
    diagnostics.SeverityFilter,
    diagnostics.SpanFinished,
    diagnostics.SpanRows,
    diagnostics.SpanSource,
    diagnostics.SpanStarted,
    diagnostics.TableColumn,
    diagnostics.TableQuery,
    diagnostics.TableView,
    diagnostics.TimeSeriesQuery,
    diagnostics.TimeSeriesView,
    diagnostics.TimelineQuery,
    diagnostics.TimelineView,
    diagnostics.ToolCallDetail,
)

frozen_json: diagnostics.FrozenJsonValue = diagnostics.FrozenJsonObject(
    entries=(
        (
            "payload",
            diagnostics.FrozenJsonArray(items=(None, True, 7, Decimal("1.25"), "text")),
        ),
    )
)
tool_input = diagnostics.DiagnosticToolInput(raw_input=frozen_json, truncated=False)
tool_output = diagnostics.DiagnosticToolOutput(
    raw_output=None,
    content=("complete",),
    locations=(diagnostics.DiagnosticToolLocation(path="src/main.py", line=7),),
    truncated=False,
)
tool_detail = diagnostics.ToolCallDetail(
    title="Inspect repository",
    tool_kind="read",
    status="completed",
    error_code=None,
    captured_input=tool_input,
    captured_output=tool_output,
)
assert_type(tool_detail.captured_input, diagnostics.DiagnosticToolInput | None)
assert_type(tool_detail.captured_output, diagnostics.DiagnosticToolOutput | None)

capture = diagnostics.DiagnosticCapture(
    agent_messages=True,
    plans=True,
    tool_calls=True,
    result_validation=True,
    usage=True,
    custom_events=True,
    tool_inputs=True,
    tool_outputs=True,
)


class SyncSink(diagnostics.DiagnosticSink):
    def __init__(self) -> None:
        super().__init__(capture=capture)
        self.events: list[diagnostics.DiagnosticEvent] = []

    def on_event(self, event: diagnostics.DiagnosticEvent, /) -> None:
        self.events.append(event)


class AsyncSink(diagnostics.DiagnosticSink):
    async def on_event(self, event: diagnostics.DiagnosticEvent, /) -> None:
        _ = event


sync_sink = SyncSink()
async_sink = AsyncSink()
sink_state: Literal["UNBOUND", "BOUND", "SEALED", "CLOSED"] = sync_sink.state
assert_type(sync_sink.capture, diagnostics.DiagnosticCapture)
summary_wait: Awaitable[diagnostics.DiagnosticSinkSummary] = async_sink.wait_closed()

scope = diagnostics.DiagnosticScope(act_id="act-1")
event_value: diagnostics.DiagnosticEvent = diagnostics.AgentMessageDelta(
    schema_version=1,
    run_id=UUID("12345678-1234-4234-9234-123456789abc"),
    sequence=1,
    elapsed_ns=0,
    scope=scope,
    caused_by=(),
    message_id="message-1",
    source_message_id=None,
    text_delta="hello",
)
span_detail: diagnostics.SpanStartDetail = tool_detail
instant_detail: diagnostics.InstantDetail = tool_detail
diagnostic_scalar: diagnostics.DiagnosticScalar = 0.5
attribute_value: diagnostics.DiagnosticAttributeValue = [None, 1, Decimal("2.5")]
dimension_value: diagnostics.DiagnosticDimension = Decimal("3.5")
attributes: diagnostics.DiagnosticAttributes = (("key", (None, 1, Decimal("2.5"))),)
dimensions: diagnostics.DiagnosticDimensions = (("key", Decimal("3.5")),)
_diagnostic_values = (
    PUBLIC_CLASSES,
    event_value,
    span_detail,
    instant_detail,
    diagnostic_scalar,
    attribute_value,
    dimension_value,
    attributes,
    dimensions,
    sink_state,
    summary_wait,
)


def publish_custom_events() -> None:
    diagnostics.event(
        "example.completed",
        severity="info",
        attributes={"attempt": 1, "ratios": [0.5, Decimal("0.75")]},
    )
    diagnostics.counter(
        "example.queue_depth",
        3,
        unit="items",
        dimensions={"queue": "ready"},
    )
    with diagnostics.span("example.operation", attributes={"region": "east"}):
        pass


timeline_source: diagnostics.TimelineSource = diagnostics.SpanSource(kind="cue.execution")
view_filter: diagnostics.ViewFilter = diagnostics.OutcomeFilter(value="completed")
timeline = diagnostics.TimelineView(
    id="cue_timeline",
    title="Cue timeline",
    query=diagnostics.TimelineQuery(
        source=timeline_source,
        filters=(view_filter,),
        group_by=diagnostics.GroupBy(dimension="actor"),
    ),
    time_range="run",
    scope="run",
)

metric_source: diagnostics.MetricSource = diagnostics.ActTokenMetric(metric="input_tokens")
metric = diagnostics.MetricView(
    id="act_input_mean",
    title="Act input mean",
    query=diagnostics.MetricQuery(
        source=metric_source,
        reducer="mean",
        group_by=diagnostics.GroupBy(dimension="act"),
    ),
    time_range="run",
    scope="selection",
)

table_source: diagnostics.TableSource = diagnostics.EventRows(
    kind="agent_message_completed"
)
table = diagnostics.TableView(
    id="message_table",
    title="Completed messages",
    query=diagnostics.TableQuery(
        source=table_source,
        columns=(
            diagnostics.TableColumn(column="sequence"),
            diagnostics.TableColumn(column="elapsed_ns"),
        ),
        page_size=500,
    ),
    time_range="viewport",
    scope="run",
)

series_source = diagnostics.CounterValue(
    selector=diagnostics.CounterSource(name="example.queue_depth")
)
series = diagnostics.TimeSeriesView(
    id="queue_depth",
    title="Queue depth",
    query=diagnostics.TimeSeriesQuery(
        source=series_source,
        reducer="max",
        filters=(diagnostics.AttributeExistsFilter(key="queue"),),
        group_by=diagnostics.GroupBy(dimension="custom_dimension", key="queue"),
    ),
    time_range="viewport",
    scope="selection",
)

view_scalar: diagnostics.ViewScalar = Decimal("1.5")
views: tuple[diagnostics.ViewSpec, ...] = (timeline, metric, table, series)
_view_values = (view_scalar, views)
