use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyAnyMethods, PyDict, PyDictMethods};
use pyo3_async_runtimes::TaskLocals;
use tokio::sync::oneshot;

#[derive(Clone, Copy)]
enum HookMode {
    Awaitable,
    SceneTask,
}

#[pyclass]
struct HookCallback {
    production: Py<PyAny>,
    hook: &'static str,
    mode: HookMode,
    sender: Option<oneshot::Sender<PyResult<Py<PyAny>>>>,
}

#[pyclass]
struct TaskCancelCallback {
    task: Py<PyAny>,
    sender: Option<oneshot::Sender<PyResult<()>>>,
}

impl TaskCancelCallback {
    fn new(task: Py<PyAny>, sender: oneshot::Sender<PyResult<()>>) -> Self {
        Self {
            task,
            sender: Some(sender),
        }
    }

    fn invoke(&mut self, py: Python<'_>) -> PyResult<()> {
        let Some(sender) = self.sender.take() else {
            return Ok(());
        };
        let result = self.task.bind(py).call_method0("cancel").map(|_| ());
        let _ = sender.send(result);
        Ok(())
    }
}

impl HookCallback {
    fn new(
        production: Py<PyAny>,
        hook: &'static str,
        mode: HookMode,
        sender: oneshot::Sender<PyResult<Py<PyAny>>>,
    ) -> Self {
        Self {
            production,
            hook,
            mode,
            sender: Some(sender),
        }
    }

    fn invoke(&mut self, py: Python<'_>) -> PyResult<()> {
        let Some(sender) = self.sender.take() else {
            return Ok(());
        };
        let result = (|| {
            let awaitable = self.production.bind(py).getattr(self.hook)?.call0()?;
            match self.mode {
                HookMode::Awaitable => Ok(awaitable.unbind()),
                HookMode::SceneTask => Ok(py
                    .import("asyncio")?
                    .call_method1("create_task", (awaitable,))?
                    .unbind()),
            }
        })();
        let _ = sender.send(result);
        Ok(())
    }
}

#[pymethods]
impl TaskCancelCallback {
    fn __call__(&mut self, py: Python<'_>) -> PyResult<()> {
        self.invoke(py)
    }
}

#[pymethods]
impl HookCallback {
    fn __call__(&mut self, py: Python<'_>) -> PyResult<()> {
        self.invoke(py)
    }
}

async fn dispatch_hook(
    locals: &TaskLocals,
    production: &Py<PyAny>,
    hook: &'static str,
    mode: HookMode,
) -> PyResult<Py<PyAny>> {
    let receiver = Python::attach(|py| {
        let (sender, receiver) = oneshot::channel();
        let callback = Py::new(
            py,
            HookCallback::new(production.clone_ref(py), hook, mode, sender),
        )?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("context", locals.context(py))?;
        locals
            .event_loop(py)
            .call_method("call_soon_threadsafe", (callback,), Some(&kwargs))?;
        Ok::<_, PyErr>(receiver)
    })?;

    match receiver.await {
        Ok(result) => result,
        Err(_) => Err(PyRuntimeError::new_err(
            "Python hook callback did not return a result",
        )),
    }
}

pub(crate) async fn await_hook(
    locals: &TaskLocals,
    production: &Py<PyAny>,
    hook: &'static str,
) -> PyResult<()> {
    let awaitable = dispatch_hook(locals, production, hook, HookMode::Awaitable).await?;
    let future = Python::attach(|py| {
        pyo3_async_runtimes::into_future_with_locals(locals, awaitable.into_bound(py))
    })?;
    future.await?;
    Ok(())
}

pub(crate) async fn create_scene_task(
    locals: &TaskLocals,
    production: &Py<PyAny>,
) -> PyResult<PythonTask> {
    let task = dispatch_hook(locals, production, "scene", HookMode::SceneTask).await?;
    Ok(PythonTask { task })
}

pub(crate) struct PythonTask {
    task: Py<PyAny>,
}

