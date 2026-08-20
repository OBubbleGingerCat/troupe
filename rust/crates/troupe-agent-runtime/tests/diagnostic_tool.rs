use std::{fs, path::PathBuf};

use agent_client_protocol::schema::v1::{
    ContentBlock, Meta, SessionUpdate, ToolCall, ToolCallLocation,
    ToolCallStatus as AcpToolCallStatus, ToolKind as AcpToolKind,
};
use serde_json::json;
use troupe_agent_runtime::{
    AgentDiagnosticCandidate,
    diagnostics::tool::{
        AGENT_TOOL_OBSERVATION_KIND, AgentToolCandidate, AgentToolErrorCode, AgentToolNormalizer,
        AgentToolObservation, AgentToolSourceGapReason, AgentToolTerminalOutcome,
    },
};
use troupe_diagnostics_core::kinds::{ToolCallStatus, ToolKind};

fn start(
    normalizer: &mut AgentToolNormalizer,
    id: &str,
    title: &str,
    kind: ToolKind,
    status: ToolCallStatus,
    elapsed_ns: u64,
) -> Vec<AgentToolCandidate> {
    normalizer.observe_start(id, title, kind, status, elapsed_ns)
}

fn update(
    normalizer: &mut AgentToolNormalizer,
    id: &str,
    title: Option<&str>,
    kind: Option<ToolKind>,
    status: Option<ToolCallStatus>,
    elapsed_ns: u64,
) -> Vec<AgentToolCandidate> {
    normalizer.observe_update_fields(id, title, kind, status, elapsed_ns)
}

fn gap(candidate: &AgentToolCandidate) -> AgentToolSourceGapReason {
    candidate.source_gap().expect("source gap").reason()
}

#[test]
fn start_progress_and_success_finish_are_ordered_and_typed() {
    let mut normalizer = AgentToolNormalizer::new();

    let started = start(
        &mut normalizer,
        "tool-1",
        "Read source",
        ToolKind::Read,
        ToolCallStatus::Pending,
        10,
    );
    assert_eq!(started.len(), 1);
    let started = started[0].started().unwrap();
    assert_eq!(started.elapsed_ns(), 10);
    assert_eq!(started.metadata().source_tool_call_id(), "tool-1");
    assert_eq!(started.metadata().title(), "Read source");
    assert_eq!(started.metadata().tool_kind(), ToolKind::Read);
    assert_eq!(started.metadata().status(), ToolCallStatus::Pending);

    let progress = update(
        &mut normalizer,
        "tool-1",
        Some("Read diagnostics source"),
        None,
        Some(ToolCallStatus::InProgress),
        20,
    );
    assert_eq!(progress.len(), 1);
    let progress = progress[0].updated().unwrap();
    assert_eq!(progress.elapsed_ns(), 20);
    assert_eq!(progress.metadata().title(), "Read diagnostics source");
    assert_eq!(progress.metadata().tool_kind(), ToolKind::Read);
    assert_eq!(progress.metadata().status(), ToolCallStatus::InProgress);

    let terminal = update(
        &mut normalizer,
        "tool-1",
        None,
        None,
        Some(ToolCallStatus::Completed),
        30,
    );
    assert_eq!(terminal.len(), 2);
    assert_eq!(
        terminal[0].updated().unwrap().metadata().status(),
        ToolCallStatus::Completed
    );
    let finished = terminal[1].finished().unwrap();
    assert_eq!(finished.elapsed_ns(), 30);
    assert_eq!(finished.source_tool_call_id(), "tool-1");
    assert_eq!(finished.outcome(), AgentToolTerminalOutcome::Completed);
    assert_eq!(finished.error_code(), None);
    assert_eq!(normalizer.active_tool_count(), 0);
}

