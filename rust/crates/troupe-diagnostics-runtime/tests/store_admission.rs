use std::{
    sync::{Arc, Barrier},
    thread,
};

use troupe_diagnostics_core::{
    event::{
        AgentMessageDelta, CounterSampled, DiagnosticEvent, DiagnosticEventHeader, DiagnosticScope,
        SpanFinished,
    },
    hub::{
        DeliveryFailure, DiagnosticEventCandidate, EventIdentity, HubAdmissionError,
        LiveEventNotifier, ProductionDiagnosticHub,
    },
    id::{CanonicalUuid, RunLocalId},
    kinds::{CounterKind, SpanOutcome},
    scalar::SchemaU64,
    time::ElapsedNs,
};
use troupe_diagnostics_runtime::store::{
    admission::{
        IngressAdmissionError, MAX_UNCOMMITTED_CANONICAL_BYTES, MAX_UNCOMMITTED_EVENTS,
        MandatoryIngress,
    },
    batch::EventBatch,
    watermark::CommittedWatermark,
};

const RUN: &str = "12345678-1234-4234-9234-123456789abc";

fn run_id() -> CanonicalUuid {
    CanonicalUuid::parse(RUN).unwrap()
}

fn header(identity: EventIdentity, elapsed_ns: u64) -> DiagnosticEventHeader {
    DiagnosticEventHeader::new(
        identity.run_id(),
        identity.sequence(),
        ElapsedNs::new(elapsed_ns),
        DiagnosticScope::new(None, None, None, None, None, None, None),
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

fn exact_size_message_candidate(target_canonical_bytes: usize) -> impl DiagnosticEventCandidate {
    move |identity| {
        let build = |text_delta| {
            DiagnosticEvent::AgentMessageDelta(AgentMessageDelta::new(
                header(identity, identity.sequence().get()),
                RunLocalId::parse("message-1").unwrap(),
                None,
                text_delta,
            ))
        };
        let base = serde_json::to_vec(&build(String::new())).unwrap().len();
        assert!(base <= target_canonical_bytes);
        let event = build("x".repeat(target_canonical_bytes - base));
        assert_eq!(
            serde_json::to_vec(&event).unwrap().len(),
            target_canonical_bytes
        );
        event
    }
}

fn invalid_finish_candidate() -> impl DiagnosticEventCandidate {
    move |identity| {
        DiagnosticEvent::SpanFinished(SpanFinished::new(
            header(identity, 1),
            SchemaU64::new(999),
            SpanOutcome::Completed,
            None,
        ))
    }
}

struct NoopLive;

impl LiveEventNotifier for NoopLive {
    fn notify(
        &mut self,
        _event: troupe_diagnostics_core::hub::AcceptedDiagnosticEvent,
    ) -> Result<(), DeliveryFailure> {
        Ok(())
    }
}

fn hub(ingress: MandatoryIngress) -> ProductionDiagnosticHub<MandatoryIngress> {
    ProductionDiagnosticHub::production(run_id(), ingress, Box::new(NoopLive))
}

#[test]
fn event_budget_is_exact_atomic_and_signals_once_under_concurrent_producers() {
    let (ingress, failures) = MandatoryIngress::new();
    let hub = Arc::new(hub(ingress.clone()));

    for elapsed in 1..=(MAX_UNCOMMITTED_EVENTS - 8) {
        hub.admit(counter_candidate(elapsed as u64), None).unwrap();
    }

    let barrier = Arc::new(Barrier::new(17));
    let mut workers = Vec::new();
    for offset in 0..16_u64 {
        let hub = Arc::clone(&hub);
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            barrier.wait();
            hub.admit(counter_candidate(40_000 + offset), None)
        }));
    }
    barrier.wait();

    let mut accepted = Vec::new();
    let mut rejected = 0;
    for worker in workers {
        match worker.join().unwrap() {
            Ok(receipt) => accepted.push(receipt.accepted().identity().sequence().get()),
            Err(HubAdmissionError::Reservation(
                IngressAdmissionError::BudgetExhausted(_) | IngressAdmissionError::Sealed(_),
            )) => rejected += 1,
            Err(other) => panic!("unexpected concurrent admission error: {other}"),
        }
    }
    accepted.sort_unstable();
    assert_eq!(
        accepted,
        ((MAX_UNCOMMITTED_EVENTS - 7) as u64..=32_768).collect::<Vec<_>>()
    );
    assert_eq!(rejected, 8);

    let status = ingress.status().unwrap();
    assert_eq!(status.accepted_uncommitted_events(), MAX_UNCOMMITTED_EVENTS);
    assert_eq!(status.queued_events(), MAX_UNCOMMITTED_EVENTS);
    assert_eq!(status.in_flight_events(), 0);
    assert!(status.normal_ingress_sealed());
    let failure = failures.try_recv().expect("the first overflow must signal");
    assert_eq!(failure.code(), "mandatory_ingress_budget_exhausted");
    assert!(failure.event_limit_exceeded());
    assert!(!failure.byte_limit_exceeded());
    assert!(failures.try_recv().is_none());

    let first = ingress.try_dequeue().unwrap().unwrap();
    assert_eq!(first.identity().sequence().get(), 1);
    assert_eq!(
        ingress.status().unwrap().accepted_uncommitted_events(),
        MAX_UNCOMMITTED_EVENTS
    );
    assert!(matches!(
        hub.admit_fatal(counter_candidate(50_000), None),
        Err(HubAdmissionError::Reservation(
            IngressAdmissionError::FatalCapacityUnavailable
        ))
    ));

    let batch = EventBatch::new(vec![first]).unwrap();
    let notification = CommittedWatermark::fresh(run_id())
        .candidate(&batch)
        .unwrap();
    ingress.mark_committed(notification).unwrap();
    assert_eq!(
        ingress.status().unwrap().accepted_uncommitted_events(),
        MAX_UNCOMMITTED_EVENTS - 1
    );

    assert!(matches!(
        hub.admit(counter_candidate(50_001), None),
        Err(HubAdmissionError::Reservation(
            IngressAdmissionError::Sealed(_)
        ))
    ));
    assert!(matches!(
        hub.admit_fatal(invalid_finish_candidate(), None),
        Err(HubAdmissionError::Reference(_))
    ));
    let fatal = hub.admit_fatal(counter_candidate(50_002), None).unwrap();
    assert_eq!(fatal.accepted().identity().sequence().get(), 32_769);
    assert!(matches!(
        hub.admit_fatal(counter_candidate(50_003), None),
        Err(HubAdmissionError::Reservation(
            IngressAdmissionError::FatalAlreadyAdmitted
        ))
    ));
    assert!(failures.try_recv().is_none());
}

