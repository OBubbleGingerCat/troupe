use std::{cell::RefCell, fmt, sync::Arc, time::Instant};

use pyo3::{
    exceptions::PySystemExit,
    prelude::*,
    types::{PyAny, PyList},
};
use troupe_diagnostics_core::{
    detail::{
        CustomNumber, DiagnosticAttributes, DiagnosticDimensions, InstantDetail,
        ProductionConstructDetail, ProductionLoadDetail, ProductionPathResolutionDetail,
        SpanStartDetail,
    },
    event::{
        CausalLink, CounterSampled, CustomCounterSampled, CustomInstantOccurred,
        CustomSpanFinished, CustomSpanStarted, DiagnosticEvent, DiagnosticEventHeader,
        DiagnosticScope, EventValidationError, InstantOccurred, SpanFinished, SpanStarted,
    },
    hub::{EventIdentity, HubAdmissionError, MandatoryDurableReserver, ProductionDiagnosticHub},
    kinds::{CounterKind, CustomSeverity, SpanOutcome},
    scalar::SchemaU64,
    time::{ElapsedNs, RunClock, TimeError},
};

use crate::{
    application::loader::{
        PrevalidatedProductionRoot, ProductionLoadError, ResolvedProductionClass,
        ResolvedProductionPath, construct_production, resolve_production_class,
        resolve_production_package,
    },
    diagnostic_runtime::bootstrap::DiagnosticRuntimeGuard,
};

const PATH_RESOLUTION_FALLBACK_ERROR: &str = "production-path-resolution-failed";
const LOAD_FALLBACK_ERROR: &str = "production-load-failed";
const CONSTRUCT_FALLBACK_ERROR: &str = "production-construct-failed";

trait DiagnosticEventAdmission: Send + Sync {
    fn admit_start(
        &self,
        elapsed_ns: ElapsedNs,
        scope: DiagnosticScope,
        detail: SpanStartDetail,
        parent_span_id: Option<SchemaU64>,
        caused_by: Vec<CausalLink>,
    ) -> Result<SchemaU64, DiagnosticProducerError>;

    fn admit_finish(
        &self,
        elapsed_ns: ElapsedNs,
        scope: DiagnosticScope,
        span_id: SchemaU64,
        outcome: SpanOutcome,
        error_code: Option<String>,
        caused_by: Vec<CausalLink>,
    ) -> Result<SchemaU64, DiagnosticProducerError>;

    fn admit_instant(
        &self,
        elapsed_ns: ElapsedNs,
        scope: DiagnosticScope,
        detail: InstantDetail,
        containing_span_id: Option<SchemaU64>,
        caused_by: Vec<CausalLink>,
    ) -> Result<SchemaU64, DiagnosticProducerError>;

    fn admit_counter(
        &self,
        elapsed_ns: ElapsedNs,
        scope: DiagnosticScope,
        counter_kind: CounterKind,
        value: SchemaU64,
        caused_by: Vec<CausalLink>,
    ) -> Result<SchemaU64, DiagnosticProducerError>;

    fn admit_custom_start(
        &self,
        elapsed_ns: ElapsedNs,
        scope: DiagnosticScope,
        name: String,
        parent_span_id: Option<SchemaU64>,
        attributes: DiagnosticAttributes,
    ) -> Result<SchemaU64, DiagnosticProducerError>;

    fn admit_custom_finish(
        &self,
        elapsed_ns: ElapsedNs,
        scope: DiagnosticScope,
        span_id: SchemaU64,
        outcome: SpanOutcome,
    ) -> Result<SchemaU64, DiagnosticProducerError>;

    fn admit_custom_instant(
        &self,
        elapsed_ns: ElapsedNs,
        scope: DiagnosticScope,
        name: String,
        containing_span_id: Option<SchemaU64>,
        severity: Option<CustomSeverity>,
        attributes: DiagnosticAttributes,
    ) -> Result<SchemaU64, DiagnosticProducerError>;

    fn admit_custom_counter(
        &self,
        elapsed_ns: ElapsedNs,
        scope: DiagnosticScope,
        name: String,
        value: CustomNumber,
        unit: Option<String>,
        dimensions: DiagnosticDimensions,
    ) -> Result<SchemaU64, DiagnosticProducerError>;
}

