use std::sync::Arc;

use clap::error::ErrorKind;
use pyo3::exceptions::PyBaseException;
use pyo3::prelude::*;
use pyo3::types::{PyAnyMethods, PyList, PyListMethods, PyString};

use crate::diagnostics::{format_lifecycle_failure, format_load_failure, write_stderr};
use crate::failure::ProductionFailed;
use crate::invocation::{InvocationError, parse_arguments};
use crate::loader::{ProductionLoadError, load_production};
use crate::production::Production;
use crate::python_task::create_run_binding;
use crate::runtime::{RuntimeCore, run_lifecycle};
use crate::signals::SignalGuard;

fn write_stream(py: Python<'_>, name: &str, text: &str) -> PyResult<()> {
    let stream = py.import("sys")?.getattr(name)?;
    stream.call_method1("write", (text,))?;
    stream.call_method0("flush")?;
    Ok(())
}

enum Invocation {
    Run(Py<PyString>, Py<PyList>),
    Exit(i32),
}

fn invocation(py: Python<'_>) -> PyResult<Invocation> {
    let argv = py.import("sys")?.getattr("argv")?.cast_into::<PyList>()?;
    let arguments = argv.get_slice(1, argv.len());
    match parse_arguments(py, &arguments) {
        Ok((path, production_args)) => Ok(Invocation::Run(path.unbind(), production_args.unbind())),
        Err(InvocationError::Python(error)) => Err(error),
        Err(InvocationError::Clap(error)) => {
            let help = matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            );
            write_stream(
                py,
                if help { "stdout" } else { "stderr" },
                &error.to_string(),
            )?;
            Ok(Invocation::Exit(if help { 0 } else { 2 }))
        }
    }
}

#[pyfunction]
pub fn main(py: Python<'_>) -> PyResult<i32> {
    let (path, production_args) = match invocation(py)? {
        Invocation::Run(path, production_args) => (path, production_args),
        Invocation::Exit(code) => return Ok(code),
    };

    let production = match load_production(py, path.bind(py), production_args.bind(py)) {
        Ok(production) => production,
        Err(error) if error.is_instance_of::<ProductionLoadError>(py) => {
            let value = error.value(py).cast::<PyBaseException>()?;
            let rendered = format_load_failure(py, value)?;
            write_stderr(py, &rendered)?;
            return Ok(1);
        }
        Err(error) => return Err(error),
    };

    let core = Arc::new(RuntimeCore::new());
    let guard = SignalGuard::install(py, Arc::clone(&core))?;
    let permit = core
        .begin()
        .expect("a new CLI runtime must accept its first run");
    let production_state = production.bind(py).cast::<Production>()?.borrow().state();
    let run_result = pyo3_async_runtimes::tokio::run(py, async move {
        let locals = Python::attach(pyo3_async_runtimes::tokio::get_current_locals)?;
        let result = async {
            let binding = create_run_binding(&locals, &production).await?;
            run_lifecycle(permit, locals, production, binding).await
        }
        .await;
        production_state.shutdown_agent_sessions().await;
        result
    });
    let restore_result = guard.restore(py);

    match (run_result, restore_result) {
        (Err(error), _) if error.is_instance_of::<ProductionFailed>(py) => {
            let value = error.value(py).cast::<PyBaseException>()?;
            let rendered = format_lifecycle_failure(py, value)?;
            write_stderr(py, &rendered)?;
            Ok(1)
        }
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(0),
    }
}
