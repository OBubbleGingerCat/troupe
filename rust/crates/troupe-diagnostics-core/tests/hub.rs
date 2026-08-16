use std::{
    fs,
    process::Command,
    sync::{Arc, Barrier, Mutex},
    thread,
    time::SystemTime,
};

use troupe_diagnostics_core::{
    detail::{EmptyDetail, SpanStartDetail},
    event::{
        CounterSampled, DiagnosticEvent, DiagnosticEventHeader, DiagnosticEventKind,
        DiagnosticScope, ObservationGap, SpanFinished, SpanStarted,
    },
    hub::{
        AcceptedDiagnosticEvent, ActEventSubscriber, AdmissionReservation, AdmissionReserver,
        AdmissionSize, BoundedInMemoryReserver, DeliveryFailure, DeliveryOutcome,
        DiagnosticEventCandidate, EventIdentity, HubAdmissionError, LiveEventNotifier,
        MandatoryDurableReserver, ProductionDiagnosticHub, SinkOnlyDiagnosticHub,
        SubscriberLocalGap,
    },
    id::CanonicalUuid,
    kinds::{CounterKind, SpanKind, SpanOutcome},
    scalar::SchemaU64,
    time::ElapsedNs,
    validate::ReferenceValidationCode,
};

const RUN: &str = "12345678-1234-4234-9234-123456789abc";

fn run_id() -> CanonicalUuid {
    CanonicalUuid::parse(RUN).unwrap()
}

fn empty_scope() -> DiagnosticScope {
    DiagnosticScope::new(None, None, None, None, None, None, None)
}

fn header(identity: EventIdentity, elapsed_ns: u64) -> DiagnosticEventHeader {
    DiagnosticEventHeader::new(
        identity.run_id(),
        identity.sequence(),
        ElapsedNs::new(elapsed_ns),
        empty_scope(),
        Vec::new(),
    )
    .unwrap()
}

fn counter_candidate(elapsed_ns: u64) -> impl DiagnosticEventCandidate {
    move |identity| {
        DiagnosticEvent::CounterSampled(CounterSampled::new(
            header(identity, elapsed_ns),
            CounterKind::CueActive,
            SchemaU64::new(1),
        ))
    }
}

fn finish_before_start_candidate() -> impl DiagnosticEventCandidate {
    move |identity| {
        DiagnosticEvent::SpanFinished(SpanFinished::new(
            header(identity, 1),
            SchemaU64::new(2),
            SpanOutcome::Completed,
            None,
        ))
    }
}

fn span_start_candidate(elapsed_ns: u64) -> impl DiagnosticEventCandidate {
    move |identity| {
        DiagnosticEvent::SpanStarted(SpanStarted::new(
            header(identity, elapsed_ns),
            SpanStartDetail::RunLifecycle(EmptyDetail::new()),
            None,
        ))
    }
}

fn span_finish_candidate(elapsed_ns: u64, span_id: u64) -> impl DiagnosticEventCandidate {
    move |identity| {
        DiagnosticEvent::SpanFinished(SpanFinished::new(
            header(identity, elapsed_ns),
            SchemaU64::new(span_id),
            SpanOutcome::Completed,
            None,
        ))
    }
}