impl<R> DiagnosticEventAdmission for ProductionDiagnosticHub<R>
where
    R: MandatoryDurableReserver,
{
    fn admit_start(
        &self,
        elapsed_ns: ElapsedNs,
        scope: DiagnosticScope,
        detail: SpanStartDetail,
        parent_span_id: Option<SchemaU64>,
        caused_by: Vec<CausalLink>,
    ) -> Result<SchemaU64, DiagnosticProducerError> {
        let receipt = self
            .admit(
                move |identity: EventIdentity| {
                    let header = DiagnosticEventHeader::new(
                        identity.run_id(),
                        identity.sequence(),
                        elapsed_ns,
                        scope,
                        caused_by,
                    )
                    .expect("hub-assigned identity always has a nonzero sequence");
                    DiagnosticEvent::SpanStarted(SpanStarted::new(header, detail, parent_span_id))
                },
                None,
            )
            .map_err(DiagnosticProducerError::admission)?;
        Ok(receipt.accepted().identity().sequence())
    }

    fn admit_finish(
        &self,
        elapsed_ns: ElapsedNs,
        scope: DiagnosticScope,
        span_id: SchemaU64,
        outcome: SpanOutcome,
        error_code: Option<String>,
        caused_by: Vec<CausalLink>,
    ) -> Result<SchemaU64, DiagnosticProducerError> {
        let receipt = self
            .admit(
                move |identity: EventIdentity| {
                    let header = DiagnosticEventHeader::new(
                        identity.run_id(),
                        identity.sequence(),
                        elapsed_ns,
                        scope,
                        caused_by,
                    )
                    .expect("hub-assigned identity always has a nonzero sequence");
                    DiagnosticEvent::SpanFinished(SpanFinished::new(
                        header, span_id, outcome, error_code,
                    ))
                },
                None,
            )
            .map_err(DiagnosticProducerError::admission)?;
        Ok(receipt.accepted().identity().sequence())
    }

    fn admit_instant(
        &self,
        elapsed_ns: ElapsedNs,
        scope: DiagnosticScope,
        detail: InstantDetail,
        containing_span_id: Option<SchemaU64>,
        caused_by: Vec<CausalLink>,
    ) -> Result<SchemaU64, DiagnosticProducerError> {
        let receipt = self
            .admit(
                move |identity: EventIdentity| {
                    let header = DiagnosticEventHeader::new(
                        identity.run_id(),
                        identity.sequence(),
                        elapsed_ns,
                        scope,
                        caused_by,
                    )
                    .expect("hub-assigned identity always has a nonzero sequence");
                    DiagnosticEvent::InstantOccurred(InstantOccurred::new(
                        header,
                        detail,
                        containing_span_id,
                    ))
                },
                None,
            )
            .map_err(DiagnosticProducerError::admission)?;
        Ok(receipt.accepted().identity().sequence())
    }

    fn admit_counter(
        &self,
        elapsed_ns: ElapsedNs,
        scope: DiagnosticScope,
        counter_kind: CounterKind,
        value: SchemaU64,
        caused_by: Vec<CausalLink>,
    ) -> Result<SchemaU64, DiagnosticProducerError> {
        let receipt = self
            .admit(
                move |identity: EventIdentity| {
                    let header = DiagnosticEventHeader::new(
                        identity.run_id(),
                        identity.sequence(),
                        elapsed_ns,
                        scope,
                        caused_by,
                    )
                    .expect("hub-assigned identity always has a nonzero sequence");
                    DiagnosticEvent::CounterSampled(CounterSampled::new(
                        header,
                        counter_kind,
                        value,
                    ))
                },
                None,
            )
            .map_err(DiagnosticProducerError::admission)?;
        Ok(receipt.accepted().identity().sequence())
    }

    fn admit_custom_start(
        &self,
        elapsed_ns: ElapsedNs,
        scope: DiagnosticScope,
        name: String,
        parent_span_id: Option<SchemaU64>,
        attributes: DiagnosticAttributes,
    ) -> Result<SchemaU64, DiagnosticProducerError> {
        let receipt = self
            .admit(
                move |identity: EventIdentity| {
                    let header = DiagnosticEventHeader::new(
                        identity.run_id(),
                        identity.sequence(),
                        elapsed_ns,
                        scope,
                        Vec::new(),
                    )
                    .expect("hub-assigned identity always has a nonzero sequence");
                    DiagnosticEvent::CustomSpanStarted(
                        CustomSpanStarted::new(header, name, parent_span_id, attributes)
                            .expect("custom span fields were validated before admission"),
                    )
                },
                None,
            )
            .map_err(DiagnosticProducerError::admission)?;
        Ok(receipt.accepted().identity().sequence())
    }

    fn admit_custom_finish(
        &self,
        elapsed_ns: ElapsedNs,
        scope: DiagnosticScope,
        span_id: SchemaU64,
        outcome: SpanOutcome,
    ) -> Result<SchemaU64, DiagnosticProducerError> {
        let receipt = self
            .admit(
                move |identity: EventIdentity| {
                    let header = DiagnosticEventHeader::new(
                        identity.run_id(),
                        identity.sequence(),
                        elapsed_ns,
                        scope,
                        Vec::new(),
                    )
                    .expect("hub-assigned identity always has a nonzero sequence");
                    DiagnosticEvent::CustomSpanFinished(CustomSpanFinished::new(
                        header, span_id, outcome,
                    ))
                },
                None,
            )
            .map_err(DiagnosticProducerError::admission)?;
        Ok(receipt.accepted().identity().sequence())
    }

    fn admit_custom_instant(
        &self,
        elapsed_ns: ElapsedNs,
        scope: DiagnosticScope,
        name: String,
        containing_span_id: Option<SchemaU64>,
        severity: Option<CustomSeverity>,
        attributes: DiagnosticAttributes,
    ) -> Result<SchemaU64, DiagnosticProducerError> {
        let receipt = self
            .admit(
                move |identity: EventIdentity| {
                    let header = DiagnosticEventHeader::new(
                        identity.run_id(),
                        identity.sequence(),
                        elapsed_ns,
                        scope,
                        Vec::new(),
                    )
                    .expect("hub-assigned identity always has a nonzero sequence");
                    DiagnosticEvent::CustomInstantOccurred(
                        CustomInstantOccurred::new(
                            header,
                            name,
                            containing_span_id,
                            severity,
                            attributes,
                        )
                        .expect("custom instant fields were validated before admission"),
                    )
                },
                None,
            )
            .map_err(DiagnosticProducerError::admission)?;
        Ok(receipt.accepted().identity().sequence())
    }

    fn admit_custom_counter(
        &self,
        elapsed_ns: ElapsedNs,
        scope: DiagnosticScope,
        name: String,
        value: CustomNumber,
        unit: Option<String>,
        dimensions: DiagnosticDimensions,
    ) -> Result<SchemaU64, DiagnosticProducerError> {
        let receipt = self
            .admit(
                move |identity: EventIdentity| {
                    let header = DiagnosticEventHeader::new(
                        identity.run_id(),
                        identity.sequence(),
                        elapsed_ns,
                        scope,
                        Vec::new(),
                    )
                    .expect("hub-assigned identity always has a nonzero sequence");
                    DiagnosticEvent::CustomCounterSampled(
                        CustomCounterSampled::new(header, name, value, unit, dimensions)
                            .expect("custom counter fields were validated before admission"),
                    )
                },
                None,
            )
            .map_err(DiagnosticProducerError::admission)?;
        Ok(receipt.accepted().identity().sequence())
    }
}

#[derive(Clone)]
pub(crate) struct DiagnosticRunContext {
    admission: Arc<dyn DiagnosticEventAdmission>,
    clock: RunClock,
}

