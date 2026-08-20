use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use pyo3::{Py, PyAny};
use troupe_diagnostics_core::id::{CanonicalUuid, RunLocalId};

use super::seal::{SinkHandle, SinkSealFacts};
use super::summary::SinkDeliverySummary;
use super::thread::{
    DiagnosticThread, DiagnosticThreadControlError, DiagnosticThreadJoinError,
    DiagnosticThreadStartError,
};

const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(1);
const ASYNC_CANCEL_SETTLE_INTERVAL: Duration = Duration::from_millis(10);
const DISCARD_CONTENTION_ATTEMPTS: usize = 8;

#[derive(Debug)]
pub(crate) struct DiagnosticSinkRuntime {
    thread: Option<DiagnosticThread>,
    sinks: Mutex<Vec<SinkHandle>>,
    shutting_down: AtomicBool,
}

impl DiagnosticSinkRuntime {
    pub(crate) fn start() -> Result<Self, DiagnosticSinkRuntimeStartError> {
        Ok(Self {
            thread: Some(
                DiagnosticThread::start().map_err(DiagnosticSinkRuntimeStartError::Thread)?,
            ),
            sinks: Mutex::new(Vec::new()),
            shutting_down: AtomicBool::new(false),
        })
    }

    pub(crate) fn register_sink(
        &self,
        run_id: CanonicalUuid,
        act_id: RunLocalId,
        callback: Py<PyAny>,
    ) -> Result<SinkHandle, DiagnosticSinkRuntimeRegisterError> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(DiagnosticSinkRuntimeRegisterError::ShuttingDown);
        }
        let thread = self
            .thread
            .as_ref()
            .ok_or(DiagnosticSinkRuntimeRegisterError::ShuttingDown)?;
        let dispatcher = thread
            .register_sink(callback)
            .map_err(DiagnosticSinkRuntimeRegisterError::Thread)?;
        let sink = SinkHandle::new(run_id, act_id, dispatcher);
        self.sinks
            .lock()
            .expect("diagnostic sink registry mutex poisoned")
            .push(sink.clone());
        Ok(sink)
    }

    pub(crate) fn shutdown_until(mut self, deadline: Instant) -> SinkShutdownReport {
        self.shutting_down.store(true, Ordering::Release);
        let sinks = self
            .sinks
            .lock()
            .expect("diagnostic sink registry mutex poisoned")
            .clone();
        for sink in &sinks {
            if sink.is_open() {
                let _ = sink.seal(SinkSealFacts::runtime_shutdown(None));
            }
        }

        let thread = self
            .thread
            .as_ref()
            .expect("diagnostic sink Runtime must own its callback thread");
        let cancel_at = deadline
            .checked_sub(ASYNC_CANCEL_SETTLE_INTERVAL)
            .unwrap_or_else(Instant::now);
        let stop_error = thread.request_stop_at(cancel_at).err();

        loop {
            for sink in &sinks {
                if sink.summary().is_none() {
                    let _ = sink.try_close_drained();
                }
            }
            let all_closed = sinks.iter().all(SinkHandle::is_closed);
            if all_closed && thread.is_finished() {
                let thread = self
                    .thread
                    .take()
                    .expect("diagnostic sink Runtime must retain thread until close");
                let thread_close = match thread.join() {
                    Ok(()) => DiagnosticThreadClose::Joined,
                    Err(error) => DiagnosticThreadClose::Failed(error),
                };
                return SinkShutdownReport::new(sinks, thread_close, stop_error);
            }
            if thread.is_finished() || Instant::now() >= deadline {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            thread::sleep(SHUTDOWN_POLL_INTERVAL.min(remaining));
        }

        for sink in &sinks {
            if sink.summary().is_some() {
                continue;
            }
            for _ in 0..DISCARD_CONTENTION_ATTEMPTS {
                if sink.try_discard_pending().is_ok() {
                    break;
                }
                thread::yield_now();
            }
            let callback_abandoned = sink.callback_is_active().unwrap_or(false);
            sink.close_for_runtime_shutdown(callback_abandoned);
        }

        let thread = self
            .thread
            .take()
            .expect("diagnostic sink Runtime must retain thread until close");
        let thread_close = if thread.is_finished() {
            match thread.join() {
                Ok(()) => DiagnosticThreadClose::Joined,
                Err(error) => DiagnosticThreadClose::Failed(error),
            }
        } else {
            drop(thread);
            DiagnosticThreadClose::Abandoned
        };
        SinkShutdownReport::new(sinks, thread_close, stop_error)
    }
}

