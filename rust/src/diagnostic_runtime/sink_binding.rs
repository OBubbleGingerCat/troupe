use std::sync::Arc;

use pyo3::prelude::*;
use troupe_agent_runtime::AgentTurnControl;

use crate::orchestration::scene_context::RunBinding;

use super::hooks::DiagnosticActBinding;

pub(crate) fn admit_act(
    py: Python<'_>,
    run: &RunBinding,
    control: &Arc<AgentTurnControl>,
    binding: DiagnosticActBinding,
) -> PyResult<()> {
    if let Some(capability) = run.diagnostic_admission().capability() {
        return capability.admit_act(py, control, binding);
    }
    if !binding.is_active() {
        return Ok(());
    }
    Ok(())
}
