use troupe_diagnostics_core::{
    event::{
        AgentMessageCompleted, AgentMessageDelta, CausalLink, CounterSampled, DiagnosticEvent,
        DiagnosticEventHeader, DiagnosticScope,
    },
    id::{CanonicalUuid, RunLocalId},
    kinds::{CausalRelation, CounterKind},
    scalar::SchemaU64,
    time::ElapsedNs,
};
use troupe_diagnostics_runtime::store::projector::messages::{
    MessageIdentityField, MessageProjector, MessageReadModel, project_messages,
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
const OTHER_RUN_ID: &str = "87654321-4321-4321-8321-cba987654321";
const DELTA_FIXTURE: &[u8] =
    include_bytes!("../../../../tests/fixtures/diagnostics/events/agent-message-delta.json");
const COMPLETED_FIXTURE: &[u8] =
    include_bytes!("../../../../tests/fixtures/diagnostics/events/agent-message-completed.json");

fn run_id() -> CanonicalUuid {
    CanonicalUuid::parse(RUN_ID).expect("canonical Run UUID")
}

fn other_run_id() -> CanonicalUuid {
    CanonicalUuid::parse(OTHER_RUN_ID).expect("canonical alternate Run UUID")
}

fn local_id(value: &str) -> RunLocalId {
    RunLocalId::parse(value).expect("valid Run-local ID")
}

fn scope(actor: &str, cue: &str, act: &str) -> DiagnosticScope {
    DiagnosticScope::new(
        Some(local_id("scene-1")),
        Some(local_id(actor)),
        Some(local_id(cue)),
        None,
        Some(local_id(act)),
        None,
        Some(SchemaU64::new(1)),
    )
}

fn header_for(
    run_id: CanonicalUuid,
    sequence: u64,
    elapsed_ns: u64,
    scope: DiagnosticScope,
    caused_by: Vec<CausalLink>,
) -> DiagnosticEventHeader {
    DiagnosticEventHeader::new(
        run_id,
        SchemaU64::new(sequence),
        ElapsedNs::new(elapsed_ns),
        scope,
        caused_by,
    )
    .expect("valid event header")
}

fn delta(
    sequence: u64,
    elapsed_ns: u64,
    scope: DiagnosticScope,
    message_id: &str,
    source_message_id: Option<&str>,
    text: &str,
) -> DiagnosticEvent {
    DiagnosticEvent::AgentMessageDelta(AgentMessageDelta::new(
        header_for(run_id(), sequence, elapsed_ns, scope, Vec::new()),
        local_id(message_id),
        source_message_id.map(str::to_owned),
        text.to_owned(),
    ))
}

fn completion(
    sequence: u64,
    elapsed_ns: u64,
    scope: DiagnosticScope,
    message_id: &str,
    utf8_bytes: u64,
    unicode_scalar_count: u64,
    truncated: bool,
) -> DiagnosticEvent {
    DiagnosticEvent::AgentMessageCompleted(AgentMessageCompleted::new(
        header_for(run_id(), sequence, elapsed_ns, scope, Vec::new()),
        local_id(message_id),
        SchemaU64::new(utf8_bytes),
        SchemaU64::new(unicode_scalar_count),
        truncated,
    ))
}

fn counter(sequence: u64, elapsed_ns: u64, scope: DiagnosticScope) -> DiagnosticEvent {
    DiagnosticEvent::CounterSampled(CounterSampled::new(
        header_for(run_id(), sequence, elapsed_ns, scope, Vec::new()),
        CounterKind::AgentTurnActive,
        SchemaU64::new(1),
    ))
}

fn fixture_events(bytes: &[u8]) -> Vec<DiagnosticEvent> {
    serde_json::from_slice(bytes).expect("parse event fixture")
}

fn interleaved_events() -> Vec<DiagnosticEvent> {
    let first = scope("actor-1", "cue-1", "act-1");
    let second = scope("actor-1", "cue-2", "act-2");
    vec![
        delta(1, 10, first.clone(), "message-1", None, "hello "),
        delta(
            2,
            20,
            second.clone(),
            "message-2",
            Some("provider-2"),
            "other",
        ),
        counter(3, 15, first.clone()),
        delta(
            4,
            40,
            first.clone(),
            "message-1",
            Some("provider-1"),
            "世界",
        ),
        completion(5, 50, second, "message-2", 5, 5, false),
        completion(6, 60, first, "message-1", 12, 8, true),
    ]
}

#[test]
fn assembles_utf8_deltas_and_promotes_the_first_known_source_identity() {
    let events = fixture_events(DELTA_FIXTURE);
    let model = project_messages(run_id(), &events).expect("project frozen delta fixture");

    assert_eq!(model.model_schema_version(), 1);
    assert_eq!(model.run_id(), run_id());
    assert_eq!(model.through_sequence().get(), 2);
    assert_eq!(model.through_elapsed_ns().get(), 20);
    assert_eq!(model.messages().len(), 1);
    let message = model.message(&local_id("message-1")).expect("message");
    assert_eq!(message.first_sequence().get(), 1);
    assert_eq!(message.latest_sequence().get(), 2);
    assert_eq!(message.first_elapsed_ns().get(), 10);
    assert_eq!(message.latest_elapsed_ns().get(), 20);
    assert_eq!(message.source_message_id(), Some("provider-message-9"));
    assert_eq!(message.text(), "你好，Troupe 👋\nsecond line");
    assert!(message.is_open());
}

#[test]
fn keeps_actor_cue_and_act_messages_separate_and_preserves_terminal_counts() {
    let model = project_messages(run_id(), &interleaved_events()).expect("project interleaving");

    assert_eq!(model.messages().len(), 2);
    assert_eq!(model.open_messages().count(), 0);
    assert_eq!(model.completed_messages().count(), 2);
    assert_eq!(model.through_sequence().get(), 6);
    assert_eq!(model.through_elapsed_ns().get(), 60);

    let first = model
        .message(&local_id("message-1"))
        .expect("first message");
    assert_eq!(first.text(), "hello 世界");
    assert_eq!(first.source_message_id(), Some("provider-1"));
    assert!(first.is_truncated());
    let completion = first.completion().expect("first completion");
    assert_eq!(completion.sequence().get(), 6);
    assert_eq!(completion.elapsed_ns().get(), 60);
    assert_eq!(completion.utf8_bytes().get(), 12);
    assert_eq!(completion.unicode_scalar_count().get(), 8);
    assert!(completion.truncated());
    assert!(completion.caused_by().is_empty());

    let cue_one = scope("actor-1", "cue-1", "act-1");
    let cue_two = scope("actor-1", "cue-2", "act-2");
    assert_eq!(model.messages_in_scope(&cue_one).count(), 1);
    assert_eq!(model.messages_in_scope(&cue_two).count(), 1);
    assert_eq!(
        model
            .message(&local_id("message-2"))
            .expect("second message")
            .text(),
        "other"
    );
}

#[test]
fn incremental_and_full_replay_are_byte_identical() {
    let events = interleaved_events();
    let full = project_messages(run_id(), &events).expect("full projection");
    let mut incremental = MessageProjector::new(run_id());
    incremental
        .apply_all(&events[..2])
        .expect("first incremental batch");
    incremental
        .apply_all(&events[2..5])
        .expect("second incremental batch");
    incremental
        .apply_all(&events[5..])
        .expect("last incremental batch");
    let incremental = incremental.into_model();

    assert_eq!(incremental, full);
    assert_eq!(
        incremental.canonical_json().expect("encode incremental"),
        full.canonical_json().expect("encode full")
    );
    let decoded: MessageReadModel =
        serde_json::from_slice(&full.canonical_json().expect("encode read model"))
            .expect("decode read model");
    assert_eq!(decoded, full);
}

#[test]
fn completion_only_messages_remain_representable_without_invented_text() {
    let events = fixture_events(COMPLETED_FIXTURE);
    let model = project_messages(run_id(), &events).expect("project completion fixture");

    assert_eq!(model.messages().len(), 2);
    let empty = model
        .message(&local_id("empty-message"))
        .expect("empty completion");
    assert_eq!(empty.text(), "");
    assert_eq!(empty.first_sequence().get(), 1);
    assert_eq!(
        empty
            .completion()
            .expect("terminal metadata")
            .utf8_bytes()
            .get(),
        0
    );
    let observed_elsewhere = model
        .message(&local_id("message-1"))
        .expect("completion without local deltas");
    assert_eq!(observed_elsewhere.text(), "");
    assert_eq!(
        observed_elsewhere
            .completion()
            .expect("terminal metadata")
            .utf8_bytes()
            .get(),
        20,
        "terminal source counts are facts, not inferred from locally assembled text"
    );
    assert!(observed_elsewhere.is_truncated());
}

#[test]
fn scope_and_nonempty_source_changes_fail_with_stable_identity_errors() {
    let first_scope = scope("actor-1", "cue-1", "act-1");
    for other_scope in [
        scope("actor-2", "cue-1", "act-1"),
        scope("actor-1", "cue-2", "act-1"),
        scope("actor-1", "cue-1", "act-2"),
    ] {
        let mut projector = MessageProjector::new(run_id());
        projector
            .apply(&delta(
                1,
                10,
                first_scope.clone(),
                "message-1",
                Some("provider-1"),
                "one",
            ))
            .expect("first delta");
        let before = projector
            .model()
            .canonical_json()
            .expect("state before error");
        let error = projector
            .apply(&delta(
                2,
                20,
                other_scope.clone(),
                "message-1",
                Some("provider-1"),
                "wrong scope",
            ))
            .expect_err("same Run-local ID cannot move scope");
        assert_eq!(error.code(), "message_identity_mismatch");
        assert_eq!(
            error,
            troupe_diagnostics_runtime::store::projector::messages::MessageProjectionError::IdentityMismatch {
                message_id: local_id("message-1"),
                event_sequence: SchemaU64::new(2),
                field: MessageIdentityField::Scope,
            }
        );
        assert_eq!(
            projector
                .model()
                .canonical_json()
                .expect("state after error"),
            before
        );
        projector
            .apply(&delta(
                2,
                20,
                other_scope,
                "message-2",
                Some("provider-1"),
                "separate",
            ))
            .expect("rejected event did not consume its sequence");
    }

    let mut source_projector = MessageProjector::new(run_id());
    source_projector
        .apply(&delta(
            1,
            10,
            first_scope.clone(),
            "message-1",
            Some("provider-1"),
            "one",
        ))
        .expect("first source");
    let error = source_projector
        .apply(&delta(
            2,
            20,
            first_scope,
            "message-1",
            Some("provider-2"),
            "two",
        ))
        .expect_err("nonempty source identity cannot change");
    assert_eq!(error.code(), "message_identity_mismatch");
    assert_eq!(error.event_sequence().get(), 2);
}

#[test]
fn duplicate_completion_and_delta_after_completion_have_distinct_stable_errors() {
    let message_scope = scope("actor-1", "cue-1", "act-1");
    let prefix = vec![
        delta(1, 10, message_scope.clone(), "message-1", None, "done"),
        completion(2, 20, message_scope.clone(), "message-1", 4, 4, false),
    ];

    let mut duplicate = MessageProjector::new(run_id());
    duplicate.apply_all(&prefix).expect("completed prefix");
    let error = duplicate
        .apply(&completion(
            3,
            30,
            message_scope.clone(),
            "message-1",
            4,
            4,
            false,
        ))
        .expect_err("duplicate completion");
    assert_eq!(error.code(), "duplicate_completion");
    assert_eq!(error.event_sequence().get(), 3);

    let mut after = MessageProjector::new(run_id());
    after.apply_all(&prefix).expect("completed prefix");
    let error = after
        .apply(&delta(3, 30, message_scope, "message-1", None, "late"))
        .expect_err("delta after completion");
    assert_eq!(error.code(), "delta_after_completion");
    assert_eq!(error.event_sequence().get(), 3);
}

#[test]
fn canonical_position_and_reference_failures_do_not_advance_the_model() {
    let message_scope = scope("actor-1", "cue-1", "act-1");
    let skipped = delta(2, 20, message_scope.clone(), "message-1", None, "late");
    let error = project_messages(run_id(), &[skipped]).expect_err("reject skipped sequence");
    assert_eq!(error.code(), "noncanonical_sequence");

    let cross_run = DiagnosticEvent::AgentMessageDelta(AgentMessageDelta::new(
        header_for(other_run_id(), 1, 10, message_scope.clone(), Vec::new()),
        local_id("message-1"),
        None,
        "wrong run".to_owned(),
    ));
    let error = project_messages(run_id(), &[cross_run]).expect_err("reject another Run");
    assert_eq!(error.code(), "cross_run");

    let forward = DiagnosticEvent::AgentMessageDelta(AgentMessageDelta::new(
        header_for(
            run_id(),
            1,
            10,
            message_scope.clone(),
            vec![CausalLink::new(
                SchemaU64::new(2),
                CausalRelation::FollowsFrom,
            )],
        ),
        local_id("message-1"),
        None,
        "bad reference".to_owned(),
    ));
    let mut projector = MessageProjector::new(run_id());
    let error = projector
        .apply(&forward)
        .expect_err("reject forward causal link");
    assert_eq!(error.code(), "forward_link");
    assert_eq!(projector.model().through_sequence().get(), 0);
    projector
        .apply(&delta(
            1,
            10,
            message_scope.clone(),
            "message-1",
            None,
            "valid",
        ))
        .expect("reference rejection did not consume sequence");
    projector
        .apply(&counter(2, 9, message_scope))
        .expect("other facts advance the prefix without message interpretation");
    assert_eq!(projector.model().messages().len(), 1);
    assert_eq!(projector.model().through_sequence().get(), 2);
    assert_eq!(projector.model().through_elapsed_ns().get(), 10);
}
