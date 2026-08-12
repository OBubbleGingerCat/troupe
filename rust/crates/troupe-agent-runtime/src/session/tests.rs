use tokio::io::AsyncReadExt as _;
use tokio::sync::oneshot;

use super::*;

#[test]
fn opening_retry_window_uses_half_to_full_exponential_range_with_cap() {
    assert_eq!(opening_retry_window_ms(0), (125, 250));
    assert_eq!(opening_retry_window_ms(1), (250, 500));
    assert_eq!(opening_retry_window_ms(6), (8_000, 16_000));
    assert_eq!(opening_retry_window_ms(7), (15_000, 30_000));
    assert_eq!(opening_retry_window_ms(u32::MAX), (15_000, 30_000));
}

#[test]
fn opening_retry_sampling_rejects_the_biased_prefix_and_keeps_boundaries() {
    let mut words = [15_u64, 16].into_iter();
    assert_eq!(
        sample_opening_retry_delay_ms(0, || words.next().ok_or(())),
        Ok(141)
    );
    assert_eq!(
        sample_opening_retry_delay_ms(0, || Ok::<u64, ()>(126)),
        Ok(125)
    );
    assert_eq!(
        sample_opening_retry_delay_ms(0, || Ok::<u64, ()>(125)),
        Ok(250)
    );
}

#[tokio::test]
async fn npx_preparation_waiters_release_only_after_initialize_succeeds() {
    let gate = NpxPreparationGate::new();
    let cancellation = CancellationToken::new();
    let leader = match gate.enter(&cancellation).await {
        NpxPreparationAdmission::Leader(leader) => leader,
        _ => panic!("the first preparation admission must lead"),
    };
    let waiter = tokio::spawn({
        let gate = Arc::clone(&gate);
        let cancellation = cancellation.clone();
        async move { gate.enter(&cancellation).await }
    });
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());

    leader.mark_ready();

    assert!(matches!(
        waiter.await.unwrap(),
        NpxPreparationAdmission::Prepared
    ));
}

#[tokio::test]
async fn abandoned_npx_preparation_leader_hands_leadership_to_a_waiter() {
    let gate = NpxPreparationGate::new();
    let cancellation = CancellationToken::new();
    let leader = match gate.enter(&cancellation).await {
        NpxPreparationAdmission::Leader(leader) => leader,
        _ => panic!("the first preparation admission must lead"),
    };
    let waiter = tokio::spawn({
        let gate = Arc::clone(&gate);
        let cancellation = cancellation.clone();
        async move { gate.enter(&cancellation).await }
    });
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());

    drop(leader);

    assert!(matches!(
        waiter.await.unwrap(),
        NpxPreparationAdmission::Leader(_)
    ));
}

#[tokio::test]
async fn deterministic_npx_preparation_failure_is_cached_for_waiters() {
    let gate = NpxPreparationGate::new();
    let cancellation = CancellationToken::new();
    let leader = match gate.enter(&cancellation).await {
        NpxPreparationAdmission::Leader(leader) => leader,
        _ => panic!("the first preparation admission must lead"),
    };
    let waiter = tokio::spawn({
        let gate = Arc::clone(&gate);
        let cancellation = cancellation.clone();
        async move { gate.enter(&cancellation).await }
    });
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());

    let failure = preparation_failure();
    leader.mark_failed(failure.clone());

    match waiter.await.unwrap() {
        NpxPreparationAdmission::Failed(observed) => assert_eq!(observed, failure),
        _ => panic!("a deterministic preparation failure must be shared"),
    }
    match gate.enter(&cancellation).await {
        NpxPreparationAdmission::Failed(observed) => assert_eq!(observed, failure),
        _ => panic!("the gate must retain its deterministic failure"),
    }
}

