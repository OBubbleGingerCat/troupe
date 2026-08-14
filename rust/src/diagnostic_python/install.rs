use pyo3::prelude::*;

pub(crate) fn install(_module: &Bound<'_, PyModule>) -> PyResult<()> {
    Ok(())
}
