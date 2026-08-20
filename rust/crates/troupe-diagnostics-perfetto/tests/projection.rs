use std::collections::BTreeMap;

use troupe_diagnostics_core::{
    detail::{
        ActorDetail, AgentSessionDetail, CanonicalInteger, CustomNumber, DiagnosticAttributeValue,
        DiagnosticAttributes, DiagnosticDimensions, EmptyDetail, InstantDetail, SpanStartDetail,
    },
    event::{
        ActTokenUsageFinalized, AffectedElapsedInterval, AgentMessageDelta, CausalLink,
        ContextUsageSampled, CounterSampled, CustomCounterSampled, CustomInstantOccurred,
        DiagnosticEvent, DiagnosticEventHeader, DiagnosticScope, InstantOccurred, ObservationGap,
        SpanFinished, SpanStarted,
    },
    id::{CanonicalUuid, RunLocalId},
    kinds::{
        CausalRelation, ContextSampleOrigin, CounterKind, CustomSeverity, SpanOutcome,
        UsageAvailability, UsageSource,
    },
    scalar::{CurrencyCode, DecimalString, SchemaU64, TokenCount},
    time::ElapsedNs,
};
use troupe_diagnostics_perfetto::{
    collect::{ProjectionError, ProjectionLimits, ProjectionMetadata},
    project::{project_prefix, project_prefix_with_limits},
};

const RUN: &str = "12345678-1234-4234-9234-123456789abc";

fn run() -> CanonicalUuid {
    CanonicalUuid::parse(RUN).unwrap()
}

fn local(value: &str) -> RunLocalId {
    RunLocalId::parse(value).unwrap()
}

fn scope(
    scene: Option<&str>,
    actor: Option<&str>,
    cue: Option<&str>,
    act: Option<&str>,
) -> DiagnosticScope {
    DiagnosticScope::new(
        scene.map(local),
        actor.map(local),
        cue.map(local),
        None,
        act.map(local),
        None,
        None,
    )
}

fn header(
    sequence: u64,
    elapsed_ns: u64,
    scope: DiagnosticScope,
    caused_by: Vec<CausalLink>,
) -> DiagnosticEventHeader {
    DiagnosticEventHeader::new(
        run(),
        SchemaU64::new(sequence),
        ElapsedNs::new(elapsed_ns),
        scope,
        caused_by,
    )
    .unwrap()
}

fn start(
    sequence: u64,
    elapsed_ns: u64,
    scope: DiagnosticScope,
    detail: SpanStartDetail,
    parent: Option<u64>,
) -> DiagnosticEvent {
    DiagnosticEvent::SpanStarted(SpanStarted::new(
        header(sequence, elapsed_ns, scope, Vec::new()),
        detail,
        parent.map(SchemaU64::new),
    ))
}

fn finish(sequence: u64, elapsed_ns: u64, scope: DiagnosticScope, span_id: u64) -> DiagnosticEvent {
    DiagnosticEvent::SpanFinished(SpanFinished::new(
        header(sequence, elapsed_ns, scope, Vec::new()),
        SchemaU64::new(span_id),
        SpanOutcome::Completed,
        None,
    ))
}

fn metadata(watermark: u64) -> ProjectionMetadata {
    ProjectionMetadata::new(
        run(),
        SchemaU64::new(watermark),
        SchemaU64::new(watermark),
        "0.1.0-test",
    )
    .with_completion(Some("completed".to_owned()), Some(true))
}