#[test]
fn npx_preparation_classifier_accepts_only_closed_npm_code_records() {
    let mut transient = NpxPreparationStderrClassifier::default();
    transient.push(b"npm error co");
    transient.push(b"de EAI_AGAIN\n");
    assert_eq!(
        transient.finish(),
        Some(NpxPreparationFailureKind::Transient)
    );

    let mut socket_timeout = NpxPreparationStderrClassifier::default();
    socket_timeout.push(b"npm error code ERR_SOCKET_TIMEOUT\n");
    assert_eq!(
        socket_timeout.finish(),
        Some(NpxPreparationFailureKind::Transient)
    );

    let mut deterministic = NpxPreparationStderrClassifier::default();
    deterministic.push(b"npm ERR! code ETARGET\r\n");
    assert_eq!(
        deterministic.finish(),
        Some(NpxPreparationFailureKind::Deterministic)
    );

    let mut unknown = NpxPreparationStderrClassifier::default();
    unknown.push(b"network request failed with EAI_AGAIN\n");
    assert_eq!(unknown.finish(), None);

    let mut conflicting = NpxPreparationStderrClassifier::default();
    conflicting.push(b"npm error code EAI_AGAIN\nnpm error code ETARGET\n");
    assert_eq!(conflicting.finish(), None);

    let mut unknown_record = NpxPreparationStderrClassifier::default();
    unknown_record.push(b"npm error code EAI_AGAIN\nnpm error code EUNKNOWN\n");
    assert_eq!(unknown_record.finish(), None);

    let mut malformed_record = NpxPreparationStderrClassifier::default();
    malformed_record.push(b"npm error code EAI_AGAIN\nnpm error code not-a-code\n");
    assert_eq!(malformed_record.finish(), None);

    let mut oversized = NpxPreparationStderrClassifier::default();
    oversized.push(&[b'x'; NPM_ERROR_CODE_LINE_MAX_BYTES + 1]);
    oversized.push(b"npm error code EAI_AGAIN\n");
    assert_eq!(oversized.finish(), None);

    let mut oversized_record = NpxPreparationStderrClassifier::default();
    oversized_record.push(b"npm error code EAI_AGAIN\n");
    oversized_record.push(b"npm error code ");
    oversized_record.push(&[b'E'; NPM_ERROR_CODE_LINE_MAX_BYTES]);
    oversized_record.push(b"\n");
    assert_eq!(oversized_record.finish(), None);
}

#[test]
fn post_ready_mode_transition_requires_both_pinned_claude_snapshots() {
    let mut transition = PostReadyModeTransition::default();

    assert!(transition.observe(
        ModeObservationSource::CurrentModeUpdate,
        "default",
        "plan",
        true,
    ));
    assert!(!transition.is_stable());
    assert!(transition.observe(
        ModeObservationSource::ConfigOptionSnapshot,
        "default",
        "plan",
        true,
    ));
    assert!(!transition.is_stable());
    assert!(transition.observe(
        ModeObservationSource::CurrentModeUpdate,
        "default",
        "default",
        true,
    ));
    assert!(!transition.is_stable());
    assert!(transition.observe(
        ModeObservationSource::ConfigOptionSnapshot,
        "default",
        "default",
        true,
    ));
    assert!(transition.is_stable());
}

#[test]
fn post_ready_mode_transition_rejects_restoration_before_plan_snapshot() {
    let mut transition = PostReadyModeTransition::default();

    assert!(transition.observe(
        ModeObservationSource::CurrentModeUpdate,
        "default",
        "plan",
        true,
    ));
    assert!(!transition.observe(
        ModeObservationSource::CurrentModeUpdate,
        "default",
        "default",
        true,
    ));
    assert!(!transition.is_stable());
}

