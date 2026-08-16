use std::{
    convert::Infallible,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use troupe_diagnostics_core::{
    detail::{DiagnosticDimension, EmptyDetail, SpanStartDetail},
    event::{
        ActTokenUsageFinalized, AffectedElapsedInterval, AgentMessageCompleted, CounterSampled,
        DiagnosticEvent, DiagnosticEventHeader, DiagnosticEventKind, DiagnosticScope,
        InstantOccurred, ObservationGap, SpanFinished, SpanStarted,
    },
    hub::{
        AcceptedDiagnosticEvent, AdmissionReservation, AdmissionReserver, AdmissionSize,
        DeliveryFailure, DiagnosticEventCandidate, EventIdentity, LiveEventNotifier,
        MandatoryDurableReserver, ProductionDiagnosticHub,
    },
    id::{CanonicalUuid, RunLocalId},
    kinds::{
        CounterKind, InstantKind, SpanKind, SpanOutcome, UsageAvailability, UsageSource,
        UsageUnavailableReason,
    },
    scalar::{DecimalString, SchemaU64, TokenCount},
    time::ElapsedNs,
    view_protocol::{
        AggregateValue, CounterSelection, CounterSelector, CoverageStatus, GroupDimension,
        InstantSelector, MetricQuery, MetricSource, MetricViewRecord, OpaqueCursor, QueryFilter,
        Reducer, ScopeMode, SpanSelector, TableColumn, TableQuery, TableSource, TableViewRecord,
        TimeRangeMode, TimeSeriesQuery, TimeSeriesViewRecord, TimelineQuery, TimelineSource,
        TimelineViewRecord, TokenMetric, ViewRecord,
    },
};
use troupe_diagnostics_runtime::{
    archive::lease::ActiveArchiveLease,
    query::{
        reader::{DiagnosticReader, ReaderProfile},
        views::{
            CursorKey, ViewQueryEngine, ViewQueryErrorClass, ViewQueryErrorCode, ViewQueryRequest,
            Viewport,
        },
    },
    store::{
        batch::EventBatch,
        connection::{DiagnosticStore, InitialStoreMetadata},
        writer::TransactionalWriter,
    },
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
const CURSOR_KEY: [u8; 32] = [0x5a; 32];
const HUGE_INPUT: &str = "12345678901234567890123456789012345678901234567890";

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestRunDirectory(PathBuf);

impl TestRunDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "troupe-q01-views-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create test Run directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRunDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_id() -> CanonicalUuid {
    CanonicalUuid::parse(RUN_ID).expect("canonical test Run UUID")
}

fn create_store(directory: &Path) -> DiagnosticStore {
    DiagnosticStore::create(
        directory,
        &InitialStoreMetadata::new(run_id(), "2026-08-16T00:00:00Z", "configuration-sha256:q01"),
    )
    .expect("create diagnostic store")
}

#[derive(Clone, Copy, Debug, Default)]
struct AcceptAll;

#[derive(Debug)]
struct AcceptedReservation;

impl AdmissionReservation for AcceptedReservation {
    fn commit(self, _event: AcceptedDiagnosticEvent) {}
}

impl AdmissionReserver for AcceptAll {
    type Error = Infallible;
    type Reservation = AcceptedReservation;

    fn try_reserve(&mut self, _size: AdmissionSize) -> Result<Self::Reservation, Self::Error> {
        Ok(AcceptedReservation)
    }
}

impl MandatoryDurableReserver for AcceptAll {}

#[derive(Debug)]
struct IgnoreLive;

impl LiveEventNotifier for IgnoreLive {
    fn notify(&mut self, _event: AcceptedDiagnosticEvent) -> Result<(), DeliveryFailure> {
        Ok(())
    }
}

fn diagnostic_hub() -> ProductionDiagnosticHub<AcceptAll> {
    ProductionDiagnosticHub::production(run_id(), AcceptAll, Box::new(IgnoreLive))
}

fn header(
    identity: EventIdentity,
    elapsed_ns: u64,
    scope: DiagnosticScope,
) -> DiagnosticEventHeader {
    DiagnosticEventHeader::new(
        identity.run_id(),
        identity.sequence(),
        ElapsedNs::new(elapsed_ns),
        scope,
        Vec::new(),
    )
    .expect("valid test event header")
}

fn run_scope() -> DiagnosticScope {
    DiagnosticScope::new(None, None, None, None, None, None, None)
}

fn act_scope(act: &str) -> DiagnosticScope {
    DiagnosticScope::new(
        Some(RunLocalId::parse("scene-1").unwrap()),
        Some(RunLocalId::parse("actor-1").unwrap()),
        Some(RunLocalId::parse("cue-1").unwrap()),
        None,
        Some(RunLocalId::parse(act).unwrap()),
        None,
        None,
    )
}

fn admit<C>(
    hub: &ProductionDiagnosticHub<AcceptAll>,
    accepted: &mut Vec<AcceptedDiagnosticEvent>,
    candidate: C,
) where
    C: DiagnosticEventCandidate,
{
    accepted.push(
        hub.admit(candidate, None)
            .expect("admit query fixture event")
            .accepted()
            .clone(),
    );
}

fn commit(writer: &mut TransactionalWriter<()>, accepted: Vec<AcceptedDiagnosticEvent>) {
    writer
        .commit_batch(&EventBatch::new(accepted).expect("nonempty query fixture batch"))
        .expect("commit query fixture batch");
}

fn engine() -> ViewQueryEngine {
    ViewQueryEngine::new(CursorKey::new(CURSOR_KEY))
}

fn metric_record(id: &str, source: MetricSource, reducer: Reducer) -> ViewRecord {
    ViewRecord::Metric(
        MetricViewRecord::new(
            id.to_owned(),
            id.to_owned(),
            TimeRangeMode::Run,
            ScopeMode::Run,
            MetricQuery::new(source, Vec::new(), None, reducer).unwrap(),
        )
        .unwrap(),
    )
}

fn exact_text(value: &AggregateValue) -> &str {
    match value {
        AggregateValue::Exact { value } => value.as_str(),
        AggregateValue::Mean { numerator, .. } => numerator.as_str(),
    }
}

#[test]
fn token_metrics_sum_arbitrary_integers_and_report_availability_coverage() {
    let directory = TestRunDirectory::new("tokens");
    let lease = ActiveArchiveLease::acquire(directory.path()).unwrap();
    let mut writer = TransactionalWriter::new(create_store(directory.path()), ()).unwrap();
    let hub = diagnostic_hub();
    let mut accepted = Vec::new();

    admit(&hub, &mut accepted, |identity| {
        DiagnosticEvent::ActTokenUsageFinalized(
            ActTokenUsageFinalized::new(
                header(identity, 1, act_scope("act-1")),
                UsageAvailability::Available,
                Some(UsageSource::AcpPromptResponseUsage),
                None,
                Some(TokenCount::parse(HUGE_INPUT).unwrap()),
                Some(TokenCount::parse(HUGE_INPUT).unwrap()),
                Some(TokenCount::parse("5").unwrap()),
                Some(TokenCount::parse("3").unwrap()),
                Some(TokenCount::parse("2").unwrap()),
                Some(TokenCount::parse("1").unwrap()),
            )
            .unwrap(),
        )
    });
    admit(&hub, &mut accepted, |identity| {
        DiagnosticEvent::ActTokenUsageFinalized(
            ActTokenUsageFinalized::new(
                header(identity, 2, act_scope("act-2")),
                UsageAvailability::Partial,
                Some(UsageSource::AcpPromptResponseUsage),
                None,
                None,
                Some(TokenCount::parse("7").unwrap()),
                None,
                None,
                None,
                None,
            )
            .unwrap(),
        )
    });
    admit(&hub, &mut accepted, |identity| {
        DiagnosticEvent::ActTokenUsageFinalized(
            ActTokenUsageFinalized::new(
                header(identity, 3, act_scope("act-3")),
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
            .unwrap(),
        )
    });
    commit(&mut writer, accepted);

    let mut reader = DiagnosticReader::open_active(run_id(), lease.guard()).unwrap();
    let captured = reader.capture().unwrap();
    let response = engine()
        .query(
            &captured,
            &metric_record(
                "input_sum",
                MetricSource::ActToken {
                    metric: TokenMetric::InputTokens,
                },
                Reducer::Sum,
            ),
            &ViewQueryRequest::new(),
        )
        .unwrap();
    let metric = response.metric().unwrap();
    assert_eq!(metric.series().len(), 1);
    assert_eq!(
        exact_text(metric.series()[0].value().unwrap()),
        "12345678901234567890123456789012345678901234567897"
    );
    let coverage = metric.series()[0].coverage();
    assert_eq!(coverage.status(), CoverageStatus::Partial);
    assert_eq!(coverage.matched_count().get(), 3);
    assert_eq!(coverage.contributing_count().get(), 2);
    assert_eq!(coverage.excluded().unavailable_values().get(), 1);

    let provider = engine()
        .query(
            &captured,
            &metric_record(
                "provider_sum",
                MetricSource::ActToken {
                    metric: TokenMetric::ProviderTotalTokens,
                },
                Reducer::Sum,
            ),
            &ViewQueryRequest::new(),
        )
        .unwrap();
    let coverage = provider.metric().unwrap().series()[0].coverage();
    assert_eq!(coverage.contributing_count().get(), 1);
    assert_eq!(coverage.excluded().missing_values().get(), 1);
    assert_eq!(coverage.excluded().unavailable_values().get(), 1);
}

#[test]
fn completed_span_duration_excludes_open_spans_while_timeline_keeps_them_open() {
    let directory = TestRunDirectory::new("spans");
    let lease = ActiveArchiveLease::acquire(directory.path()).unwrap();
    let mut writer = TransactionalWriter::new(create_store(directory.path()), ()).unwrap();
    let hub = diagnostic_hub();
    let mut accepted = Vec::new();

    admit(&hub, &mut accepted, |identity| {
        DiagnosticEvent::SpanStarted(SpanStarted::new(
            header(identity, 10, act_scope("act-1")),
            SpanStartDetail::CueExecution(EmptyDetail::new()),
            None,
        ))
    });
    admit(&hub, &mut accepted, |identity| {
        DiagnosticEvent::SpanFinished(SpanFinished::new(
            header(identity, 20, act_scope("act-1")),
            SchemaU64::new(1),
            SpanOutcome::Completed,
            None,
        ))
    });
    admit(&hub, &mut accepted, |identity| {
        DiagnosticEvent::SpanStarted(SpanStarted::new(
            header(identity, 30, act_scope("act-2")),
            SpanStartDetail::CueExecution(EmptyDetail::new()),
            None,
        ))
    });
    commit(&mut writer, accepted);

    let mut reader = DiagnosticReader::open_active(run_id(), lease.guard()).unwrap();
    let captured = reader.capture().unwrap();
    let duration = engine()
        .query(
            &captured,
            &metric_record(
                "cue_duration",
                MetricSource::CompletedSpanDuration {
                    selector: SpanSelector::BuiltIn {
                        kind: SpanKind::CueExecution,
                    },
                },
                Reducer::Sum,
            ),
            &ViewQueryRequest::new(),
        )
        .unwrap();
    let series = &duration.metric().unwrap().series()[0];
    assert_eq!(exact_text(series.value().unwrap()), "10");
    assert_eq!(series.coverage().matched_count().get(), 2);
    assert_eq!(series.coverage().contributing_count().get(), 1);
    assert_eq!(series.coverage().excluded().open_spans().get(), 1);

    let timeline_record = ViewRecord::Timeline(
        TimelineViewRecord::new(
            "cue_timeline".to_owned(),
            "Cue timeline".to_owned(),
            TimeRangeMode::Run,
            ScopeMode::Run,
            TimelineQuery::new(
                TimelineSource::Span {
                    selector: SpanSelector::BuiltIn {
                        kind: SpanKind::CueExecution,
                    },
                },
                Vec::new(),
                Some(GroupDimension::Act),
            )
            .unwrap(),
        )
        .unwrap(),
    );
    let timeline = engine()
        .query(
            &captured,
            &timeline_record,
            &ViewQueryRequest::new().with_page_size(10),
        )
        .unwrap();
    let rows = timeline.timeline().unwrap().rows();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].end_ns().unwrap().get(), 20);
    assert!(rows[1].end_ns().is_none());
}

