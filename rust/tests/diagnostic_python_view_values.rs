use std::ffi::{CStr, CString};

use pyo3::prelude::*;

#[path = "../src/diagnostic_python/events.rs"]
mod events;
#[path = "../src/diagnostic_python/fragment_test_support.rs"]
mod fragment_test_support;
#[path = "../src/diagnostic_python/views.rs"]
mod views;

fn combined_source() -> CString {
    let mut source = events::source().to_bytes().to_vec();
    source.push(b'\n');
    source.extend_from_slice(views::source().to_bytes());
    CString::new(source).expect("diagnostic fragments must not contain embedded NUL bytes")
}

fn with_fresh_view_module(test: &CStr) {
    pyo3::Python::initialize();
    Python::attach(|py| {
        let source = combined_source();
        let module = fragment_test_support::install_fresh_fragment(
            py,
            &source,
            c"diagnostic-views.py",
            c"_troupe_diagnostic_views_test",
        )
        .expect("install diagnostic event and view fragments");
        module
            .setattr(
                "_timeline_fixture_json",
                include_str!("../../tests/fixtures/diagnostics/views/timeline.json"),
            )
            .unwrap();
        module
            .setattr(
                "_metric_fixture_json",
                include_str!("../../tests/fixtures/diagnostics/views/metric.json"),
            )
            .unwrap();
        module
            .setattr(
                "_table_fixture_json",
                include_str!("../../tests/fixtures/diagnostics/views/table.json"),
            )
            .unwrap();
        module
            .setattr(
                "_timeseries_fixture_json",
                include_str!("../../tests/fixtures/diagnostics/views/timeseries.json"),
            )
            .unwrap();
        let namespace = module.dict();
        py.run(test, Some(&namespace), Some(&namespace))
            .expect("diagnostic Python view assertion failed");
    });
}

#[test]
fn public_values_are_closed_immutable_keyword_only_and_eager() {
    with_fresh_view_module(
        cr#"
import inspect as _inspect
from dataclasses import FrozenInstanceError as _FrozenInstanceError, fields as _fields
from typing import get_args as _get_args

_span = SpanSource(kind="cue.execution")
_instant = InstantSource(name="orders.rejected")
_counter = CounterSource(name="orders.pending")
_filters = (
    OutcomeFilter(value="completed"),
    AttributeExistsFilter(key="region"),
    AttributeEqualsFilter(key="attempt", value=1),
)
_group = GroupBy(dimension="actor")
_timeline_query = TimelineQuery(source=_span, filters=(_filters[0],), group_by=_group)
_metric_query = MetricQuery(
    source=CompletedSpanDuration(selector=_span),
    filters=(_filters[0],),
    group_by=_group,
    reducer="mean",
)
_table_query = TableQuery(
    source=EventRows(kind="agent_message_completed"),
    filters=(),
    columns=(TableColumn(column="sequence"), TableColumn(column="elapsed_ns")),
    page_size=100,
)
_series_query = TimeSeriesQuery(
    source=CounterValue(selector=_counter),
    filters=(_filters[1],),
    group_by=GroupBy(dimension="custom_dimension", key="region"),
    reducer="latest",
)
_views = (
    TimelineView(
        id="timeline", title="Timeline", query=_timeline_query, time_range="run", scope="run"
    ),
    MetricView(
        id="metric", title="Metric", query=_metric_query, time_range="run", scope="selection"
    ),
    TableView(
        id="table", title="Table", query=_table_query, time_range="viewport", scope="run"
    ),
    TimeSeriesView(
        id="series", title="Series", query=_series_query,
        time_range="viewport", scope="selection",
    ),
)

_classes_and_values = (
    (SpanSource, _span),
    (InstantSource, _instant),
    (CounterSource, _counter),
    (SeverityFilter, SeverityFilter(value="warning")),
    (OutcomeFilter, _filters[0]),
    (AttributeEqualsFilter, _filters[2]),
    (AttributeExistsFilter, _filters[1]),
    (GroupBy, _group),
    (CounterValue, CounterValue(selector=_counter)),
    (InstantCount, InstantCount(selector=_instant)),
    (CompletedSpanDuration, _metric_query.source),
    (ActTokenMetric, ActTokenMetric(metric="input_tokens")),
    (EventRows, _table_query.source),
    (SpanRows, SpanRows(selector=_span)),
    (InstantRows, InstantRows(selector=_instant)),
    (CounterRows, CounterRows(selector=_counter)),
    (ActTokenUsageRows, ActTokenUsageRows()),
    (TableColumn, _table_query.columns[0]),
    (TimelineQuery, _timeline_query),
    (MetricQuery, _metric_query),
    (TableQuery, _table_query),
    (TimeSeriesQuery, _series_query),
    (TimelineView, _views[0]),
    (MetricView, _views[1]),
    (TableView, _views[2]),
    (TimeSeriesView, _views[3]),
)
for _class, _value in _classes_and_values:
    assert _class.__dataclass_params__.frozen
    assert "__slots__" in _class.__dict__
    assert not hasattr(_value, "__dict__")
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

assert tuple(_get_args(TimelineSource)) == (SpanSource, InstantSource)
assert tuple(_get_args(MetricSource)) == (
    CounterValue, InstantCount, CompletedSpanDuration, ActTokenMetric,
)
assert tuple(_get_args(TableSource)) == (
    EventRows, SpanRows, InstantRows, CounterRows, ActTokenUsageRows,
)
assert tuple(_get_args(ViewFilter)) == (
    SeverityFilter, OutcomeFilter, AttributeEqualsFilter, AttributeExistsFilter,
)
assert tuple(_get_args(ViewSpec)) == (TimelineView, MetricView, TableView, TimeSeriesView)
assert tuple(view.renderer for view in _views) == ("timeline", "metric", "table", "time_series")
assert tuple(field.name for field in _fields(TimelineView)) == (
    "id", "title", "query", "time_range", "scope",
)
assert not any(hasattr(view, "compile") for view in _views)

try:
    _views[0].title = "changed"
except _FrozenInstanceError:
    pass
else:
    raise AssertionError("ViewSpec was mutable")

class _TupleSubclass(tuple):
    pass

for _bad_filters in ([], (_filter for _filter in ()), _TupleSubclass()):
    try:
        TimelineQuery(source=_span, filters=_bad_filters)
    except TypeError:
        pass
    else:
        raise AssertionError("query retained a mutable, lazy, or subclass container")
"#,
    );
}

