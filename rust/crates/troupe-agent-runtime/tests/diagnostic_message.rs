use agent_client_protocol::schema::v1::SessionUpdate;
use troupe_agent_runtime::{
    AgentDiagnosticCandidate,
    diagnostics::message::{
        ACT_AGENT_MESSAGES_MAX_BYTES, AGENT_MESSAGE_DELTA_FLUSH_BYTES,
        AGENT_MESSAGE_DELTA_FLUSH_NS, AGENT_MESSAGE_MAX_BYTES, AgentMessageCandidate,
        AgentMessageNormalizationError, AgentMessageNormalizer, AgentMessageSourceGapReason,
    },
};

const MESSAGE_DELTA_FIXTURE: &str =
    include_str!("../../../../tests/fixtures/acp/message-delta.json");
const MESSAGE_COMPLETED_FIXTURE: &str =
    include_str!("../../../../tests/fixtures/acp/message-completed.json");
const MESSAGE_MALFORMED_FIXTURE: &str =
    include_str!("../../../../tests/fixtures/acp/message-malformed.json");

fn fixture(source: &str) -> Vec<SessionUpdate> {
    serde_json::from_str(source).expect("valid ACP SessionUpdate fixture")
}

fn append(
    target: &mut Vec<AgentMessageCandidate>,
    source: Result<Vec<AgentMessageCandidate>, AgentMessageNormalizationError>,
) {
    target.extend(source.expect("message normalization succeeds"));
}

fn deltas(candidates: &[AgentMessageCandidate]) -> Vec<&str> {
    candidates
        .iter()
        .filter_map(AgentMessageCandidate::delta)
        .map(|candidate| candidate.text_delta())
        .collect()
}

#[derive(Default)]
struct PausedClock {
    elapsed_ns: u64,
}

impl PausedClock {
    fn now(&self) -> u64 {
        self.elapsed_ns
    }

    fn advance(&mut self, elapsed_ns: u64) {
        self.elapsed_ns += elapsed_ns;
    }
}

#[test]
fn fixture_keeps_only_user_visible_agent_text_and_never_retains_thoughts() {
    let mut normalizer = AgentMessageNormalizer::new();
    let mut candidates = Vec::new();
    for (index, update) in fixture(MESSAGE_DELTA_FIXTURE).iter().enumerate() {
        append(
            &mut candidates,
            normalizer.observe_session_update(update, index as u64),
        );
    }
    append(&mut candidates, normalizer.turn_terminal(10, false));

    assert_eq!(deltas(&candidates), vec!["hello ", "world"]);
    let captured = deltas(&candidates).concat();
    assert_eq!(captured, "hello world");
    assert!(!captured.contains("USER PROMPT"));
    assert!(!captured.contains("THOUGHT"));

    let delta_ids = candidates
        .iter()
        .filter_map(AgentMessageCandidate::delta)
        .map(|candidate| candidate.message_id())
        .collect::<Vec<_>>();
    assert_eq!(delta_ids.len(), 2);
    assert_eq!(delta_ids[0], delta_ids[1]);
    assert!(
        delta_ids
            .iter()
            .all(|_| !format!("{normalizer:?}").contains("THOUGHT TEXT"))
    );

    let completed = candidates
        .iter()
        .find_map(AgentMessageCandidate::completed)
        .unwrap();
    assert_eq!(completed.utf8_bytes(), 11);
    assert_eq!(completed.unicode_scalar_count(), 11);
    assert!(!completed.truncated());
}

#[test]
fn explicit_id_change_flushes_and_completes_before_the_new_message() {
    let updates = fixture(MESSAGE_COMPLETED_FIXTURE);
    let mut normalizer = AgentMessageNormalizer::new();
    let mut candidates = Vec::new();
    append(
        &mut candidates,
        normalizer.observe_session_update(&updates[0], 10),
    );
    append(
        &mut candidates,
        normalizer.observe_session_update(&updates[1], 20),
    );
    append(&mut candidates, normalizer.turn_terminal(30, false));

    assert_eq!(
        candidates
            .iter()
            .map(AgentDiagnosticCandidate::kind)
            .collect::<Vec<_>>(),
        vec![
            "agent_message_delta",
            "agent_message_completed",
            "agent_message_delta",
            "agent_message_completed",
        ]
    );
    let deltas = candidates
        .iter()
        .filter_map(AgentMessageCandidate::delta)
        .collect::<Vec<_>>();
    assert_eq!(deltas[0].source_message_id(), Some("provider-message-a"));
    assert_eq!(deltas[1].source_message_id(), Some("provider-message-b"));
    assert_ne!(deltas[0].message_id(), deltas[1].message_id());
    assert_eq!(deltas[0].elapsed_ns(), 10);
    assert_eq!(deltas[1].elapsed_ns(), 20);
}

