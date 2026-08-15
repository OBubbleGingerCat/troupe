use std::ffi::CStr;

const SOURCE: &CStr = cr#"
import math as _math
import unicodedata as _unicodedata


_VIEW_SCHEMA_VERSION = 1
_MAX_VIEW_ID_BYTES = 64
_MAX_VIEW_TITLE_BYTES = 128
_MAX_VIEW_FILTERS = 32
_MAX_TABLE_COLUMNS = 32
_MAX_TABLE_PAGE_SIZE = 500
_VIEW_ID = _re.compile(r"[a-z][a-z0-9_]*\Z")

_VIEW_TIME_RANGES = frozenset(("viewport", "run"))
_VIEW_SCOPES = frozenset(("selection", "run"))
_VIEW_REDUCERS = frozenset(("count", "sum", "min", "max", "mean", "latest"))
_VIEW_TOKEN_METRICS = frozenset((
    "provider_total_tokens",
    "input_tokens",
    "output_tokens",
    "thought_tokens",
    "cached_read_tokens",
    "cached_write_tokens",
))
_VIEW_GROUP_DIMENSIONS = frozenset((
    "scene",
    "actor",
    "cue",
    "act",
    "event_name",
    "custom_name",
    "attribute",
    "custom_dimension",
))
_VIEW_TABLE_COLUMNS = frozenset((
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
))

ViewScalar: _TypeAlias = _NoneType | bool | int | float | _Decimal | str


def _view_utf8_length(value, field):
    try:
        return len(value.encode("utf-8"))
    except UnicodeEncodeError as error:
        raise ValueError(f"{field} must be valid UTF-8 text") from error


def _contains_forbidden_view_markup(value):
    lower = value.lower()
    return (
        any(character in value for character in ("<", ">", "`"))
        or "javascript:" in lower
        or "data:text/html" in lower
        or "http://" in lower
        or "https://" in lower
        or "url(" in lower
        or "@import" in lower
    )


def _require_view_id(value):
    _require_exact(value, str, "view id")
    if (
        not value
        or _view_utf8_length(value, "view id") > _MAX_VIEW_ID_BYTES
        or _VIEW_ID.fullmatch(value) is None
    ):
        raise ValueError("view id is invalid")
    return value


def _require_view_plain_text(value, field, maximum_bytes):
    _require_exact(value, str, field)
    if (
        not value
        or _view_utf8_length(value, field) > maximum_bytes
        or any(_unicodedata.category(character) == "Cc" for character in value)
    ):
        raise ValueError(f"{field} is out of bounds")
    if _contains_forbidden_view_markup(value):
        raise ValueError(f"{field} must be plain text")
    return value


def _require_view_key(value):
    value = _require_view_plain_text(value, "view attribute key", _MAX_CUSTOM_KEY_BYTES)
    return value


def _normalize_view_scalar(value):
    if value is None or type(value) in (bool, int):
        return value
    if type(value) is float:
        if not _math.isfinite(value):
            raise ValueError("view scalar float must be finite")
        return _normalize_decimal(_Decimal(repr(value)))
    if type(value) is _Decimal:
        return _normalize_decimal(value)
    if type(value) is str:
        _view_utf8_length(value, "view scalar string")
        return value
    raise TypeError("view scalar has an unsupported type")


def _require_view_selector(kind, name, kinds, field):
    if kind is None and name is None:
        raise ValueError(f"{field} requires exactly one of kind or name")
    if kind is not None and name is not None:
        raise ValueError(f"{field} requires exactly one of kind or name")
    if kind is not None:
        _require_enum(kind, kinds, f"{field} kind")
    else:
        _require_custom_name(name)


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class SpanSource:
    kind: str | None = None
    name: str | None = None

    def __post_init__(self):
        _require_view_selector(self.kind, self.name, _SPAN_KINDS, "span source")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class InstantSource:
    kind: str | None = None
    name: str | None = None

    def __post_init__(self):
        _require_view_selector(self.kind, self.name, _INSTANT_KINDS, "instant source")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class CounterSource:
    kind: str | None = None
    name: str | None = None

    def __post_init__(self):
        _require_view_selector(self.kind, self.name, _COUNTER_KINDS, "counter source")