#[test]
fn selection_scoped_view_without_selection_falls_back_to_the_whole_run() {
    let directory = TestRunDirectory::new("selection-fallback");
    let lease = ActiveArchiveLease::acquire(directory.path()).unwrap();
    let mut writer = TransactionalWriter::new(create_store(directory.path()), ()).unwrap();
    let hub = diagnostic_hub();
    let mut accepted = Vec::new();
    for (elapsed, act) in [(1, "act-1"), (2, "act-2")] {
        admit(&hub, &mut accepted, move |identity| {
            DiagnosticEvent::InstantOccurred(InstantOccurred::new(
                header(identity, elapsed, act_scope(act)),
                troupe_diagnostics_core::detail::InstantDetail::CueAdmitted(EmptyDetail::new()),
                None,
            ))
        });
    }
    commit(&mut writer, accepted);

    let mut reader = DiagnosticReader::open_active(run_id(), lease.guard()).unwrap();
    let captured = reader.capture().unwrap();
    let record = ViewRecord::Timeline(
        TimelineViewRecord::new(
            "selection_fallback".to_owned(),
            "Selection fallback".to_owned(),
            TimeRangeMode::Run,
            ScopeMode::Selection,
            TimelineQuery::new(
                TimelineSource::Instant {
                    selector: InstantSelector::BuiltIn {
                        kind: InstantKind::CueAdmitted,
                    },
                },
                Vec::new(),
                None,
            )
            .unwrap(),
        )
        .unwrap(),
    );

    let whole_run = engine()
        .query(
            &captured,
            &record,
            &ViewQueryRequest::new().with_page_size(10),
        )
        .unwrap();
    assert_eq!(whole_run.timeline().unwrap().rows().len(), 2);
    assert_eq!(whole_run.metadata().binding().scope(), ScopeMode::Selection);
    assert!(whole_run.metadata().binding().selected_scope().is_none());

    let selected = engine()
        .query(
            &captured,
            &record,
            &ViewQueryRequest::new()
                .with_selected_scope(act_scope("act-1"))
                .with_page_size(10),
        )
        .unwrap();
    assert_eq!(selected.timeline().unwrap().rows().len(), 1);
    assert_eq!(
        selected.metadata().binding().selected_scope(),
        Some(&act_scope("act-1"))
    );
}

