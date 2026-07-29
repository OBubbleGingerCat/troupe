use pyo3::class::gc::{PyTraverseError, PyVisit};
use pyo3::create_exception;
use pyo3::exceptions::{PyBaseException, PyException};
use pyo3::prelude::*;
use pyo3::types::{PyAnyMethods, PyString, PyTuple};

create_exception!(troupe._runtime, ProductionFailed, PyException);

#[pyclass(name = "PhaseFailure", module = "troupe._runtime", frozen)]
pub struct PhaseFailure {
    #[pyo3(get)]
    phase: Py<PyString>,
    #[pyo3(get)]
    error: Py<PyBaseException>,
}

#[pymethods]
impl PhaseFailure {
    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.phase)?;
        visit.call(&self.error)
    }
}

pub(crate) fn lifecycle_result(failures: Vec<(&'static str, PyErr)>) -> PyResult<()> {
    if failures.is_empty() {
        return Ok(());
    }

    Python::attach(|py| {
        let failures = failures
            .into_iter()
            .map(|(phase, error)| {
                Py::new(
                    py,
                    PhaseFailure {
                        phase: PyString::new(py, phase).unbind(),
                        error: error.into_value(py),
                    },
                )
            })
            .collect::<PyResult<Vec<_>>>()?;
        let failures = PyTuple::new(py, failures)?;
        let error = py
            .get_type::<ProductionFailed>()
            .call1(("production lifecycle failed",))?;
        error.setattr("failures", failures)?;
        Err(PyErr::from_value(error))
    })
}
