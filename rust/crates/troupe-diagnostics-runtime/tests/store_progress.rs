use std::time::Duration;

use troupe_diagnostics_runtime::store::progress::{
    DEFAULT_SHUTDOWN_DRAIN_TIMEOUT, DEFAULT_WRITER_STALL_TIMEOUT, DrainState,
    ProgressObservationError, WriterDeadlines, WriterFailureCode, WriterFailureStage,
    WriterProgressSample, WriterProgressSupervisor, WriterTaskOutcome,
};

fn sample(committed_sequence: u64, accepted_tail_events: usize) -> WriterProgressSample {
    WriterProgressSample::new(committed_sequence, accepted_tail_events)
}

#[test]
fn deadlines_are_positive_finite_and_visible_in_status() {
    let defaults = WriterDeadlines::default();
    assert_eq!(defaults.writer_stall_timeout(), Duration::from_secs(10));
    assert_eq!(defaults.shutdown_drain_timeout(), Duration::from_secs(30));
    assert_eq!(
        defaults.writer_stall_timeout(),
        DEFAULT_WRITER_STALL_TIMEOUT
    );
    assert_eq!(
        defaults.shutdown_drain_timeout(),
        DEFAULT_SHUTDOWN_DRAIN_TIMEOUT
    );

    assert!(WriterDeadlines::new(Duration::ZERO, Duration::from_secs(1)).is_err());
    assert!(WriterDeadlines::new(Duration::from_secs(1), Duration::ZERO).is_err());

    let custom = WriterDeadlines::new(Duration::from_millis(125), Duration::from_secs(7)).unwrap();
    let supervisor = WriterProgressSupervisor::new(custom);
    let status = supervisor.status();
    assert_eq!(status.deadlines(), custom);
    assert_eq!(status.writer_stall_timeout(), Duration::from_millis(125));
    assert_eq!(status.shutdown_drain_timeout(), Duration::from_secs(7));
}

#[test]
fn idle_never_stalls_and_continuous_watermark_progress_resets_the_deadline() {
    let mut idle = WriterProgressSupervisor::default();
    assert_eq!(idle.observe(Duration::ZERO, sample(0, 0)).unwrap(), None);
    assert_eq!(
        idle.observe(Duration::from_secs(10_000), sample(0, 0))
            .unwrap(),
        None
    );
    assert_eq!(idle.status().stalled_for(), None);

    let mut progressing = WriterProgressSupervisor::default();
    assert_eq!(
        progressing.observe(Duration::ZERO, sample(0, 4)).unwrap(),
        None
    );
    for sequence in 1..=100_u64 {
        let now = Duration::from_secs(sequence * 9);
        assert_eq!(progressing.observe(now, sample(sequence, 4)).unwrap(), None);
        assert_eq!(progressing.status().stalled_for(), Some(Duration::ZERO));
    }
    assert_eq!(progressing.status().failure(), None);
}

#[test]
fn accepted_tail_stalls_at_the_exact_boundary_and_latches_once() {
    let mut supervisor = WriterProgressSupervisor::default();
    assert_eq!(
        supervisor.observe(Duration::ZERO, sample(0, 1)).unwrap(),
        None
    );
    assert_eq!(
        supervisor
            .observe(
                DEFAULT_WRITER_STALL_TIMEOUT - Duration::from_nanos(1),
                sample(0, 1),
            )
            .unwrap(),
        None
    );

    let failure = supervisor
        .observe(DEFAULT_WRITER_STALL_TIMEOUT, sample(0, 1))
        .unwrap()
        .expect("exact deadline must fail");
    assert_eq!(failure.component(), "writer");
    assert_eq!(failure.stage(), WriterFailureStage::Progress);
    assert_eq!(failure.code(), WriterFailureCode::ProgressStalled);
    assert_eq!(failure.code().as_str(), "writer_progress_stalled");
    assert_eq!(
        supervisor
            .observe(Duration::from_secs(100), sample(0, 1))
            .unwrap(),
        None
    );
    assert_eq!(supervisor.status().failure(), Some(failure));
}