#[test]
fn time_series_is_run_origin_aligned_and_selects_latest_counter_per_bucket() {
    let directory = TestRunDirectory::new("timeseries");
    let lease = ActiveArchiveLease::acquire(directory.path()).unwrap();
    let mut writer = TransactionalWriter::new(create_store(directory.path()), ()).unwrap();
    let hub = diagnostic_hub();
    let mut accepted = Vec::new();
    for (elapsed, value) in [(1, 10), (2, 20), (3, 30), (2047, 40)] {
        admit(&hub, &mut accepted, move |identity| {
            DiagnosticEvent::CounterSampled(CounterSampled::new(
                header(identity, elapsed, run_scope()),
                CounterKind::CueActive,
                SchemaU64::new(value),
            ))
        });
    }
    commit(&mut writer, accepted);

    let mut reader = DiagnosticReader::open_active(run_id(), lease.guard()).unwrap();
    let captured = reader.capture().unwrap();
    let record = ViewRecord::TimeSeries(
        TimeSeriesViewRecord::new(
            "cue_active".to_owned(),
            "Cue active".to_owned(),
            TimeRangeMode::Viewport,
            ScopeMode::Run,
            TimeSeriesQuery::new(
                MetricSource::CounterValue {
                    selector: CounterSelector::BuiltIn {
                        kind: CounterKind::CueActive,
                    },
                    selection: CounterSelection::LatestBeforeReduce,
                },
                Vec::new(),
                None,
                Reducer::Sum,
            )
            .unwrap(),
        )
        .unwrap(),
    );
    let response = engine()
        .query(
            &captured,
            &record,
            &ViewQueryRequest::new()
                .with_viewport(Viewport::new(SchemaU64::new(1), SchemaU64::new(2048))),
        )
        .unwrap();
    let time_series = response.time_series().unwrap();
    assert_eq!(time_series.bucket_width_ns().get(), 3);
    let points = time_series.series()[0].points();
    assert_eq!(points.len(), 683);
    assert!(points.first().unwrap().is_partial());
    assert!(points.last().unwrap().is_partial());
    assert_eq!(points[0].bucket_start_ns().get(), 0);
    assert_eq!(exact_text(points[0].value().unwrap()), "20");
    assert_eq!(points[1].bucket_start_ns().get(), 3);
    assert_eq!(exact_text(points[1].value().unwrap()), "30");
    assert!(points[2].value().is_none());
    assert_eq!(points[2].coverage().contributing_count().get(), 0);
    assert_eq!(response.metadata().coverage().matched_count().get(), 3);
}