#[derive(Debug)]
pub(crate) enum DiagnosticSinkRuntimeStartError {
    Thread(DiagnosticThreadStartError),
}

impl fmt::Display for DiagnosticSinkRuntimeStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Thread(error) => write!(formatter, "start diagnostic sink Runtime: {error}"),
        }
    }
}

impl std::error::Error for DiagnosticSinkRuntimeStartError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiagnosticSinkRuntimeRegisterError {
    ShuttingDown,
    Thread(DiagnosticThreadControlError),
}

impl fmt::Display for DiagnosticSinkRuntimeRegisterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShuttingDown => formatter.write_str("diagnostic sink Runtime is shutting down"),
            Self::Thread(error) => write!(formatter, "register diagnostic sink: {error}"),
        }
    }
}

impl std::error::Error for DiagnosticSinkRuntimeRegisterError {}

#[derive(Debug)]
pub(crate) enum DiagnosticThreadClose {
    Joined,
    Failed(DiagnosticThreadJoinError),
    Abandoned,
}

#[derive(Debug)]
pub(crate) struct SinkShutdownReport {
    summaries: Vec<Arc<SinkDeliverySummary>>,
    thread_close: DiagnosticThreadClose,
    stop_error: Option<DiagnosticThreadControlError>,
}

impl SinkShutdownReport {
    fn new(
        sinks: Vec<SinkHandle>,
        thread_close: DiagnosticThreadClose,
        stop_error: Option<DiagnosticThreadControlError>,
    ) -> Self {
        let summaries = sinks
            .into_iter()
            .map(|sink| {
                sink.summary()
                    .expect("shutdown must close every registered diagnostic sink")
            })
            .collect();
        Self {
            summaries,
            thread_close,
            stop_error,
        }
    }

    pub(crate) fn summaries(&self) -> &[Arc<SinkDeliverySummary>] {
        &self.summaries
    }

    pub(crate) const fn thread_close(&self) -> &DiagnosticThreadClose {
        &self.thread_close
    }

    pub(crate) const fn stop_error(&self) -> Option<DiagnosticThreadControlError> {
        self.stop_error
    }

