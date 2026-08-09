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

mod actor;
mod actor_handle;
mod actor_registry;
mod agent_error;
mod agent_launch;
mod agent_profile;
mod agent_session;
mod agent_supervisor;
mod cli;
mod cue;
mod cue_future;
mod diagnostics;
mod effect;
mod failure;
mod invocation;
mod loader;
mod mailbox;
mod production;
mod python_task;
mod result_mcp;
mod runtime;
mod scene_context;
mod signals;

#[pymodule(gil_used = true)]
mod _runtime {
    use pyo3::prelude::*;

    #[pymodule_export]
    use crate::actor::Actor;
    #[pymodule_export]
    use crate::actor_handle::ActorHandle;
    #[pymodule_export]
    use crate::agent_error::{
        AgentAuthenticationRequiredError, AgentError, AgentSessionError, AgentSessionStartError,
    };
    #[cfg(feature = "agent-test-support")]
    #[pymodule_export]
    use crate::agent_launch::{
        hold_test_configuration_ready, hold_test_mcp_ready, hold_test_opening,
        launch_specs_for_test, readiness_gate_states, release_test_configuration_ready,
        release_test_mcp_ready, release_test_opening, reset_test_launch, set_test_launch,
    };
    #[pymodule_export]
    use crate::cli::main;
    #[pymodule_export]
    use crate::cue::{Cue, CueContextError};
    #[pymodule_export]
    use crate::diagnostics::format_failure_for_test;
    #[pymodule_export]
    use crate::effect::{Effect, EffectContextError};
    #[pymodule_export]
    use crate::failure::{PhaseFailure, ProductionFailed};
    #[pymodule_export]
    use crate::invocation::parse_invocation;
    #[pymodule_export]
    use crate::loader::{ProductionLoadError, load_production};
    #[pymodule_export]
    use crate::production::Production;
    #[cfg(feature = "agent-test-support")]
    #[pymodule_export]
    use crate::result_mcp::result_generation_isolation_for_test;
    #[pymodule_export]
    use crate::runtime::Runtime;

    #[pymodule_init]
    fn init_private_coroutines(module: &Bound<'_, PyModule>) -> PyResult<()> {
        let coroutine = module
            .py()
            .import("collections.abc")?
            .getattr("Coroutine")?;
        coroutine.call_method1(
            "register",
            (module.py().get_type::<crate::cue_future::CueCall>(),),
        )?;
        coroutine.call_method1(
            "register",
            (module.py().get_type::<crate::scene_context::ScopeDriver>(),),
        )?;
        Ok(())
    }
}
