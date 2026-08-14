use pyo3::pymodule;

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

mod act_call;
mod application;
#[allow(dead_code)]
mod diagnostic_python;
#[allow(dead_code)]
mod diagnostic_runtime;
#[allow(dead_code)]
mod diagnostic_sink;
mod orchestration;

use troupe_agent_runtime as agent;

#[pymodule(gil_used = true)]
mod _runtime {
    use pyo3::prelude::*;

    #[cfg(feature = "agent-test-support")]
    #[pymodule_export]
    use crate::agent::result_generation_isolation_for_test;
    #[pymodule_export]
    use crate::agent::{
        AgentAuthenticationRequiredError, AgentError, AgentResultError, AgentResultIssue,
        AgentResultMissingError, AgentSessionBrokenError, AgentSessionBusyError, AgentSessionError,
        AgentSessionStartError, AgentTurnError,
    };
    #[cfg(feature = "agent-test-support")]
    #[pymodule_export]
    use crate::agent::{
        hold_test_configuration_ready, hold_test_mcp_ready, hold_test_opening,
        hold_test_opening_backoff, hold_test_turn_intake, hold_test_turn_outcome,
        hold_test_turn_registration, hold_test_turn_response_flush, hold_test_turn_settlement,
        hold_test_turn_submission, hold_test_turn_terminal_delivery, launch_specs_for_test,
        opening_backoff_state, readiness_gate_states, release_test_configuration_ready,
        release_test_mcp_ready, release_test_opening, release_test_opening_backoff,
        release_test_turn_intake, release_test_turn_outcome, release_test_turn_registration,
        release_test_turn_response_flush, release_test_turn_settlement,
        release_test_turn_submission, release_test_turn_terminal_delivery, reset_test_launch,
        set_test_launch, turn_gate_states,
    };
    #[cfg(feature = "agent-test-support")]
    #[pymodule_export]
    use crate::agent::{permission_for_test, settlement_for_test, supervisor_response_for_test};
    #[pymodule_export]
    use crate::application::cli::main;
    #[pymodule_export]
    use crate::application::diagnostics::format_failure_for_test;
    #[pymodule_export]
    use crate::application::failure::{PhaseFailure, ProductionFailed};
    #[pymodule_export]
    use crate::application::invocation::parse_invocation;
    #[pymodule_export]
    use crate::application::loader::{ProductionLoadError, load_production};
    #[pymodule_export]
    use crate::orchestration::actor::Actor;
    #[pymodule_export]
    use crate::orchestration::actor_handle::ActorHandle;
    #[pymodule_export]
    use crate::orchestration::cue::{Cue, CueContextError};
    #[pymodule_export]
    use crate::orchestration::effect::{Effect, EffectContextError};
    #[pymodule_export]
    use crate::orchestration::production::Production;
    #[pymodule_export]
    use crate::orchestration::runtime::Runtime;

    #[pymodule_init]
    fn init_private_coroutines(module: &Bound<'_, PyModule>) -> PyResult<()> {
        crate::agent::install_schema(module)?;
        crate::diagnostic_python::install(module)?;
        let coroutine = module
            .py()
            .import("collections.abc")?
            .getattr("Coroutine")?;
        coroutine.call_method1(
            "register",
            (module
                .py()
                .get_type::<crate::orchestration::cue_future::CueCall>(),),
        )?;
        coroutine.call_method1(
            "register",
            (module.py().get_type::<crate::act_call::ActCall>(),),
        )?;
        coroutine.call_method1(
            "register",
            (module
                .py()
                .get_type::<crate::orchestration::scene_context::ScopeDriver>(),),
        )?;
        Ok(())
    }
}
