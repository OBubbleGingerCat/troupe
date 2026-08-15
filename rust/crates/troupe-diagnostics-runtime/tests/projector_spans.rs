use serde::Deserialize;
use serde_json::Value;
use troupe_diagnostics_core::{
    detail::{ActorDetail, AgentSessionDetail, EmptyDetail, InstantDetail, SpanStartDetail},
    event::{
        CausalLink, DiagnosticEvent, DiagnosticEventHeader, DiagnosticScope, InstantOccurred,
        SpanFinished, SpanStarted,
    },
    id::{CanonicalUuid, RunLocalId},
    kinds::{CausalRelation, SpanKind, SpanOutcome},
    scalar::SchemaU64,
    time::ElapsedNs,
};
use troupe_diagnostics_runtime::store::projector::spans::{
    ProjectedSpanFamily, SpanProjector, SpanReadModel, project_spans,
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
const NESTED_OVERLAP: &[u8] =
    include_bytes!("../../../../tests/fixtures/diagnostics/events/nested-overlap.json");
const MESSAGE_EVENTS: &[u8] =
    include_bytes!("../../../../tests/fixtures/diagnostics/events/agent-message-delta.json");
const MALFORMED_REFERENCES: &[&[u8]] = &[
    include_bytes!(
        "../../../../tests/fixtures/diagnostics/reference-validation/child-outside-parent.json"
    ),
    include_bytes!("../../../../tests/fixtures/diagnostics/reference-validation/cross-run.json"),
    include_bytes!(
        "../../../../tests/fixtures/diagnostics/reference-validation/double-finish.json"
    ),
    include_bytes!(
        "../../../../tests/fixtures/diagnostics/reference-validation/finish-before-start.json"
    ),
    include_bytes!("../../../../tests/fixtures/diagnostics/reference-validation/forward-link.json"),
    include_bytes!(
        "../../../../tests/fixtures/diagnostics/reference-validation/kind-mismatch.json"
    ),
    include_bytes!("../../../../tests/fixtures/diagnostics/reference-validation/self-link.json"),
];

#[derive(Deserialize)]
struct MalformedReferenceFixture {
    expected_code: String,
    events: Vec<DiagnosticEvent>,
}

fn run_id() -> CanonicalUuid {
    CanonicalUuid::parse(RUN_ID).expect("canonical test Run UUID")
}

fn fixture_events(bytes: &[u8]) -> Vec<DiagnosticEvent> {
    serde_json::from_slice(bytes).expect("parse diagnostic event fixture")
}

fn local_id(value: &str) -> RunLocalId {
    RunLocalId::parse(value).expect("valid Run-local ID")
}

fn scope(cue_id: Option<&str>, act_id: Option<&str>) -> DiagnosticScope {
    DiagnosticScope::new(
        Some(local_id("scene-1")),
        Some(local_id("actor-1")),
        cue_id.map(local_id),
        None,
        act_id.map(local_id),
        None,
        None,
    )
}

fn header(
    sequence: u64,
    scope: DiagnosticScope,
    caused_by: Vec<CausalLink>,
) -> DiagnosticEventHeader {
    DiagnosticEventHeader::new(
        run_id(),
        SchemaU64::new(sequence),
        ElapsedNs::new(sequence * 10),
        scope,
        caused_by,
    )
    .expect("valid event header")
}

fn started(
    sequence: u64,
    scope: DiagnosticScope,
    detail: SpanStartDetail,
    parent_span_id: Option<u64>,
    caused_by: Vec<CausalLink>,
) -> DiagnosticEvent {
    DiagnosticEvent::SpanStarted(SpanStarted::new(
        header(sequence, scope, caused_by),
        detail,
        parent_span_id.map(SchemaU64::new),
    ))
}

fn finished(
    sequence: u64,
    scope: DiagnosticScope,
    span_id: u64,
    outcome: SpanOutcome,
    error_code: Option<&str>,
) -> DiagnosticEvent {
    DiagnosticEvent::SpanFinished(SpanFinished::new(
        header(sequence, scope, Vec::new()),
        SchemaU64::new(span_id),
        outcome,
        error_code.map(str::to_owned),
    ))
}

fn caller_and_multiple_cue_events() -> Vec<DiagnosticEvent> {
    let actor_scope = scope(None, None);
    let cue_one_scope = scope(Some("cue-1"), None);
    let act_one_scope = scope(Some("cue-1"), Some("act-1"));
    let cue_two_scope = scope(Some("cue-2"), None);
    let act_two_scope = scope(Some("cue-2"), Some("act-2"));
    let session_detail = || {
        AgentSessionDetail::new(
            "codex".to_owned(),
            Some("gpt-5".to_owned()),
            Some("high".to_owned()),
        )
    };

    vec![
        started(
            1,
            actor_scope,
            SpanStartDetail::ActorHandleLifetime(ActorDetail::new(
                "Worker".to_owned(),
                "Worker".to_owned(),
            )),
            None,
            Vec::new(),
        ),
        started(
            2,
            cue_one_scope.clone(),
            SpanStartDetail::CueExecution(EmptyDetail::new()),
            Some(1),
            Vec::new(),
        ),
        started(
            3,
            act_one_scope.clone(),
            SpanStartDetail::ActLifecycle(session_detail()),
            Some(2),
            Vec::new(),
        ),
        started(
            4,
            act_one_scope.clone(),
            SpanStartDetail::ActCaller(EmptyDetail::new()),
            Some(3),
            Vec::new(),
        ),
        DiagnosticEvent::InstantOccurred(InstantOccurred::new(
            header(5, act_one_scope.clone(), Vec::new()),
            InstantDetail::ActPromptSubmitted(EmptyDetail::new()),
            Some(SchemaU64::new(3)),
        )),
        started(
            6,
            act_one_scope.clone(),
            SpanStartDetail::AgentTurn(session_detail()),
            Some(3),
            vec![CausalLink::new(SchemaU64::new(5), CausalRelation::Dispatch)],
        ),
        finished(
            7,
            act_one_scope.clone(),
            4,
            SpanOutcome::Cancelled,
            Some("caller_cancelled"),
        ),
        finished(8, act_one_scope.clone(), 6, SpanOutcome::Completed, None),
        finished(9, act_one_scope, 3, SpanOutcome::Completed, None),
        finished(10, cue_one_scope, 2, SpanOutcome::Completed, None),
        started(
            11,
            cue_two_scope,
            SpanStartDetail::CueExecution(EmptyDetail::new()),
            Some(1),
            Vec::new(),
        ),
        started(
            12,
            act_two_scope,
            SpanStartDetail::ActLifecycle(session_detail()),
            Some(11),
            Vec::new(),
        ),
    ]
}

#[test]
fn projects_open_nested_completed_and_non_nested_overlap_without_invention() {
    let events = fixture_events(NESTED_OVERLAP);
    let model = project_spans(run_id(), &events).expect("project valid nested fixture");

    assert_eq!(model.model_schema_version(), 1);
    assert_eq!(model.run_id(), run_id());
    assert_eq!(model.through_sequence().get(), 9);
    assert_eq!(model.through_elapsed_ns().get(), 55);
    assert_eq!(model.spans().len(), 4);
    assert_eq!(
        model
            .open_spans()
            .map(|span| span.span_id().get())
            .collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(
        model
            .completed_spans()
            .map(|span| span.span_id().get())
            .collect::<Vec<_>>(),
        vec![2, 3, 4]
    );
    assert_eq!(
        model
            .roots()
            .map(|span| span.span_id().get())
            .collect::<Vec<_>>(),
        vec![1, 3],
        "the overlapping Scene span must remain a sibling, not an invented child"
    );
    assert_eq!(
        model
            .children_of(SchemaU64::new(1))
            .map(|span| span.span_id().get())
            .collect::<Vec<_>>(),
        vec![2]
    );
    assert_eq!(
        model
            .children_of(SchemaU64::new(2))
            .map(|span| span.span_id().get())
            .collect::<Vec<_>>(),
        vec![4]
    );

    let actor = model.span(SchemaU64::new(2)).expect("actor span");
    assert_eq!(
        actor.definition().built_in_kind(),
        Some(SpanKind::ActorHandleLifetime)
    );
    assert_eq!(
        actor.elapsed_duration_ns().map(|value| value.get()),
        Some(30)
    );
    assert_eq!(actor.latest_sequence().get(), 8);
    let custom = model.span(SchemaU64::new(4)).expect("custom span");
    assert_eq!(custom.definition().family(), ProjectedSpanFamily::Custom);
    assert_eq!(custom.definition().custom_name(), Some("example.nested"));
    assert_eq!(
        custom.elapsed_duration_ns().map(|value| value.get()),
        Some(10)
    );

    let canonical = model.canonical_json().expect("encode read model");
    let decoded: SpanReadModel = serde_json::from_slice(&canonical).expect("decode read model");
    assert_eq!(decoded, model);
}

#[test]
fn incremental_and_full_replay_are_byte_identical() {
    let events = fixture_events(NESTED_OVERLAP);
    let full = project_spans(run_id(), &events).expect("full projection");

    let mut incremental = SpanProjector::new(run_id());
    incremental
        .apply_all(&events[..3])
        .expect("first incremental batch");
    incremental
        .apply_all(&events[3..7])
        .expect("second incremental batch");
    incremental
        .apply_all(&events[7..])
        .expect("final incremental batch");
    let incremental = incremental.into_model();

    assert_eq!(incremental, full);
    assert_eq!(
        incremental.canonical_json().expect("encode incremental"),
        full.canonical_json().expect("encode full")
    );
}

#[test]
fn caller_remote_turn_and_multiple_cues_remain_distinct() {
    let events = caller_and_multiple_cue_events();
    let model = project_spans(run_id(), &events).expect("project multiple Cue flow");

    assert_eq!(model.spans().len(), 7);
    assert_eq!(model.open_spans().count(), 3);
    assert_eq!(model.completed_spans().count(), 4);
    assert_eq!(
        model
            .children_of(SchemaU64::new(3))
            .map(|span| span.span_id().get())
            .collect::<Vec<_>>(),
        vec![4, 6],
        "caller and remote turn are siblings under Act lifecycle"
    );
    assert_eq!(
        model
            .causal_successors_of(SchemaU64::new(5))
            .map(|span| span.span_id().get())
            .collect::<Vec<_>>(),
        vec![6]
    );

    let caller = model.span(SchemaU64::new(4)).expect("caller span");
    let turn = model.span(SchemaU64::new(6)).expect("remote turn span");
    assert_eq!(
        caller.definition().built_in_kind(),
        Some(SpanKind::ActCaller)
    );
    assert_eq!(turn.definition().built_in_kind(), Some(SpanKind::AgentTurn));
    assert_eq!(
        caller.completion().expect("caller completion").outcome(),
        SpanOutcome::Cancelled
    );
    assert_eq!(
        caller.completion().expect("caller completion").error_code(),
        Some("caller_cancelled")
    );
    assert_eq!(
        turn.completion().expect("turn completion").outcome(),
        SpanOutcome::Completed
    );

    let cue_one = scope(Some("cue-1"), None);
    let cue_two = scope(Some("cue-2"), None);
    assert_eq!(model.spans_in_scope(&cue_one).count(), 1);
    assert_eq!(model.spans_within_scope(&cue_one).count(), 4);
    assert_eq!(model.spans_within_scope(&cue_two).count(), 2);
    assert_eq!(model.spans_within_scope(&scope(None, None)).count(), 7);
    assert!(
        model.spans_within_scope(&cue_one).all(|span| span
            .scope()
            .cue_id()
            .map(|value| value.as_str())
            == Some("cue-1"))
    );
    assert!(
        model.spans_within_scope(&cue_two).all(|span| span
            .scope()
            .cue_id()
            .map(|value| value.as_str())
            == Some("cue-2"))
    );
}

#[test]
fn ignores_other_fact_families_while_advancing_the_canonical_prefix() {
    let events = fixture_events(MESSAGE_EVENTS);
    let model = project_spans(run_id(), &events).expect("project message-only prefix");

    assert!(model.spans().is_empty());
    assert_eq!(model.through_sequence().get(), 2);
    assert_eq!(model.through_elapsed_ns().get(), 20);
}

#[test]
fn malformed_references_return_the_frozen_reference_code_without_partial_commit() {
    for bytes in MALFORMED_REFERENCES {
        let fixture: MalformedReferenceFixture =
            serde_json::from_slice(bytes).expect("parse malformed reference fixture");
        let mut projector = SpanProjector::new(run_id());
        let mut observed = None;
        for event in &fixture.events {
            match projector.apply(event) {
                Ok(()) => {}
                Err(error) => {
                    observed = Some(error.code());
                    break;
                }
            }
        }
        assert_eq!(
            observed,
            Some(fixture.expected_code.as_str()),
            "wrong projection error for fixture {}",
            fixture.expected_code
        );
    }

    let events = fixture_events(NESTED_OVERLAP);
    let mut projector = SpanProjector::new(run_id());
    projector.apply(&events[0]).expect("apply first event");
    let before = projector
        .model()
        .canonical_json()
        .expect("encode state before error");
    let error = projector
        .apply(&events[2])
        .expect_err("reject skipped sequence");
    assert_eq!(error.code(), "noncanonical_sequence");
    assert_eq!(
        projector
            .model()
            .canonical_json()
            .expect("encode state after error"),
        before,
        "a rejected event must not mutate materialized state"
    );
    projector
        .apply(&events[1])
        .expect("the expected next event remains admissible");
}

#[test]
fn rejects_out_of_order_but_preserves_the_maximum_captured_elapsed_time() {
    let events = fixture_events(NESTED_OVERLAP);
    let error = project_spans(run_id(), &events[1..])
        .expect_err("a replay must begin with canonical sequence one");
    assert_eq!(error.code(), "noncanonical_sequence");

    let mut values: Vec<Value> = serde_json::from_slice(MESSAGE_EVENTS).expect("parse values");
    values[1]["elapsed_ns"] = Value::String("9".to_owned());
    let regressing: Vec<DiagnosticEvent> = values
        .into_iter()
        .map(|value| serde_json::from_value(value).expect("parse regressing event"))
        .collect();
    let model = project_spans(run_id(), &regressing).expect("elapsed observations may interleave");
    assert_eq!(model.through_sequence().get(), 2);
    assert_eq!(model.through_elapsed_ns().get(), 10);
}