#[test]
fn post_ready_mode_transition_cannot_advance_after_the_prompt_response() {
    let mut transition = PostReadyModeTransition::default();
    assert!(transition.observe(
        ModeObservationSource::CurrentModeUpdate,
        "default",
        "plan",
        true,
    ));
    assert!(transition.observe(
        ModeObservationSource::ConfigOptionSnapshot,
        "default",
        "plan",
        true,
    ));

    assert!(!transition.observe(
        ModeObservationSource::CurrentModeUpdate,
        "default",
        "default",
        false,
    ));
    assert!(!transition.is_stable());
}

#[tokio::test]
async fn cleanup_waits_for_the_stderr_drain_task() {
    let (release, released) = oneshot::channel();
    let stderr_drain = tokio::spawn(async move {
        let _ = released.await;
        None
    });
    let cleanup = tokio::spawn(async move {
        assert_eq!(wait_for_stderr_drain(Some(stderr_drain)).await, None);
    });
    tokio::task::yield_now().await;
    assert!(!cleanup.is_finished());

    release
        .send(())
        .expect("stderr drain receiver remains live");

    cleanup.await.expect("cleanup task completes");
}

#[tokio::test]
async fn acp_frame_reader_enforces_the_exact_sixteen_mibibyte_boundary() {
    async fn read_frame(size: usize) -> (std::io::Result<Vec<u8>>, bool) {
        const PREFIX: &[u8] = br#"{"jsonrpc":"2.0","id":"size","result":""#;
        const SUFFIX: &[u8] = br#""}"#;
        assert!(size >= PREFIX.len() + SUFFIX.len());
        let mut wire = Vec::with_capacity(size + 1);
        wire.extend_from_slice(PREFIX);
        wire.resize(size - SUFFIX.len(), b'x');
        wire.extend_from_slice(SUFFIX);
        wire.push(b'\n');
        let exceeded = Arc::new(AtomicBool::new(false));
        let mut reader = AcpFrameLimitedReader::new(
            wire.as_slice(),
            Arc::clone(&exceeded),
            Arc::new(ProtocolViolationBoundary::default()),
        );
        let mut output = Vec::new();
        let result = reader.read_to_end(&mut output).await.map(|_| output);
        (result, exceeded.load(Ordering::Acquire))
    }

    for size in [ACP_FRAME_MAX_BYTES - 1, ACP_FRAME_MAX_BYTES] {
        let (result, exceeded) = read_frame(size).await;
        assert_eq!(result.unwrap().len(), size + 1);
        assert!(!exceeded);
    }
    let (result, exceeded) = read_frame(ACP_FRAME_MAX_BYTES + 1).await;
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    assert!(exceeded);

    let exceeded = Arc::new(AtomicBool::new(false));
    let mut reader = AcpFrameLimitedReader::new(
        br#"{"jsonrpc":"2.0","id":1,"result":"first"}
{"jsonrpc":"2.0","id":2,"result":"second"}
"#
        .as_slice(),
        Arc::clone(&exceeded),
        Arc::new(ProtocolViolationBoundary::default()),
    );
    let mut output = Vec::new();
    reader.read_to_end(&mut output).await.unwrap();
    assert_eq!(
        output,
        br#"{"jsonrpc":"2.0","id":1,"result":"first"}
{"jsonrpc":"2.0","id":2,"result":"second"}
"#,
    );
    assert!(!exceeded.load(Ordering::Acquire));
}