#[test]
fn all_four_c05_fixture_descriptors_compile_byte_exact() {
    with_fresh_view_module(
        cr#"
_fixture_views = (
    (
        _timeline_fixture_json,
        TimelineView(
            id="cue_timeline",
            title="Cue timeline",
            time_range="run",
            scope="run",
            query=TimelineQuery(
                source=SpanSource(kind="cue.execution"),
                filters=(OutcomeFilter(value="completed"),),
                group_by=GroupBy(dimension="actor"),
            ),
        ),
    ),
    (
        _metric_fixture_json,
        MetricView(
            id="act_input_mean",
            title="Act input mean",
            time_range="run",
            scope="run",
            query=MetricQuery(
                source=ActTokenMetric(metric="input_tokens"),
                filters=(),
                group_by=GroupBy(dimension="act"),
                reducer="mean",
            ),
        ),
    ),
    (
        _table_fixture_json,
        TableView(
            id="message_table",
            title="Completed messages",
            time_range="run",
            scope="run",
            query=TableQuery(
                source=EventRows(kind="agent_message_completed"),
                filters=(),
                columns=(TableColumn(column="sequence"), TableColumn(column="elapsed_ns")),
                page_size=500,
            ),
        ),
    ),
    (
        _timeseries_fixture_json,
        TimeSeriesView(
            id="queue_depth",
            title="Queue depth",
            time_range="viewport",
            scope="selection",
            query=TimeSeriesQuery(
                source=CounterValue(selector=CounterSource(name="example.queue_depth")),
                filters=(AttributeExistsFilter(key="queue"),),
                group_by=GroupBy(dimension="custom_dimension", key="queue"),
                reducer="max",
            ),
        ),
    ),
)

def _assert_pure_json(value):
    if type(value) is dict:
        assert all(type(key) is str for key in value)
        for item in value.values():
            _assert_pure_json(item)
        return
    if type(value) is list:
        for item in value:
            _assert_pure_json(item)
        return
    assert value is None or type(value) in (bool, int, str)

for _fixture_json, _view in _fixture_views:
    _expected = _json.loads(_fixture_json)["descriptor"]
    _expected_bytes = _json.dumps(
        _expected, ensure_ascii=False, allow_nan=False, separators=(",", ":")
    ).encode("utf-8")
    _compiled = _view_to_mapping(_view)
    _assert_pure_json(_compiled)
    assert _compiled == _expected
    assert _view_to_json_bytes(_view) == _expected_bytes
    assert _view_to_json_bytes(_view) == _view_to_json_bytes(_view)

    _compiled["title"] = "mutated mapping"
    assert _view.title != "mutated mapping"
    assert _view_to_mapping(_view) == _expected
"#,
    );
}

