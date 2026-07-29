use pyo3::exceptions::PyNotImplementedError;
use pyo3::prelude::*;
use pyo3::types::{PyList, PyString};

/// Native Production base with a synchronous constructor over raw argument tokens.
#[pyclass(name = "Production", module = "troupe", subclass)]
pub struct Production;

#[pymethods]
impl Production {
    #[new]
    #[pyo3(signature = (args, /))]
    fn new(args: &Bound<'_, PyList>) -> PyResult<Self> {
        for arg in args.iter() {
            arg.cast::<PyString>()?;
        }
        Ok(Self)
    }

    /// Acquire asynchronous resources before any scene starts.
    async fn start(_self: Py<Self>) {}

    /// Run one scene as the runtime-owned top-level asynchronous task.
    async fn scene(_self: Py<Self>) -> PyResult<()> {
        Err(PyNotImplementedError::new_err(
            "Production.scene() is not implemented",
        ))
    }

    /// Finish asynchronous work, await cleanup, and release resources after the scene.
    async fn stop(_self: Py<Self>) {}
}
