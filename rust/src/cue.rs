use std::sync::{Mutex, MutexGuard};

use pyo3::PyTypeCheck;
use pyo3::class::gc::{PyTraverseError, PyVisit};
use pyo3::create_exception;
use pyo3::exceptions::{PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyString};

const CUE_DIRECT_ERROR: &str = "Cue cannot be constructed directly";

create_exception!(troupe, CueContextError, PyRuntimeError);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[pyclass(name = "Cue", module = "troupe")]
pub struct Cue {
    id: Mutex<Option<Py<PyString>>>,
    instruction: Mutex<Option<Py<PyAny>>>,
    source: Mutex<Option<Py<PyString>>>,
}

impl Cue {
    pub(crate) fn new_runtime(
        id: Py<PyString>,
        instruction: Py<PyAny>,
        source: Py<PyString>,
    ) -> Self {
        Self {
            id: Mutex::new(Some(id)),
            instruction: Mutex::new(Some(instruction)),
            source: Mutex::new(Some(source)),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_runtime_for_test(py: Python<'_>) -> Self {
        Self {
            id: Mutex::new(Some(PyString::new(py, "scene-test-cue0").unbind())),
            instruction: Mutex::new(Some(py.None())),
            source: Mutex::new(Some(PyString::new(py, "scene-test").unbind())),
        }
    }

    fn get<T>(value: &Mutex<Option<Py<T>>>, py: Python<'_>) -> PyResult<Py<T>>
    where
        T: PyTypeCheck,
    {
        lock(value)
            .as_ref()
            .map(|value| value.clone_ref(py))
            .ok_or_else(|| PyRuntimeError::new_err("Cue is no longer attached"))
    }
}

#[pymethods]
impl Cue {
    #[new]
    fn new() -> PyResult<Self> {
        Err(PyTypeError::new_err(CUE_DIRECT_ERROR))
    }

    #[getter]
    fn id(&self, py: Python<'_>) -> PyResult<Py<PyString>> {
        Self::get(&self.id, py)
    }

    #[getter]
    fn instruction(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        Self::get(&self.instruction, py)
    }

    #[getter]
    fn source(&self, py: Python<'_>) -> PyResult<Py<PyString>> {
        Self::get(&self.source, py)
    }

    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&*lock(&self.id))?;
        visit.call(&*lock(&self.instruction))?;
        visit.call(&*lock(&self.source))
    }

    fn __clear__(&self) {
        let id = lock(&self.id).take();
        let instruction = lock(&self.instruction).take();
        let source = lock(&self.source).take();
        drop((id, instruction, source));
    }
}
