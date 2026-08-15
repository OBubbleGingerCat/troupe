use std::{collections::BTreeMap, fmt::Write as _};

use troupe_diagnostics_core::{
    detail::{
        CustomNumber, DiagnosticAttributeValue, DiagnosticDimension, DiagnosticScalar,
        InstantDetail, SpanStartDetail,
    },
    event::{DiagnosticEvent, DiagnosticScope},
    kinds::{SpanKind, UsageAvailability},
    scalar::{SchemaU64, TokenCount},
};
use troupe_diagnostics_runtime::query::reader::CapturedEvent;

use crate::{
    collect::{
        ProjectionCollector, ProjectionError, ProjectionLimits, ProjectionMetadata,
        ProjectionPlan, USAGE_COVERAGE_COUNTERS, UsageField, UsageSummary,
        builtin_counter_series, context_cost_series, context_counter_series,
        custom_counter_series, usage_counter_series, usage_coverage_series,
    },
    identity::{ExactCounterValue, exact_counter_value},
    schema::{
        BuiltinClock, DebugAnnotation, TracePacket, TrackDescriptor, TrackEvent, TrackEventType,
        debug_annotation, encode_trace_packet_fragment, trace_packet, track_descriptor,
        track_event,
    },
    tracks::{
        ROOT_TRACK_IDENTITY, counter_track_identity, scope_track_identity,
    },
};

pub fn project_prefix(
    metadata: ProjectionMetadata,
    events: &[DiagnosticEvent],
) -> Result<ProjectedTrace, ProjectionError> {
    project_prefix_with_limits(metadata, events, ProjectionLimits::default())
}

pub fn project_prefix_with_limits(
    metadata: ProjectionMetadata,
    events: &[DiagnosticEvent],
    limits: ProjectionLimits,
) -> Result<ProjectedTrace, ProjectionError> {
    let mut collector = ProjectionCollector::new(metadata, limits)?;
    for event in events {
        collector.observe(event)?;
    }
    let plan = collector.finish()?;
    ProjectedTrace::from_plan(&plan, events)
}

#[derive(Debug)]
pub enum CapturedProjectionError<E> {
    Source(E),
    Projection(ProjectionError),
}

pub fn project_captured_prefix<I, E>(
    metadata: ProjectionMetadata,
    events: I,
) -> Result<ProjectedTrace, CapturedProjectionError<E>>
where
    I: IntoIterator<Item = Result<CapturedEvent, E>>,
{
    let mut canonical_events = Vec::new();
    for event in events {
        let (event, _) = event.map_err(CapturedProjectionError::Source)?.into_parts();
        canonical_events.push(event);
    }
    project_prefix(metadata, &canonical_events).map_err(CapturedProjectionError::Projection)
}

pub struct ProjectedTrace {
    packets: Vec<TracePacket>,
    descriptor_count: usize,
}

impl ProjectedTrace {
    fn from_plan(
        plan: &ProjectionPlan,
        events: &[DiagnosticEvent],
    ) -> Result<Self, ProjectionError> {
        let mut packets = plan.descriptor_packets().collect::<Vec<_>>();
        let descriptor_count = packets.len();
        let mut projector = plan.packet_projector();
        for event in events {
            packets.extend(projector.project_event(event)?);
        }
        projector.finish()?;
        Ok(Self {
            packets,
            descriptor_count,
        })
    }

    pub const fn descriptor_count(&self) -> usize {
        self.descriptor_count
    }

    pub fn event_packet_count(&self) -> usize {
        self.packets.len() - self.descriptor_count
    }

    pub fn trace_bytes(&self) -> Result<Vec<u8>, ProjectionError> {
        let mut output = Vec::new();
        for packet in &self.packets {
            encode_trace_packet_fragment(packet, &mut output)
                .map_err(|error| ProjectionError::ProtobufEncode(error.to_string()))?;
        }
        Ok(output)
    }

    pub fn debug_packets_json(&self) -> String {
        packets_json(&self.packets)
    }
}

impl ProjectionPlan {
    pub(crate) fn descriptor_packets(&self) -> impl Iterator<Item = TracePacket> + '_ {
        self.tracks.descriptors().map(|descriptor| TracePacket {
            timestamp: None,
            data: Some(trace_packet::Data::TrackDescriptor(TrackDescriptor {
                uuid: Some(descriptor.uuid),
                static_or_dynamic_name: Some(track_descriptor::StaticOrDynamicName::Name(
                    descriptor.name.to_owned(),
                )),
                parent_uuid: descriptor.parent_uuid,
            })),
            timestamp_clock_id: None,
        })
    }

    pub(crate) fn packet_projector(&self) -> PacketProjector<'_> {
        PacketProjector {
            plan: self,
            next_sequence: 1,
            usage_totals: BTreeMap::new(),
            coverage: BTreeMap::new(),
        }
    }
}

pub(crate) struct PacketProjector<'plan> {
    plan: &'plan ProjectionPlan,
    next_sequence: u64,
    usage_totals: BTreeMap<UsageField, String>,
    coverage: BTreeMap<&'static str, u64>,
}

