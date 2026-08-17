use std::{
    fmt,
    future::Future,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use tokio::time::MissedTickBehavior;
use troupe_diagnostics_runtime::{
    query::reader::ReaderErrorCode,
    server::{
        query::QueryCoreFailureSignal,
        sse::replay::{ReplayErrorKind, SseCoreFailureCode, SseCoreFailureSignal},
        views::ViewCoreFailureSignal,
    },
};

use crate::{
    diagnostic_runtime::{
        bootstrap::DiagnosticCoreFailure, runtime_producer::RuntimeProducerError,
    },
    orchestration::runtime::RuntimeCore,
};

const FAILURE_POLL_INTERVAL: Duration = Duration::from_millis(1);
const RUNTIME_FAILURE_PREFIX: &str = "troupe: diagnostic runtime failed: ";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeInfrastructureFailure {
    component: &'static str,
    stage: &'static str,
    code: String,
    message: String,
}

impl RuntimeInfrastructureFailure {
    pub(crate) fn new(
        component: &'static str,
        stage: &'static str,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            component,
            stage,
            code: code.into(),
            message: message.into(),
        }
    }

    #[cfg(test)]
    pub(crate) fn code(&self) -> &str {
        &self.code
    }

    pub(crate) fn line(&self) -> String {
        format!("{RUNTIME_FAILURE_PREFIX}{self}\n")
    }

    pub(crate) fn guard(failure: DiagnosticCoreFailure) -> Self {
        Self::new(
            failure.component(),
            failure.stage(),
            failure.code(),
            failure.message(),
        )
    }

    fn producer(failure: RuntimeProducerError) -> Self {
        let code = failure.code().to_owned();
        Self::new(
            "canonical_pipeline",
            "publication",
            code,
            failure.to_string(),
        )
    }

    fn query(failure: QueryCoreFailureSignal) -> Self {
        let message = match failure.store_code() {
            Some(code) => format!(
                "active {} query failed [{}]",
                failure.endpoint().as_str(),
                code.as_str()
            ),
            None => format!("active {} query failed", failure.endpoint().as_str()),
        };
        Self::new(
            "active_query",
            failure.endpoint().as_str(),
            failure.code().as_str(),
            message,
        )
    }

    fn view(failure: ViewCoreFailureSignal) -> Self {
        let message = match failure.store_code() {
            Some(code) => format!("active View query failed [{}]", code.as_str()),
            None => "active View query execution failed".to_owned(),
        };
        Self::new("active_view", "query", failure.code().as_str(), message)
    }

    fn sse(failure: SseCoreFailureSignal) -> Option<Self> {
        Self::sse_parts(failure.code(), failure.reader_code())
    }

    fn sse_parts(code: SseCoreFailureCode, reader_code: Option<ReaderErrorCode>) -> Option<Self> {
        match code {
            SseCoreFailureCode::DriverPanicked => Some(Self::new(
                "sse",
                "driver",
                "diagnostic_sse.driver_panicked",
                "active SSE replay driver panicked",
            )),
            SseCoreFailureCode::Replay(ReplayErrorKind::Reader) => reader_code
                .map(|code| Self::new("sse", "reader", code.as_str(), "active SSE reader failed")),
            SseCoreFailureCode::Replay(_) => None,
        }
    }
}

impl fmt::Display for RuntimeInfrastructureFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}/{} [{}]: {}",
            self.component, self.stage, self.code, self.message
        )
    }
}

impl std::error::Error for RuntimeInfrastructureFailure {}

#[derive(Clone, Default)]
pub(crate) struct FirstCoreFailure {
    failure: Arc<Mutex<Option<RuntimeInfrastructureFailure>>>,
}

impl FirstCoreFailure {
    pub(crate) fn failure(&self) -> Option<RuntimeInfrastructureFailure> {
        lock(&self.failure).clone()
    }

    fn latch(&self, failure: RuntimeInfrastructureFailure) -> bool {
        let mut current = lock(&self.failure);
        if current.is_some() {
            return false;
        }
        *current = Some(failure);
        true
    }

    pub(crate) fn report_guard(&self, failure: DiagnosticCoreFailure) {
        self.latch(RuntimeInfrastructureFailure::guard(failure));
    }

    pub(crate) fn report_producer(&self, failure: RuntimeProducerError) {
        self.latch(RuntimeInfrastructureFailure::producer(failure));
    }

    pub(crate) fn report_query(&self, failure: QueryCoreFailureSignal) {
        self.latch(RuntimeInfrastructureFailure::query(failure));
    }

    pub(crate) fn report_view(&self, failure: ViewCoreFailureSignal) {
        self.latch(RuntimeInfrastructureFailure::view(failure));
    }

