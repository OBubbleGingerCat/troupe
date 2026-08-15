use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use troupe_diagnostics_core::view_protocol::{
    ArchivedViewRecordStatus, Coverage, CoverageStatus, ExcludedCounts, IncompatibilityReason,
    MAX_PAGE_ROWS, MAX_TIME_SERIES_POINTS, OperationalCapabilities, Pagination, QueryBinding,
    Renderer, ResultMetadata, ScopeMode, TimeRangeMode, ViewRecord, ViewResponse,
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
