use troupe_diagnostics_core::{
    detail::{EmptyDetail, PlanEntry, SpanStartDetail},
    event::{
        ActTokenUsageFinalized, AffectedElapsedInterval, AgentMessageCompleted, AgentMessageDelta,
        AgentPlanSnapshot, CausalLink, CounterSampled, DiagnosticEvent, DiagnosticEventHeader,
        DiagnosticEventKind, DiagnosticScope, ObservationGap, SpanFinished, SpanStarted,
    },
    id::{CanonicalUuid, RunLocalId},
    kinds::{
        CausalRelation, CounterKind, PlanEntryPriority, PlanEntryStatus, SpanOutcome,
        UsageAvailability, UsageUnavailableReason,
    },
    scalar::SchemaU64,
    time::ElapsedNs,
};
use troupe_diagnostics_runtime::store::projector::{
    messages::{MessageIdentityField, MessageProjectionError},
    plans::PlanProjectionError,
    snapshot::{
        ProjectedTruncation, SnapshotProjectionError, SnapshotProjector, SnapshotReadModel,
        project_snapshot,
    },
    spans::SpanProjectionError,
    usage::UsageProjectionError,
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";

fn run_id() -> CanonicalUuid {
    CanonicalUuid::parse(RUN_ID).expect("canonical Run UUID")
}

fn local_id(value: &str) -> RunLocalId {
    RunLocalId::parse(value).expect("valid Run-local ID")
}

fn empty_scope() -> DiagnosticScope {
    DiagnosticScope::new(None, None, None, None, None, None, None)
}

fn act_scope(cue: &str, act: &str) -> DiagnosticScope {
    DiagnosticScope::new(
        Some(local_id("scene-1")),
        Some(local_id("actor-1")),
        Some(local_id(cue)),
        None,
        Some(local_id(act)),
        None,
        Some(SchemaU64::new(1)),
    )
}

fn header(
    sequence: u64,
    elapsed_ns: u64,
    scope: DiagnosticScope,
    caused_by: Vec<CausalLink>,
) -> DiagnosticEventHeader {
    DiagnosticEventHeader::new(
        run_id(),
        SchemaU64::new(sequence),
        ElapsedNs::new(elapsed_ns),
        scope,
        caused_by,
    )
    .expect("valid event header")
}

fn span_started(sequence: u64, elapsed_ns: u64) -> DiagnosticEvent {
    DiagnosticEvent::SpanStarted(SpanStarted::new(
        header(sequence, elapsed_ns, empty_scope(), Vec::new()),
        SpanStartDetail::RunLifecycle(EmptyDetail::new()),
        None,
    ))
}

fn span_finished(sequence: u64, elapsed_ns: u64, span_id: u64) -> DiagnosticEvent {
    DiagnosticEvent::SpanFinished(SpanFinished::new(
        header(sequence, elapsed_ns, empty_scope(), Vec::new()),
        SchemaU64::new(span_id),
        SpanOutcome::Completed,
        None,
    ))
}

fn message_delta(
    sequence: u64,
    elapsed_ns: u64,
    scope: DiagnosticScope,
    message_id: &str,
) -> DiagnosticEvent {
    DiagnosticEvent::AgentMessageDelta(AgentMessageDelta::new(
        header(sequence, elapsed_ns, scope, Vec::new()),
        local_id(message_id),
        None,
        "hello".to_owned(),
    ))
}

fn message_completed(
    sequence: u64,
    elapsed_ns: u64,
    scope: DiagnosticScope,
    message_id: &str,
    truncated: bool,
) -> DiagnosticEvent {
    DiagnosticEvent::AgentMessageCompleted(AgentMessageCompleted::new(
        header(sequence, elapsed_ns, scope, Vec::new()),
        local_id(message_id),
        SchemaU64::new(5),
        SchemaU64::new(5),
        truncated,
    ))
}

fn plan_snapshot(
    sequence: u64,
    elapsed_ns: u64,
    scope: DiagnosticScope,
    truncated: bool,
) -> DiagnosticEvent {
    DiagnosticEvent::AgentPlanSnapshot(AgentPlanSnapshot::new(
        header(sequence, elapsed_ns, scope, Vec::new()),
        vec![PlanEntry::new(
            "inspect".to_owned(),
            PlanEntryPriority::High,
            PlanEntryStatus::InProgress,
        )],
        truncated,
    ))
}

fn counter(sequence: u64, elapsed_ns: u64, scope: DiagnosticScope) -> DiagnosticEvent {
    DiagnosticEvent::CounterSampled(CounterSampled::new(
        header(sequence, elapsed_ns, scope, Vec::new()),
        CounterKind::AgentTurnActive,
        SchemaU64::new(u64::MAX),
    ))
}

fn unavailable_usage(sequence: u64, elapsed_ns: u64, scope: DiagnosticScope) -> DiagnosticEvent {
    DiagnosticEvent::ActTokenUsageFinalized(
        ActTokenUsageFinalized::new(
            header(sequence, elapsed_ns, scope, Vec::new()),
            UsageAvailability::Unavailable,
            None,
            Some(UsageUnavailableReason::UsageNotReported),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("valid unavailable usage"),
    )
}

fn gap(sequence: u64, elapsed_ns: u64, scope: DiagnosticScope) -> DiagnosticEvent {
    DiagnosticEvent::ObservationGap(ObservationGap::new(
        header(
            sequence,
            elapsed_ns,
            scope.clone(),
            vec![CausalLink::new(
                SchemaU64::new(2),
                CausalRelation::FollowsFrom,
            )],
        ),
        "acp-normalizer".to_owned(),
        Some("message-stream".to_owned()),
        "provider_sequence_gap".to_owned(),
        Some(SchemaU64::new(0)),
        Some(AffectedElapsedInterval::new(
            ElapsedNs::new(15),
            ElapsedNs::new(55),
        )),
        Some(DiagnosticEventKind::AgentMessageDelta),
        Some(scope),
    ))
}

fn interleaved_events() -> Vec<DiagnosticEvent> {
    let scope = act_scope("cue-1", "act-1");
    vec![
        span_started(1, 10),
        message_delta(2, 20, scope.clone(), "message-1"),
        plan_snapshot(3, 30, scope.clone(), false),
        counter(4, 25, scope.clone()),
        unavailable_usage(5, 50, scope.clone()),
        gap(6, 60, scope.clone()),
        message_completed(7, 70, scope.clone(), "message-1", true),
        plan_snapshot(8, 80, scope, true),
        span_finished(9, 90, 1),
    ]
}

#[test]
fn empty_snapshot_is_a_complete_zero_watermark_join() {
    let model = SnapshotProjector::new(run_id()).into_model();

    assert_eq!(model.model_schema_version(), 1);
    assert_eq!(model.run_id(), run_id());
    assert_eq!(model.through_sequence().get(), 0);
    assert_eq!(model.through_elapsed_ns().get(), 0);
    assert!(model.spans().spans().is_empty());
    assert!(model.messages().messages().is_empty());
    assert!(model.plans().plans().is_empty());
    assert!(model.counters().series().is_empty());
    assert_eq!(model.usage().aggregate().finalized_acts().get(), 0);
    assert!(model.gaps().is_empty());
    assert!(model.truncations().is_empty());
}

#[test]
fn canonical_gaps_and_only_explicit_resource_truncations_are_projected() {
    let events = interleaved_events();
    let model = project_snapshot(run_id(), &events).expect("project interleaved snapshot");

    assert_eq!(model.through_sequence().get(), 9);
    assert_eq!(model.through_elapsed_ns().get(), 90);
    for child_watermark in [
        model.spans().through_sequence(),
        model.messages().through_sequence(),
        model.plans().through_sequence(),
        model.counters().through_sequence(),
        model.usage().through_sequence(),
    ] {
        assert_eq!(child_watermark, model.through_sequence());
    }
    assert_eq!(model.gaps().len(), 1);
    let DiagnosticEvent::ObservationGap(expected_gap) = &events[5] else {
        panic!("gap fixture")
    };
    assert_eq!(&model.gaps()[0], expected_gap);
    assert_eq!(model.gaps()[0].producer(), "acp-normalizer");
    assert_eq!(model.gaps()[0].dropped_count().map(SchemaU64::get), Some(0));

    assert_eq!(model.truncations().len(), 2);
    assert_eq!(model.truncations()[0].sequence().get(), 7);
    assert_eq!(
        model.truncations()[0].message_id(),
        Some(&local_id("message-1"))
    );
    assert!(matches!(
        model.truncations()[0],
        ProjectedTruncation::AgentMessage { .. }
    ));
    assert_eq!(model.truncations()[1].sequence().get(), 8);
    assert!(matches!(
        model.truncations()[1],
        ProjectedTruncation::AgentPlan { .. }
    ));
    assert_eq!(model.spans().open_spans().count(), 0);
}

#[test]
fn c03_fixture_streams_rebuild_from_events_byte_exactly() {
    let fixtures: &[(&str, &[u8])] = &[
        (
            "message-completed",
            include_bytes!(
                "../../../../tests/fixtures/diagnostics/events/agent-message-completed.json"
            ),
        ),
        (
            "message-delta",
            include_bytes!(
                "../../../../tests/fixtures/diagnostics/events/agent-message-delta.json"
            ),
        ),
        (
            "plan",
            include_bytes!(
                "../../../../tests/fixtures/diagnostics/events/agent-plan-snapshot.json"
            ),
        ),
        (
            "counter",
            include_bytes!("../../../../tests/fixtures/diagnostics/events/counter-sampled.json"),
        ),
        (
            "custom-counter",
            include_bytes!(
                "../../../../tests/fixtures/diagnostics/events/custom-counter-sampled.json"
            ),
        ),
        (
            "gap",
            include_bytes!("../../../../tests/fixtures/diagnostics/events/observation-gap.json"),
        ),
        (
            "nested-overlap",
            include_bytes!("../../../../tests/fixtures/diagnostics/events/nested-overlap.json"),
        ),
    ];

    for (name, bytes) in fixtures {
        let events: Vec<DiagnosticEvent> =
            serde_json::from_slice(bytes).expect("parse C03 fixture");
        let mut materialized = SnapshotProjector::new(run_id());
        for event in &events {
            materialized
                .apply(event)
                .unwrap_or_else(|error| panic!("incrementally project {name}: {error}"));
        }
        let expected = materialized
            .model()
            .canonical_json()
            .expect("encode expected snapshot");
        drop(materialized);

        let rebuilt = project_snapshot(run_id(), &events)
            .unwrap_or_else(|error| panic!("rebuild {name}: {error}"));
        assert_eq!(
            rebuilt.canonical_json().expect("encode rebuild"),
            expected,
            "{name}"
        );
        let decoded: SnapshotReadModel =
            serde_json::from_slice(&expected).expect("decode materialized snapshot");
        assert_eq!(decoded, rebuilt, "{name}");
    }
}

#[test]
fn every_interleaved_prefix_matches_a_fresh_full_replay() {
    let events = interleaved_events();
    let mut incremental = SnapshotProjector::new(run_id());

    for end in 0..=events.len() {
        if end > 0 {
            incremental
                .apply(&events[end - 1])
                .expect("incremental event");
        }
        let full = project_snapshot(run_id(), &events[..end]).expect("full prefix replay");
        assert_eq!(incremental.model(), &full, "prefix {end}");
        assert_eq!(
            incremental
                .model()
                .canonical_json()
                .expect("incremental bytes"),
            full.canonical_json().expect("full bytes"),
            "prefix {end}"
        );
    }
}

#[test]
fn message_child_error_is_preserved_without_partial_state_or_sequence_commit() {
    let first_scope = act_scope("cue-1", "act-1");
    let changed_scope = act_scope("cue-2", "act-2");
    let mut projector = SnapshotProjector::new(run_id());
    projector
        .apply(&message_delta(1, 10, first_scope.clone(), "message-1"))
        .expect("first message delta");
    let before = projector
        .model()
        .canonical_json()
        .expect("state before error");

    let error = projector
        .apply(&message_delta(2, 20, changed_scope, "message-1"))
        .expect_err("scope drift must fail");
    assert_eq!(
        error,
        SnapshotProjectionError::Message(MessageProjectionError::IdentityMismatch {
            message_id: local_id("message-1"),
            event_sequence: SchemaU64::new(2),
            field: MessageIdentityField::Scope,
        })
    );
    assert_eq!(error.code(), "message_identity_mismatch");
    assert_eq!(
        projector
            .model()
            .canonical_json()
            .expect("state after error"),
        before
    );

    projector
        .apply(&counter(2, 20, first_scope))
        .expect("failed child did not consume sequence");
    assert_eq!(projector.model().through_sequence().get(), 2);
}

#[test]
fn plan_and_usage_child_errors_are_preserved_and_atomic() {
    let first_scope = act_scope("cue-1", "act-1");
    let changed_scope = act_scope("cue-2", "act-1");
    let mut plans = SnapshotProjector::new(run_id());
    plans
        .apply(&plan_snapshot(1, 10, first_scope.clone(), false))
        .expect("first plan");
    let before = plans.model().clone();
    let error = plans
        .apply(&plan_snapshot(2, 20, changed_scope, false))
        .expect_err("same Act cannot change exact scope");
    assert_eq!(
        error,
        SnapshotProjectionError::Plan(PlanProjectionError::ScopeMismatch {
            act_id: local_id("act-1"),
            event_sequence: SchemaU64::new(2),
        })
    );
    assert_eq!(plans.model(), &before);
    plans
        .apply(&counter(2, 20, first_scope))
        .expect("plan failure did not consume sequence");

    let mut usage = SnapshotProjector::new(run_id());
    let missing_act = DiagnosticScope::new(
        Some(local_id("scene-1")),
        Some(local_id("actor-1")),
        Some(local_id("cue-1")),
        None,
        None,
        None,
        Some(SchemaU64::new(1)),
    );
    let error = usage
        .apply(&unavailable_usage(1, 10, missing_act))
        .expect_err("terminal usage requires an Act identity");
    assert_eq!(
        error,
        SnapshotProjectionError::Usage(UsageProjectionError::MissingActIdentity {
            event_sequence: SchemaU64::new(1),
        })
    );
    assert_eq!(usage.model().through_sequence().get(), 0);
    usage
        .apply(&counter(1, 10, act_scope("cue-1", "act-1")))
        .expect("usage failure did not consume sequence");
}

#[test]
fn position_and_reference_errors_leave_the_whole_join_retryable() {
    let scope = act_scope("cue-1", "act-1");
    let mut projector = SnapshotProjector::new(run_id());
    let error = projector
        .apply(&counter(2, 20, scope.clone()))
        .expect_err("skipped sequence");
    assert_eq!(
        error,
        SnapshotProjectionError::Span(SpanProjectionError::NonCanonicalSequence {
            expected: SchemaU64::new(1),
            actual: SchemaU64::new(2),
        })
    );
    assert_eq!(projector.model().through_sequence().get(), 0);

    let forward = DiagnosticEvent::CounterSampled(CounterSampled::new(
        header(
            1,
            10,
            scope.clone(),
            vec![CausalLink::new(
                SchemaU64::new(2),
                CausalRelation::FollowsFrom,
            )],
        ),
        CounterKind::AgentTurnActive,
        SchemaU64::new(1),
    ));
    let error = projector.apply(&forward).expect_err("forward reference");
    assert_eq!(error.code(), "forward_link");
    assert!(matches!(
        error,
        SnapshotProjectionError::InvalidReference(_)
    ));
    assert_eq!(projector.model().through_sequence().get(), 0);

    projector
        .apply(&counter(1, 10, scope))
        .expect("reference failure did not consume sequence");
    assert_eq!(projector.model().through_sequence().get(), 1);
}