impl PacketProjector<'_> {
    pub(crate) fn project_event(
        &mut self,
        event: &DiagnosticEvent,
    ) -> Result<Vec<TracePacket>, ProjectionError> {
        let header = event.header();
        if header.run_id() != self.plan.metadata.run_id() {
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

        let mut packets = match event {
            DiagnosticEvent::SpanStarted(value) => self.builtin_span_start(value)?,
            DiagnosticEvent::SpanFinished(value) => self.builtin_span_finish(value)?,
            DiagnosticEvent::InstantOccurred(value) => self.builtin_instant(value)?,
            DiagnosticEvent::CounterSampled(value) => {
                let scope_track = scope_track_identity(value.header().scope());
                let track = counter_track_identity(
                    &scope_track,
                    &builtin_counter_series(value.counter_kind().as_str()),
                );
                vec![self.numeric_packet(
                    value.header(),
                    &track,
                    value.counter_kind().as_str(),
                    &value.value().get().to_string(),
                    Vec::new(),
                )?]
            }
            DiagnosticEvent::AgentMessageDelta(value) => {
                let track = scope_track_identity(value.header().scope());
                let mut annotations = vec![string_annotation(
                    "troupe.message.id",
                    value.message_id().as_str(),
                )];
                if let Some(source) = value.source_message_id() {
                    annotations.push(string_annotation("troupe.message.source_id", source));
                }
                vec![self.instant_packet(
                    value.header(),
                    &track,
                    "agent.message.delta",
                    annotations,
                )?]
            }
            DiagnosticEvent::AgentMessageCompleted(value) => {
                let track = scope_track_identity(value.header().scope());
                vec![self.instant_packet(
                    value.header(),
                    &track,
                    "agent.message.completed",
                    vec![
                        string_annotation("troupe.message.id", value.message_id().as_str()),
                        uint_annotation("troupe.message.utf8_bytes", value.utf8_bytes().get()),
                        uint_annotation(
                            "troupe.message.unicode_scalar_count",
                            value.unicode_scalar_count().get(),
                        ),
                        bool_annotation("troupe.message.truncated", value.truncated()),
                    ],
                )?]
            }
            DiagnosticEvent::AgentPlanSnapshot(value) => {
                let track = scope_track_identity(value.header().scope());
                let mut status_counts = BTreeMap::<&str, u64>::new();
                for entry in value.entries() {
                    *status_counts.entry(entry.status().as_str()).or_default() += 1;
                }
                let mut annotations = vec![
                    uint_annotation("troupe.plan.entry_count", value.entries().len() as u64),
                    bool_annotation("troupe.plan.truncated", value.truncated()),
                ];
                for (status, count) in status_counts {
                    annotations.push(uint_annotation(
                        &format!("troupe.plan.status.{status}"),
                        count,
                    ));
                }
                vec![self.instant_packet(
                    value.header(),
                    &track,
                    "agent.plan.snapshot",
                    annotations,
                )?]
            }
            DiagnosticEvent::ContextUsageSampled(value) => self.context_usage(value)?,
            DiagnosticEvent::ActTokenUsageFinalized(value) => self.act_usage(value)?,
            DiagnosticEvent::ObservationGap(value) => {
                let track = scope_track_identity(value.header().scope());
                let mut annotations = vec![
                    string_annotation("troupe.gap.producer", value.producer()),
                    string_annotation("troupe.gap.reason", value.reason()),
                ];
                if let Some(component) = value.component() {
                    annotations.push(string_annotation("troupe.gap.component", component));
                }
                if let Some(count) = value.dropped_count() {
                    annotations.push(uint_annotation("troupe.gap.dropped_count", count.get()));
                }
                if let Some(interval) = value.affected_elapsed() {
                    annotations.push(uint_annotation(
                        "troupe.gap.affected_start_ns",
                        interval.start_ns().get(),
                    ));
                    annotations.push(uint_annotation(
                        "troupe.gap.affected_end_ns",
                        interval.end_ns().get(),
                    ));
                }
                if let Some(kind) = value.affected_kind() {
                    annotations.push(string_annotation(
                        "troupe.gap.affected_kind",
                        kind.as_str(),
                    ));
                }
                if let Some(scope) = value.affected_scope() {
                    annotations.push(string_annotation(
                        "troupe.gap.affected_scope",
                        &scope_track_identity(scope),
                    ));
                }
                vec![self.instant_packet(
                    value.header(),
                    &track,
                    "troupe.observation_gap",
                    annotations,
                )?]
            }
            DiagnosticEvent::CustomSpanStarted(value) => {
                let track = self.span_track(value.header().sequence())?.to_owned();
                let mut annotations = attribute_annotations(value.attributes());
                if let Some(parent) = value.parent_span_id() {
                    annotations.push(uint_annotation("troupe.span.parent_id", parent.get()));
                }
                annotations.push(uint_annotation(
                    "troupe.span.id",
                    value.header().sequence().get(),
                ));
                vec![self.event_packet(
                    value.header(),
                    &track,
                    TrackEventType::SliceBegin,
                    Some(value.name()),
                    None,
                    annotations,
                )?]
            }
            DiagnosticEvent::CustomSpanFinished(value) => {
                let track = self.span_track(value.span_id())?.to_owned();
                vec![self.event_packet(
                    value.header(),
                    &track,
                    TrackEventType::SliceEnd,
                    None,
                    None,
                    vec![
                        uint_annotation("troupe.span.id", value.span_id().get()),
                        string_annotation("troupe.span.outcome", value.outcome().as_str()),
                    ],
                )?]
            }
            DiagnosticEvent::CustomInstantOccurred(value) => {
                let track = self.containing_or_scope_track(
                    value.header().scope(),
                    value.containing_span_id(),
                )?;
                let mut annotations = attribute_annotations(value.attributes());
                if let Some(severity) = value.severity() {
                    annotations.push(string_annotation(
                        "troupe.custom.severity",
                        severity.as_str(),
                    ));
                }
                vec![self.instant_packet(
                    value.header(),
                    &track,
                    value.name(),
                    annotations,
                )?]
            }
            DiagnosticEvent::CustomCounterSampled(value) => {
                let scope_track = scope_track_identity(value.header().scope());
                let series = custom_counter_series(
                    value.name(),
                    value.unit(),
                    value.dimensions(),
                );
                let track = counter_track_identity(&scope_track, &series);
                let mut annotations = dimension_annotations(value.dimensions());
                if let Some(unit) = value.unit() {
                    annotations.push(string_annotation("troupe.counter.unit", unit));
                }
                let number = match value.value() {
                    CustomNumber::Integer(value) => value.as_str(),
                    CustomNumber::Decimal(value) => value.as_str(),
                };
                vec![self.numeric_packet(
                    value.header(),
                    &track,
                    value.name(),
                    number,
                    annotations,
                )?]
            }
        };
        let first = packets
            .first_mut()
            .expect("every canonical diagnostic event has a structural projection packet");
        self.attach_flows(header.sequence(), first)?;
        self.next_sequence = self.next_sequence.checked_add(1).ok_or(
            ProjectionError::SequenceMismatch {
                expected: u64::MAX,
                actual: u64::MAX,
            },
        )?;
        Ok(packets)
    }

    pub(crate) fn finish(&self) -> Result<(), ProjectionError> {
        let projected = self.next_sequence.saturating_sub(1);
        if projected != self.plan.event_count {
            return Err(ProjectionError::ProjectionIncomplete {
                expected: self.plan.event_count,
                projected,
            });
        }
        Ok(())
    }

    fn builtin_span_start(
        &self,
        value: &troupe_diagnostics_core::event::SpanStarted,
    ) -> Result<Vec<TracePacket>, ProjectionError> {
        let track = self.span_track(value.header().sequence())?;
        let mut annotations = vec![
            uint_annotation("troupe.span.id", value.header().sequence().get()),
            string_annotation("troupe.span.kind", value.span_kind().as_str()),
        ];
        if let Some(parent) = value.parent_span_id() {
            annotations.push(uint_annotation("troupe.span.parent_id", parent.get()));
        }
        annotations.extend(span_detail_annotations(value.detail()));
        if value.span_kind() == SpanKind::ActLifecycle {
            let act_identity = scope_track_identity(value.header().scope());
            if let Some(usage) = self.plan.act_usage.get(&act_identity) {
                annotations.extend(usage_annotations(usage));
            }
        }
        Ok(vec![self.event_packet(
            value.header(),
            track,
            TrackEventType::SliceBegin,
            Some(value.span_kind().as_str()),
            None,
            annotations,
        )?])
    }

    fn builtin_span_finish(
        &self,
        value: &troupe_diagnostics_core::event::SpanFinished,
    ) -> Result<Vec<TracePacket>, ProjectionError> {
        let track = self.span_track(value.span_id())?;
        let mut annotations = vec![
            uint_annotation("troupe.span.id", value.span_id().get()),
            string_annotation("troupe.span.outcome", value.outcome().as_str()),
        ];
        if let Some(error) = value.error_code() {
            annotations.push(string_annotation("troupe.error.code", error));
        }
        Ok(vec![self.event_packet(
            value.header(),
            track,
            TrackEventType::SliceEnd,
            None,
            None,
            annotations,
        )?])
    }

    fn builtin_instant(
        &self,
        value: &troupe_diagnostics_core::event::InstantOccurred,
    ) -> Result<Vec<TracePacket>, ProjectionError> {
        let track = self.containing_or_scope_track(
            value.header().scope(),
            value.containing_span_id(),
        )?;
        Ok(vec![self.instant_packet(
            value.header(),
            &track,
            value.instant_kind().as_str(),
            instant_detail_annotations(value.detail()),
        )?])
    }

    fn context_usage(
        &self,
        value: &troupe_diagnostics_core::event::ContextUsageSampled,
    ) -> Result<Vec<TracePacket>, ProjectionError> {
        let scope_track = scope_track_identity(value.header().scope());
        let mut summary = vec![string_annotation(
            "troupe.context.sample_origin",
            value.sample_origin().as_str(),
        )];
        if let Some(observed) = value.observed_elapsed_ns() {
            summary.push(uint_annotation(
                "troupe.context.observed_elapsed_ns",
                observed.get(),
            ));
        }
        let mut packets = vec![self.instant_packet(
            value.header(),
            &scope_track,
            "context.usage",
            summary,
        )?];
        if let Some(used) = value.context_used_tokens() {
            let track = counter_track_identity(
                &scope_track,
                context_counter_series("used_tokens"),
            );
            packets.push(self.numeric_packet(
                value.header(),
                &track,
                "context.used_tokens",
                &used.get().to_string(),
                Vec::new(),
            )?);
        }
        if let Some(window) = value.context_window_tokens() {
            let track = counter_track_identity(
                &scope_track,
                context_counter_series("window_tokens"),
            );
            packets.push(self.numeric_packet(
                value.header(),
                &track,
                "context.window_tokens",
                &window.get().to_string(),
                Vec::new(),
            )?);
        }
        if let (Some(amount), Some(currency)) = (
            value.cumulative_cost_amount(),
            value.cumulative_cost_currency(),
        ) {
            let track = counter_track_identity(
                &scope_track,
                &context_cost_series(currency.as_str()),
            );
            packets.push(self.numeric_packet(
                value.header(),
                &track,
                "context.cumulative_cost",
                amount.as_str(),
                vec![string_annotation("troupe.cost.currency", currency.as_str())],
            )?);
        }
        Ok(packets)
    }

    fn act_usage(
        &mut self,
        value: &troupe_diagnostics_core::event::ActTokenUsageFinalized,
    ) -> Result<Vec<TracePacket>, ProjectionError> {
        let scope_track = scope_track_identity(value.header().scope());
        let summary = UsageSummary {
            availability: value.availability().as_str(),
            source: value.source().map(|source| source.as_str()),
            unavailable_reason: value.unavailable_reason().map(|reason| reason.as_str()),
            values: UsageField::ALL
                .into_iter()
                .filter_map(|field| {
                    usage_value(value, field)
                        .map(|tokens| (field, tokens.as_str().to_owned()))
                })
                .collect(),
        };
        let mut packets = vec![self.instant_packet(
            value.header(),
            &scope_track,
            "act.token_usage",
            usage_annotations(&summary),
        )?];

        for (field, amount) in &summary.values {
            let total = self
                .usage_totals
                .entry(*field)
                .or_insert_with(|| "0".to_owned());
            add_nonnegative_decimal(total, amount);
            let total = total.clone();
            let track = counter_track_identity(
                ROOT_TRACK_IDENTITY,
                usage_counter_series(field.as_str()),
            );
            packets.push(self.numeric_packet(
                value.header(),
                &track,
                &format!("usage.known.{}", field.as_str()),
                &total,
                Vec::new(),
            )?);
        }

        increment(&mut self.coverage, "finalized");
        increment(&mut self.coverage, value.availability().as_str());
        if value.availability() != UsageAvailability::Unavailable {
            increment(&mut self.coverage, "reported");
        }
        for coverage in USAGE_COVERAGE_COUNTERS {
            let count = self.coverage.get(coverage).copied().unwrap_or(0);
            let track = counter_track_identity(
                ROOT_TRACK_IDENTITY,
                usage_coverage_series(coverage),
            );
            packets.push(self.numeric_packet(
                value.header(),
                &track,
                &format!("usage.coverage.{coverage}"),
                &count.to_string(),
                Vec::new(),
            )?);
        }
        Ok(packets)
    }

    fn span_track(&self, span_id: SchemaU64) -> Result<&str, ProjectionError> {
        self.plan
            .span_tracks
            .get(&span_id)
            .map(String::as_str)
            .ok_or(ProjectionError::MissingSpan(span_id.get()))
    }

    fn containing_or_scope_track(
        &self,
        scope: &DiagnosticScope,
        containing_span_id: Option<SchemaU64>,
    ) -> Result<String, ProjectionError> {
        containing_span_id.map_or_else(
            || Ok(scope_track_identity(scope)),
            |span_id| self.span_track(span_id).map(str::to_owned),
        )
    }

    fn instant_packet(
        &self,
        header: &troupe_diagnostics_core::event::DiagnosticEventHeader,
        track_identity: &str,
        name: &str,
        annotations: Vec<DebugAnnotation>,
    ) -> Result<TracePacket, ProjectionError> {
        self.event_packet(
            header,
            track_identity,
            TrackEventType::Instant,
            Some(name),
            None,
            annotations,
        )
    }

    fn numeric_packet(
        &self,
        header: &troupe_diagnostics_core::event::DiagnosticEventHeader,
        track_identity: &str,
        name: &str,
        canonical_decimal: &str,
        mut annotations: Vec<DebugAnnotation>,
    ) -> Result<TracePacket, ProjectionError> {
        match exact_counter_value(canonical_decimal) {
            Some(ExactCounterValue::Integer(value)) => self.event_packet(
                header,
                track_identity,
                TrackEventType::Counter,
                Some(name),
                Some(track_event::CounterValueField::CounterValue(value)),
                annotations,
            ),
            Some(ExactCounterValue::Double(value)) => self.event_packet(
                header,
                track_identity,
                TrackEventType::Counter,
                Some(name),
                Some(track_event::CounterValueField::DoubleCounterValue(value)),
                annotations,
            ),
            None => {
                annotations.push(string_annotation(
                    "troupe.counter.value_decimal",
                    canonical_decimal,
                ));
                annotations.push(string_annotation(
                    "troupe.counter_projection",
                    "not_exact",
                ));
                self.event_packet(
                    header,
                    track_identity,
                    TrackEventType::Instant,
                    Some(name),
                    None,
                    annotations,
                )
            }
        }
    }

    fn event_packet(
        &self,
        header: &troupe_diagnostics_core::event::DiagnosticEventHeader,
        track_identity: &str,
        event_type: TrackEventType,
        name: Option<&str>,
        counter: Option<track_event::CounterValueField>,
        mut annotations: Vec<DebugAnnotation>,
    ) -> Result<TracePacket, ProjectionError> {
        let track_uuid = self.plan.tracks.id(track_identity)?;
        let mut common = vec![
            string_annotation("troupe.event.kind", event_type_name(event_type)),
            uint_annotation("troupe.event.sequence", header.sequence().get()),
            string_annotation(
                "troupe.scope.identity",
                &scope_track_identity(header.scope()),
            ),
            string_annotation("troupe.track.identity", track_identity),
        ];
        common.append(&mut annotations);
        Ok(TracePacket {
            timestamp: Some(header.elapsed_ns().get()),
            data: Some(trace_packet::Data::TrackEvent(TrackEvent {
                debug_annotations: common,
                r#type: Some(event_type as i32),
                track_uuid: Some(track_uuid),
                name_field: name.map(|name| track_event::NameField::Name(name.to_owned())),
                counter_value_field: counter,
                flow_ids: Vec::new(),
                terminating_flow_ids: Vec::new(),
            })),
            timestamp_clock_id: Some(BuiltinClock::TraceFile as u32),
        })
    }

    fn attach_flows(
        &self,
        sequence: SchemaU64,
        packet: &mut TracePacket,
    ) -> Result<(), ProjectionError> {
        let Some(trace_packet::Data::TrackEvent(event)) = packet.data.as_mut() else {
            return Err(ProjectionError::unknown_identity("flow packet is not a TrackEvent"));
        };
        if let Some(flows) = self.plan.starting_flows.get(&sequence) {
            for (index, flow) in flows.iter().enumerate() {
                event.flow_ids.push(flow.id);
                event.debug_annotations.push(string_annotation(
                    &format!("troupe.flow.start.{index}"),
                    &flow.canonical_identity,
                ));
            }
        }
        if let Some(flows) = self.plan.terminating_flows.get(&sequence) {
            for (index, flow) in flows.iter().enumerate() {
                event.terminating_flow_ids.push(flow.id);
                event.debug_annotations.push(string_annotation(
                    &format!("troupe.flow.end.{index}"),
                    &flow.canonical_identity,
                ));
            }
        }
        Ok(())
    }
}