TimelineSource: _TypeAlias = SpanSource | InstantSource


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class SeverityFilter:
    filter: _ClassVar[_Literal["severity"]] = "severity"
    value: str

    def __post_init__(self):
        _require_enum(self.value, _CUSTOM_SEVERITIES, "severity filter")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class OutcomeFilter:
    filter: _ClassVar[_Literal["outcome"]] = "outcome"
    value: str

    def __post_init__(self):
        _require_enum(self.value, _SPAN_OUTCOMES, "outcome filter")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class AttributeEqualsFilter:
    filter: _ClassVar[_Literal["attribute_equals"]] = "attribute_equals"
    key: str
    value: ViewScalar

    def __post_init__(self):
        _require_view_key(self.key)
        object.__setattr__(self, "value", _normalize_view_scalar(self.value))


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class AttributeExistsFilter:
    filter: _ClassVar[_Literal["attribute_exists"]] = "attribute_exists"
    key: str

    def __post_init__(self):
        _require_view_key(self.key)


ViewFilter: _TypeAlias = (
    SeverityFilter | OutcomeFilter | AttributeEqualsFilter | AttributeExistsFilter
)
_VIEW_FILTER_CLASSES = (
    SeverityFilter,
    OutcomeFilter,
    AttributeEqualsFilter,
    AttributeExistsFilter,
)


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class GroupBy:
    dimension: str
    key: str | None = None

    def __post_init__(self):
        _require_enum(self.dimension, _VIEW_GROUP_DIMENSIONS, "group dimension")
        keyed = self.dimension in ("attribute", "custom_dimension")
        if keyed:
            if self.key is None:
                raise ValueError("attribute group dimension requires a key")
            _require_view_key(self.key)
        elif self.key is not None:
            raise ValueError("closed group dimension cannot carry a key")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class CounterValue:
    source: _ClassVar[_Literal["counter_value"]] = "counter_value"
    selector: CounterSource

    def __post_init__(self):
        if type(self.selector) is not CounterSource:
            raise TypeError("counter value source requires an exact CounterSource")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class InstantCount:
    source: _ClassVar[_Literal["instant_count"]] = "instant_count"
    selector: InstantSource

    def __post_init__(self):
        if type(self.selector) is not InstantSource:
            raise TypeError("instant count source requires an exact InstantSource")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class CompletedSpanDuration:
    source: _ClassVar[_Literal["completed_span_duration"]] = "completed_span_duration"
    selector: SpanSource

    def __post_init__(self):
        if type(self.selector) is not SpanSource:
            raise TypeError("span duration source requires an exact SpanSource")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class ActTokenMetric:
    source: _ClassVar[_Literal["act_token"]] = "act_token"
    metric: str

    def __post_init__(self):
        _require_enum(self.metric, _VIEW_TOKEN_METRICS, "token metric")


MetricSource: _TypeAlias = CounterValue | InstantCount | CompletedSpanDuration | ActTokenMetric
_METRIC_SOURCE_CLASSES = (CounterValue, InstantCount, CompletedSpanDuration, ActTokenMetric)


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class EventRows:
    source: _ClassVar[_Literal["event"]] = "event"
    kind: str

    def __post_init__(self):
        _require_enum(self.kind, _EVENT_KINDS, "event row kind")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class SpanRows:
    source: _ClassVar[_Literal["span"]] = "span"
    selector: SpanSource

    def __post_init__(self):
        if type(self.selector) is not SpanSource:
            raise TypeError("span rows require an exact SpanSource")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class InstantRows:
    source: _ClassVar[_Literal["instant"]] = "instant"
    selector: InstantSource

    def __post_init__(self):
        if type(self.selector) is not InstantSource:
            raise TypeError("instant rows require an exact InstantSource")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class CounterRows:
    source: _ClassVar[_Literal["counter"]] = "counter"
    selector: CounterSource

    def __post_init__(self):
        if type(self.selector) is not CounterSource:
            raise TypeError("counter rows require an exact CounterSource")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class ActTokenUsageRows:
    source: _ClassVar[_Literal["act_token_usage"]] = "act_token_usage"