#[test]
fn canonical_byte_budget_accepts_equality_and_rejects_one_more_byte() {
    let (ingress, failures) = MandatoryIngress::new();
    let hub = hub(ingress.clone());
    const EVENT_BYTES: usize = 1024 * 1024;
    const EVENT_COUNT: usize = MAX_UNCOMMITTED_CANONICAL_BYTES / EVENT_BYTES;

    for _ in 0..EVENT_COUNT {
        hub.admit(exact_size_message_candidate(EVENT_BYTES), None)
            .unwrap();
    }
    let exact = ingress.status().unwrap();
    assert_eq!(exact.accepted_uncommitted_events(), EVENT_COUNT);
    assert_eq!(
        exact.accepted_uncommitted_canonical_bytes(),
        MAX_UNCOMMITTED_CANONICAL_BYTES
    );

    assert!(matches!(
        hub.admit(exact_size_message_candidate(1_024), None),
        Err(HubAdmissionError::Reservation(
            IngressAdmissionError::BudgetExhausted(_)
        ))
    ));
    let failure = failures.try_recv().unwrap();
    assert!(!failure.event_limit_exceeded());
    assert!(failure.byte_limit_exceeded());
    assert!(failures.try_recv().is_none());
}

#[test]
fn validation_failure_releases_reservation_without_sequence_or_queue_effects() {
    let (ingress, failures) = MandatoryIngress::new();
    let hub = hub(ingress.clone());

    assert!(matches!(
        hub.admit(invalid_finish_candidate(), None),
        Err(HubAdmissionError::Reference(_))
    ));
    let empty = ingress.status().unwrap();
    assert_eq!(empty.accepted_uncommitted_events(), 0);
    assert_eq!(empty.queued_events(), 0);
    assert_eq!(empty.in_flight_events(), 0);
    assert!(failures.try_recv().is_none());

    let accepted = hub.admit(counter_candidate(2), None).unwrap();
    assert_eq!(accepted.accepted().identity().sequence().get(), 1);
    assert_eq!(
        ingress
            .try_dequeue()
            .unwrap()
            .unwrap()
            .identity()
            .sequence()
            .get(),
        1
    );
}

