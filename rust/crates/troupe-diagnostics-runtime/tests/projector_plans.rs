use serde_json::json;
use troupe_diagnostics_core::{
    detail::PlanEntry,
    event::{
        AgentPlanSnapshot, CausalLink, CounterSampled, DiagnosticEvent, DiagnosticEventHeader,
        DiagnosticScope,
    },
    id::{CanonicalUuid, RunLocalId},
    kinds::{CausalRelation, CounterKind, PlanEntryPriority, PlanEntryStatus},
    scalar::SchemaU64,
    time::ElapsedNs,
};
use troupe_diagnostics_runtime::store::projector::plans::{
    PlanProjectionError, PlanProjector, PlanReadModel, project_plans,
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
const OTHER_RUN_ID: &str = "87654321-4321-4321-8321-cba987654321";
const PLAN_FIXTURE: &[u8] =
    include_bytes!("../../../../tests/fixtures/diagnostics/events/agent-plan-snapshot.json");

fn run_id() -> CanonicalUuid {
    CanonicalUuid::parse(RUN_ID).expect("canonical Run UUID")
}

fn other_run_id() -> CanonicalUuid {
    CanonicalUuid::parse(OTHER_RUN_ID).expect("canonical alternate Run UUID")
}

fn local_id(value: &str) -> RunLocalId {
    RunLocalId::parse(value).expect("valid Run-local ID")
}

fn scope(actor: &str, cue: &str, act: &str) -> DiagnosticScope {
    DiagnosticScope::new(
        Some(local_id("scene-1")),
        Some(local_id(actor)),
        Some(local_id(cue)),
        None,
        Some(local_id(act)),
        None,
        Some(SchemaU64::new(1)),
    )
}

fn header_for(
    run_id: CanonicalUuid,
    sequence: u64,
    elapsed_ns: u64,
    scope: DiagnosticScope,
    caused_by: Vec<CausalLink>,
) -> DiagnosticEventHeader {
    DiagnosticEventHeader::new(
        run_id,
        SchemaU64::new(sequence),
        ElapsedNs::new(elapsed_ns),
        scope,
        caused_by,
    )
    .expect("valid event header")
}

fn entry(content: &str, priority: PlanEntryPriority, status: PlanEntryStatus) -> PlanEntry {
    PlanEntry::new(content.to_owned(), priority, status)
}

fn snapshot(
    sequence: u64,
    elapsed_ns: u64,
    scope: DiagnosticScope,
    entries: Vec<PlanEntry>,
    truncated: bool,
) -> DiagnosticEvent {
    DiagnosticEvent::AgentPlanSnapshot(AgentPlanSnapshot::new(
        header_for(run_id(), sequence, elapsed_ns, scope, Vec::new()),
        entries,
        truncated,
    ))
}

fn counter(sequence: u64, elapsed_ns: u64, scope: DiagnosticScope) -> DiagnosticEvent {
    DiagnosticEvent::CounterSampled(CounterSampled::new(
        header_for(run_id(), sequence, elapsed_ns, scope, Vec::new()),
        CounterKind::AgentTurnActive,
        SchemaU64::new(1),
    ))
}

fn fixture_events() -> Vec<DiagnosticEvent> {
    serde_json::from_slice(PLAN_FIXTURE).expect("parse plan fixture")
}

fn interleaved_events() -> Vec<DiagnosticEvent> {
    let first = scope("actor-1", "cue-1", "act-1");
    let second = scope("actor-1", "cue-2", "act-2");
    vec![
        snapshot(
            1,
            10,
            first.clone(),
            vec![entry(
                "inspect",
                PlanEntryPriority::High,
                PlanEntryStatus::InProgress,
            )],
            false,
        ),
        snapshot(
            2,
            20,
            second.clone(),
            vec![entry(
                "other",
                PlanEntryPriority::Low,
                PlanEntryStatus::Pending,
            )],
            false,
        ),
        counter(3, 15, first.clone()),
        snapshot(
            4,
            40,
            first,
            vec![
                entry(
                    "report",
                    PlanEntryPriority::Medium,
                    PlanEntryStatus::Pending,
                ),
                entry(
                    "inspect",
                    PlanEntryPriority::High,
                    PlanEntryStatus::Completed,
                ),
            ],
            true,
        ),
        snapshot(5, 50, second, Vec::new(), false),
    ]
}

#[test]
fn frozen_fixture_preserves_present_empty_and_truncated_instead_of_null() {
    let events = fixture_events();
    let mut projector = PlanProjector::new(run_id());
    assert!(
        projector
            .model()
            .plan_for_scope(&DiagnosticScope::new(
                None, None, None, None, None, None, None
            ))
            .is_none(),
        "no snapshot is null/absent rather than an invented empty plan"
    );
    projector.apply(&events[0]).expect("first fixture snapshot");
    let present = projector.model().plans().first().expect("present plan");
    assert_eq!(present.entries().len(), 3);
    assert!(!present.truncated());
    projector.apply(&events[1]).expect("empty replacement");

    let model = projector.into_model();
    assert_eq!(model.model_schema_version(), 1);
    assert_eq!(model.run_id(), run_id());
    assert_eq!(model.through_sequence().get(), 2);
    assert_eq!(model.through_elapsed_ns().get(), 20);
    assert_eq!(model.plans().len(), 1);
    let empty = &model.plans()[0];
    assert!(empty.entries().is_empty());
    assert!(empty.truncated());
    assert_eq!(empty.sequence().get(), 2);
    assert_eq!(empty.elapsed_ns().get(), 20);
}

#[test]
fn keeps_only_the_latest_typed_snapshot_for_each_exact_scope() {
    let first_scope = scope("actor-1", "cue-1", "act-1");
    let second_scope = scope("actor-1", "cue-2", "act-2");
    let model = project_plans(run_id(), &interleaved_events()).expect("project plan state");

    assert_eq!(model.plans().len(), 2);
    assert_eq!(model.through_sequence().get(), 5);
    assert_eq!(model.through_elapsed_ns().get(), 50);
    let first = model.plan_for_scope(&first_scope).expect("first Act plan");
    assert_eq!(first.sequence().get(), 4);
    assert_eq!(first.entries().len(), 2);
    assert_eq!(first.entries()[0].content(), "report");
    assert_eq!(first.entries()[0].priority(), PlanEntryPriority::Medium);
    assert_eq!(first.entries()[0].status(), PlanEntryStatus::Pending);
    assert_eq!(first.entries()[1].status(), PlanEntryStatus::Completed);
    assert!(first.truncated());
    assert!(first.caused_by().is_empty());
    assert_eq!(first.run_id(), run_id());
    assert_eq!(
        first.scope_key().expect("canonical scope key"),
        serde_json::to_string(&first_scope).expect("encode scope")
    );
    let second = model
        .plan_for_act(&local_id("act-2"))
        .expect("second Act plan");
    assert_eq!(second.scope(), &second_scope);
    assert_eq!(second.sequence().get(), 5);
    assert!(second.entries().is_empty());
    assert!(!second.truncated());
}

#[test]
fn incremental_and_full_replay_are_byte_identical() {
    let events = interleaved_events();
    let full = project_plans(run_id(), &events).expect("full projection");
    let mut incremental = PlanProjector::new(run_id());
    incremental
        .apply_all(&events[..2])
        .expect("first incremental batch");
    incremental
        .apply_all(&events[2..4])
        .expect("second incremental batch");
    incremental
        .apply_all(&events[4..])
        .expect("last incremental batch");
    let incremental = incremental.into_model();

    assert_eq!(incremental, full);
    assert_eq!(
        incremental.canonical_json().expect("encode incremental"),
        full.canonical_json().expect("encode full")
    );
    let decoded: PlanReadModel =
        serde_json::from_slice(&full.canonical_json().expect("encode read model"))
            .expect("decode read model");
    assert_eq!(decoded, full);
}

#[test]
fn scope_drift_for_the_same_act_fails_without_consuming_sequence() {
    let original = scope("actor-1", "cue-1", "act-1");
    let drifted = scope("actor-1", "cue-2", "act-1");
    let mut projector = PlanProjector::new(run_id());
    projector
        .apply(&snapshot(
            1,
            10,
            original.clone(),
            vec![entry(
                "one",
                PlanEntryPriority::High,
                PlanEntryStatus::Pending,
            )],
            false,
        ))
        .expect("initial plan");
    let before = projector
        .model()
        .canonical_json()
        .expect("state before error");
    let error = projector
        .apply(&snapshot(
            2,
            20,
            drifted,
            vec![entry(
                "two",
                PlanEntryPriority::Low,
                PlanEntryStatus::Pending,
            )],
            false,
        ))
        .expect_err("same Act cannot change scope");
    assert_eq!(error.code(), "scope_mismatch");
    assert_eq!(
        error,
        PlanProjectionError::ScopeMismatch {
            act_id: local_id("act-1"),
            event_sequence: SchemaU64::new(2),
        }
    );
    assert_eq!(
        projector
            .model()
            .canonical_json()
            .expect("state after error"),
        before
    );
    projector
        .apply(&snapshot(2, 20, original, Vec::new(), false))
        .expect("rejected snapshot did not consume sequence");
}

#[test]
fn typed_empty_content_is_preserved_but_malformed_wire_fails_explicitly() {
    let plan_scope = scope("actor-1", "cue-1", "act-1");
    let mut projector = PlanProjector::new(run_id());
    projector
        .apply(&snapshot(
            1,
            10,
            plan_scope.clone(),
            vec![entry("", PlanEntryPriority::High, PlanEntryStatus::Pending)],
            false,
        ))
        .expect("ACP permits an empty content string");
    assert_eq!(projector.model().plans()[0].entries()[0].content(), "");

    let mut malformed = serde_json::to_value(snapshot(
        2,
        20,
        plan_scope,
        vec![entry(
            "typed",
            PlanEntryPriority::High,
            PlanEntryStatus::Pending,
        )],
        false,
    ))
    .expect("encode plan event");
    malformed["entries"][0]["status"] = json!("future");
    assert!(
        serde_json::from_value::<DiagnosticEvent>(malformed).is_err(),
        "unknown closed plan state must fail before projection"
    );
}

#[test]
fn out_of_order_cross_run_and_reference_errors_do_not_advance_state() {
    let plan_scope = scope("actor-1", "cue-1", "act-1");
    let skipped = snapshot(2, 20, plan_scope.clone(), Vec::new(), false);
    let error = project_plans(run_id(), &[skipped]).expect_err("reject skipped sequence");
    assert_eq!(error.code(), "noncanonical_sequence");

    let cross_run = DiagnosticEvent::AgentPlanSnapshot(AgentPlanSnapshot::new(
        header_for(other_run_id(), 1, 10, plan_scope.clone(), Vec::new()),
        Vec::new(),
        false,
    ));
    let error = project_plans(run_id(), &[cross_run]).expect_err("reject another Run");
    assert_eq!(error.code(), "cross_run");

    let forward = DiagnosticEvent::AgentPlanSnapshot(AgentPlanSnapshot::new(
        header_for(
            run_id(),
            1,
            10,
            plan_scope.clone(),
            vec![CausalLink::new(
                SchemaU64::new(2),
                CausalRelation::FollowsFrom,
            )],
        ),
        Vec::new(),
        false,
    ));
    let mut projector = PlanProjector::new(run_id());
    let error = projector
        .apply(&forward)
        .expect_err("reject forward causal link");
    assert_eq!(error.code(), "forward_link");
    assert_eq!(projector.model().through_sequence().get(), 0);
    projector
        .apply(&snapshot(1, 10, plan_scope.clone(), Vec::new(), false))
        .expect("reference rejection did not consume sequence");
    projector
        .apply(&counter(2, 9, plan_scope))
        .expect("other facts only advance the canonical prefix");
    assert_eq!(projector.model().plans().len(), 1);
    assert_eq!(projector.model().through_sequence().get(), 2);
    assert_eq!(projector.model().through_elapsed_ns().get(), 10);
}
