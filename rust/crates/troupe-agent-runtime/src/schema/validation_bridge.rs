use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};

use pyo3::exceptions::{PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use tokio::sync::{Mutex as AsyncMutex, MutexGuard as AsyncMutexGuard, oneshot};
use tokio_util::sync::CancellationToken;

use crate::schema::{
    CompiledActSchema, CustomValidationJob, defensive_python_copy, schema_callback_error,
};

const VALUE_REJECTED_MAX_BYTES: usize = 4 * 1024;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug)]
struct ValidationCloseSignal {
    token: CancellationToken,
    dispatch_closed: Mutex<bool>,
}

impl ValidationCloseSignal {
    fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            dispatch_closed: Mutex::new(false),
        }
    }

    fn is_closed(&self) -> bool {
        *lock(&self.dispatch_closed)
    }

    fn begin_dispatch(&self) -> Option<MutexGuard<'_, bool>> {
        let state = lock(&self.dispatch_closed);
        (!*state).then_some(state)
    }

    fn close(&self) {
        let mut state = lock(&self.dispatch_closed);
        if *state {
            return;
        }
        *state = true;
        self.token.cancel();
    }

    async fn wait<F>(&self, future: F) -> Option<F::Output>
    where
        F: Future,
    {
        tokio::select! {
            biased;
            () = self.token.cancelled() => None,
            output = future => Some(output),
        }
    }
}

#[derive(Debug)]
pub struct PythonSchemaValidationBridge {
    worker: Py<PyAny>,
    serial: AsyncMutex<()>,
    closed: ValidationCloseSignal,
}

#[derive(Debug)]
pub(crate) enum CustomValidationOutcome {
    Accepted,
    Rejected {
        path: String,
        message: String,
        truncated: bool,
    },
    CallbackFailed(PyErr),
    Closed,
}

#[pyclass]
struct ValidationCompletion {
    sender: Mutex<Option<oneshot::Sender<PyResult<Py<PyAny>>>>>,
}

impl PythonSchemaValidationBridge {
    pub fn new(py: Python<'_>) -> PyResult<Arc<Self>> {
        let worker = py
            .import("troupe.act_schema")?
            .getattr("_PythonValidationBridge")?
            .call0()?
            .unbind();
        Ok(Arc::new(Self {
            worker,
            serial: AsyncMutex::new(()),
            closed: ValidationCloseSignal::new(),
        }))
    }

    pub(crate) async fn acquire(&self) -> Option<AsyncMutexGuard<'_, ()>> {
        let guard = self.closed.wait(self.serial.lock()).await?;
        (!self.closed.is_closed()).then_some(guard)
    }

    pub(crate) async fn validate_jobs(
        &self,
        schema: &CompiledActSchema,
        jobs: &[CustomValidationJob],
    ) -> CustomValidationOutcome {
        for job in jobs {
            if self.closed.is_closed() {
                return CustomValidationOutcome::Closed;
            }
            let receiver = match Python::attach(|py| self.dispatch(py, schema, job)) {
                Ok(Some(receiver)) => receiver,
                Ok(None) => return CustomValidationOutcome::Closed,
                Err(cause) => {
                    return CustomValidationOutcome::CallbackFailed(Python::attach(|py| {
                        schema_callback_error(py, "validate", &job.path, cause)
                    }));
                }
            };
            let Some(completion) = self.closed.wait(receiver).await else {
                return CustomValidationOutcome::Closed;
            };
            let result = match completion {
                Ok(result) => result,
                Err(_) => {
                    let cause = PyRuntimeError::new_err("schema validation callback was lost");
                    return CustomValidationOutcome::CallbackFailed(Python::attach(|py| {
                        schema_callback_error(py, "validate", &job.path, cause)
                    }));
                }
            };
            let outcome = Python::attach(|py| classify_result(py, &job.path, result));
            if !matches!(outcome, CustomValidationOutcome::Accepted) {
                return outcome;
            }
            if self.closed.is_closed() {
                return CustomValidationOutcome::Closed;
            }
        }
        CustomValidationOutcome::Accepted
    }

    fn dispatch(
        &self,
        py: Python<'_>,
        schema: &CompiledActSchema,
        job: &CustomValidationJob,
    ) -> PyResult<Option<oneshot::Receiver<PyResult<Py<PyAny>>>>> {
        let Some(_dispatch) = self.closed.begin_dispatch() else {
            return Ok(None);
        };
        let validator = schema
            .custom_validator(job.validator_id)
            .ok_or_else(|| PyRuntimeError::new_err("custom validator binding is unavailable"))?;
        let value = defensive_python_copy(&job.value, py)?;
        let completion = self
            .worker
            .bind(py)
            .call_method1("submit", (validator.bind(py), value.bind(py)))?;
        let (sender, receiver) = oneshot::channel();
        let callback = Py::new(
            py,
            ValidationCompletion {
                sender: Mutex::new(Some(sender)),
            },
        )?;
        completion.call_method1("add_done_callback", (callback,))?;
        Ok(Some(receiver))
    }

    pub(crate) fn begin_close(&self) {
        self.closed.close();
    }

    pub(crate) fn close(&self) {
        self.begin_close();
        let _ = Python::try_attach(|py| self.worker.bind(py).call_method0("close").map(|_| ()));
    }
}

