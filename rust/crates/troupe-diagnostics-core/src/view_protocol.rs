use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::Value;

use crate::{
    detail::{CanonicalInteger, DiagnosticScalar},
    event::{DiagnosticEventKind, DiagnosticScope, EVENT_SCHEMA_VERSION},
    id::CanonicalUuid,
    kinds::{CounterKind, CustomSeverity, InstantKind, SpanKind, SpanOutcome},
    scalar::{DecimalString, SchemaU64, TokenCount},
    wire::{WireValueError, deserialize_string},
};

pub const VIEW_SCHEMA_VERSION: u8 = 1;
pub const API_SCHEMA_VERSION: u8 = 1;
pub const MAX_PAGE_ROWS: u16 = 500;
pub const MAX_TIME_SERIES_POINTS: u16 = 1024;
pub const MAX_TIME_SERIES_SERIES: u16 = 64;
pub const MAX_VIEW_ID_BYTES: usize = 64;
pub const MAX_VIEW_TITLE_BYTES: usize = 128;
pub const MAX_OPAQUE_CURSOR_BYTES: usize = 512;

const TIME_SERIES_TARGET_INTERVALS: u64 = 1023;
const MAX_QUERY_FILTERS: usize = 32;
const MAX_TABLE_COLUMNS: usize = 32;
const MAX_CUSTOM_NAME_BYTES: usize = 128;
const MAX_CUSTOM_KEY_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ViewProtocolError(&'static str);

impl ViewProtocolError {
    const fn new(message: &'static str) -> Self {
        Self(message)
    }
}

impl fmt::Display for ViewProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for ViewProtocolError {}

macro_rules! closed_string_enum {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
        pub enum $name {
            $(#[serde(rename = $wire)] $variant),+
        }

        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire),+
                }
            }
        }
    };
}

closed_string_enum!(Renderer {
    Timeline => "timeline",
    Metric => "metric",
    Table => "table",
    TimeSeries => "time_series",
});

closed_string_enum!(TimeRangeMode {
    Viewport => "viewport",
    Run => "run",
});

closed_string_enum!(ScopeMode {
    Selection => "selection",
    Run => "run",
});

closed_string_enum!(Reducer {
    Count => "count",
    Sum => "sum",
    Min => "min",
    Max => "max",
    Mean => "mean",
    Latest => "latest",
});

closed_string_enum!(TokenMetric {
    ProviderTotalTokens => "provider_total_tokens",
    InputTokens => "input_tokens",
    OutputTokens => "output_tokens",
    ThoughtTokens => "thought_tokens",
    CachedReadTokens => "cached_read_tokens",
    CachedWriteTokens => "cached_write_tokens",
});

closed_string_enum!(CounterSelection {
    LatestBeforeReduce => "latest_before_reduce",
});

closed_string_enum!(CoverageStatus {
    Complete => "complete",
    Partial => "partial",
    Unavailable => "unavailable",
});

closed_string_enum!(BucketOrigin {
    Run => "run",
});

closed_string_enum!(IntervalSemantics {
    LeftClosedRightOpen => "left_closed_right_open",
});

closed_string_enum!(IncompatibilityReason {
    NewerViewSchema => "newer_view_schema",
    CorruptRecord => "corrupt_record",
});

closed_string_enum!(TimelineItemType {
    Span => "span",
    Instant => "instant",
});

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OpaqueCursor(String);

