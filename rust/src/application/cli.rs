use std::{sync::Arc, time::Duration};

use clap::error::ErrorKind;
use pyo3::exceptions::{PyBaseException, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::types::{PyAnyMethods, PyList, PyListMethods, PyString};
use tokio_util::sync::CancellationToken;

use crate::application::diagnostic_cli::{
    DiagnosticCommand, DiagnosticOutput, RuntimeDiagnosticArgs, dispatch,
};
use crate::application::diagnostics::{
    format_lifecycle_failure, format_load_failure, write_stderr,
};
use crate::application::failure::ProductionFailed;
use crate::application::invocation::{InvocationError, ParsedInvocation, parse_arguments};
use crate::application::loader::ProductionLoadError;
use crate::application::signals::SignalGuard;
use crate::diagnostic_runtime::{activation, runtime_producer, supervisor};
use crate::orchestration::production::Production;
use crate::orchestration::python_task::create_run_binding;
use crate::orchestration::runtime::{RuntimeCore, run_lifecycle};

fn write_stream(py: Python<'_>, name: &str, text: &str) -> PyResult<()> {
    let stream = py.import("sys")?.getattr(name)?;
    stream.call_method1("write", (text,))?;
    stream.call_method0("flush")?;
    Ok(())
}

enum Invocation {
    Production {
        path: Py<PyString>,
        diagnostics: RuntimeDiagnosticArgs,
        production_args: Py<PyList>,
    },
    Diagnostic(DiagnosticCommand),
    Exit(i32),
}

fn invocation(py: Python<'_>) -> PyResult<Invocation> {
    let argv = py.import("sys")?.getattr("argv")?.cast_into::<PyList>()?;
    let arguments = argv.get_slice(1, argv.len());
    match parse_arguments(py, &arguments) {
        Ok(ParsedInvocation::Production {
            path,
            diagnostics,
            production_args,
        }) => Ok(Invocation::Production {
            path: path.unbind(),
            diagnostics,
            production_args: production_args.unbind(),
        }),
        Ok(ParsedInvocation::Diagnostic(command)) => Ok(Invocation::Diagnostic(command)),
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

#[pyclass(module = "troupe._runtime")]
struct DiagnosticShutdownSignalHandler {
    cancellation: CancellationToken,
}

#[pymethods]
impl DiagnosticShutdownSignalHandler {
    fn __call__(&self, _signum: &Bound<'_, PyAny>, _frame: &Bound<'_, PyAny>) {
        self.cancellation.cancel();
    }
}

struct DiagnosticSignalGuard {
    signal_module: Py<PyAny>,
    originals: Vec<(Py<PyAny>, Py<PyAny>)>,
}

impl DiagnosticSignalGuard {
    fn install(py: Python<'_>, cancellation: CancellationToken) -> PyResult<Self> {
        let signal_module = py.import("signal")?;
        let handler = Py::new(py, DiagnosticShutdownSignalHandler { cancellation })?;
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

    fn restore(mut self, py: Python<'_>) -> PyResult<()> {
        self.restore_all(py)
    }
}

impl Drop for DiagnosticSignalGuard {
    fn drop(&mut self) {
        if !self.originals.is_empty() {
            Python::attach(|py| self.restore_best_effort(py));
        }
    }
}

struct PythonDiagnosticOutput {
    stdout: Py<PyAny>,
    stderr: Py<PyAny>,
}

impl PythonDiagnosticOutput {
    fn new(py: Python<'_>) -> PyResult<Self> {
        let sys = py.import("sys")?;
        Ok(Self {
            stdout: sys.getattr("stdout")?.unbind(),
            stderr: sys.getattr("stderr")?.unbind(),
        })
    }

    fn write(stream: &Py<PyAny>, text: &str) -> PyResult<()> {
        Python::attach(|py| {
            let stream = stream.bind(py);
            stream.call_method1("write", (text,))?;
            stream.call_method0("flush")?;
            Ok(())
        })
    }
}

impl DiagnosticOutput for PythonDiagnosticOutput {
    type Error = PyErr;

    fn write_stdout(&mut self, text: &str) -> Result<(), Self::Error> {
        Self::write(&self.stdout, text)
    }

    fn write_stderr(&mut self, text: &str) -> Result<(), Self::Error> {
        Self::write(&self.stderr, text)
    }
}

async fn drive_diagnostic(
    command: DiagnosticCommand,
    mut output: PythonDiagnosticOutput,
    cancellation: CancellationToken,
) -> PyResult<Result<dispatch::DiagnosticTermination, dispatch::DiagnosticDispatchError>> {
    let operation = dispatch::execute(command, &mut output, cancellation.clone());
    tokio::pin!(operation);
    let mut signal_poll = tokio::time::interval(Duration::from_millis(25));
    signal_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            result = &mut operation => return Ok(result),
            _ = signal_poll.tick() => {
                if let Err(error) = Python::attach(|py| py.check_signals()) {
                    cancellation.cancel();
                    let _ = operation.await;
                    return Err(error);
                }
            }
        }
    }
}

fn run_diagnostic(py: Python<'_>, command: DiagnosticCommand) -> PyResult<i32> {
    let cancellation = CancellationToken::new();
    let guard = DiagnosticSignalGuard::install(py, cancellation.clone())?;
    let output = PythonDiagnosticOutput::new(py)?;
    let command_result = py.detach(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                PyRuntimeError::new_err(format!(
                    "cannot create diagnostic command runtime: {error}"
                ))
            })?;
        runtime.block_on(drive_diagnostic(command, output, cancellation))
    });
    let restore_result = guard.restore(py);

    match (command_result, restore_result) {
        (Ok(Ok(termination)), Ok(())) => Ok(i32::from(termination.exit_code())),
        (Ok(Ok(_)), Err(error)) => Err(error),
        (Ok(Err(error)), _) => {
            write_stream(py, "stderr", &error.line())?;
            Ok(1)
        }
        (Err(error), _) => Err(error),
    }
}