impl DiagnosticRunContext {
    fn ready(runtime: &DiagnosticRuntimeGuard) -> Self {
        let hub = Arc::clone(runtime.hub());
        let admission: Arc<dyn DiagnosticEventAdmission> = hub;
        Self {
            admission,
            clock: RunClock::from_origin(Instant::now()),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_hub<R>(hub: Arc<ProductionDiagnosticHub<R>>, clock: RunClock) -> Self
    where
        R: MandatoryDurableReserver + 'static,
    {
        let admission: Arc<dyn DiagnosticEventAdmission> = hub;
        Self { admission, clock }
    }

    pub(crate) const fn clock(&self) -> RunClock {
        self.clock
    }

    pub(crate) fn start_span(
        &self,
        scope: DiagnosticScope,
        detail: SpanStartDetail,
        parent_span_id: Option<SchemaU64>,
    ) -> Result<SchemaU64, DiagnosticProducerError> {
        self.start_span_with_causes(scope, detail, parent_span_id, Vec::new())
    }

    pub(crate) fn start_span_with_causes(
        &self,
        scope: DiagnosticScope,
        detail: SpanStartDetail,
        parent_span_id: Option<SchemaU64>,
        caused_by: Vec<CausalLink>,
    ) -> Result<SchemaU64, DiagnosticProducerError> {
        let elapsed_ns = self
            .clock
            .elapsed_now()
            .map_err(DiagnosticProducerError::clock)?;
        self.admission
            .admit_start(elapsed_ns, scope, detail, parent_span_id, caused_by)
    }

    pub(crate) fn finish_span(
        &self,
        scope: DiagnosticScope,
        span_id: SchemaU64,
        outcome: SpanOutcome,
        error_code: Option<String>,
    ) -> Result<(), DiagnosticProducerError> {
        self.finish_span_with_causes(scope, span_id, outcome, error_code, Vec::new())
            .map(|_| ())
    }

    pub(crate) fn finish_span_with_causes(
        &self,
        scope: DiagnosticScope,
        span_id: SchemaU64,
        outcome: SpanOutcome,
        error_code: Option<String>,
        caused_by: Vec<CausalLink>,
    ) -> Result<SchemaU64, DiagnosticProducerError> {
        let elapsed_ns = self
            .clock
            .elapsed_now()
            .map_err(DiagnosticProducerError::clock)?;
        self.admission
            .admit_finish(elapsed_ns, scope, span_id, outcome, error_code, caused_by)
    }

    pub(crate) fn emit_instant(
        &self,
        scope: DiagnosticScope,
        detail: InstantDetail,
        containing_span_id: Option<SchemaU64>,
    ) -> Result<(), DiagnosticProducerError> {
        self.emit_instant_with_causes(scope, detail, containing_span_id, Vec::new())
            .map(|_| ())
    }

    pub(crate) fn emit_instant_with_causes(
        &self,
        scope: DiagnosticScope,
        detail: InstantDetail,
        containing_span_id: Option<SchemaU64>,
        caused_by: Vec<CausalLink>,
    ) -> Result<SchemaU64, DiagnosticProducerError> {
        let elapsed_ns = self
            .clock
            .elapsed_now()
            .map_err(DiagnosticProducerError::clock)?;
        self.admission
            .admit_instant(elapsed_ns, scope, detail, containing_span_id, caused_by)
    }

    pub(crate) fn emit_counter(
        &self,
        scope: DiagnosticScope,
        counter_kind: CounterKind,
        value: SchemaU64,
        caused_by: Vec<CausalLink>,
    ) -> Result<SchemaU64, DiagnosticProducerError> {
        let elapsed_ns = self
            .clock
            .elapsed_now()
            .map_err(DiagnosticProducerError::clock)?;
        self.admission
            .admit_counter(elapsed_ns, scope, counter_kind, value, caused_by)
    }

    pub(crate) fn start_custom_span(
        &self,
        scope: DiagnosticScope,
        name: String,
        parent_span_id: Option<SchemaU64>,
        attributes: DiagnosticAttributes,
    ) -> Result<SchemaU64, DiagnosticProducerError> {
        CustomSpanStarted::validate_fields(&name, &attributes)
            .map_err(DiagnosticProducerError::event)?;
        let elapsed_ns = self
            .clock
            .elapsed_now()
            .map_err(DiagnosticProducerError::clock)?;
        self.admission
            .admit_custom_start(elapsed_ns, scope, name, parent_span_id, attributes)
    }

    pub(crate) fn finish_custom_span(
        &self,
        scope: DiagnosticScope,
        span_id: SchemaU64,
        outcome: SpanOutcome,
    ) -> Result<(), DiagnosticProducerError> {
        let elapsed_ns = self
            .clock
            .elapsed_now()
            .map_err(DiagnosticProducerError::clock)?;
        self.admission
            .admit_custom_finish(elapsed_ns, scope, span_id, outcome)
            .map(|_| ())
    }

    pub(crate) fn emit_custom_instant(
        &self,
        scope: DiagnosticScope,
        name: String,
        containing_span_id: Option<SchemaU64>,
        severity: Option<CustomSeverity>,
        attributes: DiagnosticAttributes,
    ) -> Result<(), DiagnosticProducerError> {
        CustomInstantOccurred::validate_fields(&name, &attributes)
            .map_err(DiagnosticProducerError::event)?;
        let elapsed_ns = self
            .clock
            .elapsed_now()
            .map_err(DiagnosticProducerError::clock)?;
        self.admission
            .admit_custom_instant(
                elapsed_ns,
                scope,
                name,
                containing_span_id,
                severity,
                attributes,
            )
            .map(|_| ())
    }

    pub(crate) fn emit_custom_counter(
        &self,
        scope: DiagnosticScope,
        name: String,
        value: CustomNumber,
        unit: Option<String>,
        dimensions: DiagnosticDimensions,
    ) -> Result<(), DiagnosticProducerError> {
        CustomCounterSampled::validate_fields(&name, unit.as_deref(), &dimensions)
            .map_err(DiagnosticProducerError::event)?;
        let elapsed_ns = self
            .clock
            .elapsed_now()
            .map_err(DiagnosticProducerError::clock)?;
        self.admission
            .admit_custom_counter(elapsed_ns, scope, name, value, unit, dimensions)
            .map(|_| ())
    }
}

#[derive(Clone)]
pub(crate) struct ProductionConstructionSnapshot {
    context: DiagnosticRunContext,
    construct_span_id: SchemaU64,
}

impl ProductionConstructionSnapshot {
    pub(crate) fn context(&self) -> DiagnosticRunContext {
        self.context.clone()
    }

    pub(crate) const fn construct_span_id(&self) -> SchemaU64 {
        self.construct_span_id
    }
}

thread_local! {
    static PRODUCTION_CONSTRUCTION_STACK: RefCell<Vec<ProductionConstructionSnapshot>> =
        const { RefCell::new(Vec::new()) };
}

pub(crate) fn current_production_construction() -> Option<ProductionConstructionSnapshot> {
    PRODUCTION_CONSTRUCTION_STACK.with(|stack| stack.borrow().last().cloned())
}

struct ProductionConstructionGuard {
    context: DiagnosticRunContext,
    construct_span_id: SchemaU64,
}

impl ProductionConstructionGuard {
    fn enter(context: DiagnosticRunContext, construct_span_id: SchemaU64) -> Self {
        PRODUCTION_CONSTRUCTION_STACK.with(|stack| {
            stack.borrow_mut().push(ProductionConstructionSnapshot {
                context: context.clone(),
                construct_span_id,
            });
        });
        Self {
            context,
            construct_span_id,
        }
    }
}

impl Drop for ProductionConstructionGuard {
    fn drop(&mut self) {
        PRODUCTION_CONSTRUCTION_STACK.with(|stack| {
            let popped = stack
                .borrow_mut()
                .pop()
                .expect("Production construction context must remain installed");
            assert!(
                popped.construct_span_id == self.construct_span_id
                    && Arc::ptr_eq(&popped.context.admission, &self.context.admission),
                "Production construction contexts must leave in LIFO order"
            );
        });
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiagnosticProducerError {
    code: String,
    message: String,
}

impl DiagnosticProducerError {
    fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    fn clock(error: TimeError) -> Self {
        Self::new("diagnostic.elapsed-unavailable", error.to_string())
    }

    fn event(error: EventValidationError) -> Self {
        Self::new("diagnostic.custom-event-invalid", error.to_string())
    }

    fn admission<E>(error: HubAdmissionError<E>) -> Self
    where
        E: std::error::Error,
    {
        let code = match &error {
            HubAdmissionError::StatePoisoned => "diagnostic.hub-state-poisoned".to_owned(),
            HubAdmissionError::SequenceExhausted => "diagnostic.sequence-exhausted".to_owned(),
            HubAdmissionError::CandidateIdentityMismatch { .. } => {
                "diagnostic.candidate-identity-mismatch".to_owned()
            }
            HubAdmissionError::CanonicalEncoding => {
                "diagnostic.canonical-encoding-failed".to_owned()
            }
            HubAdmissionError::Reference(reference) => {
                format!("diagnostic.reference-{}", reference.code().as_str())
            }
            HubAdmissionError::Reservation(_) => "diagnostic.admission-failed".to_owned(),
        };
        Self::new(code, error.to_string())
    }

    pub(crate) fn code(&self) -> &str {
        &self.code
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for DiagnosticProducerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "diagnostic producer failed [{}]: {}",
            self.code, self.message
        )
    }
}

impl std::error::Error for DiagnosticProducerError {}

#[derive(Debug)]
pub(crate) enum LoadProducerError {
    Python {
        error: PyErr,
        diagnostic_finish_error: Option<DiagnosticProducerError>,
    },
    Diagnostic {
        error: DiagnosticProducerError,
        cleanup_error: Option<PyErr>,
    },
}

impl LoadProducerError {
    fn diagnostic(error: DiagnosticProducerError) -> Self {
        Self::Diagnostic {
            error,
            cleanup_error: None,
        }
    }

    fn python(error: PyErr, diagnostic_finish_error: Option<DiagnosticProducerError>) -> Self {
        Self::Python {
            error,
            diagnostic_finish_error,
        }
    }

    pub(crate) fn python_error(&self) -> Option<&PyErr> {
        match self {
            Self::Python { error, .. } => Some(error),
            Self::Diagnostic { .. } => None,
        }
    }

    pub(crate) fn diagnostic_error(&self) -> Option<&DiagnosticProducerError> {
        match self {
            Self::Python {
                diagnostic_finish_error,
                ..
            } => diagnostic_finish_error.as_ref(),
            Self::Diagnostic { error, .. } => Some(error),
        }
    }

    pub(crate) fn cleanup_error(&self) -> Option<&PyErr> {
        match self {
            Self::Diagnostic { cleanup_error, .. } => cleanup_error.as_ref(),
            Self::Python { .. } => None,
        }
    }
}

impl fmt::Display for LoadProducerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Python {
                diagnostic_finish_error: Some(diagnostic),
                ..
            } => write!(
                formatter,
                "Production loading failed and its diagnostic finish failed: {diagnostic}"
            ),
            Self::Python { .. } => formatter.write_str("Production loading failed"),
            Self::Diagnostic {
                error,
                cleanup_error: Some(_),
            } => write!(
                formatter,
                "{error}; transactional Production cleanup also failed"
            ),
            Self::Diagnostic { error, .. } => fmt::Display::fmt(error, formatter),
        }
    }
}