#[test]
fn writer_task_outcomes_have_stable_distinct_stage_and_error_codes() {
    let expected = [
        (
            WriterTaskOutcome::Exited,
            WriterFailureStage::TaskExit,
            WriterFailureCode::UnexpectedExit,
            "writer_unexpected_exit",
        ),
        (
            WriterTaskOutcome::Panicked,
            WriterFailureStage::TaskExit,
            WriterFailureCode::Panicked,
            "writer_panicked",
        ),
        (
            WriterTaskOutcome::CommitUnavailable,
            WriterFailureStage::Commit,
            WriterFailureCode::CommitUnavailable,
            "writer_commit_unavailable",
        ),
        (
            WriterTaskOutcome::FlushUnavailable,
            WriterFailureStage::Flush,
            WriterFailureCode::FlushUnavailable,
            "writer_flush_unavailable",
        ),
        (
            WriterTaskOutcome::StorageUnavailable,
            WriterFailureStage::Storage,
            WriterFailureCode::StorageUnavailable,
            "writer_storage_unavailable",
        ),
    ];

    for (outcome, stage, code, wire_code) in expected {
        let mut supervisor = WriterProgressSupervisor::default();
        let failure = supervisor
            .report_writer_outcome(Duration::ZERO, sample(0, 0), outcome)
            .unwrap()
            .expect("failure outcome must signal");
        assert_eq!(failure.component(), "writer");
        assert_eq!(failure.stage(), stage);
        assert_eq!(failure.code(), code);
        assert_eq!(failure.code().as_str(), wire_code);
        assert_eq!(
            supervisor
                .report_writer_outcome(Duration::ZERO, sample(0, 0), outcome)
                .unwrap(),
            None
        );
    }

    let mut expected_exit = WriterProgressSupervisor::default();
    expected_exit.observe(Duration::ZERO, sample(0, 1)).unwrap();
    assert_eq!(
        expected_exit.begin_shutdown(Duration::ZERO).unwrap(),
        DrainState::Draining
    );
    assert_eq!(
        expected_exit
            .report_writer_outcome(
                Duration::from_millis(1),
                sample(1, 0),
                WriterTaskOutcome::Exited,
            )
            .unwrap(),
        None
    );
    assert_eq!(expected_exit.status().drain_state(), DrainState::Drained);
    assert_eq!(expected_exit.status().failure(), None);
}

#[test]
fn shutdown_drain_has_a_total_deadline_and_zero_tail_wins_at_the_boundary() {
    let deadlines =
        WriterDeadlines::new(Duration::from_secs(100), Duration::from_secs(30)).unwrap();
    let mut timed_out = WriterProgressSupervisor::new(deadlines);
    timed_out.observe(Duration::ZERO, sample(0, 3)).unwrap();
    assert_eq!(
        timed_out.begin_shutdown(Duration::ZERO).unwrap(),
        DrainState::Draining
    );
    for (now, watermark) in [(10, 1), (20, 2)] {
        assert_eq!(
            timed_out
                .observe(Duration::from_secs(now), sample(watermark, 3))
                .unwrap(),
            None
        );
    }
    let failure = timed_out
        .observe(Duration::from_secs(30), sample(3, 3))
        .unwrap()
        .expect("total drain deadline must fail despite writer progress");
    assert_eq!(failure.stage(), WriterFailureStage::Drain);
    assert_eq!(failure.code(), WriterFailureCode::ShutdownDrainTimedOut);
    assert_eq!(failure.code().as_str(), "writer_shutdown_drain_timed_out");
    assert_eq!(timed_out.status().drain_state(), DrainState::TimedOut);
    assert!(!timed_out.status().drain_complete());

    let mut drained = WriterProgressSupervisor::new(deadlines);
    drained.observe(Duration::ZERO, sample(0, 1)).unwrap();
    drained.begin_shutdown(Duration::ZERO).unwrap();
    assert_eq!(
        drained
            .observe(Duration::from_secs(30), sample(1, 0))
            .unwrap(),
        None
    );
    assert_eq!(drained.status().drain_state(), DrainState::Drained);
    assert!(drained.status().drain_complete());
    assert_eq!(drained.status().failure(), None);
}

#[test]
fn first_core_failure_wins_and_clock_regression_does_not_mutate_progress() {
    let mut supervisor = WriterProgressSupervisor::default();
    supervisor
        .observe(Duration::from_secs(5), sample(7, 1))
        .unwrap();
    assert_eq!(
        supervisor.observe(Duration::from_secs(4), sample(8, 1)),
        Err(ProgressObservationError::ClockRegressed)
    );
    assert_eq!(supervisor.status().committed_sequence(), 7);

    let first = supervisor
        .report_writer_outcome(
            Duration::from_secs(5),
            sample(7, 1),
            WriterTaskOutcome::Panicked,
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        supervisor
            .report_writer_outcome(
                Duration::from_secs(5),
                sample(7, 1),
                WriterTaskOutcome::StorageUnavailable,
            )
            .unwrap(),
        None
    );
    assert_eq!(
        supervisor
            .observe(Duration::from_secs(100), sample(7, 1))
            .unwrap(),
        None
    );
    assert_eq!(supervisor.status().failure(), Some(first));
}
