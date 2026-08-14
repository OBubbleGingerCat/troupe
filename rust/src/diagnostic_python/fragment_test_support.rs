use std::ffi::CStr;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyModule;

pub(crate) fn install_fresh_fragment<'py>(
    py: Python<'py>,
    source: &CStr,
    filename: &CStr,
    module_name: &CStr,
) -> PyResult<Bound<'py, PyModule>> {
    let module_name = module_name
        .to_str()
        .map_err(|_| PyValueError::new_err("fragment module name must be UTF-8"))?;
    let filename = filename
        .to_str()
        .map_err(|_| PyValueError::new_err("fragment filename must be UTF-8"))?;
    let module = PyModule::new(py, module_name)?;
    module.setattr("__file__", filename)?;
    let namespace = module.dict();
    py.run(source, Some(&namespace), Some(&namespace))?;
    Ok(module)
}