#[test]
fn failure_has_only_a_stable_error_code() {
    let mut normalizer = AgentToolNormalizer::new();
    start(
        &mut normalizer,
        "tool-fail",
        "Run command",
        ToolKind::Execute,
        ToolCallStatus::InProgress,
        1,
    );

    let terminal = update(
        &mut normalizer,
        "tool-fail",
        None,
        None,
        Some(ToolCallStatus::Failed),
        2,
    );
    let finished = terminal[1].finished().unwrap();
    assert_eq!(finished.outcome(), AgentToolTerminalOutcome::Failed);
    assert_eq!(finished.error_code(), Some(AgentToolErrorCode::ToolFailed));
    assert_eq!(AgentToolErrorCode::ToolFailed.as_str(), "tool_failed");
}

#[test]
fn interleaved_tools_remain_independent_and_terminal_cancels_in_start_order() {
    let mut normalizer = AgentToolNormalizer::new();
    start(
        &mut normalizer,
        "a",
        "Search",
        ToolKind::Search,
        ToolCallStatus::Pending,
        1,
    );
    start(
        &mut normalizer,
        "b",
        "Edit",
        ToolKind::Edit,
        ToolCallStatus::InProgress,
        2,
    );
    update(&mut normalizer, "b", Some("Edit file"), None, None, 3);
    assert_eq!(normalizer.active_tool_count(), 2);

    let cancelled = normalizer.turn_terminal(4);
    assert_eq!(cancelled.len(), 2);
    assert_eq!(cancelled[0].finished().unwrap().source_tool_call_id(), "a");
    assert_eq!(cancelled[1].finished().unwrap().source_tool_call_id(), "b");
    for candidate in &cancelled {
        assert_eq!(
            candidate.finished().unwrap().outcome(),
            AgentToolTerminalOutcome::Cancelled
        );
        assert_eq!(candidate.finished().unwrap().error_code(), None);
    }
    assert_eq!(normalizer.active_tool_count(), 0);
    assert!(normalizer.is_terminal());
}

#[test]
fn unknown_duplicate_reused_and_regressing_transitions_are_stable_gaps() {
    let mut normalizer = AgentToolNormalizer::new();
    assert_eq!(
        gap(&update(&mut normalizer, "missing", None, None, None, 1,)[0]),
        AgentToolSourceGapReason::UpdateBeforeStart
    );
    assert_eq!(
        gap(&start(
            &mut normalizer,
            "",
            "empty",
            ToolKind::Other,
            ToolCallStatus::Pending,
            2,
        )[0]),
        AgentToolSourceGapReason::EmptyToolCallId
    );

    start(
        &mut normalizer,
        "same",
        "Execute",
        ToolKind::Execute,
        ToolCallStatus::InProgress,
        3,
    );
    assert_eq!(
        gap(&start(
            &mut normalizer,
            "same",
            "Duplicate",
            ToolKind::Other,
            ToolCallStatus::Pending,
            4,
        )[0]),
        AgentToolSourceGapReason::DuplicateActiveToolCall
    );
    assert_eq!(
        gap(&update(
            &mut normalizer,
            "same",
            None,
            None,
            Some(ToolCallStatus::Pending),
            5,
        )[0]),
        AgentToolSourceGapReason::InvalidStatusTransition
    );
    update(
        &mut normalizer,
        "same",
        None,
        None,
        Some(ToolCallStatus::Completed),
        6,
    );
    assert_eq!(
        gap(&update(&mut normalizer, "same", None, None, None, 7,)[0]),
        AgentToolSourceGapReason::UpdateAfterFinish
    );
    assert_eq!(
        gap(&start(
            &mut normalizer,
            "same",
            "Reused",
            ToolKind::Other,
            ToolCallStatus::Pending,
            8,
        )[0]),
        AgentToolSourceGapReason::FinishedToolCallIdReused
    );
}