    pub(crate) fn callback_abandoned(&self) -> usize {
        self.summaries
            .iter()
            .filter(|summary| summary.callback_abandoned())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::sync::Arc;

    use pyo3::IntoPyObjectExt;
    use pyo3::prelude::*;
    use pyo3::types::{PyAnyMethods, PyModule};
    use troupe_diagnostics_core::event::DiagnosticEventKind;

    use super::*;
    use crate::diagnostic_sink::callback::CallbackFailureKind;
    use crate::diagnostic_sink::dispatcher::DispatchEvent;
    use crate::diagnostic_sink::queue::{AdmissionClass, AdmissionOutcome};
    use crate::diagnostic_sink::seal::{SinkEnqueueRejection, SinkSealError};
    use crate::diagnostic_sink::summary::{ActOutcome, SinkCloseReason};

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);
    const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";

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
            .expect("compile diagnostic close test module")
        })
    }

    fn callback(module: &Py<PyModule>, class_name: &str) -> Py<PyAny> {
        Python::attach(|py| {
            module
                .bind(py)
                .getattr(class_name)
                .and_then(|class| class.call0())
                .map(Bound::unbind)
                .expect("construct diagnostic close callback")
        })
    }

    fn value(value: u64) -> Py<PyAny> {
        Python::attach(|py| value.into_py_any(py).expect("convert test event value"))
    }

    fn run_id() -> CanonicalUuid {
        CanonicalUuid::parse(RUN_ID).expect("canonical test Run UUID")
    }

    fn act_id(value: &str) -> RunLocalId {
        RunLocalId::parse(value).expect("valid test Act ID")
    }

    fn admit(sink: &SinkHandle, sequence: u64, class: AdmissionClass) -> AdmissionOutcome {
        sink.try_enqueue(
            DispatchEvent::new(sequence, value(sequence)),
            DiagnosticEventKind::AgentMessageDelta,
            1,
            class,
        )
        .expect("open diagnostic sink accepts event")
    }

    fn admit_terminal(sink: &SinkHandle, sequence: u64) -> AdmissionOutcome {
        sink.try_enqueue_terminal(
            DispatchEvent::new(sequence, value(sequence)),
            DiagnosticEventKind::SpanFinished,
            1,
        )
        .expect("open diagnostic sink accepts structural terminal")
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
    fn normal_drain_latches_one_summary_and_rejects_after_seal_and_close() {
        let _python_test_guard = crate::initialize_python_for_test();
        let module = compile_module(
            r#"
events = []

class Healthy:
    def __call__(self, event):
        events.append(int(event))
        return None
"#,
            "diagnostic_close_normal",
        );
        let runtime = DiagnosticSinkRuntime::start().expect("start diagnostic sink Runtime");
        let sink = runtime
            .register_sink(run_id(), act_id("act-normal"), callback(&module, "Healthy"))
            .expect("register normal sink");

        assert_eq!(
            sink.seal(SinkSealFacts::act_finished(ActOutcome::Completed)),
            Err(SinkSealError::TerminalNotAccounted)
        );
        assert!(matches!(
            admit(&sink, 1, AdmissionClass::Content),
            AdmissionOutcome::Enqueued { .. }
        ));
        assert!(matches!(
            admit_terminal(&sink, 2),
            AdmissionOutcome::Enqueued { .. }
        ));
        sink.seal(SinkSealFacts::act_finished(ActOutcome::Completed))
            .expect("seal after structural terminal");
        assert_eq!(
            sink.seal(SinkSealFacts::act_finished(ActOutcome::Completed)),
            Err(SinkSealError::AlreadySealed)
        );
        assert_eq!(
            sink.try_enqueue(
                DispatchEvent::new(3, value(3)),
                DiagnosticEventKind::AgentMessageDelta,
                1,
                AdmissionClass::Content,
            ),
            Err(SinkEnqueueRejection::Sealed)
        );

        let report = runtime.shutdown_until(Instant::now() + TEST_TIMEOUT);
        assert!(matches!(
            report.thread_close(),
            DiagnosticThreadClose::Joined
        ));
        assert_eq!(report.stop_error(), None);
        assert_eq!(report.callback_abandoned(), 0);
        let summary = &report.summaries()[0];
        assert!(Arc::ptr_eq(
            summary,
            &sink.summary().expect("repeat summary read")
        ));
        assert_eq!(summary.run_id(), run_id());
        assert_eq!(summary.act_id(), &act_id("act-normal"));
        assert_eq!(summary.act_outcome(), Some(ActOutcome::Completed));
        assert_eq!(summary.close_reason(), SinkCloseReason::ActFinished);
        assert_eq!(summary.close_reason().as_str(), "act_finished");
        assert!(summary.complete());
        assert_eq!(summary.delivered_events(), 2);
        assert_eq!(summary.first_delivered_sequence(), Some(1));
        assert_eq!(summary.last_delivered_sequence(), Some(2));
        assert_eq!(summary.dropped_events(), 0);
        assert_eq!(summary.dropped_bytes(), 0);
        assert!(summary.dropped_by_kind().is_empty());
        assert_eq!(summary.source_gaps(), 0);
        assert_eq!(summary.truncated_payloads(), 0);
        assert_eq!(summary.callback_failure(), None);
        assert!(!summary.callback_abandoned());
        for sequence in [4, 5] {
            assert_eq!(
                sink.try_enqueue(
                    DispatchEvent::new(sequence, value(sequence)),
                    DiagnosticEventKind::AgentMessageDelta,
                    1,
                    AdmissionClass::Content,
                ),
                Err(SinkEnqueueRejection::Closed)
            );
        }
        let events = Python::attach(|py| {
            module
                .bind(py)
                .getattr("events")
                .and_then(|events| events.extract::<Vec<u64>>())
                .expect("read delivered normal events")
        });
        assert_eq!(events, vec![1, 2]);
    }

    #[test]
    fn callback_failure_closes_only_that_sink_with_first_typed_failure() {
        let _python_test_guard = crate::initialize_python_for_test();
        let module = compile_module(
            r#"
calls = 0

class Raised:
    def __call__(self, event):
        global calls
        calls += 1
        raise RuntimeError("close-marker")
"#,
            "diagnostic_close_failure",
        );
        let runtime = DiagnosticSinkRuntime::start().expect("start diagnostic sink Runtime");
        let sink = runtime
            .register_sink(run_id(), act_id("act-failure"), callback(&module, "Raised"))
            .expect("register failing sink");
        admit(&sink, 10, AdmissionClass::Content);
        admit_terminal(&sink, 11);
        sink.seal(SinkSealFacts::act_finished(ActOutcome::Failed))
            .expect("seal failing sink");

        let report = runtime.shutdown_until(Instant::now() + TEST_TIMEOUT);
        assert!(matches!(
            report.thread_close(),
            DiagnosticThreadClose::Joined
        ));
        let summary = &report.summaries()[0];
        assert_eq!(summary.close_reason(), SinkCloseReason::CallbackFailed);
        assert_eq!(summary.close_reason().as_str(), "callback_failed");
        assert!(!summary.complete());
        assert!(!summary.callback_abandoned());
        let failure = summary.callback_failure().expect("typed callback failure");
        assert_eq!(failure.kind(), CallbackFailureKind::Raised);
        assert_eq!(failure.event_sequence(), 10);
        assert_eq!(failure.exception_type(), Some("RuntimeError"));
        assert_eq!(failure.message(), Some("close-marker"));
        let calls = Python::attach(|py| {
            module
                .bind(py)
                .getattr("calls")
                .and_then(|calls| calls.extract::<usize>())
                .expect("read failing callback count")
        });
        assert_eq!(calls, 1);
    }

    #[test]
    fn structural_reserve_exhaustion_closes_with_exact_drop_facts() {
        let _python_test_guard = crate::initialize_python_for_test();
        let module = compile_module(
            r#"
import threading

started = threading.Event()
release = threading.Event()

class Blocking:
    def __call__(self, event):
        if int(event) == 1:
            started.set()
            if not release.wait(5):
                raise RuntimeError("overflow test did not release callback")
        return None
"#,
            "diagnostic_close_overflow",
        );
        let runtime = DiagnosticSinkRuntime::start().expect("start diagnostic sink Runtime");
        let sink = runtime
            .register_sink(
                run_id(),
                act_id("act-overflow"),
                callback(&module, "Blocking"),
            )
            .expect("register overflow sink");
        admit(&sink, 1, AdmissionClass::Structural);
        wait_until("overflow callback start", || {
            python_event_is_set(&module, "started")
        });
        for sequence in 2..1_025 {
            assert!(matches!(
                admit(&sink, sequence, AdmissionClass::Structural),
                AdmissionOutcome::Enqueued { .. }
            ));
        }
        sink.record_source_gaps(1).expect("record source gap");
        sink.record_truncated_payloads(2)
            .expect("record payload truncation");
        assert!(matches!(
            admit_terminal(&sink, 1_025),
            AdmissionOutcome::Terminalized { .. }
        ));
        sink.seal(SinkSealFacts::act_finished(ActOutcome::Completed))
            .expect("seal overflow sink");
        set_python_event(&module, "release");

        let report = runtime.shutdown_until(Instant::now() + TEST_TIMEOUT);
        assert!(matches!(
            report.thread_close(),
            DiagnosticThreadClose::Joined
        ));
        let summary = &report.summaries()[0];
        assert_eq!(summary.close_reason(), SinkCloseReason::DeliveryOverflow);
        assert_eq!(summary.close_reason().as_str(), "delivery_overflow");
        assert!(!summary.complete());
        assert_eq!(summary.delivered_events(), 1);
        assert_eq!(summary.dropped_events(), 1_024);
        assert_eq!(summary.dropped_bytes(), 1_024);
        assert_eq!(summary.dropped_by_kind().len(), 2);
        let dropped = summary
            .dropped_by_kind()
            .iter()
            .map(|count| (count.event_kind(), count.events()))
            .collect::<HashMap<_, _>>();
        assert_eq!(
            dropped.get(&DiagnosticEventKind::AgentMessageDelta),
            Some(&1_023)
        );
        assert_eq!(dropped.get(&DiagnosticEventKind::SpanFinished), Some(&1));
        assert_eq!(summary.source_gaps(), 1);
        assert_eq!(summary.truncated_payloads(), 2);
    }

    #[test]
    fn deadline_abandons_blocking_callback_without_joining_it() {
        let _python_test_guard = crate::initialize_python_for_test();
        let module = compile_module(
            r#"
import threading

started = threading.Event()
release = threading.Event()
finished = threading.Event()
healthy_calls = 0

class Blocking:
    def __call__(self, event):
        started.set()
        if not release.wait(5):
            raise RuntimeError("abandon test did not release callback")
        finished.set()
        return None

class Healthy:
    def __call__(self, event):
        global healthy_calls
        healthy_calls += 1
        return None
"#,
            "diagnostic_close_abandon",
        );
        let runtime = DiagnosticSinkRuntime::start().expect("start diagnostic sink Runtime");
        let sink = runtime
            .register_sink(
                run_id(),
                act_id("act-abandon"),
                callback(&module, "Blocking"),
            )
            .expect("register abandoned sink");
        admit(&sink, 1, AdmissionClass::Content);
        wait_until("blocking callback start", || {
            python_event_is_set(&module, "started")
        });
        let peer = runtime
            .register_sink(run_id(), act_id("act-peer"), callback(&module, "Healthy"))
            .expect("register peer behind blocking callback");
        admit(&peer, 2, AdmissionClass::Content);

        let started = Instant::now();
        let report = runtime.shutdown_until(started + Duration::from_millis(30));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(matches!(
            report.thread_close(),
            DiagnosticThreadClose::Abandoned
        ));
        assert_eq!(report.callback_abandoned(), 1);
        let summary = &report.summaries()[0];
        let peer_summary = &report.summaries()[1];
        assert_eq!(summary.close_reason(), SinkCloseReason::RuntimeShutdown);
        assert_eq!(summary.close_reason().as_str(), "runtime_shutdown");
        assert!(!summary.complete());
        assert!(summary.callback_abandoned());
        assert_eq!(summary.delivered_events(), 0);
        assert_eq!(
            peer_summary.close_reason(),
            SinkCloseReason::RuntimeShutdown
        );
        assert!(!peer_summary.callback_abandoned());
        assert_eq!(peer_summary.delivered_events(), 0);
        assert_eq!(peer_summary.dropped_events(), 1);
        assert!(Arc::ptr_eq(
            summary,
            &sink.summary().expect("abandoned summary remains readable")
        ));
        assert_eq!(
            sink.try_enqueue(
                DispatchEvent::new(2, value(2)),
                DiagnosticEventKind::AgentMessageDelta,
                1,
                AdmissionClass::Content,
            ),
            Err(SinkEnqueueRejection::Closed)
        );

        set_python_event(&module, "release");
        wait_until("detached callback release", || {
            python_event_is_set(&module, "finished")
        });
        thread::sleep(Duration::from_millis(10));
        let repeated = sink
            .summary()
            .expect("late callback cannot replace summary");
        assert!(Arc::ptr_eq(summary, &repeated));
        assert_eq!(repeated.close_reason(), SinkCloseReason::RuntimeShutdown);
        assert_eq!(repeated.delivered_events(), 0);
        assert!(repeated.callback_abandoned());
        let healthy_calls = Python::attach(|py| {
            module
                .bind(py)
                .getattr("healthy_calls")
                .and_then(|calls| calls.extract::<usize>())
                .expect("read peer callback count")
        });
        assert_eq!(healthy_calls, 0);
    }

    #[test]
    fn deadline_cancels_async_callback_without_recording_callback_failure() {
        let _python_test_guard = crate::initialize_python_for_test();
        let module = compile_module(
            r#"
import asyncio
import threading

started = threading.Event()
cancelled = threading.Event()

class AsyncBlocking:
    async def __call__(self, event):
        started.set()
        try:
            await asyncio.Event().wait()
        except asyncio.CancelledError:
            cancelled.set()
            raise
"#,
            "diagnostic_close_async_cancel",
        );
        let runtime = DiagnosticSinkRuntime::start().expect("start diagnostic sink Runtime");
        let sink = runtime
            .register_sink(
                run_id(),
                act_id("act-async-cancel"),
                callback(&module, "AsyncBlocking"),
            )
            .expect("register async sink");
        admit(&sink, 1, AdmissionClass::Content);
        wait_until("async callback start", || {
            python_event_is_set(&module, "started")
        });

        let started = Instant::now();
        let report = runtime.shutdown_until(started + Duration::from_millis(50));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(matches!(
            report.thread_close(),
            DiagnosticThreadClose::Joined
        ));
        assert!(python_event_is_set(&module, "cancelled"));
        assert_eq!(report.callback_abandoned(), 0);
        let summary = &report.summaries()[0];
        assert_eq!(summary.close_reason(), SinkCloseReason::RuntimeShutdown);
        assert!(!summary.complete());
        assert!(!summary.callback_abandoned());
        assert_eq!(summary.callback_failure(), None);
        assert_eq!(summary.delivered_events(), 0);
        assert!(Arc::ptr_eq(
            summary,
            &sink
                .summary()
                .expect("async cancellation summary remains stable")
        ));
    }

    #[test]
    fn summary_waits_until_delivery_progress_publication_is_settled() {
        let _python_test_guard = crate::initialize_python_for_test();
        let module = compile_module(
            r#"
class Healthy:
    def __call__(self, event):
        return None
"#,
            "diagnostic_close_delivery_settlement",
        );
        let runtime = DiagnosticSinkRuntime::start().expect("start diagnostic sink Runtime");
        let sink = runtime
            .register_sink(
                run_id(),
                act_id("act-delivery-settlement"),
                callback(&module, "Healthy"),
            )
            .expect("register settlement sink");
        sink.seal(SinkSealFacts::runtime_shutdown(None))
            .expect("seal empty settlement sink");

        sink.set_delivery_settling_for_test(true);
        assert_eq!(
            sink.try_close_drained().expect("poll settling sink"),
            crate::diagnostic_sink::seal::SinkClosePoll::Pending
        );
        sink.set_delivery_settling_for_test(false);
        assert!(matches!(
            sink.try_close_drained().expect("close settled sink"),
            crate::diagnostic_sink::seal::SinkClosePoll::Closed(_)
        ));

        let report = runtime.shutdown_until(Instant::now() + TEST_TIMEOUT);
        assert!(matches!(
            report.thread_close(),
            DiagnosticThreadClose::Joined
        ));
        assert!(Arc::ptr_eq(
            &report.summaries()[0],
            &sink.summary().expect("settled summary remains stable")
        ));
    }

    #[test]
    fn idle_runtime_stops_and_joins_with_no_sinks() {
        let _python_test_guard = crate::initialize_python_for_test();
        let runtime = DiagnosticSinkRuntime::start().expect("start idle diagnostic sink Runtime");
        let report = runtime.shutdown_until(Instant::now() + TEST_TIMEOUT);
        assert!(matches!(
            report.thread_close(),
            DiagnosticThreadClose::Joined
        ));
        assert!(report.summaries().is_empty());
        assert_eq!(report.callback_abandoned(), 0);
    }
}
