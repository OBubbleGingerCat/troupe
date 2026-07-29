use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3_async_runtimes::TaskLocals;
use tokio_util::sync::CancellationToken;

use crate::failure::lifecycle_result;
use crate::python_task::{await_hook, create_scene_task};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum RunState {
    New = 0,
    Running = 1,
    Finished = 2,
}

pub(crate) struct RuntimeCore {
    state: AtomicU8,
    shutdown: CancellationToken,
}

impl RuntimeCore {
    pub(crate) fn new() -> Self {
        Self {
            state: AtomicU8::new(RunState::New as u8),
            shutdown: CancellationToken::new(),
        }
    }

    pub(crate) fn begin(self: &Arc<Self>) -> Result<RunPermit, AlreadyRun> {
        self.state
            .compare_exchange(
                RunState::New as u8,
                RunState::Running as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| RunPermit {
                core: Arc::clone(self),
            })
            .map_err(|_| AlreadyRun)
    }

    pub(crate) fn request_shutdown(&self) {
        self.shutdown.cancel();
    }

    fn shutdown_requested(&self) -> bool {
        self.shutdown.is_cancelled()
    }

    #[cfg(test)]
    fn state(&self) -> RunState {
        match self.state.load(Ordering::Acquire) {
            value if value == RunState::New as u8 => RunState::New,
            value if value == RunState::Running as u8 => RunState::Running,
            value if value == RunState::Finished as u8 => RunState::Finished,
            _ => unreachable!("runtime state must be valid"),
        }
    }
}

#[derive(Debug)]
pub(crate) struct AlreadyRun;

pub(crate) struct RunPermit {
    core: Arc<RuntimeCore>,
}

enum SceneFailure {
    Completion(PyErr),
    CancellationDispatch(PyErr),
}

impl Drop for RunPermit {
    fn drop(&mut self) {
        self.core
            .state
            .store(RunState::Finished as u8, Ordering::Release);
    }
}

async fn await_after_cancel<F>(
    cancel_result: PyResult<()>,
    completion: Pin<&mut F>,
) -> Result<(), SceneFailure>
where
    F: Future<Output = PyResult<()>>,
{
    let completion_result = completion.await;
    match completion_result {
        Err(error) if !is_cancelled_error(&error) => Err(SceneFailure::Completion(error)),
        completion_result => match cancel_result {
            Ok(()) => completion_result.map_err(SceneFailure::Completion),
            Err(error) => Err(SceneFailure::CancellationDispatch(error)),
        },
    }
}

pub(crate) async fn run_lifecycle(
    permit: RunPermit,
    locals: TaskLocals,
    production: Py<PyAny>,
) -> PyResult<()> {
    let mut failures = Vec::with_capacity(2);
    if let Err(error) = await_hook(&locals, &production, "start").await {
        failures.push(("start", error));
        return lifecycle_result(failures);
    }

    while !permit.core.shutdown_requested() {
        let scene_result = async {
            let task = create_scene_task(&locals, &production)
                .await
                .map_err(SceneFailure::Completion)?;
            let completion = task.wait(&locals);
            tokio::pin!(completion);
            tokio::select! {
                result = &mut completion => result.map_err(SceneFailure::Completion),
                _ = permit.core.shutdown.cancelled() => {
                    let cancel_result = task.cancel(&locals).await;
                    await_after_cancel(cancel_result, completion.as_mut()).await
                }
            }
        }
        .await;
        match scene_result {
            Ok(()) => {}
            Err(SceneFailure::Completion(error)) => {
                if !is_cancelled_error(&error) {
                    failures.push(("scene", error));
                }
                break;
            }
            Err(SceneFailure::CancellationDispatch(error)) => {
                failures.push(("scene", error));
                break;
            }
        }
    }

    if let Err(error) = await_hook(&locals, &production, "stop").await {
        failures.push(("stop", error));
    }
    lifecycle_result(failures)
}

fn is_cancelled_error(error: &PyErr) -> bool {
    Python::attach(|py| {
        py.import("asyncio")
            .and_then(|asyncio| asyncio.getattr("CancelledError"))
            .is_ok_and(|cancelled| error.is_instance(py, &cancelled))
    })
}

