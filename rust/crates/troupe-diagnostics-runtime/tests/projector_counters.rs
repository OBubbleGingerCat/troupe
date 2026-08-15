use std::collections::BTreeMap;

use serde_json::json;
use troupe_diagnostics_core::{
    detail::{CanonicalInteger, CustomNumber, DiagnosticDimension, DiagnosticDimensions},
    event::{
        CausalLink, CounterSampled, CustomCounterSampled, DiagnosticEvent, DiagnosticEventHeader,
        DiagnosticScope,
    },
    id::{CanonicalUuid, RunLocalId},
    kinds::{CausalRelation, CounterKind},
    scalar::{DecimalString, SchemaU64},
    time::ElapsedNs,
};
use troupe_diagnostics_runtime::store::projector::counters::{
    CounterProjector, CounterReadModel, CounterSeriesFamily, CounterSeriesIdentity,
    CounterValueTag, project_counters,
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
const OTHER_RUN_ID: &str = "87654321-4321-4321-8321-cba987654321";
const BUILT_IN_FIXTURE: &[u8] =
    include_bytes!("../../../../tests/fixtures/diagnostics/events/counter-sampled.json");
const CUSTOM_FIXTURE: &[u8] =
    include_bytes!("../../../../tests/fixtures/diagnostics/events/custom-counter-sampled.json");

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

fn built_in(
    sequence: u64,
    elapsed_ns: u64,
    scope: DiagnosticScope,
    kind: CounterKind,
    value: u64,
) -> DiagnosticEvent {
    DiagnosticEvent::CounterSampled(CounterSampled::new(
        header_for(run_id(), sequence, elapsed_ns, scope, Vec::new()),
        kind,
        SchemaU64::new(value),
    ))
}

fn dimensions(region: &str) -> DiagnosticDimensions {
    BTreeMap::from([(
        "region".to_owned(),
        DiagnosticDimension::String(region.to_owned()),
    )])
}

#[allow(clippy::too_many_arguments)]
fn custom(
    sequence: u64,
    elapsed_ns: u64,
    scope: DiagnosticScope,
    name: &str,
    value: CustomNumber,
    unit: Option<&str>,
    dimensions: DiagnosticDimensions,
) -> DiagnosticEvent {
    DiagnosticEvent::CustomCounterSampled(
        CustomCounterSampled::new(
            header_for(run_id(), sequence, elapsed_ns, scope, Vec::new()),
            name.to_owned(),
            value,
            unit.map(str::to_owned),
            dimensions,
        )
        .expect("valid custom counter"),
    )
}

fn integer(value: &str) -> CustomNumber {
    CustomNumber::Integer(CanonicalInteger::parse(value).expect("canonical integer"))
}

fn decimal(value: &str) -> CustomNumber {
    CustomNumber::Decimal(DecimalString::parse(value).expect("canonical decimal"))
}

fn fixture_events(bytes: &[u8]) -> Vec<DiagnosticEvent> {
    serde_json::from_slice(bytes).expect("parse counter fixture")
}

fn interleaved_events() -> Vec<DiagnosticEvent> {
    let first = scope("actor-1", "cue-1", "act-1");
    let second = scope("actor-1", "cue-2", "act-2");
    vec![
        built_in(1, 10, first.clone(), CounterKind::CueActive, 5),
        custom(
            2,
            20,
            first.clone(),
            "example.pending",
            integer("123456789012345678901234567890"),
            Some("items"),
            dimensions("east"),
        ),
        built_in(3, 15, first.clone(), CounterKind::CueActive, 2),
        built_in(4, 40, second, CounterKind::CueActive, 9),
        custom(
            5,
            50,
            first.clone(),
            "example.pending",
            integer("-7"),
            Some("items"),
            dimensions("east"),
        ),
        custom(
            6,
            60,
            first,
            "example.pending",
            integer("1"),
            Some("items"),
            dimensions("west"),
        ),
    ]
}

#[test]
fn frozen_built_in_fixture_preserves_u64_values_without_accumulation() {
    let events = fixture_events(BUILT_IN_FIXTURE);
    let model = project_counters(run_id(), &events).expect("project built-in fixture");

    assert_eq!(model.model_schema_version(), 1);
    assert_eq!(model.run_id(), run_id());
    assert_eq!(model.through_sequence().get(), 5);
    assert_eq!(model.through_elapsed_ns().get(), 40);
    assert_eq!(model.series().len(), 5);
    let dropped = model
        .series()
        .iter()
        .find(|sample| {
            sample.identity().built_in_kind() == Some(CounterKind::DiagnosticDroppedEvents)
        })
        .expect("dropped-events counter");
    assert_eq!(dropped.identity().family(), CounterSeriesFamily::BuiltIn);
    assert_eq!(dropped.value().tag(), CounterValueTag::Unsigned);
    assert_eq!(
        dropped.value().unsigned().map(SchemaU64::get),
        Some(u64::MAX)
    );
    assert_eq!(dropped.sequence().get(), 5);
    assert_eq!(dropped.elapsed_ns().get(), 40);
    assert!(dropped.caused_by().is_empty());
    assert_eq!(dropped.series_key(), dropped.identity().canonical_key());
    model.validate().expect("valid materialized series");
}

#[test]
fn frozen_custom_fixture_preserves_exact_tags_decimals_and_dimensions() {
    let model = project_counters(run_id(), &fixture_events(CUSTOM_FIXTURE))
        .expect("project custom fixture");

    assert_eq!(model.series().len(), 2);
    let temperature = model
        .series()
        .iter()
        .find(|sample| sample.identity().custom_name() == Some("example.temperature"))
        .expect("temperature series");
    assert_eq!(temperature.identity().family(), CounterSeriesFamily::Custom);
    assert_eq!(temperature.identity().custom_unit(), Some("degC"));
    assert_eq!(temperature.value().tag(), CounterValueTag::Decimal);
    assert_eq!(
        temperature.value().decimal().map(DecimalString::as_str),
        Some("-12.34")
    );
    let dimensions = temperature
        .identity()
        .custom_dimensions()
        .expect("custom dimensions");
    assert_eq!(dimensions.len(), 4);
    assert_eq!(
        serde_json::to_value(dimensions).expect("encode dimensions"),
        json!({
            "active": {"type": "boolean", "value": true},
            "attempt": {"type": "integer", "value": "-1"},
            "ratio": {"type": "decimal", "value": "0.5"},
            "region": {"type": "string", "value": "华东"}
        })
    );
}

#[test]
fn exact_series_replace_latest_while_scope_and_dimensions_partition_state() {
    let events = interleaved_events();
    let model = project_counters(run_id(), &events).expect("project interleaved counters");

    assert_eq!(model.series().len(), 4);
    assert_eq!(model.through_sequence().get(), 6);
    assert_eq!(model.through_elapsed_ns().get(), 60);
    let first_scope = scope("actor-1", "cue-1", "act-1");
    let first_built_in = CounterSeriesIdentity::BuiltIn {
        scope: first_scope.clone(),
        counter_kind: CounterKind::CueActive,
    };
    assert_eq!(
        model
            .sample(&first_built_in)
            .expect("latest first Cue sample")
            .value()
            .unsigned()
            .map(SchemaU64::get),
        Some(2),
        "absolute gauge samples replace rather than sum"
    );
    let east = CounterSeriesIdentity::Custom {
        scope: first_scope.clone(),
        name: "example.pending".to_owned(),
        unit: Some("items".to_owned()),
        dimensions: dimensions("east"),
    };
    let east_sample = model.sample(&east).expect("east series");
    assert_eq!(east_sample.sequence().get(), 5);
    assert_eq!(
        east_sample.value().integer().map(CanonicalInteger::as_str),
        Some("-7")
    );
    let west = CounterSeriesIdentity::Custom {
        scope: first_scope,
        name: "example.pending".to_owned(),
        unit: Some("items".to_owned()),
        dimensions: dimensions("west"),
    };
    assert_eq!(
        model.sample(&west).expect("west series").sequence().get(),
        6
    );
    assert!(model.sample_by_key(&west.canonical_key()).is_some());
}

#[test]
fn custom_value_tag_change_fails_without_partial_update() {
    let sample_scope = scope("actor-1", "cue-1", "act-1");
    let mut projector = CounterProjector::new(run_id());
    projector
        .apply(&custom(
            1,
            10,
            sample_scope.clone(),
            "example.pending",
            integer("1"),
            Some("items"),
            dimensions("east"),
        ))
        .expect("integer sample");
    let before = projector
        .model()
        .canonical_json()
        .expect("state before error");
    let error = projector
        .apply(&custom(
            2,
            20,
            sample_scope.clone(),
            "example.pending",
            decimal("1.5"),
            Some("items"),
            dimensions("east"),
        ))
        .expect_err("series cannot change numeric tag");
    assert_eq!(error.code(), "tag_mismatch");
    assert_eq!(error.event_sequence().get(), 2);
    assert_eq!(
        projector
            .model()
            .canonical_json()
            .expect("state after error"),
        before
    );
    projector
        .apply(&custom(
            2,
            20,
            sample_scope,
            "example.pending",
            integer("2"),
            Some("items"),
            dimensions("east"),
        ))
        .expect("rejected sample did not consume sequence");
}

#[test]
fn sequence_ties_and_corrupt_series_or_tags_fail_closed() {
    let sample_scope = scope("actor-1", "cue-1", "act-1");
    let first = built_in(1, 10, sample_scope.clone(), CounterKind::CueActive, 1);
    let mut projector = CounterProjector::new(run_id());
    projector.apply(&first).expect("first sample");
    let error = projector.apply(&first).expect_err("duplicate sequence tie");
    assert_eq!(error.code(), "sequence_tie");

    let model = projector.into_model();
    let mut corrupt_key = serde_json::to_value(&model).expect("encode model");
    corrupt_key["series"][0]["series_key"] = json!("wrong");
    let corrupt_key: CounterReadModel =
        serde_json::from_value(corrupt_key).expect("decode structurally valid corrupt model");
    assert_eq!(
        corrupt_key.validate().expect_err("series key drift").code(),
        "series_mismatch"
    );

    let mut corrupt_tag = serde_json::to_value(&model).expect("encode model");
    corrupt_tag["series"][0]["value"] = json!({"type": "integer", "value": "1"});
    let corrupt_tag: CounterReadModel =
        serde_json::from_value(corrupt_tag).expect("decode structurally valid corrupt tag");
    assert_eq!(
        corrupt_tag.validate().expect_err("value tag drift").code(),
        "tag_mismatch"
    );
}

#[test]
fn incremental_and_full_replay_are_byte_identical() {
    let events = interleaved_events();
    let full = project_counters(run_id(), &events).expect("full projection");
    let mut incremental = CounterProjector::new(run_id());
    incremental
        .apply_all(&events[..2])
        .expect("first incremental batch");
    incremental
        .apply_all(&events[2..5])
        .expect("second incremental batch");
    incremental
        .apply_all(&events[5..])
        .expect("last incremental batch");
    let incremental = incremental.into_model();

    assert_eq!(incremental, full);
    assert_eq!(
        incremental.canonical_json().expect("encode incremental"),
        full.canonical_json().expect("encode full")
    );
    let decoded: CounterReadModel =
        serde_json::from_slice(&full.canonical_json().expect("encode read model"))
            .expect("decode read model");
    assert_eq!(decoded, full);
}

#[test]
fn canonical_position_reference_and_other_facts_are_handled_without_inference() {
    let sample_scope = scope("actor-1", "cue-1", "act-1");
    let skipped = built_in(2, 20, sample_scope.clone(), CounterKind::CueActive, 1);
    assert_eq!(
        project_counters(run_id(), &[skipped])
            .expect_err("reject skipped sequence")
            .code(),
        "noncanonical_sequence"
    );

    let cross_run = DiagnosticEvent::CounterSampled(CounterSampled::new(
        header_for(other_run_id(), 1, 10, sample_scope.clone(), Vec::new()),
        CounterKind::CueActive,
        SchemaU64::new(1),
    ));
    assert_eq!(
        project_counters(run_id(), &[cross_run])
            .expect_err("reject another Run")
            .code(),
        "cross_run"
    );

    let forward = DiagnosticEvent::CounterSampled(CounterSampled::new(
        header_for(
            run_id(),
            1,
            10,
            sample_scope.clone(),
            vec![CausalLink::new(
                SchemaU64::new(2),
                CausalRelation::FollowsFrom,
            )],
        ),
        CounterKind::CueActive,
        SchemaU64::new(1),
    ));
    let mut projector = CounterProjector::new(run_id());
    assert_eq!(
        projector
            .apply(&forward)
            .expect_err("reject forward causal link")
            .code(),
        "forward_link"
    );
    assert_eq!(projector.model().through_sequence().get(), 0);

    let non_counter: Vec<DiagnosticEvent> = serde_json::from_slice(include_bytes!(
        "../../../../tests/fixtures/diagnostics/events/agent-plan-snapshot.json"
    ))
    .expect("parse non-counter fixture");
    let model = project_counters(run_id(), &non_counter).expect("ignore plan semantics");
    assert!(model.series().is_empty());
    assert_eq!(model.through_sequence().get(), 2);
    assert_eq!(model.through_elapsed_ns().get(), 20);
}
