use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

pub(crate) trait PreparedActAuthorityExpiry: Send + 'static {
    /// Expire authority reversibly until the coordinator commits every participant.
    fn commit(&mut self);

    /// Restore the exact pre-commit authority after a sink rejects settlement.
    fn rollback(&mut self);
}

pub(crate) trait ActAuthorityExpiry: Send + Sync + 'static {
    /// Stage expiry without changing the authority visible to publishers.
    fn prepare_expiry(
        &self,
    ) -> Result<Box<dyn PreparedActAuthorityExpiry>, ActAuthorityExpiryPrepareError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ActAuthorityExpiryPrepareError {
    code: &'static str,
}

impl ActAuthorityExpiryPrepareError {
    pub(crate) const fn new(code: &'static str) -> Self {
        Self { code }
    }

    pub(crate) const fn code(self) -> &'static str {
        self.code
    }
}

impl fmt::Display for ActAuthorityExpiryPrepareError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code)
    }
}

impl std::error::Error for ActAuthorityExpiryPrepareError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ActAuthorityExpiryInstallError;

impl fmt::Display for ActAuthorityExpiryInstallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Act authority expiry is already installed or settlement has begun")
    }
}

impl std::error::Error for ActAuthorityExpiryInstallError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ActSettlementError {
    code: &'static str,
    detail: String,
}

impl ActSettlementError {
    pub(crate) fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    pub(crate) const fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for ActSettlementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} [{}]", self.detail, self.code)
    }
}

impl std::error::Error for ActSettlementError {}

pub(crate) enum ActSettlementSinkCommit {
    Committed,
    Rejected(ActSettlementError),
    CommittedWithFailure(ActSettlementError),
}

pub(crate) trait ActSettlementSink: Send + Sync {
    /// Validate every fallible precondition without changing sink state.
    fn prepare_settlement(&self) -> Result<(), ActSettlementError>;

