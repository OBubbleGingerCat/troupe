from __future__ import annotations

from troupe import Production, diagnostics


CUE_TIMELINE = diagnostics.TimelineView(
    id="cue_timeline",
    title="Cue execution",
    query=diagnostics.TimelineQuery(
        source=diagnostics.SpanSource(kind="cue.execution"),
        filters=(diagnostics.OutcomeFilter(value="completed"),),
        group_by=diagnostics.GroupBy(dimension="actor"),
    ),
    time_range="run",
    scope="run",
)

ACT_INPUT_TOKENS = diagnostics.MetricView(
    id="act_input_tokens",
    title="Act input tokens",
    query=diagnostics.MetricQuery(
        source=diagnostics.ActTokenMetric(metric="input_tokens"),
        reducer="sum",
        group_by=diagnostics.GroupBy(dimension="act"),
    ),
    time_range="run",
    scope="selection",
)

USAGE_TABLE = diagnostics.TableView(
    id="act_usage",
    title="Act usage",
    query=diagnostics.TableQuery(
        source=diagnostics.ActTokenUsageRows(),
        columns=(
            diagnostics.TableColumn(column="sequence"),
            diagnostics.TableColumn(column="act_id"),
            diagnostics.TableColumn(column="token", metric="input_tokens"),
        ),
        page_size=100,
    ),
    time_range="viewport",
    scope="run",
)

QUEUE_DEPTH = diagnostics.TimeSeriesView(
    id="queue_depth",
    title="Queue depth",
    query=diagnostics.TimeSeriesQuery(
        source=diagnostics.CounterValue(
            selector=diagnostics.CounterSource(name="example.queue_depth")
        ),
        reducer="max",
        group_by=diagnostics.GroupBy(
            dimension="custom_dimension",
            key="region",
        ),
    ),
    time_range="viewport",
    scope="selection",
)

DIAGNOSTIC_VIEWS: tuple[diagnostics.ViewSpec, ...] = (
    CUE_TIMELINE,
    ACT_INPUT_TOKENS,
    USAGE_TABLE,
    QUEUE_DEPTH,
)


class ObservedProduction(Production):
    diagnostic_views = DIAGNOSTIC_VIEWS


def main() -> None:
    assert tuple(view.renderer for view in DIAGNOSTIC_VIEWS) == (
        "timeline",
        "metric",
        "table",
        "time_series",
    )


if __name__ == "__main__":
    main()
