use pyo3::PyErr;

use crate::orchestration::actor_registry::ProductionState;
use crate::orchestration::runtime::RuntimeCore;
use crate::orchestration::scene_context::RunBinding;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeHook {
    ProductionCreated,
    RunStarted,
    ProductionStartEntered,
    ProductionStartReturned,
    SceneEntered,
    SceneReturned,
    ProductionStopEntered,
    ProductionStopReturned,
    ShutdownRequested,
    RunFinished,
}

#[inline]
pub(crate) fn observe_core(_core: &RuntimeCore, _hook: RuntimeHook) {}

#[inline]
pub(crate) fn run_started(_core: &RuntimeCore, _binding: &RunBinding) {}

#[inline]
pub(crate) fn observe_production(_production: &ProductionState, _hook: RuntimeHook) {}

#[inline]
pub(crate) fn observe_binding(_binding: &RunBinding, _hook: RuntimeHook, _error: Option<&PyErr>) {}
