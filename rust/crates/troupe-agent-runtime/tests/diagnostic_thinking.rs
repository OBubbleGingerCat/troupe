use std::{fs, path::PathBuf};

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, ImageContent, Plan, SessionUpdate,
};
use troupe_agent_runtime::{
    AgentDiagnosticCandidate,
    diagnostics::thinking::{
        AGENT_THINKING_ACTIVITY_OBSERVATION_KIND, AgentThinkingActivityObservation,
        AgentThinkingActivityPhase, AgentThinkingCandidate, AgentThinkingNormalizer,
        AgentThinkingSourceGapReason,
    },
};

fn thought(value: &str) -> SessionUpdate {
    SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::from(value)))
}

fn image_thought(value: &str) -> SessionUpdate {
    SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::Image(ImageContent::new(
        value,
        "image/secret",
    ))))
}

fn activity_phases(candidates: &[AgentThinkingCandidate]) -> Vec<AgentThinkingActivityPhase> {
    candidates
        .iter()
        .copied()
        .filter_map(AgentThinkingCandidate::activity)
        .map(|candidate| candidate.phase())
        .collect()
}

#[test]
fn thought_updates_emit_content_free_start_progress_and_terminal_finish() {
    let first_secret = "FIRST PRIVATE REASONING";
    let second_secret = "SECOND PRIVATE SUMMARY";
    let mut normalizer = AgentThinkingNormalizer::new();

    let start = normalizer.observe_session_update(&thought(first_secret), 10);
    let progress = normalizer.observe_session_update(&thought(second_secret), 20);
    let finish = normalizer.turn_terminal(30);

    assert_eq!(activity_phases(&start), [AgentThinkingActivityPhase::Start]);
    assert_eq!(
        activity_phases(&progress),
        [AgentThinkingActivityPhase::Progress]
    );
    assert_eq!(
        activity_phases(&finish),
        [AgentThinkingActivityPhase::Finish]
    );
    assert_eq!(start[0].activity().unwrap().elapsed_ns(), 10);
    assert_eq!(progress[0].activity().unwrap().elapsed_ns(), 20);
    assert_eq!(finish[0].activity().unwrap().elapsed_ns(), 30);
    assert_eq!(start[0].kind(), "agent_thinking_start");
    assert_eq!(progress[0].kind(), "agent_thinking_progress");
    assert_eq!(finish[0].kind(), "agent_thinking_finish");
    assert!(!normalizer.is_active());
    assert!(normalizer.is_terminal());

    let display = format!("{start:?}{progress:?}{finish:?}{normalizer:?}");
    assert!(!display.contains(first_secret));
    assert!(!display.contains(second_secret));
}

#[test]
fn non_text_blocks_are_activity_only_and_do_not_retain_raw_data() {
    let raw_secret = "BASE64 PRIVATE BLOCK";
    let mut normalizer = AgentThinkingNormalizer::new();

    let start = normalizer.observe_session_update(&image_thought(raw_secret), 1);
    assert_eq!(activity_phases(&start), [AgentThinkingActivityPhase::Start]);
    assert!(!format!("{start:?}{normalizer:?}").contains(raw_secret));
}

#[test]
fn unrelated_interleave_does_not_split_or_close_thinking_activity() {
    let mut normalizer = AgentThinkingNormalizer::new();
    let start = normalizer.observe_session_update(&thought("private-a"), 1);
    let message =
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from("visible response")));
    let plan = SessionUpdate::Plan(Plan::new(Vec::new()));

    assert!(normalizer.observe_session_update(&message, 2).is_empty());
    assert!(normalizer.observe_session_update(&plan, 3).is_empty());
    assert!(normalizer.is_active());

    let progress = normalizer.observe_session_update(&thought("private-b"), 4);
    let finish = normalizer.turn_terminal(5);
    assert_eq!(activity_phases(&start), [AgentThinkingActivityPhase::Start]);
    assert_eq!(
        activity_phases(&progress),
        [AgentThinkingActivityPhase::Progress]
    );
    assert_eq!(
        activity_phases(&finish),
        [AgentThinkingActivityPhase::Finish]
    );
}

#[test]
fn malformed_post_terminal_transitions_produce_stable_source_gaps() {
    let mut normalizer = AgentThinkingNormalizer::new();
    normalizer.observe_session_update(&thought("private"), 1);
    normalizer.turn_terminal(2);

    let repeated_terminal = normalizer.turn_terminal(3);
    let repeated_gap = repeated_terminal[0].source_gap().unwrap();
    assert_eq!(
        repeated_gap.reason(),
        AgentThinkingSourceGapReason::RepeatedTurnTerminal
    );
    assert_eq!(
        repeated_gap.reason().as_str(),
        "thinking_turn_terminal_repeated"
    );
    assert_eq!(repeated_gap.elapsed_ns(), 3);
    assert_eq!(repeated_terminal[0].kind(), "observation_gap");

    let late_activity = normalizer.observe_session_update(&thought("must disappear"), 4);
    let late_gap = late_activity[0].source_gap().unwrap();
    assert_eq!(
        late_gap.reason(),
        AgentThinkingSourceGapReason::ActivityAfterTurnTerminal
    );
    assert_eq!(
        late_gap.reason().as_str(),
        "thinking_activity_after_turn_terminal"
    );
    assert_eq!(late_gap.elapsed_ns(), 4);
    assert!(late_activity[0].activity().is_none());
    assert!(!format!("{late_activity:?}").contains("must disappear"));
}

#[test]
fn a_turn_without_thinking_closes_without_synthesizing_activity() {
    let mut normalizer = AgentThinkingNormalizer::new();
    assert!(normalizer.turn_terminal(10).is_empty());
    assert!(normalizer.is_terminal());

    let late = normalizer.observe_session_update(&thought("late private text"), 11);
    assert_eq!(late.len(), 1);
    assert!(late[0].activity().is_none());
    assert_eq!(
        late[0].source_gap().unwrap().reason(),
        AgentThinkingSourceGapReason::ActivityAfterTurnTerminal
    );
}

#[test]
fn public_contract_has_no_content_or_cross_normalizer_payload_seam() {
    fn assert_candidate<T: AgentDiagnosticCandidate>() {}
    assert_candidate::<AgentThinkingActivityObservation>();
    assert_candidate::<AgentThinkingCandidate>();
    assert_eq!(
        AGENT_THINKING_ACTIVITY_OBSERVATION_KIND,
        "agent_thinking_activity_observed"
    );
    assert_eq!(AgentThinkingActivityPhase::Start.as_str(), "start");
    assert_eq!(AgentThinkingActivityPhase::Progress.as_str(), "progress");
    assert_eq!(AgentThinkingActivityPhase::Finish.as_str(), "finish");

    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = fs::read_to_string(crate_root.join("src/diagnostics/thinking.rs"))
        .expect("read thinking normalizer source");
    for required in [
        "SessionUpdate::AgentThoughtChunk(_)",
        "AgentThinkingActivityPhase::Start",
        "AgentThinkingActivityPhase::Progress",
        "AgentThinkingActivityPhase::Finish",
        "AgentThinkingSourceGapReason",
    ] {
        assert!(
            source.contains(required),
            "thinking contract is missing {required}"
        );
    }
    for forbidden in [
        ".content",
        ".text",
        "ContentBlock",
        "serde_json",
        "AgentMessageDelta",
        "UsageUpdate",
        "AgentPlanSnapshot",
        "RunSequence",
    ] {
        assert!(
            !source.contains(forbidden),
            "thinking normalizer must not retain or interpret {forbidden}"
        );
    }
}