fn observation_gap_candidate() -> impl DiagnosticEventCandidate {
    move |identity| {
        DiagnosticEvent::ObservationGap(ObservationGap::new(
            header(identity, 2),
            "agent_observer".to_owned(),
            None,
            "source_truncated".to_owned(),
            Some(SchemaU64::new(1)),
            None,
            None,
            None,
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FakeReserveError {
    CapacityUnavailable,
}

impl std::fmt::Display for FakeReserveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("capacity unavailable")
    }
}

impl std::error::Error for FakeReserveError {}

#[derive(Clone, Default)]
struct FakeReserver {
    state: Arc<Mutex<FakeReserverState>>,
}

#[derive(Default)]
struct FakeReserverState {
    fail_next: bool,
    fatal_requests: usize,
    requests: Vec<AdmissionSize>,
    committed: Vec<AcceptedDiagnosticEvent>,
    released_without_commit: usize,
}

#[derive(Clone)]
struct FakeReserverSnapshot {
    fatal_requests: usize,
    requests: Vec<AdmissionSize>,
    committed: Vec<AcceptedDiagnosticEvent>,
    released_without_commit: usize,
}

impl FakeReserver {
    fn fail_next(&self) {
        self.state.lock().unwrap().fail_next = true;
    }

    fn snapshot(&self) -> FakeReserverSnapshot {
        let state = self.state.lock().unwrap();
        FakeReserverSnapshot {
            fatal_requests: state.fatal_requests,
            requests: state.requests.clone(),
            committed: state.committed.clone(),
            released_without_commit: state.released_without_commit,
        }
    }

    fn try_reserve(&self, size: AdmissionSize) -> Result<FakeReservation, FakeReserveError> {
        let mut state = self.state.lock().unwrap();
        state.requests.push(size);
        if state.fail_next {
            state.fail_next = false;
            return Err(FakeReserveError::CapacityUnavailable);
        }
        drop(state);
        Ok(FakeReservation {
            state: Arc::clone(&self.state),
            committed: false,
        })
    }

    fn try_reserve_fatal(&self, size: AdmissionSize) -> Result<FakeReservation, FakeReserveError> {
        self.state.lock().unwrap().fatal_requests += 1;
        self.try_reserve(size)
    }
}

struct FakeReservation {
    state: Arc<Mutex<FakeReserverState>>,
    committed: bool,
}

impl AdmissionReservation for FakeReservation {
    fn commit(mut self, event: AcceptedDiagnosticEvent) {
        self.state.lock().unwrap().committed.push(event);
        self.committed = true;
    }
}

impl Drop for FakeReservation {
    fn drop(&mut self) {
        if !self.committed {
            self.state.lock().unwrap().released_without_commit += 1;
        }
    }
}

#[derive(Clone)]
struct FakeDurableReserver(FakeReserver);

impl AdmissionReserver for FakeDurableReserver {
    type Error = FakeReserveError;
    type Reservation = FakeReservation;

    fn try_reserve(&mut self, size: AdmissionSize) -> Result<Self::Reservation, Self::Error> {
        self.0.try_reserve(size)
    }

    fn try_reserve_fatal(&mut self, size: AdmissionSize) -> Result<Self::Reservation, Self::Error> {
        self.0.try_reserve_fatal(size)
    }
}

impl MandatoryDurableReserver for FakeDurableReserver {}

#[derive(Clone)]
struct FakeBoundedReserver(FakeReserver);

impl AdmissionReserver for FakeBoundedReserver {
    type Error = FakeReserveError;
    type Reservation = FakeReservation;

    fn try_reserve(&mut self, size: AdmissionSize) -> Result<Self::Reservation, Self::Error> {
        self.0.try_reserve(size)
    }
}

impl BoundedInMemoryReserver for FakeBoundedReserver {}

#[derive(Clone, Default)]
struct RecordingLiveNotifier {
    events: Arc<Mutex<Vec<AcceptedDiagnosticEvent>>>,
}

impl RecordingLiveNotifier {
    fn events(&self) -> Vec<AcceptedDiagnosticEvent> {
        self.events.lock().unwrap().clone()
    }
}

impl LiveEventNotifier for RecordingLiveNotifier {
    fn notify(&mut self, event: AcceptedDiagnosticEvent) -> Result<(), DeliveryFailure> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}

#[derive(Clone, Default)]
struct RecordingSubscriber {
    state: Arc<Mutex<RecordingSubscriberState>>,
}

#[derive(Default)]
struct RecordingSubscriberState {
    fail: bool,
    events: Vec<AcceptedDiagnosticEvent>,
}

impl RecordingSubscriber {
    fn failing() -> Self {
        let value = Self::default();
        value.state.lock().unwrap().fail = true;
        value
    }

    fn events(&self) -> Vec<AcceptedDiagnosticEvent> {
        self.state.lock().unwrap().events.clone()
    }
}

impl ActEventSubscriber for RecordingSubscriber {
    fn deliver(&self, event: AcceptedDiagnosticEvent) -> Result<(), DeliveryFailure> {
        let mut state = self.state.lock().unwrap();
        state.events.push(event);
        if state.fail {
            Err(DeliveryFailure::new("subscriber_unavailable"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn production_admission_reserves_exact_bytes_and_fans_out_one_immutable_fact() {
    let reserver = FakeReserver::default();
    let live = RecordingLiveNotifier::default();
    let live_observer = live.clone();
    let subscriber = RecordingSubscriber::default();
    let hub = ProductionDiagnosticHub::production(
        run_id(),
        FakeDurableReserver(reserver.clone()),
        Box::new(live),
    );

    let receipt = hub.admit(counter_candidate(11), Some(&subscriber)).unwrap();
    let accepted = receipt.accepted();

    assert_eq!(accepted.identity().run_id(), run_id());
    assert_eq!(accepted.identity().sequence().get(), 1);
    assert_eq!(accepted.event().header().sequence().get(), 1);
    assert_eq!(receipt.live_delivery(), &DeliveryOutcome::Delivered);
    assert_eq!(receipt.subscriber_delivery(), &DeliveryOutcome::Delivered);
    assert_eq!(
        serde_json::from_slice::<DiagnosticEvent>(accepted.canonical_bytes()).unwrap(),
        *accepted.event()
    );

    let durable = reserver.snapshot();
    let live = live_observer.events();
    let subscriber = subscriber.events();
    assert_eq!(durable.requests.len(), 1);
    assert_eq!(durable.requests[0].event_count(), 1);
    assert_eq!(
        durable.requests[0].canonical_bytes(),
        accepted.canonical_bytes().len()
    );
    assert!(durable.committed[0].same_fact(accepted));
    assert!(live[0].same_fact(accepted));
    assert!(subscriber[0].same_fact(accepted));
    assert_eq!(
        durable.committed[0].canonical_bytes(),
        live[0].canonical_bytes()
    );
    assert_eq!(live[0].canonical_bytes(), subscriber[0].canonical_bytes());
}

#[test]
fn accepted_finish_resolves_span_kind_without_changing_canonical_bytes() {
    let reserver = FakeReserver::default();
    let hub = ProductionDiagnosticHub::production(
        run_id(),
        FakeDurableReserver(reserver),
        Box::new(RecordingLiveNotifier::default()),
    );

    let started = hub.admit(span_start_candidate(10), None).unwrap();
    let finished = hub.admit(span_finish_candidate(20, 1), None).unwrap();

    assert_eq!(
        started.accepted().built_in_span_kind(),
        Some(SpanKind::RunLifecycle)
    );
    assert_eq!(
        finished.accepted().built_in_span_kind(),
        Some(SpanKind::RunLifecycle)
    );
    let canonical =
        serde_json::from_slice::<serde_json::Value>(finished.accepted().canonical_bytes()).unwrap();
    assert_eq!(canonical["kind"], "span_finished");
    assert!(canonical.get("span_kind").is_none());
    assert_eq!(
        serde_json::from_slice::<DiagnosticEvent>(finished.accepted().canonical_bytes()).unwrap(),
        *finished.accepted().event()
    );
}

#[test]
fn production_fatal_admission_uses_a_distinct_reservation_path_and_dense_sequence() {
    let reserver = FakeReserver::default();
    let hub = ProductionDiagnosticHub::production(
        run_id(),
        FakeDurableReserver(reserver.clone()),
        Box::new(RecordingLiveNotifier::default()),
    );

    let first = hub.admit(counter_candidate(1), None).unwrap();
    let fatal = hub.admit_fatal(counter_candidate(2), None).unwrap();

    assert_eq!(first.accepted().identity().sequence().get(), 1);
    assert_eq!(fatal.accepted().identity().sequence().get(), 2);
    let snapshot = reserver.snapshot();
    assert_eq!(snapshot.fatal_requests, 1);
    assert_eq!(snapshot.requests.len(), 2);
    assert!(snapshot.committed[1].same_fact(fatal.accepted()));
}

#[test]
fn reservation_failure_does_not_consume_sequence_or_reach_fanout() {
    let reserver = FakeReserver::default();
    reserver.fail_next();
    let live = RecordingLiveNotifier::default();
    let live_observer = live.clone();
    let hub = ProductionDiagnosticHub::production(
        run_id(),
        FakeDurableReserver(reserver.clone()),
        Box::new(live),
    );

    let error = hub.admit(counter_candidate(1), None).unwrap_err();
    assert!(matches!(
        error,
        HubAdmissionError::Reservation(FakeReserveError::CapacityUnavailable)
    ));
    assert!(reserver.snapshot().committed.is_empty());
    assert!(live_observer.events().is_empty());

    let accepted = hub.admit(counter_candidate(1), None).unwrap();
    assert_eq!(accepted.accepted().identity().sequence().get(), 1);
    let snapshot = reserver.snapshot();
    assert_eq!(snapshot.requests.len(), 2);
    assert_eq!(snapshot.committed.len(), 1);
    assert_eq!(snapshot.released_without_commit, 0);
}

#[test]
fn reference_failure_releases_reservation_without_consuming_sequence() {
    let reserver = FakeReserver::default();
    let hub = ProductionDiagnosticHub::production(
        run_id(),
        FakeDurableReserver(reserver.clone()),
        Box::new(RecordingLiveNotifier::default()),
    );

    let error = hub
        .admit(finish_before_start_candidate(), None)
        .unwrap_err();
    assert!(matches!(
        error,
        HubAdmissionError::Reference(ref error)
            if error.code() == ReferenceValidationCode::FinishBeforeStart
    ));
    let after_error = reserver.snapshot();
    assert!(after_error.committed.is_empty());
    assert_eq!(after_error.released_without_commit, 1);

    let accepted = hub.admit(counter_candidate(2), None).unwrap();
    assert_eq!(accepted.accepted().identity().sequence().get(), 1);
}

#[test]
fn candidate_cannot_override_hub_identity_and_failure_is_not_admitted() {
    let reserver = FakeReserver::default();
    let hub = ProductionDiagnosticHub::production(
        run_id(),
        FakeDurableReserver(reserver.clone()),
        Box::new(RecordingLiveNotifier::default()),
    );
    let other_run = CanonicalUuid::parse("87654321-4321-4321-8321-cba987654321").unwrap();

    let error = hub
        .admit(
            move |_| {
                DiagnosticEvent::CounterSampled(CounterSampled::new(
                    DiagnosticEventHeader::new(
                        other_run,
                        SchemaU64::new(99),
                        ElapsedNs::new(1),
                        empty_scope(),
                        Vec::new(),
                    )
                    .unwrap(),
                    CounterKind::CueActive,
                    SchemaU64::new(1),
                ))
            },
            None,
        )
        .unwrap_err();
    assert!(matches!(
        error,
        HubAdmissionError::CandidateIdentityMismatch { .. }
    ));
    assert!(reserver.snapshot().requests.is_empty());

    let accepted = hub.admit(counter_candidate(1), None).unwrap();
    assert_eq!(accepted.accepted().identity().sequence().get(), 1);
}

#[test]
fn subscriber_failure_is_reported_without_rolling_back_the_fact() {
    let reserver = FakeReserver::default();
    let live = RecordingLiveNotifier::default();
    let live_observer = live.clone();
    let subscriber = RecordingSubscriber::failing();
    let hub = ProductionDiagnosticHub::production(
        run_id(),
        FakeDurableReserver(reserver.clone()),
        Box::new(live),
    );

    let first = hub.admit(counter_candidate(1), Some(&subscriber)).unwrap();
    assert!(matches!(
        first.subscriber_delivery(),
        DeliveryOutcome::Failed(error) if error.code() == "subscriber_unavailable"
    ));
    assert_eq!(first.live_delivery(), &DeliveryOutcome::Delivered);
    assert_eq!(reserver.snapshot().committed.len(), 1);
    assert_eq!(live_observer.events().len(), 1);

    let second = hub.admit(counter_candidate(2), None).unwrap();
    assert_eq!(second.accepted().identity().sequence().get(), 2);
    assert_eq!(
        second.subscriber_delivery(),
        &DeliveryOutcome::NotConfigured
    );
    assert_eq!(reserver.snapshot().committed.len(), 2);
}

#[test]
fn sink_only_profile_has_no_live_delivery_and_keeps_local_gap_out_of_event_stream() {
    let reserver = FakeReserver::default();
    let subscriber = RecordingSubscriber::default();
    let hub = SinkOnlyDiagnosticHub::sink_only(run_id(), FakeBoundedReserver(reserver.clone()));

    let counter = hub.admit(counter_candidate(1), &subscriber).unwrap();
    assert_eq!(counter.live_delivery(), &DeliveryOutcome::NotConfigured);
    assert_eq!(counter.subscriber_delivery(), &DeliveryOutcome::Delivered);
    assert_eq!(counter.accepted().identity().sequence().get(), 1);
    assert!(subscriber.events()[0].same_fact(counter.accepted()));

    let production = ProductionDiagnosticHub::production(
        run_id(),
        FakeDurableReserver(FakeReserver::default()),
        Box::new(RecordingLiveNotifier::default()),
    );
    let production_counter = production.admit(counter_candidate(1), None).unwrap();
    assert_eq!(
        production_counter.accepted().identity(),
        counter.accepted().identity()
    );
    assert_eq!(
        production_counter.accepted().canonical_bytes(),
        counter.accepted().canonical_bytes()
    );

    let gap = SubscriberLocalGap::new(3, 120);
    assert_eq!(gap.dropped_events(), 3);
    assert_eq!(gap.dropped_canonical_bytes(), 120);

    let canonical_gap = hub.admit(observation_gap_candidate(), &subscriber).unwrap();
    assert_eq!(
        canonical_gap.accepted().event().kind(),
        DiagnosticEventKind::ObservationGap
    );
    assert_eq!(canonical_gap.accepted().identity().sequence().get(), 2);
    assert_eq!(reserver.snapshot().committed.len(), 2);
}

#[test]
fn concurrent_producers_are_dense_and_ordered_across_repeated_barrier_seeds() {
    const SEEDS: usize = 12;
    const PRODUCERS: usize = 6;
    const EVENTS_PER_PRODUCER: usize = 20;

    for seed in 0..SEEDS {
        let reserver = FakeReserver::default();
        let live = RecordingLiveNotifier::default();
        let live_observer = live.clone();
        let subscriber = Arc::new(RecordingSubscriber::default());
        let hub = Arc::new(ProductionDiagnosticHub::production(
            run_id(),
            FakeDurableReserver(reserver.clone()),
            Box::new(live),
        ));
        let barrier = Arc::new(Barrier::new(PRODUCERS + 1));
        let mut handles = Vec::new();

        for offset in 0..PRODUCERS {
            let producer = (offset + seed) % PRODUCERS;
            let hub = Arc::clone(&hub);
            let barrier = Arc::clone(&barrier);
            let subscriber = Arc::clone(&subscriber);
            handles.push(thread::spawn(move || {
                barrier.wait();
                let mut sequences = Vec::new();
                for ordinal in 0..EVENTS_PER_PRODUCER {
                    if (seed + producer * 17 + ordinal * 31).is_multiple_of(5) {
                        thread::yield_now();
                    }
                    let elapsed = u64::try_from(seed * 10_000 + producer * 100 + ordinal).unwrap();
                    let receipt = hub
                        .admit(counter_candidate(elapsed), Some(subscriber.as_ref()))
                        .unwrap();
                    sequences.push(receipt.accepted().identity().sequence().get());
                }
                sequences
            }));
        }

        barrier.wait();
        let mut observed = handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        observed.sort_unstable();
        let expected =
            (1..=u64::try_from(PRODUCERS * EVENTS_PER_PRODUCER).unwrap()).collect::<Vec<_>>();
        assert_eq!(observed, expected, "seed {seed}");

        let durable = reserver.snapshot();
        let durable_sequences = durable
            .committed
            .iter()
            .map(|event| event.identity().sequence().get())
            .collect::<Vec<_>>();
        let live_sequences = live_observer
            .events()
            .iter()
            .map(|event| event.identity().sequence().get())
            .collect::<Vec<_>>();
        let subscriber_sequences = subscriber
            .events()
            .iter()
            .map(|event| event.identity().sequence().get())
            .collect::<Vec<_>>();
        assert_eq!(durable_sequences, expected, "durable seed {seed}");
        assert_eq!(live_sequences, expected, "live seed {seed}");
        assert_eq!(subscriber_sequences, expected, "subscriber seed {seed}");
        assert_eq!(durable.requests.len(), expected.len());
    }
}

#[test]
fn profiles_and_local_gap_are_compile_time_separated() {
    let current = std::env::current_exe().unwrap();
    let deps = current.parent().unwrap();
    let core_rlib = fs::read_dir(deps)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("libtroupe_diagnostics_core-") && name.ends_with(".rlib")
                })
        })
        .max_by_key(|path| {
            path.metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap()
        })
        .expect("diagnostics core rlib is next to the integration test binary");
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let scratch = std::env::temp_dir().join(format!("troupe-c02-{nonce}-{}", std::process::id()));
    fs::create_dir(&scratch).unwrap();

    let compile = |crate_name: &str, source_text: &str| {
        let source = scratch.join(format!("{crate_name}.rs"));
        fs::write(&source, source_text).unwrap();
        Command::new("rustc")
            .arg("--edition=2024")
            .arg(format!("--crate-name={crate_name}"))
            .arg(&source)
            .arg("--extern")
            .arg(format!("troupe_diagnostics_core={}", core_rlib.display()))
            .arg("-L")
            .arg(format!("dependency={}", deps.display()))
            .arg("--out-dir")
            .arg(&scratch)
            .output()
            .unwrap()
    };

    let bounded_as_production = compile(
        "c02_bounded_as_production",
        r#"
use troupe_diagnostics_core::{
    hub::{AcceptedDiagnosticEvent, AdmissionReservation, AdmissionReserver, AdmissionSize,
          BoundedInMemoryReserver, DeliveryFailure, LiveEventNotifier, ProductionDiagnosticHub},
    id::CanonicalUuid,
};
#[derive(Debug)] struct E;
impl std::fmt::Display for E { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("e") } }
impl std::error::Error for E {}
struct Token;
impl AdmissionReservation for Token { fn commit(self, _: AcceptedDiagnosticEvent) {} }
struct Bounded;
impl AdmissionReserver for Bounded {
    type Error = E; type Reservation = Token;
    fn try_reserve(&mut self, _: AdmissionSize) -> Result<Token, E> { Ok(Token) }
}
impl BoundedInMemoryReserver for Bounded {}
struct Live;
impl LiveEventNotifier for Live {
    fn notify(&mut self, _: AcceptedDiagnosticEvent) -> Result<(), DeliveryFailure> { Ok(()) }
}
fn main() {
    let run = CanonicalUuid::parse("12345678-1234-4234-9234-123456789abc").unwrap();
    let _: ProductionDiagnosticHub<Bounded> = ProductionDiagnosticHub::production(run, Bounded, Box::new(Live));
}
"#,
    );
    assert!(!bounded_as_production.status.success());
    let stderr = String::from_utf8(bounded_as_production.stderr).unwrap();
    assert!(stderr.contains("MandatoryDurableReserver"), "{stderr}");

    let durable_as_sink_only = compile(
        "c02_durable_as_sink_only",
        r#"
use troupe_diagnostics_core::{
    hub::{AcceptedDiagnosticEvent, AdmissionReservation, AdmissionReserver, AdmissionSize,
          MandatoryDurableReserver, SinkOnlyDiagnosticHub},
    id::CanonicalUuid,
};
#[derive(Debug)] struct E;
impl std::fmt::Display for E { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("e") } }
impl std::error::Error for E {}
struct Token;
impl AdmissionReservation for Token { fn commit(self, _: AcceptedDiagnosticEvent) {} }
struct Durable;
impl AdmissionReserver for Durable {
    type Error = E; type Reservation = Token;
    fn try_reserve(&mut self, _: AdmissionSize) -> Result<Token, E> { Ok(Token) }
}
impl MandatoryDurableReserver for Durable {}
fn main() {
    let run = CanonicalUuid::parse("12345678-1234-4234-9234-123456789abc").unwrap();
    let _: SinkOnlyDiagnosticHub<Durable> = SinkOnlyDiagnosticHub::sink_only(run, Durable);
}
"#,
    );
    assert!(!durable_as_sink_only.status.success());
    let stderr = String::from_utf8(durable_as_sink_only.stderr).unwrap();
    assert!(stderr.contains("BoundedInMemoryReserver"), "{stderr}");

    let local_gap_as_candidate = compile(
        "c02_local_gap_as_candidate",
        r#"
use troupe_diagnostics_core::{
    hub::{AcceptedDiagnosticEvent, ActEventSubscriber, AdmissionReservation, AdmissionReserver,
          AdmissionSize, BoundedInMemoryReserver, DeliveryFailure, SinkOnlyDiagnosticHub,
          SubscriberLocalGap},
    id::CanonicalUuid,
};
#[derive(Debug)] struct E;
impl std::fmt::Display for E { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("e") } }
impl std::error::Error for E {}
struct Token;
impl AdmissionReservation for Token { fn commit(self, _: AcceptedDiagnosticEvent) {} }
struct Bounded;
impl AdmissionReserver for Bounded {
    type Error = E; type Reservation = Token;
    fn try_reserve(&mut self, _: AdmissionSize) -> Result<Token, E> { Ok(Token) }
}
impl BoundedInMemoryReserver for Bounded {}
struct Subscriber;
impl ActEventSubscriber for Subscriber {
    fn deliver(&self, _: AcceptedDiagnosticEvent) -> Result<(), DeliveryFailure> { Ok(()) }
}
fn main() {
    let run = CanonicalUuid::parse("12345678-1234-4234-9234-123456789abc").unwrap();
    let hub = SinkOnlyDiagnosticHub::sink_only(run, Bounded);
    let _ = hub.admit(SubscriberLocalGap::new(1, 1), &Subscriber);
}
"#,
    );
    fs::remove_dir_all(&scratch).unwrap();
    assert!(!local_gap_as_candidate.status.success());
    let stderr = String::from_utf8(local_gap_as_candidate.stderr).unwrap();
    assert!(
        stderr.contains("DiagnosticEventCandidate") || stderr.contains("FnOnce"),
        "{stderr}"
    );
}
