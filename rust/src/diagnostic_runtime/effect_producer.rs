use pyo3::types::PyString;
use pyo3::{Bound, Py, PyErr};

use crate::orchestration::effect::{Effect, EffectConstruction};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EffectHook {
    Created,
    Consumed,
}

#[inline]
pub(crate) fn observe(_effect: &Effect, _hook: EffectHook) {}

#[inline]
pub(crate) fn construction_started(_construction: &EffectConstruction) {}

#[inline]
pub(crate) fn construction_finished(
    _construction: &EffectConstruction,
    _outcome: Result<&Bound<'_, Effect>, &PyErr>,
) {
}

#[inline]
pub(crate) fn cleared(_effect: &Effect, _id: Option<&Py<PyString>>, _owner: Option<&Py<PyString>>) {
}