#[test]
fn completed_provider_id_reuse_gets_a_new_local_id_and_source_gap() {
    let updates = fixture(MESSAGE_MALFORMED_FIXTURE);
    let mut normalizer = AgentMessageNormalizer::new();
    let mut candidates = Vec::new();
    for (index, update) in updates.iter().enumerate() {
        append(
            &mut candidates,
            normalizer.observe_session_update(update, (index as u64 + 1) * 10),
        );
    }
    append(&mut candidates, normalizer.turn_terminal(40, false));

    let generations = candidates
        .iter()
        .filter_map(AgentMessageCandidate::delta)
        .filter(|candidate| candidate.source_message_id() == Some("provider-message-reused"))
        .collect::<Vec<_>>();
    assert_eq!(generations.len(), 2);
    assert_ne!(generations[0].message_id(), generations[1].message_id());

    let gaps = candidates
        .iter()
        .filter_map(AgentMessageCandidate::source_gap)
        .collect::<Vec<_>>();
    assert_eq!(gaps.len(), 1);
    assert_eq!(
        gaps[0].reason(),
        AgentMessageSourceGapReason::CompletedSourceMessageIdReused
    );
    assert_eq!(
        gaps[0].reason().as_str(),
        "completed_source_message_id_reused"
    );
    assert_eq!(gaps[0].source_message_id(), "provider-message-reused");
    assert_eq!(gaps[0].previous_message_id(), generations[0].message_id());
    assert_eq!(
        gaps[0].replacement_message_id(),
        generations[1].message_id()
    );
    assert_eq!(gaps[0].elapsed_ns(), 30);
}

#[test]
fn anonymous_and_explicit_messages_coexist_across_interleaved_candidates() {
    let mut normalizer = AgentMessageNormalizer::new();
    let mut candidates = Vec::new();
    append(
        &mut candidates,
        normalizer.observe_text(None, "anonymous-a", 1),
    );
    append(&mut candidates, normalizer.observe_other_candidate(2));
    append(
        &mut candidates,
        normalizer.observe_text(Some("explicit-a"), "explicit", 3),
    );
    append(&mut candidates, normalizer.observe_other_candidate(4));
    append(
        &mut candidates,
        normalizer.observe_text(None, "anonymous-b", 5),
    );
    append(&mut candidates, normalizer.turn_terminal(6, false));

    let deltas = candidates
        .iter()
        .filter_map(AgentMessageCandidate::delta)
        .collect::<Vec<_>>();
    assert_eq!(deltas.len(), 3);
    assert_eq!(deltas[0].source_message_id(), None);
    assert_eq!(deltas[1].source_message_id(), Some("explicit-a"));
    assert_eq!(deltas[2].source_message_id(), None);
    assert_eq!(deltas[0].message_id(), deltas[2].message_id());
    assert_ne!(deltas[0].message_id(), deltas[1].message_id());

    let completed = candidates
        .iter()
        .filter_map(AgentMessageCandidate::completed)
        .collect::<Vec<_>>();
    assert_eq!(completed.len(), 2);
    assert_eq!(completed[0].message_id(), deltas[0].message_id());
    assert_eq!(completed[1].message_id(), deltas[1].message_id());
}

#[test]
fn size_flush_is_exact_and_keeps_first_chunk_elapsed_time() {
    let exact = "a".repeat(AGENT_MESSAGE_DELTA_FLUSH_BYTES);
    let mut normalizer = AgentMessageNormalizer::new();
    let candidates = normalizer.observe_text(Some("exact"), &exact, 10).unwrap();
    assert_eq!(deltas(&candidates), vec![exact.as_str()]);
    assert_eq!(candidates[0].delta().unwrap().elapsed_ns(), 10);
    assert_eq!(normalizer.next_flush_deadline_ns(), None);

    let one_over = "b".repeat(AGENT_MESSAGE_DELTA_FLUSH_BYTES + 1);
    let mut normalizer = AgentMessageNormalizer::new();
    let candidates = normalizer
        .observe_text(Some("one-over"), &one_over, 20)
        .unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates[0].delta().unwrap().text_delta().len(),
        AGENT_MESSAGE_DELTA_FLUSH_BYTES
    );
    assert_eq!(
        normalizer.next_flush_deadline_ns(),
        Some(20 + AGENT_MESSAGE_DELTA_FLUSH_NS)
    );
    let tail = normalizer.observe_other_candidate(21).unwrap();
    assert_eq!(deltas(&tail), vec!["b"]);

    let unicode_boundary = format!("{}é", "c".repeat(AGENT_MESSAGE_DELTA_FLUSH_BYTES - 1));
    let mut normalizer = AgentMessageNormalizer::new();
    let candidates = normalizer
        .observe_text(Some("unicode"), &unicode_boundary, 30)
        .unwrap();
    assert_eq!(candidates[0].delta().unwrap().text_delta().len(), 16_383);
    let tail = normalizer.observe_other_candidate(31).unwrap();
    assert_eq!(deltas(&tail), vec!["é"]);
}