#[tokio::test]
async fn acp_frame_reader_enforces_protocol_depth_63_64_65() {
    let nested_frame = |depth: usize| {
        let mut nested = serde_json::json!(null);
        for _ in 3..depth {
            nested = serde_json::json!({"nested": nested});
        }
        let value = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "depth",
            "result": {"_meta": nested},
        });
        let mut frame = serde_json::to_vec(&value).unwrap();
        frame.push(b'\n');
        frame
    };

    for depth in [63, 64] {
        let exceeded = Arc::new(AtomicBool::new(false));
        let frame = nested_frame(depth);
        let mut reader = AcpFrameLimitedReader::new(
            frame.as_slice(),
            Arc::clone(&exceeded),
            Arc::new(ProtocolViolationBoundary::default()),
        );
        let mut output = Vec::new();
        reader.read_to_end(&mut output).await.unwrap();
        assert_eq!(output, frame);
        assert!(!exceeded.load(Ordering::Acquire));
    }

    let exceeded = Arc::new(AtomicBool::new(false));
    let frame = nested_frame(65);
    let mut reader = AcpFrameLimitedReader::new(
        frame.as_slice(),
        Arc::clone(&exceeded),
        Arc::new(ProtocolViolationBoundary::default()),
    );
    let mut output = Vec::new();
    assert_eq!(
        reader.read_to_end(&mut output).await.unwrap_err().kind(),
        std::io::ErrorKind::InvalidData,
    );
    assert!(exceeded.load(Ordering::Acquire));

    let exceeded = Arc::new(AtomicBool::new(false));
    let mut reader = AcpFrameLimitedReader::new(
        b"{not-json}\n".as_slice(),
        Arc::clone(&exceeded),
        Arc::new(ProtocolViolationBoundary::default()),
    );
    let mut output = Vec::new();
    assert_eq!(
        reader.read_to_end(&mut output).await.unwrap_err().kind(),
        std::io::ErrorKind::InvalidData,
    );
    assert!(!exceeded.load(Ordering::Acquire));
}

#[tokio::test]
async fn acp_frame_reader_rejects_malformed_json_rpc_response_envelope() {
    let exceeded = Arc::new(AtomicBool::new(false));
    let protocol_violation = Arc::new(ProtocolViolationBoundary::default());
    let frame = br#"{"jsonrpc":"2.0","id":"request","result":{},"error":{"code":-32603,"message":"ambiguous"}}
"#;
    let mut reader = AcpFrameLimitedReader::new(
        frame.as_slice(),
        Arc::clone(&exceeded),
        Arc::clone(&protocol_violation),
    );
    let mut output = Vec::new();

    assert_eq!(
        reader.read_to_end(&mut output).await.unwrap_err().kind(),
        std::io::ErrorKind::InvalidData,
    );
    assert!(!exceeded.load(Ordering::Acquire));
    assert!(protocol_violation.is_observed());
}

