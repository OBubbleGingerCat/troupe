use pyo3::types::PyString;
use pyo3::{Py, PyErr};

use crate::orchestration::mailbox::CueOperation;
use crate::orchestration::scene_context::{RunBinding, SceneScope};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CueHook {
    Admitted,
    Dispatched,
    CancelRequested,
    CallerFinished,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CueMailboxHook {
    Enqueued,
    Retired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CueTerminalOutcome {
    Completed,
    Failed,
    Cancelled,
    CleanupFailed,
}

#[inline]
pub(crate) fn created(_id: &Py<PyString>, _source: &Py<PyString>) {}

#[inline]
pub(crate) fn admission_started(_binding: &RunBinding, _scene: &SceneScope) {}

#[inline]
pub(crate) fn observe(_operation: &CueOperation, _hook: CueHook) {}

#[inline]
pub(crate) fn mailbox_changed(
    _operation: &CueOperation,
    _hook: CueMailboxHook,
    _queued: usize,
    _running: bool,
) {
}

#[inline]
pub(crate) fn terminal(
    _operation: &CueOperation,
    _outcome: CueTerminalOutcome,
    _error: impl FnOnce() -> Option<PyErr>,
) {
}
