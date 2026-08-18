use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use serde_json::Value;
use troupe_diagnostics_core::{
    detail::{CustomNumber, InstantDetail},
    event::{DiagnosticEvent, DiagnosticEventKind},
    kinds::{CounterKind, InstantKind, SpanKind},
    validate::validate_event_stream,
};

const ARBITRARY_TOKEN_COUNT: &str =
    "12345678901234567890123456789012345678901234567890123456789012345678901234567890";
const MAX_U64: &str = "18446744073709551615";
const EVENT_FIXTURES: &[&str] = &[
    "act-token-usage-finalized.json",
    "agent-message-completed.json",
    "agent-message-delta.json",
    "agent-plan-snapshot.json",
    "context-usage-sampled.json",
    "counter-sampled.json",
    "custom-counter-sampled.json",
    "custom-instant-occurred.json",
    "custom-span-finished.json",
    "custom-span-started.json",
    "diagnostic-component-failed.json",
    "instant-occurred.json",
    "limits.json",
    "nested-overlap.json",
    "observation-gap.json",
    "span-finished.json",
    "span-started.json",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MalformedFixture {
    cases: Vec<MalformedCase>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MalformedCase {
    name: String,
    expected_error: String,
    event: Value,
}

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tests/fixtures/diagnostics/events")
}

fn fixture_bytes(path: &Path) -> Vec<u8> {
    let bytes = fs::read(path).unwrap_or_else(|error| panic!("could not read {path:?}: {error}"));
    assert!(
        bytes.ends_with(b"\n"),
        "fixture must end in one LF: {path:?}"
    );
    assert!(
        !bytes[..bytes.len() - 1].contains(&b'\n'),
        "fixture must use one canonical compact JSON line: {path:?}"
    );
    bytes
}

fn canonical_body(bytes: &[u8]) -> &[u8] {
    bytes
        .strip_suffix(b"\n")
        .expect("fixture reader already checked its final LF")
}

fn load_event_files(reverse: bool) -> BTreeMap<String, (Vec<DiagnosticEvent>, Vec<Vec<u8>>)> {
    let mut entries = EVENT_FIXTURES.to_vec();
    if reverse {
        entries.reverse();
    }
    entries
        .into_iter()
        .map(|file| {
            let bytes = fixture_bytes(&fixtures_root().join(file));
            let events: Vec<DiagnosticEvent> = serde_json::from_slice(&bytes)
                .unwrap_or_else(|error| panic!("{file} must decode: {error}"));
            assert_eq!(
                serde_json::to_vec(&events).unwrap(),
                canonical_body(&bytes),
                "Rust encoding drifted from checked-in bytes for {file}",
            );
            validate_event_stream(&events)
                .unwrap_or_else(|error| panic!("{file} is not a valid stream: {error}"));
            let per_event = events
                .iter()
                .map(|event| serde_json::to_vec(event).unwrap())
                .collect();
            (file.to_owned(), (events, per_event))
        })
        .collect()
}

fn contains_string(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(value) => value == expected,
        Value::Array(values) => values.iter().any(|value| contains_string(value, expected)),
        Value::Object(values) => values
            .values()
            .any(|value| contains_string(value, expected)),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

#[test]
fn fixture_inventory_is_closed_and_event_files_are_canonical() {
    let mut actual = fs::read_dir(fixtures_root())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|file| file.ends_with(".json"))
        .collect::<Vec<_>>();
    actual.sort();
    let mut expected = EVENT_FIXTURES
        .iter()
        .copied()
        .chain(["malformed.json"])
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(actual, expected);
    load_event_files(false);
}

#[test]
fn fixtures_cover_the_closed_taxonomy_and_protocol_boundaries() {
    let files = load_event_files(false);
    let events = files
        .values()
        .flat_map(|(events, _)| events.iter())
        .collect::<Vec<_>>();

    let event_kinds = events
        .iter()
        .map(|event| event.kind().as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        event_kinds,
        DiagnosticEventKind::ALL
            .into_iter()
            .map(DiagnosticEventKind::as_str)
            .collect()
    );

    let span_kinds = events
        .iter()
        .filter_map(|event| match event {
            DiagnosticEvent::SpanStarted(start) => Some(start.span_kind().as_str()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        span_kinds,
        SpanKind::ALL
            .iter()
            .copied()
            .map(SpanKind::as_str)
            .collect()
    );

    let instant_kinds = events
        .iter()
        .filter_map(|event| match event {
            DiagnosticEvent::InstantOccurred(instant) => Some(instant.instant_kind().as_str()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        instant_kinds,
        InstantKind::ALL
            .iter()
            .copied()
            .map(InstantKind::as_str)
            .collect()
    );

    let counter_kinds = events
        .iter()
        .filter_map(|event| match event {
            DiagnosticEvent::CounterSampled(counter) => Some(counter.counter_kind().as_str()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        counter_kinds,
        CounterKind::ALL
            .iter()
            .copied()
            .map(CounterKind::as_str)
            .collect()
    );

    let component_failures = events
        .iter()
        .filter_map(|event| match event {
            DiagnosticEvent::InstantOccurred(instant) => match instant.detail() {
                InstantDetail::DiagnosticComponentFailed(detail) => {
                    Some((detail.stage().as_str(), detail.error_code().as_str()))
                }
                _ => None,
            },
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        component_failures,
        BTreeSet::from([
            ("callback", "callback_invalid_return"),
            ("callback", "callback_raised"),
            ("enqueue", "delivery_queue_unavailable"),
        ])
    );

    assert!(
        events
            .iter()
            .any(|event| event.header().caused_by().len() >= 2)
    );
    assert!(events.iter().any(|event| {
        match event {
            DiagnosticEvent::ActTokenUsageFinalized(usage) => usage
                .provider_total_tokens()
                .is_some_and(|value| value.as_str() == ARBITRARY_TOKEN_COUNT),
            _ => false,
        }
    }));
    assert!(events.iter().any(|event| match event {
        DiagnosticEvent::CustomCounterSampled(counter) => {
            matches!(counter.value(), CustomNumber::Decimal(value) if value.as_str() == "-12.34")
        }
        _ => false,
    }));

    let limits = fixture_bytes(&fixtures_root().join("limits.json"));
    let limits: Value = serde_json::from_slice(&limits).unwrap();
    assert!(contains_string(&limits, "0"));
    assert!(contains_string(&limits, MAX_U64));
    assert!(contains_string(&limits, ARBITRARY_TOKEN_COUNT));
    assert!(
        fixture_bytes(&fixtures_root().join("agent-message-delta.json"))
            .windows("你好，Troupe 👋".len())
            .any(|window| window == "你好，Troupe 👋".as_bytes())
    );
}

#[test]
fn nested_overlap_fixture_keeps_one_open_span_and_valid_temporal_relationships() {
    let files = load_event_files(false);
    let events = &files["nested-overlap.json"].0;
    let started = events
        .iter()
        .filter_map(|event| match event {
            DiagnosticEvent::SpanStarted(start) => Some((
                start.header().sequence().get(),
                start.parent_span_id().map(|value| value.get()),
            )),
            DiagnosticEvent::CustomSpanStarted(start) => Some((
                start.header().sequence().get(),
                start.parent_span_id().map(|value| value.get()),
            )),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let finished = events
        .iter()
        .filter_map(|event| match event {
            DiagnosticEvent::SpanFinished(finish) => Some(finish.span_id().get()),
            DiagnosticEvent::CustomSpanFinished(finish) => Some(finish.span_id().get()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();

    assert!(
        started.values().any(Option::is_some),
        "fixture needs nesting"
    );
    assert!(
        started.values().filter(|parent| parent.is_none()).count() >= 2,
        "fixture needs overlapping root spans"
    );
    assert_eq!(
        started
            .keys()
            .copied()
            .filter(|span_id| !finished.contains(span_id))
            .collect::<Vec<_>>(),
        [1]
    );
}

#[test]
fn malformed_cases_are_checked_in_and_rejected_by_rust_decode() {
    let bytes = fixture_bytes(&fixtures_root().join("malformed.json"));
    let fixture: MalformedFixture = serde_json::from_slice(&bytes).unwrap();
    assert!(fixture.cases.len() >= 8);
    for case in fixture.cases {
        assert!(!case.name.is_empty());
        assert!(!case.expected_error.is_empty());
        assert!(
            serde_json::from_value::<DiagnosticEvent>(case.event).is_err(),
            "malformed case unexpectedly decoded: {}",
            case.name
        );
    }
}

#[test]
fn reversing_fixture_load_order_does_not_change_any_event_bytes() {
    let forward = load_event_files(false)
        .into_iter()
        .map(|(name, (_, bytes))| (name, bytes))
        .collect::<BTreeMap<_, _>>();
    let reverse = load_event_files(true)
        .into_iter()
        .map(|(name, (_, bytes))| (name, bytes))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(forward, reverse);
}