fn canonical_prefix() -> Vec<DiagnosticEvent> {
    let root = scope(None, None, None, None);
    let scene = scope(Some("scene-1"), None, None, None);
    let actor = scope(Some("scene-1"), Some("actor-1"), None, None);
    let cue_one = scope(Some("scene-1"), Some("actor-1"), Some("cue-1"), None);
    let act = scope(
        Some("scene-1"),
        Some("actor-1"),
        Some("cue-1"),
        Some("act-1"),
    );
    let cue_two = scope(Some("scene-1"), Some("actor-1"), Some("cue-2"), None);

    let exact_counter = CustomCounterSampled::new(
        header(10, 80, act.clone(), Vec::new()),
        "orders.ratio".to_owned(),
        CustomNumber::Decimal(DecimalString::parse("0.5").unwrap()),
        Some("ratio".to_owned()),
        DiagnosticDimensions::new(),
    )
    .unwrap();
    let non_exact_counter = CustomCounterSampled::new(
        header(11, 90, act.clone(), Vec::new()),
        "orders.approximation".to_owned(),
        CustomNumber::Decimal(DecimalString::parse("0.1").unwrap()),
        None,
        DiagnosticDimensions::new(),
    )
    .unwrap();
    let mut marker_attributes = DiagnosticAttributes::new();
    marker_attributes.insert(
        "label".to_owned(),
        DiagnosticAttributeValue::String("完成 一".to_owned()),
    );

    vec![
        start(
            1,
            0,
            root.clone(),
            SpanStartDetail::RunLifecycle(EmptyDetail::new()),
            None,
        ),
        start(
            2,
            10,
            scene.clone(),
            SpanStartDetail::SceneLifecycle(EmptyDetail::new()),
            Some(1),
        ),
        start(
            3,
            20,
            actor.clone(),
            SpanStartDetail::ActorHandleLifetime(ActorDetail::new(
                "Worker 一".to_owned(),
                "Worker".to_owned(),
            )),
            Some(2),
        ),
        start(
            4,
            30,
            cue_one.clone(),
            SpanStartDetail::CueExecution(EmptyDetail::new()),
            Some(3),
        ),
        start(
            5,
            40,
            cue_one.clone(),
            SpanStartDetail::CueMailboxWait(EmptyDetail::new()),
            Some(4),
        ),
        finish(6, 50, cue_one.clone(), 5),
        DiagnosticEvent::InstantOccurred(InstantOccurred::new(
            header(
                7,
                50,
                cue_one.clone(),
                vec![CausalLink::new(SchemaU64::new(5), CausalRelation::Dispatch)],
            ),
            InstantDetail::CueDispatched(EmptyDetail::new()),
            Some(SchemaU64::new(4)),
        )),
        start(
            8,
            60,
            act.clone(),
            SpanStartDetail::ActLifecycle(AgentSessionDetail::new(
                "codex".to_owned(),
                Some("gpt-5".to_owned()),
                Some("high".to_owned()),
            )),
            Some(4),
        ),
        DiagnosticEvent::ContextUsageSampled(
            ContextUsageSampled::new(
                header(9, 70, act.clone(), Vec::new()),
                Some(SchemaU64::new(4_096)),
                Some(SchemaU64::new(32_768)),
                Some(DecimalString::parse("1.25").unwrap()),
                Some(CurrencyCode::parse("USD").unwrap()),
                ContextSampleOrigin::Provider,
                None,
            )
            .unwrap(),
        ),
        DiagnosticEvent::CustomCounterSampled(exact_counter),
        DiagnosticEvent::CustomCounterSampled(non_exact_counter),
        DiagnosticEvent::AgentMessageDelta(AgentMessageDelta::new(
            header(12, 100, act.clone(), Vec::new()),
            local("message-1"),
            Some("provider-message-1".to_owned()),
            "SECRET MESSAGE BODY MUST NOT APPEAR".to_owned(),
        )),
        DiagnosticEvent::ActTokenUsageFinalized(
            ActTokenUsageFinalized::new(
                header(13, 110, act.clone(), Vec::new()),
                UsageAvailability::Available,
                Some(UsageSource::AcpPromptResponseUsage),
                None,
                Some(TokenCount::parse("30").unwrap()),
                Some(TokenCount::parse("20").unwrap()),
                Some(TokenCount::parse("10").unwrap()),
                Some(TokenCount::parse("2").unwrap()),
                Some(TokenCount::parse("3").unwrap()),
                Some(TokenCount::parse("1").unwrap()),
            )
            .unwrap(),
        ),
        DiagnosticEvent::ObservationGap(ObservationGap::new(
            header(14, 120, act.clone(), Vec::new()),
            "agent-observer".to_owned(),
            Some("message".to_owned()),
            "resource_limit".to_owned(),
            Some(SchemaU64::new(2)),
            Some(AffectedElapsedInterval::new(
                ElapsedNs::new(100),
                ElapsedNs::new(119),
            )),
            None,
            Some(act.clone()),
        )),
        finish(15, 130, act, 8),
        finish(16, 140, cue_one, 4),
        start(
            17,
            150,
            cue_two.clone(),
            SpanStartDetail::CueExecution(EmptyDetail::new()),
            Some(3),
        ),
        finish(18, 160, cue_two, 17),
        DiagnosticEvent::CustomInstantOccurred(
            CustomInstantOccurred::new(
                header(19, 170, actor.clone(), Vec::new()),
                "orders.completed".to_owned(),
                Some(SchemaU64::new(3)),
                Some(CustomSeverity::Info),
                marker_attributes,
            )
            .unwrap(),
        ),
        finish(20, 180, actor, 3),
        finish(21, 190, scene, 2),
        finish(22, 200, root, 1),
    ]
}

