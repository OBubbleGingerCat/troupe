use std::sync::Arc;

use pyo3::PyErr;
use troupe_agent_runtime::AgentTurnControl;

use crate::orchestration::scene_context::{CuedScope, RunBinding};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActHook {
    Admitted,
    DriverStarted,
    CancelRequested,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActCallerExit {
    Returned,
    AdmissionFailed,
    Cancelled,
    Closed,
    Cleared,
}

#[inline]
pub(crate) fn admitted(_binding: &RunBinding, _cued: &CuedScope, _control: &Arc<AgentTurnControl>) {
}

#[inline]
pub(crate) fn observe(_control: &Arc<AgentTurnControl>, _hook: ActHook) {}

#[inline]
pub(crate) fn caller_finished(
    _control: &Arc<AgentTurnControl>,
    _exit: ActCallerExit,
    _error: Option<&PyErr>,
) {
}
