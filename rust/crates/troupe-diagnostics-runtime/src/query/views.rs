use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use troupe_diagnostics_core::{
    detail::{CanonicalInteger, DiagnosticScalar},
    event::DiagnosticScope,
    id::CanonicalUuid,
    scalar::SchemaU64,
    view_protocol::{
        AggregateValue, Coverage, GroupKey, MAX_METRIC_SERIES, MAX_PAGE_ROWS,
        MAX_TIME_SERIES_POINTS, MAX_TIME_SERIES_SERIES, MetricSeries, MetricSource, OpaqueCursor,
        Pagination, QueryBinding, Reducer, ResultMetadata, ScopeMode, TableColumn, TableRow,
        TimeRangeMode, TimeSeriesPoint, TimeSeriesSeries, TimelineItemType, TimelineRow,
        ViewRecord, ViewResponse, expected_bucket_width_ns,
    },
};

use super::{
    aggregate::{AggregateError, CoverageTally, Exclusion, reduce},
    events::{EventQueryError, FiniteEventQuery, query_events},
    filter::{
        Candidate, CandidateShape, CandidateValue, EventIndex, relevant_kinds_for_metric,
        relevant_kinds_for_table, relevant_kinds_for_timeline, scope_contains,
    },
    pagination::{CursorCodec, CursorError},
    reader::{CapturedEventSource, ReaderFailureClass, ReaderProfile},
};

pub use super::pagination::CursorKey;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Viewport {
    start_ns: SchemaU64,
    end_ns: SchemaU64,
}

impl Viewport {
    pub const fn new(start_ns: SchemaU64, end_ns: SchemaU64) -> Self {
        Self { start_ns, end_ns }
    }

    pub const fn start_ns(self) -> SchemaU64 {
        self.start_ns
    }