fn usage_value(
    usage: &troupe_diagnostics_core::event::ActTokenUsageFinalized,
    field: UsageField,
) -> Option<&TokenCount> {
    match field {
        UsageField::ProviderTotal => usage.provider_total_tokens(),
        UsageField::Input => usage.input_tokens(),
        UsageField::Output => usage.output_tokens(),
        UsageField::Thought => usage.thought_tokens(),
        UsageField::CachedRead => usage.cached_read_tokens(),
        UsageField::CachedWrite => usage.cached_write_tokens(),
    }
}

fn increment(counts: &mut BTreeMap<&'static str, u64>, key: &'static str) {
    let value = counts.entry(key).or_default();
    *value = value
        .checked_add(1)
        .expect("captured event count cannot exceed the u64 sequence domain");
}

fn add_nonnegative_decimal(total: &mut String, amount: &str) {
    let mut carry = 0_u8;
    let mut left = total.bytes().rev();
    let mut right = amount.bytes().rev();
    let mut digits = Vec::with_capacity(total.len().max(amount.len()) + 1);
    loop {
        let left = left.next().map(|digit| digit - b'0');
        let right = right.next().map(|digit| digit - b'0');
        if left.is_none() && right.is_none() && carry == 0 {
            break;
        }
        let sum = left.unwrap_or(0) + right.unwrap_or(0) + carry;
        digits.push(b'0' + sum % 10);
        carry = sum / 10;
    }
    digits.reverse();
    *total = String::from_utf8(digits).expect("decimal addition emits ASCII digits");
}