#[test]
fn timeline_cursor_is_opaque_bound_and_rejects_tamper_and_cross_query_reuse() {
    let directory = TestRunDirectory::new("cursor");
    let lease = ActiveArchiveLease::acquire(directory.path()).unwrap();
    let mut writer = TransactionalWriter::new(create_store(directory.path()), ()).unwrap();
    let hub = diagnostic_hub();
    let mut accepted = Vec::new();
    for elapsed in [1, 2, 3] {
        admit(&hub, &mut accepted, move |identity| {
            DiagnosticEvent::InstantOccurred(InstantOccurred::new(
                header(identity, elapsed, run_scope()),
                troupe_diagnostics_core::detail::InstantDetail::CueAdmitted(EmptyDetail::new()),
                None,
            ))
        });
    }
    commit(&mut writer, accepted);

    let mut reader = DiagnosticReader::open_active(run_id(), lease.guard()).unwrap();
    let captured = reader.capture().unwrap();
    let make_record = |id: &str| {
        ViewRecord::Timeline(
            TimelineViewRecord::new(
                id.to_owned(),
                id.to_owned(),
                TimeRangeMode::Run,
                ScopeMode::Run,
                TimelineQuery::new(
                    TimelineSource::Instant {
                        selector: InstantSelector::BuiltIn {
                            kind: InstantKind::CueAdmitted,
                        },
                    },
                    Vec::new(),
                    None,
                )
                .unwrap(),
            )
            .unwrap(),
        )
    };
    let record = make_record("instant_page");
    let engine = engine();
    let first = engine
        .query(
            &captured,
            &record,
            &ViewQueryRequest::new().with_page_size(2),
        )
        .unwrap();
    let cursor = first
        .metadata()
        .pagination()
        .unwrap()
        .next_cursor()
        .unwrap()
        .clone();
    assert_eq!(first.timeline().unwrap().rows().len(), 2);
    assert!(!cursor.as_str().contains("instant_page"));

    let second = engine
        .query(
            &captured,
            &record,
            &ViewQueryRequest::new()
                .with_page_size(2)
                .with_cursor(cursor.clone()),
        )
        .unwrap();
    assert_eq!(second.timeline().unwrap().rows().len(), 1);
    assert!(
        second
            .metadata()
            .pagination()
            .unwrap()
            .next_cursor()
            .is_none()
    );

    let mut tampered = cursor.as_str().as_bytes().to_vec();
    let last = tampered.last_mut().unwrap();
    *last = if *last == b'0' { b'1' } else { b'0' };
    let tampered = OpaqueCursor::parse(std::str::from_utf8(&tampered).unwrap()).unwrap();
    let error = engine
        .query(
            &captured,
            &record,
            &ViewQueryRequest::new()
                .with_page_size(2)
                .with_cursor(tampered),
        )
        .unwrap_err();
    assert_eq!(error.class(), ViewQueryErrorClass::LocalQuery);
    assert_eq!(error.code(), ViewQueryErrorCode::InvalidCursor);

    let error = engine
        .query(
            &captured,
            &make_record("other_page"),
            &ViewQueryRequest::new()
                .with_page_size(2)
                .with_cursor(cursor),
        )
        .unwrap_err();
    assert_eq!(error.class(), ViewQueryErrorClass::LocalQuery);
    assert_eq!(error.code(), ViewQueryErrorCode::InvalidCursor);
}