#[test]
fn closed_values_and_renderer_compatibility_fail_eagerly() {
    with_fresh_view_module(
        cr#"
from decimal import Decimal as _Decimal

def _raises(error, callable, *args, **kwargs):
    try:
        callable(*args, **kwargs)
    except error:
        return
    raise AssertionError(f"{callable!r} accepted invalid input")

class _StringSubclass(str):
    pass

_raises(TypeError, SpanSource, kind=_StringSubclass("cue.execution"))
_raises(ValueError, SpanSource)
_raises(ValueError, SpanSource, kind="cue.execution", name="orders.custom")
_raises(ValueError, SpanSource, kind="future.span")
_raises(ValueError, SpanSource, name="single")
_raises(ValueError, SpanSource, name="troupe.reserved")
_raises(ValueError, InstantSource, kind="future.instant")
_raises(ValueError, CounterSource, kind="future.counter")

_raises(TypeError, SeverityFilter, value=1)
_raises(ValueError, SeverityFilter, value="fatal")
_raises(ValueError, OutcomeFilter, value="unknown")
_raises(TypeError, AttributeEqualsFilter, key="value", value=[1])
_raises(TypeError, AttributeEqualsFilter, key="value", value={"nested": 1})
_raises(TypeError, AttributeEqualsFilter, key="value", value=lambda: None)
_raises(ValueError, AttributeEqualsFilter, key="value", value=float("nan"))
_raises(ValueError, AttributeEqualsFilter, key="value", value=_Decimal("Infinity"))
_raises(ValueError, AttributeExistsFilter, key="")

_raises(ValueError, GroupBy, dimension="future")
_raises(ValueError, GroupBy, dimension="actor", key="extra")
_raises(ValueError, GroupBy, dimension="attribute")
_raises(ValueError, TableColumn, column="future")
_raises(ValueError, TableColumn, column="sequence", key="extra")
_raises(ValueError, TableColumn, column="attribute")
_raises(ValueError, TableColumn, column="token")
_raises(ValueError, TableColumn, column="token", metric="future")

_built_in_span = SpanSource(kind="cue.execution")
_custom_span = SpanSource(name="orders.operation")
_built_in_instant = InstantSource(kind="cue.admitted")
_custom_instant = InstantSource(name="orders.rejected")
_built_in_counter = CounterSource(kind="cue.active")
_custom_counter = CounterSource(name="orders.pending")

_raises(
    ValueError,
    TimelineQuery,
    source=_built_in_span,
    filters=(SeverityFilter(value="info"),),
)
_raises(
    ValueError,
    TimelineQuery,
    source=_built_in_instant,
    filters=(OutcomeFilter(value="completed"),),
)
_raises(
    ValueError,
    TimelineQuery,
    source=_custom_instant,
    filters=(OutcomeFilter(value="completed"),),
)
_raises(
    ValueError,
    TimelineQuery,
    source=_custom_span,
    filters=(SeverityFilter(value="info"),),
)
_raises(TypeError, TimelineQuery, source=CounterValue(selector=_custom_counter))
_raises(
    ValueError,
    TimelineQuery,
    source=_built_in_span,
    group_by=GroupBy(dimension="custom_name"),
)
_raises(
    ValueError,
    MetricQuery,
    source=CounterValue(selector=_built_in_counter),
    filters=(AttributeExistsFilter(key="region"),),
    reducer="latest",
)
_raises(
    ValueError,
    MetricQuery,
    source=CounterValue(selector=_custom_counter),
    group_by=GroupBy(dimension="attribute", key="region"),
    reducer="future",
)
_raises(
    ValueError,
    MetricQuery,
    source=InstantCount(selector=_built_in_instant),
    reducer="mean",
)
_raises(
    ValueError,
    TimeSeriesQuery,
    source=InstantCount(selector=_custom_instant),
    reducer="sum",
)
_raises(TypeError, MetricQuery, source=_custom_counter, reducer="latest")

_raises(TypeError, TableQuery, source=_built_in_span, columns=(TableColumn(column="sequence"),), page_size=1)
_raises(TypeError, TableQuery, source=EventRows(kind="span_started"), columns=[], page_size=1)
_raises(TypeError, TableQuery, source=EventRows(kind="span_started"), columns=(item for item in ()), page_size=1)
_raises(ValueError, TableQuery, source=EventRows(kind="span_started"), columns=(), page_size=1)
_raises(ValueError, EventRows, kind="future_event")

_query = TimelineQuery(source=_built_in_span)
for _kwargs in (
    {"id": "", "title": "Title", "query": _query, "time_range": "run", "scope": "run"},
    {"id": "Upper", "title": "Title", "query": _query, "time_range": "run", "scope": "run"},
    {"id": "view", "title": "", "query": _query, "time_range": "run", "scope": "run"},
    {"id": "view", "title": "<script>x</script>", "query": _query, "time_range": "run", "scope": "run"},
    {"id": "view", "title": "https://example.invalid", "query": _query, "time_range": "run", "scope": "run"},
    {"id": "view", "title": "Title", "query": _query, "time_range": "future", "scope": "run"},
    {"id": "view", "title": "Title", "query": _query, "time_range": "run", "scope": "future"},
):
    _raises(ValueError, TimelineView, **_kwargs)

_raises(TypeError, TimelineView, id="view", title="Title", query=object(), time_range="run", scope="run")
_raises(TypeError, TimelineView, id="view", title="Title", query=_query, time_range="run", scope="run", renderer="custom")
_raises(TypeError, TimelineQuery, source=_built_in_span, sql="select 1")
_raises(TypeError, TimelineQuery, source=_built_in_span, regex=".*")
_raises(TypeError, TimelineQuery, source=_built_in_span, join=object())
_raises(TypeError, TimelineQuery, source=_built_in_span, callback=lambda row: row)
"#,
    );
}