fn span_detail_annotations(detail: &SpanStartDetail) -> Vec<DebugAnnotation> {
    match detail {
        SpanStartDetail::ProductionPathResolution(detail) => vec![
            string_annotation("troupe.production.root", detail.production_root()),
            string_annotation("troupe.production.package", detail.package()),
        ],
        SpanStartDetail::ProductionLoad(detail) => vec![string_annotation(
            "troupe.production.package",
            detail.package(),
        )],
        SpanStartDetail::ProductionConstruct(detail) => vec![
            string_annotation("troupe.production.package", detail.package()),
            string_annotation("troupe.production.class", detail.class_name()),
        ],
        SpanStartDetail::ActorHandleLifetime(detail) => actor_annotations(detail),
        SpanStartDetail::EffectLifecycle(detail) => vec![string_annotation(
            "troupe.effect.type",
            detail.effect_type(),
        )],
        SpanStartDetail::AgentSessionOpening(detail)
        | SpanStartDetail::AgentSessionLifecycle(detail)
        | SpanStartDetail::AgentSessionClosing(detail)
        | SpanStartDetail::ActLifecycle(detail)
        | SpanStartDetail::AgentTurn(detail) => session_annotations(detail),
        SpanStartDetail::ToolCall(detail) => tool_annotations(detail),
        SpanStartDetail::RunLifecycle(_)
        | SpanStartDetail::ProductionStart(_)
        | SpanStartDetail::ProductionStop(_)
        | SpanStartDetail::ProductionShutdown(_)
        | SpanStartDetail::SceneLifecycle(_)
        | SpanStartDetail::SceneDrain(_)
        | SpanStartDetail::SceneCleanup(_)
        | SpanStartDetail::CueMailboxWait(_)
        | SpanStartDetail::CueExecution(_)
        | SpanStartDetail::ActCaller(_)
        | SpanStartDetail::AgentThinking(_) => Vec::new(),
    }
}

