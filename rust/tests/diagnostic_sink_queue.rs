use std::fs;
use std::sync::{Arc, Barrier};
use std::thread;

#[allow(dead_code)]
#[path = "../src/diagnostic_sink/budget.rs"]
mod budget;
#[allow(dead_code)]
#[path = "../src/diagnostic_sink/queue.rs"]
mod queue;

use budget::{
    BudgetError, BudgetLimits, BudgetUsage, RUNTIME_MAX_ENCODED_BYTES, RUNTIME_MAX_EVENTS,
    RuntimeBudget,
};
use queue::{
    AdmissionClass, AdmissionOutcome, DropReason, PER_SINK_MAX_ENCODED_BYTES, PER_SINK_MAX_EVENTS,
    QueueEvent, STRUCTURAL_RESERVE_ENCODED_BYTES, STRUCTURAL_RESERVE_EVENTS, SinkQueue,
    SinkQueueLimits, SinkTerminalReason,
};
use troupe_diagnostics_core::event::DiagnosticEventKind;

fn usage(events: usize, encoded_bytes: usize) -> BudgetUsage {
    BudgetUsage::new(events, encoded_bytes)
}

fn event(
    value: &'static str,
    kind: DiagnosticEventKind,
    encoded_bytes: usize,
    class: AdmissionClass,
) -> QueueEvent<&'static str> {
    QueueEvent::new(value, kind, encoded_bytes, class)
}

fn test_queue(
    runtime: RuntimeBudget,
    total: BudgetUsage,
    structural_reserve: BudgetUsage,
) -> SinkQueue<&'static str> {
    SinkQueue::with_limits(runtime, SinkQueueLimits::new(total, structural_reserve))
}

#[test]
fn frozen_product_limits_are_exact() {
    assert_eq!(PER_SINK_MAX_EVENTS, 1_024);
    assert_eq!(PER_SINK_MAX_ENCODED_BYTES, 8 * 1024 * 1024);
    assert_eq!(STRUCTURAL_RESERVE_EVENTS, 32);
    assert_eq!(STRUCTURAL_RESERVE_ENCODED_BYTES, 256 * 1024);
    assert_eq!(RUNTIME_MAX_EVENTS, 16_384);
    assert_eq!(RUNTIME_MAX_ENCODED_BYTES, 64 * 1024 * 1024);

    let runtime = RuntimeBudget::new();
    assert_eq!(
        runtime.limits(),
        BudgetLimits::new(RUNTIME_MAX_EVENTS, RUNTIME_MAX_ENCODED_BYTES)
    );
    let queue = SinkQueue::<()>::new(runtime);
    assert_eq!(
        queue.limits(),
        SinkQueueLimits::new(
            usage(PER_SINK_MAX_EVENTS, PER_SINK_MAX_ENCODED_BYTES),
            usage(STRUCTURAL_RESERVE_EVENTS, STRUCTURAL_RESERVE_ENCODED_BYTES,),
        )
    );
    assert_eq!(
        queue.limits().structural_reserve(),
        usage(STRUCTURAL_RESERVE_EVENTS, STRUCTURAL_RESERVE_ENCODED_BYTES,)
    );
}

#[test]
fn runtime_admission_commits_event_and_byte_dimensions_atomically() {
    let runtime = RuntimeBudget::with_limits(BudgetLimits::new(2, 10));

    assert_eq!(
        runtime.try_replace(BudgetUsage::ZERO, usage(1, 7)),
        Ok(usage(1, 7))
    );
    assert!(matches!(
        runtime.try_replace(BudgetUsage::ZERO, usage(1, 4)),
        Err(BudgetError::LimitExceeded { .. })
    ));
    assert_eq!(runtime.usage(), usage(1, 7));

    assert_eq!(
        runtime.try_replace(usage(1, 7), usage(2, 10)),
        Ok(usage(2, 10))
    );
    assert!(matches!(
        runtime.try_replace(BudgetUsage::ZERO, usage(1, 0)),
        Err(BudgetError::LimitExceeded { .. })
    ));
    assert_eq!(runtime.usage(), usage(2, 10));
}

