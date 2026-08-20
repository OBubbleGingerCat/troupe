mod budget;
mod callback;
mod dispatcher;
mod queue;
mod seal;
mod shutdown;
mod summary;
mod thread;

#[allow(unused_imports)] // Re-exported for sibling runtime bindings.
pub(crate) use {
    dispatcher::{DispatchEvent, SinkDeliveryFailure},
    queue::{AdmissionClass, AdmissionOutcome},
    seal::{SinkCloseError, SinkClosePoll, SinkHandle, SinkSealError, SinkSealFacts},
    shutdown::DiagnosticSinkRuntime,
    summary::{ActOutcome, SinkDeliverySummary},
};