#[test]
fn acp_response_tracker_rejects_unknown_and_duplicate_response_ids() {
    let protocol_violation = Arc::new(ProtocolViolationBoundary::default());
    let tracker = AcpResponseTracker::new(Arc::clone(&protocol_violation));
    tracker
        .observe_outgoing(r#"{"jsonrpc":"2.0","id":"expected","method":"initialize","params":{}}"#)
        .unwrap();
    tracker
        .observe_incoming(r#"{"jsonrpc":"2.0","id":"expected","result":{}}"#)
        .unwrap();
    assert!(!protocol_violation.is_observed());

    assert_eq!(
        tracker
            .observe_incoming(r#"{"jsonrpc":"2.0","id":"expected","result":{}}"#)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidData,
    );
    assert!(protocol_violation.is_observed());
}

#[tokio::test]
async fn acp_response_tracker_signals_only_after_the_response_is_flushed() {
    let tracker = AcpResponseTracker::new(Arc::new(ProtocolViolationBoundary::default()));
    let response_id = serde_json::from_str::<RequestId>(r#""late-request""#).unwrap();
    let mut flushed = tracker.wait_for_response_flush(response_id);
    let response = r#"{"jsonrpc":"2.0","id":"late-request","error":{"code":-32601,"message":"Method not found"}}"#;

    tracker.observe_outgoing(response).unwrap();
    assert!(flushed.try_recv().is_err());
    tracker.observe_outgoing_flushed(response);
    flushed.await.unwrap();
}

#[tokio::test]
async fn response_then_request_batch_predeclares_deferred_settlement_evidence() {
    let boundary = Arc::new(ProtocolViolationBoundary::default());
    let tracker = AcpResponseTracker::new(Arc::clone(&boundary));
    tracker
        .observe_outgoing(
            r#"{"jsonrpc":"2.0","id":"prompt","method":"session/prompt","params":{}}"#,
        )
        .unwrap();
    tracker
            .observe_incoming(
                r#"[{"jsonrpc":"2.0","id":"prompt","result":{"stopReason":"end_turn"}},{"jsonrpc":"2.0","id":"late","method":"terminal/create","params":{}}]"#,
            )
            .unwrap();
    let late_id = serde_json::from_str::<RequestId>(r#""late""#).unwrap();
    assert!(boundary.is_observed());
    assert!(tracker.response_was_predeferred(&late_id));
    let waiter = tokio::spawn({
        let boundary = Arc::clone(&boundary);
        async move { boundary.wait_for_settlement_evidence().await }
    });
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());

    let response =
        r#"{"jsonrpc":"2.0","id":"late","error":{"code":-32601,"message":"Method not found"}}"#;
    tracker.observe_outgoing(response).unwrap();
    tracker.observe_outgoing_flushed(response);
    assert!(waiter.await.unwrap());
    assert!(!tracker.response_was_predeferred(&late_id));
}

#[tokio::test]
async fn deferred_protocol_violation_waits_for_every_response_flush() {
    let boundary = Arc::new(ProtocolViolationBoundary::default());

    boundary.begin_deferred_response();
    boundary.begin_deferred_response();
    assert!(boundary.is_observed());
    let waiter_boundary = Arc::clone(&boundary);
    let waiter = tokio::spawn(async move { waiter_boundary.wait_for_settlement_evidence().await });
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());
    assert!(!boundary.complete_deferred_response());
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());
    assert!(boundary.complete_deferred_response());
    assert!(waiter.await.unwrap());
}

#[tokio::test]
async fn turn_index_overflow_is_internalized_as_protocol_violation() {
    let slot = AgentSessionSlot::new();
    slot.commit_ready(
        AgentReadySnapshot {
            pid: std::process::id(),
            session_id: "overflow-test".to_owned(),
            agent_info: None,
            agent_capabilities: AgentCapabilities::default(),
            generation: 1,
            server_name: "overflow-test".to_owned(),
            endpoint: "http://127.0.0.1:1/mcp".to_owned(),
            effective_model: "test-model".to_owned(),
            effective_effort: None,
        },
        None,
        None,
    );
    slot.next_turn_index.store(u64::MAX, Ordering::Release);

    let error = match slot.claim_session_turn().await {
        Ok(_) => panic!("an exhausted internal turn index was reused"),
        Err(error) => error,
    };
    let AgentActError::SessionBroken(failure) = error else {
        panic!("turn index overflow escaped through an unplanned public branch");
    };
    assert_eq!(failure.code, "protocol_violation");
    assert!(matches!(
        &*lock(&slot.state),
        AgentSessionState::Broken(stored) if stored.code == "protocol_violation"
    ));
}

#[tokio::test]
async fn abandoned_cancelling_turn_cannot_publish_ready_without_settlement() {
    let slot = AgentSessionSlot::new();
    slot.commit_ready(
        AgentReadySnapshot {
            pid: std::process::id(),
            session_id: "abandoned-turn-test".to_owned(),
            agent_info: None,
            agent_capabilities: AgentCapabilities::default(),
            generation: 1,
            server_name: "abandoned-turn-test".to_owned(),
            endpoint: "http://127.0.0.1:1/mcp".to_owned(),
            effective_model: "test-model".to_owned(),
            effective_effort: None,
        },
        None,
        None,
    );
    let (_, session_turn, _) = slot.claim_session_turn().await.unwrap();
    session_turn.cancelling_marker().mark_cancelling();

    drop(session_turn);

    assert!(matches!(
        &*lock(&slot.state),
        AgentSessionState::Cancelling(_)
    ));
}