impl Drop for PythonSchemaValidationBridge {
    fn drop(&mut self) {
        self.close();
    }
}

fn classify_result(
    py: Python<'_>,
    path: &str,
    result: PyResult<Py<PyAny>>,
) -> CustomValidationOutcome {
    match result {
        Ok(value) if value.bind(py).is_none() => CustomValidationOutcome::Accepted,
        Ok(_) => CustomValidationOutcome::CallbackFailed(schema_callback_error(
            py,
            "validate",
            path,
            PyTypeError::new_err("custom validate() must return None"),
        )),
        Err(error) => {
            let rejected = py
                .import("troupe.act_schema")
                .and_then(|module| module.getattr("ValueRejected"))
                .is_ok_and(|class| error.is_instance(py, &class));
            if !rejected {
                return CustomValidationOutcome::CallbackFailed(schema_callback_error(
                    py, "validate", path, error,
                ));
            }
            let message = match error
                .value(py)
                .str()
                .and_then(|value| value.to_str().map(str::to_owned))
            {
                Ok(message) => message,
                Err(cause) => {
                    return CustomValidationOutcome::CallbackFailed(schema_callback_error(
                        py, "validate", path, cause,
                    ));
                }
            };
            let (message, truncated) = truncate_utf8(message, VALUE_REJECTED_MAX_BYTES);
            CustomValidationOutcome::Rejected {
                path: path.to_owned(),
                message,
                truncated,
            }
        }
    }
}

fn truncate_utf8(mut value: String, maximum: usize) -> (String, bool) {
    if value.len() <= maximum {
        return (value, false);
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    (value, true)
}

#[pymethods]
impl ValidationCompletion {
    fn __call__(&self, future: &Bound<'_, PyAny>) {
        let Some(sender) = lock(&self.sender).take() else {
            return;
        };
        let result = future.call_method0("result").map(Bound::unbind);
        let _ = sender.send(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_and_dispatch_share_one_linearization_gate() {
        let signal = Arc::new(ValidationCloseSignal::new());
        let dispatch = signal
            .begin_dispatch()
            .expect("an open bridge admits dispatch");
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let closer = std::thread::spawn({
            let signal = Arc::clone(&signal);
            move || {
                started_tx.send(()).unwrap();
                signal.close();
                finished_tx.send(()).unwrap();
            }
        });
        started_rx.recv().unwrap();
        assert!(finished_rx.try_recv().is_err());

        drop(dispatch);

        finished_rx.recv().unwrap();
        closer.join().unwrap();
        assert!(signal.begin_dispatch().is_none());
    }

    #[tokio::test]
    async fn close_signal_wakes_permit_and_callback_waiters() {
        let signal = Arc::new(ValidationCloseSignal::new());
        let serial = Arc::new(AsyncMutex::new(()));
        let held = Arc::clone(&serial).lock_owned().await;
        let permit_waiter = tokio::spawn({
            let signal = Arc::clone(&signal);
            let serial = Arc::clone(&serial);
            async move { signal.wait(serial.lock_owned()).await.is_none() }
        });
        let (_sender, receiver) = oneshot::channel::<()>();
        let callback_waiter = tokio::spawn({
            let signal = Arc::clone(&signal);
            async move { signal.wait(receiver).await.is_none() }
        });
        tokio::task::yield_now().await;

        signal.close();

        assert!(permit_waiter.await.unwrap());
        assert!(callback_waiter.await.unwrap());
        drop(held);
    }

    #[test]
    fn value_rejected_message_enforces_4095_4096_4097_encoded_bytes() {
        for size in [VALUE_REJECTED_MAX_BYTES - 1, VALUE_REJECTED_MAX_BYTES] {
            let original = "x".repeat(size);
            let (bounded, truncated) = truncate_utf8(original.clone(), VALUE_REJECTED_MAX_BYTES);
            assert_eq!(bounded, original);
            assert!(!truncated);
        }

        let (bounded, truncated) = truncate_utf8(
            "x".repeat(VALUE_REJECTED_MAX_BYTES + 1),
            VALUE_REJECTED_MAX_BYTES,
        );
        assert_eq!(bounded.len(), VALUE_REJECTED_MAX_BYTES);
        assert!(truncated);

        let original = format!("{}ab", "界".repeat(1_365));
        assert_eq!(original.len(), VALUE_REJECTED_MAX_BYTES + 1);
        let (bounded, truncated) = truncate_utf8(original, VALUE_REJECTED_MAX_BYTES);
        assert_eq!(bounded.len(), VALUE_REJECTED_MAX_BYTES);
        assert!(truncated);
    }
}
