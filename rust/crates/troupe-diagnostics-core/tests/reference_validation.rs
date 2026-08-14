use serde::Deserialize;
use troupe_diagnostics_core::{
    detail::{DiagnosticAttributes, EmptyDetail, InstantDetail, SpanStartDetail},
    event::{
        CausalLink, CounterSampled, CustomSpanStarted, DiagnosticEvent, DiagnosticEventHeader,
        DiagnosticScope, InstantOccurred, ObservationGap, SpanFinished, SpanStarted,
    },
    id::{CanonicalUuid, RunLocalId},
    kinds::{CausalRelation, CounterKind, SpanOutcome},
    scalar::SchemaU64,
    time::ElapsedNs,
    validate::{
        MAX_CAUSAL_LINKS, ReferenceValidationCode, ReferenceValidator, validate_event_stream,
    },
};

const RUN_A: &str = "12345678-1234-4234-9234-123456789abc";
const RUN_B: &str = "87654321-4321-4321-8321-cba987654321";

#[derive(Debug, Deserialize)]
struct InvalidFixture {
    expected_code: ReferenceValidationCode,
    events: Vec<DiagnosticEvent>,
}

fn fixture(source: &str) -> InvalidFixture {
    serde_json::from_str(source).expect("reference-validation fixture must decode")
}

fn run(value: &str) -> CanonicalUuid {
    CanonicalUuid::parse(value).unwrap()
}

fn local(value: &str) -> RunLocalId {
    RunLocalId::parse(value).unwrap()
}

fn empty_scope() -> DiagnosticScope {
    DiagnosticScope::new(None, None, None, None, None, None, None)
}

fn scene_scope(scene: &str) -> DiagnosticScope {
    DiagnosticScope::new(Some(local(scene)), None, None, None, None, None, None)
}

fn actor_scope(scene: &str, actor: &str) -> DiagnosticScope {
    DiagnosticScope::new(
        Some(local(scene)),
        Some(local(actor)),
        None,
        None,
        None,
        None,
        None,
    )
}

fn header(
    run_id: &str,
    sequence: u64,
    elapsed_ns: u64,
    scope: DiagnosticScope,
    caused_by: Vec<CausalLink>,
) -> DiagnosticEventHeader {
    DiagnosticEventHeader::new(
        run(run_id),
        SchemaU64::new(sequence),
        ElapsedNs::new(elapsed_ns),
        scope,
        caused_by,
    )
    .unwrap()
}

fn counter(
    run_id: &str,
    sequence: u64,
    elapsed_ns: u64,
    scope: DiagnosticScope,
    caused_by: Vec<CausalLink>,
) -> DiagnosticEvent {
    DiagnosticEvent::CounterSampled(CounterSampled::new(
        header(run_id, sequence, elapsed_ns, scope, caused_by),
        CounterKind::CueActive,
        SchemaU64::new(1),
    ))
}

fn builtin_start(
    run_id: &str,
    sequence: u64,
    elapsed_ns: u64,
    scope: DiagnosticScope,
    parent_span_id: Option<u64>,
) -> DiagnosticEvent {
    DiagnosticEvent::SpanStarted(SpanStarted::new(
        header(run_id, sequence, elapsed_ns, scope, Vec::new()),
        SpanStartDetail::RunLifecycle(EmptyDetail::new()),
        parent_span_id.map(SchemaU64::new),
    ))
}

fn custom_start(
    run_id: &str,
    sequence: u64,
    elapsed_ns: u64,
    scope: DiagnosticScope,
    parent_span_id: Option<u64>,
) -> DiagnosticEvent {
    DiagnosticEvent::CustomSpanStarted(
        CustomSpanStarted::new(
            header(run_id, sequence, elapsed_ns, scope, Vec::new()),
            "example.operation".to_owned(),
            parent_span_id.map(SchemaU64::new),
            DiagnosticAttributes::new(),
        )
        .unwrap(),
    )
}

fn builtin_finish(
    run_id: &str,
    sequence: u64,
    elapsed_ns: u64,
    scope: DiagnosticScope,
    span_id: u64,
) -> DiagnosticEvent {
    DiagnosticEvent::SpanFinished(SpanFinished::new(
        header(run_id, sequence, elapsed_ns, scope, Vec::new()),
        SchemaU64::new(span_id),
        SpanOutcome::Completed,
        None,
    ))
}

fn builtin_instant(
    run_id: &str,
    sequence: u64,
    elapsed_ns: u64,
    scope: DiagnosticScope,
    containing_span_id: Option<u64>,
) -> DiagnosticEvent {
    DiagnosticEvent::InstantOccurred(InstantOccurred::new(
        header(run_id, sequence, elapsed_ns, scope, Vec::new()),
        InstantDetail::CueAdmitted(EmptyDetail::new()),
        containing_span_id.map(SchemaU64::new),
    ))
}