impl OpaqueCursor {
    pub fn parse(value: &str) -> Result<Self, ViewProtocolError> {
        if value.is_empty() || value.len() > MAX_OPAQUE_CURSOR_BYTES || !value.is_ascii() {
            return Err(ViewProtocolError::new("opaque cursor is out of bounds"));
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for OpaqueCursor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for OpaqueCursor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_string(deserializer, |value| {
            Self::parse(value).map_err(|_| WireValueError::new("opaque cursor is out of bounds"))
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "selector", rename_all = "snake_case", deny_unknown_fields)]
pub enum SpanSelector {
    BuiltIn { kind: SpanKind },
    Custom { name: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "selector", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstantSelector {
    BuiltIn { kind: InstantKind },
    Custom { name: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "selector", rename_all = "snake_case", deny_unknown_fields)]
pub enum CounterSelector {
    BuiltIn { kind: CounterKind },
    Custom { name: String },
}

impl SpanSelector {
    fn validate(&self) -> Result<(), ViewProtocolError> {
        match self {
            Self::BuiltIn { .. } => Ok(()),
            Self::Custom { name } => validate_custom_name(name),
        }
    }
}

impl InstantSelector {
    fn validate(&self) -> Result<(), ViewProtocolError> {
        match self {
            Self::BuiltIn { .. } => Ok(()),
            Self::Custom { name } => validate_custom_name(name),
        }
    }
}

impl CounterSelector {
    fn validate(&self) -> Result<(), ViewProtocolError> {
        match self {
            Self::BuiltIn { .. } => Ok(()),
            Self::Custom { name } => validate_custom_name(name),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "filter", rename_all = "snake_case", deny_unknown_fields)]
pub enum QueryFilter {
    Severity {
        value: CustomSeverity,
    },
    Outcome {
        value: SpanOutcome,
    },
    AttributeEquals {
        key: String,
        value: DiagnosticScalar,
    },
    AttributeExists {
        key: String,
    },
}

impl QueryFilter {
    fn validate(&self) -> Result<(), ViewProtocolError> {
        match self {
            Self::AttributeEquals { key, .. } | Self::AttributeExists { key } => {
                validate_custom_key(key)
            }
            Self::Severity { .. } | Self::Outcome { .. } => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "dimension", rename_all = "snake_case", deny_unknown_fields)]
pub enum GroupDimension {
    Scene,
    Actor,
    Cue,
    Act,
    EventName,
    CustomName,
    Attribute { key: String },
    CustomDimension { key: String },
}

impl GroupDimension {
    fn validate(&self) -> Result<(), ViewProtocolError> {
        match self {
            Self::Attribute { key } | Self::CustomDimension { key } => validate_custom_key(key),
            Self::Scene
            | Self::Actor
            | Self::Cue
            | Self::Act
            | Self::EventName
            | Self::CustomName => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum TimelineSource {
    Span { selector: SpanSelector },
    Instant { selector: InstantSelector },
}

impl TimelineSource {
    fn validate(&self) -> Result<(), ViewProtocolError> {
        match self {
            Self::Span { selector } => selector.validate(),
            Self::Instant { selector } => selector.validate(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum MetricSource {
    CounterValue {
        selector: CounterSelector,
        selection: CounterSelection,
    },
    InstantCount {
        selector: InstantSelector,
    },
    CompletedSpanDuration {
        selector: SpanSelector,
    },
    ActToken {
        metric: TokenMetric,
    },
}

impl MetricSource {
    fn validate(&self) -> Result<(), ViewProtocolError> {
        match self {
            Self::CounterValue { selector, .. } => selector.validate(),
            Self::InstantCount { selector } => selector.validate(),
            Self::CompletedSpanDuration { selector } => selector.validate(),
            Self::ActToken { .. } => Ok(()),
        }
    }

    fn supports(&self, reducer: Reducer) -> bool {
        match self {
            Self::InstantCount { .. } => reducer == Reducer::Count,
            Self::CounterValue { .. }
            | Self::CompletedSpanDuration { .. }
            | Self::ActToken { .. } => true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum TableSource {
    Event { kind: DiagnosticEventKind },
    Span { selector: SpanSelector },
    Instant { selector: InstantSelector },
    Counter { selector: CounterSelector },
    ActTokenUsage,
}

impl TableSource {
    fn validate(&self) -> Result<(), ViewProtocolError> {
        match self {
            Self::Span { selector } => selector.validate(),
            Self::Instant { selector } => selector.validate(),
            Self::Counter { selector } => selector.validate(),
            Self::Event { .. } | Self::ActTokenUsage => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "column", rename_all = "snake_case", deny_unknown_fields)]
pub enum TableColumn {
    Sequence,
    ElapsedNs,
    EventKind,
    SpanKind,
    InstantKind,
    CounterKind,
    SceneId,
    ActorId,
    CueId,
    ActId,
    CustomName,
    Outcome,
    Severity,
    Attribute { key: String },
    Token { metric: TokenMetric },
    Value,
}

impl TableColumn {
    fn validate(&self) -> Result<(), ViewProtocolError> {
        match self {
            Self::Attribute { key } => validate_custom_key(key),
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimelineQuery {
    source: TimelineSource,
    filters: Vec<QueryFilter>,
    group_by: Option<GroupDimension>,
}

impl TimelineQuery {
    pub fn new(
        source: TimelineSource,
        filters: Vec<QueryFilter>,
        group_by: Option<GroupDimension>,
    ) -> Result<Self, ViewProtocolError> {
        let value = Self {
            source,
            filters,
            group_by,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ViewProtocolError> {
        self.source.validate()?;
        validate_filters(&self.filters)?;
        let shape = SourceShape::for_timeline(&self.source);
        validate_filter_compatibility(&self.filters, shape)?;
        if let Some(group_by) = &self.group_by {
            group_by.validate()?;
            validate_group_compatibility(group_by, shape)?;
        }
        Ok(())
    }

    pub const fn source(&self) -> &TimelineSource {
        &self.source
    }
    pub fn filters(&self) -> &[QueryFilter] {
        &self.filters
    }
    pub const fn group_by(&self) -> Option<&GroupDimension> {
        self.group_by.as_ref()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricQuery {
    source: MetricSource,
    filters: Vec<QueryFilter>,
    group_by: Option<GroupDimension>,
    reducer: Reducer,
}

impl MetricQuery {
    pub fn new(
        source: MetricSource,
        filters: Vec<QueryFilter>,
        group_by: Option<GroupDimension>,
        reducer: Reducer,
    ) -> Result<Self, ViewProtocolError> {
        let value = Self {
            source,
            filters,
            group_by,
            reducer,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ViewProtocolError> {
        self.source.validate()?;
        if !self.source.supports(self.reducer) {
            return Err(ViewProtocolError::new(
                "reducer is incompatible with metric source",
            ));
        }
        validate_filters(&self.filters)?;
        let shape = SourceShape::for_metric(&self.source);
        validate_filter_compatibility(&self.filters, shape)?;
        if let Some(group_by) = &self.group_by {
            group_by.validate()?;
            validate_group_compatibility(group_by, shape)?;
        }
        Ok(())
    }

    pub const fn source(&self) -> &MetricSource {
        &self.source
    }
    pub fn filters(&self) -> &[QueryFilter] {
        &self.filters
    }
    pub const fn group_by(&self) -> Option<&GroupDimension> {
        self.group_by.as_ref()
    }
    pub const fn reducer(&self) -> Reducer {
        self.reducer
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TableQuery {
    source: TableSource,
    filters: Vec<QueryFilter>,
    columns: Vec<TableColumn>,
    page_size: u16,
}

impl TableQuery {
    pub fn new(
        source: TableSource,
        filters: Vec<QueryFilter>,
        columns: Vec<TableColumn>,
        page_size: u16,
    ) -> Result<Self, ViewProtocolError> {
        let value = Self {
            source,
            filters,
            columns,
            page_size,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ViewProtocolError> {
        self.source.validate()?;
        validate_filters(&self.filters)?;
        validate_filter_compatibility(&self.filters, SourceShape::for_table(&self.source))?;
        if self.columns.is_empty() || self.columns.len() > MAX_TABLE_COLUMNS {
            return Err(ViewProtocolError::new(
                "table column count is out of bounds",
            ));
        }
        for column in &self.columns {
            column.validate()?;
        }
        if self.page_size == 0 || self.page_size > MAX_PAGE_ROWS {
            return Err(ViewProtocolError::new("page size is out of bounds"));
        }
        Ok(())
    }

    pub const fn source(&self) -> &TableSource {
        &self.source
    }
    pub fn filters(&self) -> &[QueryFilter] {
        &self.filters
    }
    pub fn columns(&self) -> &[TableColumn] {
        &self.columns
    }
    pub const fn page_size(&self) -> u16 {
        self.page_size
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimeSeriesQuery {
    source: MetricSource,
    filters: Vec<QueryFilter>,
    group_by: Option<GroupDimension>,
    reducer: Reducer,
}

impl TimeSeriesQuery {
    pub fn new(
        source: MetricSource,
        filters: Vec<QueryFilter>,
        group_by: Option<GroupDimension>,
        reducer: Reducer,
    ) -> Result<Self, ViewProtocolError> {
        let value = Self {
            source,
            filters,
            group_by,
            reducer,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ViewProtocolError> {
        self.source.validate()?;
        if !self.source.supports(self.reducer) {
            return Err(ViewProtocolError::new(
                "reducer is incompatible with time-series source",
            ));
        }
        validate_filters(&self.filters)?;
        let shape = SourceShape::for_metric(&self.source);
        validate_filter_compatibility(&self.filters, shape)?;
        if let Some(group_by) = &self.group_by {
            group_by.validate()?;
            validate_group_compatibility(group_by, shape)?;
        }
        Ok(())
    }

    pub const fn source(&self) -> &MetricSource {
        &self.source
    }
    pub fn filters(&self) -> &[QueryFilter] {
        &self.filters
    }
    pub const fn group_by(&self) -> Option<&GroupDimension> {
        self.group_by.as_ref()
    }
    pub const fn reducer(&self) -> Reducer {
        self.reducer
    }
}

fn validate_filters(filters: &[QueryFilter]) -> Result<(), ViewProtocolError> {
    if filters.len() > MAX_QUERY_FILTERS {
        return Err(ViewProtocolError::new("too many query filters"));
    }
    for filter in filters {
        filter.validate()?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct SourceShape {
    outcome: bool,
    severity: bool,
    scalar_fields: bool,
    custom_name: bool,
    custom_dimensions: bool,
}

impl SourceShape {
    const fn builtin() -> Self {
        Self {
            outcome: false,
            severity: false,
            scalar_fields: false,
            custom_name: false,
            custom_dimensions: false,
        }
    }

    fn for_timeline(source: &TimelineSource) -> Self {
        match source {
            TimelineSource::Span { selector } => Self::for_span(selector),
            TimelineSource::Instant { selector } => Self::for_instant(selector),
        }
    }

    fn for_metric(source: &MetricSource) -> Self {
        match source {
            MetricSource::CounterValue { selector, .. } => match selector {
                CounterSelector::BuiltIn { .. } => Self::builtin(),
                CounterSelector::Custom { .. } => Self {
                    scalar_fields: true,
                    custom_name: true,
                    custom_dimensions: true,
                    ..Self::builtin()
                },
            },
            MetricSource::InstantCount { selector } => Self::for_instant(selector),
            MetricSource::CompletedSpanDuration { selector } => Self::for_span(selector),
            MetricSource::ActToken { .. } => Self::builtin(),
        }
    }

    fn for_table(source: &TableSource) -> Self {
        match source {
            TableSource::Span { selector } => Self::for_span(selector),
            TableSource::Instant { selector } => Self::for_instant(selector),
            TableSource::Counter { selector } => match selector {
                CounterSelector::BuiltIn { .. } => Self::builtin(),
                CounterSelector::Custom { .. } => Self {
                    scalar_fields: true,
                    custom_name: true,
                    custom_dimensions: true,
                    ..Self::builtin()
                },
            },
            TableSource::Event { kind } => match kind {
                DiagnosticEventKind::CustomSpanStarted => Self {
                    scalar_fields: true,
                    custom_name: true,
                    ..Self::builtin()
                },
                DiagnosticEventKind::CustomSpanFinished => Self {
                    outcome: true,
                    custom_name: true,
                    ..Self::builtin()
                },
                DiagnosticEventKind::CustomInstantOccurred => Self {
                    severity: true,
                    scalar_fields: true,
                    custom_name: true,
                    ..Self::builtin()
                },
                DiagnosticEventKind::CustomCounterSampled => Self {
                    scalar_fields: true,
                    custom_name: true,
                    custom_dimensions: true,
                    ..Self::builtin()
                },
                DiagnosticEventKind::SpanFinished => Self {
                    outcome: true,
                    ..Self::builtin()
                },
                _ => Self::builtin(),
            },
            TableSource::ActTokenUsage => Self::builtin(),
        }
    }

    fn for_span(selector: &SpanSelector) -> Self {
        match selector {
            SpanSelector::BuiltIn { .. } => Self {
                outcome: true,
                ..Self::builtin()
            },
            SpanSelector::Custom { .. } => Self {
                outcome: true,
                scalar_fields: true,
                custom_name: true,
                ..Self::builtin()
            },
        }
    }

    fn for_instant(selector: &InstantSelector) -> Self {
        match selector {
            InstantSelector::BuiltIn { .. } => Self::builtin(),
            InstantSelector::Custom { .. } => Self {
                severity: true,
                scalar_fields: true,
                custom_name: true,
                ..Self::builtin()
            },
        }
    }
}

fn validate_filter_compatibility(
    filters: &[QueryFilter],
    shape: SourceShape,
) -> Result<(), ViewProtocolError> {
    for filter in filters {
        let supported = match filter {
            QueryFilter::Outcome { .. } => shape.outcome,
            QueryFilter::Severity { .. } => shape.severity,
            QueryFilter::AttributeEquals { .. } | QueryFilter::AttributeExists { .. } => {
                shape.scalar_fields
            }
        };
        if !supported {
            return Err(ViewProtocolError::new(
                "filter is incompatible with query source",
            ));
        }
    }
    Ok(())
}

fn validate_group_compatibility(
    group: &GroupDimension,
    shape: SourceShape,
) -> Result<(), ViewProtocolError> {
    let supported = match group {
        GroupDimension::Scene
        | GroupDimension::Actor
        | GroupDimension::Cue
        | GroupDimension::Act
        | GroupDimension::EventName => true,
        GroupDimension::CustomName => shape.custom_name,
        GroupDimension::Attribute { .. } => shape.scalar_fields,
        GroupDimension::CustomDimension { .. } => shape.custom_dimensions,
    };
    if !supported {
        return Err(ViewProtocolError::new(
            "group dimension is incompatible with query source",
        ));
    }
    Ok(())
}

macro_rules! view_record_struct {
    ($name:ident, $query:ty) => {
        #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
        #[serde(deny_unknown_fields)]
        pub struct $name {
            view_schema_version: u8,
            id: String,
            title: String,
            time_range: TimeRangeMode,
            scope: ScopeMode,
            query: $query,
        }

        impl $name {
            pub fn new(
                id: String,
                title: String,
                time_range: TimeRangeMode,
                scope: ScopeMode,
                query: $query,
            ) -> Result<Self, ViewProtocolError> {
                let value = Self {
                    view_schema_version: VIEW_SCHEMA_VERSION,
                    id,
                    title,
                    time_range,
                    scope,
                    query,
                };
                value.validate_common()?;
                value.query.validate()?;
                Ok(value)
            }

            fn validate_common(&self) -> Result<(), ViewProtocolError> {
                validate_record_common(self.view_schema_version, &self.id, &self.title)
            }

            pub fn id(&self) -> &str {
                &self.id
            }

            pub fn title(&self) -> &str {
                &self.title
            }

            pub const fn time_range(&self) -> TimeRangeMode {
                self.time_range
            }

            pub const fn scope(&self) -> ScopeMode {
                self.scope
            }

            pub const fn query(&self) -> &$query {
                &self.query
            }
        }
    };
}

view_record_struct!(TimelineViewRecord, TimelineQuery);
view_record_struct!(MetricViewRecord, MetricQuery);
view_record_struct!(TableViewRecord, TableQuery);
view_record_struct!(TimeSeriesViewRecord, TimeSeriesQuery);

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "renderer", rename_all = "snake_case")]
pub enum ViewRecord {
    Timeline(TimelineViewRecord),
    Metric(MetricViewRecord),
    Table(TableViewRecord),
    TimeSeries(TimeSeriesViewRecord),
}

#[derive(Deserialize)]
#[serde(tag = "renderer", rename_all = "snake_case")]
enum ViewRecordWire {
    Timeline(TimelineViewRecord),
    Metric(MetricViewRecord),
    Table(TableViewRecord),
    TimeSeries(TimeSeriesViewRecord),
}

impl ViewRecord {
    pub const fn renderer(&self) -> Renderer {
        match self {
            Self::Timeline(_) => Renderer::Timeline,
            Self::Metric(_) => Renderer::Metric,
            Self::Table(_) => Renderer::Table,
            Self::TimeSeries(_) => Renderer::TimeSeries,
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Timeline(value) => value.id(),
            Self::Metric(value) => value.id(),
            Self::Table(value) => value.id(),
            Self::TimeSeries(value) => value.id(),
        }
    }

    pub const fn time_range(&self) -> TimeRangeMode {
        match self {
            Self::Timeline(value) => value.time_range(),
            Self::Metric(value) => value.time_range(),
            Self::Table(value) => value.time_range(),
            Self::TimeSeries(value) => value.time_range(),
        }
    }

    pub const fn scope(&self) -> ScopeMode {
        match self {
            Self::Timeline(value) => value.scope(),
            Self::Metric(value) => value.scope(),
            Self::Table(value) => value.scope(),
            Self::TimeSeries(value) => value.scope(),
        }
    }

    pub fn validate(&self) -> Result<(), ViewProtocolError> {
        match self {
            Self::Timeline(value) => {
                value.validate_common()?;
                value.query.validate()
            }
            Self::Metric(value) => {
                value.validate_common()?;
                value.query.validate()
            }
            Self::Table(value) => {
                value.validate_common()?;
                value.query.validate()
            }
            Self::TimeSeries(value) => {
                value.validate_common()?;
                value.query.validate()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ViewRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = match ViewRecordWire::deserialize(deserializer)? {
            ViewRecordWire::Timeline(value) => Self::Timeline(value),
            ViewRecordWire::Metric(value) => Self::Metric(value),
            ViewRecordWire::Table(value) => Self::Table(value),
            ViewRecordWire::TimeSeries(value) => Self::TimeSeries(value),
        };
        value.validate().map_err(de::Error::custom)?;
        Ok(value)
    }
}

fn validate_record_common(
    view_schema_version: u8,
    id: &str,
    title: &str,
) -> Result<(), ViewProtocolError> {
    if view_schema_version != VIEW_SCHEMA_VERSION {
        return Err(ViewProtocolError::new("unsupported view schema version"));
    }
    validate_view_id(id)?;
    validate_plain_title(title)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperationalCapabilities {
    event_schema_version: u8,
    view_schema_version: u8,
    api_schema_version: u8,
    max_page_rows: u16,
    max_time_series_points: u16,
    max_time_series_series: u16,
    bucket_origin: BucketOrigin,
    interval_semantics: IntervalSemantics,
    counter_selection: CounterSelection,
    exact_mean_components: bool,
}

impl Default for OperationalCapabilities {
    fn default() -> Self {
        Self {
            event_schema_version: EVENT_SCHEMA_VERSION,
            view_schema_version: VIEW_SCHEMA_VERSION,
            api_schema_version: API_SCHEMA_VERSION,
            max_page_rows: MAX_PAGE_ROWS,
            max_time_series_points: MAX_TIME_SERIES_POINTS,
            max_time_series_series: MAX_TIME_SERIES_SERIES,
            bucket_origin: BucketOrigin::Run,
            interval_semantics: IntervalSemantics::LeftClosedRightOpen,
            counter_selection: CounterSelection::LatestBeforeReduce,
            exact_mean_components: true,
        }
    }
}

impl OperationalCapabilities {
    pub fn validate(&self) -> Result<(), ViewProtocolError> {
        if self.event_schema_version != EVENT_SCHEMA_VERSION
            || self.view_schema_version != VIEW_SCHEMA_VERSION
            || self.api_schema_version != API_SCHEMA_VERSION
            || self.max_page_rows != MAX_PAGE_ROWS
            || self.max_time_series_points != MAX_TIME_SERIES_POINTS
            || self.max_time_series_series != MAX_TIME_SERIES_SERIES
            || !self.exact_mean_components
        {
            return Err(ViewProtocolError::new(
                "operational capabilities are incompatible",
            ));
        }
        Ok(())
    }

    pub const fn event_schema_version(&self) -> u8 {
        self.event_schema_version
    }
    pub const fn view_schema_version(&self) -> u8 {
        self.view_schema_version
    }
    pub const fn api_schema_version(&self) -> u8 {
        self.api_schema_version
    }
    pub const fn max_page_rows(&self) -> u16 {
        self.max_page_rows
    }
    pub const fn max_time_series_points(&self) -> u16 {
        self.max_time_series_points
    }
    pub const fn max_time_series_series(&self) -> u16 {
        self.max_time_series_series
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryBinding {
    captured_watermark: SchemaU64,
    captured_elapsed_end_ns: SchemaU64,
    time_range: TimeRangeMode,
    range_start_ns: SchemaU64,
    range_end_ns: SchemaU64,
    scope: ScopeMode,
    selected_scope: Option<DiagnosticScope>,
}

impl QueryBinding {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        captured_watermark: SchemaU64,
        captured_elapsed_end_ns: SchemaU64,
        time_range: TimeRangeMode,
        range_start_ns: SchemaU64,
        range_end_ns: SchemaU64,
        scope: ScopeMode,
        selected_scope: Option<DiagnosticScope>,
    ) -> Result<Self, ViewProtocolError> {
        let value = Self {
            captured_watermark,
            captured_elapsed_end_ns,
            time_range,
            range_start_ns,
            range_end_ns,
            scope,
            selected_scope,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), ViewProtocolError> {
        let start = self.range_start_ns.get();
        let end = self.range_end_ns.get();
        if start > end || end > self.captured_elapsed_end_ns.get() {
            return Err(ViewProtocolError::new(
                "time binding is outside captured data",
            ));
        }
        if self.time_range == TimeRangeMode::Run
            && (start != 0 || end != self.captured_elapsed_end_ns.get())
        {
            return Err(ViewProtocolError::new(
                "run time binding must cover captured run",
            ));
        }
        if self.scope == ScopeMode::Run && self.selected_scope.is_some() {
            return Err(ViewProtocolError::new(
                "run scope binding cannot select a scope",
            ));
        }
        Ok(())
    }

    pub const fn captured_watermark(&self) -> SchemaU64 {
        self.captured_watermark
    }
    pub const fn captured_elapsed_end_ns(&self) -> SchemaU64 {
        self.captured_elapsed_end_ns
    }
    pub const fn time_range(&self) -> TimeRangeMode {
        self.time_range
    }
    pub const fn range_start_ns(&self) -> SchemaU64 {
        self.range_start_ns
    }
    pub const fn range_end_ns(&self) -> SchemaU64 {
        self.range_end_ns
    }
    pub const fn scope(&self) -> ScopeMode {
        self.scope
    }
    pub const fn selected_scope(&self) -> Option<&DiagnosticScope> {
        self.selected_scope.as_ref()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExcludedCounts {
    open_spans: SchemaU64,
    missing_values: SchemaU64,
    non_numeric_values: SchemaU64,
    unavailable_values: SchemaU64,
    resource_truncated: SchemaU64,
}

impl ExcludedCounts {
    pub const fn new(
        open_spans: SchemaU64,
        missing_values: SchemaU64,
        non_numeric_values: SchemaU64,
        unavailable_values: SchemaU64,
        resource_truncated: SchemaU64,
    ) -> Self {
        Self {
            open_spans,
            missing_values,
            non_numeric_values,
            unavailable_values,
            resource_truncated,
        }
    }

    fn checked_total(&self) -> Option<u64> {
        self.open_spans
            .get()
            .checked_add(self.missing_values.get())?
            .checked_add(self.non_numeric_values.get())?
            .checked_add(self.unavailable_values.get())?
            .checked_add(self.resource_truncated.get())
    }

    pub const fn open_spans(&self) -> SchemaU64 {
        self.open_spans
    }
    pub const fn missing_values(&self) -> SchemaU64 {
        self.missing_values
    }
    pub const fn non_numeric_values(&self) -> SchemaU64 {
        self.non_numeric_values
    }
    pub const fn unavailable_values(&self) -> SchemaU64 {
        self.unavailable_values
    }
    pub const fn resource_truncated(&self) -> SchemaU64 {
        self.resource_truncated
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    status: CoverageStatus,
    matched_count: SchemaU64,
    contributing_count: SchemaU64,
    excluded_count: SchemaU64,
    excluded: ExcludedCounts,
    gap_count: SchemaU64,
}

impl Coverage {
    pub fn new(
        status: CoverageStatus,
        matched_count: SchemaU64,
        contributing_count: SchemaU64,
        excluded_count: SchemaU64,
        excluded: ExcludedCounts,
        gap_count: SchemaU64,
    ) -> Result<Self, ViewProtocolError> {
        let value = Self {
            status,
            matched_count,
            contributing_count,
            excluded_count,
            excluded,
            gap_count,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), ViewProtocolError> {
        if self.contributing_count.get() > self.matched_count.get()
            || self.excluded_count.get() > self.matched_count.get()
            || self
                .contributing_count
                .get()
                .checked_add(self.excluded_count.get())
                .is_none_or(|total| total > self.matched_count.get())
            || self.excluded.checked_total() != Some(self.excluded_count.get())
        {
            return Err(ViewProtocolError::new("coverage counts are inconsistent"));
        }
        let complete = self.excluded_count.get() == 0 && self.gap_count.get() == 0;
        match self.status {
            CoverageStatus::Complete if !complete => Err(ViewProtocolError::new(
                "complete coverage contains exclusions or gaps",
            )),
            CoverageStatus::Partial if complete => Err(ViewProtocolError::new(
                "partial coverage has no exclusions or gaps",
            )),
            CoverageStatus::Unavailable if self.contributing_count.get() != 0 => Err(
                ViewProtocolError::new("unavailable coverage has contributing values"),
            ),
            _ => Ok(()),
        }
    }

    pub const fn status(&self) -> CoverageStatus {
        self.status
    }
    pub const fn matched_count(&self) -> SchemaU64 {
        self.matched_count
    }
    pub const fn contributing_count(&self) -> SchemaU64 {
        self.contributing_count
    }
    pub const fn excluded_count(&self) -> SchemaU64 {
        self.excluded_count
    }
    pub const fn excluded(&self) -> &ExcludedCounts {
        &self.excluded
    }
    pub const fn gap_count(&self) -> SchemaU64 {
        self.gap_count
    }
    pub const fn is_partial(&self) -> bool {
        matches!(self.status, CoverageStatus::Partial)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Pagination {
    page_size: u16,
    next_cursor: Option<OpaqueCursor>,
}

impl Pagination {
    pub fn new(
        page_size: u16,
        next_cursor: Option<OpaqueCursor>,
    ) -> Result<Self, ViewProtocolError> {
        let value = Self {
            page_size,
            next_cursor,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ViewProtocolError> {
        if self.page_size == 0 || self.page_size > MAX_PAGE_ROWS {
            return Err(ViewProtocolError::new(
                "pagination page size is out of bounds",
            ));
        }
        Ok(())
    }

    pub const fn page_size(&self) -> u16 {
        self.page_size
    }
    pub const fn next_cursor(&self) -> Option<&OpaqueCursor> {
        self.next_cursor.as_ref()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IncompatibleView {
    reason: IncompatibilityReason,
    supported_view_schema_version: u8,
    record_view_schema_version: Option<u8>,
}

impl IncompatibleView {
    pub fn newer(record_view_schema_version: u8) -> Result<Self, ViewProtocolError> {
        if record_view_schema_version <= VIEW_SCHEMA_VERSION {
            return Err(ViewProtocolError::new("newer record version is not newer"));
        }
        Ok(Self {
            reason: IncompatibilityReason::NewerViewSchema,
            supported_view_schema_version: VIEW_SCHEMA_VERSION,
            record_view_schema_version: Some(record_view_schema_version),
        })
    }

    pub const fn corrupt(record_view_schema_version: Option<u8>) -> Self {
        Self {
            reason: IncompatibilityReason::CorruptRecord,
            supported_view_schema_version: VIEW_SCHEMA_VERSION,
            record_view_schema_version,
        }
    }

    pub const fn reason(&self) -> IncompatibilityReason {
        self.reason
    }
    pub const fn record_view_schema_version(&self) -> Option<u8> {
        self.record_view_schema_version
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResultMetadata {
    api_schema_version: u8,
    view_schema_version: u8,
    run_id: CanonicalUuid,
    view_id: String,
    binding: QueryBinding,
    coverage: Coverage,
    pagination: Option<Pagination>,
    truncated: bool,
    incompatible: Option<IncompatibleView>,
    capabilities: OperationalCapabilities,
}

impl ResultMetadata {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        run_id: CanonicalUuid,
        view_id: String,
        binding: QueryBinding,
        coverage: Coverage,
        pagination: Option<Pagination>,
        truncated: bool,
        incompatible: Option<IncompatibleView>,
    ) -> Result<Self, ViewProtocolError> {
        let value = Self {
            api_schema_version: API_SCHEMA_VERSION,
            view_schema_version: VIEW_SCHEMA_VERSION,
            run_id,
            view_id,
            binding,
            coverage,
            pagination,
            truncated,
            incompatible,
            capabilities: OperationalCapabilities::default(),
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ViewProtocolError> {
        if self.api_schema_version != API_SCHEMA_VERSION
            || self.view_schema_version != VIEW_SCHEMA_VERSION
        {
            return Err(ViewProtocolError::new(
                "result schema version is incompatible",
            ));
        }
        validate_view_id(&self.view_id)?;
        self.binding.validate()?;
        self.coverage.validate()?;
        if let Some(pagination) = &self.pagination {
            pagination.validate()?;
        }
        self.capabilities.validate()?;
        if let Some(incompatible) = &self.incompatible
            && incompatible.supported_view_schema_version != VIEW_SCHEMA_VERSION
        {
            return Err(ViewProtocolError::new(
                "incompatible state has wrong supported version",
            ));
        }
        if self.truncated != (self.coverage.excluded.resource_truncated.get() > 0) {
            return Err(ViewProtocolError::new(
                "truncation state and coverage disagree",
            ));
        }
        Ok(())
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }
    pub fn view_id(&self) -> &str {
        &self.view_id
    }
    pub const fn binding(&self) -> &QueryBinding {
        &self.binding
    }
    pub const fn coverage(&self) -> &Coverage {
        &self.coverage
    }
    pub const fn pagination(&self) -> Option<&Pagination> {
        self.pagination.as_ref()
    }
    pub const fn truncated(&self) -> bool {
        self.truncated
    }
    pub const fn incompatible(&self) -> Option<&IncompatibleView> {
        self.incompatible.as_ref()
    }
    pub const fn capabilities(&self) -> &OperationalCapabilities {
        &self.capabilities
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ExactNumber {
    Integer(CanonicalInteger),
    Decimal(DecimalString),
}

impl ExactNumber {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Integer(value) => value.as_str(),
            Self::Decimal(value) => value.as_str(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "aggregate", rename_all = "snake_case", deny_unknown_fields)]
pub enum AggregateValue {
    Exact {
        value: ExactNumber,
    },
    Mean {
        numerator: ExactNumber,
        contributing_count: TokenCount,
    },
}

pub struct MeanComponents<'a> {
    numerator: &'a ExactNumber,
    contributing_count: &'a TokenCount,
}

impl MeanComponents<'_> {
    pub const fn numerator(&self) -> &ExactNumber {
        self.numerator
    }
    pub const fn contributing_count(&self) -> &TokenCount {
        self.contributing_count
    }
}

impl AggregateValue {
    pub fn as_mean(&self) -> Option<MeanComponents<'_>> {
        match self {
            Self::Mean {
                numerator,
                contributing_count,
            } => Some(MeanComponents {
                numerator,
                contributing_count,
            }),
            Self::Exact { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupKey {
    dimension: GroupDimension,
    value: DiagnosticScalar,
}

impl GroupKey {
    pub fn new(
        dimension: GroupDimension,
        value: DiagnosticScalar,
    ) -> Result<Self, ViewProtocolError> {
        dimension.validate()?;
        Ok(Self { dimension, value })
    }

    pub const fn dimension(&self) -> &GroupDimension {
        &self.dimension
    }
    pub const fn value(&self) -> &DiagnosticScalar {
        &self.value
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimelineRow {
    sequence: SchemaU64,
    item_type: TimelineItemType,
    name: String,
    start_ns: SchemaU64,
    end_ns: Option<SchemaU64>,
    scope: DiagnosticScope,
    outcome: Option<SpanOutcome>,
}

impl TimelineRow {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sequence: SchemaU64,
        item_type: TimelineItemType,
        name: String,
        start_ns: SchemaU64,
        end_ns: Option<SchemaU64>,
        scope: DiagnosticScope,
        outcome: Option<SpanOutcome>,
    ) -> Result<Self, ViewProtocolError> {
        let value = Self {
            sequence,
            item_type,
            name,
            start_ns,
            end_ns,
            scope,
            outcome,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ViewProtocolError> {
        if self.name.is_empty() || self.name.len() > MAX_CUSTOM_NAME_BYTES || !self.name.is_ascii()
        {
            return Err(ViewProtocolError::new(
                "timeline item name is out of bounds",
            ));
        }
        match self.item_type {
            TimelineItemType::Span => {
                if let Some(end) = self.end_ns
                    && end.get() < self.start_ns.get()
                {
                    return Err(ViewProtocolError::new("span row ends before it starts"));
                }
            }
            TimelineItemType::Instant => {
                if self.end_ns.is_some() || self.outcome.is_some() {
                    return Err(ViewProtocolError::new("instant row contains span fields"));
                }
            }
        }
        Ok(())
    }

    pub const fn sequence(&self) -> SchemaU64 {
        self.sequence
    }
    pub const fn item_type(&self) -> TimelineItemType {
        self.item_type
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub const fn start_ns(&self) -> SchemaU64 {
        self.start_ns
    }
    pub const fn end_ns(&self) -> Option<SchemaU64> {
        self.end_ns
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricSeries {
    group: Option<GroupKey>,
    value: Option<AggregateValue>,
    coverage: Coverage,
}

impl MetricSeries {
    pub fn new(
        group: Option<GroupKey>,
        value: Option<AggregateValue>,
        coverage: Coverage,
    ) -> Result<Self, ViewProtocolError> {
        let result = Self {
            group,
            value,
            coverage,
        };
        result.validate()?;
        Ok(result)
    }

    fn validate(&self) -> Result<(), ViewProtocolError> {
        self.coverage.validate()?;
        if self.value.is_none() && self.coverage.contributing_count().get() != 0 {
            return Err(ViewProtocolError::new(
                "empty metric has contributing values",
            ));
        }
        validate_mean_count(self.value.as_ref(), &self.coverage)?;
        Ok(())
    }

    pub const fn value(&self) -> Option<&AggregateValue> {
        self.value.as_ref()
    }
    pub const fn group(&self) -> Option<&GroupKey> {
        self.group.as_ref()
    }
    pub const fn coverage(&self) -> &Coverage {
        &self.coverage
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TableRow {
    sequence: SchemaU64,
    cells: Vec<Option<DiagnosticScalar>>,
}

impl TableRow {
    pub const fn new(sequence: SchemaU64, cells: Vec<Option<DiagnosticScalar>>) -> Self {
        Self { sequence, cells }
    }

    pub const fn sequence(&self) -> SchemaU64 {
        self.sequence
    }
    pub fn cells(&self) -> &[Option<DiagnosticScalar>] {
        &self.cells
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimeSeriesPoint {
    bucket_start_ns: SchemaU64,
    bucket_end_ns: SchemaU64,
    partial: bool,
    value: Option<AggregateValue>,
    coverage: Coverage,
}

impl TimeSeriesPoint {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bucket_start_ns: SchemaU64,
        bucket_end_ns: SchemaU64,
        partial: bool,
        value: Option<AggregateValue>,
        coverage: Coverage,
    ) -> Result<Self, ViewProtocolError> {
        if bucket_end_ns.get() <= bucket_start_ns.get() {
            return Err(ViewProtocolError::new(
                "time-series bucket is empty or reversed",
            ));
        }
        if value.is_none() && coverage.contributing_count().get() != 0 {
            return Err(ViewProtocolError::new(
                "empty bucket has contributing values",
            ));
        }
        coverage.validate()?;
        Ok(Self {
            bucket_start_ns,
            bucket_end_ns,
            partial,
            value,
            coverage,
        })
    }

    pub const fn bucket_start_ns(&self) -> SchemaU64 {
        self.bucket_start_ns
    }
    pub const fn bucket_end_ns(&self) -> SchemaU64 {
        self.bucket_end_ns
    }
    pub const fn is_partial(&self) -> bool {
        self.partial
    }
    pub const fn value(&self) -> Option<&AggregateValue> {
        self.value.as_ref()
    }
    pub const fn coverage(&self) -> &Coverage {
        &self.coverage
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimeSeriesSeries {
    group: Option<GroupKey>,
    points: Vec<TimeSeriesPoint>,
}

impl TimeSeriesSeries {
    pub const fn new(group: Option<GroupKey>, points: Vec<TimeSeriesPoint>) -> Self {
        Self { group, points }
    }

    pub const fn group(&self) -> Option<&GroupKey> {
        self.group.as_ref()
    }
    pub fn points(&self) -> &[TimeSeriesPoint] {
        &self.points
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimelineResult {
    #[serde(flatten)]
    metadata: ResultMetadata,
    rows: Vec<TimelineRow>,
}

impl TimelineResult {
    pub const fn metadata(&self) -> &ResultMetadata {
        &self.metadata
    }
    pub fn rows(&self) -> &[TimelineRow] {
        &self.rows
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricResult {
    #[serde(flatten)]
    metadata: ResultMetadata,
    series: Vec<MetricSeries>,
}

impl MetricResult {
    pub const fn metadata(&self) -> &ResultMetadata {
        &self.metadata
    }
    pub fn series(&self) -> &[MetricSeries] {
        &self.series
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TableResult {
    #[serde(flatten)]
    metadata: ResultMetadata,
    columns: Vec<TableColumn>,
    rows: Vec<TableRow>,
}

impl TableResult {
    pub const fn metadata(&self) -> &ResultMetadata {
        &self.metadata
    }
    pub fn columns(&self) -> &[TableColumn] {
        &self.columns
    }
    pub fn rows(&self) -> &[TableRow] {
        &self.rows
    }
    pub fn pagination(&self) -> &Pagination {
        self.metadata
            .pagination()
            .expect("validated table result has pagination")
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimeSeriesResult {
    #[serde(flatten)]
    metadata: ResultMetadata,
    bucket_width_ns: SchemaU64,
    series: Vec<TimeSeriesSeries>,
}

impl TimeSeriesResult {
    pub const fn metadata(&self) -> &ResultMetadata {
        &self.metadata
    }
    pub const fn bucket_width_ns(&self) -> SchemaU64 {
        self.bucket_width_ns
    }
    pub fn series(&self) -> &[TimeSeriesSeries] {
        &self.series
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "renderer", rename_all = "snake_case")]
pub enum ViewResponse {
    Timeline(TimelineResult),
    Metric(MetricResult),
    Table(TableResult),
    TimeSeries(TimeSeriesResult),
}

#[derive(Deserialize)]
#[serde(tag = "renderer", rename_all = "snake_case")]
enum ViewResponseWire {
    Timeline(TimelineResult),
    Metric(MetricResult),
    Table(TableResult),
    TimeSeries(TimeSeriesResult),
}

impl ViewResponse {
    pub fn new_timeline(
        metadata: ResultMetadata,
        rows: Vec<TimelineRow>,
    ) -> Result<Self, ViewProtocolError> {
        let value = Self::Timeline(TimelineResult { metadata, rows });
        value.validate()?;
        Ok(value)
    }

    pub fn new_metric(
        metadata: ResultMetadata,
        series: Vec<MetricSeries>,
    ) -> Result<Self, ViewProtocolError> {
        let value = Self::Metric(MetricResult { metadata, series });
        value.validate()?;
        Ok(value)
    }

    pub fn new_table(
        metadata: ResultMetadata,
        columns: Vec<TableColumn>,
        rows: Vec<TableRow>,
    ) -> Result<Self, ViewProtocolError> {
        let value = Self::Table(TableResult {
            metadata,
            columns,
            rows,
        });
        value.validate()?;
        Ok(value)
    }

    pub fn new_time_series(
        metadata: ResultMetadata,
        bucket_width_ns: SchemaU64,
        series: Vec<TimeSeriesSeries>,
    ) -> Result<Self, ViewProtocolError> {
        let value = Self::TimeSeries(TimeSeriesResult {
            metadata,
            bucket_width_ns,
            series,
        });
        value.validate()?;
        Ok(value)
    }

    pub const fn renderer(&self) -> Renderer {
        match self {
            Self::Timeline(_) => Renderer::Timeline,
            Self::Metric(_) => Renderer::Metric,
            Self::Table(_) => Renderer::Table,
            Self::TimeSeries(_) => Renderer::TimeSeries,
        }
    }

    pub const fn metadata(&self) -> &ResultMetadata {
        match self {
            Self::Timeline(value) => &value.metadata,
            Self::Metric(value) => &value.metadata,
            Self::Table(value) => &value.metadata,
            Self::TimeSeries(value) => &value.metadata,
        }
    }

    pub const fn timeline(&self) -> Option<&TimelineResult> {
        match self {
            Self::Timeline(value) => Some(value),
            _ => None,
        }
    }

    pub const fn metric(&self) -> Option<&MetricResult> {
        match self {
            Self::Metric(value) => Some(value),
            _ => None,
        }
    }

    pub const fn table(&self) -> Option<&TableResult> {
        match self {
            Self::Table(value) => Some(value),
            _ => None,
        }
    }

    pub const fn time_series(&self) -> Option<&TimeSeriesResult> {
        match self {
            Self::TimeSeries(value) => Some(value),
            _ => None,
        }
    }

    pub fn validate(&self) -> Result<(), ViewProtocolError> {
        self.metadata().validate()?;
        match self {
            Self::Timeline(result) => {
                let pagination = result.metadata.pagination.as_ref().ok_or_else(|| {
                    ViewProtocolError::new("timeline result requires pagination state")
                })?;
                if result.rows.len() > usize::from(pagination.page_size) {
                    return Err(ViewProtocolError::new("timeline result exceeds page size"));
                }
                for row in &result.rows {
                    row.validate()?;
                }
                Ok(())
            }
            Self::Metric(result) => {
                if result.metadata.pagination.is_some() {
                    return Err(ViewProtocolError::new("metric result cannot be paginated"));
                }
                for series in &result.series {
                    series.validate()?;
                }
                Ok(())
            }
            Self::Table(result) => {
                let pagination = result.metadata.pagination.as_ref().ok_or_else(|| {
                    ViewProtocolError::new("table result requires pagination state")
                })?;
                if result.columns.is_empty()
                    || result.columns.len() > MAX_TABLE_COLUMNS
                    || result.rows.len() > usize::from(pagination.page_size)
                {
                    return Err(ViewProtocolError::new(
                        "table result shape is out of bounds",
                    ));
                }
                for column in &result.columns {
                    column.validate()?;
                }
                if result
                    .rows
                    .iter()
                    .any(|row| row.cells.len() != result.columns.len())
                {
                    return Err(ViewProtocolError::new("table row does not match columns"));
                }
                Ok(())
            }
            Self::TimeSeries(result) => validate_time_series(result),
        }
    }

    pub fn validate_for(&self, record: &ViewRecord) -> Result<(), ViewProtocolError> {
        record.validate()?;
        self.validate()?;
        if self.renderer() != record.renderer()
            || self.metadata().view_id() != record.id()
            || self.metadata().binding().time_range() != record.time_range()
            || self.metadata().binding().scope() != record.scope()
        {
            return Err(ViewProtocolError::new(
                "view response does not match its descriptor",
            ));
        }
        match (self, record) {
            (Self::Timeline(_), ViewRecord::Timeline(_)) => Ok(()),
            (Self::Metric(result), ViewRecord::Metric(record)) => {
                validate_aggregate_series(&result.series, record.query())
            }
            (Self::Table(result), ViewRecord::Table(record)) => {
                if result.columns != record.query().columns
                    || result
                        .metadata
                        .pagination
                        .as_ref()
                        .is_none_or(|pagination| pagination.page_size != record.query().page_size)
                {
                    return Err(ViewProtocolError::new(
                        "table result projection differs from descriptor",
                    ));
                }
                Ok(())
            }
            (Self::TimeSeries(result), ViewRecord::TimeSeries(record)) => {
                validate_time_series_aggregates(&result.series, record.query())
            }
            _ => Err(ViewProtocolError::new(
                "view response renderer differs from descriptor",
            )),
        }
    }
}

fn validate_aggregate_series(
    series: &[MetricSeries],
    query: &MetricQuery,
) -> Result<(), ViewProtocolError> {
    for item in series {
        validate_group_key(item.group.as_ref(), query.group_by())?;
        validate_aggregate_kind(item.value.as_ref(), query.reducer())?;
    }
    Ok(())
}

fn validate_time_series_aggregates(
    series: &[TimeSeriesSeries],
    query: &TimeSeriesQuery,
) -> Result<(), ViewProtocolError> {
    for item in series {
        validate_group_key(item.group.as_ref(), query.group_by())?;
        for point in &item.points {
            validate_aggregate_kind(point.value.as_ref(), query.reducer())?;
        }
    }
    Ok(())
}

fn validate_group_key(
    actual: Option<&GroupKey>,
    expected: Option<&GroupDimension>,
) -> Result<(), ViewProtocolError> {
    match (actual, expected) {
        (None, None) => Ok(()),
        (Some(actual), Some(expected)) if actual.dimension() == expected => Ok(()),
        _ => Err(ViewProtocolError::new(
            "result group key differs from descriptor",
        )),
    }
}

fn validate_aggregate_kind(
    value: Option<&AggregateValue>,
    reducer: Reducer,
) -> Result<(), ViewProtocolError> {
    let compatible = match (value, reducer) {
        (None, _) | (Some(AggregateValue::Mean { .. }), Reducer::Mean) => true,
        (Some(AggregateValue::Exact { .. }), reducer) => reducer != Reducer::Mean,
        (Some(AggregateValue::Mean { .. }), _) => false,
    };
    if !compatible {
        return Err(ViewProtocolError::new(
            "aggregate value shape differs from reducer",
        ));
    }
    Ok(())
}

impl<'de> Deserialize<'de> for ViewResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = match ViewResponseWire::deserialize(deserializer)? {
            ViewResponseWire::Timeline(value) => Self::Timeline(value),
            ViewResponseWire::Metric(value) => Self::Metric(value),
            ViewResponseWire::Table(value) => Self::Table(value),
            ViewResponseWire::TimeSeries(value) => Self::TimeSeries(value),
        };
        value.validate().map_err(de::Error::custom)?;
        Ok(value)
    }
}

fn validate_time_series(result: &TimeSeriesResult) -> Result<(), ViewProtocolError> {
    if result.metadata.pagination.is_some() {
        return Err(ViewProtocolError::new(
            "time-series result cannot be paginated",
        ));
    }
    if result.series.is_empty() || result.series.len() > usize::from(MAX_TIME_SERIES_SERIES) {
        return Err(ViewProtocolError::new(
            "time-series series count is out of bounds",
        ));
    }
    let binding = &result.metadata.binding;
    let range_start = binding.range_start_ns.get();
    let range_end = binding.range_end_ns.get();
    let expected_width = expected_bucket_width_ns(range_start, range_end)?;
    if result.bucket_width_ns != expected_width {
        return Err(ViewProtocolError::new(
            "time-series bucket width is not canonical",
        ));
    }
    let expected_buckets = expected_buckets(range_start, range_end, expected_width.get())?;
    if expected_buckets.len() > usize::from(MAX_TIME_SERIES_POINTS) {
        return Err(ViewProtocolError::new("time-series exceeds point cap"));
    }
    for series in &result.series {
        if series.points.len() != expected_buckets.len() {
            return Err(ViewProtocolError::new(
                "time-series does not emit every bucket",
            ));
        }
        for (point, (start, end, partial)) in series.points.iter().zip(&expected_buckets) {
            if point.bucket_start_ns.get() != *start
                || point.bucket_end_ns.get() != *end
                || point.partial != *partial
            {
                return Err(ViewProtocolError::new(
                    "time-series bucket is not origin aligned",
                ));
            }
            point.coverage.validate()?;
            if point.value.is_none() && point.coverage.contributing_count().get() != 0 {
                return Err(ViewProtocolError::new(
                    "empty bucket has contributing values",
                ));
            }
            validate_mean_count(point.value.as_ref(), &point.coverage)?;
        }
    }
    Ok(())
}

fn validate_mean_count(
    value: Option<&AggregateValue>,
    coverage: &Coverage,
) -> Result<(), ViewProtocolError> {
    if let Some(AggregateValue::Mean {
        contributing_count, ..
    }) = value
        && contributing_count.as_str() != coverage.contributing_count().get().to_string()
    {
        return Err(ViewProtocolError::new(
            "mean count and coverage contributing count disagree",
        ));
    }
    Ok(())
}

pub fn expected_bucket_width_ns(
    range_start_ns: u64,
    range_end_ns: u64,
) -> Result<SchemaU64, ViewProtocolError> {
    let duration = range_end_ns
        .checked_sub(range_start_ns)
        .ok_or_else(|| ViewProtocolError::new("time range ends before it starts"))?;
    if duration == 0 {
        return Ok(SchemaU64::new(1));
    }
    let width = duration / TIME_SERIES_TARGET_INTERVALS
        + u64::from(duration % TIME_SERIES_TARGET_INTERVALS != 0);
    Ok(SchemaU64::new(width.max(1)))
}

fn expected_buckets(
    range_start_ns: u64,
    range_end_ns: u64,
    width: u64,
) -> Result<Vec<(u64, u64, bool)>, ViewProtocolError> {
    if range_start_ns == range_end_ns {
        return Ok(Vec::new());
    }
    let mut bucket_start = range_start_ns / width * width;
    let mut buckets = Vec::new();
    while bucket_start < range_end_ns {
        let bucket_end = bucket_start
            .checked_add(width)
            .ok_or_else(|| ViewProtocolError::new("time-series bucket overflows u64"))?;
        let partial = bucket_start < range_start_ns || bucket_end > range_end_ns;
        buckets.push((bucket_start, bucket_end, partial));
        if buckets.len() > usize::from(MAX_TIME_SERIES_POINTS) {
            return Err(ViewProtocolError::new("time-series exceeds point cap"));
        }
        bucket_start = bucket_end;
    }
    Ok(buckets)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArchivedViewRecordStatus {
    Compatible(ViewRecord),
    Incompatible(IncompatibilityReason),
}

pub fn classify_archived_view_record(value: &Value) -> ArchivedViewRecordStatus {
    let Some(object) = value.as_object() else {
        return ArchivedViewRecordStatus::Incompatible(IncompatibilityReason::CorruptRecord);
    };
    let Some(version) = object.get("view_schema_version").and_then(Value::as_u64) else {
        return ArchivedViewRecordStatus::Incompatible(IncompatibilityReason::CorruptRecord);
    };
    if version > u64::from(VIEW_SCHEMA_VERSION) {
        return ArchivedViewRecordStatus::Incompatible(IncompatibilityReason::NewerViewSchema);
    }
    match serde_json::from_value::<ViewRecord>(value.clone()) {
        Ok(record) => ArchivedViewRecordStatus::Compatible(record),
        Err(_) => ArchivedViewRecordStatus::Incompatible(IncompatibilityReason::CorruptRecord),
    }
}

fn validate_view_id(value: &str) -> Result<(), ViewProtocolError> {
    let mut bytes = value.bytes();
    if value.is_empty()
        || value.len() > MAX_VIEW_ID_BYTES
        || !bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        || !bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(ViewProtocolError::new("view id is invalid"));
    }
    Ok(())
}

fn validate_plain_title(value: &str) -> Result<(), ViewProtocolError> {
    if value.is_empty() || value.len() > MAX_VIEW_TITLE_BYTES || value.chars().any(char::is_control)
    {
        return Err(ViewProtocolError::new("view title is out of bounds"));
    }
    let lower = value.to_ascii_lowercase();
    if value.contains(['<', '>', '`'])
        || lower.contains("javascript:")
        || lower.contains("data:text/html")
        || lower.contains("http://")
        || lower.contains("https://")
        || lower.contains("url(")
        || lower.contains("@import")
    {
        return Err(ViewProtocolError::new("view title must be plain text"));
    }
    Ok(())
}

fn validate_custom_name(value: &str) -> Result<(), ViewProtocolError> {
    if value.is_empty() || value.len() > MAX_CUSTOM_NAME_BYTES || !value.is_ascii() {
        return Err(ViewProtocolError::new("custom name is out of bounds"));
    }
    let segments = value.split('.').collect::<Vec<_>>();
    if segments.len() < 2
        || segments[0] == "troupe"
        || segments.iter().any(|part| {
            let mut bytes = part.bytes();
            !bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
                || !bytes
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        })
    {
        return Err(ViewProtocolError::new("custom name is invalid or reserved"));
    }
    Ok(())
}

fn validate_custom_key(value: &str) -> Result<(), ViewProtocolError> {
    if value.is_empty() || value.len() > MAX_CUSTOM_KEY_BYTES || value.chars().any(char::is_control)
    {
        return Err(ViewProtocolError::new("custom key is out of bounds"));
    }
    Ok(())
}