#[test]
fn stale_binding_is_local_but_execution_context_loss_is_active_core_fatal() {
    let directory = TestRunDirectory::new("failure-class");
    let lease = ActiveArchiveLease::acquire(directory.path()).unwrap();
    let mut writer = TransactionalWriter::new(create_store(directory.path()), ()).unwrap();
    let hub = diagnostic_hub();
    let mut accepted = Vec::new();
    admit(&hub, &mut accepted, |identity| {
        DiagnosticEvent::CounterSampled(CounterSampled::new(
            header(identity, 1, run_scope()),
            CounterKind::CueActive,
            SchemaU64::new(1),
        ))
    });
    commit(&mut writer, accepted);

    let mut reader = DiagnosticReader::open_active(run_id(), lease.guard()).unwrap();
    let record = metric_record(
        "cue_count",
        MetricSource::CounterValue {
            selector: CounterSelector::BuiltIn {
                kind: CounterKind::CueActive,
            },
            selection: CounterSelection::LatestBeforeReduce,
        },
        Reducer::Count,
    );
    let engine = engine();
    let captured = reader.capture().unwrap();
    let first = engine
        .query(&captured, &record, &ViewQueryRequest::new())
        .unwrap();
    let old_binding = first.metadata().binding().clone();
    drop(captured);

    let mut accepted = Vec::new();
    admit(&hub, &mut accepted, |identity| {
        DiagnosticEvent::CounterSampled(CounterSampled::new(
            header(identity, 2, run_scope()),
            CounterKind::CueActive,
            SchemaU64::new(2),
        ))
    });
    commit(&mut writer, accepted);
    let captured = reader.capture().unwrap();
    let stale = engine
        .query(
            &captured,
            &record,
            &ViewQueryRequest::new().with_expected_binding(old_binding),
        )
        .unwrap_err();
    assert_eq!(stale.class(), ViewQueryErrorClass::LocalQuery);
    assert_eq!(stale.code(), ViewQueryErrorCode::StaleBinding);

    engine.execution_context().mark_lost();
    let fatal = engine
        .query(&captured, &record, &ViewQueryRequest::new())
        .unwrap_err();
    assert_eq!(fatal.class(), ViewQueryErrorClass::CoreFatal);
    assert_eq!(fatal.code(), ViewQueryErrorCode::ExecutionContextLost);
}