#[pyclass(name = "_Runtime", module = "troupe._runtime")]
pub struct Runtime {
    core: Arc<RuntimeCore>,
}

#[pymethods]
impl Runtime {
    #[new]
    fn new() -> Self {
        Self {
            core: Arc::new(RuntimeCore::new()),
        }
    }

    fn request_shutdown(&self) {
        self.core.request_shutdown();
    }

    fn run<'py>(&self, py: Python<'py>, production: Py<PyAny>) -> PyResult<Bound<'py, PyAny>> {
        let permit = self
            .core
            .begin()
            .map_err(|_| PyRuntimeError::new_err("Runtime.run() may only be called once"))?;
        let locals = TaskLocals::with_running_loop(py)?.copy_context(py)?;
        let lifecycle_locals = locals.clone();

        pyo3_async_runtimes::tokio::future_into_py_with_locals(py, locals, async move {
            run_lifecycle(permit, lifecycle_locals, production).await?;
            Python::attach(|py| Ok::<_, PyErr>(py.None()))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use pyo3::PyErr;
    use pyo3::exceptions::PyRuntimeError;
    use tokio::sync::oneshot;

    use super::{RunState, RuntimeCore, SceneFailure, await_after_cancel};

    #[tokio::test]
    async fn cancel_failure_waits_for_completion_and_keeps_its_origin() {
        let (sender, receiver) = oneshot::channel();
        let completion = async {
            receiver.await.expect("test must release scene completion");
            Ok::<(), PyErr>(())
        };
        tokio::pin!(completion);
        let resolution = await_after_cancel(
            Err(PyRuntimeError::new_err("cancel dispatch failed")),
            completion.as_mut(),
        );
        tokio::pin!(resolution);

        tokio::select! {
            _ = &mut resolution => panic!("cancel failure returned before scene completion"),
            _ = tokio::task::yield_now() => {}
        }

        sender.send(()).expect("scene release receiver must exist");
        assert!(matches!(
            resolution.await,
            Err(SceneFailure::CancellationDispatch(_))
        ));
    }

    #[tokio::test]
    async fn scene_failure_precedes_cancel_failure_after_completion() {
        let completion = async { Err::<(), PyErr>(PyRuntimeError::new_err("scene failed")) };
        tokio::pin!(completion);

        assert!(matches!(
            await_after_cancel(
                Err(PyRuntimeError::new_err("cancel dispatch failed")),
                completion.as_mut(),
            )
            .await,
            Err(SceneFailure::Completion(_))
        ));
    }

    #[test]
    fn run_permit_transitions_new_to_running_to_finished() {
        let core = Arc::new(RuntimeCore::new());
        assert_eq!(core.state(), RunState::New);

        let permit = core.begin().expect("the first run must start");
        assert_eq!(core.state(), RunState::Running);

        drop(permit);
        assert_eq!(core.state(), RunState::Finished);
    }

    #[test]
    fn begin_rejects_a_running_core() {
        let core = Arc::new(RuntimeCore::new());
        let permit = core.begin().expect("the first run must start");

        assert!(core.begin().is_err());
        assert_eq!(core.state(), RunState::Running);

        drop(permit);
    }

    #[test]
    fn begin_rejects_a_finished_core() {
        let core = Arc::new(RuntimeCore::new());
        drop(core.begin().expect("the first run must start"));

        assert!(core.begin().is_err());
        assert_eq!(core.state(), RunState::Finished);
    }

    #[test]
    fn concurrent_begin_has_exactly_one_winner() {
        const THREADS: usize = 16;

        let core = Arc::new(RuntimeCore::new());
        let barrier = Arc::new(Barrier::new(THREADS));
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let core = Arc::clone(&core);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    core.begin().is_ok()
                })
            })
            .collect();

        let winners = handles
            .into_iter()
            .map(|handle| handle.join().expect("worker thread must not panic"))
            .filter(|won| *won)
            .count();

        assert_eq!(winners, 1);
        assert_eq!(core.state(), RunState::Finished);
    }

    #[test]
    fn shutdown_is_durable_and_idempotent() {
        let core = RuntimeCore::new();
        assert!(!core.shutdown_requested());

        core.request_shutdown();
        assert!(core.shutdown_requested());

        core.request_shutdown();
        assert!(core.shutdown_requested());
    }
}