    /// Seal and retire the sink. A committed failure must not be rolled back.
    fn commit_settlement(&self) -> ActSettlementSinkCommit;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CoordinatorPhase {
    Open,
    Settled,
}

struct CoordinatorState {
    phase: CoordinatorPhase,
    authority: Option<Arc<dyn ActAuthorityExpiry>>,
}

pub(crate) struct ActSettlementCoordinator {
    state: Mutex<CoordinatorState>,
}

impl ActSettlementCoordinator {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(CoordinatorState {
                phase: CoordinatorPhase::Open,
                authority: None,
            }),
        })
    }

    pub(crate) fn install_authority_expiry(
        &self,
        authority: Arc<dyn ActAuthorityExpiry>,
    ) -> Result<(), ActAuthorityExpiryInstallError> {
        let mut state = lock(&self.state);
        if state.phase != CoordinatorPhase::Open || state.authority.is_some() {
            return Err(ActAuthorityExpiryInstallError);
        }
        state.authority = Some(authority);
        Ok(())
    }

    pub(crate) fn settle_with_sink(
        &self,
        sink: &dyn ActSettlementSink,
    ) -> Result<(), ActSettlementError> {
        self.settle(Some(sink), false)
    }

    /// B14 uses this path when an Act has authority but no Python sink.
    pub(crate) fn settle_authority_only(&self) -> Result<(), ActSettlementError> {
        self.settle(None, true)
    }

    fn settle(
        &self,
        sink: Option<&dyn ActSettlementSink>,
        authority_required: bool,
    ) -> Result<(), ActSettlementError> {
        let mut state = lock(&self.state);
        if state.phase != CoordinatorPhase::Open {
            return Err(ActSettlementError::new(
                "act.settlement-duplicated",
                "Act settlement has already completed",
            ));
        }
        if authority_required && state.authority.is_none() {
            return Err(ActSettlementError::new(
                "act.authority-expiry-missing",
                "authority-only Act settlement requires an expiry participant",
            ));
        }
        if let Some(sink) = sink {
            sink.prepare_settlement()?;
        }

        let mut authority = state
            .authority
            .as_ref()
            .map(|authority| authority.prepare_expiry())
            .transpose()
            .map_err(|error| ActSettlementError::new(error.code(), error.to_string()))?;
        if let Some(authority) = authority.as_mut() {
            authority.commit();
        }

        match sink.map(ActSettlementSink::commit_settlement) {
            None | Some(ActSettlementSinkCommit::Committed) => {
                state.phase = CoordinatorPhase::Settled;
                Ok(())
            }
            Some(ActSettlementSinkCommit::Rejected(error)) => {
                if let Some(authority) = authority.as_mut() {
                    authority.rollback();
                }
                Err(error)
            }
            Some(ActSettlementSinkCommit::CommittedWithFailure(error)) => {
                state.phase = CoordinatorPhase::Settled;
                Err(error)
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(not(test))]
mod active {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use pyo3::prelude::*;
    use pyo3::types::{PyAnyMethods, PyDict, PyTuple};

    use super::{
        ActAuthorityExpiry, ActAuthorityExpiryInstallError, ActSettlementCoordinator,
        ActSettlementError, ActSettlementSinkCommit, lock,
    };
    use crate::diagnostic_runtime::act_producer::ActDiagnosticFailureOwner;
    use crate::diagnostic_sink::{
        ActOutcome, SinkCloseError, SinkClosePoll, SinkDeliverySummary, SinkHandle, SinkSealError,
        SinkSealFacts,
    };

    const CLOSE_POLL_INTERVAL: Duration = Duration::from_millis(1);

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum SinkSettlementPhase {
        Open,
        Sealed,
        Closed,
    }

    pub(crate) struct ActSinkSettlement {
        coordinator: Arc<ActSettlementCoordinator>,
        handle: SinkHandle,
        request: Py<PyAny>,
        failure_owner: Arc<dyn ActDiagnosticFailureOwner>,
        phase: Mutex<SinkSettlementPhase>,
    }

    impl ActSinkSettlement {
        pub(crate) fn new(
            handle: SinkHandle,
            request: Py<PyAny>,
            failure_owner: Arc<dyn ActDiagnosticFailureOwner>,
        ) -> Arc<Self> {
            Arc::new(Self {
                coordinator: ActSettlementCoordinator::new(),
                handle,
                request,
                failure_owner,
                phase: Mutex::new(SinkSettlementPhase::Open),
            })
        }

        pub(crate) fn coordinator(&self) -> Arc<ActSettlementCoordinator> {
            Arc::clone(&self.coordinator)
        }

        pub(crate) fn install_authority_expiry(
            &self,
            authority: Arc<dyn ActAuthorityExpiry>,
        ) -> Result<(), ActAuthorityExpiryInstallError> {
            self.coordinator.install_authority_expiry(authority)
        }

        pub(crate) fn prepare_terminal_seal(&self) -> Result<(), ActSettlementError> {
            let phase = lock(&self.phase);
            if *phase != SinkSettlementPhase::Open {
                return Err(ActSettlementError::new(
                    "act.sink-settlement-not-open",
                    "diagnostic sink settlement is not open",
                ));
            }
            if !self.handle.is_open() {
                return Err(ActSettlementError::new(
                    "act.sink-settlement-not-open",
                    "diagnostic sink queue is not open at Act settlement",
                ));
            }
            Python::attach(|py| require_python_state(py, &self.request, "BOUND"))
                .map_err(|error| ActSettlementError::python("preflight Python sink", error))
        }

        pub(crate) fn commit_terminal_seal(&self, outcome: ActOutcome) -> ActSettlementSinkCommit {
            let mut phase = lock(&self.phase);
            if *phase != SinkSettlementPhase::Open {
                return ActSettlementSinkCommit::Rejected(ActSettlementError::new(
                    "act.sink-settlement-not-open",
                    "diagnostic sink settlement is not open",
                ));
            }

            let mut committed_failure = match self.handle.seal(SinkSealFacts::act_finished(outcome))
            {
                Ok(()) => None,
                Err(SinkSealError::TerminalNotAccounted) => {
                    return ActSettlementSinkCommit::Rejected(ActSettlementError::new(
                        "act.sink-seal-failed",
                        SinkSealError::TerminalNotAccounted.to_string(),
                    ));
                }
                Err(SinkSealError::AlreadySealed) => Some(ActSettlementError::new(
                    "act.sink-seal-failed",
                    SinkSealError::AlreadySealed.to_string(),
                )),
            };

            // Once K02 is sealed, authority must remain expired even if Python projection fails.
            *phase = SinkSettlementPhase::Sealed;
            if let Err(error) = Python::attach(|py| {
                call_base_sink_method(py, &self.request, "_diagnostic_seal", None)
            }) {
                committed_failure
                    .get_or_insert_with(|| ActSettlementError::python("seal Python sink", error));
            }
            match committed_failure {
                Some(error) => ActSettlementSinkCommit::CommittedWithFailure(error),
                None => ActSettlementSinkCommit::Committed,
            }
        }

        pub(crate) fn start_close_waiter(self: &Arc<Self>) {
            let settlement = Arc::clone(self);
            pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
                loop {
                    match settlement.poll_close() {
                        Ok(true) => return,
                        Ok(false) => tokio::time::sleep(CLOSE_POLL_INTERVAL).await,
                        Err(error) => {
                            settlement.failure_owner.latch_state_failure(error.code());
                            return;
                        }
                    }
                }
            });
        }

        pub(crate) fn begin_runtime_shutdown(&self) -> Result<(), ActSettlementError> {
            let mut phase = lock(&self.phase);
            match *phase {
                SinkSettlementPhase::Open => {
                    Python::attach(|py| {
                        require_python_state(py, &self.request, "BOUND")?;
                        call_base_sink_method(py, &self.request, "_diagnostic_seal", None)
                    })
                    .map_err(|error| {
                        ActSettlementError::python("seal Python sink for Runtime shutdown", error)
                    })?;
                    *phase = SinkSettlementPhase::Sealed;
                }
                SinkSettlementPhase::Sealed | SinkSettlementPhase::Closed => {}
            }
            Ok(())
        }

        pub(crate) fn publish_latched_summary(&self) -> Result<(), ActSettlementError> {
            let summary = self.handle.summary().ok_or_else(|| {
                ActSettlementError::new(
                    "act.sink-summary-unavailable",
                    "diagnostic sink Runtime shutdown did not latch a summary",
                )
            })?;
            self.publish_closed(summary)
        }

        fn poll_close(&self) -> Result<bool, ActSettlementError> {
            match self.handle.try_close_drained() {
                Ok(SinkClosePoll::Pending) => Ok(false),
                Ok(SinkClosePoll::Closed(summary)) => {
                    self.publish_closed(summary)?;
                    Ok(true)
                }
                Err(SinkCloseError::Queue(_)) => Ok(false),
                Err(error) => Err(ActSettlementError::new(
                    "act.sink-close-poll-failed",
                    error.to_string(),
                )),
            }
        }

        fn publish_closed(
            &self,
            summary: Arc<SinkDeliverySummary>,
        ) -> Result<(), ActSettlementError> {
            let mut phase = lock(&self.phase);
            if *phase == SinkSettlementPhase::Closed {
                return Ok(());
            }
            if *phase != SinkSettlementPhase::Sealed {
                return Err(ActSettlementError::new(
                    "act.sink-close-before-seal",
                    "diagnostic sink summary became available before settlement seal",
                ));
            }
            Python::attach(|py| {
                let value = materialize_summary(py, &summary)?;
                call_base_sink_method(py, &self.request, "_diagnostic_close", Some(value.bind(py)))
            })
            .map_err(|error| ActSettlementError::python("close Python sink", error))?;
            *phase = SinkSettlementPhase::Closed;
            Ok(())
        }
    }

    impl ActSettlementError {
        fn python(operation: &'static str, error: PyErr) -> Self {
            Self::new(
                "act.sink-python-settlement-failed",
                format!("{operation}: {error}"),
            )
        }
    }

    fn require_python_state(py: Python<'_>, request: &Py<PyAny>, expected: &str) -> PyResult<()> {
        let diagnostics = py.import("troupe.diagnostics")?;
        let sink_type = diagnostics.getattr("DiagnosticSink")?;
        let method = sink_type
            .getattr("__dict__")?
            .get_item("_diagnostic_require_state")?;
        let state = method.call1((request.bind(py),))?.extract::<String>()?;
        if state == expected {
            Ok(())
        } else {
            Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
                "diagnostic sink state is {state}, expected {expected}"
            )))
        }
    }

    fn call_base_sink_method(
        py: Python<'_>,
        request: &Py<PyAny>,
        method_name: &str,
        argument: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<()> {
        let diagnostics = py.import("troupe.diagnostics")?;
        let sink_type = diagnostics.getattr("DiagnosticSink")?;
        let method = sink_type.getattr("__dict__")?.get_item(method_name)?;
        match argument {
            Some(argument) => method.call1((request.bind(py), argument))?,
            None => method.call1((request.bind(py),))?,
        };
        Ok(())
    }

    fn materialize_summary(py: Python<'_>, summary: &SinkDeliverySummary) -> PyResult<Py<PyAny>> {
        let diagnostics = py.import("troupe.diagnostics")?;
        let kwargs = PyDict::new(py);
        let run_id = py
            .import("uuid")?
            .getattr("UUID")?
            .call1((summary.run_id().to_string(),))?;
        kwargs.set_item("run_id", run_id)?;
        kwargs.set_item("act_id", summary.act_id().as_str())?;
        match summary.act_outcome() {
            Some(outcome) => kwargs.set_item("act_outcome", outcome.as_str())?,
            None => kwargs.set_item("act_outcome", py.None())?,
        }
        kwargs.set_item("close_reason", summary.close_reason().as_str())?;
        kwargs.set_item("complete", summary.complete())?;
        kwargs.set_item("delivered_events", summary.delivered_events())?;
        set_optional_u64(
            &kwargs,
            "first_delivered_sequence",
            summary.first_delivered_sequence(),
        )?;
        set_optional_u64(
            &kwargs,
            "last_delivered_sequence",
            summary.last_delivered_sequence(),
        )?;
        kwargs.set_item("dropped_events", summary.dropped_events())?;
        kwargs.set_item("dropped_bytes", summary.dropped_bytes())?;

        let dropped = summary
            .dropped_by_kind()
            .iter()
            .map(|count| {
                let row = PyDict::new(py);
                row.set_item("event_kind", count.event_kind().as_str())?;
                row.set_item("events", count.events())?;
                row.set_item("encoded_bytes", count.encoded_bytes())?;
                diagnostics
                    .getattr("DiagnosticDropCount")?
                    .call((), Some(&row))
                    .map(Bound::unbind)
            })
            .collect::<PyResult<Vec<_>>>()?;
        kwargs.set_item("dropped_by_kind", PyTuple::new(py, dropped.iter())?)?;
        kwargs.set_item("source_gaps", summary.source_gaps())?;
        kwargs.set_item("truncated_payloads", summary.truncated_payloads())?;

        match summary.callback_failure() {
            Some(failure) => {
                let failure_kwargs = PyDict::new(py);
                failure_kwargs.set_item("kind", failure.kind().as_str())?;
                failure_kwargs.set_item("event_sequence", failure.event_sequence())?;
                set_optional_str(
                    py,
                    &failure_kwargs,
                    "exception_type",
                    failure.exception_type(),
                )?;
                set_optional_str(py, &failure_kwargs, "message", failure.message())?;
                failure_kwargs.set_item("message_truncated", failure.message_truncated())?;
                let failure = diagnostics
                    .getattr("DiagnosticCallbackFailure")?
                    .call((), Some(&failure_kwargs))?;
                kwargs.set_item("callback_failure", failure)?;
            }
            None => kwargs.set_item("callback_failure", py.None())?,
        }
        kwargs.set_item("callback_abandoned", summary.callback_abandoned())?;
        diagnostics
            .getattr("DiagnosticSinkSummary")?
            .call((), Some(&kwargs))
            .map(Bound::unbind)
    }

    fn set_optional_u64(
        kwargs: &Bound<'_, PyDict>,
        field: &str,
        value: Option<u64>,
    ) -> PyResult<()> {
        match value {
            Some(value) => kwargs.set_item(field, value),
            None => kwargs.set_item(field, kwargs.py().None()),
        }
    }

    fn set_optional_str(
        py: Python<'_>,
        kwargs: &Bound<'_, PyDict>,
        field: &str,
        value: Option<&str>,
    ) -> PyResult<()> {
        match value {
            Some(value) => kwargs.set_item(field, value),
            None => kwargs.set_item(field, py.None()),
        }
    }
}

