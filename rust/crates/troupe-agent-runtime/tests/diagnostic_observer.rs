use std::{fs, path::PathBuf};

fn source(path: &str) -> String {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    fs::read_to_string(crate_root.join("src").join(path)).expect("read Rust source")
}

#[test]
fn observer_contract_is_typed_nonblocking_and_owner_isolated() {
    let observer = source("diagnostics/observer.rs");

    for required in [
        "pub enum AgentDiagnosticObservation",
        "pub enum AgentDiagnosticObservationKind",
        "pub trait AgentDiagnosticCandidate",
        "pub trait AgentDiagnosticDestination",
        "fn try_observe(",
        "pub trait AgentDiagnosticFailureOwner",
        "fn observer_failed(",
        "pub struct AgentDiagnosticObserverFailure",
        "catch_unwind",
    ] {
        assert!(observer.contains(required), "observer contract is missing {required}");
    }

    for forbidden in ["serde_json", "SessionUpdate", "PromptResponse", "raw_json"] {
        assert!(
            !observer.contains(forbidden),
            "observer contract must not expose {forbidden}"
        );
    }
}

#[test]
fn session_and_turn_metadata_keep_destination_and_sidecar_policy_separate() {
    let session = source("diagnostics/session.rs");

    for required in [
        "effective_observer: Option<AgentDiagnosticObserver>",
        "tool_payload_capture: ToolPayloadCapturePolicy",
        "or_else(|| self.standalone_observer.take())",
        "AgentDiagnosticObservation::SessionOpening",
        "AgentDiagnosticObservation::SessionReady",
        "AgentDiagnosticObservation::SessionBroken",
        "AgentDiagnosticObservation::SessionClosing",
        "AgentDiagnosticObservation::SessionClosed",
        "AgentDiagnosticObservation::TurnSubmitted",
        "AgentDiagnosticObservation::TurnTerminal",
    ] {
        assert!(session.contains(required), "session contract is missing {required}");
    }

    let production = session
        .find("self.effective_observer = session_observer")
        .expect("Production observer selection");
    let standalone = session[production..]
        .find("or_else(|| self.standalone_observer.take())")
        .map(|offset| production + offset)
        .expect("standalone fallback selection");
    assert!(production < standalone, "Run observer must win over sidecar fallback");
}

#[test]
fn validated_acp_update_reaches_diagnostics_only_after_adapter_acceptance() {
    let session = source("session/mod.rs");
    let accepted = session
        .find("if accepted_for_diagnostics")
        .expect("validated update acceptance boundary");
    let observed = session[accepted..]
        .find("slot.observe_update(")
        .map(|offset| accepted + offset)
        .expect("diagnostic observation hook after validation");

    assert!(observed > accepted);
    assert!(
        session[accepted..observed].contains("slot_for_updates.upgrade()"),
        "only the accepted session destination may observe the update"
    );
}
