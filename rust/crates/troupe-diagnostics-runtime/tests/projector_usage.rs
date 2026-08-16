use troupe_diagnostics_core::{
    event::{
        ActTokenUsageFinalized, CausalLink, ContextUsageSampled, DiagnosticEvent,
        DiagnosticEventHeader, DiagnosticScope,
    },
    id::{CanonicalUuid, RunLocalId},
    kinds::{
        CausalRelation, ContextSampleOrigin, UsageAvailability, UsageSource, UsageUnavailableReason,
    },
    scalar::{SchemaU64, TokenCount},
    time::ElapsedNs,
};
use troupe_diagnostics_runtime::store::projector::usage::{
    UsageProjector, UsageReadModel, project_usage,
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
const OTHER_RUN_ID: &str = "87654321-4321-4321-8321-cba987654321";
const HUGE: &str =
    "12345678901234567890123456789012345678901234567890123456789012345678901234567890";
const HUGE_PLUS_TEN: &str =
    "12345678901234567890123456789012345678901234567890123456789012345678901234567900";

fn run_id() -> CanonicalUuid {
    CanonicalUuid::parse(RUN_ID).expect("canonical Run UUID")
}

fn other_run_id() -> CanonicalUuid {
    CanonicalUuid::parse(OTHER_RUN_ID).expect("canonical alternate Run UUID")
}

fn local_id(value: &str) -> RunLocalId {
    RunLocalId::parse(value).expect("valid Run-local ID")
}

fn scope(scene: &str, actor: &str, cue: &str, act: Option<&str>) -> DiagnosticScope {
    DiagnosticScope::new(
        Some(local_id(scene)),
        Some(local_id(actor)),
        Some(local_id(cue)),
        None,
        act.map(local_id),
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

fn tokens(value: &str) -> TokenCount {
    TokenCount::parse(value).expect("canonical token count")
}

#[allow(clippy::too_many_arguments)]
fn usage(
    run_id: CanonicalUuid,
    sequence: u64,
    elapsed_ns: u64,
    scope: DiagnosticScope,
    availability: UsageAvailability,
    reason: Option<UsageUnavailableReason>,
    values: [Option<&str>; 6],
    caused_by: Vec<CausalLink>,
) -> DiagnosticEvent {
    let [
        provider_total,
        input,
        output,
        thought,
        cached_read,
        cached_write,
    ] = values.map(|value| value.map(tokens));
    let source = (availability != UsageAvailability::Unavailable)
        .then_some(UsageSource::AcpPromptResponseUsage);
    DiagnosticEvent::ActTokenUsageFinalized(
        ActTokenUsageFinalized::new(
            header_for(run_id, sequence, elapsed_ns, scope, caused_by),
            availability,
            source,
            reason,
            provider_total,
            input,
            output,
            thought,
            cached_read,
            cached_write,
        )
        .expect("valid terminal usage"),
    )
}

fn available(sequence: u64, scope: DiagnosticScope) -> DiagnosticEvent {
    usage(
        run_id(),
        sequence,
        sequence * 10,
        scope,
        UsageAvailability::Available,
        None,
        [
            Some(HUGE),
            Some("0"),
            Some("77"),
            Some("12"),
            None,
            Some("0"),
        ],
        Vec::new(),
    )
}

fn partial(sequence: u64, scope: DiagnosticScope, input: &str) -> DiagnosticEvent {
    usage(
        run_id(),
        sequence,
        sequence * 10,
        scope,
        UsageAvailability::Partial,
        None,
        [None, Some(input), None, None, None, None],
        Vec::new(),
    )
}

fn unavailable(sequence: u64, scope: DiagnosticScope) -> DiagnosticEvent {
    usage(
        run_id(),
        sequence,
        sequence * 10,
        scope,
        UsageAvailability::Unavailable,
        Some(UsageUnavailableReason::UsageNotReported),
        [None, None, None, None, None, None],
        Vec::new(),
    )
}

fn context_usage(sequence: u64, scope: DiagnosticScope) -> DiagnosticEvent {
    DiagnosticEvent::ContextUsageSampled(
        ContextUsageSampled::new(
            header_for(run_id(), sequence, sequence * 10, scope, Vec::new()),
            Some(SchemaU64::new(u64::MAX - 1)),
            Some(SchemaU64::new(u64::MAX)),
            None,
            None,
            ContextSampleOrigin::Provider,
            None,
        )
        .expect("valid context occupancy"),
    )
}

fn aggregate_events() -> Vec<DiagnosticEvent> {
    vec![
        available(1, scope("scene-1", "actor-1", "cue-1", Some("act-1"))),
        usage(
            run_id(),
            2,
            20,
            scope("scene-1", "actor-1", "cue-2", Some("act-2")),
            UsageAvailability::Partial,
            None,
            [Some("10"), Some("7"), None, None, None, None],
            Vec::new(),
        ),
        unavailable(3, scope("scene-1", "actor-2", "cue-3", Some("act-3"))),
        usage(
            run_id(),
            4,
            40,
            scope("scene-2", "actor-3", "cue-4", Some("act-4")),
            UsageAvailability::Partial,
            None,
            [None, None, None, Some("2"), None, None],
            Vec::new(),
        ),
    ]
}

#[test]
fn canonical_fixture_preserves_arbitrary_values_zero_and_availability() {
    let events: Vec<DiagnosticEvent> = serde_json::from_slice(include_bytes!(
        "../../../../tests/fixtures/diagnostics/events/act-token-usage-finalized.json"
    ))
    .expect("parse frozen usage fixture");

    let DiagnosticEvent::ActTokenUsageFinalized(available) = &events[0] else {
        panic!("available usage fixture")
    };
    assert_eq!(available.availability(), UsageAvailability::Available);
    assert_eq!(
        available.provider_total_tokens().map(TokenCount::as_str),
        Some(HUGE)
    );
    assert_eq!(available.input_tokens().map(TokenCount::as_str), Some("0"));
    assert_eq!(
        available.cached_write_tokens().map(TokenCount::as_str),
        Some("0")
    );
    assert!(available.cached_read_tokens().is_none());

    let DiagnosticEvent::ActTokenUsageFinalized(partial) = &events[1] else {
        panic!("partial usage fixture")
    };
    assert_eq!(partial.availability(), UsageAvailability::Partial);
    assert_eq!(partial.input_tokens().map(TokenCount::as_str), Some("7"));

    let DiagnosticEvent::ActTokenUsageFinalized(unavailable) = &events[2] else {
        panic!("unavailable usage fixture")
    };
    assert_eq!(unavailable.availability(), UsageAvailability::Unavailable);
    assert_eq!(
        unavailable.unavailable_reason(),
        Some(UsageUnavailableReason::UsageNotReported)
    );
}

#[test]
fn six_fields_keep_independent_exact_sums_and_coverage() {
    let model = project_usage(run_id(), &aggregate_events()).expect("project terminal usage");
    let aggregate = model.aggregate();

    assert_eq!(aggregate.finalized_acts().get(), 4);
    assert_eq!(aggregate.reported_acts().get(), 3);
    assert_eq!(aggregate.available_acts().get(), 1);
    assert_eq!(aggregate.partial_acts().get(), 2);
    assert_eq!(aggregate.unavailable_acts().get(), 1);

    assert_eq!(
        aggregate
            .provider_total_tokens()
            .known_sum()
            .map(TokenCount::as_str),
        Some(HUGE_PLUS_TEN)
    );
    assert_eq!(aggregate.provider_total_tokens().reported_acts().get(), 2);
    assert_eq!(aggregate.provider_total_tokens().finalized_acts().get(), 4);
    assert_eq!(
        aggregate.input_tokens().known_sum().map(TokenCount::as_str),
        Some("7"),
        "reported zero participates in coverage but does not change the exact sum"
    );
    assert_eq!(aggregate.input_tokens().reported_acts().get(), 2);
    assert_eq!(
        aggregate
            .output_tokens()
            .known_sum()
            .map(TokenCount::as_str),
        Some("77")
    );
    assert_eq!(
        aggregate
            .thought_tokens()
            .known_sum()
            .map(TokenCount::as_str),
        Some("14")
    );
    assert_eq!(aggregate.thought_tokens().reported_acts().get(), 2);
    assert!(aggregate.cached_read_tokens().known_sum().is_none());
    assert_eq!(aggregate.cached_read_tokens().reported_acts().get(), 0);
    assert_eq!(
        aggregate
            .cached_write_tokens()
            .known_sum()
            .map(TokenCount::as_str),
        Some("0")
    );
    assert_eq!(aggregate.cached_write_tokens().reported_acts().get(), 1);
}

#[test]
fn run_scene_actor_and_act_queries_share_the_same_terminal_records() {
    let model = project_usage(run_id(), &aggregate_events()).expect("project scoped usage");

    assert_eq!(model.usages().len(), 4);
    assert_eq!(
        model
            .usage_for_act(&local_id("act-2"))
            .expect("Act usage")
            .availability(),
        UsageAvailability::Partial
    );
    let scene = DiagnosticScope::new(
        Some(local_id("scene-1")),
        None,
        None,
        None,
        None,
        None,
        None,
    );
    let actor = DiagnosticScope::new(
        Some(local_id("scene-1")),
        Some(local_id("actor-1")),
        None,
        None,
        None,
        None,
        None,
    );
    let scene_aggregate = model
        .aggregate_within_scope(&scene)
        .expect("Scene aggregate");
    assert_eq!(scene_aggregate.finalized_acts().get(), 3);
    assert_eq!(scene_aggregate.reported_acts().get(), 2);
    let actor_aggregate = model
        .aggregate_within_scope(&actor)
        .expect("Actor aggregate");
    assert_eq!(actor_aggregate.finalized_acts().get(), 2);
    assert_eq!(actor_aggregate.input_tokens().reported_acts().get(), 2);
    assert_eq!(model.usages_within_scope(&actor).count(), 2);

    let scoped = model.scoped_aggregates();
    assert_eq!(scoped.len(), 5);
    assert_eq!(scoped[0].scope(), &scene);
    assert_eq!(scoped[0].aggregate(), &scene_aggregate);
    assert_eq!(scoped[1].scope(), &actor);
    assert_eq!(scoped[1].aggregate(), &actor_aggregate);
    assert_eq!(
        scoped
            .iter()
            .map(|item| (
                item.scope().scene_id().map(RunLocalId::as_str),
                item.scope().actor_id().map(RunLocalId::as_str),
                item.aggregate().finalized_acts().get(),
            ))
            .collect::<Vec<_>>(),
        vec![
            (Some("scene-1"), None, 3),
            (Some("scene-1"), Some("actor-1"), 2),
            (Some("scene-1"), Some("actor-2"), 1),
            (Some("scene-2"), None, 1),
            (Some("scene-2"), Some("actor-3"), 1),
        ]
    );
}

#[test]
fn duplicate_act_usage_fails_atomically_even_when_scope_drifts() {
    let mut projector = UsageProjector::new(run_id());
    projector
        .apply(&available(
            1,
            scope("scene-1", "actor-1", "cue-1", Some("act-1")),
        ))
        .expect("first terminal usage");
    let before = projector
        .model()
        .canonical_json()
        .expect("state before error");
    let duplicate = partial(2, scope("scene-1", "actor-1", "cue-2", Some("act-1")), "9");
    let error = projector
        .apply(&duplicate)
        .expect_err("duplicate Act usage");
    assert_eq!(error.code(), "duplicate_act_usage");
    assert_eq!(error.event_sequence().get(), 2);
    assert_eq!(
        projector
            .model()
            .canonical_json()
            .expect("state after error"),
        before
    );
    projector
        .apply(&partial(
            2,
            scope("scene-1", "actor-1", "cue-2", Some("act-2")),
            "9",
        ))
        .expect("rejected duplicate did not consume sequence");
}

#[test]
fn context_occupancy_is_not_terminal_accounting() {
    let events = vec![
        context_usage(1, scope("scene-1", "actor-1", "cue-1", Some("act-1"))),
        unavailable(2, scope("scene-1", "actor-1", "cue-1", Some("act-1"))),
    ];
    let model = project_usage(run_id(), &events).expect("project context and terminal usage");

    assert_eq!(model.through_sequence().get(), 2);
    assert_eq!(model.through_elapsed_ns().get(), 20);
    assert_eq!(model.contexts().len(), 1);
    let context = model
        .context_for_scope(&scope("scene-1", "actor-1", "cue-1", Some("act-1")))
        .expect("latest context sample for exact scope");
    assert_eq!(context.header().sequence().get(), 1);
    assert_eq!(
        context.context_used_tokens().map(SchemaU64::get),
        Some(u64::MAX - 1)
    );
    assert_eq!(
        context.context_window_tokens().map(SchemaU64::get),
        Some(u64::MAX)
    );
    assert_eq!(model.aggregate().finalized_acts().get(), 1);
    assert_eq!(model.aggregate().reported_acts().get(), 0);
    assert!(
        model
            .aggregate()
            .provider_total_tokens()
            .known_sum()
            .is_none()
    );
}

#[test]
fn latest_context_sample_replaces_only_the_same_exact_scope() {
    let first_scope = scope("scene-1", "actor-1", "cue-1", Some("act-1"));
    let second_scope = scope("scene-1", "actor-1", "cue-2", Some("act-2"));
    let events = vec![
        context_usage(1, first_scope.clone()),
        context_usage(2, second_scope.clone()),
        context_usage(3, first_scope.clone()),
    ];
    let model = project_usage(run_id(), &events).expect("project latest context samples");

    assert_eq!(model.contexts().len(), 2);
    assert_eq!(
        model
            .contexts()
            .iter()
            .map(|context| context.header().sequence().get())
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert_eq!(
        model
            .context_for_scope(&first_scope)
            .expect("replacement context")
            .header()
            .sequence()
            .get(),
        3
    );
    assert_eq!(
        model
            .context_for_scope(&second_scope)
            .expect("independent context")
            .header()
            .sequence()
            .get(),
        2
    );
    assert_eq!(model.aggregate().finalized_acts().get(), 0);
}

#[test]
fn illegal_availability_and_corrupt_aggregate_fail_closed() {
    let mut invalid_event = serde_json::to_value(partial(
        1,
        scope("scene-1", "actor-1", "cue-1", Some("act-1")),
        "1",
    ))
    .expect("encode valid usage");
    invalid_event["availability"] = serde_json::json!("available");
    assert!(serde_json::from_value::<DiagnosticEvent>(invalid_event).is_err());

    let model = project_usage(run_id(), &aggregate_events()).expect("valid model");
    let mut invalid_record = serde_json::to_value(&model).expect("encode model");
    invalid_record["usages"][0]["availability"] = serde_json::json!("partial");
    let invalid_record: UsageReadModel =
        serde_json::from_value(invalid_record).expect("decode structurally valid model");
    assert_eq!(
        invalid_record
            .validate()
            .expect_err("record availability")
            .code(),
        "usage_availability_mismatch"
    );

    let mut invalid_aggregate = serde_json::to_value(&model).expect("encode model");
    invalid_aggregate["aggregate"]["input_tokens"]["known_sum"] = serde_json::json!("999");
    let invalid_aggregate: UsageReadModel =
        serde_json::from_value(invalid_aggregate).expect("decode aggregate drift");
    assert_eq!(
        invalid_aggregate
            .validate()
            .expect_err("aggregate drift")
            .code(),
        "usage_aggregate_mismatch"
    );

    let mut invalid_scoped = serde_json::to_value(&model).expect("encode model");
    invalid_scoped["scoped_aggregates"][0]["aggregate"]["finalized_acts"] =
        serde_json::json!("999");
    let invalid_scoped: UsageReadModel =
        serde_json::from_value(invalid_scoped).expect("decode scoped aggregate drift");
    assert_eq!(
        invalid_scoped
            .validate()
            .expect_err("scoped aggregate drift")
            .code(),
        "usage_aggregate_mismatch"
    );

    let mut reordered_scopes = serde_json::to_value(&model).expect("encode model");
    reordered_scopes["scoped_aggregates"]
        .as_array_mut()
        .expect("scoped aggregate array")
        .swap(0, 1);
    let reordered_scopes: UsageReadModel =
        serde_json::from_value(reordered_scopes).expect("decode reordered scopes");
    assert_eq!(
        reordered_scopes
            .validate()
            .expect_err("scoped aggregate order")
            .code(),
        "usage_aggregate_mismatch"
    );

    let contexts = project_usage(
        run_id(),
        &[
            context_usage(1, scope("scene-1", "actor-1", "cue-1", Some("act-1"))),
            context_usage(2, scope("scene-1", "actor-1", "cue-2", Some("act-2"))),
        ],
    )
    .expect("valid contexts");
    let mut duplicate_scope = serde_json::to_value(contexts).expect("encode contexts");
    duplicate_scope["contexts"][1]["scope"] = duplicate_scope["contexts"][0]["scope"].clone();
    let duplicate_scope: UsageReadModel =
        serde_json::from_value(duplicate_scope).expect("decode duplicate context scope");
    assert_eq!(
        duplicate_scope
            .validate()
            .expect_err("duplicate context scope")
            .code(),
        "context_record_mismatch"
    );
}

#[test]
fn identity_sequence_and_reference_errors_do_not_advance_state() {
    let valid_scope = scope("scene-1", "actor-1", "cue-1", Some("act-1"));
    assert_eq!(
        project_usage(run_id(), &[available(2, valid_scope.clone())])
            .expect_err("skipped sequence")
            .code(),
        "noncanonical_sequence"
    );
    let cross_run = usage(
        other_run_id(),
        1,
        10,
        valid_scope.clone(),
        UsageAvailability::Available,
        None,
        [Some("3"), Some("1"), Some("2"), None, None, None],
        Vec::new(),
    );
    assert_eq!(
        project_usage(run_id(), &[cross_run])
            .expect_err("another Run")
            .code(),
        "cross_run"
    );
    let missing_act = available(1, scope("scene-1", "actor-1", "cue-1", None));
    assert_eq!(
        project_usage(run_id(), &[missing_act])
            .expect_err("terminal usage requires Act identity")
            .code(),
        "missing_act_identity"
    );

    let forward = usage(
        run_id(),
        1,
        10,
        valid_scope,
        UsageAvailability::Available,
        None,
        [Some("3"), Some("1"), Some("2"), None, None, None],
        vec![CausalLink::new(
            SchemaU64::new(2),
            CausalRelation::FollowsFrom,
        )],
    );
    let mut projector = UsageProjector::new(run_id());
    assert_eq!(
        projector
            .apply(&forward)
            .expect_err("forward causal link")
            .code(),
        "forward_link"
    );
    assert_eq!(projector.model().through_sequence().get(), 0);
}

#[test]
fn incremental_full_and_serialized_replay_are_byte_identical() {
    let mut events = aggregate_events();
    events.insert(
        2,
        context_usage(3, scope("scene-1", "actor-1", "cue-2", Some("act-2"))),
    );
    for (index, event) in events.iter_mut().enumerate() {
        let mut value = serde_json::to_value(&*event).expect("encode event");
        value["sequence"] = serde_json::json!((index + 1).to_string());
        value["elapsed_ns"] = serde_json::json!(((index + 1) * 10).to_string());
        *event = serde_json::from_value(value).expect("renumber canonical event");
    }

    let full = project_usage(run_id(), &events).expect("full replay");
    let mut incremental = UsageProjector::new(run_id());
    incremental.apply_all(&events[..2]).expect("first batch");
    incremental.apply_all(&events[2..4]).expect("second batch");
    incremental.apply_all(&events[4..]).expect("last batch");
    let incremental = incremental.into_model();

    assert_eq!(incremental, full);
    assert_eq!(
        incremental.canonical_json().expect("encode incremental"),
        full.canonical_json().expect("encode full")
    );
    let decoded: UsageReadModel =
        serde_json::from_slice(&full.canonical_json().expect("encode model"))
            .expect("decode model");
    decoded.validate().expect("validate decoded model");
    assert_eq!(decoded, full);
}
