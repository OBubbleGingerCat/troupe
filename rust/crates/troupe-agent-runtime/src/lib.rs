#[cfg(test)]
static PYTHON_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn initialize_python_for_test() -> std::sync::MutexGuard<'static, ()> {
    let guard = PYTHON_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    pyo3::Python::initialize();
    guard
}

mod adapter;
pub mod diagnostics;
mod error;
mod launch;
mod profile;
mod result;
mod schema;
mod session;

#[cfg(feature = "agent-test-support")]
pub use adapter::{permission_for_test, settlement_for_test, supervisor_response_for_test};
pub use diagnostics::observer::{
    AgentDiagnosticCandidate, AgentDiagnosticDestination, AgentDiagnosticErrorCode,
    AgentDiagnosticFailureOwner, AgentDiagnosticObservation, AgentDiagnosticObservationKind,
    AgentDiagnosticObserver, AgentDiagnosticObserverFailure, AgentDiagnosticObserverInstallError,
    AgentTurnDiagnosticOutcome, AgentTurnDiagnosticSettlement,
};
pub use diagnostics::payload::ToolPayloadCapturePolicy;
pub use diagnostics::session::{
    AgentDiagnosticProvider, AgentDiagnosticSnapshotError, AgentSessionDiagnosticContext,
    AgentSessionDiagnosticMetadata, AgentTurnDiagnosticIdentity, AgentTurnDiagnosticMetadata,
    TurnDiagnosticContext, TurnDiagnosticContextAttachError,
};
pub use error::{
    AgentAuthenticationRequiredError, AgentError, AgentResultError, AgentResultIssue,
    AgentResultIssueData, AgentResultMissingError, AgentSessionBrokenError, AgentSessionBusyError,
    AgentSessionError, AgentSessionFailure, AgentSessionStartError, AgentStartupFailure,
    AgentTurnError, busy_error, missing_result_error, result_error, session_broken_error,
    turn_error,
};
pub use launch::ResolvedLaunch;
#[cfg(feature = "agent-test-support")]
pub use launch::{
    hold_test_configuration_ready, hold_test_mcp_ready, hold_test_opening,
    hold_test_opening_backoff, hold_test_turn_intake, hold_test_turn_outcome,
    hold_test_turn_registration, hold_test_turn_response_flush, hold_test_turn_settlement,
    hold_test_turn_submission, hold_test_turn_terminal_delivery, launch_specs_for_test,
    opening_backoff_state, readiness_gate_states, release_test_configuration_ready,
    release_test_mcp_ready, release_test_opening, release_test_opening_backoff,
    release_test_turn_intake, release_test_turn_outcome, release_test_turn_registration,
    release_test_turn_response_flush, release_test_turn_settlement, release_test_turn_submission,
    release_test_turn_terminal_delivery, reset_test_launch, set_test_launch, turn_gate_states,
};
pub use profile::{ResolvedAgentProfile, resolve_agent_profile};
pub use result::MAX_REPAIRABLE_INVALID_CALLS;
#[cfg(feature = "agent-test-support")]
pub use result::result_generation_isolation_for_test;
pub use schema::validation_bridge::PythonSchemaValidationBridge;
pub use schema::{CompiledActSchema, SchemaValidationMode, compile_act_schema, extract_script};
pub use session::supervisor::{AgentCastPermit, AgentSupervisor};
pub use session::turn::{
    AgentTurnCancelDecision, AgentTurnControl, AgentTurnOutcome, AgentTurnStop,
};
pub use session::{AgentActError, AgentSessionSlot};
#[cfg(feature = "agent-test-support")]
pub use session::{AgentInfoSnapshotForTest, AgentReadySnapshotForTest};

pub fn install_schema(module: &pyo3::Bound<'_, pyo3::types::PyModule>) -> pyo3::PyResult<()> {
    schema::install(module)
}