#[pyfunction]
pub fn main(py: Python<'_>) -> PyResult<i32> {
    let (path, diagnostics, production_args) = match invocation(py)? {
        Invocation::Production {
            path,
            diagnostics,
            production_args,
        } => (path, diagnostics, production_args),
        Invocation::Diagnostic(command) => return run_diagnostic(py, command),
        Invocation::Exit(code) => return Ok(code),
    };

    let activated =
        match activation::activate(py, path.bind(py), production_args.bind(py), diagnostics) {
            Ok(activated) => activated,
            Err(error) => match error.into_python() {
                Ok(error) if error.is_instance_of::<ProductionLoadError>(py) => {
                    let value = error.value(py).cast::<PyBaseException>()?;
                    let rendered = format_load_failure(py, value)?;
                    write_stderr(py, &rendered)?;
                    return Ok(1);
                }
                Ok(error) => return Err(error),
                Err(error) => {
                    write_stream(py, "stderr", &error.line())?;
                    return Ok(1);
                }
            },
        };
    let (production, diagnostic_runtime) = activated.into_parts();

    let core = Arc::new(RuntimeCore::new());
    let guard = SignalGuard::install(py, Arc::clone(&core))?;
    let permit = core
        .begin()
        .expect("a new CLI runtime must accept its first run");
    let production_state = production.bind(py).cast::<Production>()?.borrow().state();
    let diagnostic_probe_runtime = diagnostic_runtime.clone();
    let probe = diagnostic_probe_runtime.failure_probe();
    let producer_binding = probe.clone();
    let operation_core = Arc::clone(&core);
    let run_result = pyo3_async_runtimes::tokio::run(py, async move {
        let operation = async move {
            let locals = Python::attach(pyo3_async_runtimes::tokio::get_current_locals)?;
            let binding = create_run_binding(&locals, &production).await?;
            let producer = Python::attach(|py| {
                let production_state = production.bind(py).cast::<Production>()?.borrow().state();
                activation::bind_run(py, &operation_core, &production_state, &binding)
            })?
            .ok_or_else(|| {
                PyRuntimeError::new_err(
                    "mandatory Production diagnostics were not bound to the Runtime",
                )
            })?;
            producer_binding
                .bind_producer(producer)
                .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
            runtime_producer::run_started(&operation_core, &binding);
            let result = run_lifecycle(permit, locals, production, binding).await;
            production_state.shutdown_agent_sessions().await;
            result
        };
        Ok(supervisor::supervise(probe, core, operation).await)
    });
    let restore_result = guard.restore(py);
    let diagnostic_shutdown = diagnostic_runtime.shutdown();

    let supervised = run_result?;
    let (run_result, infrastructure_failure) = supervised.into_parts();
    if let Some(failure) = infrastructure_failure {
        write_stream(py, "stderr", &failure.line())?;
        if let Err(error) = diagnostic_shutdown {
            write_stream(py, "stderr", &error.line())?;
        }
        return Ok(1);
    }

    match (run_result, restore_result, diagnostic_shutdown) {
        (Err(error), _, shutdown) if error.is_instance_of::<ProductionFailed>(py) => {
            let value = error.value(py).cast::<PyBaseException>()?;
            let rendered = format_lifecycle_failure(py, value)?;
            write_stderr(py, &rendered)?;
            if let Err(error) = shutdown {
                write_stream(py, "stderr", &error.line())?;
            }
            Ok(1)
        }
        (Err(error), _, _) => Err(error),
        (Ok(()), Err(error), _) => Err(error),
        (Ok(()), Ok(()), Err(error)) => {
            write_stream(py, "stderr", &error.line())?;
            Ok(1)
        }
        (Ok(()), Ok(()), Ok(())) => Ok(0),
    }
}
