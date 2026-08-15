use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};

use pyo3::{Py, PyAny, Python};

use super::budget::RuntimeBudget;
use super::dispatcher::{
    DispatcherCommand, DispatcherIdentity, DispatcherThreadFailure, SinkDispatcher, run_dispatcher,
};

const DIAGNOSTIC_THREAD_NAME: &str = "troupe-diagnostic-callback";

#[derive(Debug)]
pub(crate) struct DiagnosticThread {
    commands: Sender<DispatcherCommand>,
    runtime_budget: RuntimeBudget,
    next_sink_id: AtomicU64,
    stopping: AtomicBool,
    identity: DispatcherIdentity,
    thread: Option<JoinHandle<Result<(), DispatcherThreadFailure>>>,
}

impl DiagnosticThread {
    pub(crate) fn start() -> Result<Self, DiagnosticThreadStartError> {
        Python::initialize();
        let (command_sender, command_receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name(DIAGNOSTIC_THREAD_NAME.to_owned())
            .spawn(move || run_dispatcher(command_receiver, ready_sender))
            .map_err(DiagnosticThreadStartError::Spawn)?;

        let identity = match ready_receiver.recv() {
            Ok(Ok(identity)) => identity,
            Ok(Err(failure)) => {
                let _ = thread.join();
                return Err(DiagnosticThreadStartError::Dispatcher(failure));
            }
            Err(_) => {
                let failure = match thread.join() {
                    Ok(Err(failure)) => failure,
                    Ok(Ok(())) => DispatcherThreadFailure::new(
                        "diagnostic callback thread exited before readiness",
                    ),
                    Err(_) => DispatcherThreadFailure::new(
                        "diagnostic callback thread panicked before readiness",
                    ),
                };
                return Err(DiagnosticThreadStartError::Dispatcher(failure));
            }
        };
        Ok(Self {
            commands: command_sender,
            runtime_budget: RuntimeBudget::new(),
            next_sink_id: AtomicU64::new(1),
            stopping: AtomicBool::new(false),
            identity,
            thread: Some(thread),
        })
    }

    pub(crate) const fn identity(&self) -> DispatcherIdentity {
        self.identity
    }

    pub(crate) fn register_sink(
        &self,
        callback: Py<PyAny>,
    ) -> Result<SinkDispatcher, DiagnosticThreadControlError> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(DiagnosticThreadControlError::Stopping);
        }
        let sink_id = self
            .next_sink_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map_err(|_| DiagnosticThreadControlError::SinkIdExhausted)?;
        let sink = SinkDispatcher::new(sink_id, self.runtime_budget.clone(), callback);
        self.commands
            .send(DispatcherCommand::Register(sink.registration()))
            .map_err(|_| DiagnosticThreadControlError::Exited)?;
        Ok(sink)
    }

    pub(crate) fn request_stop_when_idle(&self) -> Result<(), DiagnosticThreadControlError> {
        if self.stopping.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.commands
            .send(DispatcherCommand::StopWhenIdle)
            .map_err(|_| DiagnosticThreadControlError::Exited)
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub(crate) fn join(mut self) -> Result<(), DiagnosticThreadJoinError> {
        let stop = self.request_stop_when_idle();
        let thread = self
            .thread
            .take()
            .ok_or(DiagnosticThreadJoinError::AlreadyJoined)?;
        match thread.join() {
            Ok(Ok(())) => stop.map_err(DiagnosticThreadJoinError::Control),
            Ok(Err(failure)) => Err(DiagnosticThreadJoinError::Dispatcher(failure)),
            Err(_) => Err(DiagnosticThreadJoinError::Panicked),
        }
    }
}

impl Drop for DiagnosticThread {
    fn drop(&mut self) {
        if !self.stopping.swap(true, Ordering::AcqRel) {
            let _ = self.commands.send(DispatcherCommand::StopWhenIdle);
        }
    }
}

#[derive(Debug)]
pub(crate) enum DiagnosticThreadStartError {
    Spawn(std::io::Error),
    Dispatcher(DispatcherThreadFailure),
}

impl fmt::Display for DiagnosticThreadStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(error) => write!(formatter, "spawn diagnostic callback thread: {error}"),
            Self::Dispatcher(error) => write!(formatter, "start diagnostic callback loop: {error}"),
        }
    }
}

impl std::error::Error for DiagnosticThreadStartError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiagnosticThreadControlError {
    Stopping,
    Exited,
    SinkIdExhausted,
}

impl fmt::Display for DiagnosticThreadControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stopping => formatter.write_str("diagnostic callback thread is stopping"),
            Self::Exited => formatter.write_str("diagnostic callback thread has exited"),
            Self::SinkIdExhausted => {
                formatter.write_str("diagnostic callback sink identifier space is exhausted")
            }
        }
    }
}

impl std::error::Error for DiagnosticThreadControlError {}

#[derive(Debug)]
pub(crate) enum DiagnosticThreadJoinError {
    Control(DiagnosticThreadControlError),
    Dispatcher(DispatcherThreadFailure),
    AlreadyJoined,
    Panicked,
}

impl fmt::Display for DiagnosticThreadJoinError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Control(error) => write!(formatter, "stop diagnostic callback thread: {error}"),
            Self::Dispatcher(error) => {
                write!(formatter, "diagnostic callback loop failed: {error}")
            }
            Self::AlreadyJoined => formatter.write_str("diagnostic callback thread already joined"),
            Self::Panicked => formatter.write_str("diagnostic callback thread panicked"),
        }
    }
}

impl std::error::Error for DiagnosticThreadJoinError {}
