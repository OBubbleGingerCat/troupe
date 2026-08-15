use std::sync::Arc;

use pyo3::types::PyString;
use pyo3::{Bound, Py, PyErr};
use troupe_diagnostics_core::scalar::SchemaU64;

use crate::diagnostic_runtime::cue_producer::CueTerminalOutcome;
use crate::orchestration::effect::{Effect, EffectConstruction};
use crate::orchestration::scene_context::CuedScope;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EffectHook {
    Created,
    Returned,
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

#[inline]
pub(crate) fn cue_terminal(
    _cued: &Arc<CuedScope>,
    _outcome: CueTerminalOutcome,
    _causal_source: SchemaU64,
) {
}

#[inline]
pub(crate) fn caller_finished(_cued: &Arc<CuedScope>) {}