#[test]
fn initially_terminal_calls_close_immediately_and_late_activity_is_a_gap() {
    let mut normalizer = AgentToolNormalizer::new();
    let immediate = start(
        &mut normalizer,
        "already-done",
        "Fetched",
        ToolKind::Fetch,
        ToolCallStatus::Completed,
        1,
    );
    assert_eq!(immediate.len(), 2);
    assert!(immediate[0].started().is_some());
    assert!(immediate[1].finished().is_some());

    assert!(normalizer.turn_terminal(2).is_empty());
    assert_eq!(
        gap(&update(&mut normalizer, "already-done", None, None, None, 3,)[0]),
        AgentToolSourceGapReason::ActivityAfterTurnTerminal
    );
    assert_eq!(
        gap(&normalizer.turn_terminal(4)[0]),
        AgentToolSourceGapReason::RepeatedTurnTerminal
    );
}

#[test]
fn payload_and_protocol_envelopes_have_no_candidate_or_debug_seam() {
    fn assert_candidate<T: AgentDiagnosticCandidate>() {}
    assert_candidate::<AgentToolObservation>();
    assert_candidate::<AgentToolCandidate>();
    assert_eq!(AGENT_TOOL_OBSERVATION_KIND, "agent_tool_observed");

    let secrets = [
        "SECRET RAW INPUT",
        "SECRET RAW OUTPUT",
        "SECRET CONTENT",
        "SECRET LOCATION",
        "SECRET META",
    ];
    let mut meta = Meta::new();
    meta.insert("secret".to_owned(), json!(secrets[4]));
    let call = ToolCall::new("safe-id", "Visible title")
        .kind(AcpToolKind::Read)
        .status(AcpToolCallStatus::Pending)
        .content(vec![ContentBlock::from(secrets[2]).into()])
        .locations(vec![ToolCallLocation::new(secrets[3])])
        .raw_input(json!({"secret": secrets[0]}))
        .raw_output(json!({"secret": secrets[1]}))
        .meta(meta);

    let mut normalizer = AgentToolNormalizer::new();
    let visible = normalizer.observe_session_update(&SessionUpdate::ToolCall(call), 1);
    assert_eq!(
        visible[0].started().unwrap().metadata().title(),
        "Visible title"
    );
    let debug = format!("{visible:?}{normalizer:?}");
    for secret in secrets {
        assert!(!debug.contains(secret));
    }

    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = fs::read_to_string(crate_root.join("src/diagnostics/tool.rs")).unwrap();
    for required in [
        "SessionUpdate::ToolCall(call)",
        "SessionUpdate::ToolCallUpdate(update)",
        "AgentDiagnosticObservation::Candidate",
        "AgentToolSourceGapReason",
    ] {
        assert!(source.contains(required), "missing tool seam: {required}");
    }
    for forbidden in [
        ".content",
        ".raw_input",
        ".raw_output",
        ".locations",
        "call.meta",
        "update.meta",
        "\"_meta\"",
        "ToolCallContent",
        "serde_json",
        "RunSequence",
    ] {
        assert!(
            !source.contains(forbidden),
            "tool normalizer must not retain or interpret {forbidden}"
        );
    }
}

#[test]
fn candidate_kinds_and_reason_codes_are_stable() {
    let mut normalizer = AgentToolNormalizer::new();
    let start = start(
        &mut normalizer,
        "id",
        "Title",
        ToolKind::Other,
        ToolCallStatus::Pending,
        1,
    );
    let update = update(&mut normalizer, "id", None, None, None, 2);
    let finish = normalizer.turn_terminal(3);
    let gap = normalizer.turn_terminal(4);

    assert_eq!(start[0].kind(), "agent_tool_started");
    assert_eq!(update[0].kind(), "agent_tool_updated");
    assert_eq!(finish[0].kind(), "agent_tool_finished");
    assert_eq!(gap[0].kind(), "observation_gap");
    assert_eq!(
        AgentToolSourceGapReason::UpdateBeforeStart.as_str(),
        "tool_update_before_start"
    );
    assert_eq!(AgentToolTerminalOutcome::Cancelled.as_str(), "cancelled");
}