#[test]
fn content_cannot_consume_the_structural_reserve_and_eviction_is_fifo() {
    let runtime = RuntimeBudget::with_limits(BudgetLimits::new(16, 160));
    let queue = test_queue(runtime.clone(), usage(4, 40), usage(1, 10));

    for queued in [
        event(
            "message",
            DiagnosticEventKind::AgentMessageDelta,
            10,
            AdmissionClass::Content,
        ),
        event(
            "plan",
            DiagnosticEventKind::AgentPlanSnapshot,
            10,
            AdmissionClass::Content,
        ),
        event(
            "context",
            DiagnosticEventKind::ContextUsageSampled,
            10,
            AdmissionClass::Content,
        ),
    ] {
        assert_eq!(
            queue.try_admit(queued),
            AdmissionOutcome::Enqueued { evicted: vec![] }
        );
    }

    let outcome = queue.try_admit(event(
        "counter",
        DiagnosticEventKind::CounterSampled,
        10,
        AdmissionClass::Content,
    ));
    let AdmissionOutcome::Enqueued { evicted } = outcome else {
        panic!("content replacement should be admitted");
    };
    assert_eq!(evicted.len(), 1);
    assert_eq!(
        evicted[0].event_kind(),
        DiagnosticEventKind::AgentMessageDelta
    );

    let delivery = queue
        .try_begin_callback()
        .expect("queue access")
        .expect("queued plan");
    assert_eq!(*delivery.item(), "plan");
    assert_eq!(
        delivery.event_kind(),
        DiagnosticEventKind::AgentPlanSnapshot
    );
    assert_eq!(delivery.encoded_bytes(), 10);
    let (delivery_ticket, delivered_item) = delivery.into_parts();
    assert_eq!(delivered_item, "plan");

    let outcome = queue.try_admit(event(
        "completed",
        DiagnosticEventKind::AgentMessageCompleted,
        20,
        AdmissionClass::Structural,
    ));
    let AdmissionOutcome::Enqueued { evicted } = outcome else {
        panic!("structural event should use the reserve");
    };
    assert_eq!(evicted.len(), 1);
    assert_eq!(
        evicted[0].event_kind(),
        DiagnosticEventKind::ContextUsageSampled
    );

    let snapshot = queue.try_snapshot().expect("queue access");
    assert_eq!(snapshot.queued_events(), 2);
    assert!(snapshot.callback_active());
    assert_eq!(snapshot.total_usage(), usage(3, 40));
    assert_eq!(snapshot.content_usage(), usage(2, 20));
    assert_eq!(runtime.usage(), usage(3, 40));

    queue
        .try_complete_callback(delivery_ticket)
        .expect("finish callback");
    assert_eq!(runtime.usage(), usage(2, 30));
}

#[test]
fn callback_owned_events_stay_in_both_sink_and_runtime_budgets() {
    let runtime = RuntimeBudget::with_limits(BudgetLimits::new(1, 10));
    let first = test_queue(runtime.clone(), usage(2, 20), BudgetUsage::ZERO);
    let second = test_queue(runtime.clone(), usage(2, 20), BudgetUsage::ZERO);

    assert!(matches!(
        first.try_admit(event(
            "first",
            DiagnosticEventKind::AgentMessageDelta,
            10,
            AdmissionClass::Content,
        )),
        AdmissionOutcome::Enqueued { .. }
    ));
    let delivery = first
        .try_begin_callback()
        .expect("queue access")
        .expect("first callback");

    assert_eq!(runtime.usage(), usage(1, 10));
    assert_eq!(
        second.try_admit(event(
            "second",
            DiagnosticEventKind::AgentPlanSnapshot,
            1,
            AdmissionClass::Content,
        )),
        AdmissionOutcome::Dropped {
            reason: DropReason::RuntimeCapacity,
            delta: queue::DropDelta::new(DiagnosticEventKind::AgentPlanSnapshot, 1, 1),
        }
    );
    assert_eq!(runtime.usage(), usage(1, 10));

    first
        .try_complete_callback(delivery.ticket())
        .expect("finish callback");
    assert!(matches!(
        second.try_admit(event(
            "second",
            DiagnosticEventKind::AgentPlanSnapshot,
            1,
            AdmissionClass::Content,
        )),
        AdmissionOutcome::Enqueued { .. }
    ));
}

