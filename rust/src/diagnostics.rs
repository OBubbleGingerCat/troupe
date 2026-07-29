use pyo3::exceptions::PyBaseException;
use pyo3::prelude::*;
use pyo3::types::{PyAnyMethods, PyList, PyListMethods, PyString};

fn formatted_traceback<'py>(
    py: Python<'py>,
    error: &Bound<'py, PyBaseException>,
) -> PyResult<Bound<'py, PyString>> {
    let traceback = py
        .import("traceback")?
        .getattr("TracebackException")?
        .call_method1("from_exception", (error,))?;
    let lines = traceback.call_method0("format")?;
    PyString::new(py, "")
        .call_method1("join", (lines,))?
        .cast_into::<PyString>()
        .map_err(Into::into)
}

pub(crate) fn format_lifecycle_failure<'py>(
    py: Python<'py>,
    error: &Bound<'py, PyBaseException>,
) -> PyResult<Bound<'py, PyString>> {
    let parts = PyList::empty(py);
    for failure in error.getattr("failures")?.try_iter()? {
        let failure = failure?;
        let phase = failure.getattr("phase")?.extract::<String>()?;
        let original = failure.getattr("error")?.cast_into::<PyBaseException>()?;
        parts.append(PyString::new(
            py,
            &format!("troupe: production failed during {phase} phase\n"),
        ))?;
        parts.append(formatted_traceback(py, &original)?)?;
    }
    PyString::new(py, "")
        .call_method1("join", (&parts,))?
        .cast_into::<PyString>()
        .map_err(Into::into)
}

pub(crate) fn format_load_failure<'py>(
    py: Python<'py>,
    error: &Bound<'py, PyBaseException>,
) -> PyResult<Bound<'py, PyString>> {
    let parts = PyList::new(
        py,
        [
            PyString::new(py, "troupe: failed to load production\n").as_any(),
            formatted_traceback(py, error)?.as_any(),
        ],
    )?;
    PyString::new(py, "")
        .call_method1("join", (&parts,))?
        .cast_into::<PyString>()
        .map_err(Into::into)
}

pub(crate) fn write_stderr(py: Python<'_>, text: &Bound<'_, PyString>) -> PyResult<()> {
    let stderr = py.import("sys")?.getattr("stderr")?;
    stderr.call_method1("write", (text,))?;
    stderr.call_method0("flush")?;
    Ok(())
}

#[pyfunction(name = "_format_failure_for_test")]
pub fn format_failure_for_test<'py>(
    py: Python<'py>,
    error: &Bound<'py, PyBaseException>,
) -> PyResult<Bound<'py, PyString>> {
    format_lifecycle_failure(py, error)
}
