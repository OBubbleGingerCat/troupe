use pyo3::PyErr;

use crate::orchestration::scene_context::SceneScope;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SceneDrainHook {
    AdmissionClosed,
    CancellationStarted,
    CleanupStarted,
    CleanupFinished,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SceneDriverExit {
    Returned,
    Closed,
    Cleared,
}

#[inline]
pub(crate) fn observe(_scene: &SceneScope, _hook: SceneDrainHook) {}

#[inline]
pub(crate) fn driver_exited(_scene: &SceneScope, _exit: SceneDriverExit, _error: Option<&PyErr>) {}
