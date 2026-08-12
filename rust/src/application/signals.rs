use std::sync::Arc;

use pyo3::prelude::*;
use pyo3::types::PyAnyMethods;

use crate::orchestration::runtime::RuntimeCore;

#[pyclass(module = "troupe._runtime")]
struct ShutdownSignalHandler {
    core: Arc<RuntimeCore>,
}

#[pymethods]
impl ShutdownSignalHandler {
    fn __call__(&self, _signum: &Bound<'_, PyAny>, _frame: &Bound<'_, PyAny>) {
        self.core.request_shutdown();
    }
}

pub(crate) struct SignalGuard {
    signal_module: Py<PyAny>,
    originals: Vec<(Py<PyAny>, Py<PyAny>)>,
}

impl SignalGuard {
    pub(crate) fn install(py: Python<'_>, core: Arc<RuntimeCore>) -> PyResult<Self> {
        let signal_module = py.import("signal")?;
        let handler = Py::new(py, ShutdownSignalHandler { core })?;
        let mut guard = Self {
            signal_module: signal_module.clone().into_any().unbind(),
            originals: Vec::with_capacity(2),
        };

        for name in ["SIGINT", "SIGTERM"] {
            let signum = match signal_module.getattr(name) {
                Ok(signum) => signum,
                Err(error) => {
                    guard.restore_best_effort(py);
                    return Err(error);
                }
            };
            match signal_module.call_method1("signal", (&signum, handler.bind(py))) {
                Ok(original) => guard.originals.push((signum.unbind(), original.unbind())),
                Err(error) => {
                    guard.restore_best_effort(py);
                    return Err(error);
                }
            }
        }

        Ok(guard)
    }

    fn restore_all(&mut self, py: Python<'_>) -> PyResult<()> {
        let signal_module = self.signal_module.bind(py);
        let mut first_error = None;
        while let Some((signum, original)) = self.originals.pop() {
            if let Err(error) =
                signal_module.call_method1("signal", (signum.bind(py), original.bind(py)))
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn restore_best_effort(&mut self, py: Python<'_>) {
        let _ = self.restore_all(py);
    }

    pub(crate) fn restore(mut self, py: Python<'_>) -> PyResult<()> {
        self.restore_all(py)
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        if !self.originals.is_empty() {
            Python::attach(|py| self.restore_best_effort(py));
        }
    }
}