#[test]
fn canonical_projection_is_deterministic_and_preserves_packet_contract() {
    let events = canonical_prefix();
    let first = project_prefix(metadata(events.len() as u64), &events).unwrap();
    let second = project_prefix(metadata(events.len() as u64), &events).unwrap();
    let bytes = first.trace_bytes().unwrap();
    let packets = first.debug_packets_json();

    assert_eq!(bytes, second.trace_bytes().unwrap());
    assert_eq!(packets, second.debug_packets_json());
    assert!(packets.contains("Worker 一"));
    assert!(packets.contains("完成 一"));
    assert!(packets.contains("\"clock_id\":\"11\""));
    assert!(packets.contains("\"packet_sequence_id\":\"1\""));
    assert!(packets.contains("troupe.flow.start.0"));
    assert!(packets.contains("troupe.gap.affected_start_ns"));
    assert!(packets.contains("\"double\":\"0.5\""));
    assert!(packets.contains("troupe.counter_projection"));
    assert!(packets.contains("usage.coverage.reported"));
    let act_start = packets
        .find("\"troupe.event.sequence\",\"uint\":\"8\"")
        .unwrap();
    let next_event = packets[act_start..]
        .find("\"troupe.event.sequence\",\"uint\":\"9\"")
        .map(|offset| act_start + offset)
        .unwrap();
    assert!(packets[act_start..next_event].contains("troupe.usage.input_tokens"));
    assert!(!packets.contains("SECRET MESSAGE BODY MUST NOT APPEAR"));
    assert!(!String::from_utf8_lossy(&bytes).contains("SECRET MESSAGE BODY MUST NOT APPEAR"));
}

#[test]
fn empty_prefix_is_descriptor_only_and_open_spans_have_no_fake_end() {
    let empty = project_prefix(metadata(0), &[]).unwrap();
    assert_eq!(empty.descriptor_count(), 2);
    assert_eq!(empty.event_packet_count(), 0);
    assert!(!empty.debug_packets_json().contains("\"kind\":\"event\""));

    let events = vec![start(
        1,
        10,
        scope(None, None, None, None),
        SpanStartDetail::ProductionStart(EmptyDetail::new()),
        None,
    )];
    let open = project_prefix(metadata(1), &events).unwrap();
    assert!(open.debug_packets_json().contains("slice_begin"));
    assert!(!open.debug_packets_json().contains("slice_end"));
}

#[test]
fn nested_spans_share_a_lane_but_non_lifo_overlap_uses_the_lowest_sibling_lane() {
    let root = scope(None, None, None, None);
    let nested = vec![
        start(
            1,
            10,
            root.clone(),
            SpanStartDetail::ProductionStart(EmptyDetail::new()),
            None,
        ),
        start(
            2,
            20,
            root.clone(),
            SpanStartDetail::ProductionStart(EmptyDetail::new()),
            Some(1),
        ),
        finish(3, 30, root.clone(), 2),
        finish(4, 40, root.clone(), 1),
    ];
    let nested_packets = project_prefix(metadata(4), &nested)
        .unwrap()
        .debug_packets_json();
    assert!(!nested_packets.contains("[lane 2]"));

    let overlap = vec![
        start(
            1,
            10,
            root.clone(),
            SpanStartDetail::ProductionStart(EmptyDetail::new()),
            None,
        ),
        start(
            2,
            20,
            root.clone(),
            SpanStartDetail::ProductionStart(EmptyDetail::new()),
            None,
        ),
        finish(3, 30, root.clone(), 1),
        finish(4, 40, root, 2),
    ];
    let first = project_prefix(metadata(4), &overlap).unwrap();
    let second = project_prefix(metadata(4), &overlap).unwrap();
    assert!(first.debug_packets_json().contains("[lane 2]"));
    assert_eq!(first.trace_bytes().unwrap(), second.trace_bytes().unwrap());
}

