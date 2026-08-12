use pyo3::create_exception;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

create_exception!(troupe, AgentError, PyRuntimeError);
create_exception!(troupe, AgentSessionBusyError, AgentError);
create_exception!(troupe, AgentSessionError, AgentError);
create_exception!(troupe, AgentSessionStartError, AgentSessionError);
create_exception!(
    troupe,
    AgentAuthenticationRequiredError,
    AgentSessionStartError
);
create_exception!(troupe, AgentSessionBrokenError, AgentSessionError);
create_exception!(troupe, AgentTurnError, AgentError);
create_exception!(troupe, AgentResultError, AgentTurnError);
create_exception!(troupe, AgentResultMissingError, AgentResultError);

#[pyclass(frozen, module = "troupe")]
pub(crate) struct AgentResultIssue {
    #[pyo3(get)]
    path: String,
    #[pyo3(get)]
    code: &'static str,
    #[pyo3(get)]
    message: String,
}

pub(crate) struct AgentResultIssueData {
    pub(crate) path: String,
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

fn set_code(error: &PyErr, py: Python<'_>, code: &'static str) {
    error
        .value(py)
        .setattr("code", code)
        .expect("agent exception instances accept a code attribute");
}

pub(crate) fn busy_error(py: Python<'_>) -> PyErr {
    let error = PyErr::new::<AgentSessionBusyError, _>("another act call is already active");
    set_code(&error, py, "concurrent_act");
    error
}

pub(crate) fn session_broken_error(py: Python<'_>, failure: &AgentSessionFailure) -> PyErr {
    let error = PyErr::new::<AgentSessionBrokenError, _>(failure.message);
    set_code(&error, py, failure.code);
    error
}

pub(crate) fn turn_error(py: Python<'_>, code: &'static str, message: &'static str) -> PyErr {
    let error = PyErr::new::<AgentTurnError, _>(message);
    set_code(&error, py, code);
    error
}

pub(crate) fn result_error(
    py: Python<'_>,
    code: &'static str,
    message: &'static str,
    issues: Vec<AgentResultIssueData>,
    invalid_calls: u8,
    details_truncated: bool,
) -> PyErr {
    let error = PyErr::new::<AgentResultError, _>(message);
    set_code(&error, py, code);
    let issues = issues.into_iter().map(|issue| {
        Py::new(
            py,
            AgentResultIssue {
                path: issue.path,
                code: issue.code,
                message: issue.message,
            },
        )
        .expect("a bounded AgentResultIssue is allocatable")
    });
    let issues = pyo3::types::PyTuple::new(py, issues)
        .expect("a bounded AgentResultIssue tuple is allocatable");
    let value = error.value(py);
    value
        .setattr("issues", issues)
        .expect("result exception instances accept issues");
    value
        .setattr("invalid_calls", invalid_calls)
        .expect("result exception instances accept invalid_calls");
    value
        .setattr("details_truncated", details_truncated)
        .expect("result exception instances accept details_truncated");
    error
}

pub(crate) fn missing_result_error(py: Python<'_>) -> PyErr {
    let error = PyErr::new::<AgentResultMissingError, _>("agent turn ended without a result");
    set_code(&error, py, "missing_result");
    let value = error.value(py);
    value
        .setattr("issues", pyo3::types::PyTuple::empty(py))
        .expect("missing-result exception instances accept issues");
    value
        .setattr("invalid_calls", 0)
        .expect("missing-result exception instances accept invalid_calls");
    value
        .setattr("details_truncated", false)
        .expect("missing-result exception instances accept details_truncated");
    error
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AgentStartupFailure {
    pub(crate) code: &'static str,
    pub(crate) phase: &'static str,
    pub(crate) message: &'static str,
    pub(crate) authentication_required: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AgentSessionFailure {
    pub(crate) code: &'static str,
    pub(crate) message: &'static str,
}

impl AgentSessionFailure {
    pub(crate) const fn process_exited() -> Self {
        Self {
            code: "process_exited",
            message: "agent session process exited",
        }
    }

    pub(crate) const fn transport_lost() -> Self {
        Self {
            code: "transport_lost",
            message: "agent session transport was lost",
        }
    }

    pub(crate) const fn protocol_violation() -> Self {
        Self {
            code: "protocol_violation",
            message: "agent session violated the protocol contract",
        }
    }

    pub(crate) const fn uncertain_settlement() -> Self {
        Self {
            code: "uncertain_settlement",
            message: "agent turn settlement is uncertain",
        }
    }

    pub(crate) const fn authentication_lost() -> Self {
        Self {
            code: "authentication_lost",
            message: "agent session authentication was lost",
        }
    }

    pub(crate) const fn result_channel_lost() -> Self {
        Self {
            code: "result_channel_lost",
            message: "agent result channel was lost",
        }
    }

    pub(crate) const fn resource_limit() -> Self {
        Self {
            code: "resource_limit",
            message: "agent session exceeded ResourceLimitsV1",
        }
    }
}

impl AgentStartupFailure {
    pub(crate) fn start(code: &'static str, phase: &'static str, message: &'static str) -> Self {
        assert_start_phase(phase);
        Self {
            code,
            phase,
            message,
            authentication_required: false,
        }
    }

    pub(crate) fn authentication_required(phase: &'static str) -> Self {
        assert_start_phase(phase);
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

fn assert_start_phase(phase: &str) {
    assert!(
        matches!(
            phase,
            "preparation" | "spawn" | "initialize" | "session_new" | "configure" | "mcp_ready"
        ),
        "startup phase must use the closed public vocabulary"
    );
}

#[cfg(test)]
mod tests {
    use pyo3::prelude::*;
    use pyo3::types::PyTuple;

    use super::{
        AgentResultIssueData, AgentStartupFailure, busy_error, missing_result_error, result_error,
    };

    #[test]
    #[should_panic(expected = "startup phase must use the closed public vocabulary")]
    fn startup_failure_rejects_non_contract_phase() {
        let _ = AgentStartupFailure::start(
            "result_channel_unavailable",
            "configuration",
            "invalid internal phase",
        );
    }

    #[test]
    fn turn_error_factories_publish_only_normalized_fields() {
        let _guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let busy = busy_error(py);
            assert_eq!(
                busy.value(py)
                    .getattr("code")
                    .unwrap()
                    .extract::<&str>()
                    .unwrap(),
                "concurrent_act",
            );

            let result = result_error(
                py,
                "too_many_invalid_results",
                "agent submitted too many invalid results",
                vec![AgentResultIssueData {
                    path: "/score".to_owned(),
                    code: "out_of_range",
                    message: "score is outside the accepted range".to_owned(),
                }],
                9,
                true,
            );
            let value = result.value(py);
            assert_eq!(
                value.getattr("code").unwrap().extract::<&str>().unwrap(),
                "too_many_invalid_results",
            );
            assert_eq!(
                value
                    .getattr("invalid_calls")
                    .unwrap()
                    .extract::<u8>()
                    .unwrap(),
                9,
            );
            assert!(
                value
                    .getattr("details_truncated")
                    .unwrap()
                    .extract::<bool>()
                    .unwrap()
            );
            let issues = value
                .getattr("issues")
                .unwrap()
                .cast_into::<PyTuple>()
                .unwrap();
            assert_eq!(issues.len(), 1);
            let issue = issues.get_item(0).unwrap();
            assert_eq!(
                issue.getattr("path").unwrap().extract::<&str>().unwrap(),
                "/score",
            );
            assert_eq!(
                issue.getattr("code").unwrap().extract::<&str>().unwrap(),
                "out_of_range",
            );

            let missing = missing_result_error(py);
            let value = missing.value(py);
            assert_eq!(
                value.getattr("code").unwrap().extract::<&str>().unwrap(),
                "missing_result",
            );
            assert_eq!(
                value
                    .getattr("invalid_calls")
                    .unwrap()
                    .extract::<u8>()
                    .unwrap(),
                0,
            );
            assert_eq!(
                value
                    .getattr("issues")
                    .unwrap()
                    .cast_into::<PyTuple>()
                    .unwrap()
                    .len(),
                0,
            );
            assert!(
                !value
                    .getattr("details_truncated")
                    .unwrap()
                    .is_truthy()
                    .unwrap()
            );
        });
    }
}
