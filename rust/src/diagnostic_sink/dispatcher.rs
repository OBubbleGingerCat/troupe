use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, OnceLock};
use std::thread::ThreadId;

use pyo3::prelude::*;
use pyo3::types::{PyAnyMethods, PyModule};
use troupe_diagnostics_core::event::DiagnosticEventKind;

use super::budget::RuntimeBudget;
use super::callback::{CallbackFailure, CallbackFailureKind, driver_source};
use super::queue::{
    AdmissionClass, AdmissionOutcome, CallbackTicket, DropDelta, QueueAccessError, QueueEvent,
    QueueSnapshot, SinkQueue,
};

const COMPLETE_RETRY: u8 = 0;
const COMPLETE_DONE: u8 = 1;
const COMPLETE_STOP: u8 = 2;

type PythonDispatch = (Py<PyAny>, Py<PyAny>, u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DispatcherIdentity {
    rust_thread_id: ThreadId,
    python_thread_id: u64,
    event_loop_id: u64,
    python_daemon: bool,
}

impl DispatcherIdentity {
    pub(crate) const fn rust_thread_id(self) -> ThreadId {
        self.rust_thread_id
    }

    pub(crate) const fn python_thread_id(self) -> u64 {
        self.python_thread_id
    }

    pub(crate) const fn event_loop_id(self) -> u64 {
        self.event_loop_id
    }

    pub(crate) const fn python_daemon(self) -> bool {
        self.python_daemon
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DispatcherThreadFailure {
    message: String,
}

impl DispatcherThreadFailure {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for DispatcherThreadFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DispatcherThreadFailure {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DeliveryProgress {
    delivered_events: usize,
    first_delivered_sequence: Option<u64>,
    last_delivered_sequence: Option<u64>,
}

impl DeliveryProgress {
    pub(crate) const fn delivered_events(self) -> usize {
        self.delivered_events
    }

    pub(crate) const fn first_delivered_sequence(self) -> Option<u64> {
        self.first_delivered_sequence
    }

    pub(crate) const fn last_delivered_sequence(self) -> Option<u64> {
        self.last_delivered_sequence
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UnexpectedDispatcherFailure {
    stage: &'static str,
    detail: String,
}

impl UnexpectedDispatcherFailure {
    pub(crate) const fn stage(&self) -> &'static str {
        self.stage
    }

    pub(crate) fn detail(&self) -> &str {
        &self.detail
    }
}

#[derive(Debug)]
pub(crate) struct DispatchEvent {
    sequence: u64,
    value: Py<PyAny>,
}

impl DispatchEvent {
    pub(crate) fn new(sequence: u64, value: Py<PyAny>) -> Self {
        assert!(
            sequence != 0,
            "diagnostic callback sequence must be nonzero"
        );
        Self { sequence, value }
    }

    pub(crate) const fn sequence(&self) -> u64 {
        self.sequence
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SinkDispatcher {
    inner: Arc<SinkDispatchState>,
}

impl SinkDispatcher {
    pub(crate) fn new(id: u64, runtime_budget: RuntimeBudget, callback: Py<PyAny>) -> Self {
        Self {
            inner: Arc::new(SinkDispatchState {
                id,
                callback,
                queue: SinkQueue::new(runtime_budget),
                callback_failure: OnceLock::new(),
                unexpected_failure: OnceLock::new(),
                delivered_events: AtomicUsize::new(0),
                first_delivered_sequence: AtomicU64::new(0),
                last_delivered_sequence: AtomicU64::new(0),
                last_enqueued_sequence: AtomicU64::new(0),
            }),
        }
    }

    pub(crate) fn id(&self) -> u64 {
        self.inner.id
    }

    pub(crate) fn try_enqueue(
        &self,
        event: DispatchEvent,
        event_kind: DiagnosticEventKind,
        encoded_bytes: usize,
        class: AdmissionClass,
    ) -> AdmissionOutcome {
        self.inner.record_enqueue_sequence(event.sequence());
        let outcome =
            self.inner
                .queue
                .try_admit(QueueEvent::new(event, event_kind, encoded_bytes, class));
        if self.inner.is_stopped() && matches!(outcome, AdmissionOutcome::Enqueued { .. }) {
            let _ = self.inner.queue.try_discard_queued();
        }
        outcome
    }

    pub(crate) fn callback_failure(&self) -> Option<CallbackFailure> {
        self.inner.callback_failure.get().cloned()
    }

    pub(crate) fn callback_failure_kind(&self) -> Option<CallbackFailureKind> {
        self.inner.callback_failure.get().map(CallbackFailure::kind)
    }

    pub(crate) fn unexpected_failure(&self) -> Option<UnexpectedDispatcherFailure> {
        self.inner.unexpected_failure.get().cloned()
    }

    pub(crate) fn delivery_progress(&self) -> DeliveryProgress {
        let delivered_events = self.inner.delivered_events.load(Ordering::Acquire);
        let first = self.inner.first_delivered_sequence.load(Ordering::Acquire);
        let last = self.inner.last_delivered_sequence.load(Ordering::Acquire);
        DeliveryProgress {
            delivered_events,
            first_delivered_sequence: (first != 0).then_some(first),
            last_delivered_sequence: (last != 0).then_some(last),
        }
    }

    pub(crate) fn drop_snapshot(&self) -> Vec<DropDelta> {
        self.inner.queue.drop_snapshot()
    }

    pub(crate) fn try_queue_snapshot(&self) -> Result<QueueSnapshot, QueueAccessError> {
        self.inner.queue.try_snapshot()
    }

    pub(crate) fn try_discard_queued(&self) -> Result<Vec<DropDelta>, QueueAccessError> {
        self.inner.queue.try_discard_queued()
    }

    pub(crate) fn registration(&self) -> Arc<SinkDispatchState> {
        Arc::clone(&self.inner)
    }
}

#[derive(Debug)]
pub(crate) struct SinkDispatchState {
    id: u64,
    callback: Py<PyAny>,
    queue: SinkQueue<DispatchEvent>,
    callback_failure: OnceLock<CallbackFailure>,
    unexpected_failure: OnceLock<UnexpectedDispatcherFailure>,
    delivered_events: AtomicUsize,
    first_delivered_sequence: AtomicU64,
    last_delivered_sequence: AtomicU64,
    last_enqueued_sequence: AtomicU64,
}

impl SinkDispatchState {
    fn is_stopped(&self) -> bool {
        self.callback_failure.get().is_some() || self.unexpected_failure.get().is_some()
    }

    fn record_callback_failure(&self, failure: CallbackFailure) {
        let _ = self.callback_failure.set(failure);
    }

    fn record_unexpected_failure(&self, stage: &'static str, detail: impl fmt::Display) {
        let _ = self.unexpected_failure.set(UnexpectedDispatcherFailure {
            stage,
            detail: detail.to_string(),
        });
    }

    fn record_delivery(&self, sequence: u64) {
        let _ = self.first_delivered_sequence.compare_exchange(
            0,
            sequence,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.last_delivered_sequence
            .store(sequence, Ordering::Release);
        self.delivered_events.fetch_add(1, Ordering::AcqRel);
    }

    fn record_enqueue_sequence(&self, sequence: u64) {
        if self
            .last_enqueued_sequence
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |previous| {
                (sequence > previous).then_some(sequence)
            })
            .is_err()
        {
            self.record_unexpected_failure(
                "enqueue_sequence",
                "diagnostic sink event sequence is not strictly increasing",
            );
        }
    }
}

#[derive(Debug)]
pub(crate) enum DispatcherCommand {
    Register(Arc<SinkDispatchState>),
    StopWhenIdle,
}

#[pyclass(unsendable)]
struct DispatcherBridge {
    commands: Receiver<DispatcherCommand>,
    ready: SyncSender<Result<DispatcherIdentity, DispatcherThreadFailure>>,
    sinks: HashMap<u64, Arc<SinkDispatchState>>,
    active: HashMap<u64, (CallbackTicket, u64)>,
    stopping: bool,
}

impl DispatcherBridge {
    fn new(
        commands: Receiver<DispatcherCommand>,
        ready: SyncSender<Result<DispatcherIdentity, DispatcherThreadFailure>>,
    ) -> Self {
        Self {
            commands,
            ready,
            sinks: HashMap::new(),
            active: HashMap::new(),
            stopping: false,
        }
    }

    fn sink(&self, sink_id: u64) -> PyResult<&Arc<SinkDispatchState>> {
        self.sinks.get(&sink_id).ok_or_else(|| {
            pyo3::exceptions::PyRuntimeError::new_err("unknown diagnostic sink dispatcher")
        })
    }

    fn complete_active(&mut self, sink_id: u64) -> u8 {
        let Some((ticket, _)) = self.active.get(&sink_id).copied() else {
            if let Some(sink) = self.sinks.get(&sink_id) {
                sink.record_unexpected_failure("callback_complete", "no active callback ticket");
            }
            return COMPLETE_STOP;
        };
        let sink = Arc::clone(
            self.sinks
                .get(&sink_id)
                .expect("active callback sink must remain registered"),
        );
        match sink.queue.try_complete_callback(ticket) {
            Ok(()) => {
                self.active.remove(&sink_id);
                COMPLETE_DONE
            }
            Err(QueueAccessError::Contended) => COMPLETE_RETRY,
            Err(error) => {
                sink.record_unexpected_failure("callback_complete", error);
                COMPLETE_STOP
            }
        }
    }
}

#[pymethods]
impl DispatcherBridge {
    fn _ready(&self, python_thread_id: u64, event_loop_id: u64, _python_daemon: bool) {
        let _ = self.ready.try_send(Ok(DispatcherIdentity {
            rust_thread_id: std::thread::current().id(),
            python_thread_id,
            event_loop_id,
            python_daemon: true,
        }));
    }

    fn _poll_commands(&mut self) -> (Vec<u64>, bool) {
        let mut registered = Vec::new();
        loop {
            match self.commands.try_recv() {
                Ok(DispatcherCommand::Register(sink)) => {
                    let sink_id = sink.id;
                    if self.stopping {
                        sink.record_unexpected_failure(
                            "register",
                            "dispatcher is already stopping",
                        );
                        continue;
                    }
                    if self.sinks.insert(sink_id, sink).is_some() {
                        if let Some(sink) = self.sinks.get(&sink_id) {
                            sink.record_unexpected_failure(
                                "register",
                                "duplicate diagnostic sink identifier",
                            );
                        }
                        continue;
                    }
                    registered.push(sink_id);
                }
                Ok(DispatcherCommand::StopWhenIdle) => self.stopping = true,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.stopping = true;
                    break;
                }
            }
        }
        (registered, self.stopping)
    }

    fn _next_dispatch(&mut self, py: Python<'_>, sink_id: u64) -> PyResult<Option<PythonDispatch>> {
        let sink = Arc::clone(self.sink(sink_id)?);
        if sink.is_stopped() {
            return Ok(None);
        }
        match sink.queue.try_begin_callback() {
            Ok(Some(delivery)) => {
                let (ticket, event) = delivery.into_parts();
                let sequence = event.sequence;
                if self.active.insert(sink_id, (ticket, sequence)).is_some() {
                    sink.record_unexpected_failure(
                        "callback_begin",
                        "sink already has an active callback",
                    );
                    return Ok(None);
                }
                Ok(Some((sink.callback.clone_ref(py), event.value, sequence)))
            }
            Ok(None) | Err(QueueAccessError::Contended) => Ok(None),
            Err(error) => {
                sink.record_unexpected_failure("callback_begin", error);
                Ok(None)
            }
        }
    }

    fn _record_raised(
        &mut self,
        sink_id: u64,
        sequence: u64,
        exception: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let sink = self.sink(sink_id)?;
        sink.record_callback_failure(CallbackFailure::raised(sequence, exception));
        Ok(())
    }

    fn _record_invalid_return(&mut self, sink_id: u64, sequence: u64) -> PyResult<()> {
        let sink = self.sink(sink_id)?;
        sink.record_callback_failure(CallbackFailure::invalid_return(sequence));
        Ok(())
    }

    fn _complete_success(&mut self, sink_id: u64, sequence: u64) -> u8 {
        if self
            .active
            .get(&sink_id)
            .is_none_or(|(_, active_sequence)| *active_sequence != sequence)
        {
            if let Some(sink) = self.sinks.get(&sink_id) {
                sink.record_unexpected_failure(
                    "callback_complete",
                    "callback sequence does not match active delivery",
                );
            }
            return COMPLETE_STOP;
        }
        let status = self.complete_active(sink_id);
        if status == COMPLETE_DONE {
            self.sinks
                .get(&sink_id)
                .expect("completed callback sink must remain registered")
                .record_delivery(sequence);
        }
        status
    }

    fn _complete_failed(&mut self, sink_id: u64) -> u8 {
        if self.active.contains_key(&sink_id) {
            let status = self.complete_active(sink_id);
            if status != COMPLETE_DONE {
                return status;
            }
        }
        let Some(sink) = self.sinks.get(&sink_id) else {
            return COMPLETE_STOP;
        };
        match sink.queue.try_discard_queued() {
            Ok(_) => COMPLETE_DONE,
            Err(QueueAccessError::Contended) => COMPLETE_RETRY,
            Err(error) => {
                sink.record_unexpected_failure("callback_discard", error);
                COMPLETE_STOP
            }
        }
    }

    fn _sink_stopped(&mut self, sink_id: u64) -> bool {
        let Some(sink) = self.sinks.get(&sink_id) else {
            return true;
        };
        if !sink.is_stopped() {
            return false;
        }
        let _ = sink.queue.try_discard_queued();
        true
    }

    const fn _stopping(&self) -> bool {
        self.stopping
    }
}

pub(crate) fn run_dispatcher(
    commands: Receiver<DispatcherCommand>,
    ready: SyncSender<Result<DispatcherIdentity, DispatcherThreadFailure>>,
) -> Result<(), DispatcherThreadFailure> {
    let readiness_fallback = ready.clone();
    let result = Python::attach(|py| {
        let bridge = Py::new(py, DispatcherBridge::new(commands, ready))?;
        let module = PyModule::from_code(
            py,
            driver_source(),
            c"diagnostic-callback-driver.py",
            c"_troupe_diagnostic_callback_driver",
        )?;
        module.getattr("_run_dispatcher")?.call1((bridge,))?;
        Ok::<(), PyErr>(())
    })
    .map_err(|error| DispatcherThreadFailure::new(error.to_string()));

    if let Err(failure) = &result {
        let _ = readiness_fallback.try_send(Err(failure.clone()));
    }
    result
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::diagnostic_sink::thread::DiagnosticThread;
    use pyo3::IntoPyObjectExt;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    fn compile_module(source: &str, name: &str) -> Py<PyModule> {
        Python::attach(|py| {
            let source = CString::new(source).expect("test source has no NUL byte");
            let filename = CString::new(format!("{name}.py")).expect("test filename has no NUL");
            let module_name = CString::new(name).expect("test module name has no NUL");
            PyModule::from_code(
                py,
                source.as_c_str(),
                filename.as_c_str(),
                module_name.as_c_str(),
            )
            .map(Bound::unbind)
            .expect("compile diagnostic callback test module")
        })
    }

    fn callback(module: &Py<PyModule>, class_name: &str, name: Option<&str>) -> Py<PyAny> {
        Python::attach(|py| {
            let class = module
                .bind(py)
                .getattr(class_name)
                .expect("diagnostic callback test class");
            match name {
                Some(name) => class.call1((name,)),
                None => class.call0(),
            }
            .map(Bound::unbind)
            .expect("construct diagnostic callback test instance")
        })
    }

    fn value(value: u64) -> Py<PyAny> {
        Python::attach(|py| value.into_py_any(py).expect("convert test event value"))
    }

    fn admit(sink: &SinkDispatcher, sequence: u64, value: u64) {
        assert!(matches!(
            sink.try_enqueue(
                DispatchEvent::new(sequence, self::value(value)),
                DiagnosticEventKind::AgentMessageDelta,
                1,
                AdmissionClass::Content,
            ),
            AdmissionOutcome::Enqueued { .. }
        ));
    }

    fn wait_until(description: &str, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + TEST_TIMEOUT;
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {description}"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn python_event_is_set(module: &Py<PyModule>, name: &str) -> bool {
        Python::attach(|py| {
            module
                .bind(py)
                .getattr(name)
                .and_then(|event| event.call_method0("is_set"))
                .and_then(|value| value.extract::<bool>())
                .expect("read Python threading event")
        })
    }

    fn set_python_event(module: &Py<PyModule>, name: &str) {
        Python::attach(|py| {
            module
                .bind(py)
                .getattr(name)
                .and_then(|event| event.call_method0("set"))
                .expect("set Python threading event");
        });
    }

    #[test]
    fn two_async_sinks_interleave_only_at_yield_and_each_sink_is_serial() {
        let _python_test_guard = crate::initialize_python_for_test();
        let module = compile_module(
            r#"
import asyncio
import contextvars
import threading

marker = contextvars.ContextVar("marker", default=None)
marker.set("creator-context")
log = []
observations = []
started = None
release = None

def gates():
    global started, release
    if started is None:
        started = asyncio.Event()
        release = asyncio.Event()
    return started, release

class Callback:
    def __init__(self, name):
        self.name = name

    async def __call__(self, event):
        observations.append((
            threading.get_ident(),
            id(asyncio.get_running_loop()),
            threading.current_thread().daemon,
            marker.get(),
        ))
        event = int(event)
        current_started, current_release = gates()
        if self.name == "a" and event == 1:
            log.append("a:1:start")
            current_started.set()
            await current_release.wait()
            log.append("a:1:end")
            return None
        if self.name == "b" and event == 1:
            await current_started.wait()
            log.append("b:1:start")
            current_release.set()
            await asyncio.sleep(0)
            log.append("b:1:end")
            return None
        log.append(f"{self.name}:{event}:start")
        log.append(f"{self.name}:{event}:end")
        return None
"#,
            "diagnostic_async_interleave",
        );
        let main_python_thread = Python::attach(|py| {
            py.import("threading")
                .and_then(|threading| threading.getattr("get_ident"))
                .and_then(|get_ident| get_ident.call0())
                .and_then(|value| value.extract::<u64>())
                .expect("read main Python thread identity")
        });
        let caller_rust_thread = thread::current().id();
        let runtime = DiagnosticThread::start().expect("start diagnostic callback thread");
        let sink_a = runtime
            .register_sink(callback(&module, "Callback", Some("a")))
            .expect("register sink a");
        let sink_b = runtime
            .register_sink(callback(&module, "Callback", Some("b")))
            .expect("register sink b");

        admit(&sink_a, 1, 1);
        admit(&sink_a, 2, 2);
        admit(&sink_b, 3, 1);
        admit(&sink_b, 4, 2);
        wait_until("both async sink queues", || {
            sink_a.delivery_progress().delivered_events() == 2
                && sink_b.delivery_progress().delivered_events() == 2
        });

        assert_eq!(
            sink_a.delivery_progress(),
            DeliveryProgress {
                delivered_events: 2,
                first_delivered_sequence: Some(1),
                last_delivered_sequence: Some(2),
            }
        );
        assert_eq!(
            sink_b.delivery_progress(),
            DeliveryProgress {
                delivered_events: 2,
                first_delivered_sequence: Some(3),
                last_delivered_sequence: Some(4),
            }
        );
        assert_eq!(sink_a.callback_failure(), None);
        assert_eq!(sink_b.callback_failure(), None);

        runtime
            .request_stop_when_idle()
            .expect("request diagnostic callback stop");
        wait_until("diagnostic callback thread exit", || runtime.is_finished());
        let identity = runtime.identity();
        runtime.join().expect("join diagnostic callback thread");

        let (log, observations) = Python::attach(|py| {
            let module = module.bind(py);
            let log = module.getattr("log")?.extract::<Vec<String>>()?;
            let observations =
                module
                    .getattr("observations")?
                    .extract::<Vec<(u64, u64, bool, Option<String>)>>()?;
            Ok::<_, PyErr>((log, observations))
        })
        .expect("read async callback observations");
        let position = |needle: &str| {
            log.iter()
                .position(|entry| entry == needle)
                .unwrap_or_else(|| panic!("missing callback log entry {needle}"))
        };
        assert!(position("a:1:start") < position("b:1:start"));
        assert!(position("b:1:start") < position("a:1:end"));
        assert!(position("a:1:end") < position("a:2:start"));
        assert!(position("b:1:end") < position("b:2:start"));

        assert_ne!(identity.rust_thread_id(), caller_rust_thread);
        assert_ne!(identity.python_thread_id(), main_python_thread);
        assert!(identity.python_daemon());
        assert_eq!(observations.len(), 4);
        assert!(observations.iter().all(|observation| {
            observation.0 == identity.python_thread_id()
                && observation.1 == identity.event_loop_id()
                && observation.3.is_none()
        }));
    }

    #[test]
    fn blocking_sync_callback_holds_only_the_diagnostic_loop() {
        let _python_test_guard = crate::initialize_python_for_test();
        let module = compile_module(
            r#"
import threading

started = threading.Event()
release = threading.Event()
log = []

class Blocking:
    def __call__(self, event):
        log.append(f"blocking:{int(event)}:start")
        started.set()
        if not release.wait(5):
            raise RuntimeError("test did not release blocking callback")
        log.append(f"blocking:{int(event)}:end")
        return None

class Healthy:
    def __call__(self, event):
        log.append(f"healthy:{int(event)}")
        return None
"#,
            "diagnostic_sync_block",
        );
        let runtime = DiagnosticThread::start().expect("start diagnostic callback thread");
        let blocking = runtime
            .register_sink(callback(&module, "Blocking", None))
            .expect("register blocking sink");
        let healthy = runtime
            .register_sink(callback(&module, "Healthy", None))
            .expect("register healthy sink");

        admit(&blocking, 1, 1);
        wait_until("blocking callback start", || {
            python_event_is_set(&module, "started")
        });

        let hub_progress = AtomicUsize::new(0);
        admit(&blocking, 2, 2);
        hub_progress.fetch_add(1, Ordering::SeqCst);
        admit(&healthy, 3, 3);
        hub_progress.fetch_add(1, Ordering::SeqCst);
        assert_eq!(hub_progress.load(Ordering::SeqCst), 2);
        thread::sleep(Duration::from_millis(25));
        assert_eq!(blocking.delivery_progress().delivered_events(), 0);
        assert_eq!(healthy.delivery_progress().delivered_events(), 0);
        assert!(!runtime.is_finished());

        set_python_event(&module, "release");
        wait_until("queues after releasing sync callback", || {
            blocking.delivery_progress().delivered_events() == 2
                && healthy.delivery_progress().delivered_events() == 1
        });
        runtime
            .request_stop_when_idle()
            .expect("request diagnostic callback stop");
        runtime.join().expect("join diagnostic callback thread");

        let log = Python::attach(|py| {
            module
                .bind(py)
                .getattr("log")
                .and_then(|log| log.extract::<Vec<String>>())
                .expect("read blocking callback log")
        });
        let blocking_end = log
            .iter()
            .position(|entry| entry == "blocking:1:end")
            .expect("blocking callback end");
        let healthy_start = log
            .iter()
            .position(|entry| entry == "healthy:3")
            .expect("healthy callback start");
        assert!(blocking_end < healthy_start);
    }

    #[test]
    fn callback_outcomes_are_bounded_first_failure_facts_and_never_escape() {
        let _python_test_guard = crate::initialize_python_for_test();
        let module = compile_module(
            r#"
import asyncio

calls = {}

def called(name):
    calls[name] = calls.get(name, 0) + 1

class Boom(Exception):
    pass

class Healthy:
    def __call__(self, event):
        called("healthy")
        if int(event) == 1:
            return None
        async def complete():
            await asyncio.sleep(0)
            return None
        return complete()

class Raised:
    def __call__(self, event):
        called("raised")
        raise Boom("x" * 5000)

class Cancelled:
    async def __call__(self, event):
        called("cancelled")
        await asyncio.sleep(0)
        raise asyncio.CancelledError("cancelled-marker")

class InvalidSync:
    def __call__(self, event):
        called("invalid_sync")
        return object()

class InvalidAsync:
    async def __call__(self, event):
        called("invalid_async")
        await asyncio.sleep(0)
        return "not-none"
"#,
            "diagnostic_callback_failures",
        );
        let runtime = DiagnosticThread::start().expect("start diagnostic callback thread");
        let healthy = runtime
            .register_sink(callback(&module, "Healthy", None))
            .expect("register healthy sink");
        let raised = runtime
            .register_sink(callback(&module, "Raised", None))
            .expect("register raised sink");
        let cancelled = runtime
            .register_sink(callback(&module, "Cancelled", None))
            .expect("register cancelled sink");
        let invalid_sync = runtime
            .register_sink(callback(&module, "InvalidSync", None))
            .expect("register invalid sync sink");
        let invalid_async = runtime
            .register_sink(callback(&module, "InvalidAsync", None))
            .expect("register invalid async sink");

        for (sink, first_sequence) in [
            (&raised, 10),
            (&cancelled, 20),
            (&invalid_sync, 30),
            (&invalid_async, 40),
        ] {
            admit(sink, first_sequence, 1);
            admit(sink, first_sequence + 1, 2);
        }
        admit(&healthy, 1, 1);
        admit(&healthy, 2, 2);

        wait_until("callback failures and healthy deliveries", || {
            healthy.delivery_progress().delivered_events() == 2
                && raised.callback_failure().is_some()
                && cancelled.callback_failure().is_some()
                && invalid_sync.callback_failure().is_some()
                && invalid_async.callback_failure().is_some()
        });

        let raised_failure = raised.callback_failure().expect("raised failure");
        assert_eq!(raised_failure.kind(), CallbackFailureKind::Raised);
        assert_eq!(raised_failure.event_sequence(), 10);
        assert_eq!(raised_failure.exception_type(), Some("Boom"));
        assert_eq!(raised_failure.message().map(str::len), Some(4 * 1024));
        assert!(raised_failure.message_truncated());

        let cancelled_failure = cancelled.callback_failure().expect("cancelled failure");
        assert_eq!(cancelled_failure.kind(), CallbackFailureKind::Raised);
        assert_eq!(cancelled_failure.event_sequence(), 20);
        assert_eq!(cancelled_failure.exception_type(), Some("CancelledError"));
        assert_eq!(cancelled_failure.message(), Some("cancelled-marker"));
        assert!(!cancelled_failure.message_truncated());

        for (sink, sequence) in [(&invalid_sync, 30), (&invalid_async, 40)] {
            let failure = sink.callback_failure().expect("invalid return failure");
            assert_eq!(failure.kind(), CallbackFailureKind::InvalidReturn);
            assert_eq!(failure.event_sequence(), sequence);
            assert_eq!(failure.exception_type(), None);
            assert_eq!(failure.message(), None);
            assert!(!failure.message_truncated());
        }
        assert_eq!(
            invalid_sync.callback_failure_kind(),
            Some(CallbackFailureKind::InvalidReturn)
        );
        for sink in [&raised, &cancelled, &invalid_sync, &invalid_async] {
            assert_eq!(sink.delivery_progress().delivered_events(), 0);
            assert_eq!(sink.unexpected_failure(), None);
        }

        let _ = raised.try_enqueue(
            DispatchEvent::new(12, self::value(3)),
            DiagnosticEventKind::AgentMessageDelta,
            1,
            AdmissionClass::Content,
        );
        admit(&healthy, 3, 3);
        wait_until(
            "failed queue release and healthy sink after peer failures",
            || {
                healthy.delivery_progress().delivered_events() == 3
                    && raised.try_queue_snapshot().is_ok_and(|snapshot| {
                        snapshot.queued_events() == 0 && !snapshot.callback_active()
                    })
            },
        );
        assert_eq!(
            raised.drop_snapshot(),
            vec![DropDelta::new(DiagnosticEventKind::AgentMessageDelta, 2, 2,)]
        );
        assert_eq!(raised.try_discard_queued(), Ok(Vec::new()));
        thread::sleep(Duration::from_millis(10));
        let calls = Python::attach(|py| {
            module
                .bind(py)
                .getattr("calls")
                .and_then(|calls| calls.extract::<HashMap<String, usize>>())
                .expect("read callback counts")
        });
        assert_eq!(calls.get("healthy"), Some(&3));
        assert_eq!(calls.get("raised"), Some(&1));
        assert_eq!(calls.get("cancelled"), Some(&1));
        assert_eq!(calls.get("invalid_sync"), Some(&1));
        assert_eq!(calls.get("invalid_async"), Some(&1));

        runtime
            .request_stop_when_idle()
            .expect("request diagnostic callback stop");
        runtime.join().expect("join diagnostic callback thread");
    }
}