#[test]
fn queued_and_in_flight_tail_remain_charged_until_matching_commit() {
    let (ingress, failures) = MandatoryIngress::new();
    let hub = hub(ingress.clone());
    for elapsed in 1..=3 {
        hub.admit(counter_candidate(elapsed), None).unwrap();
    }

    let first = ingress.try_dequeue().unwrap().unwrap();
    let second = ingress.try_dequeue().unwrap().unwrap();
    let before_retry = ingress.status().unwrap();
    assert_eq!(before_retry.accepted_uncommitted_events(), 3);
    assert_eq!(before_retry.queued_events(), 1);
    assert_eq!(before_retry.in_flight_events(), 2);
    assert_eq!(
        ingress
            .retry_in_flight()
            .unwrap()
            .iter()
            .map(|event| event.identity().sequence().get())
            .collect::<Vec<_>>(),
        vec![1, 2]
    );

    let batch = EventBatch::new(vec![first, second]).unwrap();
    let notification = CommittedWatermark::fresh(run_id())
        .candidate(&batch)
        .unwrap();
    ingress.mark_committed(notification).unwrap();
    let after_commit = ingress.status().unwrap();
    assert_eq!(after_commit.accepted_uncommitted_events(), 1);
    assert_eq!(after_commit.queued_events(), 1);
    assert_eq!(after_commit.in_flight_events(), 0);
    assert_eq!(after_commit.committed_sequence(), 2);
    assert!(failures.try_recv().is_none());
}

#[test]
fn separate_mandatory_ingresses_have_independent_budgets_and_failure_latches() {
    let (first, first_failures) = MandatoryIngress::new();
    let (second, second_failures) = MandatoryIngress::new();
    let first_hub = hub(first.clone());
    let second_hub = hub(second.clone());

    for elapsed in 1..=MAX_UNCOMMITTED_EVENTS {
        first_hub
            .admit(counter_candidate(elapsed as u64), None)
            .unwrap();
    }
    assert!(first_hub.admit(counter_candidate(40_000), None).is_err());
    assert!(first_failures.try_recv().is_some());

    let receipt = second_hub.admit(counter_candidate(1), None).unwrap();
    assert_eq!(receipt.accepted().identity().sequence().get(), 1);
    assert_eq!(second.status().unwrap().accepted_uncommitted_events(), 1);
    assert!(!second.status().unwrap().normal_ingress_sealed());
    assert!(second_failures.try_recv().is_none());
}

#[test]
fn graceful_seal_forbids_fatal_admission_but_external_core_failure_authorizes_it() {
    let (graceful, graceful_failures) = MandatoryIngress::new();
    let graceful_hub = hub(graceful.clone());
    assert!(graceful.seal_normal_ingress().unwrap());
    assert!(matches!(
        graceful_hub.admit(counter_candidate(1), None),
        Err(HubAdmissionError::Reservation(
            IngressAdmissionError::Sealed(None)
        ))
    ));
    assert!(matches!(
        graceful_hub.admit_fatal(counter_candidate(2), None),
        Err(HubAdmissionError::Reservation(
            IngressAdmissionError::FatalBeforeCoreFailure
        ))
    ));
    assert!(graceful_failures.try_recv().is_none());

    let (failed, failed_signals) = MandatoryIngress::new();
    let failed_hub = hub(failed.clone());
    assert!(failed.seal_for_external_core_failure().unwrap());
    assert!(!failed.seal_for_external_core_failure().unwrap());
    assert!(matches!(
        failed_hub.admit(counter_candidate(3), None),
        Err(HubAdmissionError::Reservation(
            IngressAdmissionError::Sealed(None)
        ))
    ));
    let fatal = failed_hub.admit_fatal(counter_candidate(4), None).unwrap();
    assert_eq!(fatal.accepted().identity().sequence().get(), 1);
    assert!(failed_signals.try_recv().is_none());
}