pub(crate) struct ObservedProductionPath {
    resolved: ResolvedProductionPath,
    package: String,
}

pub(crate) struct ObservedProductionClass {
    resolved: ResolvedProductionClass,
    package: String,
}

impl ObservedProductionClass {
    pub(crate) fn inspect_static_attribute(
        &self,
        py: Python<'_>,
        name: &str,
    ) -> PyResult<Option<Py<PyAny>>> {
        self.resolved.inspect_static_attribute(py, name)
    }

    pub(crate) fn rollback(self, py: Python<'_>) -> PyResult<()> {
        self.resolved.rollback(py)
    }
}

pub(crate) struct ProductionLoadProducer {
    context: DiagnosticRunContext,
    production_root: String,
}

impl ProductionLoadProducer {
    pub(crate) fn new(runtime: &DiagnosticRuntimeGuard) -> Result<Self, DiagnosticProducerError> {
        let production_root = runtime
            .layout()
            .production_root()
            .to_str()
            .ok_or_else(|| {
                DiagnosticProducerError::new(
                    "diagnostic.production-root-not-utf8",
                    "ready Production root cannot be represented in canonical diagnostic detail",
                )
            })?
            .to_owned();
        Ok(Self {
            context: DiagnosticRunContext::ready(runtime),
            production_root,
        })
    }

    #[cfg(test)]
    fn with_context(context: DiagnosticRunContext, production_root: String) -> Self {
        Self {
            context,
            production_root,
        }
    }

    pub(crate) fn context(&self) -> DiagnosticRunContext {
        self.context.clone()
    }

    pub(crate) fn resolve_path(
        &self,
        py: Python<'_>,
        root: PrevalidatedProductionRoot,
    ) -> Result<ObservedProductionPath, LoadProducerError> {
        let package = root.package_candidate().to_owned();
        let scope = empty_scope();
        let span_id = self
            .context
            .start_span(
                scope.clone(),
                SpanStartDetail::ProductionPathResolution(ProductionPathResolutionDetail::new(
                    self.production_root.clone(),
                    package.clone(),
                )),
                None,
            )
            .map_err(LoadProducerError::diagnostic)?;

        match resolve_production_package(py, root) {
            Ok(resolved) => {
                self.context
                    .finish_span(scope, span_id, SpanOutcome::Completed, None)
                    .map_err(LoadProducerError::diagnostic)?;
                Ok(ObservedProductionPath { resolved, package })
            }
            Err(error) => {
                Err(self.python_failure(py, scope, span_id, error, PATH_RESOLUTION_FALLBACK_ERROR))
            }
        }
    }

