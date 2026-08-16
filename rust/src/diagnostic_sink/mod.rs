mod budget;
mod callback;
mod dispatcher;
mod queue;
mod seal;
mod shutdown;
mod summary;
mod thread;

#[allow(unused_imports)] // Consumed by the B18/B16 sibling binding nodes.
pub(crate) use {
    dispatcher::{DispatchEvent, SinkDeliveryFailure},
    queue::{AdmissionClass, AdmissionOutcome},
    seal::SinkHandle,
    shutdown::DiagnosticSinkRuntime,
};