#[test]
fn paused_clock_flushes_at_exactly_twenty_milliseconds() {
    let mut clock = PausedClock::default();
    let mut normalizer = AgentMessageNormalizer::new();
    assert!(
        normalizer
            .observe_text(Some("timed"), "first", clock.now())
            .unwrap()
            .is_empty()
    );
    clock.advance(AGENT_MESSAGE_DELTA_FLUSH_NS - 1);
    assert!(normalizer.flush_elapsed(clock.now()).unwrap().is_empty());
    clock.advance(1);
    let candidates = normalizer.flush_elapsed(clock.now()).unwrap();
    assert_eq!(deltas(&candidates), vec!["first"]);
    assert_eq!(candidates[0].delta().unwrap().elapsed_ns(), 0);

    assert!(
        normalizer
            .observe_text(Some("timed"), "second", clock.now())
            .unwrap()
            .is_empty()
    );
    let candidates = normalizer.observe_other_candidate(clock.now()).unwrap();
    assert_eq!(deltas(&candidates), vec!["second"]);
}

#[test]
fn empty_delta_never_opens_a_message_or_emits_a_candidate() {
    let mut normalizer = AgentMessageNormalizer::new();
    assert!(
        normalizer
            .observe_text(Some("empty"), "", 1)
            .unwrap()
            .is_empty()
    );
    assert!(normalizer.turn_terminal(2, false).unwrap().is_empty());
    assert_eq!(
        normalizer.observe_text(Some("late"), "x", 3),
        Err(AgentMessageNormalizationError::TurnAlreadyTerminal)
    );
    assert_eq!(
        AgentMessageNormalizationError::TurnAlreadyTerminal.code(),
        "turn_already_terminal"
    );
}

#[test]
fn per_message_limit_accepts_equal_and_truncates_one_over() {
    let exact = "x".repeat(AGENT_MESSAGE_MAX_BYTES);
    let mut normalizer = AgentMessageNormalizer::new();
    let deltas = normalizer
        .observe_text(Some("exact-limit"), &exact, 1)
        .unwrap();
    assert_eq!(deltas.len(), AGENT_MESSAGE_MAX_BYTES / 16_384);
    let completed = normalizer.turn_terminal(2, false).unwrap();
    let completed = completed[0].completed().unwrap();
    assert_eq!(completed.utf8_bytes(), AGENT_MESSAGE_MAX_BYTES as u64);
    assert_eq!(
        completed.unicode_scalar_count(),
        AGENT_MESSAGE_MAX_BYTES as u64
    );
    assert!(!completed.truncated());

    let one_over = "y".repeat(AGENT_MESSAGE_MAX_BYTES + 1);
    let mut normalizer = AgentMessageNormalizer::new();
    let deltas = normalizer
        .observe_text(Some("over-limit"), &one_over, 1)
        .unwrap();
    assert_eq!(deltas.len(), AGENT_MESSAGE_MAX_BYTES / 16_384);
    let completed = normalizer.turn_terminal(2, false).unwrap();
    let completed = completed[0].completed().unwrap();
    assert_eq!(completed.utf8_bytes(), AGENT_MESSAGE_MAX_BYTES as u64);
    assert!(completed.truncated());
}

#[test]
fn per_act_limit_stops_later_messages_and_marks_them_truncated() {
    let exact_message = "z".repeat(AGENT_MESSAGE_MAX_BYTES);
    let mut normalizer = AgentMessageNormalizer::new();
    for index in 0..4 {
        let candidates = normalizer
            .observe_text(Some(&format!("message-{index}")), &exact_message, index)
            .unwrap();
        assert_eq!(
            candidates
                .iter()
                .filter_map(AgentMessageCandidate::delta)
                .count(),
            AGENT_MESSAGE_MAX_BYTES / 16_384
        );
        assert!(
            candidates
                .iter()
                .filter_map(AgentMessageCandidate::completed)
                .all(|candidate| !candidate.truncated())
        );
    }
    assert_eq!(
        normalizer.captured_act_utf8_bytes(),
        ACT_AGENT_MESSAGES_MAX_BYTES
    );

    let candidates = normalizer
        .observe_text(Some("message-over"), "q", 5)
        .unwrap();
    assert!(
        candidates
            .iter()
            .filter_map(AgentMessageCandidate::delta)
            .next()
            .is_none()
    );
    let terminal = normalizer.turn_terminal(6, false).unwrap();
    let completed = terminal[0].completed().unwrap();
    assert_eq!(completed.utf8_bytes(), 0);
    assert_eq!(completed.unicode_scalar_count(), 0);
    assert!(completed.truncated());
}

#[test]
fn terminal_flushes_pending_text_and_propagates_source_termination() {
    let mut normalizer = AgentMessageNormalizer::new();
    normalizer
        .observe_text(Some("source-ended"), "aé", 7)
        .unwrap();
    let candidates = normalizer.turn_terminal(9, true).unwrap();
    assert_eq!(deltas(&candidates), vec!["aé"]);
    assert_eq!(candidates[0].delta().unwrap().elapsed_ns(), 7);
    let completed = candidates[1].completed().unwrap();
    assert_eq!(completed.elapsed_ns(), 9);
    assert_eq!(completed.utf8_bytes(), 3);
    assert_eq!(completed.unicode_scalar_count(), 2);
    assert!(completed.truncated());
    assert!(normalizer.is_terminal());
}