#[test]
fn custom_scalar_filters_and_single_dimension_grouping_are_exact() {
    use std::collections::BTreeMap;
    use troupe_diagnostics_core::event::CustomCounterSampled;

    let directory = TestRunDirectory::new("custom");
    let lease = ActiveArchiveLease::acquire(directory.path()).unwrap();
    let mut writer = TransactionalWriter::new(create_store(directory.path()), ()).unwrap();
    let hub = diagnostic_hub();
    let mut accepted = Vec::new();
    for (elapsed, region, value) in [(1, "east", "1.5"), (2, "east", "2.5"), (3, "west", "9")] {
        admit(&hub, &mut accepted, move |identity| {
            let mut dimensions = BTreeMap::new();
            dimensions.insert(
                "region".to_owned(),
                DiagnosticDimension::String(region.to_owned()),
            );
            DiagnosticEvent::CustomCounterSampled(
                CustomCounterSampled::new(
                    header(identity, elapsed, run_scope()),
                    "app.queue".to_owned(),
                    troupe_diagnostics_core::detail::CustomNumber::Decimal(
                        DecimalString::parse(value).unwrap(),
                    ),
                    Some("items".to_owned()),
                    dimensions,
                )
                .unwrap(),
            )
        });
    }
    commit(&mut writer, accepted);

    let mut reader = DiagnosticReader::open_active(run_id(), lease.guard()).unwrap();
    let captured = reader.capture().unwrap();
    let record = ViewRecord::Metric(
        MetricViewRecord::new(
            "east_latest".to_owned(),
            "East latest".to_owned(),
            TimeRangeMode::Run,
            ScopeMode::Run,
            MetricQuery::new(
                MetricSource::CounterValue {
                    selector: CounterSelector::Custom {
                        name: "app.queue".to_owned(),
                    },
                    selection: CounterSelection::LatestBeforeReduce,
                },
                vec![QueryFilter::AttributeEquals {
                    key: "region".to_owned(),
                    value: troupe_diagnostics_core::detail::DiagnosticScalar::String(
                        "east".to_owned(),
                    ),
                }],
                Some(GroupDimension::CustomDimension {
                    key: "region".to_owned(),
                }),
                Reducer::Latest,
            )
            .unwrap(),
        )
        .unwrap(),
    );
    let response = engine()
        .query(&captured, &record, &ViewQueryRequest::new())
        .unwrap();
    let series = response.metric().unwrap().series();
    assert_eq!(series.len(), 1);
    assert_eq!(exact_text(series[0].value().unwrap()), "2.5");
    assert_eq!(series[0].coverage().matched_count().get(), 1);

    for (reducer, expected) in [
        (Reducer::Count, "2"),
        (Reducer::Sum, "11.5"),
        (Reducer::Min, "2.5"),
        (Reducer::Max, "9"),
        (Reducer::Mean, "11.5"),
        (Reducer::Latest, "9"),
    ] {
        let record = ViewRecord::Metric(
            MetricViewRecord::new(
                format!("all_{}", reducer.as_str()),
                "All regions".to_owned(),
                TimeRangeMode::Run,
                ScopeMode::Run,
                MetricQuery::new(
                    MetricSource::CounterValue {
                        selector: CounterSelector::Custom {
                            name: "app.queue".to_owned(),
                        },
                        selection: CounterSelection::LatestBeforeReduce,
                    },
                    Vec::new(),
                    None,
                    reducer,
                )
                .unwrap(),
            )
            .unwrap(),
        );
        let response = engine()
            .query(&captured, &record, &ViewQueryRequest::new())
            .unwrap();
        let value = response.metric().unwrap().series()[0].value().unwrap();
        assert_eq!(exact_text(value), expected, "{} reducer", reducer.as_str());
        if reducer == Reducer::Mean {
            assert_eq!(value.as_mean().unwrap().contributing_count().as_str(), "2");
        }
    }
}

