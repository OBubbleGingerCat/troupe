use std::collections::{BTreeMap, HashMap};

use troupe_diagnostics_core::{
    detail::{
        CustomNumber, DiagnosticAttributeValue, DiagnosticAttributes, DiagnosticDimension,
        DiagnosticDimensions, DiagnosticScalar,
    },
    event::{
        ActTokenUsageFinalized, DiagnosticEvent, DiagnosticEventKind, DiagnosticScope,
        ObservationGap,
    },
    kinds::{CounterKind, CustomSeverity, InstantKind, SpanKind, SpanOutcome, UsageAvailability},
    scalar::TokenCount,
    view_protocol::{
        CounterSelector, GroupDimension, GroupKey, InstantSelector, METRIC_UNIT_COUNT,
        METRIC_UNIT_NANOSECONDS, METRIC_UNIT_TOKENS, MetricSource, QueryFilter, SpanSelector,
        TableSource, TimelineSource, TokenMetric,
    },
};

use super::aggregate::{ExactNumeric, Exclusion};

#[derive(Clone, Debug)]
pub(crate) enum CandidateValue {
    Exact(ExactNumeric),
    OpenSpan,
    Missing,
    NonNumeric,
    Unavailable,
}

impl CandidateValue {
    pub(crate) const fn exclusion(&self) -> Option<Exclusion> {
        match self {
            Self::Exact(_) => None,
            Self::OpenSpan => Some(Exclusion::OpenSpan),
            Self::Missing => Some(Exclusion::MissingValue),
            Self::NonNumeric => Some(Exclusion::NonNumericValue),
            Self::Unavailable => Some(Exclusion::UnavailableValue),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CandidateShape {
    Span,
    Instant,
    Counter,
    Token,
    Event,
}

#[derive(Clone, Debug)]
pub(crate) struct Candidate {
    pub(crate) sequence: u64,
    pub(crate) timestamp_ns: u64,
    pub(crate) start_ns: u64,
    pub(crate) end_ns: Option<u64>,
    pub(crate) scope: DiagnosticScope,
    pub(crate) event_kind: DiagnosticEventKind,
    pub(crate) shape: CandidateShape,
    pub(crate) name: String,
    pub(crate) span_kind: Option<SpanKind>,
    pub(crate) instant_kind: Option<InstantKind>,
    pub(crate) counter_kind: Option<CounterKind>,
    pub(crate) custom_name: Option<String>,
    pub(crate) outcome: Option<SpanOutcome>,
    pub(crate) severity: Option<CustomSeverity>,
    pub(crate) attributes: BTreeMap<String, ScalarField>,
    pub(crate) dimensions: BTreeMap<String, DiagnosticScalar>,
    pub(crate) value: CandidateValue,
    pub(crate) token_values: [Option<TokenCount>; 6],
    pub(crate) resource_truncated: bool,
    pub(crate) series_identity: Option<String>,
    pub(crate) metric_unit: Option<String>,
}

impl Candidate {
    pub(crate) fn matches_filters(&self, filters: &[QueryFilter]) -> bool {
        filters.iter().all(|filter| match filter {
            QueryFilter::Severity { value } => self.severity == Some(*value),
            QueryFilter::Outcome { value } => self.outcome == Some(*value),
            QueryFilter::AttributeEquals { key, value } => {
                self.scalar_field(key).is_some_and(|actual| actual == value)
            }
            QueryFilter::AttributeExists { key } => {
                self.attributes.contains_key(key) || self.dimensions.contains_key(key)
            }
        })
    }

    pub(crate) fn group(
        &self,
        dimension: Option<&GroupDimension>,
    ) -> Result<Option<GroupKey>, GroupValueError> {
        let Some(dimension) = dimension else {
            return Ok(None);
        };
        let value = match dimension {
            GroupDimension::Scene => id_scalar(self.scope.scene_id().map(|value| value.as_str()))?,
            GroupDimension::Actor => id_scalar(self.scope.actor_id().map(|value| value.as_str()))?,
            GroupDimension::Cue => id_scalar(self.scope.cue_id().map(|value| value.as_str()))?,
            GroupDimension::Act => id_scalar(self.scope.act_id().map(|value| value.as_str()))?,
            GroupDimension::EventName => DiagnosticScalar::String(self.name.clone()),
            GroupDimension::CustomName => self
                .custom_name
                .as_ref()
                .map(|value| DiagnosticScalar::String(value.clone()))
                .ok_or(GroupValueError::Missing)?,
            GroupDimension::Attribute { key } => match self.attributes.get(key) {
                Some(ScalarField::Scalar(value)) => value.clone(),
                Some(ScalarField::NonScalar) => return Err(GroupValueError::NonScalar),
                None => self
                    .dimensions
                    .get(key)
                    .cloned()
                    .ok_or(GroupValueError::Missing)?,
            },
            GroupDimension::CustomDimension { key } => self
                .dimensions
                .get(key)
                .cloned()
                .ok_or(GroupValueError::Missing)?,
        };
        GroupKey::new(dimension.clone(), value)
            .map(Some)
            .map_err(|_| GroupValueError::Invalid)
    }

    pub(crate) fn scalar_field(&self, key: &str) -> Option<&DiagnosticScalar> {
        match self.attributes.get(key) {
            Some(ScalarField::Scalar(value)) => Some(value),
            Some(ScalarField::NonScalar) => None,
            None => self.dimensions.get(key),
        }
    }

    pub(crate) fn token(&self, metric: TokenMetric) -> Option<&TokenCount> {
        self.token_values[token_index(metric)].as_ref()
    }
}

fn id_scalar(value: Option<&str>) -> Result<DiagnosticScalar, GroupValueError> {
    value
        .map(|value| DiagnosticScalar::String(value.to_owned()))
        .ok_or(GroupValueError::Missing)
}

#[derive(Clone, Debug)]
pub(crate) enum ScalarField {
    Scalar(DiagnosticScalar),
    NonScalar,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GroupValueError {
    Missing,
    NonScalar,
    Invalid,
}

impl GroupValueError {
    pub(crate) const fn exclusion(self) -> Exclusion {
        match self {
            Self::Missing => Exclusion::MissingValue,
            Self::NonScalar | Self::Invalid => Exclusion::NonNumericValue,
        }
    }
}

#[derive(Clone, Debug)]
struct SpanFact {
    sequence: u64,
    start_ns: u64,
    end_sequence: Option<u64>,
    end_ns: Option<u64>,
    scope: DiagnosticScope,
    span_kind: Option<SpanKind>,
    custom_name: Option<String>,
    outcome: Option<SpanOutcome>,
    attributes: BTreeMap<String, ScalarField>,
}

#[derive(Clone, Debug)]
pub(crate) struct GapFact {
    timestamp_ns: u64,
    affected_start_ns: Option<u64>,
    affected_end_ns: Option<u64>,
    affected_kind: Option<DiagnosticEventKind>,
    affected_scope: Option<DiagnosticScope>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct EventIndex {
    spans: Vec<SpanFact>,
    events: Vec<DiagnosticEvent>,
    gaps: Vec<GapFact>,
}

impl EventIndex {
    pub(crate) fn build(events: &[DiagnosticEvent]) -> Self {
        let mut spans = Vec::new();
        let mut by_id = HashMap::new();
        for event in events {
            match event {
                DiagnosticEvent::SpanStarted(value) => {
                    let sequence = value.header().sequence().get();
                    by_id.insert(sequence, spans.len());
                    spans.push(SpanFact {
                        sequence,
                        start_ns: value.header().elapsed_ns().get(),
                        end_sequence: None,
                        end_ns: None,
                        scope: value.header().scope().clone(),
                        span_kind: Some(value.span_kind()),
                        custom_name: None,
                        outcome: None,
                        attributes: BTreeMap::new(),
                    });
                }
                DiagnosticEvent::CustomSpanStarted(value) => {
                    let sequence = value.header().sequence().get();
                    by_id.insert(sequence, spans.len());
                    spans.push(SpanFact {
                        sequence,
                        start_ns: value.header().elapsed_ns().get(),
                        end_sequence: None,
                        end_ns: None,
                        scope: value.header().scope().clone(),
                        span_kind: None,
                        custom_name: Some(value.name().to_owned()),
                        outcome: None,
                        attributes: convert_attributes(value.attributes()),
                    });
                }
                DiagnosticEvent::SpanFinished(value) => {
                    if let Some(span) = by_id
                        .get(&value.span_id().get())
                        .and_then(|index| spans.get_mut(*index))
                    {
                        span.end_sequence = Some(value.header().sequence().get());
                        span.end_ns = Some(value.header().elapsed_ns().get());
                        span.outcome = Some(value.outcome());
                    }
                }
                DiagnosticEvent::CustomSpanFinished(value) => {
                    if let Some(span) = by_id
                        .get(&value.span_id().get())
                        .and_then(|index| spans.get_mut(*index))
                    {
                        span.end_sequence = Some(value.header().sequence().get());
                        span.end_ns = Some(value.header().elapsed_ns().get());
                        span.outcome = Some(value.outcome());
                    }
                }
                _ => {}
            }
        }
        let gaps = events
            .iter()
            .filter_map(|event| match event {
                DiagnosticEvent::ObservationGap(value) => Some(gap_fact(value)),
                _ => None,
            })
            .collect();
        Self {
            spans,
            events: events.to_vec(),
            gaps,
        }
    }

    pub(crate) fn timeline(&self, source: &TimelineSource) -> Vec<Candidate> {
        match source {
            TimelineSource::Span { selector } => self
                .spans
                .iter()
                .filter(|span| span_matches(span, selector))
                .map(span_candidate)
                .collect(),
            TimelineSource::Instant { selector } => self
                .events
                .iter()
                .filter_map(|event| instant_candidate(event, selector))
                .collect(),
        }
    }

    pub(crate) fn metric(&self, source: &MetricSource) -> Vec<Candidate> {
        match source {
            MetricSource::CounterValue { selector, .. } => self
                .events
                .iter()
                .filter_map(|event| counter_candidate(event, selector))
                .collect(),
            MetricSource::InstantCount { selector } => self
                .events
                .iter()
                .filter_map(|event| instant_candidate(event, selector))
                .map(|mut candidate| {
                    candidate.value = CandidateValue::Exact(ExactNumeric::integer(1_u8));
                    candidate.metric_unit = Some(METRIC_UNIT_COUNT.to_owned());
                    candidate
                })
                .collect(),
            MetricSource::CompletedSpanDuration { selector } => self
                .spans
                .iter()
                .filter(|span| span_matches(span, selector))
                .map(metric_span_candidate)
                .collect(),
            MetricSource::ActToken { metric } => self
                .events
                .iter()
                .filter_map(|event| token_candidate(event, *metric))
                .collect(),
        }
    }

    pub(crate) fn table(&self, source: &TableSource) -> Vec<Candidate> {
        match source {
            TableSource::Event { kind } => self
                .events
                .iter()
                .filter(|event| event.kind() == *kind)
                .map(|event| self.raw_event_candidate(event))
                .collect(),
            TableSource::Span { selector } => self
                .spans
                .iter()
                .filter(|span| span_matches(span, selector))
                .map(table_span_candidate)
                .collect(),
            TableSource::Instant { selector } => self
                .events
                .iter()
                .filter_map(|event| instant_candidate(event, selector))
                .collect(),
            TableSource::Counter { selector } => self
                .events
                .iter()
                .filter_map(|event| counter_candidate(event, selector))
                .collect(),
            TableSource::ActTokenUsage => self
                .events
                .iter()
                .filter_map(|event| token_candidate(event, TokenMetric::ProviderTotalTokens))
                .collect(),
        }
    }

    pub(crate) fn gap_count(
        &self,
        range_start_ns: u64,
        range_end_ns: u64,
        selected_scope: Option<&DiagnosticScope>,
        relevant_kinds: &[DiagnosticEventKind],
    ) -> u64 {
        self.gaps
            .iter()
            .filter(|gap| {
                gap_intersects(gap, range_start_ns, range_end_ns)
                    && gap
                        .affected_kind
                        .is_none_or(|kind| relevant_kinds.contains(&kind))
                    && selected_scope.is_none_or(|scope| {
                        gap.affected_scope
                            .as_ref()
                            .is_none_or(|affected| scopes_overlap(scope, affected))
                    })
            })
            .count() as u64
    }

    pub(crate) fn gap_count_for_bucket(
        &self,
        query_start_ns: u64,
        query_end_ns: u64,
        bucket_start_ns: u64,
        bucket_end_ns: u64,
        selected_scope: Option<&DiagnosticScope>,
        relevant_kinds: &[DiagnosticEventKind],
    ) -> u64 {
        let start = query_start_ns.max(bucket_start_ns);
        let end = query_end_ns.min(bucket_end_ns);
        self.gap_count(start, end, selected_scope, relevant_kinds)
    }

    fn raw_event_candidate(&self, event: &DiagnosticEvent) -> Candidate {
        let header = event.header();
        let mut candidate = empty_candidate(
            header.sequence().get(),
            header.elapsed_ns().get(),
            header.scope().clone(),
            event.kind(),
            CandidateShape::Event,
            event.kind().as_str().to_owned(),
        );
        match event {
            DiagnosticEvent::SpanStarted(value) => candidate.span_kind = Some(value.span_kind()),
            DiagnosticEvent::SpanFinished(value) => {
                candidate.outcome = Some(value.outcome());
                candidate.span_kind = self
                    .spans
                    .iter()
                    .find(|span| span.sequence == value.span_id().get())
                    .and_then(|span| span.span_kind);
            }
            DiagnosticEvent::InstantOccurred(value) => {
                candidate.instant_kind = Some(value.instant_kind())
            }
            DiagnosticEvent::CounterSampled(value) => {
                candidate.counter_kind = Some(value.counter_kind());
                candidate.value = CandidateValue::Exact(ExactNumeric::integer(value.value().get()));
            }
            DiagnosticEvent::ActTokenUsageFinalized(value) => {
                fill_token_fields(&mut candidate, value);
                candidate.value = token_value(value, TokenMetric::ProviderTotalTokens);
            }
            DiagnosticEvent::CustomSpanStarted(value) => {
                candidate.custom_name = Some(value.name().to_owned());
                candidate.attributes = convert_attributes(value.attributes());
            }
            DiagnosticEvent::CustomSpanFinished(value) => {
                candidate.outcome = Some(value.outcome());
                candidate.custom_name = self
                    .spans
                    .iter()
                    .find(|span| span.sequence == value.span_id().get())
                    .and_then(|span| span.custom_name.clone());
            }
            DiagnosticEvent::CustomInstantOccurred(value) => {
                candidate.custom_name = Some(value.name().to_owned());
                candidate.severity = value.severity();
                candidate.attributes = convert_attributes(value.attributes());
            }
            DiagnosticEvent::CustomCounterSampled(value) => {
                candidate.custom_name = Some(value.name().to_owned());
                candidate.dimensions = convert_dimensions(value.dimensions());
                candidate.value = custom_number(value.value());
            }
            DiagnosticEvent::AgentMessageCompleted(value) => {
                candidate.resource_truncated = value.truncated()
            }
            DiagnosticEvent::AgentPlanSnapshot(value) => {
                candidate.resource_truncated = value.truncated()
            }
            DiagnosticEvent::AgentMessageDelta(_)
            | DiagnosticEvent::ContextUsageSampled(_)
            | DiagnosticEvent::ObservationGap(_) => {}
        }
        candidate
    }
}

fn gap_fact(value: &ObservationGap) -> GapFact {
    GapFact {
        timestamp_ns: value.header().elapsed_ns().get(),
        affected_start_ns: value
            .affected_elapsed()
            .map(|interval| interval.start_ns().get()),
        affected_end_ns: value
            .affected_elapsed()
            .map(|interval| interval.end_ns().get()),
        affected_kind: value.affected_kind(),
        affected_scope: value.affected_scope().cloned(),
    }
}

fn gap_intersects(gap: &GapFact, start: u64, end: u64) -> bool {
    if start >= end {
        return false;
    }
    match (gap.affected_start_ns, gap.affected_end_ns) {
        (Some(gap_start), Some(gap_end)) if gap_start < gap_end => {
            gap_start < end && gap_end > start
        }
        _ => start <= gap.timestamp_ns && gap.timestamp_ns < end,
    }
}

fn span_matches(span: &SpanFact, selector: &SpanSelector) -> bool {
    match selector {
        SpanSelector::BuiltIn { kind } => span.span_kind == Some(*kind),
        SpanSelector::Custom { name } => span.custom_name.as_deref() == Some(name),
    }
}

fn span_candidate(span: &SpanFact) -> Candidate {
    let name = span
        .span_kind
        .map(|kind| kind.as_str().to_owned())
        .or_else(|| span.custom_name.clone())
        .expect("span fact has a built-in kind or custom name");
    let value = match (span.end_ns, span.outcome) {
        (None, _) => CandidateValue::OpenSpan,
        (Some(end), Some(SpanOutcome::Completed)) => {
            CandidateValue::Exact(ExactNumeric::integer(end.saturating_sub(span.start_ns)))
        }
        (Some(_), Some(SpanOutcome::Cancelled | SpanOutcome::Failed)) => {
            CandidateValue::Unavailable
        }
        (Some(_), None) => CandidateValue::Missing,
    };
    Candidate {
        sequence: span.sequence,
        timestamp_ns: span.end_ns.unwrap_or(span.start_ns),
        start_ns: span.start_ns,
        end_ns: span.end_ns,
        scope: span.scope.clone(),
        event_kind: if span.custom_name.is_some() {
            DiagnosticEventKind::CustomSpanStarted
        } else {
            DiagnosticEventKind::SpanStarted
        },
        shape: CandidateShape::Span,
        name,
        span_kind: span.span_kind,
        instant_kind: None,
        counter_kind: None,
        custom_name: span.custom_name.clone(),
        outcome: span.outcome,
        severity: None,
        attributes: span.attributes.clone(),
        dimensions: BTreeMap::new(),
        value,
        token_values: std::array::from_fn(|_| None),
        resource_truncated: false,
        series_identity: None,
        metric_unit: None,
    }
}

fn metric_span_candidate(span: &SpanFact) -> Candidate {
    let mut candidate = span_candidate(span);
    candidate.sequence = span.end_sequence.unwrap_or(span.sequence);
    candidate.metric_unit = Some(METRIC_UNIT_NANOSECONDS.to_owned());
    candidate
}

fn table_span_candidate(span: &SpanFact) -> Candidate {
    let mut candidate = span_candidate(span);
    candidate.timestamp_ns = candidate.start_ns;
    candidate
}

fn instant_candidate(event: &DiagnosticEvent, selector: &InstantSelector) -> Option<Candidate> {
    match (event, selector) {
        (DiagnosticEvent::InstantOccurred(value), InstantSelector::BuiltIn { kind })
            if value.instant_kind() == *kind =>
        {
            let mut candidate = empty_candidate(
                value.header().sequence().get(),
                value.header().elapsed_ns().get(),
                value.header().scope().clone(),
                DiagnosticEventKind::InstantOccurred,
                CandidateShape::Instant,
                kind.as_str().to_owned(),
            );
            candidate.instant_kind = Some(*kind);
            Some(candidate)
        }
        (DiagnosticEvent::CustomInstantOccurred(value), InstantSelector::Custom { name })
            if value.name() == name =>
        {
            let mut candidate = empty_candidate(
                value.header().sequence().get(),
                value.header().elapsed_ns().get(),
                value.header().scope().clone(),
                DiagnosticEventKind::CustomInstantOccurred,
                CandidateShape::Instant,
                name.clone(),
            );
            candidate.custom_name = Some(name.clone());
            candidate.severity = value.severity();
            candidate.attributes = convert_attributes(value.attributes());
            Some(candidate)
        }
        _ => None,
    }
}

fn counter_candidate(event: &DiagnosticEvent, selector: &CounterSelector) -> Option<Candidate> {
    match (event, selector) {
        (DiagnosticEvent::CounterSampled(value), CounterSelector::BuiltIn { kind })
            if value.counter_kind() == *kind =>
        {
            let mut candidate = empty_candidate(
                value.header().sequence().get(),
                value.header().elapsed_ns().get(),
                value.header().scope().clone(),
                DiagnosticEventKind::CounterSampled,
                CandidateShape::Counter,
                kind.as_str().to_owned(),
            );
            candidate.counter_kind = Some(*kind);
            candidate.value = CandidateValue::Exact(ExactNumeric::integer(value.value().get()));
            candidate.metric_unit = Some(METRIC_UNIT_COUNT.to_owned());
            candidate.series_identity = Some(series_identity(
                value.header().scope(),
                kind.as_str(),
                None,
                &BTreeMap::new(),
            ));
            Some(candidate)
        }
        (DiagnosticEvent::CustomCounterSampled(value), CounterSelector::Custom { name })
            if value.name() == name =>
        {
            let dimensions = convert_dimensions(value.dimensions());
            let mut candidate = empty_candidate(
                value.header().sequence().get(),
                value.header().elapsed_ns().get(),
                value.header().scope().clone(),
                DiagnosticEventKind::CustomCounterSampled,
                CandidateShape::Counter,
                name.clone(),
            );
            candidate.custom_name = Some(name.clone());
            candidate.value = custom_number(value.value());
            candidate.metric_unit = value.unit().map(str::to_owned);
            candidate.series_identity = Some(series_identity(
                value.header().scope(),
                name,
                value.unit(),
                &dimensions,
            ));
            candidate.dimensions = dimensions;
            Some(candidate)
        }
        _ => None,
    }
}

fn token_candidate(event: &DiagnosticEvent, metric: TokenMetric) -> Option<Candidate> {
    let DiagnosticEvent::ActTokenUsageFinalized(value) = event else {
        return None;
    };
    let mut candidate = empty_candidate(
        value.header().sequence().get(),
        value.header().elapsed_ns().get(),
        value.header().scope().clone(),
        DiagnosticEventKind::ActTokenUsageFinalized,
        CandidateShape::Token,
        "act_token_usage_finalized".to_owned(),
    );
    fill_token_fields(&mut candidate, value);
    candidate.value = token_value(value, metric);
    candidate.metric_unit = Some(METRIC_UNIT_TOKENS.to_owned());
    Some(candidate)
}

fn fill_token_fields(candidate: &mut Candidate, value: &ActTokenUsageFinalized) {
    candidate.token_values = [
        value.provider_total_tokens().cloned(),
        value.input_tokens().cloned(),
        value.output_tokens().cloned(),
        value.thought_tokens().cloned(),
        value.cached_read_tokens().cloned(),
        value.cached_write_tokens().cloned(),
    ];
}

fn token_value(value: &ActTokenUsageFinalized, metric: TokenMetric) -> CandidateValue {
    match token_from_usage(value, metric) {
        Some(tokens) => ExactNumeric::parse_integer(tokens.as_str())
            .map(CandidateValue::Exact)
            .unwrap_or(CandidateValue::NonNumeric),
        None if value.availability() == UsageAvailability::Unavailable => {
            CandidateValue::Unavailable
        }
        None => CandidateValue::Missing,
    }
}

fn token_from_usage(value: &ActTokenUsageFinalized, metric: TokenMetric) -> Option<&TokenCount> {
    match metric {
        TokenMetric::ProviderTotalTokens => value.provider_total_tokens(),
        TokenMetric::InputTokens => value.input_tokens(),
        TokenMetric::OutputTokens => value.output_tokens(),
        TokenMetric::ThoughtTokens => value.thought_tokens(),
        TokenMetric::CachedReadTokens => value.cached_read_tokens(),
        TokenMetric::CachedWriteTokens => value.cached_write_tokens(),
    }
}

const fn token_index(metric: TokenMetric) -> usize {
    match metric {
        TokenMetric::ProviderTotalTokens => 0,
        TokenMetric::InputTokens => 1,
        TokenMetric::OutputTokens => 2,
        TokenMetric::ThoughtTokens => 3,
        TokenMetric::CachedReadTokens => 4,
        TokenMetric::CachedWriteTokens => 5,
    }
}

fn empty_candidate(
    sequence: u64,
    timestamp_ns: u64,
    scope: DiagnosticScope,
    event_kind: DiagnosticEventKind,
    shape: CandidateShape,
    name: String,
) -> Candidate {
    Candidate {
        sequence,
        timestamp_ns,
        start_ns: timestamp_ns,
        end_ns: None,
        scope,
        event_kind,
        shape,
        name,
        span_kind: None,
        instant_kind: None,
        counter_kind: None,
        custom_name: None,
        outcome: None,
        severity: None,
        attributes: BTreeMap::new(),
        dimensions: BTreeMap::new(),
        value: CandidateValue::Missing,
        token_values: std::array::from_fn(|_| None),
        resource_truncated: false,
        series_identity: None,
        metric_unit: None,
    }
}

fn convert_attributes(attributes: &DiagnosticAttributes) -> BTreeMap<String, ScalarField> {
    attributes
        .iter()
        .map(|(key, value)| {
            let value = match value {
                DiagnosticAttributeValue::Null => ScalarField::Scalar(DiagnosticScalar::Null),
                DiagnosticAttributeValue::Boolean(value) => {
                    ScalarField::Scalar(DiagnosticScalar::Boolean(*value))
                }
                DiagnosticAttributeValue::Integer(value) => {
                    ScalarField::Scalar(DiagnosticScalar::Integer(value.clone()))
                }
                DiagnosticAttributeValue::Decimal(value) => {
                    ScalarField::Scalar(DiagnosticScalar::Decimal(value.clone()))
                }
                DiagnosticAttributeValue::String(value) => {
                    ScalarField::Scalar(DiagnosticScalar::String(value.clone()))
                }
                DiagnosticAttributeValue::List(_) => ScalarField::NonScalar,
            };
            (key.clone(), value)
        })
        .collect()
}

fn convert_dimensions(dimensions: &DiagnosticDimensions) -> BTreeMap<String, DiagnosticScalar> {
    dimensions
        .iter()
        .map(|(key, value)| {
            let value = match value {
                DiagnosticDimension::Boolean(value) => DiagnosticScalar::Boolean(*value),
                DiagnosticDimension::Integer(value) => DiagnosticScalar::Integer(value.clone()),
                DiagnosticDimension::Decimal(value) => DiagnosticScalar::Decimal(value.clone()),
                DiagnosticDimension::String(value) => DiagnosticScalar::String(value.clone()),
            };
            (key.clone(), value)
        })
        .collect()
}

fn custom_number(value: &CustomNumber) -> CandidateValue {
    match value {
        CustomNumber::Integer(value) => ExactNumeric::parse_integer(value.as_str()),
        CustomNumber::Decimal(value) => ExactNumeric::parse_decimal(value.as_str()),
    }
    .map(CandidateValue::Exact)
    .unwrap_or(CandidateValue::NonNumeric)
}

fn series_identity(
    scope: &DiagnosticScope,
    name: &str,
    unit: Option<&str>,
    dimensions: &BTreeMap<String, DiagnosticScalar>,
) -> String {
    serde_json::to_string(&(scope, name, unit, dimensions))
        .expect("canonical query series identity is serializable")
}

pub(crate) fn scope_contains(parent: &DiagnosticScope, child: &DiagnosticScope) -> bool {
    parent
        .scene_id()
        .is_none_or(|value| child.scene_id() == Some(value))
        && parent
            .actor_id()
            .is_none_or(|value| child.actor_id() == Some(value))
        && parent
            .cue_id()
            .is_none_or(|value| child.cue_id() == Some(value))
        && parent
            .effect_id()
            .is_none_or(|value| child.effect_id() == Some(value))
        && parent
            .act_id()
            .is_none_or(|value| child.act_id() == Some(value))
        && parent
            .tool_call_id()
            .is_none_or(|value| child.tool_call_id() == Some(value))
        && parent
            .session_generation()
            .is_none_or(|value| child.session_generation() == Some(value))
}

fn scopes_overlap(left: &DiagnosticScope, right: &DiagnosticScope) -> bool {
    compatible_id(left.scene_id(), right.scene_id())
        && compatible_id(left.actor_id(), right.actor_id())
        && compatible_id(left.cue_id(), right.cue_id())
        && compatible_id(left.effect_id(), right.effect_id())
        && compatible_id(left.act_id(), right.act_id())
        && compatible_id(left.tool_call_id(), right.tool_call_id())
        && match (left.session_generation(), right.session_generation()) {
            (Some(left), Some(right)) => left == right,
            _ => true,
        }
}

fn compatible_id<T: PartialEq>(left: Option<&T>, right: Option<&T>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left == right,
        _ => true,
    }
}

pub(crate) fn relevant_kinds_for_timeline(source: &TimelineSource) -> Vec<DiagnosticEventKind> {
    match source {
        TimelineSource::Span {
            selector: SpanSelector::BuiltIn { .. },
        } => vec![
            DiagnosticEventKind::SpanStarted,
            DiagnosticEventKind::SpanFinished,
        ],
        TimelineSource::Span {
            selector: SpanSelector::Custom { .. },
        } => vec![
            DiagnosticEventKind::CustomSpanStarted,
            DiagnosticEventKind::CustomSpanFinished,
        ],
        TimelineSource::Instant {
            selector: InstantSelector::BuiltIn { .. },
        } => vec![DiagnosticEventKind::InstantOccurred],
        TimelineSource::Instant {
            selector: InstantSelector::Custom { .. },
        } => vec![DiagnosticEventKind::CustomInstantOccurred],
    }
}

pub(crate) fn relevant_kinds_for_metric(source: &MetricSource) -> Vec<DiagnosticEventKind> {
    match source {
        MetricSource::CounterValue {
            selector: CounterSelector::BuiltIn { .. },
            ..
        } => vec![DiagnosticEventKind::CounterSampled],
        MetricSource::CounterValue {
            selector: CounterSelector::Custom { .. },
            ..
        } => vec![DiagnosticEventKind::CustomCounterSampled],
        MetricSource::InstantCount {
            selector: InstantSelector::BuiltIn { .. },
        } => vec![DiagnosticEventKind::InstantOccurred],
        MetricSource::InstantCount {
            selector: InstantSelector::Custom { .. },
        } => vec![DiagnosticEventKind::CustomInstantOccurred],
        MetricSource::CompletedSpanDuration {
            selector: SpanSelector::BuiltIn { .. },
        } => vec![
            DiagnosticEventKind::SpanStarted,
            DiagnosticEventKind::SpanFinished,
        ],
        MetricSource::CompletedSpanDuration {
            selector: SpanSelector::Custom { .. },
        } => vec![
            DiagnosticEventKind::CustomSpanStarted,
            DiagnosticEventKind::CustomSpanFinished,
        ],
        MetricSource::ActToken { .. } => vec![DiagnosticEventKind::ActTokenUsageFinalized],
    }
}

pub(crate) fn relevant_kinds_for_table(source: &TableSource) -> Vec<DiagnosticEventKind> {
    match source {
        TableSource::Event { kind } => vec![*kind],
        TableSource::Span { selector } => relevant_kinds_for_timeline(&TimelineSource::Span {
            selector: selector.clone(),
        }),
        TableSource::Instant { selector } => {
            relevant_kinds_for_timeline(&TimelineSource::Instant {
                selector: selector.clone(),
            })
        }
        TableSource::Counter {
            selector: CounterSelector::BuiltIn { .. },
        } => vec![DiagnosticEventKind::CounterSampled],
        TableSource::Counter {
            selector: CounterSelector::Custom { .. },
        } => vec![DiagnosticEventKind::CustomCounterSampled],
        TableSource::ActTokenUsage => vec![DiagnosticEventKind::ActTokenUsageFinalized],
    }
}