    pub const fn end_ns(self) -> SchemaU64 {
        self.end_ns
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ViewQueryRequest {
    viewport: Option<Viewport>,
    selected_scope: Option<DiagnosticScope>,
    cursor: Option<OpaqueCursor>,
    page_size: Option<u16>,
    expected_binding: Option<QueryBinding>,
}

impl ViewQueryRequest {
    pub const fn new() -> Self {
        Self {
            viewport: None,
            selected_scope: None,
            cursor: None,
            page_size: None,
            expected_binding: None,
        }
    }

    pub const fn with_viewport(mut self, viewport: Viewport) -> Self {
        self.viewport = Some(viewport);
        self
    }

    pub fn with_selected_scope(mut self, scope: DiagnosticScope) -> Self {
        self.selected_scope = Some(scope);
        self
    }

    pub fn with_cursor(mut self, cursor: OpaqueCursor) -> Self {
        self.cursor = Some(cursor);
        self
    }

    pub const fn with_page_size(mut self, page_size: u16) -> Self {
        self.page_size = Some(page_size);
        self
    }

    pub fn with_expected_binding(mut self, binding: QueryBinding) -> Self {
        self.expected_binding = Some(binding);
        self
    }

    pub const fn viewport(&self) -> Option<Viewport> {
        self.viewport
    }

    pub const fn selected_scope(&self) -> Option<&DiagnosticScope> {
        self.selected_scope.as_ref()
    }

    pub const fn cursor(&self) -> Option<&OpaqueCursor> {
        self.cursor.as_ref()
    }
}

#[derive(Clone, Debug)]
pub struct ViewQueryExecutionContext {
    available: Arc<AtomicBool>,
}

impl ViewQueryExecutionContext {
    pub fn available() -> Self {
        Self {
            available: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn mark_lost(&self) {
        self.available.store(false, Ordering::Release);
    }

    pub fn is_available(&self) -> bool {
        self.available.load(Ordering::Acquire)
    }
}

#[derive(Clone, Debug)]
pub struct ViewQueryEngine {
    cursors: CursorCodec,
    execution: ViewQueryExecutionContext,
}

impl ViewQueryEngine {
    pub fn new(cursor_key: CursorKey) -> Self {
        Self {
            cursors: CursorCodec::new(cursor_key),
            execution: ViewQueryExecutionContext::available(),
        }
    }

    pub fn with_execution_context(
        cursor_key: CursorKey,
        execution: ViewQueryExecutionContext,
    ) -> Self {
        Self {
            cursors: CursorCodec::new(cursor_key),
            execution,
        }
    }

    pub fn execution_context(&self) -> ViewQueryExecutionContext {
        self.execution.clone()
    }

    pub fn query(
        &self,
        source: &CapturedEventSource<'_>,
        record: &ViewRecord,
        request: &ViewQueryRequest,
    ) -> Result<ViewResponse, ViewQueryError> {
        if !self.execution.is_available() {
            return Err(ViewQueryError::system(
                source.profile(),
                ViewQueryErrorCode::ExecutionContextLost,
                "diagnostic view query execution context is unavailable",
            ));
        }
        record.validate().map_err(|_| {
            ViewQueryError::local(
                ViewQueryErrorCode::InvalidDescriptor,
                "view descriptor is invalid",
            )
        })?;

        let mut events = Vec::with_capacity(
            usize::try_from(source.captured_watermark().get()).unwrap_or(usize::MAX.min(4096)),
        );
        for event in query_events(source, FiniteEventQuery::after(SchemaU64::new(0))) {
            events.push(event.map_err(ViewQueryError::event)?.event().clone());
        }
        let captured_elapsed_end_ns = events.last().map_or(0, |event| {
            event.header().elapsed_ns().get().saturating_add(1)
        });
        let binding = bind_query(
            source.captured_watermark(),
            captured_elapsed_end_ns,
            record,
            request,
        )?;
        if request
            .expected_binding
            .as_ref()
            .is_some_and(|expected| expected != &binding)
        {
            return Err(ViewQueryError::local(
                ViewQueryErrorCode::StaleBinding,
                "captured watermark, viewport, width, or scope binding changed",
            ));
        }
        let run_id = source.metadata().run_id();
        let index = EventIndex::build(&events);
        let response = match record {
            ViewRecord::Timeline(value) => {
                self.timeline(run_id, record, value.query(), binding, request, &index)
            }
            ViewRecord::Metric(value) => {
                self.metric(run_id, record, value.query(), binding, &index)
            }
            ViewRecord::Table(value) => {
                self.table(run_id, record, value.query(), binding, request, &index)
            }
            ViewRecord::TimeSeries(value) => {
                self.time_series(run_id, record, value.query(), binding, &index)
            }
        }
        .map_err(|error| error.for_profile(source.profile()))?;
        response.validate_for(record).map_err(|_| {
            ViewQueryError::system(
                source.profile(),
                ViewQueryErrorCode::ProtocolInvariant,
                "view query result violated the frozen C05 protocol",
            )
        })?;
        Ok(response)
    }

    fn timeline(
        &self,
        run_id: CanonicalUuid,
        record: &ViewRecord,
        query: &troupe_diagnostics_core::view_protocol::TimelineQuery,
        binding: QueryBinding,
        request: &ViewQueryRequest,
        index: &EventIndex,
    ) -> Result<ViewResponse, ViewQueryError> {
        let page_size = request.page_size.unwrap_or(MAX_PAGE_ROWS);
        validate_page_size(page_size)?;
        let mut tally = CoverageTally::default();
        let gaps = index.gap_count(
            binding.range_start_ns().get(),
            binding.range_end_ns().get(),
            binding.selected_scope(),
            &relevant_kinds_for_timeline(query.source()),
        );
        tally.add_gaps(gaps)?;
        let mut rows = Vec::new();
        for candidate in index.timeline(query.source()) {
            if !candidate_matches_binding(&candidate, &binding, true)
                || !candidate.matches_filters(query.filters())
            {
                continue;
            }
            let group = match candidate.group(query.group_by()) {
                Ok(group) => group,
                Err(error) => {
                    tally.exclude(error.exclusion())?;
                    continue;
                }
            };
            tally.contribute()?;
            rows.push(
                TimelineRow::new(
                    SchemaU64::new(candidate.sequence),
                    group,
                    match candidate.shape {
                        CandidateShape::Span => TimelineItemType::Span,
                        CandidateShape::Instant => TimelineItemType::Instant,
                        CandidateShape::Counter | CandidateShape::Token | CandidateShape::Event => {
                            return Err(ViewQueryError::protocol(
                                "timeline source produced a non-timeline candidate",
                            ));
                        }
                    },
                    candidate.name,
                    SchemaU64::new(candidate.start_ns),
                    candidate.end_ns.map(SchemaU64::new),
                    candidate.scope,
                    candidate.outcome,
                )
                .map_err(|_| ViewQueryError::protocol("timeline row is invalid"))?,
            );
        }
        rows.sort_by_key(|row| row.sequence().get());
        let cursor_context = cursor_context(record, &binding, page_size)?;
        let (rows, pagination) =
            self.paginate(rows, page_size, request.cursor(), &cursor_context)?;
        let truncated = tally.resource_truncated() > 0;
        let metadata = metadata(
            run_id,
            record.id(),
            binding,
            tally.into_coverage()?,
            Some(pagination),
            truncated,
        )?;
        ViewResponse::new_timeline(metadata, rows)
            .map_err(|_| ViewQueryError::protocol("timeline response is invalid"))
    }

    fn metric(
        &self,
        run_id: CanonicalUuid,
        record: &ViewRecord,
        query: &troupe_diagnostics_core::view_protocol::MetricQuery,
        binding: QueryBinding,
        index: &EventIndex,
    ) -> Result<ViewResponse, ViewQueryError> {
        let mut candidates = index
            .metric(query.source())
            .into_iter()
            .filter(|candidate| {
                metric_candidate_matches_binding(candidate, &binding)
                    && candidate.matches_filters(query.filters())
            })
            .collect::<Vec<_>>();
        if matches!(query.source(), MetricSource::CounterValue { .. }) {
            candidates = select_latest_counter(candidates);
        }
        let gaps = index.gap_count(
            binding.range_start_ns().get(),
            binding.range_end_ns().get(),
            binding.selected_scope(),
            &relevant_kinds_for_metric(query.source()),
        );
        let (grouped, group_exclusions) = group_candidates(candidates, query.group_by())?;
        if grouped.len() > usize::from(MAX_METRIC_SERIES) {
            return Err(ViewQueryError::local(
                ViewQueryErrorCode::ResourceLimit,
                "metric query exceeds the declared series cap",
            ));
        }
        let mut overall = CoverageTally::default();
        overall.add_gaps(gaps)?;
        for exclusion in group_exclusions {
            overall.exclude(exclusion)?;
        }
        let mut series = Vec::with_capacity(grouped.len());
        for group in grouped {
            let (value, mut coverage) = aggregate_group(query.reducer(), &group.candidates)?;
            coverage.add_gaps(gaps)?;
            overall.merge_without_gaps(&coverage)?;
            series.push(
                MetricSeries::new(group.group, value, coverage.into_coverage()?)
                    .map_err(|_| ViewQueryError::protocol("metric series is invalid"))?,
            );
        }
        let truncated = overall.resource_truncated() > 0;
        let metadata = metadata(
            run_id,
            record.id(),
            binding,
            overall.into_coverage()?,
            None,
            truncated,
        )?;
        ViewResponse::new_metric(metadata, series)
            .map_err(|_| ViewQueryError::protocol("metric response is invalid"))
    }

    fn table(
        &self,
        run_id: CanonicalUuid,
        record: &ViewRecord,
        query: &troupe_diagnostics_core::view_protocol::TableQuery,
        binding: QueryBinding,
        request: &ViewQueryRequest,
        index: &EventIndex,
    ) -> Result<ViewResponse, ViewQueryError> {
        if request
            .page_size
            .is_some_and(|page_size| page_size != query.page_size())
        {
            return Err(ViewQueryError::local(
                ViewQueryErrorCode::InvalidPagination,
                "table page size is fixed by its descriptor",
            ));
        }
        let page_size = query.page_size();
        let mut tally = CoverageTally::default();
        tally.add_gaps(index.gap_count(
            binding.range_start_ns().get(),
            binding.range_end_ns().get(),
            binding.selected_scope(),
            &relevant_kinds_for_table(query.source()),
        ))?;
        let mut rows = Vec::new();
        for candidate in index.table(query.source()) {
            if !candidate_matches_binding(
                &candidate,
                &binding,
                candidate.shape == CandidateShape::Span,
            ) || !candidate.matches_filters(query.filters())
            {
                continue;
            }
            let mut cells = Vec::with_capacity(query.columns().len());
            let mut row_exclusion = candidate
                .resource_truncated
                .then_some(Exclusion::ResourceTruncated);
            for column in query.columns() {
                let (cell, exclusion) = table_cell(&candidate, column)?;
                if row_exclusion.is_none() {
                    row_exclusion = exclusion;
                }
                cells.push(cell);
            }
            match row_exclusion {
                Some(exclusion) => tally.exclude(exclusion)?,
                None => tally.contribute()?,
            }
            rows.push(TableRow::new(SchemaU64::new(candidate.sequence), cells));
        }
        rows.sort_by_key(|row| row.sequence().get());
        let cursor_context = cursor_context(record, &binding, page_size)?;
        let (rows, pagination) =
            self.paginate(rows, page_size, request.cursor(), &cursor_context)?;
        let truncated = tally.resource_truncated() > 0;
        let metadata = metadata(
            run_id,
            record.id(),
            binding,
            tally.into_coverage()?,
            Some(pagination),
            truncated,
        )?;
        ViewResponse::new_table(metadata, query.columns().to_vec(), rows)
            .map_err(|_| ViewQueryError::protocol("table response is invalid"))
    }

    fn time_series(
        &self,
        run_id: CanonicalUuid,
        record: &ViewRecord,
        query: &troupe_diagnostics_core::view_protocol::TimeSeriesQuery,
        binding: QueryBinding,
        index: &EventIndex,
    ) -> Result<ViewResponse, ViewQueryError> {
        let range_start = binding.range_start_ns().get();
        let range_end = binding.range_end_ns().get();
        let width = expected_bucket_width_ns(range_start, range_end)
            .map_err(|_| ViewQueryError::protocol("time-series width is invalid"))?;
        let buckets = canonical_buckets(range_start, range_end, width.get())?;
        let all_candidates = index
            .metric(query.source())
            .into_iter()
            .filter_map(|mut candidate| {
                if !metric_candidate_matches_binding(&candidate, &binding)
                    || !candidate.matches_filters(query.filters())
                {
                    return None;
                }
                if candidate.shape == CandidateShape::Span
                    && matches!(&candidate.value, CandidateValue::OpenSpan)
                {
                    candidate.timestamp_ns =
                        candidate.timestamp_ns.max(binding.range_start_ns().get());
                }
                Some(candidate)
            })
            .collect::<Vec<_>>();
        let groups = discover_groups(&all_candidates, query.group_by())?;
        if groups.len() > usize::from(MAX_TIME_SERIES_SERIES) {
            return Err(ViewQueryError::local(
                ViewQueryErrorCode::ResourceLimit,
                "time-series query exceeds the declared series cap",
            ));
        }
        let relevant_kinds = relevant_kinds_for_metric(query.source());
        let mut overall = CoverageTally::default();
        overall.add_gaps(index.gap_count(
            range_start,
            range_end,
            binding.selected_scope(),
            &relevant_kinds,
        ))?;
        let mut result_series = Vec::with_capacity(groups.len());
        for group in groups {
            let mut points = Vec::with_capacity(buckets.len());
            for bucket in &buckets {
                let mut candidates = all_candidates
                    .iter()
                    .filter(|candidate| {
                        bucket_contains(bucket, candidate.timestamp_ns)
                            && candidate_group_matches(candidate, query.group_by(), group.as_ref())
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                if matches!(query.source(), MetricSource::CounterValue { .. }) {
                    candidates = select_latest_counter(candidates);
                }
                let (value, mut coverage) = aggregate_group(query.reducer(), &candidates)?;
                coverage.add_gaps(index.gap_count_for_bucket(
                    range_start,
                    range_end,
                    bucket.start,
                    bucket.end,
                    binding.selected_scope(),
                    &relevant_kinds,
                ))?;
                overall.merge_without_gaps(&coverage)?;
                points.push(
                    TimeSeriesPoint::new(
                        SchemaU64::new(bucket.start),
                        SchemaU64::new(bucket.end),
                        bucket.partial,
                        value,
                        coverage.into_coverage()?,
                    )
                    .map_err(|_| ViewQueryError::protocol("time-series point is invalid"))?,
                );
            }
            result_series.push(TimeSeriesSeries::new(group, points));
        }
        for error in all_candidates
            .iter()
            .filter_map(|candidate| candidate.group(query.group_by()).err())
        {
            overall.exclude(error.exclusion())?;
        }
        let truncated = overall.resource_truncated() > 0;
        let metadata = metadata(
            run_id,
            record.id(),
            binding,
            overall.into_coverage()?,
            None,
            truncated,
        )?;
        ViewResponse::new_time_series(metadata, width, result_series)
            .map_err(|_| ViewQueryError::protocol("time-series response is invalid"))
    }

    fn paginate<T>(
        &self,
        rows: Vec<T>,
        page_size: u16,
        cursor: Option<&OpaqueCursor>,
        cursor_context: &[u8],
    ) -> Result<(Vec<T>, Pagination), ViewQueryError> {
        let offset = match cursor {
            Some(cursor) => self.cursors.decode(cursor, cursor_context)?,
            None => 0,
        };
        let start = usize::try_from(offset).map_err(|_| {
            ViewQueryError::local(
                ViewQueryErrorCode::InvalidCursor,
                "cursor offset is too large",
            )
        })?;
        if start > rows.len() {
            return Err(ViewQueryError::local(
                ViewQueryErrorCode::InvalidCursor,
                "cursor offset lies beyond the query result",
            ));
        }
        let end = start.saturating_add(usize::from(page_size)).min(rows.len());
        let next_cursor = (end < rows.len()).then(|| {
            self.cursors.encode(
                u64::try_from(end).expect("row offset fits u64"),
                cursor_context,
            )
        });
        let page = rows.into_iter().skip(start).take(end - start).collect();
        let pagination = Pagination::new(page_size, next_cursor)
            .map_err(|_| ViewQueryError::protocol("pagination state is invalid"))?;
        Ok((page, pagination))
    }
}

impl Default for ViewQueryEngine {
    fn default() -> Self {
        Self {
            cursors: CursorCodec::default(),
            execution: ViewQueryExecutionContext::available(),
        }
    }
}

fn bind_query(
    captured_watermark: SchemaU64,
    captured_elapsed_end_ns: u64,
    record: &ViewRecord,
    request: &ViewQueryRequest,
) -> Result<QueryBinding, ViewQueryError> {
    let (range_start, range_end) = match (record.time_range(), request.viewport) {
        (TimeRangeMode::Run, None) => (0, captured_elapsed_end_ns),
        (TimeRangeMode::Run, Some(_)) => {
            return Err(ViewQueryError::local(
                ViewQueryErrorCode::InvalidBinding,
                "run-bound view cannot accept viewport bounds",
            ));
        }
        (TimeRangeMode::Viewport, Some(viewport))
            if viewport.start_ns().get() <= viewport.end_ns().get()
                && viewport.end_ns().get() <= captured_elapsed_end_ns =>
        {
            (viewport.start_ns().get(), viewport.end_ns().get())
        }
        (TimeRangeMode::Viewport, Some(_)) => {
            return Err(ViewQueryError::local(
                ViewQueryErrorCode::InvalidBinding,
                "viewport lies outside captured Run time",
            ));
        }
        (TimeRangeMode::Viewport, None) => {
            return Err(ViewQueryError::local(
                ViewQueryErrorCode::InvalidBinding,
                "viewport-bound view requires exact viewport bounds",
            ));
        }
    };
    match (record.scope(), request.selected_scope.as_ref()) {
        (ScopeMode::Run, None) | (ScopeMode::Selection, Some(_)) => {}
        (ScopeMode::Run, Some(_)) => {
            return Err(ViewQueryError::local(
                ViewQueryErrorCode::InvalidBinding,
                "run-scoped view cannot accept a selected scope",
            ));
        }
        (ScopeMode::Selection, None) => {
            return Err(ViewQueryError::local(
                ViewQueryErrorCode::InvalidBinding,
                "selection-scoped view requires a selected scope",
            ));
        }
    }
    QueryBinding::new(
        captured_watermark,
        SchemaU64::new(captured_elapsed_end_ns),
        record.time_range(),
        SchemaU64::new(range_start),
        SchemaU64::new(range_end),
        record.scope(),
        request.selected_scope.clone(),
    )
    .map_err(|_| {
        ViewQueryError::local(
            ViewQueryErrorCode::InvalidBinding,
            "scope or time binding is invalid",
        )
    })
}

fn validate_page_size(page_size: u16) -> Result<(), ViewQueryError> {
    if page_size == 0 || page_size > MAX_PAGE_ROWS {
        return Err(ViewQueryError::local(
            ViewQueryErrorCode::InvalidPagination,
            "page size must be in 1..=500",
        ));
    }
    Ok(())
}

fn cursor_context(
    record: &ViewRecord,
    binding: &QueryBinding,
    page_size: u16,
) -> Result<Vec<u8>, ViewQueryError> {
    serde_json::to_vec(&(record, binding, page_size))
        .map_err(|_| ViewQueryError::protocol("query binding could not be encoded for pagination"))
}

fn candidate_matches_binding(
    candidate: &Candidate,
    binding: &QueryBinding,
    span_intersection: bool,
) -> bool {
    if binding
        .selected_scope()
        .is_some_and(|scope| !scope_contains(scope, &candidate.scope))
    {
        return false;
    }
    let start = binding.range_start_ns().get();
    let end = binding.range_end_ns().get();
    if span_intersection && candidate.shape == CandidateShape::Span {
        let candidate_end = candidate
            .end_ns
            .unwrap_or(binding.captured_elapsed_end_ns().get());
        start < end && candidate.start_ns < end && candidate_end > start
    } else {
        start <= candidate.timestamp_ns && candidate.timestamp_ns < end
    }
}

fn metric_candidate_matches_binding(candidate: &Candidate, binding: &QueryBinding) -> bool {
    let open_span = candidate.shape == CandidateShape::Span
        && matches!(&candidate.value, CandidateValue::OpenSpan);
    candidate_matches_binding(candidate, binding, open_span)
}

#[derive(Debug)]
struct CandidateGroup {
    group: Option<GroupKey>,
    candidates: Vec<Candidate>,
}

fn group_candidates(
    candidates: Vec<Candidate>,
    dimension: Option<&troupe_diagnostics_core::view_protocol::GroupDimension>,
) -> Result<(Vec<CandidateGroup>, Vec<Exclusion>), ViewQueryError> {
    let mut groups: BTreeMap<String, CandidateGroup> = BTreeMap::new();
    let mut exclusions = Vec::new();
    if dimension.is_none() {
        groups.insert(
            String::new(),
            CandidateGroup {
                group: None,
                candidates: Vec::new(),
            },
        );
    }
    for candidate in candidates {
        let group = match candidate.group(dimension) {
            Ok(group) => group,
            Err(error) => {
                exclusions.push(error.exclusion());
                continue;
            }
        };
        let key = if dimension.is_none() {
            String::new()
        } else {
            serde_json::to_string(&group)
                .map_err(|_| ViewQueryError::protocol("group key is not serializable"))?
        };
        groups
            .entry(key)
            .or_insert_with(|| CandidateGroup {
                group,
                candidates: Vec::new(),
            })
            .candidates
            .push(candidate);
    }
    Ok((groups.into_values().collect(), exclusions))
}

fn aggregate_group(
    reducer: Reducer,
    candidates: &[Candidate],
) -> Result<(Option<AggregateValue>, CoverageTally), ViewQueryError> {
    let mut tally = CoverageTally::default();
    let mut values = Vec::new();
    for candidate in candidates {
        if candidate.resource_truncated {
            tally.exclude(Exclusion::ResourceTruncated)?;
            continue;
        }
        match &candidate.value {
            CandidateValue::Exact(value) => {
                tally.contribute()?;
                values.push((candidate.sequence, value.clone()));
            }
            value => tally.exclude(
                value
                    .exclusion()
                    .expect("non-exact candidate has an exclusion"),
            )?,
        }
    }
    Ok((reduce(reducer, &values)?, tally))
}

fn select_latest_counter(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut latest: BTreeMap<String, Candidate> = BTreeMap::new();
    for candidate in candidates {
        let identity = candidate
            .series_identity
            .clone()
            .expect("counter candidate has exact series identity");
        let replace = latest
            .get(&identity)
            .is_none_or(|current| candidate.sequence > current.sequence);
        if replace {
            latest.insert(identity, candidate);
        }
    }
    latest.into_values().collect()
}

fn discover_groups(
    candidates: &[Candidate],
    dimension: Option<&troupe_diagnostics_core::view_protocol::GroupDimension>,
) -> Result<Vec<Option<GroupKey>>, ViewQueryError> {
    if dimension.is_none() {
        return Ok(vec![None]);
    }
    let mut groups = BTreeMap::new();
    for candidate in candidates {
        let Ok(group) = candidate.group(dimension) else {
            continue;
        };
        let key = serde_json::to_string(&group)
            .map_err(|_| ViewQueryError::protocol("group key is not serializable"))?;
        groups.entry(key).or_insert(group);
    }
    Ok(groups.into_values().collect())
}

fn candidate_group_matches(
    candidate: &Candidate,
    dimension: Option<&troupe_diagnostics_core::view_protocol::GroupDimension>,
    expected: Option<&GroupKey>,
) -> bool {
    candidate
        .group(dimension)
        .is_ok_and(|actual| actual.as_ref() == expected)
}

#[derive(Clone, Copy, Debug)]
struct Bucket {
    start: u64,
    end: u64,
    partial: bool,
}

fn canonical_buckets(start: u64, end: u64, width: u64) -> Result<Vec<Bucket>, ViewQueryError> {
    if start == end {
        return Ok(Vec::new());
    }
    let mut bucket_start = start / width * width;
    let mut buckets = Vec::new();
    while bucket_start < end {
        let bucket_end = bucket_start.checked_add(width).ok_or_else(|| {
            ViewQueryError::local(
                ViewQueryErrorCode::InvalidBinding,
                "time-series bucket overflows Run time",
            )
        })?;
        buckets.push(Bucket {
            start: bucket_start,
            end: bucket_end,
            partial: bucket_start < start || bucket_end > end,
        });
        if buckets.len() > usize::from(MAX_TIME_SERIES_POINTS) {
            return Err(ViewQueryError::protocol(
                "canonical time-series width exceeded the point cap",
            ));
        }
        bucket_start = bucket_end;
    }
    Ok(buckets)
}

fn bucket_contains(bucket: &Bucket, timestamp: u64) -> bool {
    bucket.start <= timestamp && timestamp < bucket.end
}

fn table_cell(
    candidate: &Candidate,
    column: &TableColumn,
) -> Result<(Option<DiagnosticScalar>, Option<Exclusion>), ViewQueryError> {
    let string = |value: Option<&str>| {
        value
            .map(|value| (Some(DiagnosticScalar::String(value.to_owned())), None))
            .unwrap_or((None, Some(Exclusion::MissingValue)))
    };
    let exact_integer = |value: &str| {
        CanonicalInteger::parse(value)
            .map(DiagnosticScalar::Integer)
            .map(Some)
            .map(|value| (value, None))
            .map_err(|_| ViewQueryError::protocol("table integer cell is invalid"))
    };
    match column {
        TableColumn::Sequence => exact_integer(&candidate.sequence.to_string()),
        TableColumn::ElapsedNs => exact_integer(&candidate.timestamp_ns.to_string()),
        TableColumn::EventKind => Ok(string(Some(candidate.event_kind.as_str()))),
        TableColumn::SpanKind => Ok(string(candidate.span_kind.map(|kind| kind.as_str()))),
        TableColumn::InstantKind => Ok(string(candidate.instant_kind.map(|kind| kind.as_str()))),
        TableColumn::CounterKind => Ok(string(candidate.counter_kind.map(|kind| kind.as_str()))),
        TableColumn::SceneId => Ok(string(
            candidate.scope.scene_id().map(|value| value.as_str()),
        )),
        TableColumn::ActorId => Ok(string(
            candidate.scope.actor_id().map(|value| value.as_str()),
        )),
        TableColumn::CueId => Ok(string(candidate.scope.cue_id().map(|value| value.as_str()))),
        TableColumn::ActId => Ok(string(candidate.scope.act_id().map(|value| value.as_str()))),
        TableColumn::CustomName => Ok(string(candidate.custom_name.as_deref())),
        TableColumn::Outcome => Ok(string(candidate.outcome.map(|value| value.as_str()))),
        TableColumn::Severity => Ok(string(candidate.severity.map(|value| value.as_str()))),
        TableColumn::Attribute { key } => match candidate.attributes.get(key) {
            Some(super::filter::ScalarField::Scalar(value)) => Ok((Some(value.clone()), None)),
            Some(super::filter::ScalarField::NonScalar) => {
                Ok((None, Some(Exclusion::NonNumericValue)))
            }
            None => match candidate.dimensions.get(key) {
                Some(value) => Ok((Some(value.clone()), None)),
                None => Ok((None, Some(Exclusion::MissingValue))),
            },
        },
        TableColumn::Token { metric } => match candidate.token(*metric) {
            Some(value) => exact_integer(value.as_str()),
            None if candidate.shape == CandidateShape::Token => {
                let exclusion = if candidate.token_values.iter().any(Option::is_some) {
                    Exclusion::MissingValue
                } else {
                    Exclusion::UnavailableValue
                };
                Ok((None, Some(exclusion)))
            }
            None => Ok((None, Some(Exclusion::MissingValue))),
        },
        TableColumn::Value => match &candidate.value {
            CandidateValue::Exact(value) => {
                let scalar = match value.clone().into_exact_number()? {
                    troupe_diagnostics_core::view_protocol::ExactNumber::Integer(value) => {
                        DiagnosticScalar::Integer(value)
                    }
                    troupe_diagnostics_core::view_protocol::ExactNumber::Decimal(value) => {
                        DiagnosticScalar::Decimal(value)
                    }
                };
                Ok((Some(scalar), None))
            }
            value => Ok((None, value.exclusion())),
        },
    }
}

fn metadata(
    run_id: CanonicalUuid,
    view_id: &str,
    binding: QueryBinding,
    coverage: Coverage,
    pagination: Option<Pagination>,
    truncated: bool,
) -> Result<ResultMetadata, ViewQueryError> {
    ResultMetadata::new(
        run_id,
        view_id.to_owned(),
        binding,
        coverage,
        pagination,
        truncated,
        None,
    )
    .map_err(|_| ViewQueryError::protocol("view result metadata is invalid"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewQueryErrorClass {
    LocalQuery,
    CoreFatal,
    ArchiveOperation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewQueryErrorCode {
    InvalidDescriptor,
    InvalidBinding,
    StaleBinding,
    InvalidPagination,
    InvalidCursor,
    ResourceLimit,
    EventRead,
    ProtocolInvariant,
    ExecutionContextLost,
}

impl ViewQueryErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidDescriptor => "diagnostic_view.invalid_descriptor",
            Self::InvalidBinding => "diagnostic_view.invalid_binding",
            Self::StaleBinding => "diagnostic_view.stale_binding",
            Self::InvalidPagination => "diagnostic_view.invalid_pagination",
            Self::InvalidCursor => "diagnostic_view.invalid_cursor",
            Self::ResourceLimit => "diagnostic_view.resource_limit",
            Self::EventRead => "diagnostic_view.event_read",
            Self::ProtocolInvariant => "diagnostic_view.protocol_invariant",
            Self::ExecutionContextLost => "diagnostic_view.execution_context_lost",
        }
    }
}

#[derive(Debug)]
enum ViewQueryErrorSource {
    Event(EventQueryError),
    Cursor(CursorError),
    Aggregate(AggregateError),
    Detail(&'static str),
}

#[derive(Debug)]
pub struct ViewQueryError {
    class: ViewQueryErrorClass,
    profile: Option<ReaderProfile>,
    code: ViewQueryErrorCode,
    source: ViewQueryErrorSource,
}

impl ViewQueryError {
    fn local(code: ViewQueryErrorCode, detail: &'static str) -> Self {
        Self {
            class: ViewQueryErrorClass::LocalQuery,
            profile: None,
            code,
            source: ViewQueryErrorSource::Detail(detail),
        }
    }

    fn system(profile: ReaderProfile, code: ViewQueryErrorCode, detail: &'static str) -> Self {
        Self {
            class: class_for_profile(profile),
            profile: Some(profile),
            code,
            source: ViewQueryErrorSource::Detail(detail),
        }
    }

    fn protocol(detail: &'static str) -> Self {
        Self {
            class: ViewQueryErrorClass::CoreFatal,
            profile: None,
            code: ViewQueryErrorCode::ProtocolInvariant,
            source: ViewQueryErrorSource::Detail(detail),
        }
    }

    fn event(error: EventQueryError) -> Self {
        Self {
            class: match error.class() {
                ReaderFailureClass::CoreFatal => ViewQueryErrorClass::CoreFatal,
                ReaderFailureClass::ArchiveOperation => ViewQueryErrorClass::ArchiveOperation,
            },
            profile: Some(error.profile()),
            code: ViewQueryErrorCode::EventRead,
            source: ViewQueryErrorSource::Event(error),
        }
    }

    fn for_profile(mut self, profile: ReaderProfile) -> Self {
        if self.class != ViewQueryErrorClass::LocalQuery && self.profile.is_none() {
            self.class = class_for_profile(profile);
            self.profile = Some(profile);
        }
        self
    }

    pub const fn class(&self) -> ViewQueryErrorClass {
        self.class
    }

    pub const fn profile(&self) -> Option<ReaderProfile> {
        self.profile
    }

    pub const fn code(&self) -> ViewQueryErrorCode {
        self.code
    }
}

impl fmt::Display for ViewQueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "diagnostic view query failed [{}]: ",
            self.code.as_str()
        )?;
        match &self.source {
            ViewQueryErrorSource::Event(error) => fmt::Display::fmt(error, formatter),
            ViewQueryErrorSource::Cursor(error) => fmt::Display::fmt(error, formatter),
            ViewQueryErrorSource::Aggregate(error) => fmt::Display::fmt(error, formatter),
            ViewQueryErrorSource::Detail(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for ViewQueryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.source {
            ViewQueryErrorSource::Event(error) => Some(error),
            ViewQueryErrorSource::Cursor(error) => Some(error),
            ViewQueryErrorSource::Aggregate(error) => Some(error),
            ViewQueryErrorSource::Detail(_) => None,
        }
    }
}

impl From<CursorError> for ViewQueryError {
    fn from(error: CursorError) -> Self {
        Self {
            class: ViewQueryErrorClass::LocalQuery,
            profile: None,
            code: ViewQueryErrorCode::InvalidCursor,
            source: ViewQueryErrorSource::Cursor(error),
        }
    }
}

impl From<AggregateError> for ViewQueryError {
    fn from(error: AggregateError) -> Self {
        Self {
            class: ViewQueryErrorClass::CoreFatal,
            profile: None,
            code: ViewQueryErrorCode::ProtocolInvariant,
            source: ViewQueryErrorSource::Aggregate(error),
        }
    }
}

const fn class_for_profile(profile: ReaderProfile) -> ViewQueryErrorClass {
    match profile {
        ReaderProfile::Active => ViewQueryErrorClass::CoreFatal,
        ReaderProfile::Archive => ViewQueryErrorClass::ArchiveOperation,
    }
}
