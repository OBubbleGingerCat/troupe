use std::{fs, path::PathBuf};

use agent_client_protocol::schema::v1::{
    Meta, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
};
use serde_json::json;
use troupe_agent_runtime::{
    AgentDiagnosticCandidate,
    diagnostics::plan::{
        AGENT_PLAN_SNAPSHOT_CANDIDATE_KIND, AgentPlanSnapshotCandidate, AgentPlanSnapshotPayload,
        MAX_AGENT_PLAN_SNAPSHOT_BYTES,
    },
};

fn entry(
    content: impl Into<String>,
    priority: PlanEntryPriority,
    status: PlanEntryStatus,
) -> PlanEntry {
    PlanEntry::new(content, priority, status)
}

fn normalized(entries: Vec<PlanEntry>) -> AgentPlanSnapshotPayload {
    AgentPlanSnapshotPayload::from_acp(&Plan::new(entries))
}

fn encoded_entries_size(payload: &AgentPlanSnapshotPayload) -> usize {
    serde_json::to_vec(payload.entries())
        .expect("normalized plan entries serialize")
        .len()
}

#[test]
fn empty_replace_reorder_and_unicode_are_complete_deterministic_snapshots() {
    let empty = normalized(Vec::new());
    assert!(empty.entries().is_empty());
    assert!(!empty.truncated());

    let first = normalized(vec![
        entry(
            "inspect",
            PlanEntryPriority::High,
            PlanEntryStatus::Completed,
        ),
        entry(
            "实现雪线",
            PlanEntryPriority::Medium,
            PlanEntryStatus::InProgress,
        ),
    ]);
    let replacement_entries = vec![
        entry(
            "实现雪线",
            PlanEntryPriority::Low,
            PlanEntryStatus::Completed,
        ),
        entry("verify", PlanEntryPriority::High, PlanEntryStatus::Pending),
    ];
    let replacement = normalized(replacement_entries.clone());
    let repeated = normalized(replacement_entries);

    assert_eq!(first.entries()[0].content(), "inspect");
    assert_eq!(first.entries()[1].content(), "实现雪线");
    assert_eq!(replacement, repeated);
    assert_eq!(replacement.entries().len(), 2);
    assert_eq!(replacement.entries()[0].content(), "实现雪线");
    assert_eq!(replacement.entries()[1].content(), "verify");
    assert!(
        replacement
            .entries()
            .iter()
            .all(|entry| entry.content() != "inspect")
    );
}

#[test]
fn exact_byte_limit_is_accepted_and_one_byte_over_is_atomically_omitted() {
    let empty_content = normalized(vec![entry(
        "",
        PlanEntryPriority::High,
        PlanEntryStatus::Pending,
    )]);
    let structural_bytes = encoded_entries_size(&empty_content);
    let exact_content = "x".repeat(MAX_AGENT_PLAN_SNAPSHOT_BYTES - structural_bytes);

    let exact = normalized(vec![entry(
        exact_content.clone(),
        PlanEntryPriority::High,
        PlanEntryStatus::Pending,
    )]);
    assert_eq!(encoded_entries_size(&exact), MAX_AGENT_PLAN_SNAPSHOT_BYTES);
    assert_eq!(exact.entries()[0].content(), exact_content);
    assert!(!exact.truncated());

    let over = normalized(vec![entry(
        format!("{exact_content}x"),
        PlanEntryPriority::High,
        PlanEntryStatus::Pending,
    )]);
    assert!(over.entries().is_empty());
    assert!(over.truncated());
}

#[test]
fn escaped_and_multibyte_content_use_canonical_json_bytes() {
    let payload = normalized(vec![entry(
        "雪\n\"quoted\"",
        PlanEntryPriority::Low,
        PlanEntryStatus::InProgress,
    )]);

    let encoded = serde_json::to_string(payload.entries()).expect("encode normalized entries");
    assert_eq!(
        encoded,
        r#"[{"content":"雪\n\"quoted\"","priority":"low","status":"in_progress"}]"#
    );
    assert!(!payload.truncated());
}

#[test]
fn protocol_metadata_is_not_retained_in_the_normalized_payload() {
    let mut entry_meta = Meta::new();
    entry_meta.insert("credential".to_owned(), json!("entry-secret"));
    let mut plan_meta = Meta::new();
    plan_meta.insert("rawEnvelope".to_owned(), json!("plan-secret"));
    let plan = Plan::new(vec![
        entry(
            "visible plan content",
            PlanEntryPriority::Medium,
            PlanEntryStatus::Pending,
        )
        .meta(entry_meta),
    ])
    .meta(plan_meta);

    let payload = AgentPlanSnapshotPayload::from_acp(&plan);
    let encoded = serde_json::to_string(payload.entries()).expect("encode normalized entries");

    assert!(encoded.contains("visible plan content"));
    assert!(!encoded.contains("entry-secret"));
    assert!(!encoded.contains("plan-secret"));
    assert!(!encoded.contains("rawEnvelope"));
}

#[test]
fn candidate_contract_is_typed_and_has_no_sequence_or_cross_normalizer_state() {
    fn assert_candidate<T: AgentDiagnosticCandidate>() {}
    assert_candidate::<AgentPlanSnapshotCandidate>();
    assert_eq!(AGENT_PLAN_SNAPSHOT_CANDIDATE_KIND, "agent_plan_snapshot");

    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = fs::read_to_string(crate_root.join("src/diagnostics/plan.rs"))
        .expect("read plan normalizer source");
    for required in [
        "AgentPlanSnapshotMetadata",
        "Turn(AgentTurnDiagnosticMetadata)",
        "Session(AgentSessionDiagnosticMetadata)",
        "AgentDiagnosticObservation::Candidate",
        "SessionUpdate::Plan",
    ] {
        assert!(
            source.contains(required),
            "plan contract is missing {required}"
        );
    }
    for forbidden in [
        "DiagnosticEventHeader",
        "RunSequence",
        "UsageUpdate",
        "AgentThoughtChunk",
        "raw_json",
    ] {
        assert!(
            !source.contains(forbidden),
            "plan normalizer must not retain or interpret {forbidden}"
        );
    }
}