#[test]
fn empty_run_time_series_has_no_buckets_and_never_truncates() {
    let directory = TestRunDirectory::new("empty");
    let lease = ActiveArchiveLease::acquire(directory.path()).unwrap();
    drop(create_store(directory.path()));
    let mut reader = DiagnosticReader::open_active(run_id(), lease.guard()).unwrap();
    let captured = reader.capture().unwrap();
    let record = ViewRecord::TimeSeries(
        TimeSeriesViewRecord::new(
            "empty_series".to_owned(),
            "Empty".to_owned(),
            TimeRangeMode::Run,
            ScopeMode::Run,
            TimeSeriesQuery::new(
                MetricSource::InstantCount {
                    selector: InstantSelector::BuiltIn {
                        kind: InstantKind::CueAdmitted,
                    },
                },
                Vec::new(),
                None,
                Reducer::Count,
            )
            .unwrap(),
        )
        .unwrap(),
    );
    let response = engine()
        .query(&captured, &record, &ViewQueryRequest::new())
        .unwrap();
    let time_series = response.time_series().unwrap();
    assert_eq!(time_series.bucket_width_ns().get(), 1);
    assert_eq!(time_series.series().len(), 1);
    assert!(time_series.series()[0].points().is_empty());
    assert!(!response.metadata().truncated());
}

#[test]
fn relevant_gaps_and_resource_truncation_make_table_coverage_explicit() {
    let directory = TestRunDirectory::new("coverage");
    let lease = ActiveArchiveLease::acquire(directory.path()).unwrap();
    let mut writer = TransactionalWriter::new(create_store(directory.path()), ()).unwrap();
    let hub = diagnostic_hub();
    let mut accepted = Vec::new();
    admit(&hub, &mut accepted, |identity| {
        DiagnosticEvent::AgentMessageCompleted(AgentMessageCompleted::new(
            header(identity, 1, act_scope("act-1")),
            RunLocalId::parse("message-1").unwrap(),
            SchemaU64::new(4096),
            SchemaU64::new(4096),
            true,
        ))
    });
    admit(&hub, &mut accepted, |identity| {
        DiagnosticEvent::ObservationGap(ObservationGap::new(
            header(identity, 2, act_scope("act-1")),
            "agent".to_owned(),
            None,
            "source delivery incomplete".to_owned(),
            Some(SchemaU64::new(1)),
            Some(AffectedElapsedInterval::new(
                ElapsedNs::new(0),
                ElapsedNs::new(2),
            )),
            Some(DiagnosticEventKind::AgentMessageCompleted),
            Some(act_scope("act-1")),
        ))
    });
    commit(&mut writer, accepted);

    let mut reader = DiagnosticReader::open_active(run_id(), lease.guard()).unwrap();
    let captured = reader.capture().unwrap();
    let record = ViewRecord::Table(
        TableViewRecord::new(
            "messages".to_owned(),
            "Messages".to_owned(),
            TimeRangeMode::Run,
            ScopeMode::Run,
            TableQuery::new(
                TableSource::Event {
                    kind: DiagnosticEventKind::AgentMessageCompleted,
                },
                Vec::new(),
                vec![TableColumn::Sequence, TableColumn::ElapsedNs],
                10,
            )
            .unwrap(),
        )
        .unwrap(),
    );
    let response = engine()
        .query(&captured, &record, &ViewQueryRequest::new())
        .unwrap();
    assert_eq!(response.table().unwrap().rows().len(), 1);
    let coverage = response.metadata().coverage();
    assert_eq!(coverage.status(), CoverageStatus::Unavailable);
    assert_eq!(coverage.matched_count().get(), 1);
    assert_eq!(coverage.excluded().resource_truncated().get(), 1);
    assert_eq!(coverage.gap_count().get(), 1);
    assert!(response.metadata().truncated());
}

