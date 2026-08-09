use pyo3::create_exception;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

create_exception!(troupe, AgentError, PyRuntimeError);
create_exception!(troupe, AgentSessionError, AgentError);
create_exception!(troupe, AgentSessionStartError, AgentSessionError);
create_exception!(
    troupe,
    AgentAuthenticationRequiredError,
    AgentSessionStartError
);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AgentStartupFailure {
    pub(crate) code: &'static str,
    pub(crate) phase: &'static str,
    pub(crate) message: &'static str,
    pub(crate) authentication_required: bool,
}

impl AgentStartupFailure {
    pub(crate) const fn start(
        code: &'static str,
        phase: &'static str,
        message: &'static str,
    ) -> Self {
        Self {
            code,
            phase,
            message,
            authentication_required: false,
        }
    }

    pub(crate) const fn authentication_required(phase: &'static str) -> Self {
        Self {
            code: "authentication_required",
            phase,
            message: "agent authentication is required",
            authentication_required: true,
        }
    }

    pub(crate) fn to_pyerr(&self, py: Python<'_>) -> PyErr {
        let error = if self.authentication_required {
            PyErr::new::<AgentAuthenticationRequiredError, _>(self.message)
        } else {
            PyErr::new::<AgentSessionStartError, _>(self.message)
        };
        let value = error.value(py);
        value
            .setattr("code", self.code)
            .expect("agent exception instances accept a code attribute");
        value
            .setattr("phase", self.phase)
            .expect("startup exception instances accept a phase attribute");
        error
    }
}