#[test]
fn structural_exhaustion_terminalizes_only_the_affected_sink() {
    let runtime = RuntimeBudget::with_limits(BudgetLimits::new(8, 80));
    let affected = test_queue(runtime.clone(), usage(2, 20), usage(1, 10));
    let independent = test_queue(runtime.clone(), usage(2, 20), usage(1, 10));

    assert!(matches!(
        affected.try_admit(event(
            "active",
            DiagnosticEventKind::SpanStarted,
            10,
            AdmissionClass::Structural,
        )),
        AdmissionOutcome::Enqueued { .. }
    ));
    let delivery = affected
        .try_begin_callback()
        .expect("queue access")
        .expect("active callback");
    assert!(matches!(
        affected.try_admit(event(
            "queued",
            DiagnosticEventKind::SpanFinished,
            10,
            AdmissionClass::Structural,
        )),
        AdmissionOutcome::Enqueued { .. }
    ));

    let outcome = affected.try_admit(event(
        "overflow",
        DiagnosticEventKind::ObservationGap,
        1,
        AdmissionClass::Structural,
    ));
    let AdmissionOutcome::Terminalized { dropped } = outcome else {
        panic!("structural reserve exhaustion must terminalize the sink");
    };
    assert_eq!(
        dropped
            .iter()
            .map(|delta| delta.event_kind())
            .collect::<Vec<_>>(),
        vec![
            DiagnosticEventKind::SpanFinished,
            DiagnosticEventKind::ObservationGap,
        ]
    );

    let snapshot = affected.try_snapshot().expect("queue access");
    assert_eq!(
        snapshot.terminal_reason(),
        Some(SinkTerminalReason::DeliveryOverflow)
    );
    assert_eq!(snapshot.queued_events(), 0);
    assert!(snapshot.callback_active());
    assert_eq!(snapshot.total_usage(), usage(1, 10));
    assert_eq!(runtime.usage(), usage(1, 10));

    assert!(matches!(
        independent.try_admit(event(
            "independent",
            DiagnosticEventKind::AgentMessageDelta,
            10,
            AdmissionClass::Content,
        )),
        AdmissionOutcome::Enqueued { .. }
    ));
    assert_eq!(
        independent
            .try_snapshot()
            .expect("queue access")
            .terminal_reason(),
        None
    );

    let accumulated = affected.drop_snapshot();
    assert_eq!(accumulated.len(), 2);
    assert_eq!(
        accumulated
            .iter()
            .map(|delta| delta.events())
            .sum::<usize>(),
        2
    );
    assert_eq!(
        accumulated
            .iter()
            .map(|delta| delta.encoded_bytes())
            .sum::<usize>(),
        11
    );

    affected
        .try_complete_callback(delivery.ticket())
        .expect("finish callback");
    assert_eq!(runtime.usage(), usage(1, 10));
}

#[test]
fn concurrent_structural_one_over_has_one_terminal_transition_and_exact_drops() {
    const CONTENDERS: usize = 8;
    const ROUNDS: usize = 32;

    for _ in 0..ROUNDS {
        let runtime = RuntimeBudget::with_limits(BudgetLimits::new(1, 1));
        let queue = Arc::new(test_queue(runtime.clone(), usage(1, 1), usage(1, 1)));
        assert!(matches!(
            queue.try_admit(event(
                "reserved",
                DiagnosticEventKind::SpanStarted,
                1,
                AdmissionClass::Structural,
            )),
            AdmissionOutcome::Enqueued { .. }
        ));

        let barrier = Arc::new(Barrier::new(CONTENDERS + 1));
        let workers = (0..CONTENDERS)
            .map(|_| {
                let queue = Arc::clone(&queue);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    queue.try_admit(event(
                        "one-over",
                        DiagnosticEventKind::ObservationGap,
                        1,
                        AdmissionClass::Structural,
                    ))
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let outcomes = workers
            .into_iter()
            .map(|worker| worker.join().expect("structural admission worker"))
            .collect::<Vec<_>>();

        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, AdmissionOutcome::Terminalized { .. }))
                .count(),
            1
        );
        let snapshot = queue.try_snapshot().expect("queue access");
        assert_eq!(
            snapshot.terminal_reason(),
            Some(SinkTerminalReason::DeliveryOverflow)
        );
        assert_eq!(snapshot.total_usage(), BudgetUsage::ZERO);
        assert_eq!(runtime.usage(), BudgetUsage::ZERO);

        let drops = queue.drop_snapshot();
        assert_eq!(
            drops,
            vec![
                queue::DropDelta::new(DiagnosticEventKind::SpanStarted, 1, 1),
                queue::DropDelta::new(DiagnosticEventKind::ObservationGap, CONTENDERS, CONTENDERS,),
            ]
        );
    }
}

