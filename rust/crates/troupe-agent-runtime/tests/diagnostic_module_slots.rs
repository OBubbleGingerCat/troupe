use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const MODULES: &[&str] = &[
    "context", "cost", "message", "observer", "payload", "plan", "result", "session", "thinking",
    "tool", "usage",
];

fn source(crate_root: &Path, path: &str) -> String {
    fs::read_to_string(crate_root.join("src").join(path)).expect("read Rust source")
}

#[test]
fn diagnostic_slots_and_parent_declarations_are_exact() {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let diagnostics = crate_root.join("src/diagnostics");
    let actual = fs::read_dir(&diagnostics)
        .expect("diagnostics module directory must exist")
        .map(|entry| {
            entry
                .expect("read diagnostics entry")
                .file_name()
                .into_string()
                .expect("diagnostics filenames are UTF-8")
        })
        .collect::<BTreeSet<_>>();
    let expected = std::iter::once("mod.rs".to_owned())
        .chain(MODULES.iter().map(|module| format!("{module}.rs")))
        .collect::<BTreeSet<_>>();
    assert_eq!(actual, expected);

    let declarations = MODULES
        .iter()
        .map(|module| format!("pub(crate) mod {module};"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        source(&crate_root, "diagnostics/mod.rs"),
        format!("{declarations}\n")
    );

    let root = source(&crate_root, "lib.rs");
    for required in [
        "mod diagnostics;",
        "AgentDiagnosticProvider",
        "AgentDiagnosticObserver",
        "AgentDiagnosticObserverInstallError",
        "AgentSessionDiagnosticContext",
        "AgentSessionDiagnosticMetadata",
        "AgentTurnDiagnosticIdentity",
        "AgentTurnDiagnosticMetadata",
        "ToolPayloadCapturePolicy",
        "TurnDiagnosticContext",
        "TurnDiagnosticContextAttachError",
    ] {
        assert!(root.contains(required), "lib.rs is missing {required}");
    }
}

#[test]
fn diagnostic_identity_and_cleanup_boundaries_are_explicit() {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let supervisor = source(&crate_root, "session/supervisor.rs");
    let session = source(&crate_root, "session/mod.rs");
    let turn = source(&crate_root, "session/turn.rs");

    for required in [
        "start_with_diagnostic_context",
        "AgentSessionDiagnosticContext",
    ] {
        assert!(
            supervisor.contains(required),
            "session/supervisor.rs is missing {required}"
        );
    }
    for required in [
        "AgentSessionState::Closing",
        "bind_diagnostic_metadata",
        "diagnostic_cleanup_handle",
    ] {
        assert!(
            session.contains(required),
            "session/mod.rs is missing {required}"
        );
    }
    assert!(
        turn.contains("AgentTurnDiagnosticIdentity"),
        "turn contexts must receive typed native identity"
    );
    assert!(
        turn.contains("runtime_metadata"),
        "turn observations must retain immutable runtime metadata"
    );

    let cancel = session
        .split("    pub fn cancel(&self) {")
        .nth(1)
        .and_then(|tail| {
            tail.split("    #[cfg(feature = \"agent-test-support\")]")
                .next()
        })
        .expect("locate AgentSessionSlot::cancel");
    assert!(cancel.contains("AgentSessionState::Closing"));
    assert!(
        !cancel.contains("observe_closed"),
        "logical cancellation must not report physical cleanup as complete"
    );

    let drop_impl = session
        .split("impl Drop for AgentSessionSlot {")
        .nth(1)
        .and_then(|tail| tail.split("pub(crate) fn spawn_opening").next())
        .expect("locate AgentSessionSlot::drop");
    assert!(drop_impl.contains("AgentSessionState::Closing"));
    assert!(
        !drop_impl.contains("observe_closed"),
        "drop cannot claim that asynchronous process cleanup already finished"
    );
}

#[test]
fn shared_roots_keep_all_typed_observation_seams() {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let expectations = [
        (
            "session/supervisor.rs",
            &[
                "install_diagnostic_observer",
                "new_with_session_diagnostics",
                "observe_opening",
            ][..],
        ),
        (
            "session/mod.rs",
            &[
                "observe_opening_attempt",
                "observe_ready",
                "observe_broken",
                "observe_closing",
                "observe_closed",
                "observe_update",
                "arm_result_with_diagnostics",
            ][..],
        ),
        (
            "session/turn.rs",
            &[
                "new_diagnostic_context",
                "install_diagnostic_context",
                "diagnostic_context",
                "observe_turn_submitted",
                "observe_turn_terminal",
            ][..],
        ),
        (
            "result/mod.rs",
            &[
                "observe_submitted",
                "observe_validation_rejected",
                "observe_repair_requested",
                "observe_accepted",
                "observe_missing",
            ][..],
        ),
    ];
    for (path, required) in expectations {
        let source = source(&crate_root, path);
        for marker in required {
            assert!(source.contains(marker), "{path} is missing {marker}");
        }
    }
}

#[test]
fn diagnostics_disabled_terminal_path_preserves_the_response_move() {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let turn = source(&crate_root, "session/turn.rs");

    assert!(
        !turn.contains("response.stop_reason.clone()"),
        "the diagnostics-disabled path must not clone the stop reason"
    );
    assert!(
        turn.contains("outcome_from_settlement(response.stop_reason, result)"),
        "the baseline settlement path must keep moving the stop reason"
    );
    assert!(
        turn.contains("fn retain_prompt_response_for_diagnostics("),
        "terminal diagnostics must use the context-gated retention helper"
    );
    assert!(
        turn.contains("if context.is_none()"),
        "response retention must explicitly short-circuit without diagnostics"
    );
    assert!(
        turn.contains("response.as_ref().ok().cloned()"),
        "only the diagnostics retention helper may clone a successful response"
    );
    assert!(
        turn.contains(
            "retain_prompt_response_for_diagnostics(\n                state.diagnostic_context.as_ref(),\n                &response,\n            )"
        ),
        "complete_response must gate response retention on its diagnostic context"
    );
}

#[test]
fn diagnostic_slots_do_not_take_runtime_or_transport_dependencies() {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for module in MODULES {
        let source = source(&crate_root, &format!("diagnostics/{module}.rs"));
        for forbidden in ["pyo3", "rusqlite", "hyper", "reqwest", "std::net"] {
            assert!(
                !source.contains(forbidden),
                "diagnostics/{module}.rs must not depend on {forbidden}"
            );
        }
    }
}