#[cfg(not(test))]
#[allow(unused_imports)]
pub(crate) use active::ActSinkSettlement;

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

    use super::*;

    #[derive(Default)]
    struct FakeAuthority {
        active: Arc<AtomicBool>,
        log: Arc<Mutex<Vec<&'static str>>>,
    }

    impl ActAuthorityExpiry for FakeAuthority {
        fn prepare_expiry(
            &self,
        ) -> Result<Box<dyn PreparedActAuthorityExpiry>, ActAuthorityExpiryPrepareError> {
            lock(&self.log).push("authority.prepare");
            Ok(Box::new(PreparedFakeAuthority {
                active: Arc::clone(&self.active),
                log: Arc::clone(&self.log),
            }))
        }
    }

    struct PreparedFakeAuthority {
        active: Arc<AtomicBool>,
        log: Arc<Mutex<Vec<&'static str>>>,
    }

    impl PreparedActAuthorityExpiry for PreparedFakeAuthority {
        fn commit(&mut self) {
            lock(&self.log).push("authority.commit");
            self.active.store(false, Ordering::Release);
        }

        fn rollback(&mut self) {
            lock(&self.log).push("authority.rollback");
            self.active.store(true, Ordering::Release);
        }
    }

    struct FakeSink {
        mode: AtomicU8,
        log: Arc<Mutex<Vec<&'static str>>>,
    }

    impl FakeSink {
        fn new(mode: u8, log: Arc<Mutex<Vec<&'static str>>>) -> Self {
            Self {
                mode: AtomicU8::new(mode),
                log,
            }
        }
    }

    impl ActSettlementSink for FakeSink {
        fn prepare_settlement(&self) -> Result<(), ActSettlementError> {
            lock(&self.log).push("sink.prepare");
            Ok(())
        }

        fn commit_settlement(&self) -> ActSettlementSinkCommit {
            match self.mode.load(Ordering::Acquire) {
                0 => {
                    lock(&self.log).extend(["sink.seal", "sink.retire"]);
                    ActSettlementSinkCommit::Committed
                }
                1 => {
                    lock(&self.log).push("sink.reject");
                    ActSettlementSinkCommit::Rejected(ActSettlementError::new(
                        "test.sink-rejected",
                        "test sink rejected settlement",
                    ))
                }
                2 => {
                    lock(&self.log).extend(["sink.seal", "sink.retire"]);
                    ActSettlementSinkCommit::CommittedWithFailure(ActSettlementError::new(
                        "test.sink-committed-failure",
                        "test sink failed after commit",
                    ))
                }
                mode => panic!("invalid fake sink mode {mode}"),
            }
        }
    }

    #[test]
    fn authority_only_settlement_is_a_real_transaction_path() {
        let coordinator = ActSettlementCoordinator::new();
        assert_eq!(
            coordinator
                .settle_authority_only()
                .expect_err("authority-only settlement requires a participant")
                .code(),
            "act.authority-expiry-missing"
        );

        let authority = Arc::new(FakeAuthority::default());
        authority.active.store(true, Ordering::Release);
        coordinator
            .install_authority_expiry(authority.clone())
            .expect("install fake authority");
        coordinator
            .settle_authority_only()
            .expect("settle authority without a sink");

        assert!(!authority.active.load(Ordering::Acquire));
        assert_eq!(
            *lock(&authority.log),
            ["authority.prepare", "authority.commit"]
        );
        assert_eq!(
            coordinator
                .settle_authority_only()
                .expect_err("settlement is one-shot")
                .code(),
            "act.settlement-duplicated"
        );
    }

    #[test]
    fn rejected_sink_commit_rolls_authority_back_and_allows_retry() {
        let coordinator = ActSettlementCoordinator::new();
        let authority = Arc::new(FakeAuthority::default());
        authority.active.store(true, Ordering::Release);
        coordinator
            .install_authority_expiry(authority.clone())
            .expect("install fake authority");
        let sink = FakeSink::new(1, Arc::clone(&authority.log));

        assert_eq!(
            coordinator
                .settle_with_sink(&sink)
                .expect_err("fake sink rejects first commit")
                .code(),
            "test.sink-rejected"
        );
        assert!(authority.active.load(Ordering::Acquire));
        sink.mode.store(0, Ordering::Release);
        coordinator
            .settle_with_sink(&sink)
            .expect("retry settles both participants");
        assert!(!authority.active.load(Ordering::Acquire));
        assert_eq!(
            *lock(&authority.log),
            [
                "sink.prepare",
                "authority.prepare",
                "authority.commit",
                "sink.reject",
                "authority.rollback",
                "sink.prepare",
                "authority.prepare",
                "authority.commit",
                "sink.seal",
                "sink.retire",
            ]
        );
    }

    #[test]
    fn committed_sink_failure_never_reactivates_authority() {
        let coordinator = ActSettlementCoordinator::new();
        let authority = Arc::new(FakeAuthority::default());
        authority.active.store(true, Ordering::Release);
        coordinator
            .install_authority_expiry(authority.clone())
            .expect("install fake authority");
        let sink = FakeSink::new(2, Arc::clone(&authority.log));

        assert_eq!(
            coordinator
                .settle_with_sink(&sink)
                .expect_err("post-commit failure remains visible")
                .code(),
            "test.sink-committed-failure"
        );
        assert!(!authority.active.load(Ordering::Acquire));
        assert!(!lock(&authority.log).contains(&"authority.rollback"));
        assert_eq!(
            coordinator
                .settle_with_sink(&sink)
                .expect_err("committed transaction cannot retry")
                .code(),
            "act.settlement-duplicated"
        );
    }
}