#[test]
fn typed_drop_deltas_accumulate_without_losing_eviction_order() {
    let runtime = RuntimeBudget::with_limits(BudgetLimits::new(8, 80));
    let queue = test_queue(runtime, usage(2, 10), BudgetUsage::ZERO);

    for queued in [
        event(
            "delta",
            DiagnosticEventKind::AgentMessageDelta,
            4,
            AdmissionClass::Content,
        ),
        event(
            "plan",
            DiagnosticEventKind::AgentPlanSnapshot,
            4,
            AdmissionClass::Content,
        ),
    ] {
        assert!(matches!(
            queue.try_admit(queued),
            AdmissionOutcome::Enqueued { .. }
        ));
    }

    let outcome = queue.try_admit(event(
        "replacement",
        DiagnosticEventKind::AgentMessageCompleted,
        7,
        AdmissionClass::Content,
    ));
    let AdmissionOutcome::Enqueued { evicted } = outcome else {
        panic!("replacement should fit after deterministic evictions");
    };
    assert_eq!(
        evicted
            .iter()
            .map(|delta| delta.event_kind())
            .collect::<Vec<_>>(),
        vec![
            DiagnosticEventKind::AgentMessageDelta,
            DiagnosticEventKind::AgentPlanSnapshot,
        ]
    );

    assert_eq!(queue.drop_snapshot(), evicted);
}

#[test]
fn explicit_discard_and_queue_drop_release_runtime_usage() {
    let runtime = RuntimeBudget::with_limits(BudgetLimits::new(8, 80));
    {
        let queue = test_queue(runtime.clone(), usage(4, 40), BudgetUsage::ZERO);
        for queued in [
            event(
                "delta",
                DiagnosticEventKind::AgentMessageDelta,
                4,
                AdmissionClass::Content,
            ),
            event(
                "plan",
                DiagnosticEventKind::AgentPlanSnapshot,
                5,
                AdmissionClass::Content,
            ),
        ] {
            assert!(matches!(
                queue.try_admit(queued),
                AdmissionOutcome::Enqueued { .. }
            ));
        }
        assert_eq!(runtime.usage(), usage(2, 9));

        let discarded = queue.try_discard_queued().expect("discard queued events");
        assert_eq!(discarded.len(), 2);
        assert_eq!(runtime.usage(), BudgetUsage::ZERO);

        assert!(matches!(
            queue.try_admit(event(
                "remaining",
                DiagnosticEventKind::ContextUsageSampled,
                7,
                AdmissionClass::Content,
            )),
            AdmissionOutcome::Enqueued { .. }
        ));
        assert_eq!(runtime.usage(), usage(1, 7));
    }
    assert_eq!(runtime.usage(), BudgetUsage::ZERO);
}

#[test]
fn queue_and_budget_sources_have_no_async_python_or_blocking_lock_path() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for relative in [
        "src/diagnostic_sink/queue.rs",
        "src/diagnostic_sink/budget.rs",
    ] {
        let source = fs::read_to_string(root.join(relative)).expect("read diagnostic sink source");
        for forbidden in ["async fn", ".await", "Python<'", "pyo3::", ".lock()"] {
            assert!(
                !source.contains(forbidden),
                "{relative} contains forbidden canonical-path marker {forbidden}"
            );
        }
    }
    let queue_source =
        fs::read_to_string(root.join("src/diagnostic_sink/queue.rs")).expect("read queue source");
    assert!(queue_source.contains("try_lock()"));
}