    pub(crate) fn resolve_class(
        &self,
        py: Python<'_>,
        path: ObservedProductionPath,
    ) -> Result<ObservedProductionClass, LoadProducerError> {
        let ObservedProductionPath { resolved, package } = path;
        let scope = empty_scope();
        let span_id = self
            .context
            .start_span(
                scope.clone(),
                SpanStartDetail::ProductionLoad(ProductionLoadDetail::new(package.clone())),
                None,
            )
            .map_err(LoadProducerError::diagnostic)?;

        match resolve_production_class(py, resolved) {
            Ok(resolved) => {
                if let Err(error) =
                    self.context
                        .finish_span(scope, span_id, SpanOutcome::Completed, None)
                {
                    let cleanup_error = resolved.rollback(py).err();
                    return Err(LoadProducerError::Diagnostic {
                        error,
                        cleanup_error,
                    });
                }
                Ok(ObservedProductionClass { resolved, package })
            }
            Err(error) => Err(self.python_failure(py, scope, span_id, error, LOAD_FALLBACK_ERROR)),
        }
    }

    pub(crate) fn construct(
        &self,
        py: Python<'_>,
        resolved: ObservedProductionClass,
        args: &Bound<'_, PyList>,
    ) -> Result<Py<PyAny>, LoadProducerError> {
        let ObservedProductionClass { resolved, package } = resolved;
        let scope = empty_scope();
        let span_id = match self.context.start_span(
            scope.clone(),
            SpanStartDetail::ProductionConstruct(ProductionConstructDetail::new(
                package,
                "Production".to_owned(),
            )),
            None,
        ) {
            Ok(span_id) => span_id,
            Err(error) => {
                let cleanup_error = resolved.rollback(py).err();
                return Err(LoadProducerError::Diagnostic {
                    error,
                    cleanup_error,
                });
            }
        };

        let construction = ProductionConstructionGuard::enter(self.context.clone(), span_id);
        let result = construct_production(py, resolved, args);
        drop(construction);

        match result {
            Ok(production) => {
                if let Err(error) =
                    self.context
                        .finish_span(scope, span_id, SpanOutcome::Completed, None)
                {
                    drop(production);
                    return Err(LoadProducerError::diagnostic(error));
                }
                Ok(production)
            }
            Err(error) => {
                Err(self.python_failure(py, scope, span_id, error, CONSTRUCT_FALLBACK_ERROR))
            }
        }
    }

    fn python_failure(
        &self,
        py: Python<'_>,
        scope: DiagnosticScope,
        span_id: SchemaU64,
        error: PyErr,
        fallback_error_code: &'static str,
    ) -> LoadProducerError {
        let error_code = normalized_python_error_code(py, &error, fallback_error_code);
        let diagnostic_finish_error = self
            .context
            .finish_span(scope, span_id, SpanOutcome::Failed, Some(error_code))
            .err();
        LoadProducerError::python(error, diagnostic_finish_error)
    }
}

fn empty_scope() -> DiagnosticScope {
    DiagnosticScope::new(None, None, None, None, None, None, None)
}

