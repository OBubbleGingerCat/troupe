use pyo3::prelude::*;

use crate::orchestration::actor_registry::ProductionState;
use crate::orchestration::runtime::RuntimeCore;
use crate::orchestration::scene_context::RunBinding;

#[inline]
pub(crate) fn bind_run(
    _py: Python<'_>,
    _core: &RuntimeCore,
    _production: &ProductionState,
    _binding: &RunBinding,
) -> PyResult<()> {
    Ok(())
}

#[inline]
pub(crate) fn production_created(_py: Python<'_>, _production: &ProductionState) -> PyResult<()> {
    Ok(())
}