#[test]
fn checked_in_invalid_streams_return_stable_codes() {
    let fixtures = [
        include_str!("../../../../tests/fixtures/diagnostics/reference-validation/cross-run.json"),
        include_str!("../../../../tests/fixtures/diagnostics/reference-validation/forward-link.json"),
        include_str!("../../../../tests/fixtures/diagnostics/reference-validation/self-link.json"),
        include_str!("../../../../tests/fixtures/diagnostics/reference-validation/finish-before-start.json"),
        include_str!("../../../../tests/fixtures/diagnostics/reference-validation/double-finish.json"),
        include_str!("../../../../tests/fixtures/diagnostics/reference-validation/child-outside-parent.json"),
        include_str!("../../../../tests/fixtures/diagnostics/reference-validation/kind-mismatch.json"),
    ];

    for source in fixtures {
        let fixture = fixture(source);
        let error = validate_event_stream(&fixture.events).unwrap_err();
        assert_eq!(error.code(), fixture.expected_code);
        assert_eq!(error.code().as_str(), fixture.expected_code.as_str());
        assert_eq!(
            serde_json::to_string(&error.code()).unwrap(),
            format!("\"{}\"", fixture.expected_code.as_str())
        );
    }
}

#[test]
fn open_nested_and_overlapping_spans_are_valid_without_event_mutation() {
    let events = vec![
        builtin_start(RUN_A, 1, 10, scene_scope("scene-1"), None),
        builtin_start(RUN_A, 2, 20, actor_scope("scene-1", "actor-1"), Some(1)),
        builtin_start(RUN_A, 3, 20, scene_scope("scene-1"), None),
        builtin_finish(RUN_A, 4, 30, actor_scope("scene-1", "actor-1"), 2),
        builtin_finish(RUN_A, 5, 40, scene_scope("scene-1"), 1),
    ];
    let original = events.clone();

    let validated = validate_event_stream(&events).unwrap();

    assert_eq!(validated.len(), events.len());
    assert!(!validated.is_empty());
    assert_eq!(
        validated
            .iter()
            .map(|event| event.event().header().sequence().get())
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
    assert_eq!(events, original);
}

#[test]
fn causal_link_count_has_an_exact_public_boundary() {
    let mut events = (1..=u64::try_from(MAX_CAUSAL_LINKS).unwrap())
        .map(|sequence| counter(RUN_A, sequence, sequence, empty_scope(), Vec::new()))
        .collect::<Vec<_>>();
    events.push(counter(
        RUN_A,
        u64::try_from(MAX_CAUSAL_LINKS).unwrap() + 1,
        100,
        empty_scope(),
        (1..=u64::try_from(MAX_CAUSAL_LINKS).unwrap())
            .map(|source| CausalLink::new(SchemaU64::new(source), CausalRelation::FollowsFrom))
            .collect(),
    ));
    validate_event_stream(&events).unwrap();

    let mut over_limit = (1..=u64::try_from(MAX_CAUSAL_LINKS + 1).unwrap())
        .map(|sequence| counter(RUN_A, sequence, sequence, empty_scope(), Vec::new()))
        .collect::<Vec<_>>();
    over_limit.push(counter(
        RUN_A,
        u64::try_from(MAX_CAUSAL_LINKS).unwrap() + 2,
        101,
        empty_scope(),
        (1..=u64::try_from(MAX_CAUSAL_LINKS + 1).unwrap())
            .map(|source| CausalLink::new(SchemaU64::new(source), CausalRelation::FollowsFrom))
            .collect(),
    ));
    assert_eq!(
        validate_event_stream(&over_limit).unwrap_err().code(),
        ReferenceValidationCode::TooManyCausalLinks
    );
}

#[test]
fn failed_validation_does_not_commit_partial_state() {
    let mut validator = ReferenceValidator::new();
    let first = builtin_start(RUN_A, 1, 10, scene_scope("scene-1"), None);
    validator.validate(&first).unwrap();

    let invalid = builtin_finish(RUN_A, 2, 9, scene_scope("scene-1"), 1);
    assert_eq!(
        validator.validate(&invalid).unwrap_err().code(),
        ReferenceValidationCode::FinishBeforeStart
    );

    let valid = builtin_finish(RUN_A, 2, 20, scene_scope("scene-1"), 1);
    let accepted = validator.validate(&valid).unwrap();
    assert_eq!(accepted.event(), &valid);
}

#[test]
fn parent_and_containing_references_require_family_scope_time_and_open_state() {
    let parent = builtin_start(RUN_A, 1, 10, scene_scope("scene-1"), None);

    let family_mismatch = vec![
        parent.clone(),
        custom_start(RUN_A, 2, 20, actor_scope("scene-1", "actor-1"), Some(1)),
    ];
    assert_eq!(
        validate_event_stream(&family_mismatch).unwrap_err().code(),
        ReferenceValidationCode::KindMismatch
    );

    let scope_mismatch = vec![
        parent.clone(),
        builtin_instant(RUN_A, 2, 20, scene_scope("scene-2"), Some(1)),
    ];
    assert_eq!(
        validate_event_stream(&scope_mismatch).unwrap_err().code(),
        ReferenceValidationCode::ScopeMismatch
    );

    let before_parent = vec![
        parent.clone(),
        builtin_start(RUN_A, 2, 9, actor_scope("scene-1", "actor-1"), Some(1)),
    ];
    assert_eq!(
        validate_event_stream(&before_parent).unwrap_err().code(),
        ReferenceValidationCode::ChildOutsideParent
    );

    let closed_parent = vec![
        parent,
        builtin_finish(RUN_A, 2, 20, scene_scope("scene-1"), 1),
        builtin_instant(RUN_A, 3, 20, actor_scope("scene-1", "actor-1"), Some(1)),
    ];
    assert_eq!(
        validate_event_stream(&closed_parent).unwrap_err().code(),
        ReferenceValidationCode::ReferenceClosed
    );
}

#[test]
fn parent_cannot_finish_with_an_open_child() {
    let events = vec![
        builtin_start(RUN_A, 1, 10, scene_scope("scene-1"), None),
        builtin_start(RUN_A, 2, 20, actor_scope("scene-1", "actor-1"), Some(1)),
        builtin_finish(RUN_A, 3, 30, scene_scope("scene-1"), 1),
    ];

    assert_eq!(
        validate_event_stream(&events).unwrap_err().code(),
        ReferenceValidationCode::ChildOutsideParent
    );
}

#[test]
fn containing_instant_cannot_end_after_its_span() {
    let events = vec![
        builtin_start(RUN_A, 1, 10, scene_scope("scene-1"), None),
        builtin_instant(
            RUN_A,
            2,
            40,
            actor_scope("scene-1", "actor-1"),
            Some(1),
        ),
        builtin_finish(RUN_A, 3, 30, scene_scope("scene-1"), 1),
    ];

    assert_eq!(
        validate_event_stream(&events).unwrap_err().code(),
        ReferenceValidationCode::ChildOutsideParent
    );
}

#[test]
fn span_reference_cannot_cross_runs_even_when_sequence_exists() {
    let events = vec![
        builtin_start(RUN_A, 1, 10, scene_scope("scene-1"), None),
        builtin_finish(RUN_B, 2, 20, scene_scope("scene-1"), 1),
    ];

    assert_eq!(
        validate_event_stream(&events).unwrap_err().code(),
        ReferenceValidationCode::CrossRun
    );
}

#[test]
fn missing_scope_is_none_and_unknown_scope_sentinels_are_invalid() {
    let missing = counter(RUN_A, 1, 1, empty_scope(), Vec::new());
    let validated = validate_event_stream(std::slice::from_ref(&missing)).unwrap();
    let scope = validated.iter().next().unwrap().event().header().scope();
    assert_eq!(scope.scene_id(), None);
    assert_eq!(scope.session_generation(), None);
    assert!(RunLocalId::parse("").is_err());

    let zero_generation = DiagnosticScope::new(
        None,
        None,
        None,
        None,
        None,
        None,
        Some(SchemaU64::new(0)),
    );
    let invalid = counter(RUN_A, 1, 1, zero_generation, Vec::new());
    assert_eq!(
        validate_event_stream(std::slice::from_ref(&invalid))
            .unwrap_err()
            .code(),
        ReferenceValidationCode::InvalidScope
    );

    let affected_scope = DiagnosticScope::new(
        None,
        None,
        None,
        None,
        None,
        None,
        Some(SchemaU64::new(0)),
    );
    let invalid_gap = DiagnosticEvent::ObservationGap(ObservationGap::new(
        header(RUN_A, 1, 1, empty_scope(), Vec::new()),
        "runtime".to_owned(),
        None,
        "unobserved".to_owned(),
        None,
        None,
        None,
        Some(affected_scope),
    ));
    assert_eq!(
        validate_event_stream(std::slice::from_ref(&invalid_gap))
            .unwrap_err()
            .code(),
        ReferenceValidationCode::InvalidScope
    );
}

#[test]
fn duplicate_event_identity_and_missing_references_are_rejected() {
    let duplicate = vec![
        counter(RUN_A, 1, 1, empty_scope(), Vec::new()),
        counter(RUN_A, 1, 2, empty_scope(), Vec::new()),
    ];
    assert_eq!(
        validate_event_stream(&duplicate).unwrap_err().code(),
        ReferenceValidationCode::DuplicateSequence
    );

    let missing = vec![counter(
        RUN_A,
        2,
        2,
        empty_scope(),
        vec![CausalLink::new(
            SchemaU64::new(1),
            CausalRelation::FollowsFrom,
        )],
    )];
    assert_eq!(
        validate_event_stream(&missing).unwrap_err().code(),
        ReferenceValidationCode::ReferenceNotFound
    );
}

#[test]
fn non_span_event_references_are_kind_mismatches() {
    let finish = vec![
        counter(RUN_A, 1, 1, empty_scope(), Vec::new()),
        builtin_finish(RUN_A, 2, 2, empty_scope(), 1),
    ];
    assert_eq!(
        validate_event_stream(&finish).unwrap_err().code(),
        ReferenceValidationCode::KindMismatch
    );

    let containing = vec![
        counter(RUN_A, 1, 1, empty_scope(), Vec::new()),
        builtin_instant(RUN_A, 2, 2, empty_scope(), Some(1)),
    ];
    assert_eq!(
        validate_event_stream(&containing).unwrap_err().code(),
        ReferenceValidationCode::KindMismatch
    );
}