TableSource: _TypeAlias = EventRows | SpanRows | InstantRows | CounterRows | ActTokenUsageRows
_TABLE_SOURCE_CLASSES = (EventRows, SpanRows, InstantRows, CounterRows, ActTokenUsageRows)


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class TableColumn:
    column: str
    key: str | None = None
    metric: str | None = None

    def __post_init__(self):
        _require_enum(self.column, _VIEW_TABLE_COLUMNS, "table column")
        if self.column == "attribute":
            if self.key is None or self.metric is not None:
                raise ValueError("attribute table column requires only a key")
            _require_view_key(self.key)
        elif self.column == "token":
            if self.metric is None or self.key is not None:
                raise ValueError("token table column requires only a token metric")
            _require_enum(self.metric, _VIEW_TOKEN_METRICS, "table token metric")
        elif self.key is not None or self.metric is not None:
            raise ValueError("closed table column cannot carry key or metric data")


def _require_exact_view_value(value, classes, field):
    if type(value) not in classes:
        names = ", ".join(value.__name__ for value in classes)
        raise TypeError(f"{field} must be an exact closed value: {names}")
    return value


def _require_view_tuple(values, classes, field, maximum, *, nonempty=False):
    if type(values) is not tuple:
        raise TypeError(f"{field} must be an exact tuple")
    if (nonempty and not values) or len(values) > maximum:
        raise ValueError(f"{field} count is out of bounds")
    for value in values:
        _require_exact_view_value(value, classes, field)
    return values


def _view_source_shape(source):
    outcome = False
    severity = False
    scalar_fields = False
    custom_name = False
    custom_dimensions = False

    selector = None
    if type(source) in (SpanSource, InstantSource, CounterSource):
        selector = source
    elif type(source) in (CounterValue, InstantCount, CompletedSpanDuration):
        selector = source.selector
    elif type(source) in (SpanRows, InstantRows, CounterRows):
        selector = source.selector

    if type(selector) is SpanSource:
        outcome = True
        if selector.name is not None:
            scalar_fields = True
            custom_name = True
    elif type(selector) is InstantSource:
        if selector.name is not None:
            severity = True
            scalar_fields = True
            custom_name = True
    elif type(selector) is CounterSource:
        if selector.name is not None:
            scalar_fields = True
            custom_name = True
            custom_dimensions = True
    elif type(source) is EventRows:
        if source.kind == "custom_span_started":
            scalar_fields = True
            custom_name = True
        elif source.kind == "custom_span_finished":
            outcome = True
            custom_name = True
        elif source.kind == "custom_instant_occurred":
            severity = True
            scalar_fields = True
            custom_name = True
        elif source.kind == "custom_counter_sampled":
            scalar_fields = True
            custom_name = True
            custom_dimensions = True
        elif source.kind == "span_finished":
            outcome = True

    return outcome, severity, scalar_fields, custom_name, custom_dimensions


def _validate_view_filters(filters, source):
    _require_view_tuple(filters, _VIEW_FILTER_CLASSES, "query filters", _MAX_VIEW_FILTERS)
    outcome, severity, scalar_fields, _, _ = _view_source_shape(source)
    for value in filters:
        compatible = (
            (type(value) is OutcomeFilter and outcome)
            or (type(value) is SeverityFilter and severity)
            or (type(value) in (AttributeEqualsFilter, AttributeExistsFilter) and scalar_fields)
        )
        if not compatible:
            raise ValueError("filter is incompatible with query source")


def _validate_view_group(group_by, source):
    if group_by is None:
        return
    if type(group_by) is not GroupBy:
        raise TypeError("group_by must be an exact GroupBy or None")
    _, _, scalar_fields, custom_name, custom_dimensions = _view_source_shape(source)
    compatible = (
        group_by.dimension in ("scene", "actor", "cue", "act", "event_name")
        or (group_by.dimension == "custom_name" and custom_name)
        or (group_by.dimension == "attribute" and scalar_fields)
        or (group_by.dimension == "custom_dimension" and custom_dimensions)
    )
    if not compatible:
        raise ValueError("group dimension is incompatible with query source")