fn normalized_python_error_code(py: Python<'_>, error: &PyErr, fallback: &'static str) -> String {
    if error.is_instance_of::<ProductionLoadError>(py) {
        return error
            .value(py)
            .getattr("reason")
            .and_then(|reason| reason.extract::<String>())
            .unwrap_or_else(|_| fallback.to_owned());
    }
    if error.is_instance_of::<PySystemExit>(py) {
        "system-exit".to_owned()
    } else {
        fallback.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
        time::Instant,
    };

    use pyo3::{
        exceptions::{PyRuntimeError, PySystemExit},
        types::{PyDict, PyDictMethods, PyList, PyListMethods, PyString},
    };
    use troupe_diagnostics_core::{
        detail::{CanonicalInteger, CustomNumber, EmptyDetail, SpanStartDetail},
        event::{CausalLink, DiagnosticEvent},
        hub::{
            AcceptedDiagnosticEvent, AdmissionReservation, AdmissionReserver, AdmissionSize,
            DeliveryFailure, LiveEventNotifier, MandatoryDurableReserver, ProductionDiagnosticHub,
        },
        id::CanonicalUuid,
        kinds::{CausalRelation, CounterKind, CustomSeverity, SpanOutcome},
        time::RunClock,
    };
    use uuid::Uuid;

    use crate::{
        application::loader::{ProductionLoadError, prevalidate_production_root},
        orchestration::production::Production,
    };

    use super::*;

    const BASE_NAME: &str = "_troupe_b09_base";
    const CALLS_NAME: &str = "_troupe_b09_calls";
    const EXIT_NAME: &str = "_troupe_b09_exit";

    #[derive(Clone, Default)]
    struct EventLog(Arc<Mutex<Vec<AcceptedDiagnosticEvent>>>);

    impl EventLog {
        fn push(&self, event: AcceptedDiagnosticEvent) {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        }

        fn events(&self) -> Vec<AcceptedDiagnosticEvent> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    struct RecordingReservation(EventLog);

    impl AdmissionReservation for RecordingReservation {
        fn commit(self, event: AcceptedDiagnosticEvent) {
            self.0.push(event);
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct InjectedAdmissionError;

    impl fmt::Display for InjectedAdmissionError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("injected diagnostic admission failure")
        }
    }

    impl std::error::Error for InjectedAdmissionError {}

    struct RecordingReserver {
        log: EventLog,
        attempts: usize,
        fail_on_attempt: Option<usize>,
    }

    impl AdmissionReserver for RecordingReserver {
        type Error = InjectedAdmissionError;
        type Reservation = RecordingReservation;

        fn try_reserve(&mut self, _size: AdmissionSize) -> Result<Self::Reservation, Self::Error> {
            self.attempts += 1;
            if self.fail_on_attempt == Some(self.attempts) {
                return Err(InjectedAdmissionError);
            }
            Ok(RecordingReservation(self.log.clone()))
        }
    }

    impl MandatoryDurableReserver for RecordingReserver {}

    struct IgnoreLive;

    impl LiveEventNotifier for IgnoreLive {
        fn notify(&mut self, _event: AcceptedDiagnosticEvent) -> Result<(), DeliveryFailure> {
            Ok(())
        }
    }

    struct TestPackage {
        parent: PathBuf,
        root: PathBuf,
    }

    impl TestPackage {
        fn new(name: &str, production_source: &str) -> Self {
            let parent = std::env::temp_dir().join(format!("troupe-b09-{}", Uuid::new_v4()));
            let root = parent.join(name);
            fs::create_dir_all(&root).expect("create test Production package");
            fs::write(root.join("__init__.py"), b"").expect("write test package init");
            fs::write(root.join("production.py"), production_source)
                .expect("write test Production module");
            Self { parent, root }
        }

        fn root(&self) -> &Path {
            &self.root
        }

        fn package(&self) -> &str {
            self.root
                .file_name()
                .and_then(|name| name.to_str())
                .expect("test package name is UTF-8")
        }
    }

    impl Drop for TestPackage {
        fn drop(&mut self) {
            if self.parent.exists() {
                fs::remove_dir_all(&self.parent).expect("remove test Production package");
            }
        }
    }

    fn make_producer(
        production_root: &Path,
        fail_on_attempt: Option<usize>,
    ) -> (ProductionLoadProducer, EventLog) {
        let log = EventLog::default();
        let hub = Arc::new(ProductionDiagnosticHub::production(
            CanonicalUuid::new(Uuid::new_v4()),
            RecordingReserver {
                log: log.clone(),
                attempts: 0,
                fail_on_attempt,
            },
            Box::new(IgnoreLive),
        ));
        let context = DiagnosticRunContext::with_hub(hub, RunClock::from_origin(Instant::now()));
        (
            ProductionLoadProducer::with_context(
                context,
                production_root
                    .to_str()
                    .expect("test Production root is UTF-8")
                    .to_owned(),
            ),
            log,
        )
    }

    fn prevalidate(py: Python<'_>, package: &TestPackage) -> PyResult<PrevalidatedProductionRoot> {
        prevalidate_production_root(
            py,
            &PyString::new(
                py,
                package
                    .root()
                    .to_str()
                    .expect("test Production root is UTF-8"),
            ),
        )
    }

    fn install_builtins(py: Python<'_>) -> PyResult<Py<PyList>> {
        let builtins = py.import("builtins")?;
        let calls = PyList::empty(py);
        builtins.setattr(BASE_NAME, py.get_type::<Production>())?;
        builtins.setattr(CALLS_NAME, &calls)?;
        Ok(calls.unbind())
    }

    fn cleanup_python(py: Python<'_>, package: &TestPackage) {
        let modules = py
            .import("sys")
            .and_then(|sys| sys.getattr("modules"))
            .expect("read sys.modules");
        let modules = modules.cast_into::<PyDict>().expect("cast sys.modules");
        let _ = modules.del_item(format!("{}.production", package.package()));
        let _ = modules.del_item(package.package());

        let builtins = py.import("builtins").expect("import builtins");
        let _ = builtins.delattr(BASE_NAME);
        let _ = builtins.delattr(CALLS_NAME);
        let _ = builtins.delattr(EXIT_NAME);
    }

    fn python_reason(py: Python<'_>, error: &LoadProducerError) -> String {
        let error = error.python_error().expect("retain original Python error");
        assert!(error.is_instance_of::<ProductionLoadError>(py));
        error
            .value(py)
            .getattr("reason")
            .and_then(|reason| reason.extract::<String>())
            .expect("ProductionLoadError carries a stable reason")
    }

    fn assert_finish(
        event: &AcceptedDiagnosticEvent,
        sequence: u64,
        span_id: u64,
        outcome: SpanOutcome,
        error_code: Option<&str>,
    ) {
        assert_eq!(event.identity().sequence().get(), sequence);
        let DiagnosticEvent::SpanFinished(finish) = event.event() else {
            panic!("expected span finish event")
        };
        assert_eq!(finish.span_id().get(), span_id);
        assert_eq!(finish.outcome(), outcome);
        assert_eq!(finish.error_code(), error_code);
    }

    #[test]
    fn real_loader_phases_emit_typed_start_finish_pairs_around_construction() {
        let _python_test_guard = crate::initialize_python_for_test();
        let package = TestPackage::new(
            "diagnostic_load_success",
            "import builtins\nclass Production(builtins._troupe_b09_base):\n    diagnostic_views = ('timeline',)\n    def __init__(self, args):\n        builtins._troupe_b09_calls.append(list(args))\n",
        );
        let (producer, log) = make_producer(package.root(), None);

        Python::attach(|py| {
            let calls = install_builtins(py)?;
            let path = producer
                .resolve_path(py, prevalidate(py, &package)?)
                .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
            assert!(calls.bind(py).is_empty());
            let class = producer
                .resolve_class(py, path)
                .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
            assert!(calls.bind(py).is_empty());
            assert!(
                class
                    .inspect_static_attribute(py, "diagnostic_views")?
                    .is_some()
            );
            let args = PyList::new(py, ["first", "second"])?;
            let production = producer
                .construct(py, class, &args)
                .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
            assert_eq!(calls.bind(py).len(), 1);
            drop(production);
            cleanup_python(py, &package);
            Ok::<_, PyErr>(())
        })
        .expect("load Production through observed phases");

        let events = log.events();
        assert_eq!(events.len(), 6);
        let expected_spans = [1_u64, 3, 5];
        for (index, span_id) in expected_spans.into_iter().enumerate() {
            let start = &events[index * 2];
            assert_eq!(start.identity().sequence().get(), span_id);
            assert_finish(
                &events[index * 2 + 1],
                span_id + 1,
                span_id,
                SpanOutcome::Completed,
                None,
            );
            assert!(
                start.event().header().elapsed_ns().get()
                    <= events[index * 2 + 1].event().header().elapsed_ns().get()
            );
        }

        let DiagnosticEvent::SpanStarted(path) = events[0].event() else {
            panic!("expected path span start")
        };
        let SpanStartDetail::ProductionPathResolution(detail) = path.detail() else {
            panic!("expected typed path detail")
        };
        assert_eq!(detail.production_root(), package.root().to_str().unwrap());
        assert_eq!(detail.package(), package.package());
        assert_eq!(path.parent_span_id(), None);

        let DiagnosticEvent::SpanStarted(load) = events[2].event() else {
            panic!("expected load span start")
        };
        let SpanStartDetail::ProductionLoad(detail) = load.detail() else {
            panic!("expected typed load detail")
        };
        assert_eq!(detail.package(), package.package());

        let DiagnosticEvent::SpanStarted(construct) = events[4].event() else {
            panic!("expected construct span start")
        };
        let SpanStartDetail::ProductionConstruct(detail) = construct.detail() else {
            panic!("expected typed construct detail")
        };
        assert_eq!(detail.package(), package.package());
        assert_eq!(detail.class_name(), "Production");
    }

    #[test]
    fn user_failures_keep_public_python_errors_and_emit_stable_codes() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let invalid = TestPackage::new(
                "invalid-package-name",
                "raise RuntimeError('must not import')\n",
            );
            let (producer, log) = make_producer(invalid.root(), None);
            let error = producer
                .resolve_path(py, prevalidate(py, &invalid)?)
                .err()
                .expect("invalid package must fail");
            assert_eq!(python_reason(py, &error), "invalid-package-name");
            assert!(error.diagnostic_error().is_none());
            assert_finish(
                &log.events()[1],
                2,
                1,
                SpanOutcome::Failed,
                Some("invalid-package-name"),
            );
            cleanup_python(py, &invalid);

            let import_failure = TestPackage::new(
                "diagnostic_import_failure",
                "raise RuntimeError('secret import payload')\n",
            );
            let (producer, log) = make_producer(import_failure.root(), None);
            let path = producer
                .resolve_path(py, prevalidate(py, &import_failure)?)
                .expect("resolve valid package path");
            let error = producer
                .resolve_class(py, path)
                .err()
                .expect("import must fail");
            assert_eq!(python_reason(py, &error), "import-failed");
            assert_finish(
                &log.events()[3],
                4,
                3,
                SpanOutcome::Failed,
                Some("import-failed"),
            );
            assert!(log.events().iter().all(|event| {
                !String::from_utf8_lossy(event.canonical_bytes()).contains("secret import payload")
            }));
            cleanup_python(py, &import_failure);

            let construct_failure = TestPackage::new(
                "diagnostic_construct_failure",
                "import builtins\nclass Production(builtins._troupe_b09_base):\n    def __init__(self, args):\n        builtins._troupe_b09_calls.append(list(args))\n        raise RuntimeError('secret constructor payload')\n",
            );
            let calls = install_builtins(py)?;
            let (producer, log) = make_producer(construct_failure.root(), None);
            let path = producer
                .resolve_path(py, prevalidate(py, &construct_failure)?)
                .expect("resolve valid package path");
            let class = producer
                .resolve_class(py, path)
                .expect("resolve valid Production class");
            let error = producer
                .construct(py, class, &PyList::empty(py))
                .expect_err("constructor must fail");
            assert_eq!(python_reason(py, &error), "construction-failed");
            assert_eq!(calls.bind(py).len(), 1);
            assert_finish(
                &log.events()[5],
                6,
                5,
                SpanOutcome::Failed,
                Some("construction-failed"),
            );
            assert!(log.events().iter().all(|event| {
                !String::from_utf8_lossy(event.canonical_bytes())
                    .contains("secret constructor payload")
            }));
            cleanup_python(py, &construct_failure);
            Ok::<_, PyErr>(())
        })
        .expect("observe stable user failure outcomes");
    }

    #[test]
    fn constructor_system_exit_is_not_wrapped_or_replaced() {
        let _python_test_guard = crate::initialize_python_for_test();
        let package = TestPackage::new(
            "diagnostic_system_exit",
            "import builtins\nclass Production(builtins._troupe_b09_base):\n    def __init__(self, args):\n        raise builtins._troupe_b09_exit\n",
        );
        let (producer, log) = make_producer(package.root(), None);

        Python::attach(|py| {
            install_builtins(py)?;
            let exit = PySystemExit::new_err(23);
            let expected = exit.value(py).clone().unbind();
            py.import("builtins")?.setattr(EXIT_NAME, exit.value(py))?;
            let path = producer
                .resolve_path(py, prevalidate(py, &package)?)
                .expect("resolve valid package path");
            let class = producer
                .resolve_class(py, path)
                .expect("resolve valid Production class");
            let error = producer
                .construct(py, class, &PyList::empty(py))
                .expect_err("SystemExit constructor must fail");
            let python_error = error.python_error().expect("retain SystemExit");
            assert!(python_error.value(py).is(expected.bind(py)));
            assert_finish(
                &log.events()[5],
                6,
                5,
                SpanOutcome::Failed,
                Some("system-exit"),
            );
            cleanup_python(py, &package);
            Ok::<_, PyErr>(())
        })
        .expect("preserve constructor SystemExit");
    }

    #[test]
    fn admission_failures_gate_operations_and_retain_dual_failure_state() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let package = TestPackage::new(
                "diagnostic_start_admission_failure",
                "import builtins\nclass Production(builtins._troupe_b09_base):\n    def __init__(self, args):\n        builtins._troupe_b09_calls.append(list(args))\n",
            );
            let calls = install_builtins(py)?;
            let (producer, log) = make_producer(package.root(), Some(5));
            let path = producer
                .resolve_path(py, prevalidate(py, &package)?)
                .expect("resolve valid package path");
            let class = producer
                .resolve_class(py, path)
                .expect("resolve valid Production class");
            let error = producer
                .construct(py, class, &PyList::empty(py))
                .expect_err("construct start admission must fail");
            assert!(error.python_error().is_none());
            assert_eq!(
                error.diagnostic_error().expect("diagnostic failure").code(),
                "diagnostic.admission-failed"
            );
            assert!(error.cleanup_error().is_none());
            assert!(calls.bind(py).is_empty());
            assert_eq!(log.events().len(), 4);
            let modules = py.import("sys")?.getattr("modules")?.cast_into::<PyDict>()?;
            assert!(!modules.contains(package.package())?);
            assert!(!modules.contains(format!("{}.production", package.package()))?);
            cleanup_python(py, &package);

            let package = TestPackage::new(
                "diagnostic_dual_failure",
                "import builtins\nclass Production(builtins._troupe_b09_base):\n    def __init__(self, args):\n        builtins._troupe_b09_calls.append(list(args))\n        raise RuntimeError('original constructor error')\n",
            );
            let calls = install_builtins(py)?;
            let (producer, log) = make_producer(package.root(), Some(6));
            let path = producer
                .resolve_path(py, prevalidate(py, &package)?)
                .expect("resolve valid package path");
            let class = producer
                .resolve_class(py, path)
                .expect("resolve valid Production class");
            let error = producer
                .construct(py, class, &PyList::empty(py))
                .expect_err("constructor and finish admission must fail");
            assert_eq!(python_reason(py, &error), "construction-failed");
            assert_eq!(
                error.diagnostic_error().expect("retain finish failure").code(),
                "diagnostic.admission-failed"
            );
            assert_eq!(calls.bind(py).len(), 1);
            assert_eq!(log.events().len(), 5);
            assert_eq!(
                error
                    .python_error()
                    .expect("retain original PyErr")
                    .value(py)
                    .getattr("reason")?
                    .extract::<String>()?,
                "construction-failed"
            );
            cleanup_python(py, &package);
            Ok::<_, PyErr>(())
        })
        .expect("enforce mandatory admission around operations");
    }

    #[test]
    fn shared_context_admits_typed_causal_events_and_construction_scope_is_lifo() {
        let (producer, log) = make_producer(Path::new("/tmp/diagnostic-context"), None);
        let context = producer.context();
        let span_id = context
            .start_span(
                empty_scope(),
                SpanStartDetail::RunLifecycle(EmptyDetail::new()),
                None,
            )
            .expect("admit containing span");
        let instant_sequence = context
            .emit_instant_with_causes(
                empty_scope(),
                InstantDetail::CueAdmitted(EmptyDetail::new()),
                Some(span_id),
                vec![CausalLink::new(span_id, CausalRelation::FollowsFrom)],
            )
            .expect("admit typed instant");
        let counter_sequence = context
            .emit_counter(
                empty_scope(),
                CounterKind::CueActive,
                SchemaU64::new(1),
                vec![CausalLink::new(
                    instant_sequence,
                    CausalRelation::FollowsFrom,
                )],
            )
            .expect("admit typed counter");

        let events = log.events();
        let DiagnosticEvent::InstantOccurred(instant) = events[1].event() else {
            panic!("expected typed instant")
        };
        assert_eq!(instant.header().sequence(), instant_sequence);
        assert_eq!(instant.containing_span_id(), Some(span_id));
        assert_eq!(
            instant.header().caused_by(),
            &[CausalLink::new(span_id, CausalRelation::FollowsFrom)]
        );
        let DiagnosticEvent::CounterSampled(counter) = events[2].event() else {
            panic!("expected typed counter")
        };
        assert_eq!(counter.header().sequence(), counter_sequence);
        assert_eq!(counter.counter_kind(), CounterKind::CueActive);
        assert_eq!(counter.value(), SchemaU64::new(1));
        assert_eq!(
            counter.header().caused_by(),
            &[CausalLink::new(
                instant_sequence,
                CausalRelation::FollowsFrom,
            )]
        );

        assert!(current_production_construction().is_none());
        let outer = ProductionConstructionGuard::enter(context.clone(), span_id);
        assert_eq!(
            current_production_construction()
                .expect("outer construction scope")
                .construct_span_id(),
            span_id
        );
        let nested_id = SchemaU64::new(span_id.get() + 1);
        let nested = ProductionConstructionGuard::enter(context, nested_id);
        assert_eq!(
            current_production_construction()
                .expect("nested construction scope")
                .construct_span_id(),
            nested_id
        );
        drop(nested);
        assert_eq!(
            current_production_construction()
                .expect("restored outer construction scope")
                .construct_span_id(),
            span_id
        );
        drop(outer);
        assert!(current_production_construction().is_none());
    }

    #[test]
    fn shared_context_validates_and_admits_custom_events_through_the_same_hub() {
        let (producer, log) = make_producer(Path::new("/tmp/diagnostic-custom-context"), None);
        let context = producer.context();

        let invalid = context
            .start_custom_span(
                empty_scope(),
                "invalid".to_owned(),
                None,
                Default::default(),
            )
            .expect_err("invalid custom fields must fail before admission");
        assert_eq!(invalid.code(), "diagnostic.custom-event-invalid");
        assert!(log.events().is_empty());

        let span_id = context
            .start_custom_span(
                empty_scope(),
                "application.phase".to_owned(),
                None,
                Default::default(),
            )
            .expect("admit custom span start");
        context
            .emit_custom_instant(
                empty_scope(),
                "application.notice".to_owned(),
                Some(span_id),
                Some(CustomSeverity::Info),
                Default::default(),
            )
            .expect("admit custom instant");
        context
            .emit_custom_counter(
                empty_scope(),
                "application.items".to_owned(),
                CustomNumber::Integer(CanonicalInteger::parse("3").expect("canonical integer")),
                Some("items".to_owned()),
                Default::default(),
            )
            .expect("admit custom counter");
        context
            .finish_custom_span(empty_scope(), span_id, SpanOutcome::Completed)
            .expect("admit custom span finish");

        let events = log.events();
        assert_eq!(events.len(), 4);
        assert_eq!(span_id, SchemaU64::new(1));
        let DiagnosticEvent::CustomSpanStarted(start) = events[0].event() else {
            panic!("expected custom span start")
        };
        assert_eq!(start.name(), "application.phase");
        assert_eq!(start.parent_span_id(), None);
        assert!(start.header().caused_by().is_empty());
        let DiagnosticEvent::CustomInstantOccurred(instant) = events[1].event() else {
            panic!("expected custom instant")
        };
        assert_eq!(instant.name(), "application.notice");
        assert_eq!(instant.containing_span_id(), Some(span_id));
        assert_eq!(instant.severity(), Some(CustomSeverity::Info));
        let DiagnosticEvent::CustomCounterSampled(counter) = events[2].event() else {
            panic!("expected custom counter")
        };
        assert_eq!(counter.name(), "application.items");
        assert_eq!(counter.unit(), Some("items"));
        let DiagnosticEvent::CustomSpanFinished(finish) = events[3].event() else {
            panic!("expected custom span finish")
        };
        assert_eq!(finish.span_id(), span_id);
        assert_eq!(finish.outcome(), SpanOutcome::Completed);
        assert!(events.windows(2).all(|pair| {
            pair[0].event().header().elapsed_ns().get()
                <= pair[1].event().header().elapsed_ns().get()
        }));
    }
}
