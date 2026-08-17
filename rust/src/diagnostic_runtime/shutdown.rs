use std::fmt;

use troupe_diagnostics_core::scalar::SchemaU64;
use troupe_diagnostics_runtime::store::writer::FinalProductionOutcome;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ShutdownMetadata {
    ended_at: String,
    outcome: FinalProductionOutcome,
}

impl ShutdownMetadata {
    pub(crate) fn new(ended_at: impl Into<String>, outcome: FinalProductionOutcome) -> Self {
        Self {
            ended_at: ended_at.into(),
            outcome,
        }
    }

    pub(crate) fn ended_at(&self) -> &str {
        &self.ended_at
    }

    pub(crate) const fn outcome(&self) -> FinalProductionOutcome {
        self.outcome
    }

    fn stream_close_reason(&self) -> &'static str {
        match self.outcome {
            FinalProductionOutcome::Completed => "production_completed",
            FinalProductionOutcome::Failed => "production_failed",
            FinalProductionOutcome::Cancelled => "production_cancelled",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CleanupFailure {
    component: &'static str,
    code: String,
    message: String,
}

impl CleanupFailure {
    pub(crate) fn new(
        component: &'static str,
        code: impl Into<String>,
        error: impl fmt::Display,
    ) -> Self {
        Self {
            component,
            code: code.into(),
            message: error.to_string(),
        }
    }

    pub(crate) const fn component(&self) -> &'static str {
        self.component
    }

    pub(crate) fn code(&self) -> &str {
        &self.code
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiagnosticShutdownError {
    failures: Vec<CleanupFailure>,
}

impl DiagnosticShutdownError {
    pub(crate) fn new(failures: Vec<CleanupFailure>) -> Self {
        assert!(!failures.is_empty(), "shutdown failure must carry a cause");
        Self { failures }
    }

    pub(crate) fn failures(&self) -> &[CleanupFailure] {
        &self.failures
    }
}

impl fmt::Display for DiagnosticShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "diagnostic shutdown failed in {} operation(s)",
            self.failures.len()
        )
    }
}

impl std::error::Error for DiagnosticShutdownError {}

pub(crate) trait OrderedShutdownResources {
    fn seal_ingress(&mut self) -> Result<(), CleanupFailure>;

    fn finalize_writer(&mut self, metadata: &ShutdownMetadata)
    -> Result<SchemaU64, CleanupFailure>;

    fn close_live_stream(
        &mut self,
        reason: &str,
        final_watermark: SchemaU64,
    ) -> Result<(), CleanupFailure>;

    fn unpublish_registry(&mut self) -> Result<(), CleanupFailure>;

    fn close_listener_and_readers(&mut self) -> Result<(), CleanupFailure>;

    fn close_writer_and_store(&mut self) -> Vec<CleanupFailure>;

    fn release_runtime_resources(&mut self);
}

pub(crate) fn run_ordered_shutdown<R: OrderedShutdownResources>(
    resources: &mut R,
    metadata: &ShutdownMetadata,
) -> Result<(), DiagnosticShutdownError> {
    let mut failures = Vec::new();
    let final_watermark = match resources.seal_ingress() {
        Ok(()) => match resources.finalize_writer(metadata) {
            Ok(watermark) => Some(watermark),
            Err(error) => {
                failures.push(error);
                None
            }
        },
        Err(error) => {
            failures.push(error);
            None
        }
    };

    if let Some(final_watermark) = final_watermark
        && let Err(error) =
            resources.close_live_stream(metadata.stream_close_reason(), final_watermark)
    {
        failures.push(error);
    }
    if let Err(error) = resources.unpublish_registry() {
        failures.push(error);
    }
    if let Err(error) = resources.close_listener_and_readers() {
        failures.push(error);
    }
    failures.extend(resources.close_writer_and_store());
    resources.release_runtime_resources();

    if failures.is_empty() {
        Ok(())
    } else {
        Err(DiagnosticShutdownError::new(failures))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingResources {
        phases: Vec<&'static str>,
        fail_finalize: bool,
        fail_stream: bool,
    }

    impl OrderedShutdownResources for RecordingResources {
        fn seal_ingress(&mut self) -> Result<(), CleanupFailure> {
            self.phases.push("seal_ingress");
            Ok(())
        }

        fn finalize_writer(
            &mut self,
            _metadata: &ShutdownMetadata,
        ) -> Result<SchemaU64, CleanupFailure> {
            self.phases.push("finalize_writer");
            if self.fail_finalize {
                Err(CleanupFailure::new("writer", "injected", "failed"))
            } else {
                Ok(SchemaU64::new(17))
            }
        }

        fn close_live_stream(
            &mut self,
            _reason: &str,
            _final_watermark: SchemaU64,
        ) -> Result<(), CleanupFailure> {
            self.phases.push("close_live_stream");
            if self.fail_stream {
                Err(CleanupFailure::new("stream", "injected", "failed"))
            } else {
                Ok(())
            }
        }

        fn unpublish_registry(&mut self) -> Result<(), CleanupFailure> {
            self.phases.push("unpublish_registry");
            Ok(())
        }

        fn close_listener_and_readers(&mut self) -> Result<(), CleanupFailure> {
            self.phases.push("close_listener_and_readers");
            Ok(())
        }

        fn close_writer_and_store(&mut self) -> Vec<CleanupFailure> {
            self.phases.push("close_writer_and_store");
            Vec::new()
        }

        fn release_runtime_resources(&mut self) {
            self.phases.push("release_runtime_resources");
        }
    }

    fn metadata() -> ShutdownMetadata {
        ShutdownMetadata::new("2026-08-16T12:00:00Z", FinalProductionOutcome::Completed)
    }

    #[test]
    fn successful_shutdown_uses_the_closed_phase_order() {
        let mut resources = RecordingResources::default();
        run_ordered_shutdown(&mut resources, &metadata()).unwrap();
        assert_eq!(
            resources.phases,
            [
                "seal_ingress",
                "finalize_writer",
                "close_live_stream",
                "unpublish_registry",
                "close_listener_and_readers",
                "close_writer_and_store",
                "release_runtime_resources",
            ]
        );
    }

    #[test]
    fn failed_finalization_skips_stream_close_but_releases_every_resource() {
        let mut resources = RecordingResources {
            fail_finalize: true,
            ..RecordingResources::default()
        };
        let error = run_ordered_shutdown(&mut resources, &metadata()).unwrap_err();
        assert_eq!(error.failures()[0].component(), "writer");
        assert_eq!(
            resources.phases,
            [
                "seal_ingress",
                "finalize_writer",
                "unpublish_registry",
                "close_listener_and_readers",
                "close_writer_and_store",
                "release_runtime_resources",
            ]
        );
    }

    #[test]
    fn stream_signal_failure_does_not_skip_durable_unpublish_or_close() {
        let mut resources = RecordingResources {
            fail_stream: true,
            ..RecordingResources::default()
        };
        let error = run_ordered_shutdown(&mut resources, &metadata()).unwrap_err();
        assert_eq!(error.failures()[0].component(), "stream");
        assert_eq!(resources.phases.last(), Some(&"release_runtime_resources"));
    }
}
