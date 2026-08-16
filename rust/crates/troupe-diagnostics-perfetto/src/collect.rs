use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use troupe_diagnostics_core::{
    detail::{DiagnosticDimension, DiagnosticDimensions},
    event::{ActTokenUsageFinalized, DiagnosticEvent},
    id::CanonicalUuid,
    scalar::{SchemaU64, TokenCount},
    validate::{ReferenceValidationError, ReferenceValidator},
};

use crate::{
    identity::{DenseIdentityMap, IdentitySpace, component},
    tracks::{
        ROOT_TRACK_IDENTITY, SpanInterval, TrackCatalog, TrackCatalogBuilder, allocate_span_lanes,
        scope_track_identity,
    },
};

pub const PERFETTO_EXPORTER_SCHEMA_VERSION: u8 = 1;
pub const TRACE_CONTENT_WARNING: &str =
    "trace may contain sensitive diagnostic metadata and user-provided attributes";
pub const STRUCTURAL_INDEX_ENTRY_LIMIT: u64 = 1_000_000;
pub const STRUCTURAL_INDEX_OWNED_PAYLOAD_LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StructuralIndexDimension {
    Entries,
    OwnedPayloadBytes,
}

impl StructuralIndexDimension {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Entries => "entries",
            Self::OwnedPayloadBytes => "owned_payload_bytes",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StructuralIndexLimits {
    entries: u64,
    owned_payload_bytes: u64,
}

impl StructuralIndexLimits {
    pub(crate) const FIXED: Self = Self {
        entries: STRUCTURAL_INDEX_ENTRY_LIMIT,
        owned_payload_bytes: STRUCTURAL_INDEX_OWNED_PAYLOAD_LIMIT,
    };