def _validate_metric_query(source, filters, group_by, reducer, field):
    _require_exact_view_value(source, _METRIC_SOURCE_CLASSES, f"{field} source")
    _require_enum(reducer, _VIEW_REDUCERS, f"{field} reducer")
    if type(source) is InstantCount and reducer != "count":
        raise ValueError(f"reducer is incompatible with {field} source")
    _validate_view_filters(filters, source)
    _validate_view_group(group_by, source)


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class TimelineQuery:
    source: TimelineSource
    filters: tuple[ViewFilter, ...] = ()
    group_by: GroupBy | None = None

    def __post_init__(self):
        _require_exact_view_value(self.source, (SpanSource, InstantSource), "timeline source")
        _validate_view_filters(self.filters, self.source)
        _validate_view_group(self.group_by, self.source)


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class MetricQuery:
    source: MetricSource
    reducer: str
    filters: tuple[ViewFilter, ...] = ()
    group_by: GroupBy | None = None

    def __post_init__(self):
        _validate_metric_query(
            self.source, self.filters, self.group_by, self.reducer, "metric"
        )


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class TableQuery:
    source: TableSource
    columns: tuple[TableColumn, ...]
    page_size: int
    filters: tuple[ViewFilter, ...] = ()

    def __post_init__(self):
        _require_exact_view_value(self.source, _TABLE_SOURCE_CLASSES, "table source")
        _validate_view_filters(self.filters, self.source)
        _require_view_tuple(
            self.columns,
            (TableColumn,),
            "table columns",
            _MAX_TABLE_COLUMNS,
            nonempty=True,
        )
        if type(self.page_size) is not int:
            raise TypeError("table page_size must be an exact int")
        if not 1 <= self.page_size <= _MAX_TABLE_PAGE_SIZE:
            raise ValueError("table page_size is out of bounds")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class TimeSeriesQuery:
    source: MetricSource
    reducer: str
    filters: tuple[ViewFilter, ...] = ()
    group_by: GroupBy | None = None

    def __post_init__(self):
        _validate_metric_query(
            self.source, self.filters, self.group_by, self.reducer, "time-series"
        )


def _validate_view(id, title, query, query_class, time_range, scope):
    _require_view_id(id)
    _require_view_plain_text(title, "view title", _MAX_VIEW_TITLE_BYTES)
    if type(query) is not query_class:
        raise TypeError(f"view query must be an exact {query_class.__name__}")
    _require_enum(time_range, _VIEW_TIME_RANGES, "view time_range")
    _require_enum(scope, _VIEW_SCOPES, "view scope")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class TimelineView:
    renderer: _ClassVar[_Literal["timeline"]] = "timeline"
    id: str
    title: str
    query: TimelineQuery
    time_range: str
    scope: str

    def __post_init__(self):
        _validate_view(self.id, self.title, self.query, TimelineQuery, self.time_range, self.scope)


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class MetricView:
    renderer: _ClassVar[_Literal["metric"]] = "metric"
    id: str
    title: str
    query: MetricQuery
    time_range: str
    scope: str

    def __post_init__(self):
        _validate_view(self.id, self.title, self.query, MetricQuery, self.time_range, self.scope)


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class TableView:
    renderer: _ClassVar[_Literal["table"]] = "table"
    id: str
    title: str
    query: TableQuery
    time_range: str
    scope: str

    def __post_init__(self):
        _validate_view(self.id, self.title, self.query, TableQuery, self.time_range, self.scope)


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class TimeSeriesView:
    renderer: _ClassVar[_Literal["time_series"]] = "time_series"
    id: str
    title: str
    query: TimeSeriesQuery
    time_range: str
    scope: str

    def __post_init__(self):
        _validate_view(
            self.id,
            self.title,
            self.query,
            TimeSeriesQuery,
            self.time_range,
            self.scope,
        )


ViewSpec: _TypeAlias = TimelineView | MetricView | TableView | TimeSeriesView
_VIEW_CLASSES = (TimelineView, MetricView, TableView, TimeSeriesView)