fn instant_detail_annotations(detail: &InstantDetail) -> Vec<DebugAnnotation> {
    match detail {
        InstantDetail::ActorCast(detail) => actor_annotations(detail),
        InstantDetail::EffectCreated(detail)
        | InstantDetail::EffectReturned(detail)
        | InstantDetail::EffectConsumed(detail) => vec![string_annotation(
            "troupe.effect.type",
            detail.effect_type(),
        )],
        InstantDetail::AgentSessionReady(detail) => session_annotations(detail),
        InstantDetail::AgentSessionBroken(detail) => {
            let mut annotations = vec![
                string_annotation("troupe.agent.provider", detail.provider()),
                string_annotation("troupe.error.code", detail.error_code()),
            ];
            optional_string_annotation(
                &mut annotations,
                "troupe.agent.model",
                detail.effective_model(),
            );
            optional_string_annotation(
                &mut annotations,
                "troupe.agent.effort",
                detail.effective_effort(),
            );
            annotations
        }
        InstantDetail::AgentTurnTerminal(detail)
        | InstantDetail::AgentTurnSettled(detail) => detail
            .error_code()
            .map(|error| vec![string_annotation("troupe.error.code", error)])
            .unwrap_or_default(),
        InstantDetail::ToolUpdated(detail) => tool_annotations(detail),
        InstantDetail::ResultSubmitted(detail)
        | InstantDetail::ResultRejected(detail)
        | InstantDetail::ResultRepairRequested(detail)
        | InstantDetail::ResultAccepted(detail)
        | InstantDetail::ResultMissing(detail) => {
            let mut annotations = Vec::new();
            if let Some(issue) = detail.issue() {
                annotations.push(string_annotation("troupe.result.issue.code", issue.code()));
                annotations.push(string_annotation("troupe.result.issue.path", issue.path()));
            }
            optional_string_annotation(
                &mut annotations,
                "troupe.error.code",
                detail.error_code(),
            );
            annotations
        }
        InstantDetail::DiagnosticComponentFailed(detail) => {
            let mut annotations = vec![
                string_annotation("troupe.component.kind", detail.component().as_str()),
                string_annotation("troupe.component.id", detail.component_id().as_str()),
                string_annotation("troupe.component.stage", detail.stage().as_str()),
                string_annotation("troupe.error.code", detail.error_code().as_str()),
            ];
            if let Some(sequence) = detail.related_event_sequence() {
                annotations.push(uint_annotation(
                    "troupe.component.related_event_sequence",
                    sequence.get(),
                ));
            }
            annotations
        }
        InstantDetail::CueAdmitted(_)
        | InstantDetail::CueEnqueued(_)
        | InstantDetail::CueDispatched(_)
        | InstantDetail::CueCancelRequested(_)
        | InstantDetail::ActAdmitted(_)
        | InstantDetail::ActWaitingReady(_)
        | InstantDetail::ActPromptSubmitted(_)
        | InstantDetail::ActCancelRequested(_)
        | InstantDetail::ActSupervisorHandoff(_)
        | InstantDetail::AgentTurnActivity(_) => Vec::new(),
    }
}