impl PythonTask {
    pub(crate) async fn cancel(&self, locals: &TaskLocals) -> PyResult<()> {
        let receiver = Python::attach(|py| {
            let (sender, receiver) = oneshot::channel();
            let callback = Py::new(py, TaskCancelCallback::new(self.task.clone_ref(py), sender))?;
            let kwargs = PyDict::new(py);
            kwargs.set_item("context", locals.context(py))?;
            locals.event_loop(py).call_method(
                "call_soon_threadsafe",
                (callback,),
                Some(&kwargs),
            )?;
            Ok::<_, PyErr>(receiver)
        })?;

        match receiver.await {
            Ok(result) => result,
            Err(_) => Err(PyRuntimeError::new_err(
                "Python task cancellation callback did not return a result",
            )),
        }
    }

    pub(crate) async fn wait(&self, locals: &TaskLocals) -> PyResult<()> {
        let future = Python::attach(|py| {
            pyo3_async_runtimes::into_future_with_locals(
                locals,
                self.task.clone_ref(py).into_bound(py),
            )
        })?;
        future.await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;

    use pyo3::prelude::*;
    use pyo3::types::{PyAnyMethods, PyModule};
    use tokio::sync::oneshot;

    use super::{HookCallback, HookMode};

    struct AttributeRestore {
        module: Py<PyModule>,
        name: &'static str,
        value: Py<PyAny>,
    }

    impl Drop for AttributeRestore {
        fn drop(&mut self) {
            Python::attach(|py| {
                let _ = self.module.bind(py).setattr(self.name, self.value.bind(py));
            });
        }
    }

    fn assert_transported_identity(
        py: Python<'_>,
        production: Py<PyAny>,
        hook: &'static str,
        mode: HookMode,
        expected: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let (sender, mut receiver) = oneshot::channel();
        let callback = Py::new(py, HookCallback::new(production, hook, mode, sender))?;

        let returned = callback.bind(py).call0()?;
        assert!(returned.is_none());

        let transported = receiver
            .try_recv()
            .expect("callback must send exactly one result");
        let error = match transported {
            Ok(_) => panic!("the callback unexpectedly returned an awaitable"),
            Err(error) => error,
        };
        assert!(error.value(py).is(expected));
        Ok(())
    }

    #[test]
    fn callback_transports_each_synchronous_error_without_raising() {
        Python::initialize();
        Python::attach(|py| {
            let source = CString::new(
                "lookup_error = ValueError('lookup boom')\n".to_owned()
                    + "call_error = ValueError('call boom')\n"
                    + "create_task_error = ValueError('create-task boom')\n"
                    + "class LookupFailure:\n"
                    + "    @property\n"
                    + "    def start(self):\n"
                    + "        raise lookup_error\n"
                    + "class CallFailure:\n"
                    + "    def start(self):\n"
                    + "        raise call_error\n"
                    + "class CreateTaskFailure:\n"
                    + "    def scene(self):\n"
                    + "        return object()\n"
                    + "def fail_create_task(awaitable):\n"
                    + "    raise create_task_error\n",
            )
            .expect("test source has no nul byte");
            let module = PyModule::from_code(
                py,
                source.as_c_str(),
                c"python_task_test.py",
                c"python_task_test",
            )?;

            assert_transported_identity(
                py,
                module.getattr("LookupFailure")?.call0()?.unbind(),
                "start",
                HookMode::Awaitable,
                &module.getattr("lookup_error")?,
            )?;
            assert_transported_identity(
                py,
                module.getattr("CallFailure")?.call0()?.unbind(),
                "start",
                HookMode::Awaitable,
                &module.getattr("call_error")?,
            )?;

            let asyncio = py.import("asyncio")?;
            let restore_create_task = AttributeRestore {
                module: asyncio.clone().unbind(),
                name: "create_task",
                value: asyncio.getattr("create_task")?.unbind(),
            };
            asyncio.setattr("create_task", module.getattr("fail_create_task")?)?;
            assert_transported_identity(
                py,
                module.getattr("CreateTaskFailure")?.call0()?.unbind(),
                "scene",
                HookMode::SceneTask,
                &module.getattr("create_task_error")?,
            )?;
            drop(restore_create_task);

            Ok::<(), PyErr>(())
        })
        .expect("embedded Python test must succeed");
    }
}
