use std::sync::Arc;

use pyo3::prelude::*;
use troupe_agent_runtime::AgentTurnControl;

use crate::orchestration::scene_context::{CuedScope, RunBinding};

use super::{act_producer, hooks::DiagnosticActBinding};

pub(crate) fn admit_act(
    py: Python<'_>,
    run: &RunBinding,
    cued: &Arc<CuedScope>,
    control: &Arc<AgentTurnControl>,
    binding: DiagnosticActBinding,
) -> PyResult<()> {
    if let Some(capability) = run.diagnostic_admission().capability() {
        return capability.admit_act(py, run, cued, control, binding);
    }
    act_producer::admitted(run, cued, control);
    let _ = binding;
    Ok(())
}
