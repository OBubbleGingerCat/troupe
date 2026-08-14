use std::sync::Arc;

use pyo3::types::{PyAny, PyString, PyType};
use pyo3::{Bound, Py, PyErr};

use crate::orchestration::actor::{Actor, ActorCapabilityNode, ActorIdentity};
use crate::orchestration::actor_handle::{ActorHandle, ActorHandleIdentity};
use crate::orchestration::actor_registry::ProductionState;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActorHook {
    Constructed,
    HandleCreated,
    HandleCleared,
    RegistryReserved,
    RegistryCommitted,
    RegistryDetached,
}

#[inline]
pub(crate) fn observe_identity(
    _production: Option<&ProductionState>,
    _identity: &ActorIdentity,
    _name: Option<&Bound<'_, PyString>>,
    _hook: ActorHook,
) {
}

#[inline]
pub(crate) fn observe_handle(
    _handle: ActorHandleIdentity,
    _node: &Py<ActorCapabilityNode>,
    _hook: ActorHook,
) {
}

#[inline]
pub(crate) fn cleared(_actor: &Actor, _production: Option<&Py<PyAny>>) {}

#[inline]
pub(crate) fn cast_started(
    _production: &ProductionState,
    _identity: &Arc<ActorIdentity>,
    _actor_type: &Bound<'_, PyType>,
    _name: &Bound<'_, PyString>,
) {
}

#[inline]
pub(crate) fn cast_finished(
    _production: &ProductionState,
    _identity: &Arc<ActorIdentity>,
    _actor_type: &Bound<'_, PyType>,
    _name: &Bound<'_, PyString>,
    _outcome: Result<&Py<ActorHandle>, &PyErr>,
) {
}