fn actor_annotations(
    detail: &troupe_diagnostics_core::detail::ActorDetail,
) -> Vec<DebugAnnotation> {
    vec![
        string_annotation("troupe.actor.display_name", detail.display_name()),
        string_annotation("troupe.actor.type", detail.actor_type()),
    ]
}

fn session_annotations(
    detail: &troupe_diagnostics_core::detail::AgentSessionDetail,
) -> Vec<DebugAnnotation> {
    let mut annotations = vec![string_annotation(
        "troupe.agent.provider",
        detail.provider(),
    )];
    optional_string_annotation(
        &mut annotations,
        "troupe.agent.model",
        detail.effective_model(),
    );
    optional_string_annotation(
        &mut annotations,
        "troupe.agent.effort",
        detail.effective_effort(),
    );
    annotations
}

fn tool_annotations(
    detail: &troupe_diagnostics_core::detail::ToolCallDetail,
) -> Vec<DebugAnnotation> {
    let mut annotations = vec![
        string_annotation("troupe.tool.title", detail.title()),
        string_annotation("troupe.tool.kind", detail.tool_kind().as_str()),
        string_annotation("troupe.tool.status", detail.status().as_str()),
    ];
    optional_string_annotation(
        &mut annotations,
        "troupe.error.code",
        detail.error_code(),
    );
    annotations
}