#[test]
fn captured_time_uses_max_elapsed_across_the_frozen_prefix() {
    let directory = TestRunDirectory::new("captured-time-max");
    let lease = ActiveArchiveLease::acquire(directory.path()).unwrap();
    let mut writer = TransactionalWriter::new(create_store(directory.path()), ()).unwrap();
    let hub = diagnostic_hub();
    let mut accepted = Vec::new();
    for (elapsed, value) in [(100, 1), (5, 2)] {
        admit(&hub, &mut accepted, move |identity| {
            DiagnosticEvent::CounterSampled(CounterSampled::new(
                header(identity, elapsed, run_scope()),
                CounterKind::CueActive,
                SchemaU64::new(value),
            ))
        });
    }
    commit(&mut writer, accepted);

    let mut reader = DiagnosticReader::open_active(run_id(), lease.guard()).unwrap();
    let captured = reader.capture().unwrap();
    let record = ViewRecord::Table(
        TableViewRecord::new(
            "captured_time".to_owned(),
            "Captured time".to_owned(),
            TimeRangeMode::Run,
            ScopeMode::Run,
            TableQuery::new(
                TableSource::Event {
                    kind: DiagnosticEventKind::CounterSampled,
                },
                Vec::new(),
                vec![TableColumn::Sequence, TableColumn::ElapsedNs],
                10,
            )
            .unwrap(),
        )
        .unwrap(),
    );

    let response = engine()
        .query(&captured, &record, &ViewQueryRequest::new())
        .unwrap();
    assert_eq!(
        response
            .metadata()
            .binding()
            .captured_elapsed_end_ns()
            .get(),
        101
    );
    assert_eq!(response.table().unwrap().rows().len(), 2);
}

#[test]
fn captured_time_overflow_is_fail_closed_for_active_and_archive_readers() {
    let directory = TestRunDirectory::new("captured-time-overflow");
    let lease = ActiveArchiveLease::acquire(directory.path()).unwrap();
    let mut writer = TransactionalWriter::new(create_store(directory.path()), ()).unwrap();
    let hub = diagnostic_hub();
    let mut accepted = Vec::new();
    admit(&hub, &mut accepted, |identity| {
        DiagnosticEvent::CounterSampled(CounterSampled::new(
            header(identity, u64::MAX, run_scope()),
            CounterKind::CueActive,
            SchemaU64::new(1),
        ))
    });
    commit(&mut writer, accepted);

    let record = metric_record(
        "captured_time_overflow",
        MetricSource::CounterValue {
            selector: CounterSelector::BuiltIn {
                kind: CounterKind::CueActive,
            },
            selection: CounterSelection::LatestBeforeReduce,
        },
        Reducer::Latest,
    );
    let query_engine = engine();
    let mut reader = DiagnosticReader::open_active(run_id(), lease.guard()).unwrap();
    let captured = reader.capture().unwrap();
    let active_error = query_engine
        .query(&captured, &record, &ViewQueryRequest::new())
        .unwrap_err();
    assert_eq!(active_error.class(), ViewQueryErrorClass::CoreFatal);
    assert_eq!(active_error.profile(), Some(ReaderProfile::Active));
    assert_eq!(
        active_error.code(),
        ViewQueryErrorCode::CapturedTimeOverflow
    );

    drop(captured);
    drop(reader);
    drop(writer);
    drop(lease);

    let mut reader = DiagnosticReader::open_archive(directory.path(), run_id()).unwrap();
    let captured = reader.capture().unwrap();
    let archive_error = query_engine
        .query(&captured, &record, &ViewQueryRequest::new())
        .unwrap_err();
    assert_eq!(archive_error.class(), ViewQueryErrorClass::ArchiveOperation);
    assert_eq!(archive_error.profile(), Some(ReaderProfile::Archive));
    assert_eq!(
        archive_error.code(),
        ViewQueryErrorCode::CapturedTimeOverflow
    );
}
