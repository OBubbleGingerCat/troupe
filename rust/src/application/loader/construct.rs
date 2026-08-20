use pyo3::prelude::*;
use pyo3::types::{PyAny, PyList};

use super::{LoadFailure, Reason, ResolvedProductionClass, finish_failure};

pub(crate) fn construct_production(
    py: Python<'_>,
    resolved: ResolvedProductionClass,
    args: &Bound<'_, PyList>,
) -> PyResult<Py<PyAny>> {
    let result = resolved
        .production_type(py)
        .call1((args,))
        .map(Bound::unbind)
        .map_err(|error| LoadFailure::from_error(py, Reason::ConstructionFailed, error));

    match result {
        Ok(production) => Ok(production),
        Err(failure) => {
            let package_dir = resolved.package_dir(py).clone().unbind();
            resolved.rollback(py)?;
            finish_failure(py, package_dir.bind(py), failure)
        }
    }
}
