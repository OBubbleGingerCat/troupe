use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use troupe_diagnostics_core::view_protocol::{
    ArchivedViewRecordStatus, Coverage, CoverageStatus, ExcludedCounts, IncompatibilityReason,
    MAX_METRIC_SERIES, MAX_PAGE_ROWS, MAX_TIME_SERIES_POINTS, OperationalCapabilities, Pagination,
    QueryBinding, Renderer, ResultMetadata, ScopeMode, TimeRangeMode, ViewRecord, ViewResponse,
    classify_archived_view_record, expected_bucket_width_ns,
};
use troupe_diagnostics_core::{id::CanonicalUuid, scalar::SchemaU64};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RendererFixture {
    descriptor: ViewRecord,
    response: ViewResponse,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CompatibleFixture {
    capabilities: OperationalCapabilities,
    records: Vec<ViewRecord>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchivedFixture {
    record: Value,
    expected_reason: IncompatibilityReason,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InvalidFixture {
    cases: Vec<InvalidCase>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InvalidCase {
    name: String,
    record: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u8,
    fixtures: Vec<ManifestEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestEntry {
    file: String,
    format: String,
    sha256: String,
}

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tests/fixtures/diagnostics/views")
}

fn fixture_bytes(name: &str) -> Vec<u8> {
    let path = fixtures_root().join(name);
    let bytes = fs::read(&path).unwrap_or_else(|error| panic!("could not read {path:?}: {error}"));
    assert!(
        bytes.ends_with(b"\n"),
        "fixture must end in one LF: {path:?}"
    );
    assert!(
        !bytes[..bytes.len() - 1].contains(&b'\n'),
        "fixture must be one compact JSON line: {path:?}"
    );
    bytes
}

fn canonical_body(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\n").unwrap()
}

fn renderer_fixture(name: &str) -> RendererFixture {
    let bytes = fixture_bytes(name);
    let fixture: RendererFixture = serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("{name} must decode: {error}"));
    assert_eq!(
        serde_json::to_vec(&fixture).unwrap(),
        canonical_body(&bytes)
    );
    fixture
}

#[test]
fn manifest_is_closed_and_all_four_renderer_records_are_byte_exact() {
    let bytes = fixture_bytes("manifest.json");
    let manifest: Manifest = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(manifest.schema_version, 1);
    assert_eq!(
        manifest
            .fixtures
            .iter()
            .map(|entry| entry.file.as_str())
            .collect::<Vec<_>>(),
        [
            "compatible.json",
            "corrupt.json",
            "invalid-descriptor.json",
            "metric.json",
            "newer.json",
            "table.json",
            "timeline.json",
            "timeseries.json",
        ]
    );
    for entry in &manifest.fixtures {
        assert!(!entry.format.is_empty());
        assert_eq!(entry.sha256.len(), 64);
        assert!(entry.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(fixtures_root().join(&entry.file).is_file());
    }

    for (name, renderer) in [
        ("timeline.json", Renderer::Timeline),
        ("metric.json", Renderer::Metric),
        ("table.json", Renderer::Table),
        ("timeseries.json", Renderer::TimeSeries),
    ] {
        let fixture = renderer_fixture(name);
        assert_eq!(fixture.descriptor.renderer(), renderer);
        assert_eq!(fixture.response.renderer(), renderer);
        assert_eq!(
            fixture.descriptor.id(),
            fixture.response.metadata().view_id()
        );
        fixture.descriptor.validate().unwrap();
        fixture.response.validate().unwrap();
        fixture.response.validate_for(&fixture.descriptor).unwrap();
    }
}

#[test]
fn fixtures_freeze_empty_boundary_partial_and_exact_mean_semantics() {
    let timeline = renderer_fixture("timeline.json");
    assert!(timeline.response.timeline().unwrap().rows().is_empty());
    assert_eq!(
        timeline
            .response
            .metadata()
            .coverage()
            .matched_count()
            .get(),
        0
    );

    let table = renderer_fixture("table.json");
    let table = table.response.table().unwrap();
    assert_eq!(table.rows().len(), usize::from(MAX_PAGE_ROWS));
    assert_eq!(table.pagination().page_size(), MAX_PAGE_ROWS);
    assert!(table.pagination().next_cursor().is_some());

    let metric = renderer_fixture("metric.json");
    let metric = metric.response.metric().unwrap();
    let mean = metric.series()[0].value().unwrap().as_mean().unwrap();
    assert_eq!(metric.series()[0].unit(), Some("tokens"));
    assert_eq!(mean.numerator().as_str(), "123456789012345678901234567890");
    assert_eq!(mean.contributing_count().as_str(), "3");
    assert_eq!(metric.series()[0].coverage().excluded_count().get(), 2);
    assert!(metric.series()[0].coverage().is_partial());

    let timeseries = renderer_fixture("timeseries.json");
    let timeseries = timeseries.response.time_series().unwrap();
    assert_eq!(
        timeseries.series()[0].points().len(),
        usize::from(MAX_TIME_SERIES_POINTS)
    );
    assert_eq!(timeseries.bucket_width_ns().get(), 2);
    assert!(
        timeseries.series()[0]
            .points()
            .first()
            .unwrap()
            .is_partial()
    );
    assert!(timeseries.series()[0].points().last().unwrap().is_partial());
    assert!(timeseries.series()[0].points()[1].value().is_none());
    assert_eq!(
        timeseries.series()[0].points()[1]
            .coverage()
            .contributing_count()
            .get(),
        0
    );
    assert_eq!(expected_bucket_width_ns(1, 2047).unwrap().get(), 2);
    assert_eq!(expected_bucket_width_ns(9, 9).unwrap().get(), 1);
}

#[test]
fn operational_versions_are_independent_and_capabilities_are_explicit() {
    let bytes = fixture_bytes("compatible.json");
    let fixture: CompatibleFixture = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        serde_json::to_vec(&fixture).unwrap(),
        canonical_body(&bytes)
    );
    fixture.capabilities.validate().unwrap();
    assert_eq!(fixture.capabilities.event_schema_version(), 1);
    assert_eq!(fixture.capabilities.view_schema_version(), 1);
    assert_eq!(fixture.capabilities.api_schema_version(), 1);
    assert_eq!(fixture.capabilities.max_page_rows(), MAX_PAGE_ROWS);
    assert_eq!(fixture.capabilities.max_metric_series(), MAX_METRIC_SERIES);
    assert_eq!(
        fixture.capabilities.max_time_series_points(),
        MAX_TIME_SERIES_POINTS
    );
    assert_eq!(fixture.records.len(), 4);

    let bytes = fixture_bytes("newer.json");
    let fixture: ArchivedFixture = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(fixture.record["event_schema_version"], 1);
    assert_eq!(fixture.record["api_schema_version"], 1);
    assert_eq!(fixture.record["view_schema_version"], 2);
    assert_eq!(
        fixture.expected_reason,
        IncompatibilityReason::NewerViewSchema
    );
    match classify_archived_view_record(&fixture.record) {
        ArchivedViewRecordStatus::Incompatible(reason) => {
            assert_eq!(reason, IncompatibilityReason::NewerViewSchema)
        }
        ArchivedViewRecordStatus::Compatible(_) => panic!("newer view schema must be local-only"),
    }
}

#[test]
fn corrupt_and_forbidden_descriptors_are_rejected_without_open_ended_escape_hatches() {
    let bytes = fixture_bytes("corrupt.json");
    let fixture: ArchivedFixture = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        fixture.expected_reason,
        IncompatibilityReason::CorruptRecord
    );
    match classify_archived_view_record(&fixture.record) {
        ArchivedViewRecordStatus::Incompatible(reason) => {
            assert_eq!(reason, IncompatibilityReason::CorruptRecord)
        }
        ArchivedViewRecordStatus::Compatible(_) => panic!("corrupt view record decoded"),
    }

    let bytes = fixture_bytes("invalid-descriptor.json");
    let fixture: InvalidFixture = serde_json::from_slice(&bytes).unwrap();
    let expected = [
        "sql",
        "regex",
        "join",
        "nested_path",
        "callable",
        "custom_renderer",
        "executable_markup",
        "incompatible_reducer",
        "page_size_over_limit",
    ];
    assert_eq!(
        fixture
            .cases
            .iter()
            .map(|case| case.name.as_str())
            .collect::<Vec<_>>(),
        expected
    );
    for case in fixture.cases {
        assert!(
            serde_json::from_value::<ViewRecord>(case.record).is_err(),
            "invalid case decoded: {}",
            case.name
        );
    }
}

#[test]
fn public_builders_keep_downstream_query_execution_on_the_valid_wire_path() {
    let zero = SchemaU64::new(0);
    let binding = QueryBinding::new(
        zero,
        zero,
        TimeRangeMode::Run,
        zero,
        zero,
        ScopeMode::Run,
        None,
    )
    .unwrap();
    let coverage = Coverage::new(
        CoverageStatus::Complete,
        zero,
        zero,
        zero,
        ExcludedCounts::new(zero, zero, zero, zero, zero),
        zero,
    )
    .unwrap();
    let metadata = ResultMetadata::new(
        CanonicalUuid::parse("12345678-1234-4234-9234-123456789abc").unwrap(),
        "cue_timeline".to_owned(),
        binding,
        coverage,
        Some(Pagination::new(100, None).unwrap()),
        false,
        None,
    )
    .unwrap();
    let response = ViewResponse::new_timeline(metadata, Vec::new()).unwrap();
    assert_eq!(response.renderer(), Renderer::Timeline);

    let incompatible_filter = serde_json::json!({
        "renderer": "timeline",
        "view_schema_version": 1,
        "id": "bad_filter",
        "title": "Bad filter",
        "time_range": "run",
        "scope": "run",
        "query": {
            "source": {
                "source": "instant",
                "selector": {"selector": "built_in", "kind": "cue.admitted"}
            },
            "filters": [{"filter": "outcome", "value": "completed"}],
            "group_by": null
        }
    });
    assert!(serde_json::from_value::<ViewRecord>(incompatible_filter).is_err());
}

#[test]
fn response_contract_closes_group_caps_count_coverage_and_incompatibility() {
    let timeline_bytes = fixture_bytes("timeline.json");
    let mut timeline: Value = serde_json::from_slice(&timeline_bytes).unwrap();
    let response = timeline["response"].as_object_mut().unwrap();
    response["binding"]["captured_watermark"] = Value::String("1".to_owned());
    response["binding"]["captured_elapsed_end_ns"] = Value::String("20".to_owned());
    response["binding"]["range_end_ns"] = Value::String("20".to_owned());
    response["coverage"]["matched_count"] = Value::String("1".to_owned());
    response["coverage"]["contributing_count"] = Value::String("1".to_owned());
    response["rows"] = serde_json::json!([{
        "sequence": "1",
        "group": {
            "dimension": {"dimension": "actor"},
            "value": {"type": "string", "value": "actor-1"}
        },
        "item_type": "span",
        "name": "cue.execution",
        "start_ns": "10",
        "end_ns": "20",
        "scope": {
            "scene_id": "scene-1",
            "actor_id": "actor-1",
            "cue_id": "cue-1",
            "effect_id": null,
            "act_id": null,
            "tool_call_id": null,
            "session_generation": null
        },
        "outcome": "completed"
    }]);
    let timeline_fixture: RendererFixture = serde_json::from_value(timeline.clone()).unwrap();
    timeline_fixture
        .response
        .validate_for(&timeline_fixture.descriptor)
        .unwrap();
    let expected_group = match &timeline_fixture.descriptor {
        ViewRecord::Timeline(record) => record.query().group_by().unwrap(),
        _ => panic!("timeline fixture changed renderer"),
    };
    assert_eq!(
        timeline_fixture.response.timeline().unwrap().rows()[0]
            .group()
            .unwrap()
            .dimension(),
        expected_group
    );

    let mut future_row = timeline.clone();
    future_row["response"]["rows"][0]["sequence"] = Value::String("2".to_owned());
    assert!(serde_json::from_value::<RendererFixture>(future_row).is_err());

    let mut outside_range = timeline.clone();
    outside_range["response"]["rows"][0]["start_ns"] = Value::String("20".to_owned());
    outside_range["response"]["rows"][0]["end_ns"] = Value::String("20".to_owned());
    assert!(serde_json::from_value::<RendererFixture>(outside_range).is_err());

    let mut open_with_outcome = timeline.clone();
    open_with_outcome["response"]["rows"][0]["end_ns"] = Value::Null;
    assert!(serde_json::from_value::<RendererFixture>(open_with_outcome).is_err());

    let mut wrong_source = timeline.clone();
    wrong_source["response"]["rows"][0]["item_type"] = Value::String("instant".to_owned());
    wrong_source["response"]["rows"][0]["name"] = Value::String("cue.admitted".to_owned());
    wrong_source["response"]["rows"][0]["end_ns"] = Value::Null;
    wrong_source["response"]["rows"][0]["outcome"] = Value::Null;
    let wrong_source: RendererFixture = serde_json::from_value(wrong_source).unwrap();
    assert!(
        wrong_source
            .response
            .validate_for(&wrong_source.descriptor)
            .is_err()
    );

    let selected_scope = serde_json::json!({
        "scene_id": "scene-1",
        "actor_id": "actor-2",
        "cue_id": null,
        "effect_id": null,
        "act_id": null,
        "tool_call_id": null,
        "session_generation": null
    });
    let mut outside_selection = timeline.clone();
    outside_selection["descriptor"]["scope"] = Value::String("selection".to_owned());
    outside_selection["response"]["binding"]["scope"] = Value::String("selection".to_owned());
    outside_selection["response"]["binding"]["selected_scope"] = selected_scope;
    assert!(serde_json::from_value::<RendererFixture>(outside_selection).is_err());

    let mut wrong_value = timeline.clone();
    wrong_value["response"]["rows"][0]["group"]["value"]["value"] =
        Value::String("actor-2".to_owned());
    let wrong_value: RendererFixture = serde_json::from_value(wrong_value).unwrap();
    assert!(
        wrong_value
            .response
            .validate_for(&wrong_value.descriptor)
            .is_err()
    );

    timeline["response"]["rows"][0]["group"]["dimension"] = serde_json::json!({"dimension": "cue"});
    let wrong_group: RendererFixture = serde_json::from_value(timeline).unwrap();
    assert!(
        wrong_group
            .response
            .validate_for(&wrong_group.descriptor)
            .is_err()
    );

    let metric_bytes = fixture_bytes("metric.json");
    let mut metric_boundary: Value = serde_json::from_slice(&metric_bytes).unwrap();
    let metric_series = metric_boundary["response"]["series"][0].clone();
    metric_boundary["response"]["series"] = Value::Array(
        (0..MAX_METRIC_SERIES)
            .map(|index| {
                let mut series = metric_series.clone();
                series["group"]["value"]["value"] = Value::String(format!("act-{index}"));
                series
            })
            .collect(),
    );
    let metric_boundary: RendererFixture = serde_json::from_value(metric_boundary).unwrap();
    metric_boundary
        .response
        .validate_for(&metric_boundary.descriptor)
        .unwrap();

    let mut duplicate_metric: Value = serde_json::from_slice(&metric_bytes).unwrap();
    let duplicate_series = duplicate_metric["response"]["series"][0].clone();
    duplicate_metric["response"]["series"] =
        Value::Array(vec![duplicate_series.clone(), duplicate_series]);
    assert!(serde_json::from_value::<RendererFixture>(duplicate_metric).is_err());

    let mut unit_qualified: Value = serde_json::from_slice(&metric_bytes).unwrap();
    unit_qualified["descriptor"]["query"]["source"] = serde_json::json!({
        "source": "counter_value",
        "selector": {"selector": "custom", "name": "app.queue"},
        "selection": "latest_before_reduce"
    });
    let mut items = unit_qualified["response"]["series"][0].clone();
    items["unit"] = Value::String("items".to_owned());
    let mut bytes = items.clone();
    bytes["unit"] = Value::String("bytes".to_owned());
    unit_qualified["response"]["series"] = Value::Array(vec![items, bytes]);
    let unit_qualified: RendererFixture = serde_json::from_value(unit_qualified).unwrap();
    unit_qualified
        .response
        .validate_for(&unit_qualified.descriptor)
        .unwrap();

    let mut wrong_fixed_unit: Value = serde_json::from_slice(&metric_bytes).unwrap();
    wrong_fixed_unit["response"]["series"][0]["unit"] = Value::String("count".to_owned());
    let wrong_fixed_unit: RendererFixture = serde_json::from_value(wrong_fixed_unit).unwrap();
    assert!(
        wrong_fixed_unit
            .response
            .validate_for(&wrong_fixed_unit.descriptor)
            .is_err()
    );

    let mut missing_unit: Value = serde_json::from_slice(&metric_bytes).unwrap();
    missing_unit["response"]["series"][0]
        .as_object_mut()
        .unwrap()
        .remove("unit");
    assert!(serde_json::from_value::<RendererFixture>(missing_unit).is_err());

    for invalid_unit in [String::new(), "u".repeat(33)] {
        let mut invalid: Value = serde_json::from_slice(&metric_bytes).unwrap();
        invalid["response"]["series"][0]["unit"] = Value::String(invalid_unit);
        assert!(serde_json::from_value::<RendererFixture>(invalid).is_err());
    }

    let mut invalid_group_value: Value = serde_json::from_slice(&metric_bytes).unwrap();
    invalid_group_value["response"]["series"][0]["group"]["value"] =
        serde_json::json!({"type": "null"});
    assert!(serde_json::from_value::<RendererFixture>(invalid_group_value).is_err());

    let mut metric: Value = serde_json::from_slice(&metric_bytes).unwrap();
    let one_series = metric["response"]["series"][0].clone();
    metric["response"]["series"] = Value::Array(vec![one_series; 65]);
    assert!(serde_json::from_value::<RendererFixture>(metric).is_err());

    let mut count: Value = serde_json::from_slice(&metric_bytes).unwrap();
    count["descriptor"]["query"]["reducer"] = Value::String("count".to_owned());
    count["response"]["series"][0]["value"] = serde_json::json!({
        "aggregate": "exact",
        "value": {"type": "decimal", "value": "1.5"}
    });
    let decimal_count: RendererFixture = serde_json::from_value(count.clone()).unwrap();
    assert!(
        decimal_count
            .response
            .validate_for(&decimal_count.descriptor)
            .is_err()
    );
    count["response"]["series"][0]["value"] = serde_json::json!({
        "aggregate": "exact",
        "value": {"type": "integer", "value": "3"}
    });
    let integer_count: RendererFixture = serde_json::from_value(count).unwrap();
    integer_count
        .response
        .validate_for(&integer_count.descriptor)
        .unwrap();

    let mut fractional_tokens: Value = serde_json::from_slice(&metric_bytes).unwrap();
    fractional_tokens["response"]["series"][0]["value"]["numerator"] =
        serde_json::json!({"type": "decimal", "value": "1.5"});
    let fractional_tokens: RendererFixture = serde_json::from_value(fractional_tokens).unwrap();
    assert!(
        fractional_tokens
            .response
            .validate_for(&fractional_tokens.descriptor)
            .is_err()
    );

    let mut inconsistent_coverage: Value = serde_json::from_slice(&metric_bytes).unwrap();
    inconsistent_coverage["response"]["coverage"]["matched_count"] = Value::String("6".to_owned());
    assert!(serde_json::from_value::<RendererFixture>(inconsistent_coverage).is_err());

    let mut incompatible: Value = serde_json::from_slice(&metric_bytes).unwrap();
    incompatible["response"]["incompatible"] = serde_json::json!({
        "reason": "corrupt_record",
        "supported_view_schema_version": 1,
        "record_view_schema_version": 1
    });
    assert!(serde_json::from_value::<RendererFixture>(incompatible).is_err());

    let mut unavailable: Value = serde_json::from_slice(&metric_bytes).unwrap();
    unavailable["response"]["incompatible"] = serde_json::json!({
        "reason": "corrupt_record",
        "supported_view_schema_version": 1,
        "record_view_schema_version": 1
    });
    unavailable["response"]["coverage"]["status"] = Value::String("unavailable".to_owned());
    unavailable["response"]["coverage"]["contributing_count"] = Value::String("0".to_owned());
    unavailable["response"]["coverage"]["excluded_count"] = Value::String("5".to_owned());
    unavailable["response"]["coverage"]["excluded"]["missing_values"] =
        Value::String("4".to_owned());
    unavailable["response"]["series"] = Value::Array(Vec::new());
    serde_json::from_value::<RendererFixture>(unavailable.clone()).unwrap();
    unavailable["response"]["incompatible"]["reason"] =
        Value::String("newer_view_schema".to_owned());
    unavailable["response"]["incompatible"]["record_view_schema_version"] =
        Value::Number(256_u64.into());
    serde_json::from_value::<RendererFixture>(unavailable).unwrap();

    let timeseries_bytes = fixture_bytes("timeseries.json");
    let mut timeseries: Value = serde_json::from_slice(&timeseries_bytes).unwrap();
    timeseries["response"]["series"] = Value::Array(Vec::new());
    let empty_grouped: RendererFixture = serde_json::from_value(timeseries).unwrap();
    empty_grouped
        .response
        .validate_for(&empty_grouped.descriptor)
        .unwrap();

    let mut unsafe_key: Value = serde_json::from_slice(&timeseries_bytes).unwrap();
    unsafe_key["descriptor"]["query"]["group_by"]["key"] = Value::String("<script>".to_owned());
    assert!(serde_json::from_value::<RendererFixture>(unsafe_key).is_err());

    let table_bytes = fixture_bytes("table.json");
    let mut unordered_table: Value = serde_json::from_slice(&table_bytes).unwrap();
    unordered_table["response"]["rows"][1]["sequence"] = Value::String("1".to_owned());
    assert!(serde_json::from_value::<RendererFixture>(unordered_table).is_err());
}