    #[cfg(test)]
    pub(crate) const fn new(entries: u64, owned_payload_bytes: u64) -> Self {
        Self {
            entries,
            owned_payload_bytes,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct StructuralIndexUsage {
    entries: u64,
    owned_payload_bytes: u64,
}

impl StructuralIndexUsage {
    pub(crate) const fn entries(self) -> u64 {
        self.entries
    }

    pub(crate) const fn owned_payload_bytes(self) -> u64 {
        self.owned_payload_bytes
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct StructuralIndexBudget {
    limits: StructuralIndexLimits,
    usage: StructuralIndexUsage,
}

impl StructuralIndexBudget {
    const fn new(limits: StructuralIndexLimits) -> Self {
        Self {
            limits,
            usage: StructuralIndexUsage {
                entries: 0,
                owned_payload_bytes: 0,
            },
        }
    }

    pub(crate) fn reserve(
        &mut self,
        entries: u64,
        owned_payload_bytes: u64,
    ) -> Result<(), ProjectionError> {
        let required_entries = match self.usage.entries.checked_add(entries) {
            Some(required) => required,
            None => {
                return Err(self.limit_exceeded(
                    StructuralIndexDimension::Entries,
                    self.limits.entries,
                    u64::MAX,
                ));
            }
        };
        if required_entries > self.limits.entries {
            return Err(self.limit_exceeded(
                StructuralIndexDimension::Entries,
                self.limits.entries,
                required_entries,
            ));
        }

        let required_payload = match self
            .usage
            .owned_payload_bytes
            .checked_add(owned_payload_bytes)
        {
            Some(required) => required,
            None => {
                return Err(self.limit_exceeded(
                    StructuralIndexDimension::OwnedPayloadBytes,
                    self.limits.owned_payload_bytes,
                    u64::MAX,
                ));
            }
        };
        if required_payload > self.limits.owned_payload_bytes {
            return Err(self.limit_exceeded(
                StructuralIndexDimension::OwnedPayloadBytes,
                self.limits.owned_payload_bytes,
                required_payload,
            ));
        }

        self.usage.entries = required_entries;
        self.usage.owned_payload_bytes = required_payload;
        Ok(())
    }

    pub(crate) fn reserve_owned<'value>(
        &mut self,
        entries: u64,
        values: impl IntoIterator<Item = &'value str>,
    ) -> Result<(), ProjectionError> {
        let mut owned_payload_bytes = 0_u64;
        for value in values {
            let value_bytes = match u64::try_from(value.len()) {
                Ok(value_bytes) => value_bytes,
                Err(_) => {
                    return Err(self.limit_exceeded(
                        StructuralIndexDimension::OwnedPayloadBytes,
                        self.limits.owned_payload_bytes,
                        u64::MAX,
                    ));
                }
            };
            owned_payload_bytes = match owned_payload_bytes.checked_add(value_bytes) {
                Some(required) => required,
                None => {
                    return Err(self.limit_exceeded(
                        StructuralIndexDimension::OwnedPayloadBytes,
                        self.limits.owned_payload_bytes,
                        u64::MAX,
                    ));
                }
            };
        }
        self.reserve(entries, owned_payload_bytes)
    }

    const fn limit_exceeded(
        self,
        dimension: StructuralIndexDimension,
        limit: u64,
        required: u64,
    ) -> ProjectionError {
        ProjectionError::StructuralIndexLimitExceeded {
            dimension,
            limit,
            required,
        }
    }

    const fn usage(self) -> StructuralIndexUsage {
        self.usage
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionMetadata {
    run_id: CanonicalUuid,
    captured_watermark: SchemaU64,
    exported_through: SchemaU64,
    troupe_version: String,
    production_outcome: Option<String>,
    clean_shutdown: Option<bool>,
}

impl ProjectionMetadata {
    pub fn new(
        run_id: CanonicalUuid,
        captured_watermark: SchemaU64,
        exported_through: SchemaU64,
        troupe_version: impl Into<String>,
    ) -> Self {
        Self {
            run_id,
            captured_watermark,
            exported_through,
            troupe_version: troupe_version.into(),
            production_outcome: None,
            clean_shutdown: None,
        }
    }

    pub fn with_completion(
        mut self,
        production_outcome: Option<String>,
        clean_shutdown: Option<bool>,
    ) -> Self {
        self.production_outcome = production_outcome;
        self.clean_shutdown = clean_shutdown;
        self
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub const fn captured_watermark(&self) -> SchemaU64 {
        self.captured_watermark
    }

    pub const fn exported_through(&self) -> SchemaU64 {
        self.exported_through
    }

    fn root_track_name(&self) -> String {
        format!("Troupe Production {}", self.run_id)
    }

    fn metadata_track_name(&self) -> String {
        let outcome = self.production_outcome.as_deref().unwrap_or("unavailable");
        let clean_shutdown = self
            .clean_shutdown
            .map(|value| if value { "true" } else { "false" })
            .unwrap_or("unavailable");
        format!(
            "Troupe metadata | exporter_schema={} | event_schema={} | run_id={} | captured_watermark={} | exported_through={} | troupe_version={} | outcome={} | clean_shutdown={} | content_warning={}",
            PERFETTO_EXPORTER_SCHEMA_VERSION,
            troupe_diagnostics_core::event::EVENT_SCHEMA_VERSION,
            self.run_id,
            self.captured_watermark.get(),
            self.exported_through.get(),
            self.troupe_version,
            outcome,
            clean_shutdown,
            TRACE_CONTENT_WARNING,
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectionLimits {
    pub max_track_ids: u64,
    pub max_flow_ids: u64,
}

impl ProjectionLimits {
    pub const fn new(max_track_ids: u64, max_flow_ids: u64) -> Self {
        Self {
            max_track_ids,
            max_flow_ids,
        }
    }
}

impl Default for ProjectionLimits {
    fn default() -> Self {
        Self::new(u64::MAX, u64::MAX)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectionError {
    InvalidReference {
        code: &'static str,
        sequence: u64,
        referenced_sequence: Option<u64>,
    },
    SequenceMismatch {
        expected: u64,
        actual: u64,
    },
    RunMismatch,
    WatermarkMismatch {
        captured: u64,
        exported: u64,
        observed: u64,
    },
    TimestampOutOfRange {
        sequence: u64,
        elapsed_ns: u64,
    },
    IdentityExhausted {
        space: &'static str,
        required: u64,
        maximum: u64,
    },
    StructuralIndexLimitExceeded {
        dimension: StructuralIndexDimension,
        limit: u64,
        required: u64,
    },
    UnknownIdentity(String),
    ConflictingTrack(String),
    MissingSpan(u64),
    DuplicateActUsage(String),
    ProjectionIncomplete {
        expected: u64,
        projected: u64,
    },
    ProtobufEncode(String),
}

impl ProjectionError {
    fn invalid_reference(error: ReferenceValidationError) -> Self {
        Self::InvalidReference {
            code: error.code().as_str(),
            sequence: error.event_sequence().get(),
            referenced_sequence: error.referenced_sequence().map(SchemaU64::get),
        }
    }

    pub(crate) const fn identity_exhausted(
        space: crate::identity::IdentitySpace,
        required: u64,
        maximum: u64,
    ) -> Self {
        Self::IdentityExhausted {
            space: space.as_str(),
            required,
            maximum,
        }
    }

    pub(crate) fn unknown_identity(identity: &str) -> Self {
        Self::UnknownIdentity(identity.to_owned())
    }

    pub(crate) fn conflicting_track(identity: &str) -> Self {
        Self::ConflictingTrack(identity.to_owned())
    }
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidReference {
                code,
                sequence,
                referenced_sequence,
            } => {
                write!(
                    formatter,
                    "invalid diagnostic reference {code} at event {sequence}"
                )?;
                if let Some(reference) = referenced_sequence {
                    write!(formatter, " referencing {reference}")?;
                }
                Ok(())
            }
            Self::SequenceMismatch { expected, actual } => write!(
                formatter,
                "captured diagnostic sequence is not dense: expected {expected}, got {actual}"
            ),
            Self::RunMismatch => formatter.write_str("captured diagnostic Run identity changed"),
            Self::WatermarkMismatch {
                captured,
                exported,
                observed,
            } => write!(
                formatter,
                "projection watermark mismatch: captured {captured}, exported {exported}, observed {observed}"
            ),
            Self::TimestampOutOfRange {
                sequence,
                elapsed_ns,
            } => write!(
                formatter,
                "event {sequence} elapsed_ns {elapsed_ns} exceeds signed 64-bit Perfetto time"
            ),
            Self::IdentityExhausted {
                space,
                required,
                maximum,
            } => write!(
                formatter,
                "Perfetto {space} identity space exhausted: need {required}, maximum {maximum}"
            ),
            Self::StructuralIndexLimitExceeded {
                dimension,
                limit,
                required,
            } => write!(
                formatter,
                "Perfetto structural index {} limit exceeded: limit {limit}, required {required}",
                dimension.as_str()
            ),
            Self::UnknownIdentity(identity) => {
                write!(
                    formatter,
                    "unknown canonical projection identity {identity}"
                )
            }
            Self::ConflictingTrack(identity) => {
                write!(formatter, "conflicting canonical track identity {identity}")
            }
            Self::MissingSpan(sequence) => {
                write!(formatter, "projection span {sequence} was not collected")
            }
            Self::DuplicateActUsage(identity) => {
                write!(formatter, "duplicate terminal Act usage for {identity}")
            }
            Self::ProjectionIncomplete {
                expected,
                projected,
            } => write!(
                formatter,
                "event projection incomplete: expected {expected}, projected {projected}"
            ),
            Self::ProtobufEncode(detail) => {
                write!(formatter, "Perfetto protobuf encode failed: {detail}")
            }
        }
    }
}

impl std::error::Error for ProjectionError {}

#[derive(Clone, Debug)]
struct CollectedSpan {
    start_sequence: SchemaU64,
    finish_sequence: Option<SchemaU64>,
    parent_track_identity: String,
    role: String,
    display_name: String,
}

#[derive(Clone, Debug)]
struct CollectedFlow {
    canonical_identity: String,
    source_sequence: SchemaU64,
    target_sequence: SchemaU64,
}

#[derive(Clone, Debug)]
pub(crate) struct FlowAttachment {
    pub(crate) id: u64,
    pub(crate) canonical_identity: String,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum UsageField {
    ProviderTotal,
    Input,
    Output,
    Thought,
    CachedRead,
    CachedWrite,
}

impl UsageField {
    pub(crate) const ALL: [Self; 6] = [
        Self::ProviderTotal,
        Self::Input,
        Self::Output,
        Self::Thought,
        Self::CachedRead,
        Self::CachedWrite,
    ];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderTotal => "provider_total_tokens",
            Self::Input => "input_tokens",
            Self::Output => "output_tokens",
            Self::Thought => "thought_tokens",
            Self::CachedRead => "cached_read_tokens",
            Self::CachedWrite => "cached_write_tokens",
        }
    }

    fn value(self, usage: &ActTokenUsageFinalized) -> Option<&TokenCount> {
        match self {
            Self::ProviderTotal => usage.provider_total_tokens(),
            Self::Input => usage.input_tokens(),
            Self::Output => usage.output_tokens(),
            Self::Thought => usage.thought_tokens(),
            Self::CachedRead => usage.cached_read_tokens(),
            Self::CachedWrite => usage.cached_write_tokens(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct UsageSummary {
    pub(crate) availability: &'static str,
    pub(crate) source: Option<&'static str>,
    pub(crate) unavailable_reason: Option<&'static str>,
    pub(crate) values: BTreeMap<UsageField, String>,
}

impl UsageSummary {
    fn from_event(usage: &ActTokenUsageFinalized) -> Self {
        let values = UsageField::ALL
            .into_iter()
            .filter_map(|field| {
                field
                    .value(usage)
                    .map(|value| (field, value.as_str().to_owned()))
            })
            .collect();
        Self {
            availability: usage.availability().as_str(),
            source: usage.source().map(|value| value.as_str()),
            unavailable_reason: usage.unavailable_reason().map(|value| value.as_str()),
            values,
        }
    }
}

pub(crate) struct ProjectionCollector {
    metadata: ProjectionMetadata,
    limits: ProjectionLimits,
    structural_budget: StructuralIndexBudget,
    validator: ReferenceValidator,
    next_sequence: u64,
    tracks: TrackCatalogBuilder,
    spans: BTreeMap<SchemaU64, CollectedSpan>,
    flows: BTreeMap<String, CollectedFlow>,
    act_usage: BTreeMap<String, UsageSummary>,
}

impl ProjectionCollector {
    pub(crate) fn new(
        metadata: ProjectionMetadata,
        limits: ProjectionLimits,
    ) -> Result<Self, ProjectionError> {
        Self::new_with_structural_limits(metadata, limits, StructuralIndexLimits::FIXED)
    }

    pub(crate) fn new_with_structural_limits(
        metadata: ProjectionMetadata,
        limits: ProjectionLimits,
        structural_limits: StructuralIndexLimits,
    ) -> Result<Self, ProjectionError> {
        if metadata.exported_through().get() > metadata.captured_watermark().get() {
            return Err(ProjectionError::WatermarkMismatch {
                captured: metadata.captured_watermark().get(),
                exported: metadata.exported_through().get(),
                observed: 0,
            });
        }
        let mut structural_budget = StructuralIndexBudget::new(structural_limits);
        let mut tracks =
            TrackCatalogBuilder::new(metadata.root_track_name(), &mut structural_budget)?;
        tracks.register_metadata(metadata.metadata_track_name(), &mut structural_budget)?;
        Ok(Self {
            metadata,
            limits,
            structural_budget,
            validator: ReferenceValidator::new(),
            next_sequence: 1,
            tracks,
            spans: BTreeMap::new(),
            flows: BTreeMap::new(),
            act_usage: BTreeMap::new(),
        })
    }

    pub(crate) fn observe(&mut self, event: &DiagnosticEvent) -> Result<(), ProjectionError> {
        let header = event.header();
        if header.run_id() != self.metadata.run_id() {
            return Err(ProjectionError::RunMismatch);
        }
        if header.sequence().get() != self.next_sequence {
            return Err(ProjectionError::SequenceMismatch {
                expected: self.next_sequence,
                actual: header.sequence().get(),
            });
        }
        if header.elapsed_ns().get() > i64::MAX as u64 {
            return Err(ProjectionError::TimestampOutOfRange {
                sequence: header.sequence().get(),
                elapsed_ns: header.elapsed_ns().get(),
            });
        }

        let validator_retains_span = matches!(
            event,
            DiagnosticEvent::SpanStarted(_) | DiagnosticEvent::CustomSpanStarted(_)
        );
        if validator_retains_span {
            self.structural_budget.reserve_owned(
                2,
                [
                    header.scope().scene_id(),
                    header.scope().actor_id(),
                    header.scope().cue_id(),
                    header.scope().effect_id(),
                    header.scope().act_id(),
                    header.scope().tool_call_id(),
                ]
                .into_iter()
                .flatten()
                .map(|value| value.as_str()),
            )?;
        } else {
            self.structural_budget.reserve(1, 0)?;
        }
        self.validator
            .validate(event)
            .map_err(ProjectionError::invalid_reference)?;

        let parent_track = self
            .tracks
            .register_scope(header.scope(), &mut self.structural_budget)?;
        self.collect_event_tracks(event, &parent_track)?;
        self.collect_flows(event)?;
        self.next_sequence =
            self.next_sequence
                .checked_add(1)
                .ok_or(ProjectionError::SequenceMismatch {
                    expected: u64::MAX,
                    actual: u64::MAX,
                })?;
        Ok(())
    }

    fn collect_event_tracks(
        &mut self,
        event: &DiagnosticEvent,
        parent_track: &str,
    ) -> Result<(), ProjectionError> {
        match event {
            DiagnosticEvent::SpanStarted(start) => {
                let role = start.span_kind().as_str();
                self.structural_budget
                    .reserve_owned(1, [parent_track, role, role])?;
                let previous = self.spans.insert(
                    start.header().sequence(),
                    CollectedSpan {
                        start_sequence: start.header().sequence(),
                        finish_sequence: None,
                        parent_track_identity: parent_track.to_owned(),
                        role: role.to_owned(),
                        display_name: role.to_owned(),
                    },
                );
                debug_assert!(previous.is_none(), "span start sequences are unique");
            }
            DiagnosticEvent::CustomSpanStarted(start) => {
                let role = format!("custom:{}", start.name());
                self.structural_budget
                    .reserve_owned(1, [parent_track, role.as_str(), start.name()])?;
                let previous = self.spans.insert(
                    start.header().sequence(),
                    CollectedSpan {
                        start_sequence: start.header().sequence(),
                        finish_sequence: None,
                        parent_track_identity: parent_track.to_owned(),
                        role: role.to_owned(),
                        display_name: start.name().to_owned(),
                    },
                );
                debug_assert!(previous.is_none(), "span start sequences are unique");
            }
            DiagnosticEvent::SpanFinished(finish) => {
                self.spans
                    .get_mut(&finish.span_id())
                    .ok_or(ProjectionError::MissingSpan(finish.span_id().get()))?
                    .finish_sequence = Some(finish.header().sequence());
            }
            DiagnosticEvent::CustomSpanFinished(finish) => {
                self.spans
                    .get_mut(&finish.span_id())
                    .ok_or(ProjectionError::MissingSpan(finish.span_id().get()))?
                    .finish_sequence = Some(finish.header().sequence());
            }
            DiagnosticEvent::CounterSampled(counter) => {
                let series = builtin_counter_series(counter.counter_kind().as_str());
                self.tracks.register_counter(
                    parent_track,
                    &series,
                    counter.counter_kind().as_str(),
                    &mut self.structural_budget,
                )?;
            }
            DiagnosticEvent::CustomCounterSampled(counter) => {
                let series =
                    custom_counter_series(counter.name(), counter.unit(), counter.dimensions());
                self.tracks.register_counter(
                    parent_track,
                    &series,
                    counter.name(),
                    &mut self.structural_budget,
                )?;
            }
            DiagnosticEvent::ContextUsageSampled(usage) => {
                if usage.context_used_tokens().is_some() {
                    self.tracks.register_counter(
                        parent_track,
                        context_counter_series("used_tokens"),
                        "context used tokens",
                        &mut self.structural_budget,
                    )?;
                }
                if usage.context_window_tokens().is_some() {
                    self.tracks.register_counter(
                        parent_track,
                        context_counter_series("window_tokens"),
                        "context window tokens",
                        &mut self.structural_budget,
                    )?;
                }
                if let Some(currency) = usage.cumulative_cost_currency() {
                    let series = context_cost_series(currency.as_str());
                    self.tracks.register_counter(
                        parent_track,
                        &series,
                        &format!("cumulative cost {}", currency.as_str()),
                        &mut self.structural_budget,
                    )?;
                }
            }
            DiagnosticEvent::ActTokenUsageFinalized(usage) => {
                let act_identity = scope_track_identity(usage.header().scope());
                if self.act_usage.contains_key(&act_identity) {
                    return Err(ProjectionError::DuplicateActUsage(act_identity));
                }
                self.structural_budget.reserve_owned(
                    1,
                    std::iter::once(act_identity.as_str()).chain(
                        UsageField::ALL
                            .into_iter()
                            .filter_map(|field| field.value(usage).map(TokenCount::as_str)),
                    ),
                )?;
                self.act_usage
                    .insert(act_identity.to_owned(), UsageSummary::from_event(usage));
                for field in UsageField::ALL {
                    if field.value(usage).is_some() {
                        self.tracks.register_counter(
                            ROOT_TRACK_IDENTITY,
                            usage_counter_series(field.as_str()),
                            &format!("known {}", field.as_str()),
                            &mut self.structural_budget,
                        )?;
                    }
                }
                for coverage in USAGE_COVERAGE_COUNTERS {
                    self.tracks.register_counter(
                        ROOT_TRACK_IDENTITY,
                        usage_coverage_series(coverage),
                        &format!("Act usage {coverage}"),
                        &mut self.structural_budget,
                    )?;
                }
            }
            DiagnosticEvent::InstantOccurred(_)
            | DiagnosticEvent::AgentMessageDelta(_)
            | DiagnosticEvent::AgentMessageCompleted(_)
            | DiagnosticEvent::AgentPlanSnapshot(_)
            | DiagnosticEvent::ObservationGap(_)
            | DiagnosticEvent::CustomInstantOccurred(_) => {}
        }
        Ok(())
    }

    fn collect_flows(&mut self, event: &DiagnosticEvent) -> Result<(), ProjectionError> {
        let target = event.header().sequence();
        for link in event.header().caused_by() {
            let canonical_identity = format!(
                "{}:{}:{}",
                link.relation().as_str(),
                link.source_sequence().get(),
                target.get()
            );
            if self.flows.contains_key(&canonical_identity) {
                continue;
            }
            self.structural_budget.reserve_owned(
                1,
                [canonical_identity.as_str(), canonical_identity.as_str()],
            )?;
            self.flows.insert(
                canonical_identity.to_owned(),
                CollectedFlow {
                    canonical_identity: canonical_identity.to_owned(),
                    source_sequence: link.source_sequence(),
                    target_sequence: target,
                },
            );
        }
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<ProjectionPlan, ProjectionError> {
        let observed = self.next_sequence.saturating_sub(1);
        if observed != self.metadata.exported_through().get() {
            return Err(ProjectionError::WatermarkMismatch {
                captured: self.metadata.captured_watermark().get(),
                exported: self.metadata.exported_through().get(),
                observed,
            });
        }

        let intervals = std::mem::take(&mut self.spans)
            .into_values()
            .map(|span| SpanInterval {
                start_sequence: span.start_sequence,
                finish_sequence: span.finish_sequence,
                parent_track_identity: span.parent_track_identity,
                role: span.role,
                display_name: span.display_name,
            });
        let span_tracks =
            allocate_span_lanes(&mut self.tracks, intervals, &mut self.structural_budget)?;
        let tracks = self
            .tracks
            .finish(self.limits.max_track_ids, &mut self.structural_budget)?;

        for identity in self.flows.keys() {
            self.structural_budget
                .reserve_owned(1, [identity.as_str()])?;
        }
        let flow_identities = self.flows.keys().cloned().collect::<BTreeSet<String>>();
        let flow_ids = DenseIdentityMap::assign(
            flow_identities,
            IdentitySpace::Flow,
            self.limits.max_flow_ids,
        )?;
        let mut starting_flows = BTreeMap::<SchemaU64, Vec<FlowAttachment>>::new();
        let mut terminating_flows = BTreeMap::<SchemaU64, Vec<FlowAttachment>>::new();
        for flow in self.flows.values() {
            self.structural_budget
                .reserve_owned(1, [flow.canonical_identity.as_str()])?;
            self.structural_budget
                .reserve_owned(1, [flow.canonical_identity.as_str()])?;
            let attachment = FlowAttachment {
                id: flow_ids.id(&flow.canonical_identity)?,
                canonical_identity: flow.canonical_identity.clone(),
            };
            starting_flows
                .entry(flow.source_sequence)
                .or_default()
                .push(attachment.clone());
            terminating_flows
                .entry(flow.target_sequence)
                .or_default()
                .push(attachment);
        }
        let structural_index_usage = self.structural_budget.usage();

        Ok(ProjectionPlan {
            metadata: self.metadata,
            tracks,
            span_tracks,
            starting_flows,
            terminating_flows,
            act_usage: self.act_usage,
            event_count: observed,
            structural_index_usage,
        })
    }
}

pub(crate) struct ProjectionPlan {
    pub(crate) metadata: ProjectionMetadata,
    pub(crate) tracks: TrackCatalog,
    pub(crate) span_tracks: BTreeMap<SchemaU64, String>,
    pub(crate) starting_flows: BTreeMap<SchemaU64, Vec<FlowAttachment>>,
    pub(crate) terminating_flows: BTreeMap<SchemaU64, Vec<FlowAttachment>>,
    pub(crate) act_usage: BTreeMap<String, UsageSummary>,
    pub(crate) event_count: u64,
    structural_index_usage: StructuralIndexUsage,
}

impl ProjectionPlan {
    pub(crate) const fn structural_index_usage(&self) -> StructuralIndexUsage {
        self.structural_index_usage
    }
}

pub(crate) const USAGE_COVERAGE_COUNTERS: [&str; 5] = [
    "finalized",
    "reported",
    "available",
    "partial",
    "unavailable",
];

pub(crate) fn builtin_counter_series(kind: &str) -> String {
    format!("builtin:{}", component(kind))
}

pub(crate) fn custom_counter_series(
    name: &str,
    unit: Option<&str>,
    dimensions: &DiagnosticDimensions,
) -> String {
    let unit = unit.map_or_else(|| "none".to_owned(), component);
    let dimensions = dimensions
        .iter()
        .map(|(key, value)| format!("{}={}", component(key), component(&dimension_value(value))))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "custom:{}|unit:{unit}|dimensions:{}",
        component(name),
        component(&dimensions)
    )
}

fn dimension_value(value: &DiagnosticDimension) -> String {
    match value {
        DiagnosticDimension::Boolean(value) => format!("bool:{value}"),
        DiagnosticDimension::Integer(value) => format!("integer:{}", value.as_str()),
        DiagnosticDimension::Decimal(value) => format!("decimal:{}", value.as_str()),
        DiagnosticDimension::String(value) => format!("string:{}", component(value)),
    }
}

pub(crate) fn context_counter_series(field: &str) -> &str {
    match field {
        "used_tokens" => "context:used_tokens",
        "window_tokens" => "context:window_tokens",
        _ => "context:unknown",
    }
}

pub(crate) fn context_cost_series(currency: &str) -> String {
    format!("context:cumulative_cost:{}", component(currency))
}

pub(crate) fn usage_counter_series(field: &str) -> &str {
    match field {
        "provider_total_tokens" => "usage:known:provider_total_tokens",
        "input_tokens" => "usage:known:input_tokens",
        "output_tokens" => "usage:known:output_tokens",
        "thought_tokens" => "usage:known:thought_tokens",
        "cached_read_tokens" => "usage:known:cached_read_tokens",
        "cached_write_tokens" => "usage:known:cached_write_tokens",
        _ => "usage:known:unknown",
    }
}

pub(crate) fn usage_coverage_series(field: &str) -> &str {
    match field {
        "finalized" => "usage:coverage:finalized",
        "reported" => "usage:coverage:reported",
        "available" => "usage:coverage:available",
        "partial" => "usage:coverage:partial",
        "unavailable" => "usage:coverage:unavailable",
        _ => "usage:coverage:unknown",
    }
}
