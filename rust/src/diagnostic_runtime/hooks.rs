use std::any::Any;
use std::fmt;
use std::sync::{Arc, OnceLock};

use pyo3::class::gc::{PyTraverseError, PyVisit};
use pyo3::prelude::*;
use pyo3::types::PyAny;
use troupe_agent_runtime::{AgentTurnControl, diagnostics::payload::SinkOnlyToolPayload};
use troupe_diagnostics_core::hub::{ActEventSubscriber, DeliveryFailure};

use crate::orchestration::scene_context::{CuedScope, RunBinding};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiagnosticAdmissionProfile {
    ProductionDurable,
    SinkOnlyVolatile,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DiagnosticCaptureConfig {
    pub(crate) agent_messages: bool,
    pub(crate) plans: bool,
    pub(crate) tool_calls: bool,
    pub(crate) result_validation: bool,
    pub(crate) usage: bool,
    pub(crate) custom_events: bool,
    pub(crate) tool_inputs: bool,
    pub(crate) tool_outputs: bool,
}

impl DiagnosticCaptureConfig {
    #[allow(clippy::too_many_arguments)]
    pub(crate) const fn new(
        agent_messages: bool,
        plans: bool,
        tool_calls: bool,
        result_validation: bool,
        usage: bool,
        custom_events: bool,
        tool_inputs: bool,
        tool_outputs: bool,
    ) -> Self {
        Self {
            agent_messages,
            plans,
            tool_calls,
            result_validation,
            usage,
            custom_events,
            tool_inputs,
            tool_outputs,
        }
    }
}

pub(crate) struct DiagnosticActBinding {
    capture: DiagnosticCaptureConfig,
    request: Option<Py<PyAny>>,
}

impl DiagnosticActBinding {
    pub(crate) const fn inactive() -> Self {
        Self {
            capture: DiagnosticCaptureConfig {
                agent_messages: false,
                plans: false,
                tool_calls: false,
                result_validation: false,
                usage: false,
                custom_events: false,
                tool_inputs: false,
                tool_outputs: false,
            },
            request: None,
        }
    }

    pub(crate) fn new(capture: DiagnosticCaptureConfig, request: Py<PyAny>) -> Self {
        Self {
            capture,
            request: Some(request),
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.request.is_some()
    }

    pub(crate) fn capture(&self) -> DiagnosticCaptureConfig {
        self.capture
    }

    pub(crate) fn request(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.request.as_ref().map(|request| request.clone_ref(py))
    }

    pub(crate) fn into_parts(self) -> (DiagnosticCaptureConfig, Option<Py<PyAny>>) {
        (self.capture, self.request)
    }

    pub(crate) fn traverse(&self, visit: &PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.request)
    }

    pub(crate) fn clear(&mut self) {
        self.request = None;
    }
}

pub(crate) trait DiagnosticAdmissionCapability: Any + Send + Sync {
    fn as_any(&self) -> &dyn Any;

    fn profile(&self) -> DiagnosticAdmissionProfile;

    fn traverse(&self, _visit: &PyVisit<'_>) -> Result<(), PyTraverseError> {
        Ok(())
    }

    fn admit_act(
        &self,
        py: Python<'_>,
        run: &RunBinding,
        cued: &Arc<CuedScope>,
        control: &Arc<AgentTurnControl>,
        binding: DiagnosticActBinding,
    ) -> PyResult<()>;
}

pub(crate) trait DiagnosticActSubscriberLookup: Send + Sync + 'static {
    fn subscriber_for(&self, act_id: &str) -> Option<Arc<dyn ActEventSubscriber>>;

    fn deliver_tool_payload(
        &self,
        _act_id: &str,
        _canonical_tool_call_id: &str,
        _payload: &SinkOnlyToolPayload,
    ) {
    }
}

#[derive(Debug, Default)]
pub(crate) struct NoopDiagnosticActSubscriber;

impl ActEventSubscriber for NoopDiagnosticActSubscriber {
    fn deliver(
        &self,
        _event: troupe_diagnostics_core::hub::AcceptedDiagnosticEvent,
    ) -> Result<(), DeliveryFailure> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DiagnosticAdmissionInstallError {
    pub(crate) installed: DiagnosticAdmissionProfile,
    pub(crate) requested: DiagnosticAdmissionProfile,
}

impl fmt::Display for DiagnosticAdmissionInstallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "diagnostic admission capability already installed as {:?}; cannot install {:?}",
            self.installed, self.requested
        )
    }
}

impl std::error::Error for DiagnosticAdmissionInstallError {}

#[derive(Default)]
pub(crate) struct DiagnosticAdmissionSlot {
    capability: OnceLock<Arc<dyn DiagnosticAdmissionCapability>>,
}

impl DiagnosticAdmissionSlot {
    pub(crate) const fn new() -> Self {
        Self {
            capability: OnceLock::new(),
        }
    }

    pub(crate) fn capability(&self) -> Option<&Arc<dyn DiagnosticAdmissionCapability>> {
        self.capability.get()
    }

    pub(crate) fn profile(&self) -> Option<DiagnosticAdmissionProfile> {
        self.capability().map(|capability| capability.profile())
    }

    pub(crate) fn traverse(&self, visit: &PyVisit<'_>) -> Result<(), PyTraverseError> {
        match self.capability() {
            Some(capability) => capability.traverse(visit),
            None => Ok(()),
        }
    }

    pub(crate) fn install(
        &self,
        capability: Arc<dyn DiagnosticAdmissionCapability>,
    ) -> Result<(), DiagnosticAdmissionInstallError> {
        let requested = capability.profile();
        self.capability
            .set(capability)
            .map_err(|_| DiagnosticAdmissionInstallError {
                installed: self
                    .profile()
                    .expect("a failed OnceLock install has an installed capability"),
                requested,
            })
    }
}