fn usage_annotations(usage: &UsageSummary) -> Vec<DebugAnnotation> {
    let mut annotations = vec![string_annotation(
        "troupe.usage.availability",
        usage.availability,
    )];
    optional_string_annotation(&mut annotations, "troupe.usage.source", usage.source);
    optional_string_annotation(
        &mut annotations,
        "troupe.usage.unavailable_reason",
        usage.unavailable_reason,
    );
    for (field, value) in &usage.values {
        annotations.push(string_annotation(
            &format!("troupe.usage.{}", field.as_str()),
            value,
        ));
    }
    annotations
}

fn attribute_annotations(
    attributes: &troupe_diagnostics_core::detail::DiagnosticAttributes,
) -> Vec<DebugAnnotation> {
    attributes
        .iter()
        .map(|(key, value)| {
            let name = format!("troupe.custom.attribute.{key}");
            match value {
                DiagnosticAttributeValue::Null => string_annotation(&name, "null"),
                DiagnosticAttributeValue::Boolean(value) => bool_annotation(&name, *value),
                DiagnosticAttributeValue::Integer(value) => exact_integer_annotation(
                    &name,
                    value.as_str(),
                ),
                DiagnosticAttributeValue::Decimal(value) => {
                    string_annotation(&name, value.as_str())
                }
                DiagnosticAttributeValue::String(value) => string_annotation(&name, value),
                DiagnosticAttributeValue::List(values) => {
                    string_annotation(&name, &scalar_list(values))
                }
            }
        })
        .collect()
}

fn scalar_list(values: &[DiagnosticScalar]) -> String {
    values
        .iter()
        .map(|value| match value {
            DiagnosticScalar::Null => "null".to_owned(),
            DiagnosticScalar::Boolean(value) => format!("bool:{value}"),
            DiagnosticScalar::Integer(value) => format!("integer:{}", value.as_str()),
            DiagnosticScalar::Decimal(value) => format!("decimal:{}", value.as_str()),
            DiagnosticScalar::String(value) => format!("string:{}:{value}", value.len()),
        })
        .collect::<Vec<_>>()
        .join("|")
}

fn dimension_annotations(
    dimensions: &troupe_diagnostics_core::detail::DiagnosticDimensions,
) -> Vec<DebugAnnotation> {
    dimensions
        .iter()
        .map(|(key, value)| {
            let name = format!("troupe.counter.dimension.{key}");
            match value {
                DiagnosticDimension::Boolean(value) => bool_annotation(&name, *value),
                DiagnosticDimension::Integer(value) => {
                    exact_integer_annotation(&name, value.as_str())
                }
                DiagnosticDimension::Decimal(value) => {
                    string_annotation(&name, value.as_str())
                }
                DiagnosticDimension::String(value) => string_annotation(&name, value),
            }
        })
        .collect()
}

fn exact_integer_annotation(name: &str, value: &str) -> DebugAnnotation {
    match value.parse::<i64>() {
        Ok(value) => int_annotation(name, value),
        Err(_) => string_annotation(name, value),
    }
}

fn optional_string_annotation(
    annotations: &mut Vec<DebugAnnotation>,
    name: &str,
    value: Option<&str>,
) {
    if let Some(value) = value {
        annotations.push(string_annotation(name, value));
    }
}

fn string_annotation(name: &str, value: &str) -> DebugAnnotation {
    annotation(
        name,
        debug_annotation::Value::StringValue(value.to_owned()),
    )
}

fn uint_annotation(name: &str, value: u64) -> DebugAnnotation {
    annotation(name, debug_annotation::Value::UintValue(value))
}

fn int_annotation(name: &str, value: i64) -> DebugAnnotation {
    annotation(name, debug_annotation::Value::IntValue(value))
}

fn bool_annotation(name: &str, value: bool) -> DebugAnnotation {
    annotation(name, debug_annotation::Value::BoolValue(value))
}

fn annotation(name: &str, value: debug_annotation::Value) -> DebugAnnotation {
    DebugAnnotation {
        value: Some(value),
        name_field: Some(debug_annotation::NameField::Name(name.to_owned())),
    }
}

