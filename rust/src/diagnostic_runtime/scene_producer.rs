use crate::orchestration::python_task::TaskLineage;
use crate::orchestration::scene_context::{RunBinding, SceneScope};
use pyo3::PyErr;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SceneHook {
    BindingCreated,
    SceneCreated,
    TaskRegistered,
    SceneFinished,
}

#[inline]
pub(crate) fn binding_created(_binding: &RunBinding) {}

#[inline]
pub(crate) fn observe_scene(_scene: &SceneScope, _hook: SceneHook) {}

#[inline]
pub(crate) fn observe_task(_lineage: &TaskLineage, _hook: SceneHook) {}

#[inline]
pub(crate) fn task_finished(_scene: &SceneScope, _error: Option<&PyErr>) {}