#[test]
fn equal_timestamps_keep_canonical_sequence_order() {
    let root = scope(None, None, None, None);
    let events = vec![
        start(
            1,
            10,
            root.clone(),
            SpanStartDetail::ProductionStart(EmptyDetail::new()),
            None,
        ),
        finish(2, 10, root, 1),
    ];
    let packets = project_prefix(metadata(2), &events)
        .unwrap()
        .debug_packets_json();
    let start = packets
        .find("\"troupe.event.sequence\",\"uint\":\"1\"")
        .unwrap();
    let finish = packets
        .find("\"troupe.event.sequence\",\"uint\":\"2\"")
        .unwrap();
    assert!(start < finish);
}

#[test]
fn timestamp_reference_and_identity_boundaries_fail_explicitly() {
    let root = scope(None, None, None, None);
    let too_late = vec![DiagnosticEvent::CounterSampled(CounterSampled::new(
        header(1, i64::MAX as u64 + 1, root.clone(), Vec::new()),
        CounterKind::CueActive,
        SchemaU64::new(1),
    ))];
    assert!(matches!(
        project_prefix(metadata(1), &too_late),
        Err(ProjectionError::TimestampOutOfRange { .. })
    ));

    let boundary = vec![DiagnosticEvent::CounterSampled(CounterSampled::new(
        header(1, i64::MAX as u64, root.clone(), Vec::new()),
        CounterKind::CueActive,
        SchemaU64::new(1),
    ))];
    project_prefix(metadata(1), &boundary).unwrap();

    let malformed = vec![finish(1, 10, root.clone(), 0)];
    assert!(matches!(
        project_prefix(metadata(1), &malformed),
        Err(ProjectionError::InvalidReference { .. })
    ));

    assert!(matches!(
        project_prefix_with_limits(metadata(0), &[], ProjectionLimits::new(1, u64::MAX)),
        Err(ProjectionError::IdentityExhausted { space: "track", .. })
    ));

    let flow_events = vec![
        DiagnosticEvent::CounterSampled(CounterSampled::new(
            header(1, 1, root.clone(), Vec::new()),
            CounterKind::CueActive,
            SchemaU64::new(1),
        )),
        DiagnosticEvent::CounterSampled(CounterSampled::new(
            header(
                2,
                2,
                root,
                vec![CausalLink::new(SchemaU64::new(1), CausalRelation::Return)],
            ),
            CounterKind::CueActive,
            SchemaU64::new(0),
        )),
    ];
    assert!(matches!(
        project_prefix_with_limits(
            metadata(2),
            &flow_events,
            ProjectionLimits::new(u64::MAX, 0),
        ),
        Err(ProjectionError::IdentityExhausted { space: "flow", .. })
    ));
}

#[test]
fn exact_large_doubles_and_non_exact_numbers_are_not_rounded() {
    let root = scope(None, None, None, None);
    let exact = CustomCounterSampled::new(
        header(1, 1, root.clone(), Vec::new()),
        "numbers.exact".to_owned(),
        CustomNumber::Integer(CanonicalInteger::parse("9223372036854775808").unwrap()),
        None,
        BTreeMap::new(),
    )
    .unwrap();
    let non_exact = CustomCounterSampled::new(
        header(2, 2, root, Vec::new()),
        "numbers.non_exact".to_owned(),
        CustomNumber::Integer(CanonicalInteger::parse("9223372036854775809").unwrap()),
        None,
        BTreeMap::new(),
    )
    .unwrap();
    let packets = project_prefix(
        metadata(2),
        &[
            DiagnosticEvent::CustomCounterSampled(exact),
            DiagnosticEvent::CustomCounterSampled(non_exact),
        ],
    )
    .unwrap()
    .debug_packets_json();
    assert!(packets.contains("\"double\":\"9223372036854776000\""));
    assert!(packets.contains("9223372036854775809"));
    assert!(packets.contains("troupe.counter_projection"));
    assert!(
        packets
            .contains("\"type\":\"instant\",\"track_uuid\":\"1\",\"name\":\"numbers.non_exact\"")
    );
}