fn event_type_name(value: TrackEventType) -> &'static str {
    match value {
        TrackEventType::Unspecified => "unspecified",
        TrackEventType::SliceBegin => "slice_begin",
        TrackEventType::SliceEnd => "slice_end",
        TrackEventType::Instant => "instant",
        TrackEventType::Counter => "counter",
    }
}

fn packets_json(packets: &[TracePacket]) -> String {
    let mut output = String::from("[\n");
    for (index, packet) in packets.iter().enumerate() {
        if index != 0 {
            output.push_str(",\n");
        }
        output.push_str("  {");
        match packet.data.as_ref() {
            Some(trace_packet::Data::TrackDescriptor(descriptor)) => {
                output.push_str("\"kind\":\"descriptor\",\"uuid\":");
                push_optional_u64(&mut output, descriptor.uuid);
                output.push_str(",\"parent_uuid\":");
                push_optional_u64(&mut output, descriptor.parent_uuid);
                output.push_str(",\"name\":");
                let name = match descriptor.static_or_dynamic_name.as_ref() {
                    Some(track_descriptor::StaticOrDynamicName::Name(name)) => name.as_str(),
                    None => "",
                };
                push_json_string(&mut output, name);
            }
            Some(trace_packet::Data::TrackEvent(event)) => {
                output.push_str("\"kind\":\"event\",\"timestamp\":");
                push_optional_u64(&mut output, packet.timestamp);
                output.push_str(",\"clock_id\":");
                push_optional_u64(
                    &mut output,
                    packet.timestamp_clock_id.map(u64::from),
                );
                output.push_str(",\"type\":");
                push_json_string(
                    &mut output,
                    event
                        .r#type
                        .and_then(|value| TrackEventType::try_from(value).ok())
                        .map(event_type_name)
                        .unwrap_or("unknown"),
                );
                output.push_str(",\"track_uuid\":");
                push_optional_u64(&mut output, event.track_uuid);
                output.push_str(",\"name\":");
                match event.name_field.as_ref() {
                    Some(track_event::NameField::Name(name)) => push_json_string(&mut output, name),
                    None => output.push_str("null"),
                }
                output.push_str(",\"counter\":");
                match event.counter_value_field.as_ref() {
                    Some(track_event::CounterValueField::CounterValue(value)) => {
                        write!(output, "{{\"int64\":\"{value}\"}}")
                            .expect("write JSON integer counter");
                    }
                    Some(track_event::CounterValueField::DoubleCounterValue(value)) => {
                        output.push_str("{\"double\":");
                        push_json_string(&mut output, &value.to_string());
                        output.push('}');
                    }
                    None => output.push_str("null"),
                }
                output.push_str(",\"flow_ids\":");
                push_u64_array(&mut output, &event.flow_ids);
                output.push_str(",\"terminating_flow_ids\":");
                push_u64_array(&mut output, &event.terminating_flow_ids);
                output.push_str(",\"annotations\":[");
                for (annotation_index, annotation) in event.debug_annotations.iter().enumerate() {
                    if annotation_index != 0 {
                        output.push(',');
                    }
                    output.push_str("{\"name\":");
                    let name = match annotation.name_field.as_ref() {
                        Some(debug_annotation::NameField::Name(name)) => name.as_str(),
                        None => "",
                    };
                    push_json_string(&mut output, name);
                    match annotation.value.as_ref() {
                        Some(debug_annotation::Value::BoolValue(value)) => {
                            write!(output, ",\"bool\":{value}").expect("write JSON bool")
                        }
                        Some(debug_annotation::Value::UintValue(value)) => {
                            write!(output, ",\"uint\":\"{value}\"")
                                .expect("write JSON uint")
                        }
                        Some(debug_annotation::Value::IntValue(value)) => {
                            write!(output, ",\"int\":\"{value}\"")
                                .expect("write JSON int")
                        }
                        Some(debug_annotation::Value::DoubleValue(value)) => {
                            output.push_str(",\"double\":");
                            push_json_string(&mut output, &value.to_string());
                        }
                        Some(debug_annotation::Value::StringValue(value)) => {
                            output.push_str(",\"string\":");
                            push_json_string(&mut output, value);
                        }
                        None => output.push_str(",\"null\":true"),
                    }
                    output.push('}');
                }
                output.push(']');
            }
            None => output.push_str("\"kind\":\"empty\""),
        }
        output.push('}');
    }
    output.push_str("\n]\n");
    output
}

fn push_optional_u64(output: &mut String, value: Option<u64>) {
    match value {
        Some(value) => {
            write!(output, "\"{value}\"").expect("write JSON u64");
        }
        None => output.push_str("null"),
    }
}

fn push_u64_array(output: &mut String, values: &[u64]) {
    output.push('[');
    for (index, value) in values.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        write!(output, "\"{value}\"").expect("write JSON u64 array");
    }
    output.push(']');
}

fn push_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character <= '\u{1f}' => {
                write!(output, "\\u{:04x}", u32::from(character))
                    .expect("write JSON control escape");
            }
            character => output.push(character),
        }
    }
    output.push('"');
}