#[test]
fn resource_bounds_scalar_normalization_and_pure_compilation_are_exact() {
    with_fresh_view_module(
        cr#"
from decimal import Decimal as _Decimal

def _raises(error, callable, *args, **kwargs):
    try:
        callable(*args, **kwargs)
    except error:
        return
    raise AssertionError(f"{callable!r} accepted invalid input")

_source = SpanSource(name="orders.operation")
_query = TimelineQuery(source=_source)
for _length in (63, 64):
    TimelineView(
        id="v" * _length, title="Title", query=_query, time_range="run", scope="run"
    )
_raises(
    ValueError,
    TimelineView,
    id="v" * 65,
    title="Title",
    query=_query,
    time_range="run",
    scope="run",
)

for _length in (127, 128):
    TimelineView(
        id="view", title="T" * _length, query=_query, time_range="run", scope="run"
    )
_raises(
    ValueError,
    TimelineView,
    id="view",
    title="T" * 129,
    query=_query,
    time_range="run",
    scope="run",
)
_raises(
    ValueError,
    TimelineView,
    id="view",
    title="\u84dd" * 43,
    query=_query,
    time_range="run",
    scope="run",
)

for _length in (63, 64):
    AttributeExistsFilter(key="k" * _length)
_raises(ValueError, AttributeExistsFilter, key="k" * 65)
for _forbidden in ("line\nbreak", "`code`", "javascript:x", "data:text/html,x", "url(x)", "@import x"):
    _raises(ValueError, AttributeExistsFilter, key=_forbidden)

_filter = AttributeExistsFilter(key="region")
for _count in (31, 32):
    TimelineQuery(source=_source, filters=(_filter,) * _count)
_raises(ValueError, TimelineQuery, source=_source, filters=(_filter,) * 33)

_column = TableColumn(column="sequence")
_rows = EventRows(kind="span_started")
for _count in (31, 32):
    TableQuery(source=_rows, columns=(_column,) * _count, page_size=1)
_raises(ValueError, TableQuery, source=_rows, columns=(_column,) * 33, page_size=1)
for _page_size in (1, 500):
    TableQuery(source=_rows, columns=(_column,), page_size=_page_size)
for _page_size in (0, 501):
    _raises(ValueError, TableQuery, source=_rows, columns=(_column,), page_size=_page_size)
_raises(TypeError, TableQuery, source=_rows, columns=(_column,), page_size=True)

_normalized = AttributeEqualsFilter(key="ratio", value=0.1)
assert _normalized.value == _Decimal("0.1")
assert type(_normalized.value) is _Decimal
_exact = AttributeEqualsFilter(key="ratio", value=_Decimal("1.2300"))
assert _exact.value == _Decimal("1.23")
_huge = 10 ** 5000
_integer = AttributeEqualsFilter(key="integer", value=_huge)
assert _integer.value == _huge and type(_integer.value) is int

_view = TimelineView(
    id="normalized",
    title="Normalized",
    time_range="run",
    scope="run",
    query=TimelineQuery(
        source=_source,
        filters=(_normalized, _exact, _integer),
        group_by=GroupBy(dimension="attribute", key="ratio"),
    ),
)
_record = _view_to_mapping(_view)
_filters = _record["query"]["filters"]
assert _filters[0]["value"] == {"type": "decimal", "value": "0.1"}
assert _filters[1]["value"] == {"type": "decimal", "value": "1.23"}
assert _filters[2]["value"] == {"type": "integer", "value": "1" + "0" * 5000}
assert b"normalized" in _view_to_json_bytes(_view)

_raises(ValueError, AttributeEqualsFilter, key="value", value="bad\ud800")
"#,
    );
}