def _view_integer_text(value):
    if type(value) is not int:
        raise TypeError("view integer must be an exact int")
    return format(_Decimal(value), "f")


def _encode_view_scalar(value):
    if value is None:
        return {"type": "null"}
    if type(value) is bool:
        return {"type": "boolean", "value": value}
    if type(value) is int:
        return {"type": "integer", "value": _view_integer_text(value)}
    if type(value) is _Decimal:
        return {"type": "decimal", "value": _decimal_text(value)}
    if type(value) is str:
        return {"type": "string", "value": value}
    raise TypeError("normalized view scalar has an unsupported type")


def _encode_view_selector(value):
    if value.kind is not None:
        return {"selector": "built_in", "kind": value.kind}
    return {"selector": "custom", "name": value.name}


def _encode_view_filter(value):
    if type(value) in (SeverityFilter, OutcomeFilter):
        return {"filter": value.filter, "value": value.value}
    if type(value) is AttributeEqualsFilter:
        return {
            "filter": value.filter,
            "key": value.key,
            "value": _encode_view_scalar(value.value),
        }
    if type(value) is AttributeExistsFilter:
        return {"filter": value.filter, "key": value.key}
    raise TypeError("value is not an exact ViewFilter")


def _encode_view_group(value):
    if value is None:
        return None
    result = {"dimension": value.dimension}
    if value.key is not None:
        result["key"] = value.key
    return result


def _encode_metric_source(value):
    if type(value) is CounterValue:
        return {
            "source": value.source,
            "selector": _encode_view_selector(value.selector),
            "selection": "latest_before_reduce",
        }
    if type(value) in (InstantCount, CompletedSpanDuration):
        return {"source": value.source, "selector": _encode_view_selector(value.selector)}
    if type(value) is ActTokenMetric:
        return {"source": value.source, "metric": value.metric}
    raise TypeError("value is not an exact MetricSource")


def _encode_table_source(value):
    if type(value) is EventRows:
        return {"source": value.source, "kind": value.kind}
    if type(value) in (SpanRows, InstantRows, CounterRows):
        return {"source": value.source, "selector": _encode_view_selector(value.selector)}
    if type(value) is ActTokenUsageRows:
        return {"source": value.source}
    raise TypeError("value is not an exact TableSource")


def _encode_table_column(value):
    result = {"column": value.column}
    if value.key is not None:
        result["key"] = value.key
    if value.metric is not None:
        result["metric"] = value.metric
    return result


def _encode_view_query(value):
    if type(value) is TimelineQuery:
        source_kind = "span" if type(value.source) is SpanSource else "instant"
        return {
            "source": {
                "source": source_kind,
                "selector": _encode_view_selector(value.source),
            },
            "filters": [_encode_view_filter(item) for item in value.filters],
            "group_by": _encode_view_group(value.group_by),
        }
    if type(value) in (MetricQuery, TimeSeriesQuery):
        return {
            "source": _encode_metric_source(value.source),
            "filters": [_encode_view_filter(item) for item in value.filters],
            "group_by": _encode_view_group(value.group_by),
            "reducer": value.reducer,
        }
    if type(value) is TableQuery:
        return {
            "source": _encode_table_source(value.source),
            "filters": [_encode_view_filter(item) for item in value.filters],
            "columns": [_encode_table_column(item) for item in value.columns],
            "page_size": value.page_size,
        }
    raise TypeError("value is not an exact view query")


def _view_to_mapping(value):
    _require_exact_view_value(value, _VIEW_CLASSES, "ViewSpec")
    return {
        "renderer": value.renderer,
        "view_schema_version": _VIEW_SCHEMA_VERSION,
        "id": value.id,
        "title": value.title,
        "time_range": value.time_range,
        "scope": value.scope,
        "query": _encode_view_query(value.query),
    }


def _view_to_json_bytes(value):
    return _json.dumps(
        _view_to_mapping(value),
        ensure_ascii=False,
        allow_nan=False,
        separators=(",", ":"),
    ).encode("utf-8")
"#;

pub(crate) const fn source() -> &'static CStr {
    SOURCE
}