    pub(crate) fn report_sse(&self, failure: SseCoreFailureSignal) {
        if let Some(failure) = RuntimeInfrastructureFailure::sse(failure) {
            self.latch(failure);
        }
    }
}

pub(crate) trait RuntimeFailureProbe: Send + Sync + 'static {
    fn try_core_failure(&self) -> Option<RuntimeInfrastructureFailure>;

    fn seal_new_work(&self) -> Result<(), RuntimeInfrastructureFailure>;
}

pub(crate) struct SupervisedRun<T> {
    result: T,
    failure: Option<RuntimeInfrastructureFailure>,
}

impl<T> SupervisedRun<T> {
    pub(crate) fn into_parts(self) -> (T, Option<RuntimeInfrastructureFailure>) {
        (self.result, self.failure)
    }
}

pub(crate) async fn supervise<P, F, T>(
    probe: P,
    core: Arc<RuntimeCore>,
    operation: F,
) -> SupervisedRun<T>
where
    P: RuntimeFailureProbe,
    F: Future<Output = T> + Send,
    T: Send,
{
    tokio::pin!(operation);
    let mut poll = tokio::time::interval(FAILURE_POLL_INTERVAL);
    poll.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            result = operation.as_mut() => {
                let failure = probe.try_core_failure();
                if failure.is_some() {
                    let _ = probe.seal_new_work();
                    core.request_shutdown();
                }
                return SupervisedRun { result, failure };
            }
            _ = poll.tick() => {
                if let Some(failure) = probe.try_core_failure() {
                    let _ = probe.seal_new_work();
                    core.request_shutdown();
                    let result = operation.as_mut().await;
                    return SupervisedRun {
                        result,
                        failure: Some(failure),
                    };
                }
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[derive(Clone, Default)]
    struct FakeProbe {
        failure: FirstCoreFailure,
        sealed: Arc<AtomicBool>,
    }

    impl RuntimeFailureProbe for FakeProbe {
        fn try_core_failure(&self) -> Option<RuntimeInfrastructureFailure> {
            self.failure.failure()
        }

        fn seal_new_work(&self) -> Result<(), RuntimeInfrastructureFailure> {
            self.sealed.store(true, Ordering::Release);
            Ok(())
        }
    }

    fn failure(code: &str) -> RuntimeInfrastructureFailure {
        RuntimeInfrastructureFailure::new("test", "poll", code, "test failure")
    }

    #[test]
    fn first_core_failure_is_never_overwritten() {
        let latch = FirstCoreFailure::default();
        assert!(latch.latch(failure("first")));
        assert!(!latch.latch(failure("second")));
        assert_eq!(
            latch
                .failure()
                .expect("first failure remains latched")
                .code(),
            "first"
        );
    }

    #[test]
    fn subscriber_local_sse_failure_is_not_core_fatal() {
        assert!(
            RuntimeInfrastructureFailure::sse_parts(
                SseCoreFailureCode::Replay(ReplayErrorKind::Subscriber),
                None,
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn fatal_failure_seals_before_runtime_cancellation_and_waits_for_settlement() {
        let probe = FakeProbe::default();
        let core = Arc::new(RuntimeCore::new());
        let cancellation = core.shutdown_token();
        let sealed = Arc::clone(&probe.sealed);
        assert!(probe.failure.latch(failure("writer_failed")));

        let run = supervise(probe, Arc::clone(&core), async move {
            cancellation.cancelled().await;
            assert!(sealed.load(Ordering::Acquire));
            "settled"
        })
        .await;
        let (result, failure) = run.into_parts();

        assert_eq!(result, "settled");
        assert_eq!(
            failure.expect("fatal failure is returned").code(),
            "writer_failed"
        );
        assert!(core.shutdown_requested());
    }

    #[tokio::test]
    async fn caught_publication_failure_still_wins_after_operation_returns() {
        let probe = FakeProbe::default();
        let core = Arc::new(RuntimeCore::new());
        assert!(probe.failure.latch(failure("publication_failed")));

        let run = supervise(probe, Arc::clone(&core), async { "user caught error" }).await;
        let (result, failure) = run.into_parts();

        assert_eq!(result, "user caught error");
        assert_eq!(
            failure
                .expect("publication failure cannot be recovered")
                .code(),
            "publication_failed"
        );
        assert!(core.shutdown_requested());
    }

    #[tokio::test]
    async fn local_success_does_not_seal_or_cancel_the_runtime() {
        let probe = FakeProbe::default();
        let sealed = Arc::clone(&probe.sealed);
        let core = Arc::new(RuntimeCore::new());

        let run = supervise(probe, Arc::clone(&core), async { "completed" }).await;
        let (result, failure) = run.into_parts();

        assert_eq!(result, "completed");
        assert!(failure.is_none());
        assert!(!sealed.load(Ordering::Acquire));
        assert!(!core.shutdown_requested());
    }
}
